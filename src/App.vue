<script setup lang="ts">
import { ref, computed, onMounted, nextTick, watch } from "vue";
import Viewer from "./components/Viewer.vue";
import type {
  DicomMeta,
  DicomImage,
  NiftiMeta,
  NiftiVolume,
  BatchResult,
  ImageInfo,
  FileTags,
} from "./types";
import { decodePixelBytes } from "./types";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import aboutIcon from "./assets/about-icon.png";

type ImageView = { meta: DicomMeta; frames: Float32Array[] };
type NiftiView = { meta: NiftiMeta; volume: Float32Array; axis: number };
type OpenedImage = { id: number; info: ImageInfo; view: ImageView | null };

// 已导入图像列表（从文件夹导入可形成多张；单张打开独占，列表长度为 1 不显示下拉）
const imageList = ref<OpenedImage[]>([]);
const activeId = ref<number | null>(null);
const idSeq = ref(0);
const listLoading = ref(false);

const niftiView = ref<NiftiView | null>(null);
// 每次成功打开 NIfTI 自增，作为 Viewer 的 :key，强制组件重挂载（重置帧索引）
const openSeq = ref(0);

const loading = ref(false);
const error = ref<string | null>(null);

// 批量导出
const batchFormat = ref<"png" | "jpeg" | "htj2k">("png");
const batchQuality = ref(90);
const batchResult = ref<BatchResult | null>(null);
const batchBusy = ref(false);

// 菜单栏
const openMenu = ref<"file" | "help" | null>(null);
function toggleMenu(m: "file" | "help") {
  openMenu.value = openMenu.value === m ? null : m;
}
function closeMenu() {
  openMenu.value = null;
}

// 关于对话框
const aboutOpen = ref(false);
const appVersion = ref("0.1.0");
function openAbout() {
  aboutOpen.value = true;
  closeMenu();
}

// 详情对话框
const detailsOpen = ref(false);
const detailsLoading = ref(false);
const detailsTags = ref<FileTags | null>(null);
const detailsError = ref<string | null>(null);
const detailsQuery = ref("");

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

function infoFromMeta(m: DicomMeta, path: string, kind: string): ImageInfo {
  return {
    path,
    filename: m.filename,
    width: m.width,
    height: m.height,
    frames: m.frames,
    kind,
  };
}

async function loadByKind(path: string, kind: string): Promise<ImageView> {
  let img: DicomImage;
  if (kind === "htj2k") img = await invoke<DicomImage>("load_htj2k", { path });
  else if (kind === "dicom") img = await invoke<DicomImage>("load_dicom", { path });
  else img = await invoke<DicomImage>("load_image", { path });
  return toImageView(img);
}

function clearImages() {
  imageList.value = [];
  activeId.value = null;
}
function clearNifti() {
  niftiView.value = null;
}

// 单文件打开（独占显示，列表长度=1，不显示下拉）
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
      clearImages();
      openSeq.value++;
    } else {
      const kind =
        lower.endsWith(".j2c") || lower.endsWith(".jph")
          ? "htj2k"
          : lower.endsWith(".dcm") || lower.endsWith(".dicom")
            ? "dicom"
            : "image";
      const view = await loadByKind(selected, kind);
      idSeq.value++;
      imageList.value = [{ id: idSeq.value, info: infoFromMeta(view.meta, selected, kind), view }];
      activeId.value = idSeq.value;
      clearNifti();
    }
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    loading.value = false;
  }
}

// 从文件夹导入（仅顶层文件，形成可切换的多图列表；像素按需懒加载）
async function importFolder() {
  error.value = null;
  listLoading.value = true;
  try {
    const dir = await open({ directory: true, title: "选择包含影像的文件夹" });
    if (!dir || Array.isArray(dir)) return;
    const infos = await invoke<ImageInfo[]>("list_folder_images", { dir });
    if (!infos.length) {
      error.value = "该文件夹未找到可识别的影像文件";
      return;
    }
    const list: OpenedImage[] = infos.map((info) => ({
      id: ++idSeq.value,
      info,
      view: null,
    }));
    imageList.value = list;
    activeId.value = list[0].id;
    clearNifti();
    await ensureLoaded(list[0].id);
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    listLoading.value = false;
  }
}

