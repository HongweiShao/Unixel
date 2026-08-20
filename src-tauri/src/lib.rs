#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dicom_pixeldata::PixelDecoder;
use serde::{Serialize, Deserialize};
use std::path::{Path, PathBuf};

use dicom_core::Tag;
use dicom_core::dictionary::DataDictionary;
use dicom_core::header::Header;
use image::GenericImageView;
// 文字叠加（四角 DICOM 标签 + 右下角水平水印）
use image::RgbaImage;
use imageproc::drawing::{draw_text_mut, text_size};
use ab_glyph::{FontRef, PxScale};
use dicom_object::{FileDicomObject, InMemDicomObject};

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

// 详情对话框：文件标签信息（DICOM 全量标签 / NIfTI 头 / 图像格式头）
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct TagRow {
    tag: String, // DICOM: "(gggg,eeee)"；其他: 字段名
    vr: String,  // DICOM: VR；其他: "-"
    keyword: String, // DICOM: 标准字典关键字；其他: 人类可读标签
    value: String,
    description: String, // DICOM: 标签中文释义（innolitics 风格：含义+常见值），缺省回退 VR 含义；其它类型留空（前端不显示问号）
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileTags {
    kind: String, // "dicom" | "nifti" | "image"
    filename: String,
    rows: Vec<TagRow>,
}

// 从文件夹导入：仅返回顶层影像文件的概要信息（不含像素），前端按需懒加载像素
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ImageInfo {
    path: String,
    filename: String,
    width: u32,
    height: u32,
    frames: u32,
    kind: String, // "dicom" | "image" | "htj2k"
    // 系列与位置信息（用于按系列分组、位置排序、系列内切换；非 DICOM 均为 None）
    series_uid: Option<String>,
    series_number: Option<u32>,
    modality: Option<String>,
    instance_number: Option<u32>,
    slice_location: Option<f64>,
    image_pos_patient: Option<Vec<f64>>, // 3 个分量
    image_orientation: Option<Vec<f64>>, // 6 个分量（行/列方向余弦）
    // 分组展示用（后端已算好）：同系列 series_group 相同；series_label 作为 optgroup 标题
    series_group: Option<u32>,
    series_label: Option<String>,
}

// 序列选择对话框：单个序列概要（来自 scan_folder_series）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesBrief {
    study_uid: Option<String>,
    series_uid: Option<String>,
    modality: Option<String>,
    series_number: Option<u32>,
    series_description: Option<String>,
    patient_name: Option<String>,
    patient_id: Option<String>,
    series_date: Option<String>,
    study_date: Option<String>,
    file_count: usize,
    paths: Vec<String>,
}

// 序列选择对话框：单个检查（Study）及其下序列
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StudyBrief {
    study_uid: Option<String>,
    patient_name: Option<String>,
    patient_id: Option<String>,
    study_date: Option<String>,
    series: Vec<SeriesBrief>,
}

