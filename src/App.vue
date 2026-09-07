<script setup lang="ts">
import { ref, computed, reactive, onMounted, nextTick, watch } from "vue";
import Viewer from "./components/Viewer.vue";
import HelpModal from "./components/HelpModal.vue";
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
  BatchResult,
  BatchProgress,
  AnonDecrypted,
} from "./types";
import { decodePixelBytes } from "./types";
import { open, save } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
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
const openMenu = ref<"file" | "help" | "proc" | null>(null);
function toggleMenu(m: "file" | "help" | "proc") {
  openMenu.value = openMenu.value === m ? null : m;
}
function closeMenu() {
  openMenu.value = null;
}

// 关于对话框
const aboutOpen = ref(false);
const helpOpen = ref(false);
function openHelp() {
  helpOpen.value = true;
  openMenu.value = null;
}
const appVersion = ref("0.1.0");
function openAbout() {
  aboutOpen.value = true;
  closeMenu();
}

// ===== 批量转换 =====
const batchOpen = ref(false);
const batchInputDir = ref("");
const batchInputType = ref<"dicom" | "nifti">("dicom");
const batchOutputDir = ref("");
const batchOutputType = ref<"dicom" | "nifti">("nifti");
const batchRunning = ref(false);
const batchCancelling = ref(false);
const batchDone = ref(false);
const batchLog = ref<BatchProgress[]>([]);
const batchProgress = ref<{ k: number; n: number; label: string } | null>(null);
const progressPercent = computed(() => {
  const p = batchProgress.value;
  return p && p.n > 0 ? Math.round((p.k / p.n) * 100) : 0;
});
const batchSummary = ref<string | null>(null);
const batchError = ref<string | null>(null);

// 输出选项（复刻导出 DICOM / 导出 NIfTI 的选项）
const tsOptions = [
  { value: "explicit", label: "未压缩（显式 VR）" },
  { value: "implicit", label: "未压缩（隐式 VR）" },
  { value: "rle", label: "RLE 无损" },
  { value: "jpegls_lossless", label: "JPEG-LS 无损" },
  { value: "jpegls_loss", label: "JPEG-LS 有损（近无损）" },
  { value: "htj2k_lossless", label: "HTJ2K 无损" },
  { value: "htj2k_lossy", label: "HTJ2K 有损" },
] as const;
const anonGroups = [
  {
    id: "patient",
    label: "患者身份",
    tip:
      "PatientName (0010,0010)\nPatientID (0010,0020)\nPatientBirthDate (0010,0030)\nPatientAddress (0010,1040)\nPatientTelephoneNumbers (0010,2154)\nOtherPatientIDs (0010,1000)",
  },
  {
    id: "personnel",
    label: "人员身份",
    tip:
      "ReferringPhysicianName (0008,0090)\nPerformingPhysicianName (0008,1050)\nOperatorsName (0008,1070)\nNameOfPhysiciansReadingStudy (0008,1060)\nConsultingPhysicianName (0008,009C)",
  },
  {
    id: "institution",
    label: "机构信息",
    tip:
      "InstitutionName (0008,0080)\nInstitutionAddress (0008,0081)\nInstitutionalDepartmentName (0008,1040)\nStationName (0008,1010)",
  },
  {
    id: "device",
    label: "设备信息",
    tip:
      "Manufacturer (0008,0070)\nManufacturerModelName (0008,1090)\nDeviceSerialNumber (0018,1000)",
  },
  {
    id: "datetime",
    label: "日期时间",
    tip:
      "StudyDate (0008,0020)\nStudyTime (0008,0030)\nAcquisitionDate (0008,0022)\nAcquisitionTime (0008,0032)\nSeriesDate (0008,0021)\nSeriesTime (0008,0031)",
  },
  {
    id: "uid",
    label: "唯一标识",
    tip:
      "StudyInstanceUID (0020,000D)\nSeriesInstanceUID (0020,000E)\nSOPInstanceUID (0008,0018)\nFrameOfReferenceUID (0020,0052)\nAcquisitionUID (0008,0017)\nAccessionNumber (0008,0050)",
  },
  {
    id: "freetext",
    label: "自由文本",
    tip:
      "StudyDescription (0008,1030)\nSeriesDescription (0008,103E)\nImageComments (0020,4000)\nAdditionalPatientHistory (0010,21B0)\nIdentifyingComments (0008,4000)\nAcquisitionProtocolDescription (0018,9424)",
  },
] as const;
const anonMethodOptions = (gid: string) => {
  const base = [
    { value: "keep", label: "保留" },
    { value: "delete", label: "删除" },
    { value: "hash", label: "MD5 摘要" },
    { value: "encrypt", label: "加密" },
  ] as const;
  if (gid === "uid") return [...base, { value: "regenerate", label: "重生成 UID" }] as const;
  return base;
};
const niftiTypeOptions = [
  { value: "int16", label: "int16（默认 · HU 整数 · 无损当 HU∈[-32768,32767]）" },
  { value: "int32", label: "int32（HU 整数 · 范围大）" },
  { value: "uint16", label: "uint16（偏移 +1024 · HU∈[-1024,64511]）" },
  { value: "uint8", label: "uint8（线性映射到 0-255 · 强制有损）" },
  { value: "float32", label: "float32（HU 浮点 · 无损 · 体积大）" },
  { value: "float64", label: "float64（高精度 · 体积最大）" },
] as const;

