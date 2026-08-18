#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dicom_pixeldata::PixelDecoder;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DicomMeta {
    path: String,
    filename: String,
    width: u32,
    height: u32,
    frames: u32,
    bits_stored: u16,
    pixel_representation: u16, // 0=unsigned, 1=signed
    slope: f64,
    intercept: f64,
    window_center: f64,
    window_width: f64,
    photometric: String,
    hu_min: f32,
    hu_max: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DicomImage {
    meta: DicomMeta,
    // 连续 f32 (LE) 像素字节，顺序 [frame][row][col]，长度 = width*height*frames
    #[serde(with = "serde_bytes")]
    pixel_bytes: Vec<u8>,
}

// NIfTI 体数据（MPR 基础）：返回 3D 体（f32 LE，顺序 [x][y][z]）与维度
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NiftiMeta {
    path: String,
    filename: String,
    dims: [u32; 3], // [nx, ny, nz]
    hu_min: f32,
    hu_max: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NiftiVolume {
    meta: NiftiMeta,
    #[serde(with = "serde_bytes")]
    voxel_bytes: Vec<u8>, // f32 LE，顺序 [x][y][z]，长度 = nx*ny*nz
}

// 批量导出结果
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchResult {
    ok: usize,
    failed: Vec<BatchFail>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchFail {
    path: String,
    error: String,
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! 脚手架就绪", name)
}

/// 将 DICOM 标签里的窗位/窗宽从「原始像素值尺度」换算到 HU 尺度。
///
/// `pixel_bytes` 在 decode 时已应用 Modality LUT（RescaleSlope/Intercept）并转成 HU，
/// 因此返回给前端做窗映射的窗值也必须是 HU 尺度。但不少数据集（尤其 CBCT / 牙科）
/// 把 WindowCenter/Width 写在「原始像素值尺度」上，若直接套到 HU 数据，会把整图压黑。
///
/// 仅当存在 Modality LUT（斜率≠1 或截距≠0）时换算：
///   HU窗位 = WC × slope + intercept，  HU窗宽 = WW × slope（线性变换，窗宽随斜率缩放）。
pub(crate) fn window_to_hu(wc: f64, ww: f64, slope: f64, intercept: f64) -> (f64, f64) {
    if slope != 1.0 || intercept != 0.0 {
        (wc * slope + intercept, ww * slope)
    } else {
        (wc, ww)
    }
}

// 核心解码逻辑（与 Tauri 解耦，便于单元测试）
pub(crate) fn decode_dicom_file(path: &str) -> Result<DicomImage, String> {
    let obj = dicom_object::open_file(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let pd = obj
        .decode_pixel_data()
        .map_err(|e| format!("解码像素数据失败（传输语法可能未支持）: {}", e))?;

    let width = pd.columns();
    let height = pd.rows();
    let frames = pd.number_of_frames();
    let samples = pd.samples_per_pixel();
    let bits_stored = pd.bits_stored();

    if samples != 1 {
        return Err(format!(
            "暂仅支持单通道（灰度）影像，当前每像素样本数 = {}",
            samples
        ));
    }

    // HU（已应用 Modality LUT / Rescale）。返回的是强度值（CT 即 HU），前端实时做窗宽窗位。
    let hu: Vec<f32> = pd
        .to_vec::<f32>()
        .map_err(|e| format!("像素值转换失败: {}", e))?;

    let filename = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.dcm")
        .to_string();

    let pixel_representation: u16 = match pd.pixel_representation() {
        dicom_pixeldata::PixelRepresentation::Signed => 1,
        dicom_pixeldata::PixelRepresentation::Unsigned => 0,
    };

    let attr_f64 = |name: &str, default: f64| -> f64 {
        obj.element_by_name(name)
            .ok()
            .and_then(|e| e.to_str().ok())
            .and_then(|s| {
                s.split('\\')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse::<f64>()
                    .ok()
            })
            .unwrap_or(default)
    };

    let slope = attr_f64("RescaleSlope", 1.0);
    let intercept = attr_f64("RescaleIntercept", 0.0);
    let window_center = attr_f64("WindowCenter", 40.0);
    let window_width = attr_f64("WindowWidth", 400.0);
    // 窗值换算到 HU 尺度，与 pixel_bytes（已是 HU）保持一致，避免整图压黑。
    let (window_center, window_width) = window_to_hu(window_center, window_width, slope, intercept);

    let photometric = obj
        .element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("MONOCHROME2"))
        .to_string();

    let mut hu_min = f32::INFINITY;
    let mut hu_max = f32::NEG_INFINITY;
    for &v in &hu {
        if v < hu_min {
            hu_min = v;
        }
        if v > hu_max {
            hu_max = v;
        }
    }
    if !hu_min.is_finite() || !hu_max.is_finite() {
        hu_min = -1024.0;
        hu_max = 3071.0;
    }

    let mut pixel_bytes = Vec::with_capacity(hu.len() * 4);
    for v in &hu {
        pixel_bytes.extend_from_slice(&v.to_le_bytes());
    }

    Ok(DicomImage {
        meta: DicomMeta {
            path: path.to_string(),
            filename,
            width,
            height,
            frames,
            bits_stored,
            pixel_representation,
            slope,
            intercept,
            window_center,
            window_width,
            photometric,
            hu_min,
            hu_max,
        },
        pixel_bytes,
    })
}

// B1：常规图像导入（PNG/JPG/TIFF）→ 单帧灰度 DicomImage 结构
pub(crate) fn decode_regular_image(path: &str) -> Result<DicomImage, String> {
    let img = image::open(path).map_err(|e| format!("打开图像失败: {}", e))?;
    let gray = img.to_luma8();
    let (width, height) = (gray.width(), gray.height());
    let px = gray.into_raw(); // Vec<u8> 长度 = width*height
    let hu: Vec<f32> = px.into_iter().map(|v| v as f32).collect();

    let filename = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.img")
        .to_string();

    let mut pixel_bytes = Vec::with_capacity(hu.len() * 4);
    for v in &hu {
        pixel_bytes.extend_from_slice(&v.to_le_bytes());
    }

    Ok(DicomImage {
        meta: DicomMeta {
            path: path.to_string(),
            filename,
            width,
            height,
            frames: 1,
            bits_stored: 8,
            pixel_representation: 0,
            slope: 1.0,
            intercept: 0.0,
            window_center: 128.0,
            window_width: 255.0,
            photometric: "MONOCHROME2".to_string(),
            hu_min: 0.0,
            hu_max: 255.0,
        },
        pixel_bytes,
    })
}

// B2：NIfTI 体数据导入（.nii / .nii.gz）
pub(crate) fn decode_nifti(path: &str) -> Result<NiftiVolume, String> {
    use nifti::{IntoNdArray, NiftiObject, ReaderOptions};

    let obj = ReaderOptions::new()
        .read_file(path)
        .map_err(|e| format!("读取 NIfTI 失败: {}", e))?;
    let volume = obj
        .into_volume()
        .into_ndarray::<f32>()
        .map_err(|e| format!("转换为 ndarray 失败: {}", e))?;

    if volume.ndim() != 3 {
        return Err(format!("仅支持 3D 体数据，当前维度 {}", volume.ndim()));
    }
    let dims = volume.dim();
    let nx = dims[0] as u32;
    let ny = dims[1] as u32;
    let nz = dims[2] as u32;

    // 显式按逻辑索引 [x][y][z] 取出体素，得到确定的 [x][y][z]（z 最内）布局，
    // 避免依赖 into_raw_vec 的内存排布假设（NIfTI 文件实际为 x 最内）。
    let n = (nx as usize) * (ny as usize) * (nz as usize);
    let mut raw = Vec::with_capacity(n);
    for x in 0..nx as usize {
        for y in 0..ny as usize {
            for z in 0..nz as usize {
                raw.push(volume[[x, y, z]]);
            }
        }
    }

    let mut hu_min = f32::INFINITY;
    let mut hu_max = f32::NEG_INFINITY;
    for &v in &raw {
        if v < hu_min {
            hu_min = v;
        }
        if v > hu_max {
            hu_max = v;
        }
    }
    if !hu_min.is_finite() || !hu_max.is_finite() {
        hu_min = 0.0;
        hu_max = 1.0;
    }

    let filename = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.nii")
        .to_string();

    let mut voxel_bytes = Vec::with_capacity(raw.len() * 4);
    for v in &raw {
        voxel_bytes.extend_from_slice(&v.to_le_bytes());
    }

    Ok(NiftiVolume {
        meta: NiftiMeta {
            path: path.to_string(),
            filename,
            dims: [nx, ny, nz],
            hu_min,
            hu_max,
        },
        voxel_bytes,
    })
}

// C1：HTJ2K 解码（纯 Rust openjph-core）。返回 (width, height, 灰度 u8)
pub(crate) fn htj2k_decode(codestream: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    use openjph_core::codestream::Codestream;
    use openjph_core::file::MemInfile;

    let mut infile = MemInfile::new(codestream);
    let mut cs = Codestream::new();
    cs.read_headers(&mut infile)
        .map_err(|e| format!("HTJ2K 读头失败: {}", e))?;
    let siz = cs.access_siz();
    let width = siz.get_width(0);
    let height = siz.get_height(0);
    cs.create(&mut infile)
        .map_err(|e| format!("HTJ2K 创建失败: {}", e))?;

    let mut gray = Vec::with_capacity((width * height) as usize);
    for _ in 0..height {
        let line = cs
            .pull(0)
            .ok_or_else(|| "HTJ2K 解码行失败（codestream 不完整）".to_string())?;
        for v in line {
            gray.push(v as u8);
        }
    }
    Ok((width, height, gray))
}

// C1：HTJ2K 编码。gray: 8bit 灰度；lossless=true 用可逆 5/3 小波（TS 201）
pub(crate) fn htj2k_encode(
    gray: &[u8],
    width: u32,
    height: u32,
    lossless: bool,
) -> Result<Vec<u8>, String> {
    use openjph_core::codestream::Codestream;
    use openjph_core::file::MemOutfile;
    use openjph_core::types::{Point, Size};

    let mut cs = Codestream::new();
    cs.access_siz_mut().set_image_extent(Point::new(width, height));
    cs.access_siz_mut().set_num_components(1);
    cs.access_siz_mut().set_comp_info(0, Point::new(1, 1), 8, false);
    cs.access_siz_mut().set_tile_size(Size::new(width, height));
    {
        let cod = cs.access_cod_mut();
        cod.set_num_decomposition(5);
        cod.set_reversible(lossless);
        cod.set_color_transform(false);
    }
    cs.set_planar(0);

    let mut outfile = MemOutfile::new();
    cs.write_headers(&mut outfile, &[])
        .map_err(|e| format!("HTJ2K 写头失败: {}", e))?;
    for y in 0..height as usize {
        let start = y * width as usize;
        let line: Vec<i32> = gray[start..start + width as usize]
            .iter()
            .map(|&v| v as i32)
            .collect();
        cs.exchange(&line, 0)
            .map_err(|e| format!("HTJ2K 编码失败: {}", e))?;
    }
    cs.flush(&mut outfile)
        .map_err(|e| format!("HTJ2K flush 失败: {}", e))?;
    Ok(outfile.get_data().to_vec())
}

// C1：HTJ2K 文件导入 → 单帧灰度 DicomImage
pub(crate) fn decode_htj2k(bytes: &[u8]) -> Result<DicomImage, String> {
    let (width, height, gray) = htj2k_decode(bytes)?;
    let hu: Vec<f32> = gray.into_iter().map(|v| v as f32).collect();
    let mut pixel_bytes = Vec::with_capacity(hu.len() * 4);
    for v in &hu {
        pixel_bytes.extend_from_slice(&v.to_le_bytes());
    }
    Ok(DicomImage {
        meta: DicomMeta {
            path: String::new(),
            filename: "htj2k".to_string(),
            width,
            height,
            frames: 1,
            bits_stored: 8,
            pixel_representation: 0,
            slope: 1.0,
            intercept: 0.0,
            window_center: 128.0,
            window_width: 255.0,
            photometric: "MONOCHROME2".to_string(),
            hu_min: 0.0,
            hu_max: 255.0,
        },
        pixel_bytes,
    })
}

// 与前端 windowing.ts 保持一致的窗宽窗位映射（HU -> 8bit 灰度 RGBA）。
fn apply_window_rust(hu: &[f32], wc: f64, ww: f64, invert: bool, out: &mut [u8]) {
    let low = wc - ww / 2.0;
    let scale = if ww > 0.0 { 255.0 / ww } else { 0.0 };
    for i in 0..hu.len() {
        let v = (hu[i] as f64 - low) * scale;
        let v8: u8 = if v < 0.0 {
            0
        } else if v > 255.0 {
            255
        } else {
            v.round() as u8
        };
        let v8 = if invert { 255u8.wrapping_sub(v8) } else { v8 };
        let o = i * 4;
        out[o] = v8;
        out[o + 1] = v8;
        out[o + 2] = v8;
        out[o + 3] = 255;
    }
}

// 将 RGBA 缓冲编码并写入文件。PNG 无损；JPEG 转 RGB + 质量；HTJ2K 取灰度无损编码。
fn encode_and_save(
    width: u32,
    height: u32,
    rgba: &[u8],
    format: &str,
    quality: u8,
    out: &Path,
) -> Result<(), String> {
    match format.to_lowercase().as_str() {
        "png" => {
            let img = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(
                width,
                height,
                rgba.to_vec(),
            )
            .ok_or("PNG 图像缓冲构建失败（尺寸与数据长度不匹配）")?;
            img.save(out).map_err(|e| format!("保存 PNG 失败: {}", e))?;
        }
        "jpeg" | "jpg" => {
            let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
            for p in rgba.chunks_exact(4) {
                rgb.push(p[0]);
                rgb.push(p[1]);
                rgb.push(p[2]);
            }
            let file =
                std::fs::File::create(out).map_err(|e| format!("创建文件失败: {}", e))?;
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
                file,
                quality.clamp(1, 100),
            );
            enc.encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("编码 JPEG 失败: {}", e))?;
        }
        other => return Err(format!("不支持的导出格式: {}", other)),
    }
    Ok(())
}

