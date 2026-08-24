<script setup lang="ts">
import { ref, computed, reactive, onMounted, onUnmounted, watch } from "vue";
import type { DicomMeta } from "../types";
import { applyWindow } from "../windowing";
import { save, open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";

const props = defineProps<{
  meta: DicomMeta;
  frames: Float32Array[]; // 每帧 HU 数组，长度 = width*height
  // 当前选中文件所属系列的有序文件列表（按位置排序）；长度>1 时画布右侧竖条列出切片便于快速切换
  seriesFiles?: Array<{ id: number; info: { filename: string } }>;
  // 当前系列有序完整文件路径（"所有"导出时逐片传给后端）
  seriesPaths?: string[];
  activeId?: number | null;
}>();
const emit = defineEmits<{ selectFile: [id: number] }>();

const canvasRef = ref<HTMLCanvasElement | null>(null);
const wc = ref(props.meta.windowCenter);
const ww = ref(props.meta.windowWidth);
const frameIndex = ref(0);
const zoom = ref(1);
const pan = ref({ x: 0, y: 0 });

// 画布右侧竖向滚动条：位置对应图像序号（多帧帧号 / 同系列切片序号），
// 滚动 / 拖拽即直接切换对应位置的图像（不渲染序号按钮）。
// 方向已反转：滚动条「顶部 = 最后一个切片」、「底部 = 第 0 个切片」。
const scrollRef = ref<HTMLElement | null>(null);
const trackH = ref(0);

const stripItems = computed(() => {
  const items: Array<{ type: "frame" | "series"; index?: number; id?: number }> = [];
  if (props.meta.frames > 1) {
    for (let f = 0; f < props.meta.frames; f++) {
      items.push({ type: "frame", index: f });
    }
  } else if (props.seriesFiles && props.seriesFiles.length > 1) {
    props.seriesFiles.forEach((s) => {
      items.push({ type: "series", id: s.id });
    });
  }
  return items;
});

// 当前显示的图像在 stripItems 中的序号
const currentIndex = computed(() => {
  const items = stripItems.value;
  for (let i = 0; i < items.length; i++) {
    const it = items[i];
    if (it.type === "frame" && it.index === frameIndex.value) return i;
    if (it.type === "series" && it.id === props.activeId) return i;
  }
  return 0;
});

// 滚动条→图像序号：滚动/拖拽直接切换，无中间态
function goToIndex(i: number) {
  const it = stripItems.value[i];
  if (!it) return;
  if (it.type === "frame") frameIndex.value = it.index ?? 0;
  else if (it.id != null) emit("selectFile", it.id);
}

// 滑块：高度随图像数缩放，位置对应 currentIndex
const thumbStyle = computed(() => {
  const n = stripItems.value.length;
  const track = trackH.value;
  if (n <= 1 || track <= 0) return { height: "100%", transform: "translateY(0)" };
  const minThumb = 28;
  const thumbH = Math.max(minThumb, Math.floor(track / n));
  const maxTop = Math.max(1, track - thumbH);
  // 反转：顶部(top=0) 对应最后一个切片，底部对应第 0 个
  const top = ((n - 1 - currentIndex.value) / (n - 1)) * maxTop;
  return { height: `${thumbH}px`, transform: `translateY(${top}px)` };
});

function yToIndex(clientY: number) {
  const el = scrollRef.value;
  if (!el) return 0;
  const rect = el.getBoundingClientRect();
  const frac = Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
  const n = stripItems.value.length;
  // 反转：顶部 (frac=0) 对应最后一个切片
  return Math.min(n - 1, Math.round((1 - frac) * (n - 1)));
}

let scrubbing = false;
function onScrollDown(e: PointerEvent) {
  if (stripItems.value.length <= 1) return;
  scrubbing = true;
  goToIndex(yToIndex(e.clientY));
  window.addEventListener("pointermove", onScrollMove);
  window.addEventListener("pointerup", onScrollUp);
}
function onScrollMove(e: PointerEvent) {
  if (!scrubbing) return;
  goToIndex(yToIndex(e.clientY));
}
function onScrollUp() {
  scrubbing = false;
  window.removeEventListener("pointermove", onScrollMove);
  window.removeEventListener("pointerup", onScrollUp);
}
function onScrollWheel(e: WheelEvent) {
  const n = stripItems.value.length;
  if (n <= 1) return;
  // 反转：向下滚动 (deltaY>0) 对应切片序号减小，与「顶部=最后切片」一致
  const dir = e.deltaY > 0 ? -1 : 1;
  goToIndex(Math.min(n - 1, Math.max(0, currentIndex.value + dir)));
}
function measureTrack() {
  trackH.value = scrollRef.value?.clientHeight ?? 0;
}

// 复用的 RGBA 缓冲
let rgba = new Uint8ClampedArray(props.meta.width * props.meta.height * 4);

function render() {
  const cv = canvasRef.value;
  if (!cv) return;
  const ctx = cv.getContext("2d");
  if (!ctx) return;
  const frame = props.frames[frameIndex.value];
  if (!frame || frame.length === 0) return;

  // 窗映射（按 photometric 处理 MONOCHROME1 反转）
  if (rgba.length !== frame.length * 4) {
    rgba = new Uint8ClampedArray(frame.length * 4);
  }
  applyWindow(frame, wc.value, ww.value, rgba, props.meta.photometric);

  // 离屏绘制原分辨率，再缩放上屏
  const off = document.createElement("canvas");
  off.width = props.meta.width;
  off.height = props.meta.height;
  const octx = off.getContext("2d");
  if (!octx) return;
  octx.putImageData(new ImageData(rgba, props.meta.width, props.meta.height), 0, 0);

  ctx.clearRect(0, 0, cv.width, cv.height);
  ctx.imageSmoothingEnabled = false;
  const dpr = window.devicePixelRatio || 1;
  const z = zoom.value;
  const dw = props.meta.width * z * dpr;
  const dh = props.meta.height * z * dpr;
  ctx.drawImage(off, pan.value.x + (cv.width - dw) / 2, pan.value.y + (cv.height - dh) / 2, dw, dh);
}

watch([wc, ww, frameIndex, zoom, pan], render);

// 切换图像（多帧帧号 / 同系列切片）时，校正帧索引并立即在位重绘。
// 由于 Viewer 常驻不复挂载，必须监听 meta/frames 才能随切片切换刷新画布。
watch(
  () => [props.meta, props.frames],
  () => {
    if (frameIndex.value > props.meta.frames - 1) {
      frameIndex.value = Math.max(0, props.meta.frames - 1);
    }
    render();
  }
);

// 画布自适应：把绘图缓冲同步为容器实际像素尺寸（含 DPR），窗口缩放时自动重绘
let resizeObserver: ResizeObserver | null = null;
const fitted = ref(false); // 仅首次加载自动适配一次，之后保留用户缩放

function resizeCanvas() {
  const cv = canvasRef.value;
  if (!cv) return;
  const rect = cv.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.floor(rect.width * dpr));
  const h = Math.max(1, Math.floor(rect.height * dpr));
  if (cv.width !== w || cv.height !== h) {
    cv.width = w;
    cv.height = h;
  }
}