const currentItem = computed(() => imageList.value.find((i) => i.id === activeId.value) ?? null);
const currentView = computed(() => currentItem.value?.view ?? null);
const currentPath = computed(
  () => currentItem.value?.info.path ?? niftiView.value?.meta.path ?? null
);

// 选中下拉项或首次导入时，懒加载该图像的像素
async function ensureLoaded(id: number) {
  const item = imageList.value.find((i) => i.id === id);
  if (!item || item.view) return;
  listLoading.value = true;
  try {
    item.view = await loadByKind(item.info.path, item.info.kind);
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    listLoading.value = false;
  }
}

function selectImage(id: number) {
  activeId.value = id;
  closeMenu();
  ensureLoaded(id);
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
    for (let z = 0; z < nz; z++) {
      const f = new Float32Array(nx * ny);
      for (let y = 0; y < ny; y++)
        for (let x = 0; x < nx; x++)
          f[y * nx + x] = volume[((x * ny) + y) * nz + z];
      frames.push(f);
    }
    return { width: nx, height: ny, frames };
  } else if (axis === 1) {
    for (let y = 0; y < ny; y++) {
      const f = new Float32Array(nx * nz);
      for (let z = 0; z < nz; z++)
        for (let x = 0; x < nx; x++)
          f[z * nx + x] = volume[((x * ny) + y) * nz + z];
      frames.push(f);
    }
    return { width: nx, height: nz, frames };
  } else {
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

// 状态栏信息
const statusName = computed(
  () => currentItem.value?.info.filename ?? niftiView.value?.meta.filename ?? ""
);
const currentMeta = computed(
  () => currentView.value?.meta ?? niftiRender.value?.meta ?? null
);
const statusDims = computed(() => {
  const m = currentMeta.value;
  const i = currentItem.value?.info;
  const w = m?.width ?? i?.width ?? 0;
  const h = m?.height ?? i?.height ?? 0;
  return w || h ? `${w} × ${h}` : "—";
});
const statusFrames = computed(() => {
  const m = currentMeta.value;
  const i = currentItem.value?.info;
  return m?.frames ?? i?.frames ?? 0;
});
const canShowDetails = computed(() => currentPath.value !== null);
const filteredTags = computed(() => {
  if (!detailsTags.value) return [];
  const q = detailsQuery.value.trim().toLowerCase();
  if (!q) return detailsTags.value.rows;
  return detailsTags.value.rows.filter(
    (r) =>
      r.tag.toLowerCase().includes(q) ||
      r.keyword.toLowerCase().includes(q) ||
      r.value.toLowerCase().includes(q)
  );
});

// 标签行展开 / 溢出检测：默认单行显示，溢出行在行尾提供“展开”按钮
const expandedTags = ref<Set<string>>(new Set());
const overflowMap = ref<Record<string, boolean>>({});
const valRefs = new Map<string, HTMLElement>();
function setValRef(tag: string, el: unknown) {
  if (el) valRefs.set(tag, el as HTMLElement);
  else valRefs.delete(tag);
}
function measureOverflow() {
  const m: Record<string, boolean> = {};
  valRefs.forEach((el, tag) => {
    m[tag] = el.scrollWidth > el.clientWidth;
  });
  overflowMap.value = m;
}
function toggleTag(tag: string) {
  const s = new Set(expandedTags.value);
  if (s.has(tag)) s.delete(tag);
  else s.add(tag);
  expandedTags.value = s;
}
watch(
  filteredTags,
  async () => {
    await nextTick();
    measureOverflow();
  },
  { flush: "post" }
);

async function openDetails() {
  const path = currentPath.value;
  if (!path) return;
  detailsOpen.value = true;
  detailsLoading.value = true;
  detailsTags.value = null;
  detailsError.value = null;
  detailsQuery.value = "";
  expandedTags.value = new Set();
  try {
    detailsTags.value = await invoke<FileTags>("file_tags", { path });
  } catch (e) {
    detailsError.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    detailsLoading.value = false;
  }
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
const useMock = () => {
  idSeq.value++;
  const mock = genMock();
  imageList.value = [
    { id: idSeq.value, info: infoFromMeta(mock.meta, "mock://phantom", "dicom"), view: mock },
  ];
  activeId.value = idSeq.value;
  clearNifti();
};

onMounted(async () => {
  try {
    appVersion.value = await getVersion();
  } catch {
    /* 非 Tauri 环境忽略，保留默认版本 */
  }
});
</script>

<template>
  <div class="app">
    <!-- 顶部菜单栏 -->
    <header class="menubar">
      <nav class="menus">
        <div class="menu" :class="{ open: openMenu === 'file' }" @click="toggleMenu('file')">
          文件 <span class="caret">▾</span>
          <div v-if="openMenu === 'file'" class="dropdown" @click.stop>
            <button @click="openFile">打开文件…</button>
            <button @click="importFolder">从文件夹导入…</button>
            <button @click="batchExport">批量导出…</button>
          </div>
        </div>
        <div class="menu" :class="{ open: openMenu === 'help' }" @click="toggleMenu('help')">
          帮助 <span class="caret">▾</span>
          <div v-if="openMenu === 'help'" class="dropdown" @click.stop>
            <button @click="openAbout">关于</button>
          </div>
        </div>
      </nav>
      <div class="menubar-status">
        <span v-if="loading">解码中…</span>
        <span v-else-if="listLoading">导入中…</span>
      </div>
    </header>

    <!-- 点击空白处关闭菜单的遮罩 -->
    <div v-if="openMenu" class="menu-overlay" @click="closeMenu"></div>

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
      <template v-if="currentView">
        <Viewer
          :key="'img-' + activeId"
          :meta="currentView.meta"
          :frames="currentView.frames"
        />
      </template>
      <template v-else-if="niftiView">
        <div class="mpr-bar">
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
          v-if="niftiRender"
          :key="'nii-' + openSeq + '-' + niftiKey"
          :meta="niftiRender.meta"
          :frames="niftiRender.frames"
        />
      </template>
      <template v-else-if="activeId !== null">
        <div class="decode-loading">解码中…</div>
      </template>
      <template v-else>
        <div class="placeholder">
          <button :disabled="loading" @click="openFile">
            {{ loading ? "解码中…" : "打开文件" }}
          </button>
          <button class="ghost" @click="useMock">载入示例体数据（mock）</button>
          <p v-if="error" class="err">⚠ {{ error }}</p>
          <p class="hint">
            支持 DICOM(.dcm)、常规图像(PNG/JPG/TIFF)、NIfTI(.nii/.nii.gz)、HTJ2K(.j2c/.jph)。
          </p>
        </div>
      </template>
    </main>

    <!-- 底部状态栏 -->
    <footer class="statusbar">
      <template v-if="activeId !== null && imageList.length">
        <select
          v-if="imageList.length > 1"
          class="status-file-select"
          :value="activeId"
          @change="selectImage(Number(($event.target as HTMLSelectElement).value))"
        >
          <option v-for="it in imageList" :key="it.id" :value="it.id">
            {{ it.info.filename }}
          </option>
        </select>
        <span v-else class="status-file" :title="statusName">{{ statusName }}</span>
      </template>
      <span v-else-if="niftiView" class="status-file" :title="statusName">{{ statusName }}</span>

      <span class="status-sep">·</span>
      <span class="status-dim">{{ statusDims }}</span>
      <span class="status-sep">·</span>
      <span class="status-frames">{{ statusFrames }} 帧</span>

      <button class="status-details" :disabled="!canShowDetails" @click="openDetails">
        更多信息
      </button>

      <span class="status-spacer"></span>
    </footer>

    <!-- 关于对话框 -->
    <div v-if="aboutOpen" class="modal-mask" @click.self="aboutOpen = false">
      <div class="modal about">
        <img :src="aboutIcon" class="about-icon" alt="软件图标" />
        <h2>医学影像处理软件</h2>
        <p class="en-name">Unixel</p>
        <p class="ver">版本 {{ appVersion }}</p>
        <p class="author">作者：邵宏伟</p>
        <p class="contact">联系方式：hongweishao@outlook.com</p>
        <button class="modal-close" @click="aboutOpen = false">关闭</button>
      </div>
    </div>

    <!-- 详情对话框 -->
    <div v-if="detailsOpen" class="modal-mask" @click.self="detailsOpen = false">
      <div class="modal details">
        <div class="details-head">
          <h2>文件标签信息</h2>
          <span class="details-file">{{ detailsTags?.filename }}</span>
          <span class="status-spacer"></span>
          <input
            v-model="detailsQuery"
            class="details-search"
            placeholder="搜索标签 / 关键字 / 值…"
          />
          <button class="modal-close" @click="detailsOpen = false">关闭</button>
        </div>
        <div class="details-body">
          <div v-if="detailsLoading" class="details-loading">读取中…</div>
          <div v-else-if="detailsError" class="details-err">⚠ {{ detailsError }}</div>
          <table v-else-if="detailsTags" class="tags-table">
            <thead>
              <tr>
                <th>Tag</th>
                <th>VR</th>
                <th>关键字</th>
                <th>值</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="r in filteredTags" :key="r.tag">
                <td class="mono">{{ r.tag }}</td>
                <td>{{ r.vr }}</td>
                <td>{{ r.keyword }}</td>
                <td
                  class="val"
                  :class="{ expanded: expandedTags.has(r.tag) }"
                  :ref="(el) => setValRef(r.tag, el)"
                >{{ r.value }}</td>
                <td class="row-actions">
                  <button
                    v-if="overflowMap[r.tag] || expandedTags.has(r.tag)"
                    class="expand-btn"
                    @click="toggleTag(r.tag)"
                  >{{ expandedTags.has(r.tag) ? "收起" : "展开" }}</button>
                </td>
              </tr>
              <tr v-if="!filteredTags.length">
                <td colspan="5" class="empty">无匹配结果</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
}
/* 菜单栏 */
.menubar {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 8px 14px;
  border-bottom: 1px solid var(--border);
  background: var(--panel);
}
.menus {
  display: flex;
  gap: 4px;
}
.menu {
  position: relative;
  padding: 6px 12px;
  font-size: 13px;
  color: var(--fg-dim);
  border-radius: 6px;
  cursor: pointer;
  user-select: none;
}
.menu:hover,
.menu.open {
  background: var(--bg);
  color: var(--fg);
}
.caret {
  font-size: 10px;
  opacity: 0.7;
}
.dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  margin-top: 4px;
  min-width: 180px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 6px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  z-index: 50;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
}
.dropdown button {
  text-align: left;
  background: transparent;
  border: none;
  color: var(--fg);
  padding: 8px 10px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 13px;
}
.dropdown button:hover {
  background: var(--accent);
  color: #fff;
}
.menu-overlay {
  position: fixed;
  inset: 0;
  z-index: 40;
}
.menubar-status {
  margin-left: auto;
  font-size: 12px;
  color: var(--fg-dim);
}