// 序列选择对话框：整体树（DICOM 按 Study→Series 两级；非 DICOM 归入 others）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesTree {
    studies: Vec<StudyBrief>,
    others: Option<SeriesBrief>,
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
    let mut hu: Vec<f32> = pd
        .to_vec::<f32>()
        .map_err(|e| format!("像素值转换失败: {}", e))?;

    // 多帧 DICOM：按逐帧解剖位置重排帧序，使帧序符合物理层叠
    //（数组首帧=inferior，末帧=superior，与多文件系列/滚动条顶部=superior 一致）。
    // 仅 Enhanced 多帧具备逐帧 PlanePositionSequence；传统多帧（文件内已顺序）保持原序。
    if frames > 1 {
        if let (Some(oop), Some(per_pos)) = (
            obj.element_by_name("ImageOrientationPatient")
                .ok()
                .and_then(|e| e.to_str().ok())
                .and_then(|s| parse_ds_vec(&s)),
            per_frame_positions(&obj),
        ) {
            if oop.len() == 6 && per_pos.len() == frames as usize {
                let normal = normalize3(&cross3(
                    &[oop[0], oop[1], oop[2]],
                    &[oop[3], oop[4], oop[5]],
                ));
                // 方向校正：DICOM 患者坐标系 +Z = superior；normal[2]<0 表示投影升序对应
                // superior→inferior，取反使升序=inferior→superior。
                let flip = if normal[2] >= 0.0 { 1.0 } else { -1.0 };
                let npx = (width * height) as usize;
                let mut idxs: Vec<usize> = (0..frames as usize).collect();
                idxs.sort_by(|&a, &b| {
                    let ka = (per_pos[a][0] * normal[0]
                        + per_pos[a][1] * normal[1]
                        + per_pos[a][2] * normal[2])
                        * flip;
                    let kb = (per_pos[b][0] * normal[0]
                        + per_pos[b][1] * normal[1]
                        + per_pos[b][2] * normal[2])
                        * flip;
                    ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
                });
                let mut reordered = vec![0.0f32; hu.len()];
                for (new_i, &old_i) in idxs.iter().enumerate() {
                    reordered[new_i * npx..(new_i + 1) * npx]
                        .copy_from_slice(&hu[old_i * npx..(old_i + 1) * npx]);
                }
                hu = reordered;
            }
        }
    }

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
    // 取仿射（优先 sform，其次 qform）以判断第三轴(k)指向。NIfTI 解剖方法下 +Z = superior；
    // aff 第 3 列第 3 行（列主序存储的第 10 个元素）即 k 轴位移向量的 superior(+Z) 分量。
    // <0 表示 k 增大指向 inferior，需翻转 z 使 k 增大=inferior→superior（与多文件/多帧一致）。
    let hdr = obj.header();
    // 第三轴(k) 指向：NIfTI 解剖方法下 +Z = superior。
    // sform 优先（其仿射第3列第3行 = srow_z[2]）；否则用 qform 四元数推导；
    // 二者皆无（纯像素对齐）则不翻转。
    let flip_z = if hdr.sform_code > 0 {
        hdr.srow_z[2] < 0.0
    } else if hdr.qform_code > 0 {
        let b = hdr.quatern_b as f64;
        let c = hdr.quatern_c as f64;
        let r22 = 1.0 - 2.0 * (b * b + c * c);
        let qfac = if hdr.pixdim[0] < 0.0 { -1.0 } else { 1.0 };
        let kz = r22 * (hdr.pixdim[3] as f64).abs() * qfac;
        kz < 0.0
    } else {
        false
    };
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
    // 按 flip_z 决定是否沿 z 翻转，使 k 增大=inferior→superior。
    let n = (nx as usize) * (ny as usize) * (nz as usize);
    let mut raw = Vec::with_capacity(n);
    for x in 0..nx as usize {
        for y in 0..ny as usize {
            for z in 0..nz as usize {
                let zz = if flip_z { nz as usize - 1 - z } else { z };
                raw.push(volume[[x, y, zz]]);
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
// 仅被 HTJ2K 往返测试使用；非测试构建下标记为允许死代码。
#[cfg_attr(not(test), allow(dead_code))]
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

// ---------- 导出 JPEG（带四角 DICOM 标签叠加 + 居中斜向半透明水印） ----------

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OverlayTag {
    corner: u8,     // 0=左上 1=右上 2=左下 3=右下
    keyword: String, // DICOM keyword；"__WINDOW__" 表示窗位/窗宽
    display: String, // 中文显示标签
}

// 按源文件解码出所有帧的 HU 像素（f32 LE，长度 = w*h），并返回可选 DICOM 对象（供读标签）
fn load_source_frames(
    path: &str,
) -> Result<(Vec<Vec<f32>>, Option<FileDicomObject<InMemDicomObject>>, u32, u32), String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".dcm") || lower.ends_with(".dicom") {
        let obj = dicom_object::open_file(path).ok();
        let img = decode_dicom_file(path)?;
        let w = img.meta.width;
        let h = img.meta.height;
        let hu_all: Vec<f32> = img
            .pixel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let per = (w * h) as usize;
        let frames: Vec<Vec<f32>> = (0..img.meta.frames)
            .map(|f| {
                let s = f as usize;
                hu_all[s * per..(s + 1) * per].to_vec()
            })
            .collect();
        Ok((frames, obj, w, h))
    } else if lower.ends_with(".nii") || lower.ends_with(".nii.gz") {
        let vol = decode_nifti(path)?;
        let [nx, ny, nz] = vol.meta.dims;
        let vox: Vec<f32> = vol
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut frames = Vec::with_capacity(nz as usize);
        for z in 0..nz as usize {
            let mut f = vec![0f32; (nx * ny) as usize];
            for x in 0..nx as usize {
                for y in 0..ny as usize {
                    f[y * nx as usize + x] = vox[((x * ny as usize) + y) * nz as usize + z];
                }
            }
            frames.push(f);
        }
        Ok((frames, None, nx, ny))
    } else {
        let img = decode_regular_image(path)?;
        let w = img.meta.width;
        let h = img.meta.height;
        let hu_all: Vec<f32> = img
            .pixel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let per = (w * h) as usize;
        Ok((vec![hu_all[0..per].to_vec()], None, w, h))
    }
}

// 读取单个叠加标签的显示值（DICOM 按 keyword 读取；__WINDOW__ 用传入窗设置）
fn overlay_value(
    obj: &Option<FileDicomObject<InMemDicomObject>>,
    tag: &OverlayTag,
    wc: f64,
    ww: f64,
) -> Option<String> {
    if tag.keyword == "__WINDOW__" {
        return Some(format!("窗位 {} / 窗宽 {}", wc.round(), ww.round()));
    }
    let obj = obj.as_ref()?;
    obj.element_by_name(&tag.keyword)
        .ok()
        .and_then(|e| e.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
}

// 按最大行宽（不超过图像中线）逐字符贪心折行，避免长标签越过中线或与对向角内容重叠
fn wrap_text_to_width(line: &str, max_w: i32, scale: PxScale, font: &FontRef) -> Vec<String> {
    let (full_w, _) = text_size(scale, font, line);
    if full_w as i32 <= max_w {
        return vec![line.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0i32;
    for ch in line.chars() {
        let (cw, _) = text_size(scale, font, &ch.to_string());
        let cw = cw as i32;
        if !cur.is_empty() && cur_w + cw > max_w {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// 在某角按行绘制文字（同角多行按固定列表顺序堆叠），白字 + 暗色阴影保证可读
// 字体大小自适应：以参考串「M测0」平均字符宽推算，使整图宽度约容纳 75 个字符；
// 单行超过中线（半宽）时自动折行，确保左上/右上角内容互不重叠
fn draw_corner_lines(img: &mut RgbaImage, corner: u8, lines: &[String], font: &FontRef) {
    if lines.is_empty() {
        return;
    }
    let margin = 16u32;
    // 字体尺寸：以平均字符宽估算，使整图宽约放下 75 个字符（参考串含中英数三类字符）
    let (ref_w, _) = text_size(PxScale { x: 1.0, y: 1.0 }, font, "M测0");
    let avg_char = (ref_w as f32 / 3.0).max(0.001);
    let font_size = ((img.width() as f32) / 75.0 / avg_char).clamp(8.0, 40.0);
    let scale = PxScale { x: font_size, y: font_size };
    let line_h = (font_size * 1.3) as i32;
    let max_w = (img.width() as i32) / 2 - margin as i32; // 不超过中线（半宽）
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|l| wrap_text_to_width(l.as_str(), max_w, scale, font))
        .collect();
    let total_h = line_h * wrapped.len() as i32;
    let start_y = if corner < 2 {
        margin as i32
    } else {
        img.height() as i32 - margin as i32 - total_h
    };
    let mut y = start_y;
    for line in &wrapped {
        let (tw, _) = text_size(scale, font, line);
        let x = match corner {
            0 | 2 => margin as i32,
            _ => (img.width() as i32 - tw as i32 - margin as i32).max(margin as i32),
        };
        draw_text_mut(img, image::Rgba([0, 0, 0, 170]), x + 1, y + 1, scale, font, line);
        draw_text_mut(img, image::Rgba([255, 255, 255, 235]), x, y, scale, font, line);
        y += line_h;
    }
}

// 右下角、水平单行半透明水印（非空时已在 export_jpeg 中判定后调用）
fn draw_watermark(img: &mut RgbaImage, text: &str, font: &FontRef) {
    let font_size = ((img.width().min(img.height()) as f32) * 0.06).max(22.0);
    let scale = PxScale { x: font_size, y: font_size };
    let (tw, th) = text_size(scale, font, text);
    let margin = 16u32;
    let x = (img.width() as i32 - tw as i32 - margin as i32).max(margin as i32);
    let y = (img.height() as i32 - th as i32 - margin as i32).max(margin as i32);
    // 暗色阴影 + 白字，保证不同背景下的可读性
    draw_text_mut(img, image::Rgba([0, 0, 0, 150]), x + 1, y + 1, scale, font, text);
    draw_text_mut(img, image::Rgba([255, 255, 255, 210]), x, y, scale, font, text);
}

// 导出 JPEG：支持「当前帧 / 所有（多帧全帧或同系列全切片）」，可叠加四角 DICOM 标签与水印
#[tauri::command]
fn export_jpeg(
    mode: String,        // "current" | "all"
    file_path: String,   // 当前源文件（current 取指定帧；all 多帧取全部帧）
    series_paths: Vec<String>, // all 多文件系列：有序切片路径；其余为空
    frame_index: u32,    // current 多帧：指定帧号
    wc: f64,
    ww: f64,
    photometric: String,
    overlays: Vec<OverlayTag>,
    watermark: String,
    quality: u8,
    output: String,      // current: 文件路径；all: 目录
) -> Result<String, String> {
    let font_data = include_bytes!("../resources/fonts/simhei.ttf");
    let font = FontRef::try_from_slice(font_data)
        .map_err(|e| format!("加载字体失败: {:?}", e))?;

    // 收集导出目标：(帧 HU 像素, 可选 DICOM 对象, 宽, 高)
    let mut targets: Vec<(Vec<f32>, Option<FileDicomObject<InMemDicomObject>>, u32, u32)> =
        Vec::new();
    if mode == "all" && !series_paths.is_empty() {
        for p in &series_paths {
            let (frames, obj, w, h) = load_source_frames(p)?;
            if let Some(f) = frames.into_iter().next() {
                targets.push((f, obj, w, h));
            }
        }
    } else {
        let (frames, obj, w, h) = load_source_frames(&file_path)?;
        if mode == "all" {
            for f in frames {
                targets.push((f, obj.clone(), w, h));
            }
        } else {
            let idx = (frame_index as usize).min(frames.len().saturating_sub(1));
            if let Some(f) = frames.into_iter().nth(idx) {
                targets.push((f, obj, w, h));
            }
        }
    }

    let n = targets.len();
    if n == 0 {
        return Err("没有可导出的帧".into());
    }

    let is_dir = mode == "all";
    let q = quality.clamp(10, 100);
    for (i, (hu, obj, w, h)) in targets.into_iter().enumerate() {
        let invert = photometric == "MONOCHROME1";
        let per = (w * h) as usize;
        let mut rgba = vec![0u8; per * 4];
        apply_window_rust(&hu, wc, ww, invert, &mut rgba);
        let mut img = RgbaImage::from_raw(w, h, rgba).ok_or("图像缓冲错误")?;

        // 四角标签分组（同角按列表顺序逐行）
        let mut by_corner: [Vec<String>; 4] =
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for t in &overlays {
            if let Some(v) = overlay_value(&obj, t, wc, ww) {
                by_corner[(t.corner as usize) % 4].push(format!("{}: {}", t.display, v));
            }
        }
        for c in 0..4 {
            draw_corner_lines(&mut img, c as u8, &by_corner[c], &font);
        }
        if !watermark.trim().is_empty() {
            draw_watermark(&mut img, watermark.trim(), &font);
        }

        let out_path = if is_dir {
            // 多帧 / 多切片 / NIfTI 体积均视为同一序列的切片，统一命名实现「多帧+多切片统一序列导出」
            Path::new(&output).join(format!("slice_{:03}.jpg", i + 1))
        } else {
            Path::new(&output).to_path_buf()
        };

        let mut buf = Vec::new();
        {
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q);
            enc.encode_image(&img)
                .map_err(|e| format!("JPEG 编码失败: {}", e))?;
        }
        std::fs::write(&out_path, &buf).map_err(|e| format!("写入失败: {}", e))?;
    }

    Ok(format!("已导出 {} 张 JPEG", n))
}

// ---------- 文件标签（详情对话框） ----------

fn fname(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

// VR 中文含义，用于标签说明（标准字典无标签名称字段时的降级说明）
fn vr_meaning(vr: &str) -> &'static str {
    match vr {
        "PN" => "人名",
        "LO" => "长字符串",
        "SH" => "短字符串",
        "CS" => "短字符串/代码",
        "DS" => "十进制字符串",
        "IS" => "整数字符串",
        "UI" => "唯一标识符(UID)",
        "DA" => "日期",
        "TM" => "时间",
        "DT" => "日期时间",
        "OB" | "OW" | "OV" => "其他字节/字",
        "US" => "无符号短整型",
        "SS" => "有符号短整型",
        "UL" => "无符号长整型",
        "SL" => "有符号长整型",
        "FL" => "浮点(单精度)",
        "FD" => "浮点(双精度)",
        "SQ" => "序列(嵌套数据集)",
        "UN" => "未知",
        "AT" => "属性标签",
        "LT" => "长文本",
        "ST" => "短文本",
        "UT" => "无限制文本",
        "UR" => "URI",
        _ => "值表示",
    }
}

// 常见 DICOM 标签的中文释义（参考 dicom.innolitics.com 的标签说明风格：定义 + 常见取值）。
// 标准字典不提供标签名称字段，这里以关键字 curated 一份高频标签释义表，供详情对话框「?」悬停说明。
fn tag_explanation(keyword: &str) -> Option<&'static str> {
    let s = match keyword {
        "PatientName" => "患者姓名。格式通常为「姓^名」（DICOM Person Name），如 “Zhang^San”。",
        "PatientID" => "患者唯一标识（医疗机构内部 ID）。常见值：数字或字母数字串。",
        "PatientBirthDate" => "患者出生日期。类型 DA，格式 YYYYMMDD。",
        "PatientSex" => "患者性别（枚举）。常见值：M（男）/ F（女）/ O（其他）。",
        "PatientAge" => "患者年龄。格式为「nnnY/W/D」，如 “045Y” 表示 45 岁。",
        "PatientWeight" => "患者体重（单位 kg）。",
        "PatientSize" => "患者身高或体长（单位 m）。",
        "ReferringPhysicianName" => "转诊医师姓名（Person Name 格式）。",
        "OperatorsName" => "操作技师姓名（多人用反斜杠分隔）。",
        "StudyDate" => "检查（Study）日期。类型 DA，格式 YYYYMMDD。",
        "StudyTime" => "检查时间。类型 TM，格式 HHMMSS.FFFFFF。",
        "SeriesDate" => "序列（Series）获取日期。类型 DA。",
        "SeriesTime" => "序列获取时间。类型 TM。",
        "AcquisitionDate" => "数据采集日期。类型 DA。",
        "AcquisitionTime" => "数据采集时间。类型 TM。",
        "ContentDate" => "内容创建日期。类型 DA。",
        "ContentTime" => "内容创建时间。类型 TM。",
        "StudyInstanceUID" => "检查的唯一标识符（UID）。同一次检查下的所有序列共享同一 StudyInstanceUID。",
        "SeriesInstanceUID" => "序列的唯一标识符（UID）。同一序列的所有切片/帧共享此值，用于区分不同序列。",
        "SOPInstanceUID" => "SOP 实例 UID，单个图像/对象的全局唯一标识。",
        "SOPClassUID" => "SOP 类 UID，标识对象类型（如 CT Image Storage、RT Structure Set）。",
        "StudyDescription" => "检查描述（自由文本）。常见值：“CT HEAD”、“CBCT” 等。",
        "SeriesDescription" => "序列描述（自由文本）。常见值：“Axial T2”、“骨窗重建” 等。",
        "Modality" => "设备类型/模态（枚举）。常见值：CT、MR、US、CR、DX、PT、RTSTRUCT、RTPLAN 等。",
        "Manufacturer" => "设备厂商。常见值：SIEMENS、GE、Philips、Varian、TOSHIBA 等。",
        "ManufacturerModelName" => "设备型号名称（如 “SOMATOM Force”）。",
        "DeviceSerialNumber" => "设备序列号。",
        "SoftwareVersions" => "设备软件版本。",
        "StationName" => "设备工作站名称/编号。",
        "InstitutionName" => "检查机构/医院名称。",
        "InstitutionAddress" => "检查机构地址。",
        "AccessionNumber" => "检查登记号（医院放射科流程号）。",
        "StudyID" => "检查 ID（流程编号，与 AccessionNumber 常对应）。",
        "SeriesNumber" => "序列编号，用于同一检查内区分多个序列。",
        "InstanceNumber" => "实例号，常用于同一序列内对切片/帧进行排序编号。",
        "BodyPartExamined" => "检查部位（枚举）。常见值：HEAD、CHEST、ABDOMEN、PELVIS 等。",
        "Laterality" => "左右侧（枚举）。常见值：L（左）/ R（右）。",
        "ImageLaterality" => "图像所代表的左右侧（枚举）。常见值：L / R。",
        "PatientPosition" => "患者体位（枚举）。常见值：HFS（头在前仰卧）、FFS、HFDR、FFDL 等。",
        "ViewPosition" => "投照体位（如 X 线正侧位）。常见值：AP、PA、LAT、RLO 等。",
        "SliceThickness" => "层厚（单位 mm）。常见值：0.5、1.0、2.0、5.0 等。",
        "SpacingBetweenSlices" => "相邻切片中心之间的间距（单位 mm）。",
        "SliceLocation" => "切片位置（沿扫描平面的物理距离，单位 mm），常用于排序。",
        "ImagePositionPatient" => "图像原点在患者坐标系（LPS）中的位置 (x,y,z)，单位 mm。用于序列空间排序。",
        "ImageOrientationPatient" => "图像平面方向余弦（行方向 3 值 + 列方向 3 值，共 6 值），定义图像在患者空间中的朝向。",
        "PixelSpacing" => "像素间距（行距, 列距），单位 mm。决定图像的物理尺寸。",
        "RowSpacing" | "ColumnSpacing" => "行/列方向的像素间距（单位 mm）。",
        "PixelAspectRatio" => r"像素宽高比（两整数比，如 1\1）。",
        "Rows" => "图像行数（即高度，单位像素）。",
        "Columns" => "图像列数（即宽度，单位像素）。",
        "BitsAllocated" => "每个像素样本分配的位数。常见值：8、16。",
        "BitsStored" => "每个像素样本实际存储的位数（≤ BitsAllocated）。",
        "HighBit" => "像素值中最高有效位的位索引（BitsStored-1）。",
        "PixelRepresentation" => "像素值的数据类型（枚举）。0=无符号整数；1=有符号整数（可表示负 HU 值）。",
        "SamplesPerPixel" => "每像素的样本数。1=灰度（单通道）；3=RGB（彩色）。",
        "PhotometricInterpretation" => "光度解释（枚举）。常见值：MONOCHROME1/2（灰度，1 为反相黑白）、RGB、YBR_FULL、PALETTE COLOR。",
        "PlanarConfiguration" => "彩色像素的存储方式（枚举）。0=按像素交错；1=按通道平面存储。",
        "NumberOfFrames" => "图像中的帧数（多帧图像，如定位像、动态序列、电影循环）。1 表示单帧。",
        "ImageType" => "图像类型（多值，反斜杠分隔）。常见值：如 “DERIVED\\SECONDARY\\AXIAL” 或 “ORIGINAL\\PRIMARY”。",
        "AcquisitionNumber" => "采集编号，同一次连续采集内的图像该值相同。",
        "ScanOptions" => "扫描选项（如螺旋扫描 “SPIRAL”、序列 “SEQ”）。",
        "ReconstructionDiameter" => "重建视野直径（单位 mm）。",
        "DistanceSourceToDetector" => "射线源到探测器的距离（单位 mm，CT 几何）。",
        "DistanceSourceToPatient" => "射线源到患者（等中心）的距离（单位 mm）。",
        "GantryDetectorTilt" => "机架/探测器倾角（单位 °）。",
        "TableHeight" => "检查床高度（单位 mm）。",
        "RotationDirection" => "机架旋转方向（枚举）。常见值：CW（顺时针）/ CC（逆时针）。",
        "ExposureTime" => "曝光时间（单位 ms）。",
        "XRayTubeCurrent" => "X 线管电流（单位 mA）。",
        "Exposure" => "曝光量（单位 mAs）。",
        "ExposureInuAs" => "曝光量（单位 μAs）。",
        "KVP" => "管电压（单位 kV），CT/X 线相关。",
        "GeneratorPower" => "发生器功率（单位 W）。",
        "FocalSpot" => "焦点尺寸（单位 mm）。",
        "FilterType" => "滤线器类型。",
        "ConvolutionKernel" => "重建卷积核（枚举）。常见值：“B30s”、“B70s”、“STANDARD”、“H70s” 等。",
        "RescaleIntercept" => "像素值→真实值（常 HU）的截距 b：real = slope×stored + intercept。常见值：-1024。",
        "RescaleSlope" => "像素值→真实值的斜率 m。常见值：1。",
        "RescaleType" => "刻度类型。常见值：HU（Hounsfield Unit，豪斯菲尔德单位）。",
        "WindowCenter" => "窗位（窗中心值，常为 HU），用于窗映射显示。可为多值（反斜杠分隔）。",
        "WindowWidth" => "窗宽（窗范围），用于窗映射显示。可为多值（反斜杠分隔）。",
        "WindowCenterWidthExplanation" => "窗位/窗宽对应的含义说明（如 “脑窗”、“骨窗”）。",
        "LossyImageCompression" => "是否经过有损压缩（枚举）。常见值：00（无损）/ 01（有损）。",
        "LossyImageCompressionRatio" => "有损压缩比（如 10 表示压缩 10 倍）。",
        "TransferSyntaxUID" => "传输语法 UID，决定字节序与压缩方式（如显式 VR 小端、JPEG2000、RLE）。",
        "SpecificCharacterSet" => "字符集，决定文本编码（如 ISO_IR 100=Latin1、ISO_IR 192=UTF-8、GB18030）。",
        "PixelData" => "像素数据（原始图像字节）。体积较大，界面中已省略显示。",
        "EchoTime" => "回波时间 TE（单位 ms），MR 相关。",
        "RepetitionTime" => "重复时间 TR（单位 ms），MR 相关。",
        "InversionTime" => "反转时间 TI（单位 ms），MR 相关。",
        "EchoTrainLength" => "回波链长度（ETL），MR 快速自旋回波相关。",
        "FlipAngle" => "翻转角（单位 °），MR 梯度回波相关。",
        "MagneticFieldStrength" => "主磁场强度（单位 T），MR 相关。常见值：1.5、3.0。",
        "ImagingFrequency" => "成像频率（单位 MHz），MR 相关。",
        "SequenceName" => "脉冲序列名称。",
        "PixelBandwidth" => "像素带宽（单位 Hz/像素），MR 相关。",
        "SecondaryCaptureDeviceManufacturer" => "二次采集设备厂商（屏幕截图/导入类图像）。",
        "PresentationLUTShape" => "显示 LUT 形状（枚举）。常见值：IDENTITY（线性保持不变）。",
        "RequestingPhysician" => "申请检查的医师姓名。",
        "RequestingService" => "申请检查的科室/服务。",
        "RequestedProcedureDescription" => "申请的检查项目名称。",
        "ScheduledProcedureStepDescription" => "计划执行的操作步骤描述。",
        "ImageComments" => "图像注释自由文本。",
        "IssueDateOfFilm" => "胶片制作日期。",
        "InterpretationAuthor" => "报告/注解作者。",
        _ => return None,
    };
    Some(s)
}

fn image_format_label(lower: &str) -> &'static str {
    if lower.ends_with(".png") {
        "PNG"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "JPEG"
    } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        "TIFF"
    } else {
        "Image"
    }
}

// DICOM：遍历全部数据元素，返回 (tag, vr, keyword, value)
fn dicom_tags(path: &str) -> Result<FileTags, String> {
    let obj = dicom_object::open_file(path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
    let mut rows: Vec<TagRow> = Vec::new();
    for elem in obj.iter() {
        let tag = elem.tag();
        // 跳过像素数据元素，避免载荷爆炸
        let value = if tag == Tag(0x7FE0, 0x0010)
            || tag == Tag(0x7FE0, 0x0008)
            || tag == Tag(0x7FE0, 0x0009)
        {
            "<像素数据已省略>".to_string()
        } else {
            elem.to_str().unwrap_or_default().to_string()
        };
        let vr = format!("{}", elem.vr());
        let entry = dicom_dictionary_std::StandardDataDictionary.by_tag(tag);
        let keyword = entry.map(|e| e.alias.to_string()).unwrap_or_default();
        // 悬停说明：优先用常见标签的「含义解释 + 常见值」，其余回退到 VR 含义
        let description = match tag_explanation(&keyword) {
            Some(s) => s.to_string(),
            None => format!("VR {}（{}）", vr, vr_meaning(&vr)),
        };
        rows.push(TagRow {
            tag: format!("({:04X},{:04X})", tag.group(), tag.element()),
            vr,
            keyword,
            value,
            description,
        });
    }
    Ok(FileTags {
        kind: "dicom".into(),
        filename: fname(path),
        rows,
    })
}

// NIfTI：读取头字段
fn nifti_tags(path: &str) -> Result<FileTags, String> {
    use nifti::{NiftiObject, ReaderOptions};
    let obj = ReaderOptions::new()
        .read_file(path)
        .map_err(|e| format!("读取 NIfTI 失败: {}", e))?;
    let h = obj.header();
    let mut rows: Vec<TagRow> = Vec::new();
    let push = |rows: &mut Vec<TagRow>, tag: &str, keyword: &str, value: String| {
        rows.push(TagRow {
            tag: tag.to_string(),
            vr: "-".into(),
            keyword: keyword.into(),
            value,
            description: String::new(),
        });
    };
    push(&mut rows, "sizeof_hdr", "HeaderSize", h.sizeof_hdr.to_string());
    let dim: Vec<String> = h.dim.iter().map(|v| v.to_string()).collect();
    push(&mut rows, "dim", "Dimensions", dim.join(", "));
    push(&mut rows, "datatype", "DataType", h.datatype.to_string());
    push(&mut rows, "bitpix", "BitPix", h.bitpix.to_string());
    let pixdim: Vec<String> = h.pixdim.iter().map(|v| format!("{:.4}", v)).collect();
    push(&mut rows, "pixdim", "VoxelSize", pixdim.join(", "));
    push(&mut rows, "scl_slope", "SclSlope", h.scl_slope.to_string());
    push(&mut rows, "scl_inter", "SclInter", h.scl_inter.to_string());
    push(&mut rows, "vox_offset", "VoxOffset", h.vox_offset.to_string());
    push(
        &mut rows,
        "magic",
        "Magic",
        String::from_utf8_lossy(&h.magic).trim_end().to_string(),
    );
    Ok(FileTags {
        kind: "nifti".into(),
        filename: fname(path),
        rows,
    })
}

// 常规图像（PNG/JPG/TIFF）：尽力读取格式头信息
fn image_tags(path: &str, format_label: &str) -> Result<FileTags, String> {
    let img = image::open(path).map_err(|e| format!("打开图像失败: {}", e))?;
    let (w, h) = img.dimensions();
    let color = img.color();
    let mut rows: Vec<TagRow> = Vec::new();
    rows.push(TagRow {
        tag: "format".into(),
        vr: "-".into(),
        keyword: "Format".into(),
        value: format_label.into(),
        description: String::new(),
    });
    rows.push(TagRow {
        tag: "width".into(),
        vr: "-".into(),
        keyword: "Width".into(),
        value: w.to_string(),
        description: String::new(),
    });
    rows.push(TagRow {
        tag: "height".into(),
        vr: "-".into(),
        keyword: "Height".into(),
        value: h.to_string(),
        description: String::new(),
    });
    rows.push(TagRow {
        tag: "colorType".into(),
        vr: "-".into(),
        keyword: "ColorType".into(),
        value: format!("{:?}", color),
        description: String::new(),
    });
    rows.push(TagRow {
        tag: "bitsPerPixel".into(),
        vr: "-".into(),
        keyword: "BitsPerPixel".into(),
        value: color.bits_per_pixel().to_string(),
        description: String::new(),
    });
    if let Ok(meta) = std::fs::metadata(path) {
        rows.push(TagRow {
            tag: "fileSize".into(),
            vr: "-".into(),
            keyword: "FileSize".into(),
            value: format!("{} bytes", meta.len()),
            description: String::new(),
        });
    }
    Ok(FileTags {
        kind: "image".into(),
        filename: fname(path),
        rows,
    })
}

#[tauri::command]
fn file_tags(path: String) -> Result<FileTags, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".nii") || lower.ends_with(".nii.gz") {
        nifti_tags(&path)
    } else if lower.ends_with(".dcm") || lower.ends_with(".dicom") {
        dicom_tags(&path)
    } else if lower.ends_with(".j2c") || lower.ends_with(".jph") {
        // HTJ2K：复用解码器取尺寸/帧/光度（image crate 不支持 JPEG2000）
        let bytes = std::fs::read(&path).map_err(|e| format!("读取文件失败: {}", e))?;
        let img = decode_htj2k(&bytes)?;
        let mut rows = vec![
            TagRow {
                tag: "format".into(),
                vr: "-".into(),
                keyword: "Format".into(),
                value: "HTJ2K".into(),
                description: String::new(),
            },
            TagRow {
                tag: "width".into(),
                vr: "-".into(),
                keyword: "Width".into(),
                value: img.meta.width.to_string(),
                description: String::new(),
            },
            TagRow {
                tag: "height".into(),
                vr: "-".into(),
                keyword: "Height".into(),
                value: img.meta.height.to_string(),
                description: String::new(),
            },
            TagRow {
                tag: "frames".into(),
                vr: "-".into(),
                keyword: "Frames".into(),
                value: img.meta.frames.to_string(),
                description: String::new(),
            },
            TagRow {
                tag: "photometric".into(),
                vr: "-".into(),
                keyword: "Photometric".into(),
                value: img.meta.photometric.clone(),
                description: String::new(),
            },
        ];
        if let Ok(meta) = std::fs::metadata(&path) {
            rows.push(TagRow {
                tag: "fileSize".into(),
                vr: "-".into(),
                keyword: "FileSize".into(),
                value: format!("{} bytes", meta.len()),
                description: String::new(),
            });
        }
        Ok(FileTags {
            kind: "image".into(),
            filename: fname(&path),
            rows,
        })
    } else {
        image_tags(&path, image_format_label(&lower))
    }
}

// 单文件打开时读取系列与位置字段（不解码像素），供前端做系列分组/排序/切换
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesFields {
    series_uid: Option<String>,
    series_number: Option<u32>,
    modality: Option<String>,
    instance_number: Option<u32>,
    slice_location: Option<f64>,
    image_pos_patient: Option<Vec<f64>>,
    image_orientation: Option<Vec<f64>>,
}

#[tauri::command]
fn file_series_info(path: String) -> Result<SeriesFields, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".dcm") || lower.ends_with(".dicom") {
        let obj = dicom_object::open_file(&path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
        let (series_uid, series_number, modality, instance_number, slice_location, image_pos_patient, image_orientation) =
            read_dicom_series(&*obj);
        Ok(SeriesFields {
            series_uid,
            series_number,
            modality,
            instance_number,
            slice_location,
            image_pos_patient,
            image_orientation,
        })
    } else {
        Ok(SeriesFields {
            series_uid: None,
            series_number: None,
            modality: None,
            instance_number: None,
            slice_location: None,
            image_pos_patient: None,
            image_orientation: None,
        })
    }
}