// 统一导出核心：给定单帧 HU 像素 + 窗设置，编码为 PNG/JPEG/HTJ2K 写盘。
pub(crate) fn export_frame_from_pixels(
    hu: &[f32],
    width: u32,
    height: u32,
    photometric: &str,
    wc: f64,
    ww: f64,
    format: &str,
    quality: u8,
    output: &str,
) -> Result<String, String> {
    let invert = photometric == "MONOCHROME1";
    let per = (width as usize) * (height as usize);
    let mut rgba = vec![0u8; per * 4];
    apply_window_rust(hu, wc, ww, invert, &mut rgba);

    match format.to_lowercase().as_str() {
        "htj2k" | "j2c" | "jph" => {
            let gray: Vec<u8> = rgba.iter().step_by(4).copied().collect();
            let bytes = htj2k_encode(&gray, width, height, true)?; // 默认无损
            std::fs::write(output, &bytes).map_err(|e| format!("写入 HTJ2K 失败: {}", e))?;
        }
        _ => encode_and_save(width, height, &rgba, format, quality, Path::new(output))?,
    }
    Ok(output.to_string())
}

// 批量导出：把任意受支持文件解码为单帧后，按各自默认窗设置导出。
pub(crate) fn decode_any_to_frame(path: &str) -> Result<DicomImage, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".dcm") || lower.ends_with(".dicom") {
        decode_dicom_file(path)
    } else if lower.ends_with(".nii") || lower.ends_with(".nii.gz") {
        let vol = decode_nifti(path)?;
        let [nx, ny, nz] = vol.meta.dims;
        let z = nz / 2; // 取中间轴向切片
        let vox: Vec<f32> = vol
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut frame = Vec::with_capacity((nx * ny) as usize);
        for y in 0..ny as usize {
            for x in 0..nx as usize {
                frame.push(vox[((x * ny as usize) + y) * nz as usize + z as usize]);
            }
        }
        let mut pixel_bytes = Vec::with_capacity(frame.len() * 4);
        for v in &frame {
            pixel_bytes.extend_from_slice(&v.to_le_bytes());
        }
        Ok(DicomImage {
            meta: DicomMeta {
                path: path.to_string(),
                filename: vol.meta.filename,
                width: nx,
                height: ny,
                frames: 1,
                bits_stored: 16,
                pixel_representation: if vol.meta.hu_min < 0.0 { 1 } else { 0 },
                slope: 1.0,
                intercept: 0.0,
                window_center: ((vol.meta.hu_min + vol.meta.hu_max) / 2.0) as f64,
                window_width: (vol.meta.hu_max - vol.meta.hu_min) as f64,
                photometric: "MONOCHROME2".to_string(),
                hu_min: vol.meta.hu_min,
                hu_max: vol.meta.hu_max,
            },
            pixel_bytes,
        })
    } else if lower.ends_with(".j2c") || lower.ends_with(".jph") {
        let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
        decode_htj2k(&bytes)
    } else {
        decode_regular_image(path)
    }
}

