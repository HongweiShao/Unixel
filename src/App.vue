<script setup lang="ts">
import { ref, computed } from "vue";
import Viewer from "./components/Viewer.vue";
import type {
  DicomMeta,
  DicomImage,
  NiftiMeta,
  NiftiVolume,
  BatchResult,
} from "./types";
import { decodePixelBytes } from "./types";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";

type ImageView = { meta: DicomMeta; frames: Float32Array[] };
type NiftiView = { meta: NiftiMeta; volume: Float32Array; axis: number };

const imageView = ref<ImageView | null>(null);
const niftiView = ref<NiftiView | null>(null);

// 每次成功打开文件自增，作为 Viewer 的 :key，强制组件重挂载（重置窗位/帧/缩放，并触发重渲染）。
// 否则重新打开文件时 Vue 会复用同一 Viewer 实例，meta/frames prop 虽更新，但 render() 不重新触发，界面不刷新。
const openSeq = ref(0);

const loading = ref(false);
const error = ref<string | null>(null);

// 批量导出
const batchFormat = ref<"png" | "jpeg" | "htj2k">("png");
const batchQuality = ref(90);
const batchResult = ref<BatchResult | null>(null);
const batchBusy = ref(false);

const fileFilters = [
  {
    name: "影像",
    extensions: [
      "dcm",
      "dicom",
      "png",
      "jpg",
      "jpeg",
      "tif",
      "tiff",
      "nii",
      "nii.gz",
      "j2c",
      "jph",
    ],
  },
  { name: "所有文件", extensions: ["*"] },
];

function toImageView(img: DicomImage): ImageView {
  const all = decodePixelBytes(img.pixelBytes);
  const n = img.meta.width * img.meta.height;
  const frames: Float32Array[] = [];
  for (let f = 0; f < img.meta.frames; f++) {
    frames.push(all.subarray(f * n, (f + 1) * n));
  }
  return { meta: img.meta, frames };
}

async function openFile() {
  error.value = null;
  loading.value = true;
  try {
    const selected = await open({ multiple: false, filters: fileFilters });
    if (!selected || Array.isArray(selected)) return;
    const lower = selected.toLowerCase();
    if (lower.endsWith(".nii") || lower.endsWith(".nii.gz")) {
      const vol = await invoke<NiftiVolume>("load_nifti", { path: selected });
      niftiView.value = {
        meta: vol.meta,
        volume: decodePixelBytes(vol.voxelBytes),
        axis: 0,
      };
    } else if (lower.endsWith(".j2c") || lower.endsWith(".jph")) {
      const img = await invoke<DicomImage>("load_htj2k", { path: selected });
      imageView.value = toImageView(img);
    } else if (lower.endsWith(".dcm") || lower.endsWith(".dicom")) {
      const img = await invoke<DicomImage>("load_dicom", { path: selected });
      imageView.value = toImageView(img);
    } else {
      const img = await invoke<DicomImage>("load_image", { path: selected });
      imageView.value = toImageView(img);
    }
    // 新文件加载成功，自增序列号以触发 Viewer 重挂载（见 openSeq 注释）
    openSeq.value++;
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    loading.value = false;
  }
}

// MPR：按轴把 3D 体重排为若干帧（体顺序 [x][y][z]，z 最内）
function extractNifti(
  axis: number,
  meta: NiftiMeta,
  volume: Float32Array
): { width: number; height: number; frames: Float32Array[] } {
  const [nx, ny, nz] = meta.dims;
  const frames: Float32Array[] = [];
  if (axis === 0) {
    // 轴向：沿 z，宽 nx 高 ny
    for (let z = 0; z < nz; z++) {
      const f = new Float32Array(nx * ny);
      for (let y = 0; y < ny; y++)
        for (let x = 0; x < nx; x++)
          f[y * nx + x] = volume[((x * ny) + y) * nz + z];
      frames.push(f);
    }
    return { width: nx, height: ny, frames };
  } else if (axis === 1) {
    // 冠状：沿 y，宽 nx 高 nz
    for (let y = 0; y < ny; y++) {
      const f = new Float32Array(nx * nz);
      for (let z = 0; z < nz; z++)
        for (let x = 0; x < nx; x++)
          f[z * nx + x] = volume[((x * ny) + y) * nz + z];
      frames.push(f);
    }
    return { width: nx, height: nz, frames };
  } else {
    // 矢状：沿 x，宽 ny 高 nz
    for (let x = 0; x < nx; x++) {
      const f = new Float32Array(ny * nz);
      for (let z = 0; z < nz; z++)
        for (let y = 0; y < ny; y++)
          f[z * ny + y] = volume[((x * ny) + y) * nz + z];
      frames.push(f);
    }
    return { width: ny, height: nz, frames };
  }
}