// 自动适配：使整幅图像等比居中铺满可视区（contain），作为初始缩放
function fitToView() {
  const cv = canvasRef.value;
  if (!cv || !props.meta.width || !props.meta.height) return;
  const rect = cv.getBoundingClientRect();
  const fit = Math.min(rect.width / props.meta.width, rect.height / props.meta.height);
  zoom.value = fit > 0 ? Math.max(0.1, Math.min(8, fit)) : 1;
}

onMounted(() => {
  const cv = canvasRef.value;
  if (!cv) return;
  resizeCanvas();
  fitToView();
  render();
  measureTrack();
  resizeObserver = new ResizeObserver(() => {
    resizeCanvas();
    measureTrack();
    if (!fitted.value) {
      fitToView();
      fitted.value = true;
    }
    render();
  });
  resizeObserver.observe(cv);
});

onUnmounted(() => {
  resizeObserver?.disconnect();
  resizeObserver = null;
  if (scrubbing) {
    window.removeEventListener("pointermove", onScrollMove);
    window.removeEventListener("pointerup", onScrollUp);
  }
});

// 交互
let dragging = false;
let last = { x: 0, y: 0 };
function onWheel(e: WheelEvent) {
  const f = e.deltaY < 0 ? 1.1 : 1 / 1.1;
  zoom.value = Math.min(8, Math.max(0.1, zoom.value * f));
}
function onDown(e: MouseEvent) {
  dragging = true;
  last = { x: e.clientX, y: e.clientY };
}
function onMove(e: MouseEvent) {
  if (!dragging) return;
  pan.value = {
    x: pan.value.x + (e.clientX - last.x),
    y: pan.value.y + (e.clientY - last.y),
  };
  last = { x: e.clientX, y: e.clientY };
}
function onUp() {
  dragging = false;
}

