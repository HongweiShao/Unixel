// 窗宽窗位映射：HU 值 -> 8bit 灰度 RGBA
// hu: 长度 width*height 的 HU 数组
// wc: 窗位(window center), ww: 窗宽(window width)
// out: 长度 hu.length*4 的 RGBA 缓冲（调用方预分配复用，避免 GC 抖动）
export function applyWindow(
  hu: Float32Array,
  wc: number,
  ww: number,
  out: Uint8ClampedArray,
  photometric: string = "MONOCHROME2"
): void {
  const low = wc - ww / 2;
  const scale = ww > 0 ? 255 / ww : 0;
  const invert = photometric === "MONOCHROME1";
  for (let i = 0; i < hu.length; i++) {
    let v = (hu[i] - low) * scale;
    if (v < 0) v = 0;
    else if (v > 255) v = 255;
    if (invert) v = 255 - v;
    const o = i * 4;
    out[o] = v;
    out[o + 1] = v;
    out[o + 2] = v;
    out[o + 3] = 255;
  }
}
