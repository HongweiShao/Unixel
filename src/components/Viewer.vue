<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from "vue";
import type { DicomMeta } from "../types";
import { applyWindow } from "../windowing";
import { save } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";

const props = defineProps<{
  meta: DicomMeta;
  frames: Float32Array[]; // 每帧 HU 数组，长度 = width*height
  // 当前选中文件所属系列的有序文件列表（按位置排序）；长度>1 时画布右侧竖条列出切片便于快速切换
  seriesFiles?: Array<{ id: number; info: { filename: string } }>;
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
// 滚动 / 拖拽即直接切换对应位置的图像（不渲染序号按钮）
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
  const top = (currentIndex.value / (n - 1)) * maxTop;
  return { height: `${thumbH}px`, transform: `translateY(${top}px)` };
});

function yToIndex(clientY: number) {
  const el = scrollRef.value;
  if (!el) return 0;
  const rect = el.getBoundingClientRect();
  const frac = Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
  const n = stripItems.value.length;
  return Math.min(n - 1, Math.round(frac * (n - 1)));
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
  const dir = e.deltaY > 0 ? 1 : -1;
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

// 导出当前帧（按当前窗设置）为 PNG/JPEG/HTJ2K
const exportFormat = ref<"png" | "jpeg" | "htj2k">("png");
const exportQuality = ref(90);
const exporting = ref(false);
const exportMsg = ref<string | null>(null);
const isRealFile = computed(() => !props.meta.path.startsWith("mock"));

async function onExport() {
  if (!isRealFile.value) return;
  exportMsg.value = null;
  exporting.value = true;
  try {
    const frame = props.frames[frameIndex.value];
    if (!frame) return;
    // 取当前帧 HU 像素字节（f32 LE）回传后端做窗映射与编码
    const pixelBytes = new Uint8Array(
      frame.buffer,
      frame.byteOffset,
      frame.byteLength
    );
    const fmt = exportFormat.value;
    const ext = fmt === "png" ? "png" : fmt === "jpeg" ? "jpg" : "jph";
    const base = props.meta.filename.replace(
      /\.(dcm|dicom|nii(\.gz)?|j2c|jph|png|jpe?g|tif?f)$/i,
      ""
    );
    const suggested = `${base}_wc${Math.round(wc.value)}_ww${Math.round(
      ww.value
    )}.${ext}`;
    const filters =
      fmt === "png"
        ? [{ name: "PNG", extensions: ["png"] }]
        : fmt === "jpeg"
          ? [{ name: "JPEG", extensions: ["jpg", "jpeg"] }]
          : [{ name: "HTJ2K", extensions: ["jph", "j2c"] }];
    const out = await save({ defaultPath: suggested, filters });
    if (!out) return; // 用户取消
    const saved = await invoke<string>("export_frame", {
      pixelBytes,
      width: props.meta.width,
      height: props.meta.height,
      photometric: props.meta.photometric,
      wc: wc.value,
      ww: ww.value,
      format: fmt,
      quality: exportQuality.value,
      outputPath: out,
    });
    exportMsg.value = `已导出：${saved}`;
  } catch (e) {
    exportMsg.value =
      "导出失败：" +
      (typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e));
  } finally {
    exporting.value = false;
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

    <!-- 多帧 / 同系列多切片 竖向滚动条：位置对应图像序号，滚动/拖拽即切换（无序号按钮） -->
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
        <label class="export-label">
          格式
          <select v-model="exportFormat">
            <option value="png">PNG</option>
            <option value="jpeg">JPEG</option>
            <option value="htj2k">HTJ2K</option>
          </select>
        </label>
        <label v-if="exportFormat === 'jpeg'" class="export-label">
          质量
          <input type="range" min="10" max="100" step="1" v-model.number="exportQuality" />
          <span class="val">{{ exportQuality }}</span>
        </label>
        <button class="export-btn" :disabled="!isRealFile || exporting" @click="onExport">
          {{ exporting ? "导出中…" : "导出当前帧" }}
        </button>
        <span v-if="!isRealFile" class="hint-sm">示例数据不可导出</span>
        <span
          v-if="exportMsg"
          class="export-msg"
          :class="{ ok: exportMsg.startsWith('已导出') }"
          >{{ exportMsg }}</span
        >
      </section>
    </aside>
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
</style>
