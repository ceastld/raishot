"""渲染品牌研究对照图，不修改运行图标或 Windows 主题。"""
from io import BytesIO
from pathlib import Path
import xml.etree.ElementTree as ET

import cairosvg
from PIL import Image, ImageDraw, ImageFont


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
FONT = Path("C:/Windows/Fonts/msyh.ttc")
BOLD = Path("C:/Windows/Fonts/msyhbd.ttc")
BG, PANEL, TEXT, MUTED = "#101817", "#182320", "#ECF4F1", "#A2B6AE"
ACCENT = "#62D4AE"
NS = "{http://www.w3.org/2000/svg}"
ET.register_namespace("", NS[1:-1])


def raster(source, size):
    if isinstance(source, Path):
        source = source.read_bytes()
    return Image.open(BytesIO(cairosvg.svg2png(
        bytestring=source, output_width=size, output_height=size
    ))).convert("RGBA")


def monochrome(source, color):
    tree = ET.fromstring(source.read_bytes())
    if source.name == "app.svg":
        tree.remove(tree.find(f"{NS}defs"))
        tree.remove(tree.find(f"{NS}rect"))
    elif source.name == "take-piece.svg":
        tree = ET.fromstring((HERE / "take-piece-tray.svg").read_bytes())
        tree.set("color", color)
    else:
        tree.remove(tree.find(f"{NS}rect"))
        # 光学放大符号，让托盘不用应用底板时仍充分利用可用尺寸。
        mark = tree.find(f"{NS}g")
        mark.set("transform", "translate(-14 -14) scale(1.44)")
    for element in tree.iter():
        for prop in ("fill", "stroke"):
            if element.get(prop) == "#FFFFFF" or element.get(prop) == "white":
                element.set(prop, color)
    return ET.tostring(tree)


board = Image.new("RGB", (1200, 814), BG)
draw = ImageDraw.Draw(board)


def text(xy, value, size=18, color=TEXT, bold=False):
    font = ImageFont.truetype(str(BOLD if bold else FONT), size)
    draw.text(xy, value, fill=color, font=font)


def paste(icon, xy):
    board.paste(icon, xy, icon)


text((36, 26), "让人记住的，是轮廓", 32, bold=True)
text((36, 77), "RustShot 发布品牌研究  ·  图标方向草案  ·  2026-10-16", 17, MUTED)
text((36, 113), "统一用绿色比较图形；名称与颜色均未定稿。", 17, MUTED)

concepts = [
    ("现有图标", "取景框 + 圆点", ROOT / "assets/app.svg", "用途直观，形状偏通用", "也容易联想到扫描或瞄准"),
    ("A  取下一片", "框住，再取出来", HERE / "take-piece.svg", "不对称轮廓，建议深化", "需防止被误认成新窗口"),
    ("B  叠放卡片", "截图留在眼前", HERE / "stacked-cards.svg", "贴图含义直观", "容易联想到复制或多窗口"),
    ("C  画框一笔", "截下来，标清楚", HERE / "frame-pen.svg", "截图与标注含义直接", "容易联想到通用图片编辑"),
]

for index, (title, subtitle, source, note1, note2) in enumerate(concepts):
    x = 24 + index * 294
    draw.rounded_rectangle((x, 168, x + 270, 747), radius=18, fill=PANEL)
    if index == 1:
        draw.rounded_rectangle((x + 18, 185, x + 84, 211), radius=8, fill="#234B3D")
        text((x + 28, 187), "推荐方向", 13, ACCENT)
    else:
        text((x + 20, 187), "现状" if index == 0 else "备选方向", 13, MUTED)
    paste(raster(source, 124), (x + 73, 228))
    text((x + 20, 367), title, 23, bold=True)
    text((x + 20, 405), subtitle, 17, MUTED)

    text((x + 20, 451), "应用图标 · 原尺寸", 14, MUTED)
    for size, ox in [(16, 24), (24, 86), (32, 156)]:
        paste(raster(source, size), (x + ox, 484 + (32 - size) // 2))
        text((x + ox, 521), str(size), 12, MUTED)

    text((x + 20, 556), "单色符号 · 16px / 放大 3×", 14, MUTED)
    for k, (surface, fg) in enumerate([("#0C1110", "#FFFFFF"), ("#E7EEEA", "#18372B")]):
        sx, sy = x + 20 + k * 116, 588
        draw.rounded_rectangle((sx, sy, sx + 108, sy + 64), radius=8, fill=surface)
        mono = raster(monochrome(source, fg), 16)
        paste(mono, (sx + 10, sy + 24))
        paste(mono.resize((48, 48), Image.Resampling.NEAREST), (sx + 45, sy + 8))
    text((x + 20, 681), note1, 16, ACCENT if index == 1 else TEXT)
    text((x + 20, 710), note2, 14, MUTED)

text((36, 770), "已做原尺寸栅格化与明暗底检查；尚未装入 Windows 托盘，也未做用户识别测试。", 16, MUTED)
board.save(HERE / "icon-directions.png")
print(f"Rendered {HERE / 'icon-directions.png'} ({board.width} x {board.height})")