fn try_export_one(
    path: &str,
    format: &str,
    quality: u8,
    out_dir: &str,
) -> Result<(), String> {
    let img = decode_any_to_frame(path)?;
    let n = (img.meta.width * img.meta.height) as usize;
    let hu: Vec<f32> = img
        .pixel_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let wc = img.meta.window_center;
    let ww = img.meta.window_width;
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = match format.to_lowercase().as_str() {
        "png" => "png",
        "jpeg" | "jpg" => "jpg",
        "htj2k" | "j2c" | "jph" => "jph",
        _ => "png",
    };
    let out = Path::new(out_dir).join(format!(
        "{}_{}_wc{}_ww{}.{}",
        stem,
        format,
        wc.round(),
        ww.round(),
        ext
    ));
    export_frame_from_pixels(&hu[0..n], img.meta.width, img.meta.height, &img.meta.photometric, wc, ww, format, quality, &out.to_string_lossy())
        .map(|_| ())
}

// ---------- Tauri commands ----------

#[tauri::command]
fn load_dicom(path: String) -> Result<DicomImage, String> {
    decode_dicom_file(&path)
}

#[tauri::command]
fn load_image(path: String) -> Result<DicomImage, String> {
    decode_regular_image(&path)
}

#[tauri::command]
fn load_nifti(path: String) -> Result<NiftiVolume, String> {
    decode_nifti(&path)
}

