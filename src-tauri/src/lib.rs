#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dicom_pixeldata::{ConvertOptions, ModalityLutOption, PixelDecoder, PixelRepresentation};
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
use dicom_object::{mem::InMemElement, FileDicomObject, FileMetaTableBuilder, InMemDicomObject};
use dicom_core::{PrimitiveValue, VR};
use dicom_core::value::{InMemFragment, PixelFragmentSequence, Value};
use dicom_core::header::Length;
use dicom_core::value::fragments::Fragments;
// 传输语法注册表：用于显式以「显式 VR 小端」编码器写出 HTJ2K 等库未注册的压缩传输语法
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_encoding::transfer_syntax::TransferSyntaxIndex;
use std::fs::File;
use std::io::{BufWriter, Write};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use pbkdf2::pbkdf2 as pbkdf2_derive;
use sha2::Sha256;
use hmac::Hmac;
use md5::{Digest, Md5};
use rand::Rng;
use nifti::{NiftiObject, ReaderOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::Emitter;
// HTJ2K 传输语法未包含在 dicom-rs 内置注册表，需在运行时注册，否则 open_file 解析数据集会因
// 无法识别传输语法而报「传输语法错误」（见 decode_dicom_file 中对 HTJ2K 的自研解码拦截）。
use dicom_encoding::submit_ele_transfer_syntax;
use dicom_encoding::Codec;

// 注册 HTJ2K 传输语法为「显式 VR 小端 + 封装像素数据」：dicom-rs 仅用其解析数据集各元素，
// 像素保持封装（不解码），随后由 decode_dicom_file 的自研 decode_dicom_htj2k 解码。
// .201/.202 为 DICOM 官方标准 UID（导出侧使用）；.200/.203 为历史/兼容 UID，一并注册以便回读旧文件。
submit_ele_transfer_syntax!("1.2.840.10008.1.2.4.201", "HTJ2K Lossless", Codec::EncapsulatedPixelData(None, None));
submit_ele_transfer_syntax!("1.2.840.10008.1.2.4.202", "HTJ2K Lossy", Codec::EncapsulatedPixelData(None, None));
submit_ele_transfer_syntax!("1.2.840.10008.1.2.4.200", "HTJ2K Lossless (legacy)", Codec::EncapsulatedPixelData(None, None));
submit_ele_transfer_syntax!("1.2.840.10008.1.2.4.203", "HTJ2K (legacy 203)", Codec::EncapsulatedPixelData(None, None));

#[derive(Serialize, Clone)]
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

#[derive(Serialize, Clone)]
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
    // 若 DICOM 经过本软件「加密脱敏」，则给出算法标识；前端据此提示输入密码解密
    encrypted_anon: Option<String>,
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

    // 压缩传输语法回退：dicom-pixeldata 0.7 仅内置 JPEG(50/51)/RLE(5) 解码器，
    // 对 JPEG-LS(80/81) 与 HTJ2K(201/202/203，含已弃用的自定义 200) 无 dicom-pixeldata
    // 原生解码器，需走项目自带纯 Rust 解码，否则会出现 "Unsupported TransferSyntax" 错误。
    let ts = obj.meta().transfer_syntax.clone();
    if ts == TS_JPEGLS_LOSSLESS || ts == TS_JPEGLS_LOSS {
        return decode_dicom_jpegls(&obj, path);
    }
    if ts == TS_HTJ2K_LOSSLESS || ts == TS_HTJ2K_LOSSY || ts == "1.2.840.10008.1.2.4.203" {
        return decode_dicom_htj2k(&obj, path);
    }

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

/// JPEG-LS 解码回退（dicom-pixeldata 0.7 未内置 JPEG-LS 解码器）。
/// 项目自带 pure_jpegls（纯 Rust，ITU-T T.87）对 TS 1.2.840.10008.1.2.4.80（无损）/ .81（近无损）
/// 做精确逆解码：提取封装 PixelData 片段（每帧一个 fragment）→ 逐帧 pure_jpegls::decode →
/// 按 BitsAllocated / PixelRepresentation 还原存储值 → 应用 Modality LUT（Rescale）得 HU(f32)。
/// 多帧 Enhanced 序列按逐帧平面位置重排，与传统多帧保持原序，与标准解码路径一致。
fn decode_dicom_jpegls(
    obj: &FileDicomObject<InMemDicomObject>,
    path: &str,
) -> Result<DicomImage, String> {
    use jpegls::decode as jpegls_decode;

    let attr_u32 = |name: &str, default: u32| -> u32 {
        obj.element_by_name(name)
            .ok()
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(default)
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

    let width = attr_u32("Columns", 0);
    let height = attr_u32("Rows", 0);
    let frames = attr_u32("NumberOfFrames", 1);
    let samples = attr_u32("SamplesPerPixel", 1);
    let bits_allocated = attr_u32("BitsAllocated", 16);
    let bits_stored = attr_u32("BitsStored", bits_allocated);
    let pixel_representation = attr_u32("PixelRepresentation", 0);
    let signed = pixel_representation == 1;

    if width == 0 || height == 0 {
        return Err("JPEG-LS 解码失败：缺少 Rows/Columns".into());
    }
    if samples != 1 {
        return Err(format!(
            "暂仅支持单通道（灰度）影像，当前每像素样本数 = {}",
            samples
        ));
    }
    if !(8..=16).contains(&bits_allocated) {
        return Err(format!(
            "JPEG-LS 仅支持 8/16 bit，当前 BitsAllocated = {}",
            bits_allocated
        ));
    }

    let slope = attr_f64("RescaleSlope", 1.0);
    let intercept = attr_f64("RescaleIntercept", 0.0);

    // 提取封装 PixelData 片段（BOT 单独存储，fragments() 仅返回帧数据片段）
    let pd_elem = obj
        .element_by_name("PixelData")
        .map_err(|e| format!("读取 PixelData 失败: {}", e))?;
    let frags: Vec<Vec<u8>> = match pd_elem.value() {
        Value::PixelSequence(seq) => seq.fragments().to_vec(),
        _ => return Err("JPEG-LS 解码失败：PixelData 非封装格式".into()),
    };

    let frame_count = frames as usize;
    // 片段数 == 帧数：逐帧解码；单帧多片段：合并后整体解码。
    let per_frame: Vec<Vec<u8>> = if frags.len() == frame_count {
        frags
    } else if frame_count == 1 {
        vec![frags.concat()]
    } else {
        return Err(format!(
            "JPEG-LS 片段数({}) 与帧数({}) 不匹配且非单帧，无法划分帧边界",
            frags.len(),
            frame_count
        ));
    };

    let npx = (width * height) as usize;
    let mut hu: Vec<f32> = Vec::with_capacity(frame_count * npx);

    for frag in &per_frame {
        let (decoded, dw, dh) = jpegls_decode(frag, width, height)
            .map_err(|e| format!("JPEG-LS 解码失败: {}", e))?;
        if dw as usize != width as usize || dh as usize != height as usize {
            return Err(format!(
                "JPEG-LS 解码尺寸不符：期望 {}x{}，实际 {}x{}",
                width, height, dw, dh
            ));
        }
        // u16 → 存储值（有符号按位还原）→ HU
        let stored: Vec<f32> = if bits_allocated <= 8 {
            decoded.iter().map(|&v| (v as u8) as f32).collect()
        } else if signed {
            decoded.iter().map(|&v| (v as i16) as f32).collect()
        } else {
            decoded.iter().map(|&v| v as f32).collect()
        };
        for s in stored {
            hu.push((s as f64 * slope + intercept) as f32);
        }
    }

    // 多帧 Enhanced：按逐帧平面位置重排（与传统多帧保持原序）；与标准解码路径一致
    if frame_count > 1 {
        if let (Some(oop), Some(per_pos)) = (
            obj.element_by_name("ImageOrientationPatient")
                .ok()
                .and_then(|e| e.to_str().ok())
                .and_then(|s| parse_ds_vec(&s)),
            per_frame_positions(obj),
        ) {
            if oop.len() == 6 && per_pos.len() == frame_count {
                let normal = normalize3(&cross3(
                    &[oop[0], oop[1], oop[2]],
                    &[oop[3], oop[4], oop[5]],
                ));
                let flip = if normal[2] >= 0.0 { 1.0 } else { -1.0 };
                let mut idxs: Vec<usize> = (0..frame_count).collect();
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

    let window_center_in = attr_f64("WindowCenter", 40.0);
    let window_width_in = attr_f64("WindowWidth", 400.0);
    let (window_center, window_width) = window_to_hu(window_center_in, window_width_in, slope, intercept);

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
            bits_stored: bits_stored as u16,
            pixel_representation: pixel_representation as u16,
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

/// HTJ2K 解码回退（dicom-pixeldata 0.7 未内置 HTJ2K 解码器）。
/// 复用项目自带 openjph-core 解码器：取首个 PixelData 片段（单帧 DICOM 常见；
/// 合并多帧仅解码首帧，与 load_htj2k 行为一致）→ decode_htj2k。
/// 覆盖本应用非标准 UID(.200/.201) 与官方 UID(.203)。
fn decode_dicom_htj2k(
    obj: &FileDicomObject<InMemDicomObject>,
    path: &str,
) -> Result<DicomImage, String> {
    let pd_elem = obj
        .element_by_name("PixelData")
        .map_err(|e| format!("读取 PixelData 失败: {}", e))?;
    let frag: Vec<u8> = match pd_elem.value() {
        Value::PixelSequence(seq) => {
            let frags = seq.fragments();
            if frags.is_empty() {
                return Err("HTJ2K 解码失败：PixelData 无片段".into());
            }
            frags[0].clone()
        }
        _ => return Err("HTJ2K 解码失败：PixelData 非封装格式".into()),
    };
    let mut img = decode_htj2k(&frag)?;
    img.meta.path = path.to_string();
    img.meta.filename = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.dcm")
        .to_string();
    Ok(img)
}

#[cfg(test)]
mod jpegls_decode_tests {
    use super::*;

    /// JPEG-LS DICOM 解码回退的往返验证：用导出路径 encode_jpegls_frames 构造封装 PixelData，
    /// 再经 decode_dicom_jpegls 回读，确认存储值无损还原（无窗映射、无符号处理）。
    #[test]
    fn decode_jpegls_dicom_roundtrip() {
        let info = PixelInfo {
            bits_allocated: 16,
            signed: false,
            samples: 1,
            width: 4,
            height: 4,
        };
        let orig: Vec<u16> = (0u16..16).collect();
        let frame: Vec<u8> = orig.iter().flat_map(|v| v.to_le_bytes()).collect();
        let pd_elem = encode_jpegls_frames(&[frame], &info, 0).expect("encode jpegls");

        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.7")
            .media_storage_sop_instance_uid("1.2.3")
            .transfer_syntax(TS_JPEGLS_LOSSLESS)
            .implementation_class_uid("2.25.12638147865491203746")
            .build()
            .expect("meta");
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0010),
            VR::US,
            PrimitiveValue::from(4u16),
        )); // Rows
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0011),
            VR::US,
            PrimitiveValue::from(4u16),
        )); // Columns
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0002),
            VR::US,
            PrimitiveValue::from(1u16),
        )); // SamplesPerPixel
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0100),
            VR::US,
            PrimitiveValue::from(16u16),
        )); // BitsAllocated
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0101),
            VR::US,
            PrimitiveValue::from(16u16),
        )); // BitsStored
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0103),
            VR::US,
            PrimitiveValue::from(0u16),
        )); // PixelRepresentation
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0008),
            VR::IS,
            PrimitiveValue::from(1i32),
        )); // NumberOfFrames
        obj.put(InMemElement::new(
            Tag(0x0028, 0x0004),
            VR::CS,
            PrimitiveValue::from("MONOCHROME2"),
        )); // PhotometricInterpretation
        obj.put(pd_elem);

        let img = decode_dicom_jpegls(&obj, "test").expect("decode jpegls");
        assert_eq!(img.meta.width, 4);
        assert_eq!(img.meta.height, 4);
        assert_eq!(img.meta.frames, 1);
        let hu: Vec<f32> = img
            .pixel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(hu, orig.iter().map(|v| *v as f32).collect::<Vec<_>>());
    }
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

// 切片像素解码缓存：load_dicom_meta 与 load_dicom_pixels 共享同一次解码结果，
// 避免"取元数据"与"取像素"两次调用重复解码；有界 LRU 防止大量切片常驻内存。
struct DecodedCache {
    inner: Mutex<VecDeque<(String, Arc<DicomImage>)>>,
    cap: usize,
}
static DECODE_CACHE: OnceLock<DecodedCache> = OnceLock::new();
fn decode_cache() -> &'static DecodedCache {
    DECODE_CACHE.get_or_init(|| DecodedCache {
        inner: Mutex::new(VecDeque::new()),
        cap: 16,
    })
}
fn cache_get(path: &str) -> Option<Arc<DicomImage>> {
    let g = decode_cache().inner.lock().unwrap();
    g.iter().find(|(p, _)| p == path).map(|(_, v)| v.clone())
}
fn cache_put(path: String, img: Arc<DicomImage>) {
    let c = decode_cache();
    let mut g = c.inner.lock().unwrap();
    if g.iter().any(|(p, _)| p == &path) {
        return;
    }
    g.push_back((path, img));
    while g.len() > c.cap {
        g.pop_front();
    }
}
fn decode_or_cache(path: &str) -> Result<Arc<DicomImage>, String> {
    if let Some(a) = cache_get(path) {
        return Ok(a);
    }
    let img = decode_dicom_file(path)?;
    let a = Arc::new(img);
    cache_put(path.to_string(), a.clone());
    Ok(a)
}

/// 仅返回元数据（不含像素）。切片滚动时先取 meta（极小 JSON），像素按需经二进制通道获取。
#[tauri::command]
fn load_dicom_meta(path: String) -> Result<DicomMeta, String> {
    Ok(decode_or_cache(&path)?.meta.clone())
}

