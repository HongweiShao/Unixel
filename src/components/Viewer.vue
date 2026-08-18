<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from "vue";
import type { DicomMeta } from "../types";
import { applyWindow } from "../windowing";
import { save } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";

const props = defineProps<{
  meta: DicomMeta;
  frames: Float32Array[]; // 每帧 HU 数组，长度 = width*height
}>();
const emit = defineEmits<{ close: [] }>();

const canvasRef = ref<HTMLCanvasElement | null>(null);
const wc = ref(props.meta.windowCenter);
const ww = ref(props.meta.windowWidth);
const frameIndex = ref(0);
const zoom = ref(1);
const pan = ref({ x: 0, y: 0 });

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
  resizeObserver = new ResizeObserver(() => {
    resizeCanvas();
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
    <div class="toolbar">
      <span class="file" :title="meta.path">{{ meta.filename }}</span>
      <label>窗位
        <input type="range" :min="meta.huMin" :max="meta.huMax" step="1" v-model.number="wc" />
        <span class="val">{{ wc }}</span>
      </label>
      <label>窗宽
        <input type="range" :min="1" :max="Math.max(1, meta.huMax - meta.huMin)" step="1" v-model.number="ww" />
        <span class="val">{{ ww }}</span>
      </label>
      <label v-if="meta.frames > 1">帧
        <input type="range" min="0" :max="meta.frames - 1" step="1" v-model.number="frameIndex" />
        <span class="val">{{ frameIndex + 1 }}/{{ meta.frames }}</span>
      </label>
      <button class="reset" @click="() => { zoom = 1; pan = { x: 0, y: 0 }; wc = meta.windowCenter; ww = meta.windowWidth; frameIndex = 0; }">重置</button>
      <button class="close" @click="emit('close')">关闭</button>
      <span class="zoom">缩放 {{ zoom.toFixed(2) }}x</span>
    </div>
    <div class="export-bar">
      <label class="export-label">导出
        <select v-model="exportFormat">
          <option value="png">PNG</option>
          <option value="jpeg">JPEG</option>
          <option value="htj2k">HTJ2K</option>
        </select>
      </label>
      <label v-if="exportFormat === 'jpeg'" class="export-label">质量
        <input type="range" min="10" max="100" step="1" v-model.number="exportQuality" />
        <span class="val">{{ exportQuality }}</span>
      </label>
      <button class="export-btn" :disabled="!isRealFile || exporting" @click="onExport">
        {{ exporting ? "导出中…" : "导出当前帧" }}
      </button>
      <span v-if="!isRealFile" class="hint-sm">示例数据不可导出</span>
      <span v-if="exportMsg" class="export-msg" :class="{ ok: exportMsg.startsWith('已导出') }">{{ exportMsg }}</span>
    </div>
    <div class="meta-bar">
      {{ meta.width }}×{{ meta.height }} · {{ meta.frames }} 帧 · {{ meta.bitsStored }}bit ·
      {{ meta.photometric }} · slope {{ meta.slope }} intercept {{ meta.intercept }}
    </div>
    <canvas
      ref="canvasRef"
      class="canvas"
      @wheel.prevent="onWheel"
      @mousedown="onDown"
      @mousemove="onMove"
      @mouseup="onUp"
      @mouseleave="onUp"
    />
  </div>
</template>

<style scoped>
.viewer {
  display: flex;
  flex-direction: column;
  height: 100%;
}
.toolbar {
  display: flex;
  gap: 16px;
  align-items: center;
  padding: 8px 12px;
  background: var(--panel);
  border-bottom: 1px solid var(--border);
  flex-wrap: wrap;
}
.toolbar .file {
  font-size: 12px;
  font-weight: 600;
  color: var(--fg);
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.toolbar label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--fg-dim);
}
.toolbar .val {
  color: var(--fg);
  min-width: 44px;
  text-align: right;
}
.reset,
.close {
  background: transparent;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 10px;
  cursor: pointer;
  font-size: 12px;
}
.reset:hover,
.close:hover {
  color: var(--fg);
}
.zoom {
  font-size: 12px;
  color: var(--fg-dim);
  margin-left: auto;
}
.meta-bar {
  padding: 4px 12px;
  font-size: 11px;
  color: var(--fg-dim);
  background: var(--panel);
  border-bottom: 1px solid var(--border);
}
.export-bar {
  display: flex;
  gap: 14px;
  align-items: center;
  padding: 6px 12px;
  background: var(--panel);
  border-bottom: 1px solid var(--border);
  flex-wrap: wrap;
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
  padding: 2px 4px;
}
.export-label .val {
  color: var(--fg);
  min-width: 28px;
  text-align: right;
}
.export-btn {
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: 4px;
  padding: 4px 12px;
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
.canvas {
  flex: 1;
  width: 100%;
  background: #000;
  cursor: grab;
  display: block;
}
.canvas:active {
  cursor: grabbing;
}
</style>
