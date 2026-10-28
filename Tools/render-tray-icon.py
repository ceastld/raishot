"""从 SVG 生成 Windows 托盘资源。开发时需要 pip install cairosvg pillow；运行和构建无需 Python。"""
from io import BytesIO
from pathlib import Path
import struct
import cairosvg
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SIZES = (16, 20, 24, 32, 40, 48, 64)

def render_icon():
    frames = []
    for size in SIZES:
        png = cairosvg.svg2png(url=str(ROOT / "assets/app.svg"), output_width=size * 4, output_height=size * 4)
        image = Image.open(BytesIO(png)).convert("RGBA").resize((size, size), Image.Resampling.LANCZOS)
        # ICO DIB：自下而上 BGRA 与逐行 32 位对齐的透明掩码。
        pixels = image.load()
        bitmap = bytearray()
        mask = bytearray()
        stride = (size + 31) // 32 * 4
        for y in reversed(range(size)):
            row = bytearray(stride)
            for x in range(size):
                r, g, b, a = pixels[x, y]
                bitmap.extend((b, g, r, a))
                if a == 0:
                    row[x // 8] |= 0x80 >> (x % 8)
            mask.extend(row)
        header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(bitmap), 0, 0, 0, 0)
        frames.append(header + bitmap + mask)
    offset = 6 + len(SIZES) * 16
    directory = bytearray(struct.pack("<HHH", 0, 1, len(SIZES)))
    for size, data in zip(SIZES, frames):
        directory.extend(struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(data), offset))
        offset += len(data)
    target = ROOT / "assets/app.ico"
    target.write_bytes(directory + b"".join(frames))
    print(f"Generated {target.name}: {len(SIZES)} sizes, {target.stat().st_size} bytes")

if __name__ == "__main__":
    render_icon()