function resetView() {
  zoom.value = 1;
  pan.value = { x: 0, y: 0 };
  wc.value = props.meta.windowCenter;
  ww.value = props.meta.windowWidth;
  frameIndex.value = 0;
}

// 导出 JPEG（带 DICOM 标签叠加 + 水印）
const exportJpegOpen = ref(false);
const exportScope = ref<"current" | "all">("current");
const exportQuality = ref(90);
const watermarkText = ref("");
const exporting = ref(false);
const exportMsg = ref<string | null>(null);
const isRealFile = computed(() => !props.meta.path.startsWith("mock"));
const isMultiframe = computed(() => props.meta.frames > 1);

// 可选叠加标签：固定角映射（0=左上 1=右上 2=左下 3=右下），同角内按列表顺序逐行（自上而下）
const selectableTags = [
  // 左上：机构名称 → 患者编号 → 患者姓名 → 患者性别
  { key: "InstitutionName", label: "机构名称", corner: 0 },
  { key: "PatientID", label: "患者编号", corner: 0 },
  { key: "PatientName", label: "患者姓名", corner: 0 },
  { key: "PatientSex", label: "患者性别", corner: 0 },
  // 右上：序列UID → 检查日期 → 模态 → 窗宽窗位
  { key: "SeriesInstanceUID", label: "序列UID", corner: 1 },
  { key: "StudyDate", label: "检查日期", corner: 1 },
  { key: "Modality", label: "模态", corner: 1 },
  { key: "__WINDOW__", label: "窗宽窗位", corner: 1 },
  // 左下：制造商 → 设备型号名称
  { key: "Manufacturer", label: "制造商", corner: 2 },
  { key: "ManufacturerModelName", label: "设备型号名称", corner: 2 },
] as const;
const cornerNames = ["左上", "右上", "左下", "右下"];
// 按角分组的标签（隐藏无标签的角，如右下专用于水印），供对话框四角分组渲染
const tagGroups = cornerNames
  .map((name, i) => ({
    name,
    tags: selectableTags.filter((t) => t.corner === i),
  }))
  .filter((g) => g.tags.length > 0);
const selectedTags = ref<string[]>([]);

async function onExportJpeg() {
  if (!isRealFile.value) return;
  let output: string | null;
  if (exportScope.value === "current") {
    const base = props.meta.filename.replace(
      /\.(dcm|dicom|nii(\.gz)?|j2c|jph|png|jpe?g|tif?f)$/i,
      ""
    );
    const suggested = `${base}_wc${Math.round(wc.value)}_ww${Math.round(
      ww.value
    )}.jpg`;
    output = await save({
      defaultPath: suggested,
      filters: [{ name: "JPEG", extensions: ["jpg", "jpeg"] }],
    });
  } else {
    output = await open({ directory: true, title: "选择导出文件夹" });
  }
  if (!output) return; // 用户取消
  exporting.value = true;
  exportMsg.value = null;
  try {
    const overlays = selectableTags
      .filter((t) => selectedTags.value.includes(t.key))
      .map((t) => ({ corner: t.corner, keyword: t.key, display: t.label }));
    const seriesPaths =
      exportScope.value === "all" && !isMultiframe.value
        ? props.seriesPaths ?? []
        : [];
    await invoke<string>("export_jpeg", {
      mode: exportScope.value,
      filePath: props.meta.path,
      seriesPaths,
      frameIndex: frameIndex.value,
      wc: wc.value,
      ww: ww.value,
      photometric: props.meta.photometric,
      overlays,
      watermark: watermarkText.value,
      quality: exportQuality.value,
      output,
    });
    exportMsg.value = "已导出：" + output;
  } catch (e) {
    exportMsg.value =
      "导出失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    exporting.value = false;
    exportJpegOpen.value = false;
  }
}

