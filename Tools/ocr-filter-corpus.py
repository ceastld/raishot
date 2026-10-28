#!/usr/bin/env python3
"""生成带预先语义标注的 OCR 杂项过滤测试图，不读取桌面，也不运行 OCR。

依赖 Pillow。默认只读取明确的 Windows 字体候选；其它平台可通过 --font、
--mono-font 指定本地字体。图像与按实际绘图边界生成的标注仅写 output/。
示例：python Tools/ocr-filter-corpus.py --split development
所有场景内容、拆分和标签在 tests/data/ocr-filter/scenes.json 中预先声明。
"""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
import random
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont


ROOT = Path(__file__).resolve().parents[1]


def regular_file(path: Path) -> Path:
    """避免读取离线／按需下载占位文件。"""
    stat = path.stat()
    attributes = getattr(stat, "st_file_attributes", 0)
    if attributes & (0x1000 | 0x40000 | 0x400000):
        raise ValueError(f"跳过云端占位字体：{path}")
    if not path.is_file():
        raise ValueError(f"字体不是普通文件：{path}")
    return path


def rectangle(box):
    x, y, w, h = box
    return [x, y, x + w, y + h]


def decoration(draw, item, palette, rng):
    x, y, w, h = item["box"]
    color = item.get("color", palette["decor"])
    kind = item["kind"]
    line = lambda points, width=2: draw.line(points, fill=color, width=width)
    if kind == "icons":
        horizontal = w > h
        count = max(3, min(12, int(max(w, h) / 65)))
        side = min(36, int(min(w, h) * 0.65))
        for index in range(count):
            cx = x + (index + 0.5) * w / count if horizontal else x + w / 2
            cy = y + h / 2 if horizontal else y + (index + 0.5) * h / count
            a, b = cx - side / 2, cy - side / 2
            shape = index % 6
            if shape == 0:
                draw.ellipse((a, b, a + side * 0.7, b + side * 0.7), outline=color, width=3)
                line([(a + side * 0.6, b + side * 0.6), (a + side, b + side)], 3)
            elif shape == 1:
                draw.rectangle((a + 4, b + 3, a + side - 4, b + side - 3), outline=color, width=2)
                line([(a + 10, b + 9), (a + side - 10, b + side - 9)])
            elif shape == 2:
                line([(a + 2, b + side * 0.6), (a + side * 0.4, b + side), (a + side, b + 2)], 3)
            elif shape == 3:
                for bar in range(3):
                    yy = b + 6 + bar * (side - 12) / 2
                    line([(a, yy), (a + side, yy)], 3)
            elif shape == 4:
                line([(a, b + side / 2), (a + side, b + side / 2)], 3)
                line([(a + side / 2, b), (a + side / 2, b + side)], 3)
            else:
                draw.polygon([(a + side / 2, b), (a + side, b + side / 2), (a + side / 2, b + side), (a, b + side / 2)], outline=color)
                draw.ellipse((cx - 3, cy - 3, cx + 3, cy + 3), fill=color)
    elif kind == "strokes":
        # 不调用字体；这里画的是断裂分隔线和短装饰线段。
        for i in range(28):
            xx = x + (i + 0.2) * w / 28
            yy = y + h * (0.25 + rng.random() * 0.5)
            length = rng.randint(2, 14)
            line([(xx, yy), (xx + length, yy + rng.choice([-3, 0, 2]))], rng.choice([1, 2, 3]))
    elif kind == "glyphs":
        # 伪字形由随机短线构成，没有预设单词、汉字或可阅读文本。
        count = max(8, int(w / 30))
        cell = w / count
        for i in range(count):
            xx = x + i * cell + 3
            yy = y + h * 0.25 + rng.randint(-2, 2)
            gh = min(34, h * 0.55)
            for _ in range(rng.randint(2, 5)):
                a = (xx + rng.choice([0, cell * 0.4, cell * 0.7]), yy + rng.choice([0, gh * 0.45, gh]))
                b = (xx + rng.choice([0, cell * 0.4, cell * 0.7]), yy + rng.choice([0, gh * 0.45, gh]))
                line([a, b], rng.choice([1, 2]))
    elif kind == "texture":
        for xx in range(int(x), int(x + w), 13):
            line([(xx, y), (xx + 12, y + h)], 1)
        for yy in range(int(y), int(y + h), 17):
            line([(x, yy), (x + w, yy + 6)], 1)
        for _ in range(18):
            xx, yy = x + rng.random() * w, y + rng.random() * h
            line([(xx, yy), (min(x + w, xx + rng.randint(8, 35)), min(y + h, yy + rng.randint(5, 16)))])
    elif kind == "barcode":
        xx = x
        while xx < x + w - 5:
            bw = rng.choice([1, 2, 3, 4])
            draw.rectangle((xx, y, xx + bw, y + h - rng.choice([0, 0, 8])), fill=color)
            xx += bw + rng.choice([2, 3, 4])
    elif kind == "map":
        line([(x, y + h * 0.6), (x + w * 0.3, y + h * 0.6), (x + w * 0.6, y + h * 0.3), (x + w, y + h * 0.3)], 8)
        for i in range(6):
            xx = x + (i + 0.1) * w / 6
            draw.rectangle((xx, y + 4, xx + 55, y + 35), outline=color, width=2)
            draw.rectangle((xx, y + h - 30, xx + 55, y + h - 3), outline=color, width=2)
    elif kind == "chart":
        line([(x, y), (x, y + h), (x + w, y + h)])
        points = [(x + i * w / 15, y + h * (0.45 + 0.3 * math.sin(i * 0.8))) for i in range(16)]
        line(points, 3)
        for xx, yy in points:
            draw.ellipse((xx - 3, yy - 3, xx + 3, yy + 3), fill=color)
    else:
        raise ValueError(f"未知装饰类型：{kind}")


