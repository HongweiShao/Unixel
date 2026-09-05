# Unixel 使用文档

> 医学影像处理软件（Unixel）— 本地离线运行的 DICOM / 常规图像 / NIfTI 查看、窗位调校与格式转换工具。

版本：0.1.0 ｜ 适用平台：Windows 10+ / macOS 12+ / 主流 Linux

---

## 1. 关于 Unixel

Unixel 是一款**轻量、本地离线运行**的桌面医学影像工具，面向影像技师与数据工程师，覆盖 DICOM 与常规图像、体数据（NIfTI）的导入、查看、导出闭环，并提供 HTJ2K 高效压缩与批量转换能力，便于嵌入自研数据处理流水线。

**核心特性**

- 本地离线运行，影像数据不出本机，无网络外传。
- DICOM / 常规图像（PNG·JPG·TIFF）/ NIfTI 体数据一站式查看与导出。
- 窗宽窗位实时调校，MONOCHROME1 自动反转，多帧 / 序列无闪烁切换。
- 导出保留原始像素（CT 的 `RescaleSlope/Intercept` 一并保留），读取端可还原 HU。
- HTJ2K 纯 Rust 编解码（openjph-core，无 C/C++ FFI），支持无损 / 有损。
- 分组脱敏 + PBKDF2-AES-256-GCM 加密，符合 DICOM PS3.15 去标识化标记。
- 批量转换（DICOM ↔ NIfTI、DICOM → DICOM 重处理），进度可取消。

---

## 2. 系统要求

| 项目 | 要求 |
|------|------|
| 操作系统 | Windows 10 及以上；macOS 12 及以上；主流 Linux 发行版 |
| 内存 | 建议 8 GB 以上（处理 3D 体数据或多帧序列时更大更流畅） |
| 磁盘 | 视影像数据量而定，建议预留数 GB 临时空间 |
| 权限 | 仅需读写本地文件，无需管理员权限、无需联网 |

---

## 3. 快速开始

1. **启动 Unixel**，进入主界面（占位区提示「打开文件」或「批量转换」）。
2. **打开文件**：菜单栏「文件 → 打开文件…」或拖入文件，按扩展名自动路由解码：
   - `dcm` / `dicom` → DICOM
   - `png` / `jpg` / `jpeg` / `tif` / `tiff` → 常规图像
   - `nii` / `nii.gz` → NIfTI 体数据
   - `j2c` / `jph` → HTJ2K 帧
3. 解码后图像显示在画布，状态栏展示尺寸、帧数、位深、光度解释、Slope / Intercept 等元数据。

> 提示：仅支持单通道灰度影像；多样本彩色影像会给出明确错误提示。解码失败（如压缩传输语法未支持）会返回可读中文错误。

---

## 4. 查看影像

### 4.1 窗宽窗位调校

右侧工具栏「视图」组提供：

- **窗位（WC）** 滑块：窗中心，单位 HU，范围取图像实际 HU 区间。
- **窗宽（WW）** 滑块：窗跨度，单位 HU。
- 拖动滑块画面**实时变化**；MONOCHROME1（值越大越暗）样本会自动反转显示。

### 4.2 缩放与平移

- **滚轮缩放**：0.1×–8×，当前缩放比例在画布右下角角标 `1.00x` 实时显示。
- **拖拽平移**：在画布上按住鼠标拖动。
- **重置**：恢复默认窗位 / 缩放 / 平移 / 帧。

### 4.3 多帧与序列导航

- 多帧 / 同系列多切片时，画布与工具栏之间的**竖向滚动条**对应图像序号，拖动或滚轮即可直接切换，无闪烁。
- 也可在「视图」组用「帧」滑块切换，显示 `当前/总数`。
- NIfTI 体数据可在 MPR 视图切换轴向 / 冠状 / 矢状三视图。

---

## 5. 导出窗口图像（JPEG）

点击右侧「导出 JPEG」，按**当前窗位窗宽**导出（MONOCHROME1 反转），可叠加标签与水印：

- **范围**
  - `当前帧`：仅当前切片，输出单个 `.jpg`。
  - `整个序列`：多帧 / 多切片 / NIfTI 体积均视为同一序列连续切片，输出 `slice_001.jpg … slice_NNN.jpg`。
- **标签叠加**：勾选机构名称、患者编号、序列描述、窗位 / 窗宽等，渲染到图像四角，随当前窗实时取值。
- **水印**：可选文本，渲染到图像中央偏下。
- **质量**：JPEG 质量滑杆 10–100。

