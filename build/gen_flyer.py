#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""生成 Unixel 一页式宣传单页 PDF（A4 竖向，浅色可打印）。"""
from reportlab.lib.pagesizes import A4
from reportlab.lib.units import mm
from reportlab.lib import colors
from reportlab.lib.styles import getSampleStyleSheet, ParagraphStyle
from reportlab.lib.enums import TA_LEFT
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.platypus import (
    BaseDocTemplate, PageTemplate, Frame, Paragraph, Spacer, ListFlowable,
    ListItem, HRFlowable, KeepInFrame,
)

OUT = "docs/Unixel宣传单页.pdf"

# 中文 CID 字体（无需外部字体文件）
pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
FONT = "STSong-Light"

ACCENT = colors.HexColor("#2f81f7")
INK = colors.HexColor("#1c2128")
DIM = colors.HexColor("#57606a")
BAND = colors.HexColor("#0d1117")
PANEL = colors.HexColor("#f3f6fb")

ss = getSampleStyleSheet()
H1 = ParagraphStyle("H1", fontName=FONT, fontSize=34, leading=38, textColor=colors.white, spaceAfter=2)
SUB = ParagraphStyle("SUB", fontName=FONT, fontSize=12.5, leading=16, textColor=colors.HexColor("#cdd9e5"))
H2 = ParagraphStyle("H2", fontName=FONT, fontSize=14, leading=18, textColor=ACCENT, spaceBefore=10, spaceAfter=4)
BODY = ParagraphStyle("BODY", fontName=FONT, fontSize=10.5, leading=15, textColor=INK, alignment=TA_LEFT)
BODY_W = ParagraphStyle("BODYW", parent=BODY, textColor=colors.white)
SMALL = ParagraphStyle("SMALL", fontName=FONT, fontSize=9, leading=13, textColor=DIM)
BULLET = ParagraphStyle("BULLET", fontName=FONT, fontSize=10.5, leading=15, textColor=INK)


def banner(canvas, doc):
    canvas.saveState()
    w, h = A4
    # 顶部深色条带
    canvas.setFillColor(BAND)
    canvas.rect(0, h - 132, w, 132, stroke=0, fill=1)
    # 强调色下边线
    canvas.setFillColor(ACCENT)
    canvas.rect(0, h - 136, w, 4, stroke=0, fill=1)
    # 条带内标题
    canvas.setFillColor(colors.white)
    canvas.setFont(FONT, 34)
    canvas.drawString(40, h - 78, "Unixel")
    canvas.setFillColor(colors.HexColor("#cdd9e5"))
    canvas.setFont(FONT, 12.5)
    canvas.drawString(42, h - 100, "医学影像处理软件 · 本地离线 · 查看 / 窗位调校 / 格式转换")
    # 底部细条
    canvas.setFillColor(BAND)
    canvas.rect(0, 0, w, 26, stroke=0, fill=1)
    canvas.setFillColor(colors.white)
    canvas.setFont(FONT, 9)
    canvas.drawString(40, 9, "Unixel — 由 Hongwei Shao 构建 · 本地离线运行的医学影像工具")
    canvas.restoreState()


def cell_para(text, style=BODY):
    return Paragraph(text, style)


def bullets(items):
    return ListFlowable(
        [ListItem(Paragraph(t, BULLET), leftIndent=10, value="•") for t in items],
        bulletType="bullet", start="•", leftIndent=12,
    )


def build():
    w, h = A4
    frame = Frame(40, 40, w - 80, h - 132 - 44, id="main", leftPadding=0, rightPadding=0, topPadding=0, bottomPadding=0)
    doc = BaseDocTemplate(OUT, pagesize=A4, leftMargin=40, rightMargin=40, topMargin=148, bottomMargin=34, title="Unixel 宣传单页")
    doc.addPageTemplates([PageTemplate(id="p", frames=[frame], onPage=banner)])

    story = []
    story.append(Paragraph("Unixel 是一款轻量、本地离线运行的桌面医学影像工具，面向影像技师与数据工程师，覆盖 DICOM、常规图像与 NIfTI 体数据的导入、查看、导出闭环，并提供 HTJ2K 高效压缩与批量转换能力，便于嵌入自研数据处理流水线。", BODY))
    story.append(Spacer(1, 4))
    story.append(HRFlowable(width="100%", thickness=1, color=colors.HexColor("#d0d7de")))

    # 核心功能
    story.append(Paragraph("核心功能", H2))
    story.append(bullets([
        "<b>多格式查看</b>：DICOM / PNG·JPG·TIFF / NIfTI 体数据一键打开，自动路由解码",
        "<b>窗宽窗位实时调校</b>：MONOCHROME1 自动反转，多帧 / 序列无闪烁切换，MPR 三视图",
        "<b>保留原始像素导出</b>：导出 DICOM 保留 CT 的 Rescale，读取端可还原 HU",
        "<b>HTJ2K 压缩</b>：纯 Rust 编解码（openjph-core），无损 / 有损，无 C/C++ 依赖",
        "<b>分组脱敏与加密</b>：七组分组的删除 / MD5 / 加密，PBKDF2-AES-256-GCM，符合 DICOM PS3.15",
        "<b>批量转换</b>：DICOM ↔ NIfTI、DICOM → DICOM 重处理，文件夹级、进度可取消",
        "<b>导出 NIfTI 体数据</b>：int16/float32 等类型，写入 RAS 仿射 sform，支持 .nii.gz",
    ]))

    # 技术亮点
    story.append(Paragraph("技术亮点", H2))
    story.append(bullets([
        "Tauri 2 + Vue 3 + Rust，跨 Windows / macOS / Linux",
        "解码核心与界面解耦，可单元测试（cargo test 全绿）",
        "全程离线，影像数据不出本机，不收集用户数据",
        "软件标识写入 (0002,0013) ImplementationVersionName = \"Unixel-{Version}\"",
    ]))

    # 适用人群 + 获取
    story.append(Paragraph("适用人群", H2))
    story.append(Paragraph("影像技师：快速调窗查看、导出指定窗位图像用于报告 / 会诊。<br/>数据工程师：将 DICOM / 常规影像 / NIfTI 转为标准格式，接入下游算法与训练流水线。", BODY))

    story.append(Spacer(1, 6))
    story.append(HRFlowable(width="100%", thickness=1, color=colors.HexColor("#d0d7de")))
    story.append(Spacer(1, 4))
    story.append(Paragraph("版本 v0.1.0 · 作者 邵宏伟 · hongweishao@outlook.com", SMALL))

    doc.build(story)
    print("flyer written:", OUT)


if __name__ == "__main__":
    build()