#[tauri::command]
fn load_htj2k(path: String) -> Result<DicomImage, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("读取文件失败: {}", e))?;
    decode_htj2k(&bytes)
}

// 导出当前帧：前端传入该帧 HU 像素字节 + 尺寸/光度解释/窗设置
#[tauri::command]
fn export_frame(
    pixel_bytes: Vec<u8>,
    width: u32,
    height: u32,
    photometric: String,
    wc: f64,
    ww: f64,
    format: String,
    quality: u8,
    output_path: String,
) -> Result<String, String> {
    let hu: Vec<f32> = pixel_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    export_frame_from_pixels(
        &hu,
        width,
        height,
        &photometric,
        wc,
        ww,
        &format,
        quality,
        &output_path,
    )
}

#[tauri::command]
fn batch_export(
    paths: Vec<String>,
    format: String,
    quality: u8,
    out_dir: String,
) -> Result<BatchResult, String> {
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("创建输出目录失败: {}", e))?;
    let mut ok = 0usize;
    let mut failed = Vec::new();
    for p in &paths {
        match try_export_one(p, &format, quality, &out_dir) {
            Ok(()) => ok += 1,
            Err(e) => failed.push(BatchFail {
                path: p.clone(),
                error: e,
            }),
        }
    }
    Ok(BatchResult { ok, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_sample_ct() {
        let img = decode_dicom_file("tests/sample.dcm").expect("decode sample.dcm");
        assert_eq!(img.meta.width, 512);
        assert_eq!(img.meta.height, 512);
        assert_eq!(img.meta.frames, 1);
        assert_eq!(img.meta.bits_stored, 16);
        assert_eq!(img.meta.pixel_representation, 1);

        let n = img.meta.width as usize * img.meta.height as usize;
        assert_eq!(img.pixel_bytes.len(), n * 4);

        let hu: Vec<f32> = img
            .pixel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(hu.iter().any(|&v| (v + 1024.0).abs() < 1.0), "expected air -1024 HU");
        assert!(img.meta.hu_min <= img.meta.hu_max);

        let center = hu[n / 2 + 256];
        assert!(center > -1024.0);
    }

    #[test]
    fn export_sample_png_and_jpeg() {
        let img = decode_dicom_file("tests/sample.dcm").expect("decode");
        let n = img.meta.width as usize * img.meta.height as usize;
        let hu: Vec<f32> = img
            .pixel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let hu = &hu[0..n];

        let png = "tests/out_sample.png";
        export_frame_from_pixels(hu, img.meta.width, img.meta.height, &img.meta.photometric, 40.0, 400.0, "png", 90, png)
            .expect("export png");
        let pimg = image::open(png).expect("read exported png");
        assert_eq!(pimg.width(), 512);
        assert_eq!(pimg.height(), 512);
        let _ = std::fs::remove_file(png);

        let jpg = "tests/out_sample.jpg";
        export_frame_from_pixels(hu, img.meta.width, img.meta.height, &img.meta.photometric, 40.0, 400.0, "jpeg", 85, jpg)
            .expect("export jpeg");
        let jimg = image::open(jpg).expect("read exported jpeg");
        assert_eq!(jimg.width(), 512);
        assert_eq!(jimg.height(), 512);
        let _ = std::fs::remove_file(jpg);
    }

    #[test]
    fn htj2k_roundtrip_lossless() {
        let w = 48u32;
        let h = 32u32;
        let gray: Vec<u8> = (0..(w * h)).map(|i| (i % 256) as u8).collect();
        let bytes = htj2k_encode(&gray, w, h, true).expect("encode htj2k");
        assert!(bytes.len() > 20, "codestream too small");
        let (dw, dh, dgray) = htj2k_decode(&bytes).expect("decode htj2k");
        assert_eq!(dw, w);
        assert_eq!(dh, h);
        assert_eq!(dgray, gray, "无损 HTJ2K 像素应完全一致");
    }

    #[test]
    fn nifti_roundtrip() {
        use ndarray::Array3;
        use nifti::writer::WriterOptions;

        let path = "tests/out_sample.nii.gz";
        let (nx, ny, nz) = (32u32, 24u32, 16u32);
        {
            let mut data = Array3::<f32>::zeros((nx as usize, ny as usize, nz as usize));
            for x in 0..nx as usize {
                for y in 0..ny as usize {
                    for z in 0..nz as usize {
                        data[[x, y, z]] = ((x + y + z) % 256) as f32;
                    }
                }
            }
            WriterOptions::new(path)
                .write_nifti(&data)
                .expect("write nifti");
        }

        let vol = decode_nifti(path).expect("decode nifti");
        assert_eq!(vol.meta.dims, [nx, ny, nz]);
        // voxel (1,2,3) 在 [x][y][z] 顺序下索引 = ((1*ny)+2)*nz+3 = (1+2+3)%256 = 6
        let idx = ((1usize * ny as usize + 2) * nz as usize + 3) as usize;
        let vox: Vec<f32> = vol
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(
            (vox[idx] - 6.0).abs() < 1e-3,
            "voxel mismatch: {}",
            vox[idx]
        );
        let _ = std::fs::remove_file(path);
    }

    // 用真实 CBCT 数据（data/CBCT/*.dcm，390 连续切片）做全量解码验证：
    // 确认全部未压缩单通道灰阶 DICOM 都能被 dicom-rs 解码，并收集整体 HU 范围。
    #[test]
    fn decode_cbct_volume() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/CBCT");
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("无法读取 data/CBCT 目录")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map_or(false, |x| x.eq_ignore_ascii_case("dcm"))
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "data/CBCT 下未找到 .dcm 文件");

        let total = files.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut hu_global_min = f32::INFINITY;
        let mut hu_global_max = f32::NEG_INFINITY;
        let mut all_390x390 = true;
        let mut all_mono2 = true;

        for f in &files {
            match decode_dicom_file(f.to_str().unwrap()) {
                Ok(img) => {
                    ok += 1;
                    if img.meta.width != 390 || img.meta.height != 390 {
                        all_390x390 = false;
                    }
                    if img.meta.photometric != "MONOCHROME2" {
                        all_mono2 = false;
                    }
                    if img.meta.hu_min < hu_global_min {
                        hu_global_min = img.meta.hu_min;
                    }
                    if img.meta.hu_max > hu_global_max {
                        hu_global_max = img.meta.hu_max;
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!(
                        "CBCT FAIL {}: {}",
                        f.file_name().unwrap().to_string_lossy(),
                        e
                    );
                }
            }
        }

        println!(
            "CBCT decode: total={} ok={} failed={} all_390x390={} all_mono2={} hu=[{:.1},{:.1}]",
            total, ok, failed, all_390x390, all_mono2, hu_global_min, hu_global_max
        );
        assert_eq!(failed, 0, "存在解码失败的 CBCT 切片");
        assert!(all_390x390, "并非所有切片都是 390x390");
        assert!(all_mono2, "存在非 MONOCHROME2 切片");
        assert!(hu_global_min < hu_global_max, "HU 范围异常");
    }

    // 回归测试：窗位/窗宽必须换算到 HU 尺度（与 pixel_bytes 一致）。
    // 否则前端对 HU 数据套原始值尺度的窗，会把整图压黑（CBCT 实测：非黑像素仅 ~15%）。
    #[test]
    fn window_to_hu_scales_rescaled_ct() {
        // CBCT 实测：原始窗 1773/3547，slope=1 截距=-1000 -> HU 窗 773/3547
        assert_eq!(window_to_hu(1773.0, 3547.0, 1.0, -1000.0), (773.0, 3547.0));
        // 无 Modality LUT：单位已一致，原样返回
        assert_eq!(window_to_hu(40.0, 400.0, 1.0, 0.0), (40.0, 400.0));
        // 含斜率：窗宽随斜率缩放
        assert_eq!(
            window_to_hu(100.0, 200.0, 2.0, -1024.0),
            (-824.0, 400.0)
        );
    }

    // 回归测试：确认经 Tauri JSON IPC(serde_json) 序列化后，pixelBytes 是 number[]，
    // 长度 = width*height*frames*4（f32 LE 字节数）。前端 decodePixelBytes 必须能处理
    // 这种 array-of-numbers 形态（否则 new Float32Array(bytes.slice().buffer) 会得到空数组，
    // 导致图像区域全黑/不显示）。
    #[test]
    fn pixel_bytes_serializes_as_number_array() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/CBCT/0005.dcm");
        let img = decode_dicom_file(dir.to_str().unwrap()).expect("decode 0005.dcm");
        let v = serde_json::to_value(&img).expect("serialize");
        let pb = v.get("pixelBytes").expect("pixelBytes key");
        let arr = pb
            .as_array()
            .expect("pixelBytes 必须是 number[]（Tauri JSON 下 serde_bytes 退化）");
        let expected = (img.meta.width * img.meta.height * img.meta.frames) as usize * 4;
        assert_eq!(arr.len(), expected, "pixelBytes 字节数应为 width*height*frames*4");

        // 窗位/窗宽已换算到 HU 尺度（与 pixel_bytes 一致）
        let meta = v.get("meta").expect("meta");
        assert!(meta.get("windowCenter").is_some());
        assert!(meta.get("windowWidth").is_some());
    }

    // 回归测试：DicomImage / NiftiVolume 经 serde 序列化后，像素字段必须是
    // camelCase（pixelBytes / voxelBytes）。否则前端 toImageView 读 img.pixelBytes
    // 得到 undefined，触发 "Cannot read properties of undefined (reading 'slice')"。
    #[test]
    fn serialize_pixel_fields_camel_case() {
        let img = decode_dicom_file("tests/sample.dcm").expect("decode sample.dcm");
        let v = serde_json::to_value(&img).expect("serialize DicomImage");
        let obj = v.as_object().expect("DicomImage serializes to object");
        assert!(
            obj.contains_key("pixelBytes"),
            "缺少 camelCase 字段 pixelBytes（前端将读到 undefined 而崩溃）: keys={:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj.contains_key("pixel_bytes"),
            "仍存在 snake_case 字段 pixel_bytes，前端约定为 pixelBytes"
        );

        use ndarray::Array3;
        use nifti::writer::WriterOptions;
        let path = "tests/out_serialize_serde.nii.gz";
        let (nx, ny, nz) = (16u32, 12u32, 8u32);
        let mut data = Array3::<f32>::zeros((nx as usize, ny as usize, nz as usize));
        for x in 0..nx as usize {
            for y in 0..ny as usize {
                for z in 0..nz as usize {
                    data[[x, y, z]] = ((x + y + z) % 256) as f32;
                }
            }
        }
        WriterOptions::new(path)
            .write_nifti(&data)
            .expect("write nifti");
        let vol = decode_nifti(path).expect("decode nifti");
        let v2 = serde_json::to_value(&vol).expect("serialize NiftiVolume");
        let obj2 = v2.as_object().expect("NiftiVolume serializes to object");
        assert!(
            obj2.contains_key("voxelBytes"),
            "缺少 camelCase 字段 voxelBytes（前端将读到 undefined）: keys={:?}",
            obj2.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj2.contains_key("voxel_bytes"),
            "仍存在 snake_case 字段 voxel_bytes，前端约定为 voxelBytes"
        );
        let _ = std::fs::remove_file(path);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            load_dicom,
            load_image,
            load_nifti,
            load_htj2k,
            export_frame,
            batch_export
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