/// 返回像素原始字节（f32 LE，长度 = width*height*frames*4），走二进制响应避免 JSON number[] 膨胀。
#[tauri::command]
fn load_dicom_pixels(path: String) -> Result<tauri::ipc::Response, String> {
    Ok(tauri::ipc::Response::new(decode_or_cache(&path)?.pixel_bytes.clone()))
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
    // 检测本软件「加密脱敏」标记：私有创建者 UNIXEL 的 (0099,UNIXEL,01) 记录算法标识
    let encrypted_anon = obj
        .private_element(0x0099, "UNIXEL", 0x01)
        .ok()
        .and_then(|el| el.value().to_str().ok().map(|c| c.to_string()))
        .filter(|s| s.trim() == "PBKDF2-HMAC-SHA256;AES-256-GCM");
    Ok(FileTags {
        kind: "dicom".into(),
        filename: fname(path),
        rows,
        encrypted_anon,
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
        encrypted_anon: None,
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
        encrypted_anon: None,
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
            encrypted_anon: None,
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

// ============ 导出 DICOM ============
//
// 设计要点（与用户确认的需求）：
// - 像素来源：保留原始像素（不套窗宽窗位、不改诊断内容）。CT 通过保留 Rescale 标签
//   维持 HU 映射；脱敏仅改元数据。
// - 传输语法：未压缩（Implicit/Explicit VR LE）、RLE Lossless、HTJ2K（TS201 无损 /
//   TS203 有损）、JPEG-LS（TS 1.2.840.10008.1.2.4.80 无损 / .81 近无损；保留原始位深
//   与符号性，优于旧 JPEG 8-bit 窗映射方案）。
// - 脱敏粒度：每范围独立选 处理方式（keep/delete/hash/encrypt/regenerate）。
//   加密：PBKDF2-HMAC-SHA256(密码,盐)→AES-256-GCM；盐与算法标识写入私有标签，
//   密码不入库（留空则默认 "unixel"）；密文映射以 JSON 存于私有标签。
// - 标识：仅 SoftwareVersions (0018,1020) = "Unixel - Hongwei Shao"（后台自动写入，前端无对应 UI）。
// - 整个序列：「输出形式」可选 单个文件（多帧，合并） / 多个文件（单帧）。
// - HTJ2K 有损程度：openjph-core 0.1.0 无公开 rate/quality API，故映射到 DWT 分解层数(1..6)。

const TS_IMPLICIT: &str = "1.2.840.10008.1.2";
const TS_EXPLICIT: &str = "1.2.840.10008.1.2.1";
const TS_RLE: &str = "1.2.840.10008.1.2.5";
// HTJ2K 传输语法采用 DICOM 官方注册表标准 UID（dicom-rs 0.7.1 已识别，可正确解析数据集；
// 仅编码器未注册，本项目用 openjph-core 自研编码与解码，故无需自定义 UID）。
// 注：早期版本曾误用自定义 UID 1.2.840.10008.1.2.4.200/.201，因不在标准注册表中，
// dicom_object::open_file 解析数据集时无法识别字节编码而报「传输语法错误」，外部查看器亦无法识别，已弃用。
const TS_HTJ2K_LOSSLESS: &str = "1.2.840.10008.1.2.4.201";
const TS_HTJ2K_LOSSY: &str = "1.2.840.10008.1.2.4.202";
// JPEG-LS：无损 TS 1.2.840.10008.1.2.4.80（NEAR=0）；近无损 TS 1.2.840.10008.1.2.4.81（NEAR>0）。
// 由 pure_jpegls 输出 ITU-T T.87 标准流，保留原始位深（8/16-bit）与符号性，不套窗宽窗位。
const TS_JPEGLS_LOSSLESS: &str = "1.2.840.10008.1.2.4.80";
const TS_JPEGLS_LOSS: &str = "1.2.840.10008.1.2.4.81";
// Multiframe Secondary Capture（合并多帧单文件时的 SOP 类，通用安全）
const MF_SC_SOP_CLASS: &str = "1.2.840.10008.5.1.4.1.1.7.4";

// 脱敏范围分组：严格按「脱敏标签.txt」指定的 DICOM Tag 定义（id 与前端 anonGroups 对齐）。
// 元组为 (group_number, element_number, 显示用 keyword)；keyword 仅用于解密面板展示，不影响脱敏目标 Tag。
struct AnonGroup {
    id: &'static str,
    tags: &'static [(u16, u16, &'static str)],
}
const ANON_GROUPS: &[AnonGroup] = &[
    // 患者身份类
    AnonGroup {
        id: "patient",
        tags: &[
            (0x0010, 0x0010, "PatientName"),
            (0x0010, 0x0020, "PatientID"),
            (0x0010, 0x0030, "PatientBirthDate"),
            (0x0010, 0x1040, "PatientAddress"),
            (0x0010, 0x2154, "PatientTelephoneNumbers"),
            (0x0010, 0x1000, "OtherPatientIDs"),
        ],
    },
    // 人员身份类：注意 (0008,1060) 为 NameOfPhysiciansReadingStudy，非 PhysiciansOfRecord(0008,1048)
    AnonGroup {
        id: "personnel",
        tags: &[
            (0x0008, 0x0090, "ReferringPhysicianName"),
            (0x0008, 0x1050, "PerformingPhysicianName"),
            (0x0008, 0x1070, "OperatorsName"),
            (0x0008, 0x1060, "NameOfPhysiciansReadingStudy"),
            (0x0008, 0x009C, "ConsultingPhysicianName"),
        ],
    },
    // 机构信息类（StationName 归机构组）
    AnonGroup {
        id: "institution",
        tags: &[
            (0x0008, 0x0080, "InstitutionName"),
            (0x0008, 0x0081, "InstitutionAddress"),
            (0x0008, 0x1040, "InstitutionalDepartmentName"),
            (0x0008, 0x1010, "StationName"),
        ],
    },
    // 设备信息类：General Equipment Module 标识（DICOM PS3.15 设备相关）。
    // 不含 (0018,1020) SoftwareVersions：后台固定覆写为 "Unixel - Hongwei Shao"（工具签名），不纳入脱敏范围。
    AnonGroup {
        id: "device",
        tags: &[
            (0x0008, 0x0070, "Manufacturer"),
            (0x0008, 0x1090, "ManufacturerModelName"),
            (0x0018, 0x1000, "DeviceSerialNumber"),
        ],
    },
    // 日期时间类：仅 Study/Series/Acquisition 的 Date/Time（不含 ContentDate/ContentTime）
    AnonGroup {
        id: "datetime",
        tags: &[
            (0x0008, 0x0020, "StudyDate"),
            (0x0008, 0x0030, "StudyTime"),
            (0x0008, 0x0022, "AcquisitionDate"),
            (0x0008, 0x0032, "AcquisitionTime"),
            (0x0008, 0x0021, "SeriesDate"),
            (0x0008, 0x0031, "SeriesTime"),
        ],
    },
    // 唯一标识类
    AnonGroup {
        id: "uid",
        tags: &[
            (0x0020, 0x000D, "StudyInstanceUID"),
            (0x0020, 0x000E, "SeriesInstanceUID"),
            (0x0008, 0x0018, "SOPInstanceUID"),
            (0x0020, 0x0052, "FrameOfReferenceUID"),
            (0x0008, 0x0017, "AcquisitionUID"),
            (0x0008, 0x0050, "AccessionNumber"),
        ],
    },
    // 自由文本类
    AnonGroup {
        id: "freetext",
        tags: &[
            (0x0008, 0x1030, "StudyDescription"),
            (0x0008, 0x103E, "SeriesDescription"),
            (0x0020, 0x4000, "ImageComments"),
            (0x0010, 0x21B0, "AdditionalPatientHistory"),
            (0x0008, 0x4000, "IdentifyingComments"),
            (0x0018, 0x9424, "AcquisitionProtocolDescription"),
        ],
    },
];

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AnonRangeArg {
    id: String,
    method: String, // keep | delete | hash | encrypt | regenerate
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportDicomArgs {
    mode: String, // "current" | "all"
    file_path: String,
    series_paths: Vec<String>,
    frame_index: u32,
    transfer_syntax: String, // implicit|explicit|rle|htj2k_lossless|htj2k_lossy|jpegls_lossless|jpegls_loss
    quality: u8,            // JPEG quality 1-100
    wc: f64,
    ww: f64,
    anon_ranges: Vec<AnonRangeArg>,
    password: String,
    restore_password: String, // 方案A：非空时还原本工具加密脱敏后再按本轮策略重脱敏
    force_layered: bool,      // 非空原密码仍要对已加密占位符叠加加密（用户确认后）
    output: String,
    multifile: bool,
}

// 源像素信息（用于重建 PixelData）
struct PixelInfo {
    bits_allocated: u16,
    signed: bool,
    samples: u16,
    width: u32,
    height: u32,
}

// ---- 工具 ----

fn read_f64_attr(obj: &FileDicomObject<InMemDicomObject>, name: &str, default: f64) -> f64 {
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
}

fn md5_hex(data: &[u8]) -> String {
    let h = Md5::digest(data);
    h.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 判断字符串是否为 32 位十六进制（即本工具 MD5 摘要产物），用于再脱敏幂等守卫。
fn is_md5_hex(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 截断字符串至 max 个字符（按 Unicode 字符计），超出追加省略号。
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// 解析 "(GGGG,EEEE)" 形式的 Tag 文本（加密映射条目所用格式）。
fn parse_tag_tuple(s: &str) -> Option<(u16, u16)> {
    let s = s.trim().trim_start_matches('(').trim_end_matches(')');
    let mut parts = s.split(',');
    let g = parts.next()?.trim();
    let e = parts.next()?.trim();
    let g = u16::from_str_radix(g, 16).ok()?;
    let e = u16::from_str_radix(e, 16).ok()?;
    Some((g, e))
}

fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_val(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("非法十六进制字符: {}", c as char)),
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("十六进制字符串长度必须为偶数".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn derive_key(pw: &str, salt: &[u8]) -> Vec<u8> {
    let mut key = vec![0u8; 32];
    pbkdf2_derive::<Hmac<Sha256>>(pw.as_bytes(), salt, 100_000, &mut key)
        .expect("PBKDF2 派生失败（盐长度非法）");
    key
}

fn aes_gcm_encrypt(key: &[u8], pt: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), pt)
        .map_err(|e| e.to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

fn aes_gcm_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let (nonce, ct) = data.split_at(12);
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|e| e.to_string())
}

fn gen_uid() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill(&mut b);
    format!("2.25.{}", u128::from_be_bytes(b))
}

fn set_tag(obj: &mut FileDicomObject<InMemDicomObject>, tag: Tag, vr: VR, val: &str) {
    obj.put(InMemElement::new(tag, vr, PrimitiveValue::from(val.to_string())));
}

fn set_tag_str(obj: &mut FileDicomObject<InMemDicomObject>, tag: Tag, val: &str) {
    if let Some(el) = obj.element(tag).ok() {
        let vr = el.vr();
        obj.put(InMemElement::new(tag, vr, PrimitiveValue::from(val.to_string())));
    }
}

/// 构造 (0012,0064) DeidentificationMethodCodeSequence 的单个代码项（自定义设计符 99UNIXEL）。
fn deid_code_item(code_value: &str, code_meaning: &str) -> InMemDicomObject {
    let mut item = InMemDicomObject::new_empty();
    item.put(InMemElement::new(
        Tag(0x0008, 0x0100),
        VR::SH,
        PrimitiveValue::from(code_value.to_string()),
    ));
    item.put(InMemElement::new(
        Tag(0x0008, 0x0102),
        VR::SH,
        PrimitiveValue::from("99UNIXEL".to_string()),
    ));
    item.put(InMemElement::new(
        Tag(0x0008, 0x0104),
        VR::LO,
        PrimitiveValue::from(code_meaning.to_string()),
    ));
    item
}

// ---- 像素提取（保留原始像素，不套 Modality LUT）----

fn extract_native_frames(
    obj: &FileDicomObject<InMemDicomObject>,
) -> Result<(Vec<Vec<u8>>, PixelInfo), String> {
    let pd = obj
        .decode_pixel_data()
        .map_err(|e| format!("解码像素数据失败: {}", e))?;
    let width = pd.columns();
    let height = pd.rows();
    let samples = pd.samples_per_pixel();
    let bits_allocated = pd.bits_allocated();
    let signed = matches!(pd.pixel_representation(), PixelRepresentation::Signed);
    let frames = pd.number_of_frames();
    // 关键：ModalityLutOption::None 不套 Rescale，得到「原始存储像素值」，
    // 与文件中保留的 RescaleSlope/Intercept 配合在读取端还原 HU。
    let opts = ConvertOptions::new().with_modality_lut(ModalityLutOption::None);
    let native: Vec<u8> = if bits_allocated <= 8 {
        pd.to_vec_with_options::<u8>(&opts)
            .map_err(|e| format!("像素解码失败: {}", e))?
    } else if signed {
        pd.to_vec_with_options::<i16>(&opts)
            .map_err(|e| format!("像素解码失败: {}", e))?
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    } else {
        pd.to_vec_with_options::<u16>(&opts)
            .map_err(|e| format!("像素解码失败: {}", e))?
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    };
    let bytes_per_sample = if bits_allocated <= 8 { 1u32 } else { 2u32 };
    let frame_bytes = (width * height * samples as u32 * bytes_per_sample) as usize;
    let mut out = Vec::with_capacity(frames as usize);
    for f in 0..frames as usize {
        let s = f * frame_bytes;
        out.push(native[s..s + frame_bytes].to_vec());
    }
    Ok((
        out,
        PixelInfo {
            bits_allocated,
            signed,
            samples,
            width,
            height,
        },
    ))
}

fn load_source_for_export(
    path: &str,
) -> Result<(FileDicomObject<InMemDicomObject>, Vec<Vec<u8>>, PixelInfo), String> {
    let lower = path.to_lowercase();
    if !(lower.ends_with(".dcm") || lower.ends_with(".dicom")) {
        return Err("导出 DICOM 仅支持 DICOM 源文件（.dcm / .dicom）".into());
    }
    let obj = dicom_object::open_file(path).map_err(|e| format!("打开文件失败: {}", e))?;
    let (frames, info) = extract_native_frames(&obj)?;
    Ok((obj, frames, info))
}

// ---- DICOM RLE（PackBits 风格，逐扫描行）----

fn rle_encode_frame(frame: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if row_bytes == 0 {
        return out;
    }
    for row in frame.chunks(row_bytes) {
        let n = row.len();
        let mut i = 0usize;
        while i < n {
            // 相同字节游程
            let mut run = 1;
            while i + run < n && row[i + run] == row[i] && run < 128 {
                run += 1;
            }
            if run >= 2 {
                let cnt = run.min(128);
                out.push((257 - cnt) as u8); // 重复运行头（129..255）
                out.push(row[i]);
                i += cnt;
            } else {
                // 字面量运行
                let mut cnt = 0usize;
                while i + cnt < n && cnt < 128 {
                    if i + cnt + 1 < n && row[i + cnt] == row[i + cnt + 1] {
                        break; // 出现重复对，交还给重复分支
                    }
                    cnt += 1;
                }
                if cnt == 0 {
                    cnt = 1;
                }
                out.push((cnt - 1) as u8); // 字面量头（0..127）
                out.extend_from_slice(&row[i..i + cnt]);
                i += cnt;
            }
        }
    }
    out
}

// ---- HTJ2K（复用 openjph-core，支持 8/16-bit 有/无符号）----

fn htj2k_encode_frame(
    frame: &[u8],
    width: u32,
    height: u32,
    bit_depth: u16,
    signed: bool,
    lossless: bool,
    degree: u8,
) -> Result<Vec<u8>, String> {
    use openjph_core::codestream::Codestream;
    use openjph_core::file::MemOutfile;
    use openjph_core::types::{Point, Size};
    let mut cs = Codestream::new();
    cs.access_siz_mut()
        .set_image_extent(Point::new(width, height));
    cs.access_siz_mut().set_num_components(1);
    cs.access_siz_mut()
        .set_comp_info(0, Point::new(1, 1), bit_depth as u32, signed);
    cs.access_siz_mut()
        .set_tile_size(Size::new(width, height));
    {
        // 有损程度：openjph-core 0.1.0 无 rate/quality API，映射到 DWT 分解层数(1..6)
        let levels = (((degree as u32).clamp(10, 100) * 5 / 100) + 1).clamp(1, 6);
        let cod = cs.access_cod_mut();
        cod.set_num_decomposition(levels);
        cod.set_reversible(lossless);
        cod.set_color_transform(false);
    }
    cs.set_planar(0);
    let mut outfile = MemOutfile::new();
    cs.write_headers(&mut outfile, &[])
        .map_err(|e| format!("HTJ2K 写头失败: {}", e))?;
    let bytes_per = if bit_depth <= 8 { 1usize } else { 2usize };
    let mut i = 0usize;
    for _ in 0..height as usize {
        let mut line: Vec<i32> = Vec::with_capacity(width as usize);
        for _ in 0..width as usize {
            let v = if bytes_per == 1 {
                frame[i] as i32
            } else {
                let lo = frame[i];
                let hi = frame[i + 1];
                if signed {
                    i16::from_le_bytes([lo, hi]) as i32
                } else {
                    u16::from_le_bytes([lo, hi]) as i32
                }
            };
            i += bytes_per;
            line.push(v);
        }
        cs.exchange(&line, 0)
            .map_err(|e| format!("HTJ2K 编码失败: {}", e))?;
    }
    cs.flush(&mut outfile)
        .map_err(|e| format!("HTJ2K flush 失败: {}", e))?;
    Ok(outfile.get_data().to_vec())
}

// ---- JPEG-LS（保留原始位深，pure_jpegls 输出 ITU-T T.87 标准流）----
//
// DICOM 传输语法：
//   - 1.2.840.10008.1.2.4.80 (JPEG-LS Lossless)        → near = 0
//   - 1.2.840.10008.1.2.4.81 (JPEG-LS Near-Lossless)   → near > 0（最大重建误差 ±near）
// 关键：不做窗映射，直接压缩原始存储像素（含 16-bit 有符号 HU），保留诊断信息；
// 显式指定 precision = bits_allocated，确保解码端按相同位深/符号性还原。

/// 将单帧原始像素编码为 JPEG-LS 比特流。
/// `near = 0` 无损（TS .80）；`near > 0` 近无损（TS .81，最大误差 ±near）。
/// 源字节按 bits_allocated 重组为 u16（有符号 int16 按位 reinterpret 保留位模式），
/// 并显式传入 precision，避免 pure_jpegls 自动推导精度导致 16-bit 有符号数据被截断。
fn jpegls_encode_frame(
    frame: &[u8],
    info: &PixelInfo,
    near: u8,
) -> Result<Vec<u8>, String> {
    use jpegls::{encode_with_options, EncodeOptions, Profile};

    let w = info.width as usize;
    let h = info.height as usize;
    let per = w * h;
    let bytes_per = if info.bits_allocated <= 8 { 1usize } else { 2usize };
    if frame.len() < per * bytes_per {
        return Err("JPEG-LS：像素数据长度不足".into());
    }

    // 重组为 u16 序列（保留位模式；有符号按位 reinterpret，解码端用相同 signedness 还原）
    let mut samples: Vec<u16> = Vec::with_capacity(per);
    if bytes_per == 1 {
        for i in 0..per {
            samples.push(frame[i] as u16);
        }
    } else if info.signed {
        for i in (0..per * 2).step_by(2) {
            samples.push(i16::from_le_bytes([frame[i], frame[i + 1]]) as u16);
        }
    } else {
        for i in (0..per * 2).step_by(2) {
            samples.push(u16::from_le_bytes([frame[i], frame[i + 1]]));
        }
    }

    let precision = info.bits_allocated as u8; // 8 或 16，显式指定
    let mut opts = EncodeOptions::default();
    opts.near = near;
    opts.profile = Profile::T87;
    opts.precision = Some(precision);
    let mut out = Vec::new();
    encode_with_options(&samples, info.width, info.height, &opts, &mut out)
        .map_err(|e| format!("JPEG-LS 编码失败: {}", e))?;
    Ok(out)
}

/// 将多帧封装为 PixelData 的 JPEG-LS 片段序列（每帧一个 fragment）。
fn encode_jpegls_frames(
    frames: &[Vec<u8>],
    info: &PixelInfo,
    near: u8,
) -> Result<InMemElement, String> {
    let mut frags: Vec<Fragments> = Vec::with_capacity(frames.len());
    for f in frames {
        let bytes = jpegls_encode_frame(f, info, near)?;
        frags.push(Fragments::new(bytes, 0));
    }
    let pfs: PixelFragmentSequence<InMemFragment> = frags.into();
    Ok(InMemElement::new(
        Tag(0x7FE0, 0x0010),
        VR::OB,
        Value::PixelSequence(pfs),
    ))
}

// ---- 像素载荷构建 ----

fn encode_htj2k_frames(
    frames: &[Vec<u8>],
    info: &PixelInfo,
    lossless: bool,
    degree: u8,
) -> Result<InMemElement, String> {
    let mut frags: Vec<Fragments> = Vec::with_capacity(frames.len());
    for f in frames {
        let bytes = htj2k_encode_frame(
            f,
            info.width,
            info.height,
            info.bits_allocated,
            info.signed,
            lossless,
            degree,
        )?;
        frags.push(Fragments::new(bytes, 0));
    }
    let pfs: PixelFragmentSequence<InMemFragment> = frags.into();
    Ok(InMemElement::new(
        Tag(0x7FE0, 0x0010),
        VR::OB,
        Value::PixelSequence(pfs),
    ))
}

fn build_pixel_payload(
    ts_arg: &str,
    frames: &[Vec<u8>],
    info: &PixelInfo,
    degree: u8,
    near: u8,
) -> Result<InMemElement, String> {
    match ts_arg {
        "implicit" | "explicit" => {
            let mut all = Vec::new();
            for f in frames {
                all.extend_from_slice(f);
            }
            let vr = if info.bits_allocated <= 8 {
                VR::OB
            } else {
                VR::OW
            };
            Ok(InMemElement::new(
                Tag(0x7FE0, 0x0010),
                vr,
                Value::Primitive(PrimitiveValue::from(all)),
            ))
        }
        "rle" => {
            let row_bytes = (info.width
                * info.samples as u32
                * (if info.bits_allocated <= 8 { 1 } else { 2 }))
                as usize;
            let frags: Vec<Fragments> = frames
                .iter()
                .map(|f| Fragments::new(rle_encode_frame(f, row_bytes), 0))
                .collect();
            let pfs: PixelFragmentSequence<InMemFragment> = frags.into();
            Ok(InMemElement::new(
                Tag(0x7FE0, 0x0010),
                VR::OB,
                Value::PixelSequence(pfs),
            ))
        }
        "htj2k_lossless" => encode_htj2k_frames(frames, info, true, degree),
        "htj2k_lossy" => encode_htj2k_frames(frames, info, false, degree),
        "jpegls_lossless" => encode_jpegls_frames(frames, info, 0),
        "jpegls_loss" => encode_jpegls_frames(frames, info, near),
        _ => Err(format!("不支持的传输语法: {}", ts_arg)),
    }
}

// ---- 脱敏 ----

fn anonymize_object(
    obj: &mut FileDicomObject<InMemDicomObject>,
    ranges: &[AnonRangeArg],
    password: &str,
    regenerate_study_series: bool,
    force_layered: bool,
) -> Result<(), String> {
    let need_encrypt = ranges.iter().any(|r| r.method == "encrypt");
    let mut salt = [0u8; 16];
    rand::thread_rng().fill(&mut salt);
    // 密码可选：留空时使用默认口令 "unixel"（不入库）
    let pw = if password.is_empty() { "unixel" } else { password };
    let key = if need_encrypt {
        derive_key(pw, &salt)
    } else {
        Vec::new()
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    // applied：是否实际执行了脱敏（任一分组 method 非 keep，含 UID 重生成）。
    // 用于决定是否写强制脱敏标记 (0012,0062)/(0012,0064)。
    let mut applied = false;
    // applied_methods：记录本次实际采用的方法类型，用于写 (0012,0063) 文本与 (0012,0064) 代码项。
    let mut applied_methods: std::collections::HashSet<&'static str> = std::collections::HashSet::new();

    for group in ANON_GROUPS {
        let method = match ranges.iter().find(|r| r.id == group.id) {
            Some(r) => r.method.as_str(),
            None => "keep",
        };
        if method == "keep" {
            continue;
        }
        applied = true;

        // UID 组特殊处理：一致随机重生成（保持实例间引用；DICOM PS3.15 标准做法）
        if group.id == "uid" && method == "regenerate" {
            applied_methods.insert("uid");
            for &(g, e, _kw) in group.tags {
                let tag = Tag(g, e);
                // Study/Series UID 仅在「整体重生成」时替换（合并同序列导出时保持分组一致）
                if (tag == Tag(0x0020, 0x000D) || tag == Tag(0x0020, 0x000E)) && !regenerate_study_series {
                    continue;
                }
                set_tag_str(obj, tag, &gen_uid());
            }
            let new_sop = obj
                .element(Tag(0x0008, 0x0018))
                .ok()
                .and_then(|e| e.to_str().ok())
                .unwrap_or_default();
            obj.meta_mut().media_storage_sop_instance_uid = new_sop.to_string();
            continue;
        }

        for &(g, e, kw) in group.tags {
            let tag = Tag(g, e);
            let cur = match obj
                .element(tag)
                .ok()
                .and_then(|el| el.to_str().ok())
            {
                Some(s) => s,
                None => continue,
            };
            match method {
                "delete" => {
                    applied_methods.insert("del");
                    obj.remove_element(tag);
                }
                "hash" => {
                    // 幂等守卫：当前值已是 32 位 MD5（本工具先前哈希），跳过避免二次哈希。
                    if is_md5_hex(cur.as_ref()) {
                        continue;
                    }
                    applied_methods.insert("hash");
                    let h = md5_hex(cur.as_bytes());
                    set_tag_str(obj, tag, &h);
                }
                "encrypt" => {
                    // 幂等守卫：当前值已是本工具加密占位符，跳过避免二次加密破坏可还原性。
                    // force_layered=true 时（用户确认叠加加密）则不跳过，直接对占位符再加密一层。
                    if !force_layered && cur.as_ref() == "ANONYMIZED-ENCRYPTED" {
                        continue;
                    }
                    applied_methods.insert("enc");
                    let ct = aes_gcm_encrypt(&key, cur.as_bytes())?;
                    let tag_str = format!("({:04X},{:04X})", g, e);
                    entries.push(serde_json::json!({
                        "tag": tag_str,
                        "kw": kw,
                        "ct_hex": to_hex(&ct),
                    }));
                    set_tag_str(obj, tag, "ANONYMIZED-ENCRYPTED");
                }
                _ => {}
            }
        }
    }

    // 强制脱敏标记：只要实际执行了脱敏就写入。
    // (0012,0062) PatientIdentityRemoved=YES；
    // (0012,0063) DeidentificationMethod 文本（汇总本次实际采用的方法）；
    // (0012,0064) DeidentificationMethodCodeSequence 按实际方法写入对应代码项（自定义设计符 99UNIXEL）。
    if applied {
        obj.put(InMemElement::new(
            Tag(0x0012, 0x0062),
            VR::CS,
            PrimitiveValue::from("YES".to_string()),
        ));

        // (0012,0063) 文本：汇总本次实际方法
        let mut labels: Vec<&str> = Vec::new();
        if applied_methods.contains("uid") {
            labels.push("UID regeneration");
        }
        if applied_methods.contains("enc") {
            labels.push("AES-256-GCM encryption");
        }
        if applied_methods.contains("hash") {
            labels.push("MD5 hashing");
        }
        if applied_methods.contains("del") {
            labels.push("tag deletion");
        }
        let method_text = if labels.is_empty() {
            "Unixel DICOM de-identification".to_string()
        } else {
            format!("Unixel DICOM de-identification: {}", labels.join(", "))
        };
        obj.put(InMemElement::new(
            Tag(0x0012, 0x0063),
            VR::LO,
            PrimitiveValue::from(method_text),
        ));

        // (0012,0064) 代码序列：按实际方法逐项写入
        let mut items: Vec<InMemDicomObject> = Vec::new();
        if applied_methods.contains("uid") {
            items.push(deid_code_item(
                "UNIXEL-ANON-UID",
                "Unixel de-identification: UID regenerated",
            ));
        }
        if applied_methods.contains("enc") {
            items.push(deid_code_item(
                "UNIXEL-ANON-ENC",
                "Unixel de-identification: AES-256-GCM encryption",
            ));
        }
        if applied_methods.contains("hash") {
            items.push(deid_code_item(
                "UNIXEL-ANON-HASH",
                "Unixel de-identification: MD5 hash",
            ));
        }
        if applied_methods.contains("del") {
            items.push(deid_code_item(
                "UNIXEL-ANON-DEL",
                "Unixel de-identification: tag deleted",
            ));
        }
        obj.put(InMemElement::new(
            Tag(0x0012, 0x0064),
            VR::SQ,
            Value::new_sequence(items, Length::UNDEFINED),
        ));
    }

    if !entries.is_empty() {
        let mapping = serde_json::json!({
            "algo": "PBKDF2-HMAC-SHA256;AES-256-GCM",
            "iterations": 100000,
            "salt_hex": to_hex(&salt),
            "entries": entries,
        });
        let json = serde_json::to_vec(&mapping).map_err(|e| format!("加密映射序列化失败: {}", e))?;
        obj.put_private_element(
            0x0099,
            "UNIXEL",
            0x01,
            VR::LO,
            PrimitiveValue::from("PBKDF2-HMAC-SHA256;AES-256-GCM"),
        )
        .map_err(|e| e.to_string())?;
        obj.put_private_element(
            0x0099,
            "UNIXEL",
            0x02,
            VR::LO,
            PrimitiveValue::from(to_hex(&salt)),
        )
        .map_err(|e| e.to_string())?;
        obj.put_private_element(0x0099, "UNIXEL", 0x03, VR::OB, PrimitiveValue::from(json))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---- 加密脱敏解密（供「更多信息」标签查看时按密码还原）----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnonDecrypted {
    tag: String,
    keyword: String,
    value: String,
}

/// 读取本软件写入的加密脱敏元数据（私有创建者 UNIXEL）。
/// 返回 (算法标识, 盐字节, 映射 JSON)。
fn read_anon_mapping(
    obj: &FileDicomObject<InMemDicomObject>,
) -> Result<(String, Vec<u8>, serde_json::Value), String> {
    let algo_el = obj
        .private_element(0x0099, "UNIXEL", 0x01)
        .map_err(|_| "未检测到本软件的加密脱敏标记".to_string())?;
    let algo = algo_el
        .value()
        .to_str()
        .map_err(|e| format!("读取算法标识失败: {}", e))?
        .to_string();
    let salt_el = obj
        .private_element(0x0099, "UNIXEL", 0x02)
        .map_err(|_| "缺少盐值（加密脱敏元数据不完整）".to_string())?;
    let salt_hex = salt_el
        .value()
        .to_str()
        .map_err(|e| format!("读取盐值失败: {}", e))?
        .to_string();
    let salt = hex_decode(&salt_hex).map_err(|e| format!("盐值解析失败: {}", e))?;
    let map_el = obj
        .private_element(0x0099, "UNIXEL", 0x03)
        .map_err(|_| "缺少加密映射（加密脱敏元数据不完整）".to_string())?;
    let map_bytes = map_el
        .value()
        .to_bytes()
        .map_err(|e| format!("读取加密映射失败: {}", e))?;
    // 修复：OB 私有元素在写入时若明文长度为奇数，写入器会按 DICOM 偶长度对齐规则补一个
    // 0x00 填充字节。该尾随字节会让 serde_json 报 "trailing characters"。解析前剥离尾随的
    // NUL(0x00)/空格(0x20) 填充（映射 JSON 始终以 '}' 结尾，尾随字节纯属填充，可安全丢弃）。
    let mut end = map_bytes.len();
    while end > 0 && (map_bytes[end - 1] == 0x00 || map_bytes[end - 1] == 0x20) {
        end -= 1;
    }
    let mapping: serde_json::Value = serde_json::from_slice(&map_bytes[..end])
        .map_err(|e| format!("解析加密映射失败: {}", e))?;
    Ok((algo, salt, mapping))
}

#[tauri::command]
fn decrypt_anon(path: String, password: String) -> Result<Vec<AnonDecrypted>, String> {
    let obj = dicom_object::open_file(&path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
    let (algo, salt, mapping) = read_anon_mapping(&obj)?;
    if algo.trim() != "PBKDF2-HMAC-SHA256;AES-256-GCM" {
        return Err(format!("不支持的加密算法: {}", algo));
    }
    // 留空时回落到导出端默认口令（与 anonymize_object 保持一致）
    let pw = if password.is_empty() {
        "unixel".to_string()
    } else {
        password
    };
    let key = derive_key(&pw, &salt);
    let entries = mapping
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or("加密映射缺少 entries 字段")?;
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let tag = e
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kw = e
            .get("kw")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ct_hex = e
            .get("ct_hex")
            .and_then(|v| v.as_str())
            .ok_or("加密条目缺少密文")?;
        let ct = hex_decode(ct_hex).map_err(|err| format!("密文解析失败（{}）: {}", tag, err))?;
        let pt = aes_gcm_decrypt(&key, &ct)
            .map_err(|_| format!("解密失败：密码错误或密文损坏（标签 {}）", tag))?;
        let value = String::from_utf8_lossy(&pt).to_string();
        out.push(AnonDecrypted {
            tag,
            keyword: kw,
            value,
        });
    }
    Ok(out)
}

// ---- 诊断 + 方案A：还原重脱敏 ----

/// 各脱敏标签的当前状态（供前端诊断展示）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnonTagStatus {
    tag: String,      // (GGGG,EEEE)
    keyword: String,
    status: String,   // raw | deleted | md5 | encrypted | missing
    preview: String,  // 截断后的当前值（用于展示）
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnonGroupStatus {
    id: String,
    tags: Vec<AnonTagStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnonDiagnosis {
    has_unixel_mapping: bool,        // 是否含本工具 UNIXEL 加密标记（可还原）
    patient_identity_removed: bool,  // (0012,0062) == YES
    method_text: Option<String>,     // (0012,0063)
    method_codes: Vec<String>,       // (0012,0064) 各代码项 CodeValue
    groups: Vec<AnonGroupStatus>,
}

/// 读取 (0012,0064) DeidentificationMethodCodeSequence 中各项的 CodeValue。
fn read_deid_codes(obj: &FileDicomObject<InMemDicomObject>) -> Vec<String> {
    let mut codes = Vec::new();
    if let Ok(seq_el) = obj.element(Tag(0x0012, 0x0064)) {
        let val = seq_el.value();
        if let Some(items) = val.items() {
            for it in items {
                if let Some(cv) = it
                    .element(Tag(0x0008, 0x0100))
                    .ok()
                    .and_then(|e| e.to_str().ok())
                {
                    codes.push(cv.to_string());
                }
            }
        }
    }
    codes
}

/// 方案A：还原本工具加密脱敏的标签，再用本轮策略重新脱敏。
/// 仅当 `password` 非空且文件含 UNIXEL 加密标记时生效。
/// - 逐条解密加密映射，将标签还原为原始值（覆盖当前的 "ANONYMIZED-ENCRYPTED" 占位符）；
/// - 移除旧 UNIXEL 私有标记 (0099,UNIXEL,01/02/03) 与旧强制脱敏标记 (0012,0062/63/64)，
///   避免与本轮新策略（可能换方法/密码、或不再脱敏）冲突或误导；
/// - 返回 true 表示已实际还原，false 表示无需/未还原（无密码或无标记）。
fn restore_anon_mapping(
    obj: &mut FileDicomObject<InMemDicomObject>,
    password: &str,
) -> Result<bool, String> {
    if password.is_empty() {
        return Ok(false);
    }
    // 仅当存在本工具加密标记才还原（避免对非本工具加密/未加密文件误改）
    if obj.private_element(0x0099, "UNIXEL", 0x01).is_err() {
        return Ok(false);
    }
    let (algo, salt, mapping) = read_anon_mapping(obj)?;
    if algo.trim() != "PBKDF2-HMAC-SHA256;AES-256-GCM" {
        return Err(format!("不支持的加密算法: {}", algo));
    }
    let pw = if password.is_empty() {
        "unixel"
    } else {
        password
    };
    let key = derive_key(pw, &salt);
    let entries = mapping
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or("加密映射缺少 entries 字段")?;
    let mut restored = 0usize;
    for e in entries {
        let tag = e
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ct_hex = e
            .get("ct_hex")
            .and_then(|v| v.as_str())
            .ok_or("加密条目缺少密文")?;
        let ct = hex_decode(ct_hex).map_err(|err| format!("密文解析失败（{}）: {}", tag, err))?;
        let pt = aes_gcm_decrypt(&key, &ct)
            .map_err(|_| format!("还原失败：密码错误或密文损坏（标签 {}）", tag))?;
        let value = String::from_utf8_lossy(&pt).to_string();
        if let Some((g, el)) = parse_tag_tuple(&tag) {
            set_tag_str(obj, Tag(g, el), &value);
            restored += 1;
        }
    }
    // 移除旧加密元数据与旧强制脱敏标记
    // 注：dicom-rs 的 UNIXEL 私有块（首个私有创建者，block=0）实际标签为
    // 创建者 (0099,0010) 与数据 (0099,0101)/(0099,0102)/(0099,0103)。
    obj.remove_element(Tag(0x0099, 0x0010));
    obj.remove_element(Tag(0x0099, 0x0101));
    obj.remove_element(Tag(0x0099, 0x0102));
    obj.remove_element(Tag(0x0099, 0x0103));
    obj.remove_element(Tag(0x0012, 0x0062));
    obj.remove_element(Tag(0x0012, 0x0063));
    obj.remove_element(Tag(0x0012, 0x0064));
    Ok(restored > 0)
}

/// 诊断单个 DICOM 文件的脱敏状态（方案A 前置步骤）。
#[tauri::command]
fn diagnose_anon(path: String) -> Result<AnonDiagnosis, String> {
    let obj = dicom_object::open_file(&path).map_err(|e| format!("打开 DICOM 失败: {}", e))?;
    let has_unixel_mapping = obj.private_element(0x0099, "UNIXEL", 0x01).is_ok();
    let patient_identity_removed = obj
        .element(Tag(0x0012, 0x0062))
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim() == "YES")
        .unwrap_or(false);
    let method_text = obj
        .element(Tag(0x0012, 0x0063))
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.to_string());
    let method_codes = read_deid_codes(&obj);

    let mut groups = Vec::with_capacity(ANON_GROUPS.len());
    for g in ANON_GROUPS {
        let mut tags = Vec::with_capacity(g.tags.len());
        for &(gg, ee, kw) in g.tags {
            let tag = Tag(gg, ee);
            let (status, preview) = match obj
                .element(tag)
                .ok()
                .and_then(|e| e.to_str().ok())
            {
                None => ("missing".to_string(), String::new()),
                Some(v) => {
                    if v.as_ref() == "ANONYMIZED-ENCRYPTED" {
                        ("encrypted".to_string(), v.to_string())
                    } else if is_md5_hex(v.as_ref()) {
                        ("md5".to_string(), v.to_string())
                    } else if v.trim().is_empty() {
                        ("deleted".to_string(), String::new())
                    } else {
                        ("raw".to_string(), truncate_str(&v, 40))
                    }
                }
            };
            tags.push(AnonTagStatus {
                tag: format!("({:04X},{:04X})", gg, ee),
                keyword: kw.to_string(),
                status,
                preview,
            });
        }
        groups.push(AnonGroupStatus {
            id: g.id.to_string(),
            tags,
        });
    }

    Ok(AnonDiagnosis {
        has_unixel_mapping,
        patient_identity_removed,
        method_text,
        method_codes,
        groups,
    })
}

// ---- 单文件写出 ----

#[allow(clippy::too_many_arguments)]
fn write_one_dicom(
    obj: &mut FileDicomObject<InMemDicomObject>,
    frames: &[Vec<u8>],
    info: &PixelInfo,
    ts_arg: &str,
    ts_uid: &str,
    _compressed: bool,
    _slope: f64,
    _intercept: f64,
    _wc: f64,
    _ww: f64,
    _invert: bool,
    quality: u8,
    near: u8,
    is_merged: bool,
    ranges: &[AnonRangeArg],
    password: &str,
    restore_password: &str,
    force_layered: bool,
    path: &str,
) -> Result<(), String> {
    // 若用户在批量转换中途取消，立即中止（避免对当前文件继续编码/写入）
    if batch_cancel_requested() {
        return Err("已取消（用户中止）".into());
    }
    // 传输语法
    obj.meta_mut().transfer_syntax = ts_uid.to_string();
    obj.meta_mut().update_information_group_length();

    // 像素数据
    let payload: InMemElement = if ts_arg == "jpegls_lossless" || ts_arg == "jpegls_loss" {
        // JPEG-LS：保留原始位深与符号性（不套窗宽窗位、不改 BitsAllocated 等）。
        // near: 0=无损(TS .80)，>0=近无损(TS .81)。
        encode_jpegls_frames(frames, info, if ts_arg == "jpegls_lossless" { 0 } else { near })?
    } else {
        build_pixel_payload(ts_arg, frames, info, quality, near)?
    };
    obj.put(payload);
    // 帧数（压缩/解压/抽取后可能与原值不同）
    obj.put(InMemElement::new(
        Tag(0x0028, 0x0008),
        VR::IS,
        PrimitiveValue::from(frames.len().to_string()),
    ));

    // 合并多帧：改为 Multiframe Secondary Capture SOP 类
    if is_merged {
        set_tag(obj, Tag(0x0008, 0x0016), VR::UI, MF_SC_SOP_CLASS);
        obj.meta_mut().media_storage_sop_class_uid = MF_SC_SOP_CLASS.to_string();
    }

    // 软件标识
    set_tag(obj, Tag(0x0018, 0x1020), VR::LO, "Unixel - Hongwei Shao");

    // 方案A：若提供原密码且文件含本工具加密标记，先还原原始值，再按本轮策略重新脱敏
    // （避免对已加密占位符二次加密、对已删/哈希值重复处理）。
    // force_layered=true 时（用户确认叠加加密）不还原，直接对已加密占位符再加密一层。
    if !restore_password.is_empty() && !force_layered {
        restore_anon_mapping(obj, restore_password)?;
    }

    // 脱敏
    anonymize_object(obj, ranges, password, is_merged, force_layered)?;

    // 数据集字节编码器选择（此处的 TS 仅决定「除 PixelData 外各 DICOM 元素的字节编码方式」，
    // 并不替代文件声明的传输语法）：
    // - 隐式 VR 小端（implicit）→ TS_IMPLICIT。
    // - 其余（未压缩 explicit 与所有压缩语法 RLE/HTJ2K/JPEG）→ TS_EXPLICIT（显式 VR 小端）。
    //   依据 DICOM PS3.5：所有压缩传输语法的数据集一律以「显式 VR 小端」编码，仅 PixelData
    //   按文件声明的传输语法压缩。dicom-rs 注册表未含 HTJ2K 等压缩语法的编码器，故数据集序列化
    //   只能使用 TS_EXPLICIT（其字节与标准一致）；文件真正声明的压缩方式由 obj.meta 的
    //   transfer_syntax（= 上方 ts_uid，已写入元信息）决定。
    let dataset_ts_uid = if ts_arg == "implicit" {
        TS_IMPLICIT
    } else {
        TS_EXPLICIT
    };
    let dataset_ts = TransferSyntaxRegistry
        .get(dataset_ts_uid)
        .ok_or_else(|| format!("未知数据集编码传输语法: {}", dataset_ts_uid))?;

    let file = File::create(path).map_err(|e| format!("创建文件失败: {}", e))?;
    let mut to = BufWriter::new(file);
    to.write_all(&[0u8; 128])
        .map_err(|e| format!("写入前导区失败: {}", e))?;
    to.write_all(b"DICM")
        .map_err(|e| format!("写入魔数失败: {}", e))?;
    obj.write_meta(&mut to)
        .map_err(|e| format!("写入 DICOM 失败: {}", e))?;
    obj.write_dataset_with_ts(&mut to, dataset_ts)
        .map_err(|e| format!("写入 DICOM 失败: {}", e))
}

// ---- 命令入口 ----

#[tauri::command]
fn export_dicom(args: ExportDicomArgs) -> Result<String, String> {
    let ts_arg = args.transfer_syntax.as_str();
    let ts_uid = match ts_arg {
        "implicit" => TS_IMPLICIT,
        "explicit" => TS_EXPLICIT,
        "rle" => TS_RLE,
        "htj2k_lossless" => TS_HTJ2K_LOSSLESS,
        "htj2k_lossy" => TS_HTJ2K_LOSSY,
        "jpegls_lossless" => TS_JPEGLS_LOSSLESS,
        "jpegls_loss" => TS_JPEGLS_LOSS,
        _ => return Err(format!("不支持的传输语法: {}", args.transfer_syntax)),
    };
    let compressed = matches!(
        ts_arg,
        "rle" | "htj2k_lossless" | "htj2k_lossy" | "jpegls_lossless" | "jpegls_loss"
    );
    // JPEG-LS 近无损 NEAR（误差带 ±near），仅 jpegls_loss 使用；由前端有损程度映射
    let near = if ts_arg == "jpegls_loss" {
        args.quality.clamp(1, 255)
    } else {
        0
    };

    // 收集源
    let mut sources: Vec<(FileDicomObject<InMemDicomObject>, Vec<Vec<u8>>, PixelInfo)> =
        Vec::new();
    if args.mode == "all" && !args.series_paths.is_empty() {
        for p in &args.series_paths {
            sources.push(load_source_for_export(p)?);
        }
    } else {
        sources.push(load_source_for_export(&args.file_path)?);
    }
    if sources.is_empty() {
        return Err("没有可导出的帧".into());
    }

    // 窗口参数（JPEG 用）：取第一个源
    let (sop0, inter0) = {
        let o = &sources[0].0;
        (
            read_f64_attr(o, "RescaleSlope", 1.0),
            read_f64_attr(o, "RescaleIntercept", 0.0),
        )
    };
    let phot0 = sources[0]
        .0
        .element_by_name("PhotometricInterpretation")
        .ok()
        .and_then(|e| e.to_str().ok())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("MONOCHROME2"));
    let invert = phot0 == "MONOCHROME1";

    let mut written = 0usize;

    if args.mode == "current" {
        let (obj, frames, info) = &sources[0];
        let idx = (args.frame_index as usize).min(frames.len().saturating_sub(1));
        let single = vec![frames[idx].clone()];
        let mut o = obj.clone();
        write_one_dicom(
            &mut o,
            &single,
            info,
            ts_arg,
            ts_uid,
            compressed,
            sop0,
            inter0,
            args.wc,
            args.ww,
            invert,
            args.quality,
            near,
            false,
            &args.anon_ranges,
            &args.password,
            &args.restore_password,
            args.force_layered,
            &args.output,
        )?;
        written += 1;
    } else if args.multifile {
        // 每帧单文件
        std::fs::create_dir_all(&args.output).map_err(|e| format!("创建目录失败: {}", e))?;
        let frame_total: usize = sources.iter().map(|s| s.1.len()).sum();
        let mut fi = 0usize;
        for (obj, frames, info) in &sources {
            for f in frames {
                // 逐帧检查取消：用户可在任意帧之间中止（解决「取消无效」问题）
                if batch_cancel_requested() {
                    return Err("已取消（用户中止）".into());
                }
                let mut o = obj.clone();
                let single = vec![f.clone()];
                let path = Path::new(&args.output).join(format!("frame_{:03}.dcm", fi + 1));
                write_one_dicom(
                    &mut o,
                    &single,
                    info,
                    ts_arg,
                    ts_uid,
                    compressed,
                    sop0,
                    inter0,
                args.wc,
                args.ww,
                invert,
                args.quality,
                near,
                false,
                &args.anon_ranges,
                &args.password,
                &args.restore_password,
                args.force_layered,
                &path.to_string_lossy(),
            )?;
            written += 1;
            fi += 1;
            // 逐帧推送进度（解决「进度条无显示」问题）
            report_frame_progress(fi, frame_total);
            }
        }
    } else {
        // 整个序列合并为单文件多帧
        let (base_obj, _, base_info) = &sources[0];
        let mut o = base_obj.clone();
        let mut all_frames: Vec<Vec<u8>> = Vec::new();
        let info = base_info;
        for (_, frames, _) in &sources {
            for f in frames {
                all_frames.push(f.clone());
            }
        }
        write_one_dicom(
            &mut o,
            &all_frames,
            &info,
            ts_arg,
            ts_uid,
            compressed,
            sop0,
            inter0,
            args.wc,
            args.ww,
            invert,
            args.quality,
            near,
            true,
            &args.anon_ranges,
            &args.password,
            &args.restore_password,
            args.force_layered,
            &args.output,
        )?;
        written += 1;
    }

    Ok(format!("已导出 {} 个 DICOM 文件", written))
}

// ---------- 导出 NIfTI ----------
// 把当前查看的序列（DICOM 多帧/多切片，或已加载 NIfTI 体）导出为标准 NIfTI-1 文件。
// 复用 load_source_frames 收集多帧 HU（已按解剖位置重排 inferior→superior），重组为 3D 体 [x][y][z]。
// 由 DICOM 的 ImageOrientationPatient + ImagePositionPatient 构造 RAS 仿射 sform（LPS→RAS 对 x/y 取负）。
// nifti 写端强制 scl_slope/intercept=1，故整数类型直接存 HU 值（scl=1，无损当 HU 在类型范围内）。

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportNiftiArgs {
    mode: String, // "current" | "all"
    file_path: String,
    series_paths: Vec<String>,
    frame_index: u32,
    datatype: String, // int16 | float32 | uint16 | uint8 | int32 | float64
    write_sform: bool,
    gz: bool,
    output: String, // 完整输出文件路径（含 .nii / .nii.gz）
}

fn dicom_pixel_spacing(obj: &Option<FileDicomObject<InMemDicomObject>>) -> [f32; 2] {
    let mut s = [1.0f32, 1.0f32];
    if let Some(o) = obj {
        if let Some(v) = elem_vec_f64(o, "PixelSpacing") {
            if v.len() >= 2 {
                s[0] = v[0] as f32;
                s[1] = v[1] as f32;
            }
        }
    }
    s
}

fn dicom_image_orientation(obj: &Option<FileDicomObject<InMemDicomObject>>) -> Option<[f32; 6]> {
    let o = obj.as_ref()?;
    let v = elem_vec_f64(o, "ImageOrientationPatient")?;
    if v.len() >= 6 {
        Some([
            v[0] as f32, v[1] as f32, v[2] as f32, v[3] as f32, v[4] as f32, v[5] as f32,
        ])
    } else {
        None
    }
}

fn dicom_slice_thickness(obj: &Option<FileDicomObject<InMemDicomObject>>) -> f32 {
    if let Some(o) = obj {
        if let Some(v) = elem_vec_f64(o, "SliceThickness") {
            if !v.is_empty() && v[0] > 0.0 {
                return v[0] as f32;
            }
        }
    }
    0.0
}

fn nifti_normal(iop: [f32; 6]) -> [f32; 3] {
    let r = [iop[0], iop[1], iop[2]];
    let c = [iop[3], iop[4], iop[5]];
    [
        r[1] * c[2] - r[2] * c[1],
        r[2] * c[0] - r[0] * c[2],
        r[0] * c[1] - r[1] * c[0],
    ]
}

fn ensure_nii_ext(output: &str, gz: bool) -> String {
    let base = output.trim_end_matches(".nii.gz").trim_end_matches(".nii");
    if gz {
        format!("{}.nii.gz", base)
    } else {
        format!("{}.nii", base)
    }
}

fn nifti_lossy_note(dt: &str, hu_min: f32, hu_max: f32) -> &'static str {
    match dt {
        "uint8" => "（注意：uint8 已线性映射到 0-255，存在精度损失）",
        "int16" => {
            if hu_min < -32768.0 || hu_max > 32767.0 {
                "（注意：HU 超出 int16 范围，已截断，存在精度损失）"
            } else {
                ""
            }
        }
        "uint16" => {
            if hu_min < -1024.0 || hu_max + 1024.0 > 65535.0 {
                "（注意：HU+1024 超出 uint16 范围，已截断）"
            } else {
                ""
            }
        }
        _ => "",
    }
}

/// 核心写出：frames 按 [z][y*nx+x] 排列的多帧 HU；构造 RAS sform 并量化写出。
pub(crate) fn export_nifti_core(
    frames: &[Vec<f32>],
    nx: u32,
    ny: u32,
    spacing: [f32; 2],
    iop: Option<[f32; 6]>,
    first_pos: Option<[f32; 3]>,
    sz: f32,
    datatype: &str,
    write_sform: bool,
    gz: bool,
    output: &str,
) -> Result<String, String> {
    use nifti::{writer::WriterOptions, NiftiHeader};
    use ndarray::Array3;

    let nz = frames.len() as u32;
    if nz == 0 {
        return Err("没有可导出的帧".into());
    }
    let nxp = nx as usize;
    let nyp = ny as usize;

    // 体素 HU 范围（量化提示 / uint8 线性映射）
    let mut hu_min = f32::INFINITY;
    let mut hu_max = f32::NEG_INFINITY;
    for f in frames {
        for &v in f {
            if v < hu_min {
                hu_min = v;
            }
            if v > hu_max {
                hu_max = v;
            }
        }
    }
    if !hu_min.is_finite() {
        hu_min = 0.0;
        hu_max = 1.0;
    }

    // 构造 reference header
    let mut hdr = NiftiHeader::default();
    hdr.pixdim = [1.0, spacing[0], spacing[1], sz, 1.0, 1.0, 1.0, 1.0];
    hdr.xyzt_units = 2; // mm
    hdr.cal_max = hu_max;
    hdr.cal_min = hu_min;
    hdr.descrip = b"Unixel - Hongwei Shao".to_vec();
    if write_sform {
        if let (Some(iopv), Some(p0)) = (iop, first_pos) {
            let n = nifti_normal(iopv);
            let r = [iopv[0], iopv[1], iopv[2]];
            let c = [iopv[3], iopv[4], iopv[5]];
            // LPS→RAS：x、y 取负，z 不变；k 增大=superior（与 decode_nifti 读取约定一致）
            hdr.srow_x = [-r[0] * spacing[0], -c[0] * spacing[1], -n[0] * sz, -p0[0]];
            hdr.srow_y = [-r[1] * spacing[0], -c[1] * spacing[1], -n[1] * sz, -p0[1]];
            hdr.srow_z = [r[2] * spacing[0], c[2] * spacing[1], n[2] * sz, p0[2]];
            hdr.sform_code = 1;
            hdr.qform_code = 0;
        } else {
            hdr.sform_code = 0;
            hdr.qform_code = 0;
        }
    } else {
        hdr.sform_code = 0;
        hdr.qform_code = 0;
    }

    let path = ensure_nii_ext(output, gz);
    let note = nifti_lossy_note(datatype, hu_min, hu_max);

    // 按 datatype 量化并写出（scl 由写端强制 1.0，整数类型直接存 HU）
    match datatype {
        "float32" => {
            let mut arr = Array3::<f32>::zeros((nxp, nyp, nz as usize));
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        arr[[x, y, z]] = fz[y * nxp + x];
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        "float64" => {
            let mut arr = Array3::<f64>::zeros((nxp, nyp, nz as usize));
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        arr[[x, y, z]] = fz[y * nxp + x] as f64;
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        "int16" => {
            let mut arr = Array3::<i16>::zeros((nxp, nyp, nz as usize));
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        arr[[x, y, z]] = fz[y * nxp + x].round().clamp(-32768.0, 32767.0) as i16;
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        "int32" => {
            let mut arr = Array3::<i32>::zeros((nxp, nyp, nz as usize));
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        arr[[x, y, z]] = fz[y * nxp + x].round() as i32;
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        "uint16" => {
            let mut arr = Array3::<u16>::zeros((nxp, nyp, nz as usize));
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        arr[[x, y, z]] = (fz[y * nxp + x] + 1024.0).clamp(0.0, 65535.0) as u16;
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        "uint8" => {
            let mut arr = Array3::<u8>::zeros((nxp, nyp, nz as usize));
            let denom = (hu_max - hu_min).max(1e-6);
            for z in 0..nz as usize {
                let fz = &frames[z];
                for y in 0..nyp {
                    for x in 0..nxp {
                        let t = (fz[y * nxp + x] - hu_min) / denom;
                        arr[[x, y, z]] = (t * 255.0).clamp(0.0, 255.0) as u8;
                    }
                }
            }
            WriterOptions::new(&path)
                .reference_header(&hdr)
                .compress(gz)
                .write_nifti(&arr)
                .map_err(|e| format!("写入 NIfTI 失败: {}", e))?;
        }
        other => return Err(format!("不支持的 NIfTI 数据类型: {}", other)),
    }

    Ok(format!(
        "已导出 NIfTI（{}×{}×{}，{}）{}",
        nx, ny, nz, datatype, note
    ))
}

#[tauri::command]
fn export_nifti(args: ExportNiftiArgs) -> Result<String, String> {
    // 1. 收集多帧 HU（已按 inferior->superior 重排）
    let (all_frames, obj, w, h) = load_source_frames(&args.file_path)?;
    let frames: Vec<Vec<f32>> = if args.mode == "all" {
        let mut v = Vec::new();
        if !args.series_paths.is_empty() {
            for p in &args.series_paths {
                let (fs, _, _, _) = load_source_frames(p)?;
                if let Some(f) = fs.into_iter().next() {
                    v.push(f);
                }
            }
        } else {
            v = all_frames;
        }
        v
    } else {
        let idx = (args.frame_index as usize).min(all_frames.len().saturating_sub(1));
        vec![all_frames.into_iter().nth(idx).unwrap_or_default()]
    };

    // 2. 空间元数据
    let spacing = dicom_pixel_spacing(&obj);
    let iop = dicom_image_orientation(&obj);
    let mut positions: Vec<[f32; 3]> = Vec::new();
    if !args.series_paths.is_empty() {
        for p in &args.series_paths {
            if let Ok(o) = dicom_object::open_file(p) {
                if let Some(v) = elem_vec_f64(&o, "ImagePositionPatient") {
                    if v.len() >= 3 {
                        positions.push([v[0] as f32, v[1] as f32, v[2] as f32]);
                    }
                }
            }
        }
    } else if let Some(o) = &obj {
        if let Some(v) = elem_vec_f64(o, "ImagePositionPatient") {
            if v.len() >= 3 {
                positions.push([v[0] as f32, v[1] as f32, v[2] as f32]);
            }
        }
    }
    // 按沿法向投影排序，取首切片为原点，跨度估算切片间距
    let (first_pos, sz) = if let Some(iopv) = iop {
        let n = nifti_normal(iopv);
        if !positions.is_empty() {
            let mut idxs: Vec<usize> = (0..positions.len()).collect();
            idxs.sort_by(|&a, &b| {
                let pa = positions[a][0] * n[0] + positions[a][1] * n[1] + positions[a][2] * n[2];
                let pb = positions[b][0] * n[0] + positions[b][1] * n[1] + positions[b][2] * n[2];
                pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
            });
            let fp = positions[idxs[0]];
            let p0 = positions[idxs[0]][0] * n[0]
                + positions[idxs[0]][1] * n[1]
                + positions[idxs[0]][2] * n[2];
            let p1 = positions[idxs[positions.len() - 1]][0] * n[0]
                + positions[idxs[positions.len() - 1]][1] * n[1]
                + positions[idxs[positions.len() - 1]][2] * n[2];
            let span = (p1 - p0).abs();
            let s = if positions.len() >= 2 && span > 1e-6 {
                span / (positions.len() as f32 - 1.0)
            } else {
                let st = dicom_slice_thickness(&obj);
                if st > 0.0 {
                    st
                } else {
                    1.0
                }
            };
            (Some(fp), s)
        } else {
            (None, 1.0)
        }
    } else {
        (None, 1.0)
    };

    export_nifti_core(
        &frames,
        w,
        h,
        spacing,
        iop,
        first_pos,
        sz,
        &args.datatype,
        args.write_sform,
        args.gz,
        &args.output,
    )
}

// ============ 批量转换 ============
//
// 按文件夹级、序列级进行格式互转与重处理：
//   - DICOM → NIfTI：复用 export_nifti（mode="all"，取各源文件首帧按位置重排 inferior→superior）
//   - DICOM → DICOM：复用 export_dicom（mode="all" + multifile，逐片写出并保留几何）
//   - NIfTI → DICOM：新增路径（读 sform/qform 仿射重建 LPS 几何，构造 Secondary Capture 序列）
// 输出目录镜像输入相对结构；单序列失败不中断，收集错误并跳过继续；后端借取消令牌中止剩余序列。

static BATCH_CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();
fn batch_cancel_flag() -> &'static Arc<AtomicBool> {
    BATCH_CANCEL.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// 批量转换进度回调：由 batch_convert 在每个单元开始时设置，export_dicom 内部逐帧调用推送进度。
type BatchProgressCb = Arc<dyn Fn(usize, usize) + Send + Sync>;
static BATCH_PROGRESS_CB: OnceLock<std::sync::Mutex<Option<BatchProgressCb>>> = OnceLock::new();

fn set_batch_progress_cb(cb: Option<BatchProgressCb>) {
    let m = BATCH_PROGRESS_CB.get_or_init(|| std::sync::Mutex::new(None));
    *m.lock().unwrap() = cb;
}

/// 由 export_dicom 逐帧调用；若当前批量任务已设置进度回调，则推送一帧进度。
fn report_frame_progress(cur: usize, total: usize) {
    if let Some(m) = BATCH_PROGRESS_CB.get() {
        if let Some(cb) = m.lock().unwrap().as_ref() {
            cb(cur, total);
        }
    }
}

/// 当前批量任务是否已被用户取消（在逐帧热循环中检查以中断转换）。
fn batch_cancel_requested() -> bool {
    batch_cancel_flag().load(Ordering::SeqCst)
}

/// 批量转换生命周期守卫：无论成功/失败/取消，离开作用域即清除进度回调并复位取消标志，
/// 避免取消标志残留导致后续单次导出被误判为已取消。
struct BatchProgressGuard;
impl Drop for BatchProgressGuard {
    fn drop(&mut self) {
        set_batch_progress_cb(None);
        batch_cancel_flag().store(false, Ordering::SeqCst);
    }
}

const SC_IMAGE_STORAGE: &str = "1.2.840.10008.5.1.4.1.1.7"; // Secondary Capture Image Storage
const UNIXEL_IMPL_CLASS_UID: &str = "2.25.12638147865491203746"; // Unixel implementation class UID

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BatchOptions {
    transfer_syntax: String, // 仅 DICOM 输出：implicit|explicit|rle|htj2k_lossless|htj2k_lossy|jpegls_lossless|jpegls_loss|jpeg
    anon_ranges: Vec<AnonRangeArg>,
    password: String,
    restore_password: String, // 方案A：非空时还原本工具加密脱敏后再按本轮策略重脱敏
    force_layered: bool,      // 用户确认后对已加密占位符叠加加密
    datatype: String, // 仅 NIfTI 输出：int16|int32|uint16|uint8|float32|float64
    write_sform: bool,
    gz: bool,
    quality: u8, // JPEG 有损程度 1-100（仅 jpeg 传输语法使用）
}

impl Default for BatchOptions {
    fn default() -> Self {
        BatchOptions {
            transfer_syntax: "explicit".into(),
            anon_ranges: Vec::new(),
            password: String::new(),
            restore_password: String::new(),
            force_layered: false,
            datatype: "int16".into(),
            write_sform: true,
            gz: true,
            quality: 90,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchConvertArgs {
    input_dir: String,
    input_type: String, // "DICOM" | "NIfTI"
    output_dir: String,
    output_type: String, // "DICOM" | "NIfTI"
    options: BatchOptions,
}

#[derive(Serialize)]
struct BatchItem {
    src: String,
    out: String,
    ok: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct BatchResult {
    total: usize,
    ok: usize,
    failed: usize,
    cancelled: bool,
    items: Vec<BatchItem>,
}

#[derive(Serialize, Clone)]
struct BatchProgress {
    k: usize,
    n: usize,
    label: String,
    src: String,
    out: String,
    ok: bool,
    error: Option<String>,
}

fn fmt_ds(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let s = format!("{:.6}", v).trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() {
        "0".to_string()
    } else {
        s
    }
}

fn put_str(
    obj: &mut FileDicomObject<InMemDicomObject>,
    tag: Tag,
    vr: VR,
    val: &str,
) {
    obj.put(InMemElement::new(tag, vr, PrimitiveValue::from(val.to_string())));
}

fn put_us(obj: &mut FileDicomObject<InMemDicomObject>, tag: Tag, val: u16) {
    obj.put(InMemElement::new(tag, VR::US, PrimitiveValue::from(val)));
}

fn put_is(obj: &mut FileDicomObject<InMemDicomObject>, tag: Tag, val: i32) {
    obj.put(InMemElement::new(tag, VR::IS, PrimitiveValue::from(val.to_string())));
}

fn put_ds(obj: &mut FileDicomObject<InMemDicomObject>, tag: Tag, vals: &[f64]) {
    let s = vals
        .iter()
        .map(|v| fmt_ds(*v))
        .collect::<Vec<_>>()
        .join("\\");
    obj.put(InMemElement::new(tag, VR::DS, PrimitiveValue::from(s)));
}

fn sanitize_name(s: &str) -> String {
    s.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
        .trim()
        .to_string()
}

fn relative_parent(path: &str, root: &Path) -> PathBuf {
    match Path::new(path).strip_prefix(root) {
        Ok(rel) => rel
            .parent()
            .map(|x| x.to_path_buf())
            .unwrap_or_else(|| PathBuf::new()),
        Err(_) => PathBuf::new(),
    }
}

fn series_label(s: &SeriesBrief) -> String {
    let desc = s.series_description.clone().unwrap_or_default();
    let num = s.series_number.map(|n| n.to_string()).unwrap_or_default();
    let uid = s.series_uid.clone().unwrap_or_default();
    if !desc.is_empty() {
        format!(
            "{}_{}",
            desc,
            if num.is_empty() { uid } else { num }
        )
    } else if !num.is_empty() {
        format!("series_{}", num)
    } else {
        uid
    }
}

fn ts_uid_of(ts_arg: &str) -> Result<&'static str, String> {
    Ok(match ts_arg {
        "implicit" => TS_IMPLICIT,
        "explicit" => TS_EXPLICIT,
        "rle" => TS_RLE,
        "htj2k_lossless" => TS_HTJ2K_LOSSLESS,
        "htj2k_lossy" => TS_HTJ2K_LOSSY,
        "jpegls_lossless" => TS_JPEGLS_LOSSLESS,
        "jpegls_loss" => TS_JPEGLS_LOSS,
        _ => return Err(format!("不支持的传输语法: {}", ts_arg)),
    })
}

// 读取 NIfTI 头部仿射（RAS，行主序 4x4）与 flip_z（k 增大是否指向 inferior），
// 与 decode_nifti 的翻转约定保持一致。
fn nifti_affine_ras(path: &str) -> Result<(u32, u32, u32, [f64; 16], bool), String> {
    let obj = ReaderOptions::new()
        .read_file(path)
        .map_err(|e| format!("读取 NIfTI 失败: {}", e))?;
    let hdr = obj.header();
    let nx = hdr.dim[1] as u32;
    let ny = hdr.dim[2] as u32;
    let nz = hdr.dim[3] as u32;
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
    let aff = [
        hdr.srow_x[0] as f64,
        hdr.srow_x[1] as f64,
        hdr.srow_x[2] as f64,
        hdr.srow_x[3] as f64,
        hdr.srow_y[0] as f64,
        hdr.srow_y[1] as f64,
        hdr.srow_y[2] as f64,
        hdr.srow_y[3] as f64,
        hdr.srow_z[0] as f64,
        hdr.srow_z[1] as f64,
        hdr.srow_z[2] as f64,
        hdr.srow_z[3] as f64,
        0.0,
        0.0,
        0.0,
        1.0,
    ];
    Ok((nx, ny, nz, aff, flip_z))
}

// NIfTI 体 → DICOM 序列（每片单文件 Secondary Capture），由 sform/qform 还原 LPS 几何。
fn build_dicom_series(
    nx: u32,
    ny: u32,
    nz: u32,
    vox: &[f32],
    aff: &[f64; 16],
    flip_z: bool,
    study_uid: &str,
    series_uid: &str,
    for_uid: &str,
    hu_min: f32,
    hu_max: f32,
    out_dir: &Path,
    ts_arg: &str,
    ts_uid: &str,
    quality: u8,
    near: u8,
    anon_ranges: &[AnonRangeArg],
    password: &str,
    restore_password: &str,
    force_layered: bool,
) -> Result<Vec<String>, String> {
    // X 轴 = NIfTI i（DICOM 列），Y 轴 = NIfTI j（DICOM 行）；仿射行主序：
    // aff = [ srow_x(4) ; srow_y(4) ; srow_z(4) ; 0 0 0 1 ]
    let sx0 = aff[0];
    let sy0 = aff[4];
    let sz0 = aff[8];
    let sx1 = aff[1];
    let sy1 = aff[5];
    let sz1 = aff[9];
    let sx2 = aff[2];
    let sy2 = aff[6];
    let sz2 = aff[10];
    let ox = aff[3];
    let oy = aff[7];
    let oz = aff[11];
    let xnorm = normalize3(&[sx0, sy0, sz0]);
    let ynorm = normalize3(&[sx1, sy1, sz1]);
    let col_spacing = (sx0 * sx0 + sy0 * sy0 + sz0 * sz0).sqrt();
    let row_spacing = (sx1 * sx1 + sy1 * sy1 + sz1 * sz1).sqrt();
    // ImageOrientationPatient（LPS）：首 3 = 列方向(-X)，末 3 = 行方向(-Y)
    let iop = [
        -xnorm[0], -xnorm[1], xnorm[2], -ynorm[0], -ynorm[1], ynorm[2],
    ];
    let spacing = [row_spacing, col_spacing];
    let nxp = nx as usize;
    let nyp = ny as usize;
    let nzp = nz as usize;
    let compressed = matches!(
        ts_arg,
        "rle" | "htj2k_lossless" | "htj2k_lossy" | "jpegls_lossless" | "jpegls_loss"
    );
    let wc = (hu_min as f64 + hu_max as f64) / 2.0;
    let ww = (hu_max as f64 - hu_min as f64).max(1.0);

    let mut written: Vec<String> = Vec::with_capacity(nzp);
    for z in 0..nzp {
        // 解码体 z（superior 递增）→ 文件 k；IPP 取 (i=0,j=0,k) 的物理位置
        let k = if flip_z {
            (nz as i64 - 1 - z as i64) as i64
        } else {
            z as i64
        };
        let ipx = ox + sx2 * k as f64;
        let ipy = oy + sy2 * k as f64;
        let ipz = oz + sz2 * k as f64;
        let ipp = [-ipx, -ipy, ipz];

        // 构造单帧 i16 LE（HU，slope=1 intercept=0），按图像 (row=y, col=x) 排列
        let mut frame: Vec<u8> = Vec::with_capacity(nxp * nyp * 2);
        for y in 0..nyp {
            for x in 0..nxp {
                let idx = (x * nyp + y) * nzp + z;
                let v = vox[idx];
                let iv = v.round().clamp(-32768.0, 32767.0) as i16;
                frame.extend_from_slice(&iv.to_le_bytes());
            }
        }

        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(&gen_uid())
            .transfer_syntax(ts_uid)
            .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
            .build()
            .map_err(|e| format!("构建 DICOM 元信息失败: {}", e))?;
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        put_str(&mut obj, Tag(0x0008, 0x0005), VR::CS, "ISO_IR 100");
        put_str(&mut obj, Tag(0x0008, 0x0016), VR::UI, SC_IMAGE_STORAGE);
        put_str(&mut obj, Tag(0x0008, 0x0018), VR::UI, &gen_uid());
        put_str(&mut obj, Tag(0x0008, 0x0060), VR::CS, "OT");
        put_str(&mut obj, Tag(0x0008, 0x103E), VR::LO, "Unixel NIfTI to DICOM");
        put_str(&mut obj, Tag(0x0010, 0x0010), VR::PN, "Unixel^Batch");
        put_str(&mut obj, Tag(0x0010, 0x0020), VR::LO, "UNIXEL");
        put_str(&mut obj, Tag(0x0020, 0x000D), VR::UI, study_uid);
        put_str(&mut obj, Tag(0x0020, 0x000E), VR::UI, series_uid);
        put_us(&mut obj, Tag(0x0020, 0x0011), 1);
        put_is(&mut obj, Tag(0x0020, 0x0013), (z + 1) as i32);
        put_str(&mut obj, Tag(0x0020, 0x0052), VR::UI, for_uid);
        put_ds(&mut obj, Tag(0x0020, 0x0032), &ipp);
        put_ds(&mut obj, Tag(0x0020, 0x0037), &iop);
        put_us(&mut obj, Tag(0x0028, 0x0002), 1);
        put_str(&mut obj, Tag(0x0028, 0x0004), VR::CS, "MONOCHROME2");
        put_us(&mut obj, Tag(0x0028, 0x0010), ny as u16);
        put_us(&mut obj, Tag(0x0028, 0x0011), nx as u16);
        put_ds(&mut obj, Tag(0x0028, 0x0030), &spacing);
        put_us(&mut obj, Tag(0x0028, 0x0100), 16);
        put_us(&mut obj, Tag(0x0028, 0x0101), 16);
        put_us(&mut obj, Tag(0x0028, 0x0102), 15);
        put_us(&mut obj, Tag(0x0028, 0x0103), 1);
        put_ds(&mut obj, Tag(0x0028, 0x1050), &[wc]);
        put_ds(&mut obj, Tag(0x0028, 0x1051), &[ww]);
        put_ds(&mut obj, Tag(0x0028, 0x1052), &[0.0]);
        put_ds(&mut obj, Tag(0x0028, 0x1053), &[1.0]);
        put_ds(&mut obj, Tag(0x0018, 0x0050), &[row_spacing]);

        let info = PixelInfo {
            bits_allocated: 16,
            signed: true,
            samples: 1,
            width: nx,
            height: ny,
        };
        let p = out_dir
            .join(format!("slice_{:04}.dcm", z + 1))
            .to_string_lossy()
            .to_string();
        write_one_dicom(
            &mut obj,
            &[frame],
            &info,
            ts_arg,
            ts_uid,
            compressed,
            1.0,
            0.0,
            0.0,
            0.0,
            false,
            quality,
            near,
            false,
            anon_ranges,
            password,
            restore_password,
            force_layered,
            &p,
        )?;
        written.push(p);
    }
    Ok(written)
}

fn nifti_to_dicom_series(src: &str, out_dir: &Path, opts: &BatchOptions) -> Result<String, String> {
    let vol = decode_nifti(src)?;
    let [nx, ny, nz] = vol.meta.dims;
    if nx == 0 || ny == 0 || nz == 0 {
        return Err("NIfTI 体维度为空".into());
    }
    let vox: Vec<f32> = vol
        .voxel_bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let (_nx, _ny, _nz, aff, flip_z) = nifti_affine_ras(src)?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建输出目录失败: {}", e))?;

    let ts_arg = opts.transfer_syntax.as_str();
    let ts_uid = ts_uid_of(ts_arg)?;
    let quality = if ts_arg == "jpeg" {
        opts.quality.clamp(1, 100)
    } else {
        0
    };
    let near = if ts_arg == "jpegls_loss" { 8u8 } else { 0u8 };

    let study_uid = gen_uid();
    let series_uid = gen_uid();
    let for_uid = gen_uid();
    let written = build_dicom_series(
        nx,
        ny,
        nz,
        &vox,
        &aff,
        flip_z,
        &study_uid,
        &series_uid,
        &for_uid,
        vol.meta.hu_min,
        vol.meta.hu_max,
        out_dir,
        ts_arg,
        ts_uid,
        quality,
        near,
        &opts.anon_ranges,
        &opts.password,
        &opts.restore_password,
        opts.force_layered,
    )?;
    Ok(format!(
        "已写出 {} 个 DICOM 文件至 {}",
        written.len(),
        out_dir.display()
    ))
}

#[tauri::command]
async fn batch_convert(app: tauri::AppHandle, args: BatchConvertArgs) -> Result<BatchResult, String> {
    let input_dir = Path::new(&args.input_dir);
    let output_dir = Path::new(&args.output_dir);
    if !input_dir.is_dir() {
        return Err(format!("输入文件夹不存在: {}", args.input_dir));
    }
    if args.output_dir.trim().is_empty() {
        return Err("输出文件夹为空".into());
    }
    if args.input_type.eq_ignore_ascii_case("NIfTI")
        && args.output_type.eq_ignore_ascii_case("NIfTI")
    {
        return Err("NIfTI→NIfTI 无意义，请选择不同的输入/输出类型".into());
    }
    std::fs::create_dir_all(output_dir).map_err(|e| format!("创建输出文件夹失败: {}", e))?;

    // 收集转换单元（序列级）
    struct Unit {
        label: String,
        paths: Vec<String>,
        parent_dir: PathBuf,
        name: String,
    }
    let mut units: Vec<Unit> = Vec::new();
    if args.input_type.eq_ignore_ascii_case("DICOM") {
        let tree = scan_folder_series(args.input_dir.clone())?;
        for study in &tree.studies {
            for series in &study.series {
                if series.paths.is_empty() {
                    continue;
                }
                let rel = relative_parent(&series.paths[0], input_dir);
                let name = sanitize_name(&series_label(series));
                let parent_dir = output_dir.join(&rel);
                units.push(Unit {
                    label: format!(
                        "{} / {}",
                        study.patient_name.clone().unwrap_or_default(),
                        series.series_description.clone().unwrap_or_default()
                    ),
                    paths: series.paths.clone(),
                    parent_dir,
                    name,
                });
            }
        }
    } else {
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(input_dir, &mut files);
        for p in &files {
            let s = p.to_string_lossy().to_lowercase();
            if !(s.ends_with(".nii") || s.ends_with(".nii.gz")) {
                continue;
            }
            let rel = p
                .strip_prefix(input_dir)
                .unwrap_or(p)
                .to_string_lossy()
                .to_string();
            let no_ext = rel
                .trim_end_matches(".nii.gz")
                .trim_end_matches(".nii")
                .to_string();
            let parent_dir = output_dir.join(&no_ext); // 如 output_dir/A/B/vol
            let name = Path::new(&no_ext)
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("volume")
                .to_string();
            units.push(Unit {
                label: rel.clone(),
                paths: vec![p.to_string_lossy().to_string()],
                parent_dir,
                name,
            });
        }
    }
    if units.is_empty() {
        return Err("未找到可处理的输入文件".into());
    }

    let flag = batch_cancel_flag();
    flag.store(false, Ordering::SeqCst);
    // 生命周期守卫：函数返回（成功/失败/取消）时清除进度回调并复位取消标志
    let _guard = BatchProgressGuard;
    let total = units.len();
    let mut items: Vec<BatchItem> = Vec::with_capacity(total);
    let inp = args.input_type.clone();
    let outp = args.output_type.clone();
    let opts = args.options;
    let mut cancelled = false;

    for (idx, unit) in units.iter().enumerate() {
        if flag.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }
        let src = unit.paths[0].clone();
        // 为本单元设置逐帧进度回调（export_dicom 内部每处理一帧推送一次）
        {
            let pa = app.clone();
            let pl = unit.label.clone();
            let pi = idx + 1;
            let pt = total;
            let cb: BatchProgressCb = Arc::new(move |cur: usize, tot: usize| {
                let _ = pa.emit(
                    "batch-progress",
                    BatchProgress {
                        k: cur,
                        n: tot,
                        label: format!("{}（序列 {}/{}）", pl, pi, pt),
                        src: String::new(),
                        out: String::new(),
                        ok: false,
                        error: None,
                    },
                );
            });
            set_batch_progress_cb(Some(cb));
        }
        let res: Result<String, String> = match (
            inp.eq_ignore_ascii_case("DICOM"),
            outp.eq_ignore_ascii_case("DICOM"),
        ) {
            (true, false) => {
                // DICOM → NIfTI
                let out_file = unit
                    .parent_dir
                    .join(format!("{}.nii.gz", unit.name));
                std::fs::create_dir_all(&unit.parent_dir)
                    .map_err(|e| format!("创建输出目录失败: {}", e))?;
                let out = out_file.to_string_lossy().to_string();
                export_nifti(ExportNiftiArgs {
                    mode: "all".into(),
                    file_path: src.clone(),
                    series_paths: unit.paths.clone(),
                    frame_index: 0,
                    datatype: opts.datatype.clone(),
                    write_sform: opts.write_sform,
                    gz: opts.gz,
                    output: out.clone(),
                })
                .map(|_| out)
            }
            (true, true) => {
                // DICOM → DICOM（multifile，逐片保留几何）
                let out_dir = unit.parent_dir.join(&unit.name);
                export_dicom(ExportDicomArgs {
                    mode: "all".into(),
                    file_path: src.clone(),
                    series_paths: unit.paths.clone(),
                    frame_index: 0,
                    transfer_syntax: opts.transfer_syntax.clone(),
                    quality: 0,
                    wc: 0.0,
                    ww: 0.0,
                    anon_ranges: opts.anon_ranges.clone(),
                    password: opts.password.clone(),
                    restore_password: opts.restore_password.clone(),
                    force_layered: opts.force_layered,
                    output: out_dir.to_string_lossy().to_string(),
                    multifile: true,
                })
                .map(|_| out_dir.to_string_lossy().to_string())
            }
            (false, true) => {
                // NIfTI → DICOM（新增路径）
                let out_dir = unit.parent_dir.join(&unit.name);
                nifti_to_dicom_series(&src, &out_dir, &opts)
                    .map(|_| out_dir.to_string_lossy().to_string())
            }
            (false, false) => Err("NIfTI→NIfTI 无意义".into()),
        };
        let (ok_unit, out, error) = match res {
            Ok(out) => (true, out, None),
            Err(e) => {
                if batch_cancel_requested() {
                    // 用户在单元处理中途取消：记录当前单元并中断剩余单元
                    cancelled = true;
                    items.push(BatchItem {
                        src: src.clone(),
                        out: String::new(),
                        ok: false,
                        error: Some("已取消（用户中止）".into()),
                    });
                    break;
                } else {
                    (false, String::new(), Some(e))
                }
            }
        };
        let item = BatchItem {
            src: src.clone(),
            out: out.clone(),
            ok: ok_unit,
            error: error.clone(),
        };
        items.push(item);
        let _ = app.emit(
            "batch-progress",
            BatchProgress {
                k: idx + 1,
                n: total,
                label: unit.label.clone(),
                src: src.clone(),
                out: out.clone(),
                ok: ok_unit,
                error: error.clone(),
            },
        );
    }

    if cancelled {
        // 未处理的单元标记为已取消
        for unit in &units[items.len()..] {
            items.push(BatchItem {
                src: unit.paths[0].clone(),
                out: String::new(),
                ok: false,
                error: Some("已取消（用户中止）".into()),
            });
            let _ = app.emit(
                "batch-progress",
                BatchProgress {
                    k: items.len(),
                    n: total,
                    label: unit.label.clone(),
                    src: unit.paths[0].clone(),
                    out: String::new(),
                    ok: false,
                    error: Some("已取消（用户中止）".into()),
                },
            );
        }
    }

    let ok = items.iter().filter(|i| i.ok).count();
    let failed = items.len() - ok;
    let result = BatchResult {
        total,
        ok,
        failed,
        cancelled,
        items,
    };
    let _ = app.emit("batch-done", &result);
    Ok(result)
}

#[tauri::command]
fn batch_convert_cancel() {
    batch_cancel_flag().store(true, Ordering::SeqCst);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            load_dicom_meta,
            load_dicom_pixels,
            load_image,
            load_nifti,
            load_htj2k,
            export_jpeg,
            file_tags,
            list_folder_images,
            file_series_info,
            export_tags,
            scan_folder_series,
            load_series_files,
            export_dicom,
            export_nifti,
            batch_convert,
            batch_convert_cancel,
            decrypt_anon,
            diagnose_anon
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ============ 加密脱敏解密单元测试 ============

#[cfg(test)]
mod anon_decrypt_tests {
    use super::*;

    #[test]
    fn decrypt_anon_roundtrip() {
        // 构造含 PatientName 的 DICOM 对象
        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(&gen_uid())
            .transfer_syntax("1.2.840.10008.1.2.1")
            .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
            .build()
            .expect("构建文件元表");
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        obj.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Zhang^San"),
        ));

        // 以 encrypt 方法脱敏 patient 组
        let ranges = vec![AnonRangeArg {
            id: "patient".to_string(),
            method: "encrypt".to_string(),
        }];
        anonymize_object(&mut obj, &ranges, "secret123", false, false).expect("脱敏");

        // 写临时文件后走 decrypt_anon（密码正确）
        let path = std::env::temp_dir().join(format!("unixel_anon_test_{}.dcm", std::process::id()));
        obj.write_to_file(&path).expect("写临时文件");
        let res = decrypt_anon(path.to_str().unwrap().to_string(), "secret123".to_string())
            .expect("解密");
        let entry = res
            .iter()
            .find(|e| e.keyword == "PatientName")
            .expect("应包含 PatientName 解密结果");
        assert_eq!(entry.value, "Zhang^San");

        // 错误密码必须失败（AES-GCM 认证失败）
        let bad = decrypt_anon(path.to_str().unwrap().to_string(), "wrong".to_string());
        assert!(bad.is_err(), "错误密码应解密失败");

        // 留空密码回落到默认口令 unixel（与导出端一致）
        let mut obj2 = FileDicomObject::new_empty_with_meta(
            FileMetaTableBuilder::new()
                .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(&gen_uid())
                .transfer_syntax("1.2.840.10008.1.2.1")
                .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                .build()
                .unwrap(),
        );
        obj2.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Li^Si"),
        ));
        anonymize_object(&mut obj2, &ranges, "", false, false).unwrap();
        let path2 =
            std::env::temp_dir().join(format!("unixel_anon_test2_{}.dcm", std::process::id()));
        obj2.write_to_file(&path2).unwrap();
        let res2 = decrypt_anon(path2.to_str().unwrap().to_string(), "".to_string()).unwrap();
        assert_eq!(res2[0].value, "Li^Si");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn anon_restore_then_reanonymize() {
        // 方案A：加密脱敏 → 用原密码还原 → 按本轮策略重脱敏（可换方法/密码）。
        let build = || {
            FileDicomObject::new_empty_with_meta(
                FileMetaTableBuilder::new()
                    .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                    .media_storage_sop_instance_uid(&gen_uid())
                    .transfer_syntax("1.2.840.10008.1.2.1")
                    .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                    .build()
                    .unwrap(),
            )
        };
        let enc = vec![AnonRangeArg {
            id: "patient".to_string(),
            method: "encrypt".to_string(),
        }];
        let del = vec![AnonRangeArg {
            id: "patient".to_string(),
            method: "delete".to_string(),
        }];

        // 第一轮：encrypt（密码 oldpass）
        let mut obj = build();
        obj.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Zhang^San"),
        ));
        anonymize_object(&mut obj, &enc, "oldpass", false, false).unwrap();
        assert_eq!(
            obj.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "ANONYMIZED-ENCRYPTED"
        );
        assert!(obj.private_element(0x0099, "UNIXEL", 0x01).is_ok());

        // 方案A：用原密码还原
        let restored = restore_anon_mapping(&mut obj, "oldpass").unwrap();
        assert!(restored, "应已还原");
        assert_eq!(
            obj.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "Zhang^San"
        );
        // 旧 UNIXEL 私有标记与旧强制脱敏标记应被移除
        assert!(
            obj.private_element(0x0099, "UNIXEL", 0x01).is_err(),
            "还原后应移除 UNIXEL 私有标记"
        );
        assert!(
            obj.element(Tag(0x0012, 0x0062)).is_err(),
            "还原后应移除旧 (0012,0062)"
        );

        // 第二轮：delete（与首轮不同方法）
        anonymize_object(&mut obj, &del, "", false, false).unwrap();
        assert!(
            obj.element_by_name("PatientName").is_err(),
            "重脱敏 delete 应移除 PatientName"
        );
        assert_eq!(
            obj.element(Tag(0x0012, 0x0062)).unwrap().to_str().unwrap(),
            "YES"
        );

        // 错误原密码还原必须失败
        let mut obj2 = build();
        obj2.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Wang^Wu"),
        ));
        anonymize_object(&mut obj2, &enc, "oldpass", false, false).unwrap();
        let bad = restore_anon_mapping(&mut obj2, "wrongpass");
        assert!(bad.is_err(), "错误原密码还原应失败");

        // 无 UNIXEL 标记时 restore 应为 Ok(false)（不误改未加密文件）
        let mut obj3 = build();
        obj3.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Plain^Name"),
        ));
        let r3 = restore_anon_mapping(&mut obj3, "whatever").unwrap();
        assert!(!r3, "无标记时不应还原");
        assert_eq!(
            obj3.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "Plain^Name"
        );
    }

    #[test]
    fn anon_force_layered_reencrypts_placeholder() {
        // 叠加加密：force_layered=true 时，对已加密占位符再加密一层（用新密码），
        // 旧映射被覆盖（原密码失效），新密码可解出占位符本身。
        let build = || {
            FileDicomObject::new_empty_with_meta(
                FileMetaTableBuilder::new()
                    .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                    .media_storage_sop_instance_uid(&gen_uid())
                    .transfer_syntax("1.2.840.10008.1.2.1")
                    .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                    .build()
                    .unwrap(),
            )
        };
        let enc = vec![AnonRangeArg {
            id: "patient".to_string(),
            method: "encrypt".to_string(),
        }];

        // 首轮加密（oldpass）
        let mut obj = build();
        obj.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Zhang^San"),
        ));
        anonymize_object(&mut obj, &enc, "oldpass", false, false).unwrap();
        assert_eq!(
            obj.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "ANONYMIZED-ENCRYPTED"
        );

        // 非 layered 二次 encrypt：应被幂等守卫跳过，旧映射保留（oldpass 仍可还原）
        anonymize_object(&mut obj, &enc, "otherpass", false, false).unwrap();
        let r = restore_anon_mapping(&mut obj, "oldpass").unwrap();
        assert!(r, "非 layered 不应覆盖旧映射");
        assert_eq!(
            obj.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "Zhang^San"
        );

        // layered：用 newpass 对已加密占位符再加密一层
        let mut obj2 = build();
        obj2.put(InMemElement::new(
            Tag(0x0010, 0x0010),
            VR::PN,
            PrimitiveValue::from("Zhang^San"),
        ));
        anonymize_object(&mut obj2, &enc, "oldpass", false, false).unwrap();
        // 二次加密强制叠加
        anonymize_object(&mut obj2, &enc, "newpass", false, true).unwrap();
        assert_eq!(
            obj2.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "ANONYMIZED-ENCRYPTED"
        );
        // 旧密码应失效（旧映射被覆盖）
        assert!(
            restore_anon_mapping(&mut obj2, "oldpass").is_err(),
            "叠加加密后旧密码应失效"
        );
        // 新密码可解出占位符本身（仅一层可逆）
        let restored = restore_anon_mapping(&mut obj2, "newpass").unwrap();
        assert!(restored, "新密码应可还原");
        assert_eq!(
            obj2.element_by_name("PatientName").unwrap().to_str().unwrap(),
            "ANONYMIZED-ENCRYPTED",
            "叠加后新密码解出的应是占位符本身"
        );
    }

    #[test]
    fn anon_mapping_trailing_pad_tolerated() {
        // 回归：当 (0099,0103) 明文 JSON 长度为奇数时，写入器会补一个 0x00 填充字节，
        // read_anon_mapping 必须容忍并正确解析，否则会报 "trailing characters at line 1 column N"。
        let salt = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        // 构造一个合法 JSON，并确保其字节长度为奇数（模拟真实文件奇数长度被补齐的场景）。
        let mut json = String::from(
            r#"{"algo":"PBKDF2-HMAC-SHA256;AES-256-GCM","iterations":100000,"salt_hex":"1122334455667788","entries":[]}"#,
        );
        if json.len() % 2 == 0 {
            // 在结尾 '}' 前插入一个空格（JSON 允许 '}' 前的空白），使整体长度变奇数。
            json.insert(json.len() - 1, ' ');
        }
        let mut bytes = json.into_bytes();
        assert!(bytes.len() % 2 == 1, "测试构造的 JSON 应为奇数长度");

        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(&gen_uid())
            .transfer_syntax("1.2.840.10008.1.2.1")
            .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
            .build()
            .expect("构建文件元表");
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        obj.put_private_element(
            0x0099,
            "UNIXEL",
            0x01,
            VR::LO,
            PrimitiveValue::from("PBKDF2-HMAC-SHA256;AES-256-GCM"),
        )
        .unwrap();
        obj.put_private_element(
            0x0099,
            "UNIXEL",
            0x02,
            VR::LO,
            PrimitiveValue::from(to_hex(&salt)),
        )
        .unwrap();
        // 奇数长度字节 + 写入器会补的 0x00 尾随填充
        bytes.push(0x00);
        obj.put_private_element(0x0099, "UNIXEL", 0x03, VR::OB, PrimitiveValue::from(bytes))
            .unwrap();

        // 修复前此处会报 "trailing characters"；修复后应成功解析出 algo 与 entries。
        let (algo, _salt, mapping) = read_anon_mapping(&obj).expect("应容忍尾随 0x00 填充");
        assert_eq!(algo.trim(), "PBKDF2-HMAC-SHA256;AES-256-GCM");
        assert!(mapping.get("entries").and_then(|v| v.as_array()).is_some());
    }

    #[test]
    fn anon_markers_uid_regenerate_and_freetext() {
        // 验证：① 实际脱敏时写强制标记 (0012,0062)/(0012,0064)；
        // ② UID 重生成保持引用（SOP/FoR/Acquisition/Accession 替换，Study/Series 受 regenerate_study_series 门控）；
        // ③ 未选中的分组保持原值；④ 全 keep 不写标记；⑤ 自由文本组 delete 生效。
        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(&gen_uid())
            .transfer_syntax("1.2.840.10008.1.2.1")
            .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
            .build()
            .expect("构建文件元表");
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        obj.put(InMemElement::new(Tag(0x0010, 0x0010), VR::PN, PrimitiveValue::from("Zhang^San")));
        obj.put(InMemElement::new(Tag(0x0020, 0x000D), VR::UI, PrimitiveValue::from("1.2.3.4".to_string())));
        obj.put(InMemElement::new(Tag(0x0020, 0x000E), VR::UI, PrimitiveValue::from("1.2.3.5".to_string())));
        let sop0 = "1.2.3.6";
        obj.put(InMemElement::new(Tag(0x0008, 0x0018), VR::UI, PrimitiveValue::from(sop0.to_string())));
        obj.put(InMemElement::new(Tag(0x0020, 0x0052), VR::UI, PrimitiveValue::from("1.2.3.7".to_string())));
        obj.put(InMemElement::new(Tag(0x0008, 0x0017), VR::UI, PrimitiveValue::from("1.2.3.8".to_string())));
        obj.put(InMemElement::new(Tag(0x0008, 0x0050), VR::SH, PrimitiveValue::from("ACC123".to_string())));
        obj.put(InMemElement::new(Tag(0x0008, 0x1030), VR::LO, PrimitiveValue::from("Chest CT".to_string())));

        // 仅 UID 重生成（regenerate_study_series=false）
        let ranges = vec![AnonRangeArg {
            id: "uid".to_string(),
            method: "regenerate".to_string(),
        }];
        anonymize_object(&mut obj, &ranges, "unixel", false, false).unwrap();

        assert_eq!(
            obj.element_by_name("PatientIdentityRemoved").unwrap().to_str().unwrap(),
            "YES"
        );
        assert!(obj.element_by_name("DeidentificationMethodCodeSequence").is_ok());
        // Study/Series UID 不变（regenerate_study_series=false）
        assert_eq!(obj.element_by_name("StudyInstanceUID").unwrap().to_str().unwrap(), "1.2.3.4");
        assert_eq!(obj.element_by_name("SeriesInstanceUID").unwrap().to_str().unwrap(), "1.2.3.5");
        // SOP/FoR/Acquisition/Accession 已重生成（值改变）
        assert_ne!(obj.element_by_name("SOPInstanceUID").unwrap().to_str().unwrap(), sop0);
        assert_ne!(obj.element_by_name("FrameOfReferenceUID").unwrap().to_str().unwrap(), "1.2.3.7");
        assert_ne!(obj.element_by_name("AcquisitionUID").unwrap().to_str().unwrap(), "1.2.3.8");
        assert_ne!(obj.element_by_name("AccessionNumber").unwrap().to_str().unwrap(), "ACC123");
        // 未被选中的分组保持原值
        assert_eq!(obj.element_by_name("PatientName").unwrap().to_str().unwrap(), "Zhang^San");
        assert_eq!(obj.element_by_name("StudyDescription").unwrap().to_str().unwrap(), "Chest CT");

        // 无脱敏（全 keep）时不写标记
        let mut obj2 = FileDicomObject::new_empty_with_meta(
            FileMetaTableBuilder::new()
                .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(&gen_uid())
                .transfer_syntax("1.2.840.10008.1.2.1")
                .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                .build()
                .unwrap(),
        );
        obj2.put(InMemElement::new(Tag(0x0010, 0x0010), VR::PN, PrimitiveValue::from("A^B")));
        anonymize_object(&mut obj2, &[], "unixel", false, false).unwrap();
        assert!(
            obj2.element_by_name("PatientIdentityRemoved").is_err(),
            "全 keep 不应写强制标记"
        );

        // 自由文本组 delete 应移除 StudyDescription 并写标记
        let mut obj3 = FileDicomObject::new_empty_with_meta(
            FileMetaTableBuilder::new()
                .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                .media_storage_sop_instance_uid(&gen_uid())
                .transfer_syntax("1.2.840.10008.1.2.1")
                .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                .build()
                .unwrap(),
        );
        obj3.put(InMemElement::new(Tag(0x0008, 0x1030), VR::LO, PrimitiveValue::from("Chest CT".to_string())));
        let ranges3 = vec![AnonRangeArg {
            id: "freetext".to_string(),
            method: "delete".to_string(),
        }];
        anonymize_object(&mut obj3, &ranges3, "unixel", false, false).unwrap();
        assert!(
            obj3.element_by_name("StudyDescription").is_err(),
            "自由文本 delete 应移除 StudyDescription"
        );
        assert_eq!(
            obj3.element_by_name("PatientIdentityRemoved").unwrap().to_str().unwrap(),
            "YES"
        );
    }

    #[test]
    fn anon_markers_reflect_actual_methods() {
        // 验证 (0012,0064) 按实际采用的方法写入对应代码项，(0012,0063) 文本汇总实际方法。
        let meta = FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
            .media_storage_sop_instance_uid(&gen_uid())
            .transfer_syntax("1.2.840.10008.1.2.1")
            .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
            .build()
            .expect("构建文件元表");
        let mut obj = FileDicomObject::new_empty_with_meta(meta);
        obj.put(InMemElement::new(Tag(0x0010, 0x0010), VR::PN, PrimitiveValue::from("Zhang^San")));
        obj.put(InMemElement::new(Tag(0x0020, 0x000D), VR::UI, PrimitiveValue::from("1.2.3.4".to_string())));
        obj.put(InMemElement::new(Tag(0x0008, 0x0018), VR::UI, PrimitiveValue::from("1.2.3.6".to_string())));
        obj.put(InMemElement::new(Tag(0x0008, 0x1030), VR::LO, PrimitiveValue::from("Chest CT".to_string())));

        let ranges = vec![
            AnonRangeArg { id: "uid".to_string(), method: "regenerate".to_string() },
            AnonRangeArg { id: "freetext".to_string(), method: "delete".to_string() },
            AnonRangeArg { id: "patient".to_string(), method: "encrypt".to_string() },
        ];
        anonymize_object(&mut obj, &ranges, "unixel", false, false).unwrap();

        // (0012,0064) 代码序列：应含 UID / DEL / ENC 三项，不含未使用的 HASH
        let seq_el = obj
            .element_by_name("DeidentificationMethodCodeSequence")
            .expect("应写 (0012,0064) 代码序列");
        let items = seq_el.value().items().expect("代码序列应可读");
        let mut codes: Vec<String> = Vec::new();
        for it in items {
            if let Ok(el) = it.element(Tag(0x0008, 0x0100)) {
                if let Ok(cv) = el.to_str() {
                    codes.push(cv.to_string());
                }
            }
        }
        assert!(codes.iter().any(|c| c == "UNIXEL-ANON-UID"), "应含 UID 代码项: {:?}", codes);
        assert!(codes.iter().any(|c| c == "UNIXEL-ANON-DEL"), "应含删除代码项: {:?}", codes);
        assert!(codes.iter().any(|c| c == "UNIXEL-ANON-ENC"), "应含加密代码项: {:?}", codes);
        assert!(!codes.iter().any(|c| c == "UNIXEL-ANON-HASH"), "未用哈希不应含 HASH 代码项: {:?}", codes);

        // (0012,0063) 文本汇总实际方法
        let txt = obj
            .element_by_name("DeidentificationMethod")
            .expect("应写 (0012,0063) 文本")
            .to_str()
            .unwrap()
            .to_string();
        assert!(txt.contains("UID regeneration"), "(0012,0063) 应含 UID regeneration: {}", txt);
        assert!(txt.contains("tag deletion"), "(0012,0063) 应含 tag deletion: {}", txt);
        assert!(txt.contains("AES-256-GCM encryption"), "(0012,0063) 应含加密说明: {}", txt);
    }

    #[test]
    fn anon_device_group_handled() {
        // 验证「设备信息」组：(0008,0070)Manufacturer、(0008,1090)ManufacturerModelName、
        // (0018,1000)DeviceSerialNumber（不含 StationName，其归属机构组）。
        // ① device=delete 时三项被移除并写强制标记，StationName 因属机构组而保留；
        // ② device=keep 时保留；③ StationName 现属 institution 组，institution=delete 时应移除；
        // ④ 排除 (0018,1020)SoftwareVersions（后台固定写签名）。
        let build = || {
            FileDicomObject::new_empty_with_meta(
                FileMetaTableBuilder::new()
                    .media_storage_sop_class_uid(SC_IMAGE_STORAGE)
                    .media_storage_sop_instance_uid(&gen_uid())
                    .transfer_syntax("1.2.840.10008.1.2.1")
                    .implementation_class_uid(UNIXEL_IMPL_CLASS_UID)
                    .build()
                    .unwrap(),
            )
        };
        let put_dev = |obj: &mut FileDicomObject<InMemDicomObject>| {
            obj.put(InMemElement::new(Tag(0x0008, 0x0070), VR::LO, PrimitiveValue::from("SIEMENS".to_string())));
            obj.put(InMemElement::new(Tag(0x0008, 0x1090), VR::LO, PrimitiveValue::from("SOMATOM Force".to_string())));
            obj.put(InMemElement::new(Tag(0x0018, 0x1000), VR::LO, PrimitiveValue::from("SN-12345".to_string())));
            obj.put(InMemElement::new(Tag(0x0008, 0x1010), VR::SH, PrimitiveValue::from("CT-ROOM-1".to_string())));
            obj.put(InMemElement::new(Tag(0x0018, 0x1020), VR::LO, PrimitiveValue::from("Syngo VB20".to_string())));
        };

        // ① delete：三项移除 + 写标记；StationName 属机构组，device=delete 时保留
        let mut obj = build();
        put_dev(&mut obj);
        let ranges = vec![AnonRangeArg { id: "device".to_string(), method: "delete".to_string() }];
        anonymize_object(&mut obj, &ranges, "unixel", false, false).unwrap();
        assert!(obj.element_by_name("Manufacturer").is_err(), "device=delete 应移除 Manufacturer");
        assert!(obj.element_by_name("ManufacturerModelName").is_err(), "device=delete 应移除 ManufacturerModelName");
        assert!(obj.element_by_name("DeviceSerialNumber").is_err(), "device=delete 应移除 DeviceSerialNumber");
        assert_eq!(
            obj.element_by_name("StationName").unwrap().to_str().unwrap(),
            "CT-ROOM-1",
            "StationName 现属机构组，device=delete 不应移除"
        );
        assert_eq!(
            obj.element_by_name("PatientIdentityRemoved").unwrap().to_str().unwrap(),
            "YES"
        );
        // SoftwareVersions 不在设备组，应保留
        assert_eq!(
            obj.element_by_name("SoftwareVersions").unwrap().to_str().unwrap(),
            "Syngo VB20"
        );

        // ② keep：三项保留 + 不写标记
        let mut obj2 = build();
        put_dev(&mut obj2);
        anonymize_object(&mut obj2, &[], "unixel", false, false).unwrap();
        assert_eq!(obj2.element_by_name("Manufacturer").unwrap().to_str().unwrap(), "SIEMENS");
        assert_eq!(obj2.element_by_name("ManufacturerModelName").unwrap().to_str().unwrap(), "SOMATOM Force");
        assert_eq!(obj2.element_by_name("DeviceSerialNumber").unwrap().to_str().unwrap(), "SN-12345");
        assert_eq!(obj2.element_by_name("StationName").unwrap().to_str().unwrap(), "CT-ROOM-1");
        assert!(
            obj2.element_by_name("PatientIdentityRemoved").is_err(),
            "全 keep 不应写强制标记"
        );

        // ③ StationName 现属 institution 组：institution=delete 时应被移除
        let mut obj3 = build();
        put_dev(&mut obj3);
        let ranges3 = vec![AnonRangeArg { id: "institution".to_string(), method: "delete".to_string() }];
        anonymize_object(&mut obj3, &ranges3, "unixel", false, false).unwrap();
        assert!(
            obj3.element_by_name("StationName").is_err(),
            "StationName 现属机构组，机构组 delete 应移除它"
        );
    }
}

