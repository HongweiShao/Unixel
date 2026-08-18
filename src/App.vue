<script setup lang="ts">
import { ref, computed, onMounted, nextTick, watch } from "vue";
import Viewer from "./components/Viewer.vue";
import type {
  DicomMeta,
  DicomImage,
  NiftiMeta,
  NiftiVolume,
  ImageInfo,
  FileTags,
  SeriesFields,
  SeriesTree,
  SeriesBrief,
  StudyBrief,
} from "./types";
import { decodePixelBytes } from "./types";
import { open, save } from "@tauri-apps/plugin-dialog";
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
  currentView.value = null;
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
      const info = infoFromMeta(view.meta, selected, kind);
      // 单张 DICOM 打开时也读取系列字段，便于后续系列分组/切换
      if (kind === "dicom") {
        try {
          const sf = await invoke<SeriesFields>("file_series_info", { path: selected });
          info.seriesUid = sf.seriesUid ?? null;
          info.seriesNumber = sf.seriesNumber ?? null;
          info.modality = sf.modality ?? null;
          info.instanceNumber = sf.instanceNumber ?? null;
          info.sliceLocation = sf.sliceLocation ?? null;
          info.imagePosPatient = sf.imagePosPatient ?? null;
          info.imageOrientation = sf.imageOrientation ?? null;
        } catch {
          /* 系列信息缺失不影响打开 */
        }
      }
      idSeq.value++;
      imageList.value = [{ id: idSeq.value, info, view }];
      activeId.value = idSeq.value;
      currentView.value = view;
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

// 从文件夹导入：递归扫描，按 Study/Series 两级分组；单一序列直接打开，多序列弹对话框选择
async function importFolder() {
  error.value = null;
  listLoading.value = true;
  try {
    const dir = await open({ directory: true, title: "选择包含影像的文件夹" });
    if (!dir || Array.isArray(dir)) return;
    const tree = await invoke<SeriesTree>("scan_folder_series", { dir });
    const all: SeriesBrief[] = [
      ...tree.studies.flatMap((s) => s.series),
      ...(tree.others ? [tree.others] : []),
    ];
    if (!all.length) {
      error.value = "该文件夹未找到可识别的影像文件";
      return;
    }
    if (all.length === 1) {
      // 仅一个序列：直接打开该序列并进入主界面
      await loadSeries(all[0]);
    } else {
      // 多个序列：弹出选择对话框
      seriesTree.value = tree;
      selectedSeries.value = null;
      seriesSelectOpen.value = true;
    }
  } catch (e) {
    error.value =
      typeof e === "string"
        ? e
        : (e as { message?: string })?.message ?? String(e);
  } finally {
    listLoading.value = false;
  }
}

const seriesTree = ref<SeriesTree | null>(null);
const seriesSelectOpen = ref(false);
const selectedSeries = ref<SeriesBrief | null>(null);

// 供对话框渲染：把 others（非 DICOM）作为一个伪 Study 归入列表，统一两级展示
const selectStudies = computed<StudyBrief[]>(() => {
  const t = seriesTree.value;
  if (!t) return [];
  if (t.others) {
    return [
      ...t.studies,
      {
        studyUid: null,
        patientName: null,
        patientId: null,
        studyDate: null,
        series: [t.others],
      },
    ];
  }
  return t.studies;
});

function pickSeries(s: SeriesBrief) {
  selectedSeries.value = s;
}
function closeSeriesSelect() {
  seriesSelectOpen.value = false;
  seriesTree.value = null;
  selectedSeries.value = null;
}
// 仅加载所选序列（用户从对话框确认的那个）
async function confirmSeries() {
  if (!selectedSeries.value) return;
  seriesSelectOpen.value = false;
  const s = selectedSeries.value;
  selectedSeries.value = null;
  seriesTree.value = null;
  await loadSeries(s);
}