// 标签信息导出：根据格式生成 JSON / CSV 并写入用户指定路径
#[tauri::command]
fn export_tags(path: String, format: String, rows: Vec<TagRow>) -> Result<String, String> {
    let content = if format.eq_ignore_ascii_case("csv") {
        tags_to_csv(&rows)
    } else {
        serde_json::to_string_pretty(&rows).map_err(|e| format!("序列化 JSON 失败: {}", e))?
    };
    std::fs::write(&path, content).map_err(|e| format!("写入文件失败: {}", e))?;
    Ok(path)
}

fn tags_to_csv(rows: &[TagRow]) -> String {
    let mut s = String::from("Tag,VR,Keyword,Value,Description\n");
    for r in rows {
        s.push_str(&csv_field(&r.tag));
        s.push(',');
        s.push_str(&csv_field(&r.vr));
        s.push(',');
        s.push_str(&csv_field(&r.keyword));
        s.push(',');
        s.push_str(&csv_field(&r.value));
        s.push(',');
        s.push_str(&csv_field(&r.description));
        s.push('\n');
    }
    s
}

fn csv_field(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') {
        let mut s = String::from("\"");
        s.push_str(&v.replace('"', "\"\""));
        s.push('"');
        s
    } else {
        v.to_string()
    }
}