> 示例（mock）数据不可导出，按钮禁用并提示。

---

## 6. 导出 DICOM

点击右侧「导出 DICOM」，**保留原始像素数据**（CT 的 `Rescale` 一并保留，读取端可还原 HU），仅在所选范围内做传输语法转换与可选脱敏，并写入软件标识。

### 6.1 范围

- `当前帧`：输出单个 `.dcm`。
- `整个序列`：导出序列全部图像；源可为多帧或同系列多切片（按排序键排序）。

### 6.2 压缩方式（传输语法）

| 选项 | 说明 | 传输语法 UID |
|------|------|--------------|
| 未压缩（显式 VR） / 未压缩（隐式 VR） | 原始像素原样封装 | — |
| RLE 无损 | 逐扫描行 PackBits 风格编码 | `1.2.840.10008.1.2.5` |
| HTJ2K 无损 / 有损 | openjph-core，按 8/16-bit、有/无符号自动适配 | `.201` / `.203` |
| JPEG-LS 无损 | 压缩原始存储像素，不丢诊断位深 | `1.2.840.10008.1.2.4.80` |
| JPEG-LS 有损（近无损） | 最大重建误差 ±NEAR，不改变像素位深 | `1.2.840.10008.1.2.4.81` |

> 选「HTJ2K 有损」或「JPEG-LS 有损」时显示「有损程度」滑杆。

### 6.3 脱敏

按分组独立选择方式，七组均默认「保留」，`唯一标识` 组另可选「重生成 UID」。详见第 8 节。

### 6.4 软件标识（自动写入）

导出由后端自动写入文件元信息与数据集工具标识块（均不参与脱敏）：

- `(0002,0013) ImplementationVersionName` = `Unixel-{Version}`（{Version}=软件版本号，如 `Unixel-0.1.0`）
- `(0018,1016)` = `Hongwei Shao`
- `(0018,1018)` = `Unixel`
- `(0018,1019)` = 同 `(0002,0013)`
- `(0018,1012)` = 导出日期（DA）
- `(0018,1014)` = 导出时间（TM）

### 6.5 输出形式（仅「整个序列」出现）

- `单个文件（多帧）`：合并为单文件多帧 Secondary Capture。
- `多个文件（单帧）`：逐帧写出 `frame_001.dcm … frame_NNN.dcm`。

---

## 7. 批量转换

菜单栏「处理 → 批量转换」或启动占位区「批量转换」按钮，弹出批量转换对话框，**按文件夹级**批量处理。

1. **输入区**：选择输入文件夹与输入类型（DICOM / NIfTI）。
2. **输出区**：选择输出文件夹与输出类型（DICOM / NIfTI）。
   - `DICOM→DICOM` 允许（重处理）；`NIfTI→NIfTI` 无意义，按钮禁用并提示。
3. **输出选项**
   - 输出 = DICOM：复用「导出 DICOM」的全部选项（传输语法 + 七组脱敏 + 自动写标识）。
   - 输出 = NIfTI：数据类型 + 写 sform + 压缩 `.nii.gz`。
4. **开始 / 取消 / 进度**
   - 「开始转换」校验后调用后端批量处理；进行中按钮变为「取消」，可中止剩余序列。
   - 进度区实时显示 `处理中 k/N` 与成功 / 失败计数；单序列失败不中断，记录路径与原因后跳过。
   - 结束汇总成功 / 失败，失败可展开看明细；中途取消提示已完成 / 跳过数量。
5. **输出组织**：DICOM 按 `SeriesInstanceUID` 聚合为单元；输出目录保持输入相对路径结构。

---

## 8. 脱敏与隐私

脱敏范围严格按 DICOM Tag 定义（后端 `ANON_GROUPS` 直接以原始 Tag 表述，杜绝 keyword 漂移）。

### 8.1 七组范围

| 分组 | 代表标签 |
|------|----------|
| 患者身份 | PatientName、PatientID、PatientBirthDate、PatientAddress、PatientTelephoneNumbers、OtherPatientIDs |
| 人员身份 | ReferringPhysicianName、PerformingPhysicianName、OperatorsName、NameOfPhysiciansReadingStudy、ConsultingPhysicianName |
| 机构信息 | InstitutionName、InstitutionAddress、InstitutionalDepartmentName、StationName |
| 设备信息 | Manufacturer、ManufacturerModelName、DeviceSerialNumber、SoftwareVersions（保留源设备版本，不覆写） |
| 日期时间 | StudyDate/Time、SeriesDate/Time、AcquisitionDate/Time |
| 唯一标识 | StudyInstanceUID、SeriesInstanceUID、SOPInstanceUID、FrameOfReferenceUID、AcquisitionUID、AccessionNumber |
| 自由文本 | StudyDescription、SeriesDescription、ImageComments、AdditionalPatientHistory、IdentifyingComments、AcquisitionProtocolDescription |