const batchTs = ref<string>("explicit");
const batchTsDegree = ref<number>(90);
const batchAnonMap = reactive<Record<string, string>>(
  Object.fromEntries(anonGroups.map((g) => [g.id, "keep"]))
);
const batchAnonPassword = ref<string>("unixel");
// 方案A「还原重脱敏」：批量导出 DICOM 时，填入原脱敏密码可还原本工具加密脱敏值后再重脱敏。
const batchAnonRestorePassword = ref<string>("");
// —— 悬浮说明气泡（批量脱敏 / 文件信息标签共用一套实现）——
// .batch-body 与 .details-body 都有 overflow:auto，会裁剪 position:absolute 的气泡，
// 故统一 Teleport 到 body 用 position:fixed，坐标由触发元素的 getBoundingClientRect() 计算。
// 结构与视觉统一：粗体标题 + 逐行 .qtip-desc 明细（与 .qmark 圆形问号配套）。
type TipState = {
  show: boolean;
  x: number;
  y: number;
  flip: boolean; // 下方空间不足时向上翻转
  title: string;
  lines: string[];
};
const TIP_WIDTH = 260;
const TIP_HALF = TIP_WIDTH / 2 + 8; // 半宽 + 边框/余量，用于水平钳制

function newTip(): TipState {
  return { show: false, x: 0, y: 0, flip: false, title: "", lines: [] };
}
// 在触发元素下方（空间不足则翻到上方）定位气泡，并做水平钳制避免溢出视口
function placeTip(tip: TipState, el: HTMLElement, title: string, text: string) {
  const r = el.getBoundingClientRect();
  const minX = TIP_HALF;
  const maxX = Math.max(minX, window.innerWidth - TIP_HALF);
  tip.x = Math.min(Math.max(r.left + r.width / 2, minX), maxX);
  tip.title = title;
  tip.lines = text.split("\n").filter((s) => s.trim().length > 0);
  // 估算高度（标题行 + 明细行 + 内边距），决定向下弹还是向上弹
  const est = 16 + tip.lines.length * 17 + 18;
  const below = r.bottom + 6;
  tip.flip = below + est > window.innerHeight && r.top - 6 - est > 0;
  tip.y = tip.flip ? r.top - 6 : below;
  tip.show = true;
}

// 批量转换：脱敏范围说明气泡
const anonTip = reactive<TipState>(newTip());
function showAnonTip(e: MouseEvent | FocusEvent, title: string, text: string) {
  placeTip(anonTip, e.currentTarget as HTMLElement, title, text);
}
function hideAnonTip() {
  anonTip.show = false;
}
// 文件信息对话框：标签行圆形问号的悬停说明。
// 用单个共享节点而非每行渲染一个隐藏气泡 —— 行数可达数百，且行内气泡会被 .details-body
// 的 overflow:auto 裁剪。
const tagTip = reactive<TipState>(newTip());
function showTagTip(e: MouseEvent | FocusEvent, title: string, text: string) {
  placeTip(tagTip, e.currentTarget as HTMLElement, title, text);
}
function hideTagTip() {
  tagTip.show = false;
}
const batchNiiType = ref<string>("int16");
const batchNiiSform = ref(true);
const batchNiiGz = ref(true);

const batchOutputIsDicom = computed(() => batchOutputType.value === "dicom");
const batchTsNeedsDegree = computed(
  () => batchTs.value === "htj2k_lossy" || batchTs.value === "jpegls_loss"
);
const batchAnonNeedPassword = computed(() =>
  Object.values(batchAnonMap).some((m) => m === "encrypt")
);
const batchStartDisabled = computed(() => {
  if (batchRunning.value) return true;
  if (!batchInputDir.value || !batchOutputDir.value) return true;
  // NIfTI → NIfTI 不允许（后端同样拒绝）
  if (batchInputType.value === "nifti" && batchOutputType.value === "nifti") return true;
  return false;
});
const niftiTypeHint = computed(() => {
  switch (batchNiiType.value) {
    case "uint8":
      return "uint8 为强制有损：HU 线性映射到 0-255 并裁剪，仅适合预览。";
    case "int16":
      return "int16 无损当 HU∈[-32768,32767]；超出将截断（后端会提示）。";
    case "uint16":
      return "uint16 需 +1024 偏移后再存；HU<-1024 或 >64511 将截断（后端会提示）。";
    case "int32":
      return "int32 范围大，基本无截断风险，但体积极大。";
    default:
      return "浮点类型无损保留 HU，但体积显著大于整数类型。";
  }
});