// 从文件夹导入：扫描顶层影像文件，返回概要信息（不解码像素）
#[tauri::command]
fn list_folder_images(dir: String) -> Result<Vec<ImageInfo>, String> {
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("读取文件夹失败: {}", e))?;
    let mut out: Vec<ImageInfo> = Vec::new();
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let path = p.to_string_lossy().to_string();
        let Some(kind) = classify_image_kind(&path) else {
            continue; // 跳过不支持/非影像
        };
        // 单文件失败不影响整体，跳过即可
        if let Ok(info) = folder_image_info(&path, kind) {
            out.push(info);
        }
    }
    Ok(group_and_sort_series(out))
}

fn folder_image_info(path: &str, kind: &str) -> Result<ImageInfo, String> {
    let filename = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    // 系列与位置字段（仅 DICOM 有值，其它类型均为 None）
    let (series_uid, series_number, modality, instance_number, slice_location, image_pos_patient, image_orientation) =
        if kind == "dicom" {
            let obj =
                dicom_object::open_file(path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
            read_dicom_series(&*obj)
        } else {
            (None, None, None, None, None, None, None)
        };
    let base = |w: u32, h: u32, frames: u32| ImageInfo {
        path: path.to_string(),
        filename: filename.clone(),
        width: w,
        height: h,
        frames,
        kind: kind.into(),
        series_uid: series_uid.clone(),
        series_number,
        modality: modality.clone(),
        instance_number,
        slice_location,
        image_pos_patient: image_pos_patient.clone(),
        image_orientation: image_orientation.clone(),
        series_group: None,
        series_label: None,
    };
    if kind == "dicom" {
        let obj = dicom_object::open_file(path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
        let (w, h, frames) = read_dicom_dims(&*obj)?;
        Ok(base(w, h, frames))
    } else if kind == "htj2k" {
        let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
        let img = decode_htj2k(&bytes)?;
        Ok(base(img.meta.width, img.meta.height, img.meta.frames))
    } else {
        let img = image::open(path).map_err(|e| format!("打开图像失败: {}", e))?;
        let (w, h) = img.dimensions();
        Ok(base(w, h, 1))
    }
}

// 递归收集目录下所有文件（含子目录）
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.filter_map(|e| e.ok()) {
            let p = ent.path();
            if p.is_dir() {
                collect_files(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
}

// 文件夹导入支持的类型判定（NIfTI 暂不支持文件夹导入，仍用「打开文件」）
fn classify_image_kind(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.ends_with(".dcm") || lower.ends_with(".dicom") {
        Some("dicom")
    } else if lower.ends_with(".j2c") || lower.ends_with(".jph") {
        Some("htj2k")
    } else if lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".tif")
        || lower.ends_with(".tiff")
    {
        Some("image")
    } else {
        None
    }
}

// 取 DICOM 文本元素（已 trim），便于聚合 Study/Series 元信息
fn elem_str(obj: &InMemDicomObject, name: &str) -> Option<String> {
    obj.element_by_name(name)
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_string())
}

// 递归扫描文件夹，按 StudyInstanceUID → SeriesInstanceUID 两级聚合；非 DICOM 归入 others
#[tauri::command]
fn scan_folder_series(dir: String) -> Result<SeriesTree, String> {
    let root = Path::new(&dir);
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(root, &mut files);

    // key = (study_uid, series_uid)
    let mut series_paths: std::collections::HashMap<
        (Option<String>, Option<String>),
        Vec<String>,
    > = std::collections::HashMap::new();
    let mut study_meta: std::collections::HashMap<
        Option<String>,
        (Option<String>, Option<String>, Option<String>),
    > = std::collections::HashMap::new();
    let mut series_meta: std::collections::HashMap<
        (Option<String>, Option<String>),
        (Option<String>, Option<u32>, Option<String>, Option<String>, Option<String>),
    > = std::collections::HashMap::new();
    let mut others: Vec<String> = Vec::new();

    for p in &files {
        let path = p.to_string_lossy().to_string();
        let Some(kind) = classify_image_kind(&path) else {
            continue;
        };
        if kind != "dicom" {
            others.push(path);
            continue;
        }
        let obj = match dicom_object::open_file(&path) {
            Ok(o) => o,
            Err(_) => {
                others.push(path);
                continue;
            }
        };
        let study_uid = elem_str(&obj, "StudyInstanceUID");
        let series_uid = elem_str(&obj, "SeriesInstanceUID");
        let patient_name = elem_str(&obj, "PatientName");
        let patient_id = elem_str(&obj, "PatientID");
        let study_date = elem_str(&obj, "StudyDate");
        let series_date = elem_str(&obj, "SeriesDate").or_else(|| elem_str(&obj, "StudyDate"));
        let modality = elem_str(&obj, "Modality");
        let series_number = attr_u32_opt(&obj, "SeriesNumber");
        let series_description = elem_str(&obj, "SeriesDescription");
        let key = (study_uid.clone(), series_uid.clone());
        series_paths.entry(key.clone()).or_default().push(path);
        study_meta
            .entry(study_uid.clone())
            .or_insert((patient_name.clone(), patient_id.clone(), study_date.clone()));
        series_meta.entry(key).or_insert((
            modality,
            series_number,
            series_description,
            series_date,
            patient_name,
        ));
    }

    let mut study_map: std::collections::HashMap<Option<String>, Vec<SeriesBrief>> =
        std::collections::HashMap::new();
    for ((study_uid, series_uid), paths) in series_paths {
        let (modality, series_number, series_description, series_date, patient_name) = series_meta
            .get(&(study_uid.clone(), series_uid.clone()))
            .cloned()
            .unwrap_or_default();
        let brief = SeriesBrief {
            study_uid: study_uid.clone(),
            series_uid: series_uid.clone(),
            modality,
            series_number,
            series_description,
            patient_name,
            patient_id: study_meta.get(&study_uid).and_then(|m| m.1.clone()),
            series_date,
            study_date: study_meta.get(&study_uid).and_then(|m| m.2.clone()),
            file_count: paths.len(),
            paths,
        };
        study_map.entry(study_uid).or_default().push(brief);
    }

    let mut studies: Vec<StudyBrief> = study_map
        .into_iter()
        .map(|(study_uid, mut series)| {
            series.sort_by_key(|s| s.series_number.unwrap_or(u32::MAX));
            let meta = study_meta.get(&study_uid);
            StudyBrief {
                study_uid,
                patient_name: meta.and_then(|m| m.0.clone()),
                patient_id: meta.and_then(|m| m.1.clone()),
                study_date: meta.and_then(|m| m.2.clone()),
                series,
            }
        })
        .collect();
    studies.sort_by_key(|s| s.study_uid.clone().unwrap_or_default());

    let others_brief = if others.is_empty() {
        None
    } else {
        Some(SeriesBrief {
            study_uid: None,
            series_uid: None,
            modality: None,
            series_number: None,
            series_description: Some("其他影像文件".into()),
            patient_name: None,
            patient_id: None,
            series_date: None,
            study_date: None,
            file_count: others.len(),
            paths: others,
        })
    };

    Ok(SeriesTree {
        studies,
        others: others_brief,
    })
}

// 加载指定序列的文件路径列表：构建 ImageInfo 并按系列分组排序（仅所选序列进入视图）
#[tauri::command]
fn load_series_files(paths: Vec<String>) -> Result<Vec<ImageInfo>, String> {
    let mut infos: Vec<ImageInfo> = Vec::new();
    for p in &paths {
        if let Some(kind) = classify_image_kind(p) {
            if let Ok(info) = folder_image_info(p, kind) {
                infos.push(info);
            }
        }
    }
    if infos.is_empty() {
        return Err("所选序列未找到可识别的影像文件".into());
    }
    let mut sorted = group_and_sort_series(infos);
    // 非 DICOM（others）序列无 series_uid，统一打标签便于状态栏分组显示
    if sorted[0].series_uid.is_none() {
        for it in &mut sorted {
            it.series_group = Some(0);
            it.series_label = Some("其他影像文件".into());
        }
    }
    Ok(sorted)
}

// 读取 DICOM 的系列与位置字段（不解码像素），用于按系列分组、位置排序、系列内切换
fn read_dicom_series(
    obj: &InMemDicomObject,
) -> (
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<u32>,
    Option<f64>,
    Option<Vec<f64>>,
    Option<Vec<f64>>,
) {
    let series_uid = obj
        .element_by_name("SeriesInstanceUID")
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_string());
    let series_number = attr_u32_opt(obj, "SeriesNumber");
    let modality = obj
        .element_by_name("Modality")
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_string());
    let instance_number = attr_u32_opt(obj, "InstanceNumber");
    let slice_location = elem_f64_first(obj, "SliceLocation");
    let image_pos_patient = elem_vec_f64(obj, "ImagePositionPatient");
    let image_orientation = elem_vec_f64(obj, "ImageOrientationPatient");
    (series_uid, series_number, modality, instance_number, slice_location, image_pos_patient, image_orientation)
}