def render(scene, directory, font_path, mono_path):
    palette = ({"bg": "#151a23", "text": "#e1e6ee", "decor": "#a2acba", "grid": "#566170"}
               if scene["theme"] == "dark" else
               {"bg": "#f7f8fa", "text": "#232936", "decor": "#515a68", "grid": "#aeb5c0"})
    image = Image.new("RGB", tuple(scene["size"]), palette["bg"])
    draw = ImageDraw.Draw(image)
    regions = []
    if "table" in scene:
        x, y, w, h, cols, rows = scene["table"]
        for i in range(cols + 1):
            xx = round(x + i * w / cols)
            draw.line((xx, y, xx, y + h), fill=palette["grid"], width=1)
        for i in range(rows + 1):
            yy = round(y + i * h / rows)
            draw.line((x, yy, x + w, yy), fill=palette["grid"], width=1)
    # 稳定种子由场景名称决定，不使用 Python 进程随机化 hash。
    rng = random.Random(sum((i + 1) * ord(c) for i, c in enumerate(scene["id"])))
    for n, item in enumerate(scene.get("decorations", [])):
        decoration(draw, item, palette, rng)
        regions.append({"id": f"decor-{n}", "label": item.get("label", "drop"),
                        "rect": list(item["box"]), "text": "", "semantic": item["semantic"]})
    for n, item in enumerate(scene["texts"]):
        path = mono_path if item.get("font") == "mono" else font_path
        font = ImageFont.truetype(str(path), item["size"])
        x, y = item["at"]
        box = draw.textbbox((x, y), item["text"], font=font, anchor="lt")
        if box[2] > image.width or box[3] > image.height:
            raise ValueError(f"文字超出画布：{scene['id']} / {item['text']}")
        draw.text((x, y), item["text"], font=font, fill=item.get("color", palette["text"]), anchor="lt")
        pad = 4
        rect = [max(0, box[0] - pad), max(0, box[1] - pad),
                min(image.width, box[2] + pad) - max(0, box[0] - pad),
                min(image.height, box[3] + pad) - max(0, box[1] - pad)]
        regions.append({"id": f"text-{n}", "label": item.get("label", "keep"),
                        "rect": rect, "text": item["text"], "semantic": item["semantic"]})
    if scene.get("blur"):
        image = image.filter(ImageFilter.GaussianBlur(scene["blur"]))
    scale = scene.get("scale", 1.0)
    if scale != 1:
        image = image.resize((round(image.width * scale), round(image.height * scale)), Image.Resampling.LANCZOS)
        for region in regions:
            region["rect"] = [round(v * scale, 2) for v in region["rect"]]
    directory.mkdir(parents=True, exist_ok=True)
    image.save(directory / f"{scene['id']}.png")
    return {"id": scene["id"], "split": scene["split"], "title": scene["title"],
            "theme": scene["theme"], "image": f"images/{scene['id']}.png", "size": list(image.size),
            "source": "程序绘制的独立语义标注场景；不是用户真实截图", "regions": regions}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", type=Path, default=ROOT / "tests/data/ocr-filter/scenes.json")
    parser.add_argument("--out", type=Path, default=ROOT / "output/ocr-filter")
    parser.add_argument("--split", choices=["all", "development", "holdout"], default="all")
    parser.add_argument("--font", type=Path, default=Path("C:/Windows/Fonts/msyh.ttc"))
    parser.add_argument("--mono-font", type=Path, default=Path("C:/Windows/Fonts/consola.ttf"))
    args = parser.parse_args()
    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    font, mono = regular_file(args.font), regular_file(args.mono_font)
    selected = [scene for scene in catalog["scenes"] if args.split == "all" or scene["split"] == args.split]
    args.out.mkdir(parents=True, exist_ok=True)
    manifest = {"version": catalog["version"], "annotation_policy": catalog["annotation_policy"],
                "split_policy": catalog["split_policy"], "generation": {"font": font.name, "mono_font": mono.name,
                "catalog": str(args.catalog.resolve()), "split": args.split},
                "scenes": [render(scene, args.out / "images", font, mono) for scene in selected]}
    target = args.out / ("manifest.json" if args.split == "all" else f"manifest-{args.split}.json")
    target.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    count = sum(len(s["regions"]) for s in manifest["scenes"])
    print(json.dumps({"manifest": str(target), "scenes": len(selected), "regions": count}, ensure_ascii=False))


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    main()