function openBatchConvert() {
  batchOpen.value = true;
  batchAnonRestorePassword.value = "";
  closeMenu();
}
function closeBatchConvert() {
  if (batchRunning.value) return; // 运行中禁止关闭，使用「取消」中止
  batchOpen.value = false;
}
async function pickBatchInputDir() {
  const dir = await open({ directory: true, title: "选择输入文件夹" });
  if (dir && !Array.isArray(dir)) batchInputDir.value = dir as string;
}
async function pickBatchOutputDir() {
  const dir = await open({ directory: true, title: "选择输出文件夹" });
  if (dir && !Array.isArray(dir)) batchOutputDir.value = dir as string;
}
function buildBatchOptions() {
  const anonRanges = anonGroups
    .map((g) => ({ id: g.id, method: batchAnonMap[g.id] }))
    .filter((r) => r.method && r.method !== "keep");
  if (batchOutputIsDicom.value) {
    return {
      transferSyntax: batchTs.value,
      anonRanges,
      password: batchAnonPassword.value,
      restorePassword: batchAnonRestorePassword.value,
      forceLayered: false,
      datatype: "int16",
      writeSform: true,
      gz: true,
      quality: batchTsNeedsDegree.value ? batchTsDegree.value : 90,
    };
  }
  return {
    transferSyntax: "explicit",
    anonRanges: [],
    password: "",
    forceLayered: false,
    datatype: batchNiiType.value,
    writeSform: batchNiiSform.value,
    gz: batchNiiGz.value,
    quality: 90,
  };
}
async function startBatch() {
  if (batchStartDisabled.value) return;
  batchRunning.value = true;
  batchCancelling.value = false;
  batchDone.value = false;
  batchLog.value = [];
  batchProgress.value = null;
  batchSummary.value = null;
  batchError.value = null;
  try {
    const res = await invoke<BatchResult>("batch_convert", {
      args: {
        inputDir: batchInputDir.value,
        inputType: batchInputType.value,
        outputDir: batchOutputDir.value,
        outputType: batchOutputType.value,
        options: buildBatchOptions(),
      },
    });
    batchDone.value = true;
    batchSummary.value =
      `完成：成功 ${res.ok} · 失败 ${res.failed} · 共 ${res.total}` +
      (res.cancelled ? "（已取消）" : "");
  } catch (e) {
    batchError.value =
      "批量转换失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    batchRunning.value = false;
    batchCancelling.value = false;
  }
}
async function cancelBatch() {
  if (!batchRunning.value) return;
  batchCancelling.value = true;
  try {
    await invoke("batch_convert_cancel");
  } catch {
    /* 忽略：取消命令本身失败不影响前端状态 */
  }
}

// 详情对话框
const detailsOpen = ref(false);
const detailsLoading = ref(false);
const detailsTags = ref<FileTags | null>(null);
const detailsError = ref<string | null>(null);
const detailsQuery = ref("");
// 加密脱敏解密
const anonPwd = ref("");
const anonDecrypting = ref(false);
const anonDecryptMsg = ref<string | null>(null);
const anonDecrypted = ref<AnonDecrypted[] | null>(null);
const anonDecryptedSet = ref<Set<string>>(new Set());

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
  else if (kind === "dicom") {
    // 二进制像素传输：meta 走 JSON（极小），像素经 responseType:'binary' 取 ArrayBuffer，
    // 避免 Tauri 默认把 Vec<u8> 序列化为 number[]（大切片可达数百万数字，解析极慢）。
    const meta = await invoke<DicomMeta>("load_dicom_meta", { path });
    const buf = (await invoke(
      "load_dicom_pixels",
      { path },
      { responseType: "binary" } as any,
    )) as unknown as ArrayBuffer;
    img = { meta, pixelBytes: new Uint8Array(buf) };
  } else img = await invoke<DicomImage>("load_image", { path });
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
    closeMenu();
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
    closeMenu();
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
    await decodeInto(list[0]);
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

// 当前系列有序完整文件路径（"所有"导出时逐片传给后端）
const seriesPaths = computed(() => seriesFiles.value.map((i) => i.info.path));