const niftiRender = computed(() => {
  if (!niftiView.value) return null;
  const { width, height, frames } = extractNifti(
    niftiView.value.axis,
    niftiView.value.meta,
    niftiView.value.volume
  );
  const m = niftiView.value.meta;
  const meta: DicomMeta = {
    path: m.path,
    filename: m.filename,
    width,
    height,
    frames: frames.length,
    bitsStored: 16,
    pixelRepresentation: m.huMin < 0 ? 1 : 0,
    slope: 1,
    intercept: 0,
    windowCenter: (m.huMin + m.huMax) / 2,
    windowWidth: m.huMax - m.huMin || 1,
    photometric: "MONOCHROME2",
    huMin: m.huMin,
    huMax: m.huMax,
  };
  return { meta, frames };
});

// 用于强制 Viewer 在切换 MPR 视图时重挂载（重置帧索引）
const niftiKey = computed(() => "n" + (niftiView.value?.axis ?? 0));

function clearView() {
  imageView.value = null;
  niftiView.value = null;
}

async function batchExport() {
  error.value = null;
  batchResult.value = null;
  batchBusy.value = true;
  try {
    const files = await open({ multiple: true, filters: fileFilters });
    if (!files || typeof files === "string") return;
    const dir = await open({ directory: true, title: "选择批量导出目录" });
    if (!dir || Array.isArray(dir)) return;
    const res = await invoke<BatchResult>("batch_export", {
      paths: files,
      format: batchFormat.value,
      quality: batchQuality.value,
      outDir: dir,
    });
    batchResult.value = res;
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    batchBusy.value = false;
  }
}

function dismissBatch() {
  batchResult.value = null;
}

// 生成示例体数据（前端联调用，后端就绪后可对照真实 DICOM）
function genMock(): ImageView {
  const w = 512;
  const h = 512;
  const frames = 3;
  const fs: Float32Array[] = [];
  for (let f = 0; f < frames; f++) {
    const buf = new Float32Array(w * h);
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const dx = x - w / 2;
        const dy = y - h / 2;
        const r = Math.sqrt(dx * dx + dy * dy);
        let hu = -1000;
        if (r < 200) hu = 40 + 20 * Math.sin(f + r / 30);
        if (r < 120) hu = 800 + 200 * Math.sin(r / 20);
        buf[y * w + x] = hu;
      }
    }
    fs.push(buf);
  }
  const meta: DicomMeta = {
    path: "mock://phantom",
    filename: "MOCK_PHANTOM.dcm",
    width: w,
    height: h,
    frames,
    bitsStored: 16,
    pixelRepresentation: 1,
    slope: 1,
    intercept: 0,
    windowCenter: 40,
    windowWidth: 400,
    photometric: "MONOCHROME2",
    huMin: -1000,
    huMax: 1200,
  };
  return { meta, frames: fs };
}
const useMock = () => (imageView.value = genMock());
</script>