// 导出 DICOM（保留原始像素 + 可选脱敏；SoftwareVersions 后台自动写入 "Unixel - Hongwei Shao"）
const exportDicomOpen = ref(false);
const exportDicomScope = ref<"current" | "all">("current");
const exportTs = ref<string>("explicit");
const exportDicomQuality = ref(90);
const exportMultifile = ref(false);
const anonPassword = ref("");
const exportingDicom = ref(false);
const exportDicomMsg = ref<string | null>(null);

// 传输语法选项（与后端 ExportDicomArgs.transferSyntax 对齐）
const tsOptions = [
  { value: "explicit", label: "未压缩（显式 VR）" },
  { value: "implicit", label: "未压缩（隐式 VR）" },
  { value: "rle", label: "RLE 无损" },
  { value: "jpegls_lossless", label: "JPEG-LS 无损" },
  { value: "jpegls_loss", label: "JPEG-LS 有损（近无损）" },
  { value: "htj2k_lossless", label: "HTJ2K 无损" },
  { value: "htj2k_lossy", label: "HTJ2K 有损" },
] as const;
// 有损压缩方式（HTJ2K 有损 / JPEG-LS 有损）显示「有损程度」选择
const tsNeedsDegree = computed(
  () => exportTs.value === "htj2k_lossy" || exportTs.value === "jpegls_loss"
);

// 脱敏分组（id 与后端 ANON_GROUPS 一致）
const anonGroups = [
  { id: "patient", label: "患者" },
  { id: "institution", label: "机构" },
  { id: "personnel", label: "人员" },
  { id: "device", label: "设备" },
  { id: "datetime", label: "日期时间" },
  { id: "uid", label: "实例 UID" },
] as const;
// 各分组可选脱敏方式；uid 额外支持「重生成 UID」
const anonMethodOptions = (gid: string) => {
  const base = [
    { value: "keep", label: "保留" },
    { value: "delete", label: "删除" },
    { value: "hash", label: "MD5 摘要" },
    { value: "encrypt", label: "加密" },
  ] as const;
  if (gid === "uid") {
    return [...base, { value: "regenerate", label: "重生成 UID" }] as const;
  }
  return base;
};
// 当前各分组选定的方式（默认保留）
const anonMethodMap = reactive<Record<string, string>>(
  Object.fromEntries(anonGroups.map((g) => [g.id, "keep"]))
);
const anonNeedPassword = computed(() =>
  Object.values(anonMethodMap).some((m) => m === "encrypt")
);
// 「整个序列」输出方式仅在 all 模式有意义
const showSeriesOutput = computed(() => exportDicomScope.value === "all");

async function onExportDicom() {
  if (!isRealFile.value) return;
  // 收集脱敏范围（仅保留非「保留」的项）
  const anonRanges = anonGroups
    .map((g) => ({ id: g.id, method: anonMethodMap[g.id] }))
    .filter((r) => r.method && r.method !== "keep");

  // 输出目标：当前帧/整序列单文件 → 文件；整序列每帧单文件 → 目录
  let output: string | null;
  if (exportDicomScope.value === "current" || !exportMultifile.value) {
    const base = props.meta.filename.replace(
      /\.(dcm|dicom|nii(\.gz)?|j2c|jph|png|jpe?g|tif?f)$/i,
      ""
    );
    const suggested = `${base}_export.dcm`;
    output = await save({
      defaultPath: suggested,
      filters: [{ name: "DICOM", extensions: ["dcm"] }],
    });
  } else {
    output = await open({ directory: true, title: "选择导出文件夹（每帧单文件）" });
  }
  if (!output) return; // 用户取消

  // 整个序列且为多切片系列时，逐文件传给后端；多帧单文件不需要 seriesPaths
  const seriesPaths =
    exportDicomScope.value === "all" && !isMultiframe.value
      ? props.seriesPaths ?? []
      : [];

  exportingDicom.value = true;
  exportDicomMsg.value = null;
  try {
    const res = await invoke<string>("export_dicom", {
      args: {
        mode: exportDicomScope.value,
        filePath: props.meta.path,
        seriesPaths,
        frameIndex: frameIndex.value,
        transferSyntax: exportTs.value,
        quality: exportDicomQuality.value,
        wc: wc.value,
        ww: ww.value,
        anonRanges,
        password: anonPassword.value,
        output,
        multifile: exportMultifile.value,
      },
    });
    exportDicomMsg.value = res;
  } catch (e) {
    exportDicomMsg.value =
      "导出失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    exportingDicom.value = false;
    exportDicomOpen.value = false;
  }
}
</script>

