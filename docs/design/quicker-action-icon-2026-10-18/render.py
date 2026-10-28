"""输出 Quicker 动作图标候选和原尺寸对照，不修改运行中的动作。"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
FONT = "C:/Windows/Fonts/msyh.ttc"
BOLD = "C:/Windows/Fonts/msyhbd.ttc"


def raster(name, size):
    return Image.open(HERE / f"{name}-{size}.png").convert("RGBA")


concepts = [
    ("take-piece", "A  取下一片", "推荐 · 延续品牌方向", "框住屏幕内容，再取出一片"),
    ("paper-cut", "B  剪纸切角", "更强调 Kirie 的剪纸含义", "完整纸片与独立切角"),
    ("paper-k", "C  剪纸 K", "更强调名称记忆", "用两片纸构成字母 K"),
]

board = Image.new("RGB", (1260, 800), "#121615")
draw = ImageDraw.Draw(board)


def text(xy, value, size=18, color="#ECF3F0", bold=False):
    draw.text(xy, value, font=ImageFont.truetype(BOLD if bold else FONT, size), fill=color)


def paste(icon, xy):
    board.paste(icon, xy, icon)


text((42, 28), "Kirie · Quicker 动作图标", 34, bold=True)
text((44, 84), "同一枚图标用于 V1 / V2   ·   翡翠绿 / 透明外边缘   ·   不加名称与版本角标", 18, "#9EAEA7")

for index, (name, title, subtitle, detail) in enumerate(concepts):
    x = 30 + 410 * index
    draw.rounded_rectangle((x, 136, x + 380, 714), radius=20, fill="#1D2421")
    text((x + 24, 156), subtitle, 16, "#7AE1B9" if index == 0 else "#9EAEA7")
    paste(raster(name, 164), (x + 108, 201))
    text((x + 24, 388), title, 26, bold=True)
    text((x + 24, 432), detail, 16, "#ADBAB3")
    for row, (bg, fg, label) in enumerate([
        ("#101512", "#A9BAB0", "深色面板"),
        ("#F1F4F2", "#4D6257", "浅色面板"),
    ]):
        y = 478 + row * 100
        draw.rounded_rectangle((x + 20, y, x + 360, y + 86), radius=12, fill=bg)
        text((x + 33, y + 12), label, 13, fg)
        for size, ox in ((24, 124), (32, 195), (48, 278)):
            paste(raster(name, size), (x + ox - size // 2, y + 35 - size // 2))
            text((x + ox - 12, y + 62), str(size), 12, fg)

text((44, 746), "下方为 24 / 32 / 48 px 原尺寸。A 另备透明底单色版；均保留可编辑 SVG 与 PNG。", 17, "#9EAEA7")
board.save(HERE / "comparison.png")
print(HERE / "comparison.png")