/* 主体 */
main {
  flex: 1;
  min-height: 0;
  position: relative;
}
.decode-loading {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--fg-dim);
  font-size: 14px;
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
.placeholder .err {
  color: #e5484d;
  font-size: 13px;
  max-width: 520px;
  text-align: center;
}
.placeholder .hint {
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

/* 状态栏 */
.statusbar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 5px 12px;
  border-top: 1px solid var(--border);
  background: var(--panel);
  font-size: 12px;
  color: var(--fg-dim);
}
.status-file,
.status-dim,
.status-frames {
  color: var(--fg);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 320px;
}
.status-file-select {
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 3px 6px;
  max-width: 320px;
  font-size: 12px;
}
.status-sep {
  opacity: 0.5;
}
.status-spacer {
  flex: 1;
}
.status-details {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 12px;
  cursor: pointer;
  font-size: 12px;
}
.status-details:hover:not(:disabled) {
  color: var(--fg);
  border-color: var(--fg-dim);
}
.status-details:disabled {
  opacity: 0.4;
  cursor: default;
}

/* 批量横幅 */
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

/* 对话框 */
.modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}
.modal {
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 22px 24px;
  box-shadow: 0 16px 48px rgba(0, 0, 0, 0.5);
}
.modal.about {
  text-align: center;
  min-width: 280px;
}
.about-icon {
  width: 72px;
  height: 72px;
  margin: 0 auto 12px;
  display: block;
  border-radius: 12px;
}
.modal.about h2 {
  margin: 0 0 8px;
  font-size: 18px;
}
.modal.about .en-name {
  margin: 0 0 12px;
  font-size: 14px;
  color: var(--fg-dim);
  letter-spacing: 0.1em;
}
.modal.about .ver {
  margin: 4px 0;
  color: var(--fg-dim);
  font-size: 13px;
}
.modal.about .author,
.modal.about .contact {
  margin: 4px 0;
  font-size: 13px;
}
.modal-close {
  margin-top: 16px;
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: 6px;
  padding: 7px 18px;
  cursor: pointer;
  font-size: 13px;
}
.modal.details {
  width: min(760px, 92vw);
  max-height: 82vh;
  display: flex;
  flex-direction: column;
  padding: 0;
}
.details-head {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 18px;
  border-bottom: 1px solid var(--border);
}
.details-head h2 {
  margin: 0;
  font-size: 16px;
}
.details-file {
  font-size: 12px;
  color: var(--fg-dim);
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.details-search {
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 5px 10px;
  font-size: 12px;
  width: 220px;
}
.details-body {
  overflow: auto;
  padding: 0;
}
.details-loading,
.details-err {
  padding: 24px;
  text-align: center;
  color: var(--fg-dim);
}
.details-err {
  color: #e5484d;
}
.tags-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}
.tags-table th {
  position: sticky;
  top: 0;
  background: var(--panel);
  text-align: left;
  padding: 8px 12px;
  border-bottom: 1px solid var(--border);
  color: var(--fg-dim);
  font-weight: 600;
}
.tags-table td {
  padding: 6px 12px;
  border-bottom: 1px solid var(--border);
  vertical-align: top;
}
.tags-table td.val {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 240px;
  color: var(--fg);
}
.tags-table td.val.expanded {
  white-space: normal;
  overflow: visible;
  text-overflow: clip;
  word-break: break-all;
}
.tags-table .row-actions {
  width: 56px;
  text-align: right;
  white-space: nowrap;
}
/* 操作列（展开/收起）钉在右侧，对话框变窄时仍始终可见 */
.tags-table th:last-child,
.tags-table td:last-child {
  position: sticky;
  right: 0;
  background: var(--panel);
  border-left: 1px solid var(--border);
}
.tags-table tr:hover td:last-child {
  background: var(--bg);
}
.tags-table .expand-btn {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 2px 8px;
  cursor: pointer;
  font-size: 11px;
}
.tags-table .expand-btn:hover {
  color: var(--fg);
  border-color: var(--fg-dim);
}
.tags-table td.mono {
  font-family: ui-monospace, "SFMono-Regular", Menlo, monospace;
  color: var(--fg-dim);
  white-space: nowrap;
}
.tags-table tr:hover td {
  background: var(--bg);
}
.tags-table .empty {
  text-align: center;
  color: var(--fg-dim);
  padding: 20px;
}
</style>
