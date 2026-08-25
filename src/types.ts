// 与后端约定的数据结构（后续由 Tauri command 返回）
export interface DicomMeta {
  path: string;
  filename: string;
  width: number;
  height: number;
  frames: number;
  bitsStored: number;
  pixelRepresentation: number; // 0=unsigned, 1=signed
  slope: number;
  intercept: number;
  windowCenter: number;
  windowWidth: number;
  photometric: string; // MONOCHROME1/2, RGB 等
  huMin: number; // 像素 HU 值范围，用于滑块边界
  huMax: number;
}

// 后端 load_dicom 返回的整体结构：元数据 + 连续 f32 像素字节
export interface DicomImage {
  meta: DicomMeta;
  // 经 Tauri v2 JSON IPC：serde_bytes 的 Vec<u8> 会退化为 number[]（需转回 Uint8Array）；
  // 也可能是 base64 字符串或真正的 Uint8Array，decodePixelBytes 统一兼容。
  pixelBytes: Uint8Array | string | number[];
}

// NIfTI 体数据（B2）：3D 体（f32 LE，顺序 [x][y][z]）与维度
export interface NiftiMeta {
  path: string;
  filename: string;
  dims: [number, number, number]; // [nx, ny, nz]
  huMin: number;
  huMax: number;
}
export interface NiftiVolume {
  meta: NiftiMeta;
  // 同 pixelBytes：Tauri v2 JSON 下为 number[]，亦兼容 Uint8Array / base64
  voxelBytes: Uint8Array | string | number[];
}

// 从文件夹导入：顶层影像文件概要（不含像素，前端懒加载）
export interface ImageInfo {
  path: string;
  filename: string;
  width: number;
  height: number;
  frames: number;
  kind: string; // "dicom" | "image" | "htj2k"
  // 系列与位置信息（后端已按系列分组、位置排序；非 DICOM 为 null）
  seriesUid?: string | null;
  seriesNumber?: number | null;
  modality?: string | null;
  instanceNumber?: number | null;
  sliceLocation?: number | null;
  imagePosPatient?: number[] | null;
  imageOrientation?: number[] | null;
  seriesGroup?: number | null; // 同系列相同；null 表示不属于任何系列
  seriesLabel?: string | null; // 作为状态栏下拉分组标题
}

// 单文件打开时后端返回的系列与位置字段（file_series_info）
export interface SeriesFields {
  seriesUid?: string | null;
  seriesNumber?: number | null;
  modality?: string | null;
  instanceNumber?: number | null;
  sliceLocation?: number | null;
  imagePosPatient?: number[] | null;
  imageOrientation?: number[] | null;
}

// 详情对话框：文件标签信息
export interface TagRow {
  tag: string;
  vr: string;
  keyword: string;
  value: string;
  description: string; // DICOM: 标准字典人类可读名称；其它类型为空
}
export interface FileTags {
  kind: string; // "dicom" | "nifti" | "image"
  filename: string;
  rows: TagRow[];
}

// 文件夹导入：序列选择对话框的数据结构（scan_folder_series 返回）
export interface SeriesBrief {
  studyUid?: string | null;
  seriesUid?: string | null;
  modality?: string | null;
  seriesNumber?: number | null;
  seriesDescription?: string | null;
  patientName?: string | null;
  patientId?: string | null;
  seriesDate?: string | null;
  studyDate?: string | null;
  fileCount: number;
  paths: string[];
}
export interface StudyBrief {
  studyUid?: string | null;
  patientName?: string | null;
  patientId?: string | null;
  studyDate?: string | null;
  series: SeriesBrief[];
}
export interface SeriesTree {
  studies: StudyBrief[];
  others?: SeriesBrief | null;
}

// 将后端返回的像素字节解码为 Float32Array。
// 像素顺序为 [frame][row][col]，长度为 width*height*frames。
// 兼容三种形态（Tauri v2 JSON IPC 下 serde_bytes 的 Vec<u8> 会退化为 number[]）：
//   - number[]  ：JSON 数组（每个元素 0..255 的整数字节）—— 必须转回 Uint8Array
//   - Uint8Array：二进制 ArrayBuffer（部分 Tauri 配置/旧版）
//   - string    ：base64 编码
export function decodePixelBytes(b: Uint8Array | string | number[]): Float32Array {
  // 防御：后端未返回像素字段（如序列化字段名未对齐导致拿到 undefined）时，
  // 给出明确错误，而不是在 bytes.slice() 上报难懂的 "reading 'slice'"。
  if (b == null) {
    throw new Error("解码失败：后端未返回像素数据（pixelBytes 缺失或字段名未对齐）");
  }
  let bytes: Uint8Array;
  if (typeof b === "string") {
    const bin = atob(b);
    bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  } else if (b instanceof Uint8Array) {
    bytes = b;
  } else if (Array.isArray(b)) {
    // Tauri v2：serde_bytes 经 JSON 协议把 Vec<u8> 退化成 number[]，需转回 Uint8Array
    bytes = new Uint8Array(b);
  } else {
    bytes = new Uint8Array(b as ArrayLike<number>);
  }
  // slice() 保证 offset=0、buffer 长度对齐，便于按 f32 解释
  return new Float32Array(bytes.slice().buffer);
}

// 批量转换（batch_convert）相关结构
export interface BatchItem {
  src: string;
  out: string;
  ok: boolean;
  error: string | null;
}
export interface BatchResult {
  total: number;
  ok: number;
  failed: number;
  cancelled: boolean;
  items: BatchItem[];
}
export interface BatchProgress {
  k: number; // 已完成计数（含当前）
  n: number; // 总数
  label: string; // 序列 / 文件标签
  src: string; // 源路径
  out: string; // 输出路径
  ok: boolean;
  error: string | null;
}