// 把单个序列的文件路径构建为可显示的图像列表（懒加载像素）
async function loadSeries(series: SeriesBrief) {
  error.value = null;
  listLoading.value = true;
  try {
    const infos = await invoke<ImageInfo[]>("load_series_files", {
      paths: series.paths,
    });
    if (!infos.length) {
      error.value = "所选序列未找到可识别的影像文件";
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
    await decodeItem(list[0]);
    currentView.value = list[0].view;
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
// 当前展示的图像视图（已解码）。用 ref 而非 computed：切换到未解码切片时保留上一帧，
// 后台解码完成后再无感替换，避免 Viewer 因 currentView 变 null 而卸载 → 消除闪烁。
const currentView = ref<ImageView | null>(null);
// 切换令牌：快速拖动时只采用最新一次请求的 decode 结果，过期的中间结果丢弃。
let loadSeq = 0;
const currentPath = computed(
  () => currentItem.value?.info.path ?? niftiView.value?.meta.path ?? null
);

// 当前选中文件所属系列的有序文件列表（按位置排序）；供查看器右侧竖滚动条做切片快速切换
const seriesFiles = computed(() => {
  const cur = currentItem.value;
  if (!cur || cur.info.seriesGroup == null) return [];
  const g = cur.info.seriesGroup;
  return imageList.value.filter((i) => i.info.seriesGroup === g);
});

// 解码单个图像像素（不触碰导入 spinner，供滚动按需调用，结果缓存到 item.view）
async function decodeItem(item: OpenedImage) {
  if (item.view) return;
  item.view = await loadByKind(item.info.path, item.info.kind);
}

// 切换激活图像：已解码则即时切换；未解码则在后台解码，期间保留上一帧显示
// （Viewer 常驻不复挂载，currentView 始终非 null → 绝不闪烁）。
function selectImage(id: number) {
  if (id === activeId.value && currentView.value) return;
  activeId.value = id;
  closeMenu();
  const item = imageList.value.find((i) => i.id === id);
  if (!item) return;
  if (item.view) {
    currentView.value = item.view;
    return;
  }
  const seq = ++loadSeq;
  decodeItem(item)
    .then(() => {
      if (seq !== loadSeq) return; // 已被更新的切换请求取代，丢弃陈旧结果
      const it = imageList.value.find((i) => i.id === id);
      if (it?.view) currentView.value = it.view;
    })
    .catch((e) => {
      if (seq !== loadSeq) return;
      error.value =
        typeof e === "string"
          ? e
          : (e as { message?: string })?.message ?? String(e);
    });
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

// 状态栏下拉分组：按 seriesGroup 聚合（同系列归一组；不属于任何系列的文件各自独立）
const groupedImages = computed(() => {
  const groups: { key: string; group: number | null; label: string | null; items: OpenedImage[] }[] =
    [];
  const idx = new Map<string, number>();
  for (const it of imageList.value) {
    const g = it.info.seriesGroup ?? null;
    const key = g === null ? "u" + it.id : "g" + g;
    if (idx.has(key)) groups[idx.get(key)!].items.push(it);
    else {
      idx.set(key, groups.length);
      groups.push({ key, group: g, label: it.info.seriesLabel ?? null, items: [it] });
    }
  }
  return groups;
});

// 标签搜索过滤
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

// 标签信息导出（在系统保存框中选择 JSON / CSV 格式）
const exportingTags = ref(false);
const exportTagsMsg = ref<string | null>(null);
async function exportTags() {
  if (!detailsTags.value) return;
  const base = (detailsTags.value.filename || "tags").replace(/\.[^.]+$/, "");
  const out = await save({
    defaultPath: base,
    filters: [
      { name: "JSON", extensions: ["json"] },
      { name: "CSV", extensions: ["csv"] },
    ],
  });
  if (!out) return; // 用户取消
  // 按用户在保存框中选择的扩展名推断格式
  const fmt = out.toLowerCase().endsWith(".csv") ? "csv" : "json";
  exportingTags.value = true;
  exportTagsMsg.value = null;
  try {
    await invoke("export_tags", {
      path: out,
      format: fmt,
      rows: detailsTags.value.rows,
    });
    exportTagsMsg.value = `已导出：${out}`;
  } catch (e) {
    exportTagsMsg.value =
      "导出失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    exportingTags.value = false;
  }
}

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
  currentView.value = mock;
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

    <main>
      <template v-if="currentView">
        <Viewer
          :meta="currentView.meta"
          :frames="currentView.frames"
          :series-files="seriesFiles"
          :active-id="activeId"
          @select-file="selectImage"
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
          <button :disabled="listLoading" @click="importFolder">
            {{ listLoading ? "导入中…" : "从文件夹导入" }}
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
          <template v-for="grp in groupedImages" :key="grp.key">
            <optgroup v-if="grp.group !== null" :label="grp.label || '系列'">
              <option v-for="it in grp.items" :key="it.id" :value="it.id">
                {{ it.info.filename }}
              </option>
            </optgroup>
            <option v-else :key="grp.items[0].id" :value="grp.items[0].id">
              {{ grp.items[0].info.filename }}
            </option>
          </template>
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

    <!-- 序列选择对话框（文件夹导入含多个序列时弹出） -->
    <div v-if="seriesSelectOpen" class="modal-mask" @click.self="closeSeriesSelect">
      <div class="modal series-select">
        <div class="ss-head">
          <h2>选择要显示的序列</h2>
          <button class="modal-close-x" @click="closeSeriesSelect" aria-label="关闭">×</button>
        </div>
        <div class="ss-body">
          <div v-for="st in selectStudies" :key="st.studyUid || 'others'" class="ss-study">
            <div class="ss-study-title">
              <span class="ss-mod">{{ st.studyUid == null ? "其他影像文件" : st.patientName || "未知患者" }}</span>
              <span class="ss-sub" v-if="st.studyUid != null">
                Study {{ st.studyUid.slice(-12) }}
                <template v-if="st.studyDate"> · {{ st.studyDate }}</template>
                <template v-if="st.patientId"> · {{ st.patientId }}</template>
              </span>
            </div>
            <button
              v-for="s in st.series"
              :key="s.seriesUid || s.seriesDescription || 's'"
              class="ss-row"
              :class="{ active: selectedSeries === s }"
              @click="pickSeries(s)"
              @dblclick="pickSeries(s); confirmSeries()"
            >
              <span class="ss-row-main">
                <b>{{ s.modality || "?" }}<template v-if="s.seriesNumber != null"> #{{ s.seriesNumber }}</template></b>
                <span class="ss-desc">{{ s.seriesDescription || "未命名序列" }}</span>
              </span>
              <span class="ss-row-meta">
                <span v-if="s.seriesDate">序列日期 {{ s.seriesDate }}</span>
                <span v-if="s.patientName">患者 {{ s.patientName }}</span>
                <span>{{ s.fileCount }} 文件</span>
              </span>
            </button>
          </div>
        </div>
        <div class="ss-foot">
          <button class="ss-cancel" @click="closeSeriesSelect">取消</button>
          <button class="ss-ok" :disabled="!selectedSeries" @click="confirmSeries">打开所选序列</button>
        </div>
      </div>
    </div>

    <!-- 详情对话框 -->
    <div v-if="detailsOpen" class="modal-mask" @click.self="detailsOpen = false">
        <div class="modal details">
          <button class="modal-close-x" @click="detailsOpen = false" aria-label="关闭">×</button>
          <div class="details-head">
            <h2>文件标签信息</h2>
            <span class="details-file">{{ detailsTags?.filename }}</span>
            <span class="status-spacer"></span>
            <button
              class="modal-export"
              :disabled="!detailsTags || exportingTags"
              @click="exportTags"
            >
              {{ exportingTags ? "导出中…" : "导出" }}
            </button>
            <input
              v-model="detailsQuery"
              class="details-search"
              placeholder="搜索标签 / 关键字 / 值…"
            />
          </div>
        <div class="details-body">
          <div
            v-if="exportTagsMsg"
            class="export-tags-msg"
            :class="{ ok: exportTagsMsg.startsWith('已导出') }"
          >
            {{ exportTagsMsg }}
          </div>
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
                <td>
                  {{ r.keyword }}
                  <span v-if="r.description" class="qmark"
                    >?
                    <span class="qtip">
                      <b>{{ r.keyword }}</b>
                      <span class="qtip-desc">{{ r.description }}</span>
                    </span>
                  </span>
                </td>
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
  position: relative;
}
.details-head {
  display: flex;
  align-items: center;
  gap: 12px;
  /* 右侧预留空间，避免搜索框压到右上角关闭叉 */
  padding: 14px 44px 14px 18px;
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
  width: 180px;
}
/* 右上角关闭叉 */
.modal-close-x {
  position: absolute;
  top: 8px;
  right: 10px;
  width: 26px;
  height: 26px;
  border: none;
  background: transparent;
  color: var(--fg-dim);
  font-size: 20px;
  line-height: 1;
  border-radius: 6px;
  cursor: pointer;
  z-index: 2;
}
.modal-close-x:hover {
  background: var(--bg);
  color: var(--fg);
}
/* 导出格式与按钮 */
.details-export-fmt {
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 5px 8px;
  font-size: 12px;
}
.modal-export {
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: 6px;
  padding: 5px 12px;
  cursor: pointer;
  font-size: 12px;
}
.modal-export:disabled {
  opacity: 0.5;
  cursor: default;
}
.export-tags-msg {
  padding: 8px 18px;
  font-size: 12px;
  color: #e5484d;
  border-bottom: 1px solid var(--border);
}
.export-tags-msg.ok {
  color: #2e9e5b;
}
/* 关键字后的圆形问号与悬停说明 */
.qmark {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  margin-left: 6px;
  border-radius: 50%;
  background: var(--fg-dim);
  color: var(--panel);
  font-size: 10px;
  cursor: help;
  position: relative;
  vertical-align: middle;
}
.qtip {
  display: none;
  position: absolute;
  left: 18px;
  top: 50%;
  transform: translateY(-50%);
  width: 260px;
  padding: 8px 10px;
  background: #1c1c22;
  color: #f0f0f3;
  border: 1px solid var(--border);
  border-radius: 8px;
  font-size: 11px;
  line-height: 1.5;
  z-index: 80;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  white-space: normal;
}
.qmark:hover .qtip {
  display: block;
}
.qtip-desc {
  display: block;
  margin: 2px 0;
  color: #cdd2ff;
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

/* 序列选择对话框 */
.modal.series-select {
  width: min(720px, 92vw);
  max-height: 84vh;
  display: flex;
  flex-direction: column;
  padding: 0;
  position: relative;
}
.ss-head {
  display: flex;
  align-items: center;
  padding: 14px 44px 14px 18px;
  border-bottom: 1px solid var(--border);
}
.ss-head h2 {
  margin: 0;
  font-size: 16px;
}
.ss-body {
  overflow: auto;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.ss-study {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ss-study-title {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 2px 4px;
}
.ss-study-title .ss-mod {
  font-size: 13px;
  color: var(--fg);
  font-weight: 600;
}
.ss-study-title .ss-sub {
  font-size: 11px;
  color: var(--fg-dim);
}
.ss-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  text-align: left;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 12px;
  cursor: pointer;
  font-size: 12px;
}
.ss-row:hover {
  border-color: var(--fg-dim);
}
.ss-row.active {
  background: var(--accent);
  color: #fff;
  border-color: var(--accent);
}
.ss-row.active .ss-desc,
.ss-row.active .ss-row-meta {
  color: rgba(255, 255, 255, 0.85);
}
.ss-row-main {
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
}
.ss-row-main b {
  white-space: nowrap;
}
.ss-desc {
  color: var(--fg-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ss-row-meta {
  display: flex;
  gap: 10px;
  flex: 0 0 auto;
  color: var(--fg-dim);
  font-size: 11px;
  white-space: nowrap;
}
.ss-foot {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  padding: 12px 18px;
  border-top: 1px solid var(--border);
}
.ss-cancel {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 7px 16px;
  cursor: pointer;
  font-size: 13px;
}
.ss-cancel:hover {
  color: var(--fg);
  border-color: var(--fg-dim);
}
.ss-ok {
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: 6px;
  padding: 7px 18px;
  cursor: pointer;
  font-size: 13px;
}
.ss-ok:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