// 解码单个图像像素并上台显示，同时后台预取相邻切片（结果缓存到 item.view）。
async function decodeInto(item: OpenedImage): Promise<void> {
  if (item.view) {
    currentView.value = item.view;
    prefetchNeighbors(item.id);
    return;
  }
  const img = await loadByKind(item.info.path, item.info.kind);
  item.view = img;
  currentView.value = img;
  prefetchNeighbors(item.id);
}

// 计算同系列内 ±k 个相邻切片 id（按 seriesFiles 顺序）。
function neighborIds(id: number, k: number): number[] {
  const ids = seriesFiles.value.map((s) => s.id);
  const idx = ids.indexOf(id);
  if (idx < 0) return [];
  const out: number[] = [];
  for (let d = -k; d <= k; d++) {
    if (d === 0) continue;
    const j = idx + d;
    if (j >= 0 && j < ids.length) out.push(ids[j]);
  }
  return out;
}

// 预取相邻切片：后台解码写缓存（不切换 currentView），拖动到时即瞬时显示。
function prefetchNeighbors(id: number) {
  for (const nid of neighborIds(id, 2)) {
    const it = imageList.value.find((i) => i.id === nid);
    if (it && !it.view) {
      loadByKind(it.info.path, it.info.kind)
        .then((v) => {
          it.view = v;
        })
        .catch(() => {});
    }
  }
}

// 切片解码调度：选中即时更新（滚动条缩略块实时跟随）；解码采用「单飞 + 合并」策略——
// 同一时刻只解码一片，快速拖动只保留落点切片，杜绝重复/并发解码挤占 CPU。
// 解码期间保留上一帧显示（currentView 常驻）→ Viewer 绝不闪烁。
let decodeInFlight = false;
let pendingDecodeId: number | null = null;

function selectImage(id: number) {
  if (id === activeId.value && currentView.value) return;
  activeId.value = id; // 立即更新选中，缩略块实时跟随
  closeMenu();
  ensureDecode(id);
}

function ensureDecode(id: number) {
  const item = imageList.value.find((i) => i.id === id);
  if (!item) return;
  if (item.view) {
    currentView.value = item.view;
    prefetchNeighbors(id);
    return;
  }
  if (decodeInFlight) {
    pendingDecodeId = id; // 拖动中：只记最新落点，待当前解码完成后继续
    return;
  }
  startDecode(id);
}