fn attr_u32_opt(obj: &InMemDicomObject, name: &str) -> Option<u32> {
    obj.element_by_name(name)
        .ok()
        .and_then(|e| e.to_str().ok())
        .and_then(|s| s.trim().split('\\').next().and_then(|v| v.parse::<u32>().ok()))
}

fn elem_f64_first(obj: &InMemDicomObject, name: &str) -> Option<f64> {
    obj.element_by_name(name)
        .ok()
        .and_then(|e| e.to_str().ok())
        .and_then(|s| s.trim().split('\\').next().and_then(|v| v.parse::<f64>().ok()))
}

fn elem_vec_f64(obj: &InMemDicomObject, name: &str) -> Option<Vec<f64>> {
    obj.element_by_name(name)
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.split('\\').filter_map(|v| v.trim().parse::<f64>().ok()).collect())
}

// 三个分量叉积
fn cross3(a: &[f64; 3], b: &[f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
// 归一化
fn normalize3(v: &[f64; 3]) -> [f64; 3] {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if n == 0.0 {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / n, v[1] / n, v[2] / n]
    }
}
// 解析 DICOM DS（十进制字符串，反斜杠分隔）为多值 f64 向量
fn parse_ds_vec(s: &str) -> Option<Vec<f64>> {
    let v: Vec<f64> = s
        .split('\\')
        .filter_map(|x| x.trim().parse::<f64>().ok())
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

// 读取 Enhanced 多帧 DICOM 的逐帧图像位置，用于按解剖位置重排帧序：
// PerFrameFunctionalGroupsSequence -> 各项 -> PlanePositionSequence -> ImagePositionPatient(0020,0032)
// 返回每帧的位置向量（长度需等于帧数）；非 Enhanced 多帧或无逐帧位置时返回 None。
fn per_frame_positions(obj: &InMemDicomObject) -> Option<Vec<Vec<f64>>> {
    let pfgs = obj.element_by_name("PerFrameFunctionalGroupsSequence").ok()?;
    let items = pfgs.items()?;
    let mut out: Vec<Vec<f64>> = Vec::with_capacity(items.len());
    for item in items.iter() {
        let pps = item.element_by_name("PlanePositionSequence").ok()?;
        let pps_item = pps.items()?.first()?;
        let ipp_elem = pps_item.element_by_name("ImagePositionPatient").ok()?;
        let s = ipp_elem.to_str().ok()?;
        match parse_ds_vec(&s) {
            Some(v) if v.len() == 3 => out.push(v),
            _ => return None,
        }
    }
    Some(out)
}

// 同系列内的排序键：优先 ImagePositionPatient 沿法向量投影；其次 InstanceNumber；再次 SliceLocation
fn series_sort_key(
    ipp: &Option<Vec<f64>>,
    normal: &Option<[f64; 3]>,
    instance: Option<u32>,
    slice: Option<f64>,
) -> f64 {
    if let (Some(pos), Some(n)) = (ipp, normal) {
        if pos.len() == 3 {
            let proj = pos[0] * n[0] + pos[1] * n[1] + pos[2] * n[2];
            // 方向校正：DICOM 患者坐标系 +Z = superior；normal[2]<0 时投影升序对应
            // superior→inferior，取反使升序=inferior→superior（末帧=superior，与滚动条顶部一致）。
            let flip = if n[2] >= 0.0 { 1.0 } else { -1.0 };
            return proj * flip;
        }
    }
    if let Some(i) = instance {
        return i as f64;
    }
    if let Some(s) = slice {
        return s;
    }
    0.0
}

// 按 SeriesInstanceUID 分组、组内按位置排序、组间稳定排序；结果带 series_group / series_label
fn group_and_sort_series(items: Vec<ImageInfo>) -> Vec<ImageInfo> {
    use std::collections::HashMap;
    if items.is_empty() {
        return items;
    }
    let mut groups: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        groups.entry(it.series_uid.clone()).or_default().push(i);
    }
    // 组间顺序：有系列按 (SeriesNumber, Modality, 文件名)，无系列(None) 放最后并按文件名
    let mut group_keys: Vec<Option<String>> = groups.keys().cloned().collect();
    group_keys.sort_by(|a, b| {
        let rep = |k: &Option<String>| -> (u64, String, String) {
            match k {
                None => (u64::MAX, "\u{ffff}".to_string(), "\u{ffff}".to_string()),
                Some(_uid) => {
                    let first = &items[*groups.get(k).unwrap().first().unwrap()];
                    let sno = first.series_number.unwrap_or(u32::MAX) as u64;
                    (sno, first.modality.clone().unwrap_or_default(), first.filename.clone())
                }
            }
        };
        let (sa, ma, fa) = rep(a);
        let (sb, mb, fb) = rep(b);
        sa.cmp(&sb).then(ma.cmp(&mb)).then(fa.cmp(&fb))
    });
    let mut result: Vec<ImageInfo> = Vec::with_capacity(items.len());
    let mut group_no: u32 = 0;
    for key in group_keys {
        let idxs = &groups[&key];
        // 系列法向量：取组内第一个同时具备 IOP(6) 与 IPP(3) 的文件
        let normal: Option<[f64; 3]> = idxs.iter().find_map(|&i| {
            let it = &items[i];
            match (&it.image_orientation, &it.image_pos_patient) {
                (Some(o), Some(p)) if o.len() == 6 && p.len() == 3 => {
                    Some(normalize3(&cross3(&[o[0], o[1], o[2]], &[o[3], o[4], o[5]])))
                }
                _ => None,
            }
        });
        let mut order: Vec<usize> = idxs.clone();
        order.sort_by(|&a, &b| {
            let ka = series_sort_key(
                &items[a].image_pos_patient,
                &normal,
                items[a].instance_number,
                items[a].slice_location,
            );
            let kb = series_sort_key(
                &items[b].image_pos_patient,
                &normal,
                items[b].instance_number,
                items[b].slice_location,
            );
            ka.partial_cmp(&kb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(items[a].filename.cmp(&items[b].filename))
        });
        let is_series = key.is_some();
        let label = if is_series {
            let first = &items[order[0]];
            let modality = first.modality.clone().unwrap_or_else(|| "Series".to_string());
            let sno = first
                .series_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string());
            let uid = key.as_ref().unwrap();
            let short = &uid[uid.len().saturating_sub(8)..];
            format!("{} Series {} · {}", modality, sno, short)
        } else {
            String::new()
        };
        for &i in &order {
            let mut it = items[i].clone();
            if is_series {
                it.series_group = Some(group_no);
                it.series_label = Some(label.clone());
            } else {
                it.series_group = None;
                it.series_label = None;
            }
            result.push(it);
        }
        if is_series {
            group_no += 1;
        }
    }
    result
}

fn attr_u32(obj: &InMemDicomObject, name: &str, default: u32) -> u32 {
    obj.element_by_name(name)
        .ok()
        .and_then(|e| e.to_str().ok())
        .and_then(|s| {
            s.trim()
                .split('\\')
                .next()
                .and_then(|v| v.parse::<u32>().ok())
        })
        .unwrap_or(default)
}

// 仅读头获取尺寸（不解码像素），用于文件夹导入的概要信息
fn read_dicom_dims(obj: &InMemDicomObject) -> Result<(u32, u32, u32), String> {
    let cols = attr_u32(obj, "Columns", 0);
    let rows = attr_u32(obj, "Rows", 0);
    let frames = attr_u32(obj, "NumberOfFrames", 1);
    if cols == 0 || rows == 0 {
        return Err("DICOM 缺少尺寸信息 (Columns/Rows)".into());
    }
    Ok((cols, rows, frames))
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
    fn file_tags_sample_dicom() {
        let tags = file_tags("tests/sample.dcm".to_string()).expect("file_tags");
        assert_eq!(tags.kind, "dicom");
        assert!(!tags.rows.is_empty(), "DICOM 标签不应为空");
        // 至少应含 Modality (0008,0060)
        assert!(
            tags.rows.iter().any(|r| r.tag == "(0008,0060)"),
            "应含 Modality 标签 (0008,0060)"
        );
        // 像素数据应被省略，不得作为标签值
        assert!(
            !tags.rows.iter().any(|r| r.value.contains("像素数据已省略") && r.tag != "(7FE0,0010)"),
            "非像素数据元素不应被省略"
        );
    }

    #[test]
    fn list_folder_images_cbct() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../data/CBCT");
        let infos = list_folder_images(dir.to_string_lossy().to_string()).expect("list_folder_images");
        assert!(infos.len() > 100, "CBCT 文件夹应扫描到大量影像，实际 {}", infos.len());
        // 排序后顺序按系列分组+位置排序，首文件名不再固定；验证完整性与无重复导入
        let unique = infos
            .iter()
            .map(|i| &i.filename)
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert_eq!(unique, infos.len(), "导入文件应唯一，不应重复");
        let first = &infos[0];
        assert_eq!(first.kind, "dicom");
        assert_eq!(first.width, 390);
        assert_eq!(first.height, 390);
        assert_eq!(first.frames, 1);
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
            export_jpeg,
            file_tags,
            list_folder_images,
            file_series_info,
            export_tags,
            scan_folder_series,
            load_series_files
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