<template>
  <div class="app">
    <header>
      <h1>Unixel · 医学影像处理软件</h1>
      <span class="sub">Rust + Tauri2 + Vue3 · DICOM / 常规图像 / NIfTI / HTJ2K</span>
      <div class="actions">
        <button class="primary" :disabled="loading" @click="openFile">
          {{ loading ? "解码中…" : "打开文件" }}
        </button>
        <button :disabled="batchBusy" @click="batchExport">
          {{ batchBusy ? "导出中…" : "批量导出" }}
        </button>
      </div>
    </header>

    <div v-if="batchResult" class="batch-banner">
      <span>
        批量导出完成：成功 <b>{{ batchResult.ok }}</b> 张，失败
        <b>{{ batchResult.failed.length }}</b> 张
      </span>
      <ul v-if="batchResult.failed.length">
        <li v-for="f in batchResult.failed" :key="f.path">
          ⚠ {{ f.path }} — {{ f.error }}
        </li>
      </ul>
      <button class="close" @click="dismissBatch">关闭</button>
    </div>

    <main>
      <div v-if="!imageView && !niftiView" class="placeholder">
        <button :disabled="loading" @click="openFile">
          {{ loading ? "解码中…" : "打开文件" }}
        </button>
        <button class="ghost" @click="useMock">载入示例体数据（mock）</button>
        <p v-if="error" class="err">⚠ {{ error }}</p>
        <p class="hint">
          支持 DICOM(.dcm)、常规图像(PNG/JPG/TIFF)、NIfTI(.nii/.nii.gz)、HTJ2K(.j2c/.jph)。
        </p>
      </div>

      <template v-else>
        <div v-if="niftiView" class="mpr-bar">
          <span class="lbl">MPR 视图</span>
          <button :class="{ active: niftiView.axis === 0 }" @click="niftiView.axis = 0">
            轴向
          </button>
          <button :class="{ active: niftiView.axis === 1 }" @click="niftiView.axis = 1">
            冠状
          </button>
          <button :class="{ active: niftiView.axis === 2 }" @click="niftiView.axis = 2">
            矢状
          </button>
          <span class="dim">体尺寸 {{ niftiView.meta.dims.join(" × ") }}</span>
        </div>
        <Viewer
          v-if="imageView"
          :key="'img-' + openSeq"
          :meta="imageView.meta"
          :frames="imageView.frames"
          @close="clearView"
        />
        <Viewer
          v-else-if="niftiRender"
          :key="'nii-' + openSeq + '-' + niftiKey"
          :meta="niftiRender.meta"
          :frames="niftiRender.frames"
          @close="clearView"
        />
      </template>
    </main>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
}
header {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 16px 20px;
  border-bottom: 1px solid var(--border);
  background: var(--panel);
}
h1 {
  margin: 0;
  font-size: 18px;
}
.sub {
  color: var(--fg-dim);
  font-size: 12px;
}
.actions {
  margin-left: auto;
  display: flex;
  gap: 8px;
}
.actions button {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  padding: 6px 12px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 13px;
}
.actions button.primary {
  background: var(--accent);
  color: #fff;
  border-color: var(--accent);
}
.actions button:disabled {
  opacity: 0.6;
  cursor: default;
}
.batch-banner {
  padding: 8px 20px;
  background: #16331f;
  border-bottom: 1px solid var(--border);
  font-size: 13px;
  color: #c8f0d4;
}
.batch-banner ul {
  margin: 4px 0 0;
  padding-left: 18px;
  color: #e5a3a3;
  max-height: 120px;
  overflow: auto;
}
.batch-banner .close {
  margin-left: 12px;
  background: transparent;
  border: 1px solid var(--border);
  color: var(--fg-dim);
  border-radius: 4px;
  cursor: pointer;
}
main {
  flex: 1;
  min-height: 0;
}
.placeholder {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
}
.placeholder button {
  background: var(--accent);
  color: #fff;
  border: none;
  padding: 10px 18px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 14px;
}
.placeholder button:disabled {
  opacity: 0.6;
  cursor: default;
}
.placeholder .ghost {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
}
.placeholder .ghost:hover {
  color: var(--fg);
}
.err {
  color: #e5484d;
  font-size: 13px;
  max-width: 520px;
  text-align: center;
}
.hint {
  color: var(--fg-dim);
  font-size: 12px;
}
.mpr-bar {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 6px 12px;
  background: var(--panel);
  border-bottom: 1px solid var(--border);
}
.mpr-bar .lbl {
  font-size: 12px;
  color: var(--fg-dim);
}
.mpr-bar button {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 10px;
  cursor: pointer;
  font-size: 12px;
}
.mpr-bar button.active {
  background: var(--accent);
  color: #fff;
  border-color: var(--accent);
}
.mpr-bar .dim {
  font-size: 12px;
  color: var(--fg-dim);
  margin-left: auto;
}
</style>