不在范围内的标签（如 PatientSex、ContentDate/Time）一律不脱敏。

### 8.2 处理方式

- `保留` / `删除` / `MD5 摘要` / `加密`；`唯一标识` 组另支持 `重生成 UID`。
- **加密**：`PBKDF2-HMAC-SHA256(密码, 盐)` 派生 256-bit 密钥，AES-256-GCM 加密标签值；盐与算法标识写入私有标签（Creator = `UNIXEL`）；密码不写入文件。**密码留空时默认口令 `unixel`**。
- **重生成 UID**：一致随机重生本组全部 UID，脱敏后同步更新 `MediaStorageSOPInstanceUID`。

### 8.3 强制去标识化标记

只要实际执行了任意脱敏（含 UID 重生成），后端自动写入：

- `(0012,0062) PatientIdentityRemoved = YES`
- `(0012,0063) DeidentificationMethod`（汇总本次实际方法）
- `(0012,0064) DeidentificationMethodCodeSequence`（加密 `UNIXEL-ANON-ENC` / UID 重生成 `UNIXEL-ANON-UID` / 删除 `UNIXEL-ANON-DEL` / MD5 `UNIXEL-ANON-HASH`）

符合 DICOM PS3.15 去标识化标记要求。

### 8.4 解密还原

加密脱敏的文件可经标签详情对话框的「解密」功能，用相同密码还原标签明文（见第 9 节）。

---

## 9. 标签查看与解密

点击图像元数据区域可打开「标签详情对话框」，查看完整 DICOM 标签（含文件元信息 0002 组与传输语法）：

- 列表含数据集中全部标签，**并补充 0002 组文件元信息**（含 TransferSyntaxUID），可直接看到传输语法。
- 顶部搜索框按 Tag / 名称过滤；对话框高度固定，搜索时不收缩。
- 行内圆形「?」图标悬停弹出该标签的 DICOM 释义。
- 加密脱敏文件：在标签对话框中可输入密码「解密」，还原被 AES-256-GCM 加密的标签明文（解密不修改原文件，仅预览）。

---

## 10. 导出 NIfTI

点击右侧「导出 NIfTI」，将 DICOM 序列 / 多帧的 HU 像素导出为 NIfTI 体数据（3D 体或 2D 切片），供 MPR / 三维后处理。

- **范围**：`当前帧（2D）` / `整个序列（3D 体）`。
- **数据类型**：`int16`（默认，无损当 HU∈[-32768,32767]）/ `int32` / `uint16`（+1024 偏移）/ `uint8`（强制有损预览）/ `float32` / `float64`。整数类型越界会被截断并提示。
- **朝向**：默认写入 `sform`（RAS 仿射，由 IOP/IPP 推导，DICOM LPS→NIfTI RAS 对 x、y 取负）；取消则仅写像素间距。
- **压缩**：默认输出 `.nii.gz`（gzip）；取消则写未压缩 `.nii`。
- **软件标识**：自动写入 `descrip` 字段 `Unixel-{Version}`。

---

## 11. 帮助菜单

菜单栏「帮助」提供：

- **关于**：弹出关于对话框，显示软件名称、版本与版权。
- **使用文档**：打开本使用文档（应用内显示）。

---

## 12. 已知限制与提示

- 当前版本主要面向未压缩 DICOM；部分压缩传输语法（JPEG2000 / Lossy 等）需后续接入。
- 单帧窗映射在 CPU 端（前端 JS），超大分辨率（如 4K×4K 多帧）可能有性能压力。
- 暂不支持 DICOM 结构化报告（SR）生成与写回。
- 批量导出为「每文件按默认窗」模式，尚未实现序列级分组与窗位预设管理。
- 脱敏「加密」默认口令为 `unixel`；如留空请务必牢记自定义口令，遗失将无法解密还原。

---

## 13. 隐私与安全声明

- 全程**本地离线**运行，影像数据不上传任何服务器、不收集用户数据。
- 所有处理均在本机完成，导出文件仅含你选择保留或脱敏后的元数据。

---

*Unixel — 由 Hongwei Shao 构建。软件标识写入 `(0002,0013) ImplementationVersionName = "Unixel-{Version}"`。*