<template>
  <div class="viewer">
    <div class="stage">
      <canvas
        ref="canvasRef"
        class="canvas"
        @wheel.prevent="onWheel"
        @mousedown="onDown"
        @mousemove="onMove"
        @mouseup="onUp"
        @mouseleave="onUp"
      />
      <div class="zoom-badge">{{ zoom.toFixed(2) }}x</div>
    </div>

    <!-- 多帧 / 同系列多切片 竖向滚动条：方向已反转（顶部=最后切片，底部=第0切片） -->
    <div
      class="frame-scroll"
      ref="scrollRef"
      v-if="stripItems.length > 1"
      :title="`${currentIndex + 1} / ${stripItems.length}`"
      @wheel.prevent="onScrollWheel"
      @pointerdown="onScrollDown"
    >
      <div class="frame-thumb" :style="thumbStyle"></div>
    </div>

    <aside class="side">
      <section class="group">
        <div class="group-title">
          <span>视图</span>
          <span class="group-actions">
            <button class="mini" @click="resetView">重置</button>
          </span>
        </div>
        <label>
          窗位
          <input type="range" :min="meta.huMin" :max="meta.huMax" step="1" v-model.number="wc" />
          <span class="val">{{ wc }}</span>
        </label>
        <label>
          窗宽
          <input
            type="range"
            :min="1"
            :max="Math.max(1, meta.huMax - meta.huMin)"
            step="1"
            v-model.number="ww"
          />
          <span class="val">{{ ww }}</span>
        </label>
        <label v-if="meta.frames > 1">
          帧
          <input type="range" min="0" :max="meta.frames - 1" step="1" v-model.number="frameIndex" />
          <span class="val">{{ frameIndex + 1 }}/{{ meta.frames }}</span>
        </label>
      </section>

      <section class="group export">
        <div class="group-title"><span>导出</span></div>
        <button class="export-btn" :disabled="!isRealFile || exporting" @click="exportJpegOpen = true">
          {{ exporting ? "导出中…" : "导出JPEG" }}
        </button>
        <button class="export-btn" :disabled="!isRealFile || exportingDicom" @click="exportDicomOpen = true">
          {{ exportingDicom ? "导出中…" : "导出DICOM" }}
        </button>
        <span v-if="!isRealFile" class="hint-sm">示例数据不可导出</span>
        <span
          v-if="exportMsg"
          class="export-msg"
          :class="{ ok: exportMsg.startsWith('已导出') }"
          >{{ exportMsg }}</span
        >
        <span
          v-if="exportDicomMsg"
          class="export-msg"
          :class="{ ok: exportDicomMsg.startsWith('已导出') }"
          >{{ exportDicomMsg }}</span
        >
      </section>
    </aside>

    <!-- 导出 JPEG 对话框 -->
    <div v-if="exportJpegOpen" class="modal-mask" @click.self="exportJpegOpen = false">
      <div class="modal export-modal">
        <div class="modal-title">导出 JPEG</div>

        <div class="modal-row">
          <span class="modal-label">范围</span>
          <label class="radio"><input type="radio" value="current" v-model="exportScope" /> 当前帧</label>
          <label class="radio"><input type="radio" value="all" v-model="exportScope" /> 整个序列（多帧/多切片）</label>
        </div>

        <div class="modal-row">
          <span class="modal-label">标签叠加</span>
          <div class="tag-corners">
            <div class="tag-corner" v-for="g in tagGroups" :key="g.name">
              <div class="corner-title">{{ g.name }}</div>
              <label
                v-for="t in g.tags"
                :key="t.key"
                class="tag-chk"
              >
                <input type="checkbox" :value="t.key" v-model="selectedTags" />
                <span>{{ t.label }}</span>
              </label>
            </div>
          </div>
        </div>

        <div class="modal-row">
          <span class="modal-label">水印</span>
          <input type="text" v-model="watermarkText" placeholder="可留空（不添加水印）" />
        </div>

        <div class="modal-row">
          <span class="modal-label">质量</span>
          <input type="range" min="10" max="100" step="1" v-model.number="exportQuality" />
          <span class="val">{{ exportQuality }}</span>
        </div>

        <div class="modal-actions">
          <button @click="exportJpegOpen = false">取消</button>
          <button class="primary" :disabled="exporting" @click="onExportJpeg">
            {{ exporting ? "导出中…" : "导出" }}
          </button>
        </div>
      </div>
    </div>

    <!-- 导出 DICOM 对话框 -->
    <div v-if="exportDicomOpen" class="modal-mask" @click.self="exportDicomOpen = false">
      <div class="modal export-dicom-modal">
        <div class="modal-title">导出 DICOM</div>

        <div class="modal-row">
          <span class="modal-label">范围</span>
          <label class="radio"><input type="radio" value="current" v-model="exportDicomScope" /> 当前帧</label>
          <label class="radio"><input type="radio" value="all" v-model="exportDicomScope" /> 整个序列（多帧/多切片）</label>
        </div>

        <div class="modal-row">
          <span class="modal-label">压缩方式</span>
          <select v-model="exportTs" class="ts-select">
            <option v-for="t in tsOptions" :key="t.value" :value="t.value">{{ t.label }}</option>
          </select>
        </div>
        <div class="modal-row" v-if="tsNeedsDegree">
          <span class="modal-label">有损程度</span>
          <input type="range" min="1" max="100" step="1" v-model.number="exportDicomQuality" />
          <span class="val">{{ exportDicomQuality }}</span>
          <span class="hint-sm" v-if="exportTs === 'htj2k_lossy'">（映射到 DWT 分解层数）</span>
          <span class="hint-sm" v-else-if="exportTs === 'jpegls_loss'">（JPEG-LS 近无损误差带 NEAR，最大重建误差 ±N）</span>
        </div>

        <div class="modal-row anon-row">
          <span class="modal-label">脱敏</span>
          <div class="anon-grid">
            <div class="anon-item" v-for="g in anonGroups" :key="g.id">
              <span class="anon-name">{{ g.label }}</span>
              <select v-model="anonMethodMap[g.id]" class="anon-select">
                <option v-for="m in anonMethodOptions(g.id)" :key="m.value" :value="m.value">{{ m.label }}</option>
              </select>
            </div>
          </div>
        </div>
        <div class="modal-row" v-if="anonNeedPassword">
          <span class="modal-label">加密密码</span>
          <input type="password" v-model="anonPassword" placeholder="留空则默认 unixel（不入库）" class="anon-pwd" />
        </div>

        <div class="modal-row" v-if="showSeriesOutput">
          <span class="modal-label">输出形式</span>
          <label class="radio"><input type="radio" :value="false" v-model="exportMultifile" /> 单个文件（多帧）</label>
          <label class="radio"><input type="radio" :value="true" v-model="exportMultifile" /> 多个文件（单帧）</label>
        </div>

        <div class="modal-actions">
          <button @click="exportDicomOpen = false">取消</button>
          <button class="primary" :disabled="exportingDicom" @click="onExportDicom">
            {{ exportingDicom ? "导出中…" : "导出" }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.viewer {
  display: flex;
  flex-direction: row;
  height: 100%;
}
.stage {
  flex: 1;
  min-width: 0;
  position: relative;
  background: #000;
}
.frame-scroll {
  width: 14px;
  flex: 0 0 14px;
  align-self: stretch;
  position: relative;
  background: var(--panel);
  border-left: 1px solid var(--border);
  cursor: pointer;
}
.frame-thumb {
  position: absolute;
  left: 2px;
  right: 2px;
  top: 0;
  border-radius: 6px;
  background: var(--fg-dim);
  transition: background 0.12s;
}
.frame-scroll:hover .frame-thumb {
  background: var(--accent);
}
.frame-thumb:active {
  background: var(--accent);
}
.canvas {
  width: 100%;
  height: 100%;
  display: block;
  cursor: grab;
}
.canvas:active {
  cursor: grabbing;
}

/* 右侧工具栏 */
.side {
  width: 248px;
  flex: 0 0 248px;
  border-left: 1px solid var(--border);
  background: var(--panel);
  padding: 12px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.side-section {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.group {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.group-title {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--fg-dim);
  border-bottom: 1px solid var(--border);
  padding-bottom: 4px;
}
.group-actions {
  display: flex;
  gap: 6px;
}
.mini {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 2px 8px;
  cursor: pointer;
  font-size: 11px;
}
.mini:hover {
  color: var(--fg);
  border-color: var(--fg-dim);
}
.side label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--fg-dim);
}
.side .val {
  color: var(--fg);
  min-width: 48px;
  text-align: right;
}
.side input[type="range"] {
  flex: 1;
  min-width: 0;
}
/* 缩放比例指示已移至画布右下角 */
.zoom-badge {
  position: absolute;
  right: 10px;
  bottom: 10px;
  background: rgba(0, 0, 0, 0.55);
  color: #fff;
  padding: 2px 8px;
  border-radius: 4px;
  font-size: 12px;
  pointer-events: none;
  user-select: none;
}
.export-label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--fg-dim);
}
.export-label select {
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 3px 6px;
}
.export-btn {
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: 4px;
  padding: 7px 12px;
  cursor: pointer;
  font-size: 12px;
}
.export-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.export-msg {
  font-size: 12px;
  color: #e5484d;
  word-break: break-all;
}
.export-msg.ok {
  color: #2e9e5b;
}
.hint-sm {
  font-size: 11px;
  color: var(--fg-dim);
}