function startDecode(id: number) {
  const item = imageList.value.find((i) => i.id === id);
  if (!item) return;
  if (item.view) {
    currentView.value = item.view;
    prefetchNeighbors(id);
    return;
  }
  decodeInFlight = true;
  const seq = ++loadSeq;
  decodeInto(item)
    .then(() => {
      decodeInFlight = false;
      if (seq !== loadSeq) return;
      // 合并：拖动期间又请求了其他切片，继续解码最新落点
      if (pendingDecodeId !== null && pendingDecodeId !== id) {
        const p = pendingDecodeId;
        pendingDecodeId = null;
        startDecode(p);
      }
    })
    .catch((e) => {
      decodeInFlight = false;
      if (seq !== loadSeq) return;
      if (pendingDecodeId !== null && pendingDecodeId !== id) {
        const p = pendingDecodeId;
        pendingDecodeId = null;
        startDecode(p);
        return;
      }
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
  anonPwd.value = "";
  anonDecrypting.value = false;
  anonDecryptMsg.value = null;
  anonDecrypted.value = null;
  anonDecryptedSet.value = new Set();
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

// 对经过加密脱敏的 DICOM，用密码解密还原标签原始值并回填表格
async function decryptAnon() {
  if (!currentPath.value) return;
  anonDecrypting.value = true;
  anonDecryptMsg.value = null;
  try {
    const res = await invoke<AnonDecrypted[]>("decrypt_anon", {
      path: currentPath.value,
      password: anonPwd.value,
    });
    anonDecrypted.value = res;
    const map = new Map(res.map((r) => [r.tag, r.value]));
    const decSet = new Set<string>();
    if (detailsTags.value) {
      for (const r of detailsTags.value.rows) {
        if (map.has(r.tag)) {
          r.value = map.get(r.tag)!;
          decSet.add(r.tag);
        }
      }
    }
    anonDecryptedSet.value = decSet;
    anonDecryptMsg.value = `✓ 已解密 ${res.length} 个标签，原始值已回填`;
  } catch (e) {
    anonDecrypted.value = null;
    anonDecryptedSet.value = new Set();
    anonDecryptMsg.value =
      "解密失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    anonDecrypting.value = false;
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
  // 监听后端批量转换进度事件（命令运行期间实时推送）
  try {
    await listen<BatchProgress>("batch-progress", (e) => {
      // 实时更新进度条（逐帧）
      batchProgress.value = { k: e.payload.k, n: e.payload.n, label: e.payload.label };
      // 仅在完成/失败（ok 或 error 有意义）时写日志，避免逐帧刷屏
      if (e.payload.ok || e.payload.error) {
        batchLog.value.push(e.payload);
        if (batchLog.value.length > 400) {
          batchLog.value.splice(0, batchLog.value.length - 400);
        }
      }
    });
  } catch {
    /* 非 Tauri 环境忽略 */
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
            <button @click="openFile">打开文件</button>
            <button @click="importFolder">从文件夹导入</button>
          </div>
        </div>
        <div class="menu" :class="{ open: openMenu === 'proc' }" @click="toggleMenu('proc')">
          处理 <span class="caret">▾</span>
          <div v-if="openMenu === 'proc'" class="dropdown" @click.stop>
            <button @click="openBatchConvert">批量转换</button>
          </div>
        </div>
        <div class="menu" :class="{ open: openMenu === 'help' }" @click="toggleMenu('help')">
          帮助 <span class="caret">▾</span>
          <div v-if="openMenu === 'help'" class="dropdown" @click.stop>
            <button @click="openHelp">使用文档</button>
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
          :series-paths="seriesPaths"
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
          <button :disabled="batchRunning" @click="openBatchConvert">批量转换</button>
          <button class="ghost" @click="useMock">加载示例数据</button>
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

    <!-- 使用文档弹窗 -->
    <HelpModal :visible="helpOpen" @close="helpOpen = false" />

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

    <!-- 批量转换对话框 -->
    <div v-if="batchOpen" class="modal-mask" @click.self="closeBatchConvert">
      <div class="modal batch">
        <div class="batch-head">
          <h2>批量转换</h2>
          <button class="modal-close-x" type="button" @click="closeBatchConvert" aria-label="关闭">×</button>
        </div>

        <div class="batch-body">
          <!-- 区① 输入 -->
          <section class="batch-zone">
            <h3>① 输入</h3>
            <div class="batch-field">
              <span class="batch-label">输入文件夹</span>
              <button class="batch-pick" :disabled="batchRunning" @click="pickBatchInputDir">选择…</button>
              <span class="batch-dir" :title="batchInputDir">{{ batchInputDir || "未选择" }}</span>
            </div>
            <div class="batch-field">
              <span class="batch-label">输入类型</span>
              <label class="radio"><input type="radio" value="dicom" v-model="batchInputType" :disabled="batchRunning" /> DICOM</label>
              <label class="radio"><input type="radio" value="nifti" v-model="batchInputType" :disabled="batchRunning" /> NIfTI</label>
            </div>
          </section>

          <!-- 区② 输出 -->
          <section class="batch-zone">
            <h3>② 输出</h3>
            <div class="batch-field">
              <span class="batch-label">输出文件夹</span>
              <button class="batch-pick" :disabled="batchRunning" @click="pickBatchOutputDir">选择…</button>
              <span class="batch-dir" :title="batchOutputDir">{{ batchOutputDir || "未选择" }}</span>
            </div>
            <div class="batch-field">
              <span class="batch-label">输出类型</span>
              <label class="radio"><input type="radio" value="dicom" v-model="batchOutputType" :disabled="batchRunning" /> DICOM</label>
              <label class="radio"><input type="radio" value="nifti" v-model="batchOutputType" :disabled="batchRunning" /> NIfTI</label>
              <span v-if="batchInputType === 'nifti' && batchOutputType === 'nifti'" class="batch-warn">NIfTI → NIfTI 不支持</span>
            </div>
          </section>

          <!-- 区③ 动态输出选项 -->
          <section class="batch-zone">
            <h3>③ 输出选项</h3>
            <template v-if="batchOutputIsDicom">
              <div class="batch-field">
                <span class="batch-label">传输语法</span>
                <select v-model="batchTs" :disabled="batchRunning" class="batch-select">
                  <option v-for="o in tsOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
                </select>
              </div>
              <div v-if="batchTsNeedsDegree" class="batch-field">
                <span class="batch-label">有损程度</span>
                <input type="range" min="10" max="100" step="1" v-model.number="batchTsDegree" :disabled="batchRunning" />
                <span class="batch-degree">{{ batchTsDegree }}</span>
              </div>
              <div class="batch-anon">
                <div class="batch-anon-title">脱敏</div>
                <div class="batch-anon-grid">
                  <div class="batch-anon-item" v-for="g in anonGroups" :key="g.id">
                    <span class="batch-anon-name">{{ g.label }}</span>
                    <span
                      class="qmark anon-help"
                      tabindex="0"
                      role="img"
                      :aria-label="g.label + ' 脱敏范围说明'"
                      @mouseenter="showAnonTip($event, g.label, g.tip)"
                      @mouseleave="hideAnonTip"
                      @focus="showAnonTip($event, g.label, g.tip)"
                      @blur="hideAnonTip"
                      >?</span
                    >
                    <select v-model="batchAnonMap[g.id]" :disabled="batchRunning" class="batch-select">
                      <option v-for="m in anonMethodOptions(g.id)" :key="m.value" :value="m.value">{{ m.label }}</option>
                    </select>
                  </div>
                </div>
                <div v-if="batchAnonNeedPassword" class="batch-field">
                  <span class="batch-label">加密密码</span>
                  <input type="text" v-model="batchAnonPassword" :disabled="batchRunning" class="batch-input" placeholder="默认 unixel" />
                </div>
                <div class="batch-field">
                  <span class="batch-label">解密密码</span>
                  <input type="password" v-model="batchAnonRestorePassword" :disabled="batchRunning" class="batch-input" placeholder="还原已加密脱敏用，留空则直接处理" />
                </div>
              </div>
            </template>
            <template v-else>
              <div class="batch-field">
                <span class="batch-label">数据类型</span>
                <select v-model="batchNiiType" :disabled="batchRunning" class="batch-select">
                  <option v-for="o in niftiTypeOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
                </select>
              </div>
              <p class="batch-hint">{{ niftiTypeHint }}</p>
              <div class="batch-field">
                <label class="radio"><input type="checkbox" v-model="batchNiiSform" :disabled="batchRunning" /> 写入 sform（RAS 仿射）</label>
              </div>
              <div class="batch-field">
                <label class="radio"><input type="checkbox" v-model="batchNiiGz" :disabled="batchRunning" /> 输出 .nii.gz（gzip 压缩）</label>
              </div>
            </template>
          </section>

          <!-- 进度 -->
          <section class="batch-zone">
            <h3>进度</h3>
            <div class="batch-progress">
              <div class="batch-progress-track">
                <div class="batch-progress-fill" :style="{ width: progressPercent + '%' }"></div>
              </div>
              <span class="batch-progress-label" v-if="batchProgress">
                已处理 {{ batchProgress.k }} / {{ batchProgress.n }}（{{ progressPercent }}%）
                <span class="batch-progress-cur">· {{ batchProgress.label }}</span>
              </span>
              <span class="batch-progress-label" v-else-if="batchRunning">准备中…</span>
              <span class="batch-progress-label" v-else>尚未开始</span>
            </div>
            <div class="batch-log">
              <p v-if="!batchRunning && !batchDone && !batchError" class="batch-hint">配置完成后点击「开始转换」。</p>
              <p v-for="(p, i) in batchLog" :key="i" :class="['batch-log-line', p.ok ? 'ok' : 'fail']">
                [{{ p.k }}/{{ p.n }}] {{ p.label }} {{ p.ok ? '✓' : '✗ ' + (p.error || '') }}
                <br /><span class="batch-log-src">{{ p.out }}</span>
              </p>
              <p v-if="batchCancelling" class="batch-hint">正在取消…</p>
            </div>
            <p v-if="batchSummary" class="batch-summary">{{ batchSummary }}</p>
            <p v-if="batchError" class="batch-error">⚠ {{ batchError }}</p>
          </section>
        </div>

        <div class="batch-foot">
          <button class="ss-cancel" :disabled="batchRunning" @click="closeBatchConvert">关闭</button>
          <button v-if="!batchRunning" class="ss-ok" :disabled="batchStartDisabled" @click="startBatch">开始转换</button>
          <button v-else class="batch-cancel-btn" :disabled="batchCancelling" @click="cancelBatch">{{ batchCancelling ? "取消中…" : "取消" }}</button>
        </div>
      </div>
    </div>

    <Teleport to="body">
      <div
        v-if="anonTip.show"
        class="tip-pop"
        :class="{ flip: anonTip.flip }"
        :style="{ left: anonTip.x + 'px', top: anonTip.y + 'px' }"
      >
        <b>{{ anonTip.title }}</b>
        <span v-for="(line, i) in anonTip.lines" :key="i" class="qtip-desc">{{ line }}</span>
      </div>
      <div
        v-if="tagTip.show"
        class="tip-pop"
        :class="{ flip: tagTip.flip }"
        :style="{ left: tagTip.x + 'px', top: tagTip.y + 'px' }"
      >
        <b>{{ tagTip.title }}</b>
        <span v-for="(line, i) in tagTip.lines" :key="i" class="qtip-desc">{{ line }}</span>
      </div>
    </Teleport>

    <!-- 详情对话框 -->
    <div v-if="detailsOpen" class="modal-mask" @click.self="detailsOpen = false">
        <div class="modal details">
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
            <button class="modal-close-x" type="button" @click="detailsOpen = false" aria-label="关闭">&times;</button>
          </div>
        <div v-if="detailsTags?.encryptedAnon" class="anon-decrypt">
          <p class="anon-note">
            ⚠ 检测到本文件经过加密脱敏（{{ detailsTags.encryptedAnon }}）。输入导出时设置的密码可解密还原被隐藏标签的原始值。
          </p>
          <div class="anon-row">
            <input
              v-model="anonPwd"
              class="anon-input"
              type="password"
              placeholder="输入脱敏密码"
              @keyup.enter="decryptAnon"
            />
            <button class="anon-btn" :disabled="anonDecrypting" @click="decryptAnon">
              {{ anonDecrypting ? "解密中…" : "解密显示" }}
            </button>
          </div>
          <p v-if="anonDecryptMsg" class="anon-msg" :class="{ ok: anonDecryptMsg.startsWith('✓') }">
            {{ anonDecryptMsg }}
          </p>
        </div>
        <!-- 气泡是 fixed 定位，滚动时不会跟随，故滚动即收起，避免与触发图标错位 -->
        <div class="details-body" @scroll="hideTagTip">
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
                  <span
                    v-if="r.description"
                    class="qmark"
                    tabindex="0"
                    role="img"
                    :aria-label="r.keyword + ' 标签说明'"
                    @mouseenter="showTagTip($event, r.keyword, r.description)"
                    @mouseleave="hideTagTip"
                    @focus="showTagTip($event, r.keyword, r.description)"
                    @blur="hideTagTip"
                    >?</span
                  >
                </td>
                <td
                  class="val"
                  :class="{ expanded: expandedTags.has(r.tag), decrypted: anonDecryptedSet.has(r.tag) }"
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
  width: 168px;
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
  /* 固定高度（而非仅 max-height）：否则搜索过滤后行数变少，弹性子项 .details-body
     随之收缩，对话框会跟着「变矮」。固定高度可保证搜索前后高度恒定。 */
  height: 82vh;
  max-height: 82vh;
  display: flex;
  flex-direction: column;
  padding: 0;
  position: relative;
}
/* 标题栏：与导出对话框 .modal-title 风格一致（底色比内容区 --panel 略深一档，低对比区分） */
.details-head {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 20px;
  background: var(--titlebar-bg, var(--bg-2, #1c1f26));
  border-bottom: 1px solid var(--border);
  border-top-left-radius: 12px;
  border-top-right-radius: 12px;
}
.details-head h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
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
/* 标题栏关闭叉（与导出对话框 .modal-close 风格一致） */
.modal-close-x {
  flex: none;
  background: transparent;
  border: none;
  color: var(--fg-dim);
  font-size: 18px;
  line-height: 1;
  padding: 2px 7px;
  border-radius: 4px;
  cursor: pointer;
  transition: background 0.15s ease, color 0.15s ease;
}
.modal-close-x:hover {
  background: rgba(127, 127, 127, 0.18);
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
  flex: 0 0 auto;
}
/* 悬停说明气泡：两个滚动容器（.batch-body / .details-body）都有 overflow:auto，
   绝对定位的气泡会被裁剪，故统一 Teleport 到 body + position:fixed。
   视觉与结构由批量脱敏气泡、文件信息标签气泡共用。 */
.tip-pop {
  position: fixed;
  transform: translateX(-50%);
  width: 260px;
  padding: 8px 10px;
  background: #1c1c22;
  color: #f0f0f3;
  border: 1px solid var(--border);
  border-radius: 8px;
  font-size: 11px;
  line-height: 1.5;
  white-space: normal;
  text-align: left;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  z-index: 9999;
  pointer-events: none;
}
/* 下方空间不足时向上翻转（配合 placeTip 的 flip 判定） */
.tip-pop.flip {
  transform: translate(-50%, -100%);
}
.qtip-desc {
  display: block;
  margin: 2px 0;
  color: #cdd2ff;
}
.details-body {
  /* flex:1 + min-height:0 让 body 吃掉固定高度下的剩余空间并自行滚动；
     缺少 min-height:0 时 flex 子项不会收缩，内容溢出会顶开对话框。 */
  flex: 1 1 auto;
  min-height: 0;
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
/* 加密脱敏解密面板（文件标签信息对话框内） */
.anon-decrypt {
  flex: 0 0 auto;
  border-bottom: 1px solid var(--border);
  background: var(--bg);
  padding: 10px 14px;
}
.anon-note {
  margin: 0 0 8px;
  font-size: 12px;
  color: var(--fg-dim);
  line-height: 1.5;
}
.anon-row {
  display: flex;
  gap: 8px;
  align-items: center;
}
.anon-input {
  flex: 1;
  min-width: 0;
  padding: 6px 10px;
  font-size: 13px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel);
  color: var(--fg);
}
.anon-input:focus {
  outline: none;
  border-color: var(--accent, #2f9e6e);
}
.anon-btn {
  padding: 6px 16px;
  font-size: 13px;
  border: 1px solid var(--accent, #2f9e6e);
  border-radius: 6px;
  background: var(--accent, #2f9e6e);
  color: #fff;
  cursor: pointer;
  white-space: nowrap;
}
.anon-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.anon-msg {
  margin: 8px 0 0;
  font-size: 12px;
  color: #e5484d;
}
.anon-msg.ok {
  color: var(--accent, #2f9e6e);
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
.tags-table td.val.decrypted {
  color: var(--accent, #2f9e6e);
  font-weight: 600;
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
  justify-content: space-between;
  padding: 14px 20px;
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

/* 批量转换对话框 */
.modal.batch {
  width: min(640px, 94vw);
  max-height: 88vh;
  display: flex;
  flex-direction: column;
  padding: 0;
  position: relative;
}
.batch-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 20px;
  background: var(--titlebar-bg, var(--bg-2, #1c1f26));
  border-bottom: 1px solid var(--border);
  border-top-left-radius: 12px;
  border-top-right-radius: 12px;
}
.batch-head h2 {
  margin: 0;
  font-size: 16px;
}
.batch-body {
  overflow: auto;
  padding: 16px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.batch-zone {
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.batch-zone h3 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
  color: var(--fg-dim);
}
.batch-field {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 13px;
}
.batch-label {
  flex: 0 0 72px;
  color: var(--fg-dim);
}
.batch-dir {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: ui-monospace, "SFMono-Regular", Menlo, monospace;
  font-size: 12px;
  color: var(--fg);
}
.batch-pick {
  flex: 0 0 auto;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 6px 12px;
  cursor: pointer;
  font-size: 13px;
}
.batch-pick:hover:not(:disabled) {
  border-color: var(--fg-dim);
}
.batch-pick:disabled,
.batch-select:disabled,
.batch-input:disabled {
  opacity: 0.6;
  cursor: default;
}
.batch-select,
.batch-input {
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 6px 8px;
  font-size: 13px;
  flex: 1;
  min-width: 0;
  max-width: 320px;
}
.batch-degree {
  color: var(--fg-dim);
  font-size: 12px;
}
.radio {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 13px;
  cursor: pointer;
}
.batch-anon {
  display: flex;
  flex-direction: column;
  gap: 8px;
  border-top: 1px dashed var(--border);
  padding-top: 10px;
}
.batch-anon-title {
  font-size: 12px;
  color: var(--fg-dim);
}
.batch-anon-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px 18px;
}
.batch-anon-item {
  display: flex;
  align-items: center;
  gap: 8px;
}
.batch-anon-name {
  flex: 0 0 64px;
  font-size: 13px;
  color: var(--fg-dim);
}
.batch-progress {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-bottom: 10px;
}
.batch-progress-track {
  height: 10px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 999px;
  overflow: hidden;
}
.batch-progress-fill {
  height: 100%;
  background: #3b82f6;
  border-radius: 999px;
  transition: width 0.2s ease;
}
.batch-progress-label {
  font-size: 12px;
  color: var(--fg-dim);
}
.batch-progress-cur {
  color: var(--fg);
}
.batch-hint {
  margin: 0;
  font-size: 12px;
  color: var(--fg-dim);
  line-height: 1.5;
}
/* 脱敏范围说明：复用 .qmark 圆形问号，仅重置外边距（父级 .batch-anon-item 已用 gap 控制间距）；
   气泡视觉见上方共享的 .tip-pop（Teleport 到 body，固定定位，不被滚动容器裁剪） */
.anon-help {
  margin-left: 0;
}
.anon-help:hover,
.anon-help:focus {
  outline: none;
}
.batch-warn {
  color: #e0a000;
  font-size: 12px;
}
.batch-log {
  max-height: 200px;
  overflow: auto;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 10px;
  font-family: ui-monospace, "SFMono-Regular", Menlo, monospace;
  font-size: 11px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.batch-log-line {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-all;
}
.batch-log-line.ok {
  color: #2e9e5b;
}
.batch-log-line.fail {
  color: #e5484d;
}
.batch-log-src {
  color: var(--fg-dim);
}
.batch-summary {
  margin: 0;
  font-size: 13px;
  color: var(--fg);
}
.batch-error {
  margin: 0;
  font-size: 13px;
  color: #e5484d;
}
.batch-foot {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  padding: 12px 18px;
  border-top: 1px solid var(--border);
}
.batch-cancel-btn {
  background: #c0392b;
  color: #fff;
  border: none;
  border-radius: 6px;
  padding: 7px 18px;
  cursor: pointer;
  font-size: 13px;
}
.batch-cancel-btn:disabled {
  opacity: 0.6;
  cursor: default;
}
</style>