// ============ 导出 DICOM 单元测试 ============

#[cfg(test)]
mod export_dicom_tests {
    use super::*;

    #[test]
    fn export_nifti_args_deserialize_camel() {
        // 验证前端 camelCase 字段（filePath/seriesPaths/frameIndex/writeSform）
        // 能正确反序列化为 ExportNiftiArgs（回归：曾缺 rename_all 导致 missing field `file_path`）
        let json = r#"{
            "mode": "all",
            "filePath": "/tmp/ct.dcm",
            "seriesPaths": ["/tmp/ct_1.dcm", "/tmp/ct_2.dcm"],
            "frameIndex": 3,
            "datatype": "int16",
            "writeSform": true,
            "gz": true,
            "output": "/tmp/out.nii.gz"
        }"#;
        let args: ExportNiftiArgs = serde_json::from_str(json).expect("camelCase 反序列化失败");
        assert_eq!(args.mode, "all");
        assert_eq!(args.file_path, "/tmp/ct.dcm");
        assert_eq!(args.series_paths, vec!["/tmp/ct_1.dcm", "/tmp/ct_2.dcm"]);
        assert_eq!(args.frame_index, 3);
        assert_eq!(args.datatype, "int16");
        assert!(args.write_sform);
        assert!(args.gz);
        assert_eq!(args.output, "/tmp/out.nii.gz");
    }

    #[test]
    fn export_nifti_roundtrip() {
        use ndarray::Array3;
        use nifti::{writer::WriterOptions, NiftiHeader};
        let (nx, ny, nz) = (16u32, 12u32, 8u32);
        let nxp = nx as usize;
        let nyp = ny as usize;
        // 构造已知体素（含负值，模拟 HU），写出为临时 NIfTI 源
        let src = "tests/nii_src_roundtrip.nii.gz";
        {
            let mut data = Array3::<f32>::zeros((nxp, nyp, nz as usize));
            for x in 0..nxp {
                for y in 0..nyp {
                    for z in 0..nz as usize {
                        data[[x, y, z]] = ((x * 3 + y * 5 + z * 7) % 2000) as f32 - 500.0;
                    }
                }
            }
            WriterOptions::new(src)
                .write_nifti(&data)
                .expect("write src nii");
        }
        // 读回为 frames（[z][y*nx+x]）
        let vol = decode_nifti(src).expect("decode src");
        let vox: Vec<f32> = vol
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut frames: Vec<Vec<f32>> = Vec::with_capacity(nz as usize);
        for z in 0..nz as usize {
            let mut f = vec![0f32; nxp * nyp];
            for y in 0..nyp {
                for x in 0..nxp {
                    f[y * nxp + x] = vox[((x * nyp + y) * nz as usize) + z];
                }
            }
            frames.push(f);
        }
        // float32 无损往返
        let out = "tests/nii_out_roundtrip.nii.gz";
        let res = export_nifti_core(
            &frames, nx, ny, [1.0, 1.0], None, None, 1.0, "float32", true, true, out,
        );
        assert!(res.is_ok(), "export_nifti_core 失败: {:?}", res.err());
        let out_vol = decode_nifti(out).expect("decode out");
        assert_eq!(out_vol.meta.dims, [nx, ny, nz]);
        let out_vox: Vec<f32> = out_vol
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for x in 0..nxp {
            for y in 0..nyp {
                for z in 0..nz as usize {
                    let i = ((x * nyp + y) * nz as usize) + z;
                    let expected = ((x * 3 + y * 5 + z * 7) % 2000) as f32 - 500.0;
                    assert!(
                        (expected - out_vox[i]).abs() < 1e-2,
                        "float32 体素 ({},{},{}) 不一致: {} vs {}",
                        x,
                        y,
                        z,
                        expected,
                        out_vox[i]
                    );
                }
            }
        }
        // int16 无损（HU 在 int16 范围内，scl=1 直接存整数）
        let out2 = "tests/nii_out_int16.nii.gz";
        let res2 = export_nifti_core(
            &frames, nx, ny, [1.0, 1.0], None, None, 1.0, "int16", true, true, out2,
        );
        assert!(res2.is_ok(), "export_nifti_core int16 失败: {:?}", res2.err());
        let vol2 = decode_nifti(out2).expect("decode int16 out");
        let v2: Vec<f32> = vol2
            .voxel_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for x in 0..nxp {
            for y in 0..nyp {
                for z in 0..nz as usize {
                    let i = ((x * nyp + y) * nz as usize) + z;
                    let expected = ((x * 3 + y * 5 + z * 7) % 2000) as f32 - 500.0;
                    assert!(
                        (expected.round() - v2[i]).abs() < 1e-2,
                        "int16 体素 ({},{},{}) 不一致",
                        x,
                        y,
                        z
                    );
                }
            }
        }
    }

    #[test]
    fn md5_known_vector() {
        assert_eq!(
            md5_hex(b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn aes_gcm_roundtrip() {
        let key = derive_key("secret", b"0123456789abcdef");
        let pt = b"Patient^Zhang";
        let ct = aes_gcm_encrypt(&key, pt).expect("encrypt");
        assert_ne!(ct.len(), pt.len()); // 含 12 字节 nonce
        let dec = aes_gcm_decrypt(&key, &ct).expect("decrypt");
        assert_eq!(dec, pt);
    }

    #[test]
    fn uid_format() {
        let u = gen_uid();
        assert!(u.starts_with("2.25."));
        assert!(u.len() < 64);
    }

    // DICOM RLE 解码（与编码器配对，验证往返一致）
    fn rle_decode(data: &[u8], expected_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(expected_len);
        let mut i = 0usize;
        while i < data.len() && out.len() < expected_len {
            let h = data[i];
            i += 1;
            if h < 128 {
                let cnt = (h + 1) as usize;
                out.extend_from_slice(&data[i..i + cnt]);
                i += cnt;
            } else {
                let cnt = (257 - h as usize) as usize;
                let b = data[i];
                i += 1;
                for _ in 0..cnt {
                    out.push(b);
                }
            }
        }
        out
    }

    #[test]
    fn rle_roundtrip() {
        // 构造测试帧：含长重复段与随机段
        let mut frame = vec![0u8; 100];
        for (k, b) in frame.iter_mut().enumerate() {
            *b = if k < 40 { 7u8 } else { (k % 251) as u8 };
        }
        let enc = rle_encode_frame(&frame, 100);
        assert!(!enc.contains(&128)); // 头字节不应出现保留值 128
        let dec = rle_decode(&enc, frame.len());
        assert_eq!(dec, frame);
    }

    #[test]
    fn htj2k_8bit_roundtrip() {
        // 8-bit 灰度帧，验证 HTJ2K 编码路径（经现有 htj2k_decode 解码回 8bit）
        let w = 32u32;
        let h = 24u32;
        let mut frame = vec![0u8; (w * h) as usize];
        for (k, b) in frame.iter_mut().enumerate() {
            *b = ((k * 7) % 256) as u8;
        }
        let enc = htj2k_encode_frame(&frame, w, h, 8, false, true, 90).expect("htj2k encode");
        assert!(!enc.is_empty());
        let (dw, dh, gray) = htj2k_decode(&enc).expect("htj2k decode");
        assert_eq!(dw, w);
        assert_eq!(dh, h);
        assert_eq!(gray, frame);
    }

    #[test]
    fn htj2k_transfer_syntax_registered() {
        // 根因验证：dicom-rs 0.7 内置注册表不含任何 HTJ2K UID，若不通过 submit_ele_transfer_syntax!
        // 注册，open_file 解析数据集会因未知 TS 报「传输语法错误」。本测试确认运行时注册已生效。
        use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
        assert!(
            TransferSyntaxRegistry.get("1.2.840.10008.1.2.4.201").is_some(),
            "官方 HTJ2K 无损 UID 必须已注册（否则本应用无法回读自己导出的文件）"
        );
        assert!(
            TransferSyntaxRegistry.get("1.2.840.10008.1.2.4.202").is_some(),
            "官方 HTJ2K 有损 UID 必须已注册"
        );
        assert!(
            TransferSyntaxRegistry.get("1.2.840.10008.1.2.4.200").is_some(),
            "旧自定义 .200 也应已注册，以便回读旧版导出的文件"
        );
    }

    #[test]
    fn build_pixel_native_concat() {
        // 未压缩：两帧拼接后应得到原始字节拼接
        let info = PixelInfo {
            bits_allocated: 16,
            signed: false,
            samples: 1,
            width: 2,
            height: 2,
        };
        let f1 = vec![1u8, 0, 2, 0, 3, 0, 4, 0];
        let f2 = vec![5u8, 0, 6, 0, 7, 0, 8, 0];
        let payload = build_pixel_payload("explicit", &[f1.clone(), f2.clone()], &info, 90, 0).unwrap();
        assert_eq!(payload.vr(), VR::OW);
        match payload.value() {
            Value::Primitive(pv) => {
                let b = pv.to_bytes();
                let mut expect = f1;
                expect.extend_from_slice(&f2);
                assert_eq!(b.as_ref(), expect.as_slice());
            }
            _ => panic!("未压缩像素应为 Primitive"),
        }
    }

    #[test]
    fn jpegls_lossless_roundtrip_8bit() {
        use jpegls::{decode, encode_with_options, EncodeOptions, Profile};
        // 构造 8-bit 灰度测试图（含 0、255 与中间值，触发各类残差/游程）
        let w = 8u32;
        let h = 6u32;
        let mut samples: Vec<u16> = vec![0u16; (w * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                samples[y * w as usize + x] =
                    (((y * w as usize + x) * 37 + 11) % 256) as u16;
            }
        }
        let mut opts = EncodeOptions::default();
        opts.near = 0;
        opts.profile = Profile::T87;
        opts.precision = Some(8);
        let mut buf = Vec::new();
        encode_with_options(&samples, w, h, &opts, &mut buf).unwrap();
        assert_eq!(&buf[0..2], &[0xFF, 0xD8]);
        // jpegls 解码自身输出应逐像素无损还原
        let (dec, dw, dh) = decode(&buf, w, h).expect("JPEG-LS 解码失败");
        assert_eq!(dw, w);
        assert_eq!(dh, h);
        assert_eq!(dec, samples, "8-bit JPEG-LS 无损往返不一致");
    }

    #[test]
    fn jpegls_lossless_roundtrip_16bit_signed() {
        use jpegls::decode;
        // 构造 16-bit 有符号（int16）测试像素，按位 reinterpret 为 u16 后编码
        let w = 8u32;
        let h = 6u32;
        let raw: Vec<i16> = (0..(w * h) as i32)
            .map(|i| ((i * 131 - 4000) % 32000) as i16)
            .collect();
        let mut frame: Vec<u8> = Vec::with_capacity(raw.len() * 2);
        for v in &raw {
            frame.extend_from_slice(&v.to_le_bytes());
        }
        let info = PixelInfo {
            bits_allocated: 16,
            signed: true,
            samples: 1,
            width: w,
            height: h,
        };
        let bytes = jpegls_encode_frame(&frame, &info, 0).unwrap();
        assert_eq!(&bytes[0..2], &[0xFF, 0xD8]);
        // 将 raw 按位 reinterpret 为 u16 序列，与解码结果对比
        let samples: Vec<u16> = raw.iter().map(|&v| v as u16).collect();
        let (dec, dw, dh) = decode(&bytes, w, h).expect("JPEG-LS 16-bit 解码失败");
        assert_eq!(dw, w);
        assert_eq!(dh, h);
        assert_eq!(dec, samples, "16-bit 有符号 JPEG-LS 无损往返不一致");
    }

    #[test]
    fn jpegls_near_lossless_16bit() {
        use jpegls::{decode, encode_with_options, EncodeOptions, Profile};
        // 近无损：near=3，重建误差应 ≤ 3；同时验证压缩确实发生（体积小于原始）
        let w = 32u32;
        let h = 24u32;
        let samples: Vec<u16> = (0..(w * h) as u32)
            .map(|i| (i * 911 % 4096) as u16)
            .collect();
        let mut opts = EncodeOptions::default();
        opts.near = 3;
        opts.profile = Profile::T87;
        opts.precision = Some(12);
        let mut buf = Vec::new();
        encode_with_options(&samples, w, h, &opts, &mut buf).unwrap();
        let (dec, _, _) = decode(&buf, w, h).expect("JPEG-LS 近无损解码失败");
        assert_eq!(dec.len(), samples.len());
        for (a, b) in samples.iter().zip(dec.iter()) {
            let diff = (*a as i32 - *b as i32).abs();
            assert!(diff <= 3, "近无损误差 {} 超出 NEAR=3", diff);
        }
    }
}