/* 导出 JPEG 对话框 */
.modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}
.modal {
  background: var(--bg-1, #14161a);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 18px 20px;
  width: 420px;
  max-width: 92vw;
  box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
}
.export-modal .modal-title {
  font-size: 15px;
  font-weight: 600;
  margin-bottom: 14px;
}
.modal-row {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  margin-bottom: 14px;
}
.modal-label {
  width: 64px;
  flex: none;
  font-size: 13px;
  color: var(--fg-dim);
  padding-top: 3px;
}
.modal-row .radio {
  font-size: 13px;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  margin-right: 14px;
  cursor: pointer;
}
.tag-corners {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px 18px;
  flex: 1;
}
.tag-corner {
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 10px;
  min-width: 0;
}
.corner-title {
  font-size: 12px;
  font-weight: 600;
  color: var(--accent, #4da3ff);
  margin-bottom: 6px;
}
.tag-chk {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  cursor: pointer;
  margin-bottom: 4px;
}
.modal-row input[type="text"] {
  flex: 1;
  background: var(--bg-2, #1c1f26);
  border: 1px solid var(--border);
  border-radius: 4px;
  color: var(--fg);
  padding: 6px 8px;
  font-size: 13px;
}
.modal-row input[type="range"] {
  flex: 1;
}
.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  margin-top: 4px;
}
.modal-actions button {
  background: transparent;
  border: 1px solid var(--border);
  color: var(--fg);
  border-radius: 4px;
  padding: 7px 16px;
  cursor: pointer;
  font-size: 13px;
}
.modal-actions .primary {
  background: var(--accent);
  color: #fff;
  border-color: var(--accent);
}

/* 导出 DICOM 对话框 */
.export-dicom-modal {
  width: 520px;
}
.ts-select,
.anon-select,
.anon-pwd {
  background: var(--bg-2, #1c1f26);
  border: 1px solid var(--border);
  border-radius: 4px;
  color: var(--fg);
  padding: 5px 8px;
  font-size: 13px;
}
.ts-select {
  flex: 1;
  max-width: 320px;
}
.anon-row {
  align-items: flex-start;
}
.anon-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px 18px;
  flex: 1;
}
.anon-item {
  display: flex;
  align-items: center;
  gap: 8px;
}
.anon-name {
  width: 56px;
  flex: none;
  font-size: 13px;
  color: var(--fg-dim);
}
.anon-select {
  flex: 1;
  min-width: 0;
}
.anon-pwd {
  flex: 1;
}
</style>
