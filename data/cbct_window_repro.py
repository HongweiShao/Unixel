"""
复现并验证 CBCT 窗映射 BUG。
逻辑严格对齐前端 src/windowing.ts::applyWindow 与后端 decode_dicom_file：
- 像素用 HU（已应用 RescaleSlope/Intercept）
- 窗映射 low = wc - ww/2, scale = 255/ww, v 钳制 [0,255], MONOCHROME2 不反转
"""
import pydicom, numpy as np

PATH = r"E:/Codes/Unixel/data/CBCT/0005.dcm"
ds = pydicom.dcmread(PATH)

slope = float(getattr(ds, "RescaleSlope", 1) or 1)
intercept = float(getattr(ds, "RescaleIntercept", 0) or 0)
wc_tag = float(ds.WindowCenter) if "WindowCenter" in ds else 40.0
ww_tag = float(ds.WindowWidth) if "WindowWidth" in ds else 400.0
bits_stored = int(ds.BitsStored)

px = ds.pixel_array.astype(np.float64)
hu = px * slope + intercept          # 与后端 to_vec::<f32>() 一致的 HU
hu_min, hu_max = float(hu.min()), float(hu.max())

print(f"shape={px.shape} bitsStored={bits_stored}")
print(f"Rescale slope={slope} intercept={intercept}")
print(f"WindowCenter(tag)={wc_tag}  WindowWidth(tag)={ww_tag}")
print(f"HU range = [{hu_min:.1f}, {hu_max:.1f}]")

def apply_window(wc, ww):
    low = wc - ww / 2.0
    scale = 255.0 / ww if ww > 0 else 0.0
    v = (hu - low) * scale
    v = np.clip(v, 0, 255)
    return v

def report(name, wc, ww):
    v = apply_window(wc, ww)
    nonblack = int((v > 10).sum())
    nonwhite = int((v < 245).sum())
    visible = int(((v > 10) & (v < 245)).sum())
    tot = v.size
    print(f"[{name}] wc={wc:.0f} ww={ww:.0f} -> 非黑像素={nonblack/tot*100:5.1f}%  可见(10~245)={visible/tot*100:5.1f}%  mean={v.mean():.1f}")

print("\n--- 复现 BUG：后端直接返回的原始窗位（套到 HU 上）---")
report("原始窗位(后端现行为)", wc_tag, ww_tag)

print("\n--- 修正方案 A：窗位换算到 HU (wc*slope+intercept, ww*slope) ---")
report("窗位转HU", wc_tag * slope + intercept, ww_tag * slope)

print("\n--- 对照：标准软组织窗 / 骨窗 ---")
report("软组织窗 40/400", 40, 400)
report("骨窗 300/1500", 300, 1500)
