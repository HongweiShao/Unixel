# Unixel · 医学影像处理软件

> **Unixel** — a lightweight, fully offline desktop tool for viewing, windowing, and converting medical images (DICOM / common images / NIfTI).
> 一款轻量、本地离线运行的桌面医学影像工具，覆盖 DICOM、常规图像与 NIfTI 体数据的查看、窗位调校与格式转换。

[![Platform](https://img.shields.io/badge/platform-Windows%2010%2B%20%7C%20macOS%2012%2B%20%7C%20Linux-blue)](https://tauri.app)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24c8d8)](https://v2.tauri.app)
[![Vue](https://img.shields.io/badge/Vue-3.x-42b883)](https://vuejs.org)
[![Rust](https://img.shields.io/badge/Rust-edition%202021-dea584)](https://www.rust-lang.org)
[![Version](https://img.shields.io/badge/version-0.1.0-green)](./package.json)

---

## ✨ 功能特性（Features）

- **本地离线运行** — 影像数据全程不出本机，无网络外传，无需登录或联网。
- **多格式导入**
  - DICOM（`.dcm` / `.dicom`）：经 `dicom-rs` 解码，自动套用 Modality LUT（`RescaleSlope/Intercept`）还原 HU。
  - 常规图像（PNG / JPG / TIFF）。
  - NIfTI 体数据（`.nii` / `.nii.gz`）：MPR 轴向 / 冠状 / 矢状三视图重建。
- **查看与窗位调校** — 窗宽（WW）/ 窗位（WC）实时滑块调校、滚轮缩放（0.1×–8×）、拖拽平移、多帧切换、`MONOCHROME1` 自动反转、一键重置。
- **按窗位导出** — 将「当前窗宽窗位」下的当前帧导出为 PNG（无损）或 JPEG（可调质量 10–100）。
- **DICOM 导出与批量转换** — 作用域（当前帧 / 整序列）、传输语法（未压缩 / RLE / HTJ2K 无损·有损）、以及 DICOM ↔ NIfTI、DICOM → DICOM 重处理；批量进度可取消。
- **HTJ2K 纯 Rust 编解码** — 基于 `openjph-core`（JPEG 2000 Part 15 的 Rust 忠实移植，**无 C/C++ FFI**），支持无损（TS `1.2.840.10008.1.2.4.201`）与有损（TS `.203`）。
- **脱敏与隐私保护** — 七组分（患者 / 机构 / 就诊 / 人员 / 设备 / 私人 / 研究）分组脱敏；加密模式采用 **PBKDF2 + AES-256-GCM**，并按 DICOM PS3.15 写入去标识化标记（`DeIdentificationMethod`）。
- **NIfTI 导出** — 支持将 DICOM 序列导出为 NIfTI 体数据。
- **内置使用文档** — 帮助菜单「使用文档」直接弹出应用内 Markdown 手册，无需额外文件。

---

## 🧱 技术栈（Tech Stack）

| 层 | 选型 |
|----|------|
| 桌面框架 | **Tauri 2**（`@tauri-apps/api` v2、`@tauri-apps/cli` v2） |
| 前端 | **Vue 3** + **TypeScript** + **Vite 5** + `vue-tsc` |
| 后端 | **Rust 2021** |
| 医学影像 | `dicom-rs` 0.7（`dicom-object` / `dicom-pixeldata` / `dicom-transfer-syntax-registry` / `dicom-encoding` / `dicom-dictionary-std`） |
| 图像 | `image` 0.25、`imageproc` 0.25、`ab_glyph`（CJK 文字叠加） |
| HTJ2K | `openjph-core` 0.1（纯 Rust，无 bindgen） |
| 体数据 | `nifti` 0.11 + `ndarray` 0.13 |
| 加密脱敏 | `aes-gcm` 0.10、`pbkdf2` 0.12、`sha2` / `hmac` / `md-5`、`rand` |
| 对话框 | `@tauri-apps/plugin-dialog` + `tauri-plugin-dialog` |

---

## 🚀 快速开始（Quick Start）

### 方式一：从源码构建（推荐开发者）

#### 前置依赖

- **Rust** ≥ 1.77（含 `cargo`），参考 <https://www.rust-lang.org/tools/install>
- **Node.js** ≥ 18 + npm
- 操作系统构建依赖（Tauri 官方要求）：
  - Windows：Microsoft C++ Build Tools（MSVC）
  - macOS：Xcode Command Line Tools
  - Linux：`webkit2gtk-4.1` / `libgtk-3` / `libsoup` 等（见 [Tauri 前置依赖](https://v2.tauri.app/start/prerequisites/)）

#### 安装与运行

```bash
# 1. 克隆仓库
git clone <your-repo-url> unixel
cd unixel

# 2. 安装前端依赖
npm install

# 3. 开发模式（带热更新，自动启动桌面窗口）
npm run tauri dev

# 4. 打包发布（生成平台安装包到 src-tauri/target/release/bundle/）
npm run tauri build
```

#### 仅构建前端（调试用）

```bash
npm run dev       # 启动 Vite 开发服务器
npm run build     # vue-tsc 类型检查 + vite 生产构建
```

### 方式二：下载预编译包

> 预编译安装包随后续 Release 提供（Windows `.msi` / macOS `.dmg` / Linux `.AppImage`、`.deb`）。
> 当前 v0.1.0 请按「方式一」从源码构建。

---

## 📖 使用文档

应用内：菜单栏 **帮助 → 使用文档** 打开完整 Markdown 手册（覆盖查看、窗位、导出、批量转换、脱敏、标签查看等）。

仓库文档（`docs/`）：

- `docs/产品需求规格书.md` — 产品需求与里程碑
- `docs/界面设计文档.md` — 界面与交互规格
- `docs/Unixel宣传单页.pdf` — 一页宣传单页
- `src/assets/docs/usermanual.md` — 使用文档源（应用内渲染）

---

## 🗂️ 项目结构

```
Unixel/
├─ src/                      # 前端（Vue 3 + TS）
│  ├─ App.vue                # 主界面 / 菜单 / 弹窗编排
│  ├─ components/
│  │  ├─ Viewer.vue          # 影像查看器（窗位/缩放/导出）
│  │  └─ HelpModal.vue       # 应用内使用文档弹窗
│  └─ assets/docs/usermanual.md
├─ src-tauri/                # 后端（Rust + Tauri 2）
│  ├─ src/lib.rs             # 命令实现：解码/窗位/导出/批量/脱敏/NIfTI
│  ├─ Cargo.toml
│  ├─ tauri.conf.json
│  └─ capabilities/
├─ docs/                     # 产品 / 界面文档 + 宣传单页
└─ build/                    # PDF 生成脚本（reportlab）
```

---

## 🔧 核心命令（部分）

| 命令 | 说明 |
|------|------|
| `load_dicom` / `load_image` / `load_nifti` / `load_htj2k` | 按扩展名路由导入解码 |
| `export_image` | 当前窗位导出 PNG/JPEG |
| `export_dicom` | 导出 DICOM（作用域 / 传输语法 / 脱敏） |
| `batch_convert` | 批量转换与导出（DICOM↔NIfTI、DICOM→DICOM） |
| `dicom_tags` | 读取全部标签（含 0002 组元信息）+ 解密预览 |

导出 DICOM 自动写入工具标识块：

- 文件元信息 `(0002,0013) ImplementationVersionName` = `Unixel-{Version}`
- 数据集 `(0018,1016)=Hongwei Shao`、`(0018,1018)=Unixel`、`(0018,1019)=Unixel-{Version}`、`(0018,1012)=导出日期`、`(0018,1014)=导出时间`

---

## 🧭 路线图（Roadmap）

- [x] 里程碑 A：DICOM 解码 → 查看器 → 窗位导出 PNG/JPEG
- [x] 里程碑 B：多格式导入（常规图像 + NIfTI / MPR）
- [x] 里程碑 C：HTJ2K 编解码（纯 Rust）+ 批量转换与导出 + 脱敏
- [ ] 压缩 DICOM 传输语法（JPEG2000 / JPEG-LS / HTJ2K）直接接入查看
- [ ] 按 `SeriesInstanceUID` 的序列分组管理面板与序列级窗位预设
- [ ] 超大分辨率下放 Rust / WASM 或 GPU 加速

---

## 🤝 贡献（Contributing）

欢迎 Issue 与 PR。提交前请确保：

```bash
# 前端类型检查 + 构建
npm run build
# 后端测试（cargo 走默认或离线缓存均可）
cd src-tauri && cargo test --lib
```

---

## ⚠️ 免责声明（Disclaimer）

Unixel 仅供**研究、教育与非临床**用途，**不构成为诊断设备**，不对任何诊断或临床决策负责。使用本软件导出的影像前，请由具备资质的影像科医师复核。软件按「现状」提供，不保证适用于任何特定医疗目的。

---

## 📄 许可证（License）

本项目以 **GNU Lesser General Public License v3.0（LGPL-3.0）** 授权开源。

- 完整许可证文本见仓库根目录 `LICENSE` 文件。
- 在 LGPL-3.0 条款下，你可自由使用、修改、分发本软件，并可将其作为库链接到闭源或专有软件（须履行相应义务：对修改部分以 LGPL-3.0 开源、保留版权与许可证声明、提供源码获取方式等）。
- 本软件按「现状」提供，免责与责任限制详见 `LICENSE`。

作者：Hongwei Shao（邵宏伟）｜ 联系：hongweishao@outlook.com
