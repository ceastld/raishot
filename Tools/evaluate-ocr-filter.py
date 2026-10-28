#!/usr/bin/env python3
"""对真实 OCR 候选评估过滤，严格分开识别遗漏、过滤错误和匹配歧义。

python Tools/evaluate-ocr-filter.py --cli target/debug/raishot-cli.exe
python Tools/evaluate-ocr-filter.py --cli target/debug/raishot-cli.exe --split holdout --freeze-note "策略已冻结"
旧 CLI 可加 --cache-only --producer-note "旧识别产物，不含过滤结果" 缓存原始识别。
无 --cli 时读取 --raw-dir 已有 JSON；缺失 filter 字段时禁止计算过滤指标。
仅使用合成图；不截取桌面、不修改剪贴板、不打开窗口、不构建应用。
"""
from __future__ import annotations

import argparse
from collections import Counter
import html
import json
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))


def write_json(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def box_tuple(rect):
    if not rect:
        return None
    if isinstance(rect, dict):
        return tuple(float(rect[key]) for key in ("x", "y", "width", "height"))
    return tuple(float(v) for v in rect)


def intersection(a, b):
    x, y, w, h = a
    xx, yy, ww, hh = b
    return max(0, min(x + w, xx + ww) - max(x, xx)) * max(0, min(y + h, yy + hh) - max(y, yy))


def classify(rect, regions):
    if not rect or rect[2] <= 0 or rect[3] <= 0:
        return "unmatched", [], "缺少有效识别框"
    area = rect[2] * rect[3]
    candidates = []
    for region in regions:
        target = box_tuple(region["rect"])
        overlap = intersection(rect, target)
        if overlap <= 0:
            continue
        line_ratio = overlap / area
        target_ratio = overlap / max(1, target[2] * target[3])
        # 用双向覆盖接纳被 OCR 扩张的真实文本框，避免仅凭中心误配。
        if line_ratio >= 0.25 or target_ratio >= 0.45:
            candidates.append((region, line_ratio, target_ratio))
    labels = {r["label"] for r, _, _ in candidates}
    if "keep" in labels and "drop" in labels:
        return "ambiguous", [r["id"] for r, _, _ in candidates], "同一 OCR 行合并了真实文字和非文字区域"
    if "ignore" in labels:
        return "ignore", [r["id"] for r, _, _ in candidates], "预标注为不计分区域"
    if labels:
        label = "keep" if "keep" in labels else "drop"
        hits = [r["id"] for r, _, _ in candidates if r["label"] == label]
        return label, hits, "按原始语义区域几何匹配"
    return "unmatched", [], "未匹配预标注区域，不能自动判定去留真值"


def evaluate(scene, raw):
    filt = raw.get("filter")
    if not isinstance(filt, dict) or not isinstance(filt.get("lines"), list):
        raise ValueError(f"{scene['id']} 没有 filter.lines；旧识别产物只能缓存，不能据此报告过滤效果")
    excluded = {}
    for entry in filt["lines"]:
        if entry.get("excluded", True):
            excluded[int(entry["index"])] = entry.get("reason", "未提供原因")
    baseline = None
    if scene["split"] == "development" and isinstance(filt.get("baseline_lines"), list):
        baseline = {int(entry["index"]): entry.get("reason", "未提供原因")
                    for entry in filt["baseline_lines"] if entry.get("excluded", True)}
    lines = raw.get("lines", [])
    if any(i < 0 or i >= len(lines) for i in excluded):
        raise ValueError(f"{scene['id']} 过滤行索引超出原始 lines")
    if baseline is not None and any(i < 0 or i >= len(lines) for i in baseline):
        raise ValueError(f"{scene['id']} 基线过滤行索引超出原始 lines")
    counts = Counter()
    baseline_counts = Counter()
    results = []
    matched = set()
    for index, line in enumerate(lines):
        rect = box_tuple(line.get("rect"))
        label, region_ids, match_reason = classify(rect, scene["regions"])
        matched.update(region_ids)
        removed = index in excluded
        counts[f"{label}_lines"] += 1
        counts[f"{label}_{'filtered' if removed else 'retained'}"] += 1
        if baseline is not None:
            baseline_counts[f"{label}_lines"] += 1
            baseline_counts[f"{label}_{'filtered' if index in baseline else 'retained'}"] += 1
        results.append({"index": index, "text": line.get("text", ""), "rect": rect, "label": label,
                        "filtered": removed, "reason": excluded.get(index), "regions": region_ids,
                        "match_reason": match_reason, "recognition_score": line.get("recognition_score"),
                        "baseline_filtered": None if baseline is None else index in baseline})
    omissions = [r for r in scene["regions"] if r["id"] not in matched]
    counts["keep_regions"] = sum(r["label"] == "keep" for r in scene["regions"])
    counts["drop_regions"] = sum(r["label"] == "drop" for r in scene["regions"])
    counts["keep_regions_unrecognized"] = sum(r["label"] == "keep" for r in omissions)
    counts["drop_regions_unrecognized"] = sum(r["label"] == "drop" for r in omissions)
    for key in ("keep_regions", "drop_regions", "keep_regions_unrecognized", "drop_regions_unrecognized"):
        baseline_counts[key] = counts[key]
    return {"id": scene["id"], "split": scene["split"], "title": scene["title"], "image": scene["image"],
            "size": scene["size"], "algorithm": filt.get("algorithm"), "counts": dict(counts),
            "baseline_counts": None if baseline is None else dict(baseline_counts),
            "lines": results, "unrecognized_regions": omissions}


def metrics(counts):
    tp = counts.get("drop_filtered", 0)
    fp = counts.get("keep_filtered", 0)
    fn = counts.get("drop_retained", 0)
    keep = counts.get("keep_lines", 0)
    ratio = lambda a, b: None if not b else a / b
    return {"正文误删率": ratio(fp, keep), "杂项过滤精确率": ratio(tp, tp + fp),
            "已识别杂项召回率": ratio(tp, tp + fn),
            "正文行": keep, "正文误删": fp, "识别出的杂项行": tp + fn,
            "杂项命中": tp, "杂项漏过": fn,
            "正文区域未识别": counts.get("keep_regions_unrecognized", 0),
            "杂项区域未识别（不算成功）": counts.get("drop_regions_unrecognized", 0),
            "歧义行": counts.get("ambiguous_lines", 0), "未匹配行": counts.get("unmatched_lines", 0),
            "不计分行被过滤": counts.get("ignore_filtered", 0) + counts.get("ambiguous_filtered", 0) + counts.get("unmatched_filtered", 0)}


def summary(scenes):
    groups = {}
    for split in sorted({scene["split"] for scene in scenes}):
        counts = Counter()
        baseline_counts = Counter()
        has_baseline = False
        for scene in scenes:
            if scene["split"] == split:
                counts.update(scene["counts"])
                if scene["baseline_counts"] is not None:
                    has_baseline = True
                    baseline_counts.update(scene["baseline_counts"])
        groups[split] = {"counts": dict(counts), "metrics": metrics(counts)}
        if has_baseline:
            groups[split]["baseline"] = {"counts": dict(baseline_counts), "metrics": metrics(baseline_counts)}
    if len(groups) > 1:
        combined = Counter()
        for scene in scenes:
            combined.update(scene["counts"])
        groups["all"] = {"counts": dict(combined), "metrics": metrics(combined)}
    return groups


def fmt(value, key=""):
    if value is None:
        return "—（无分母）"
    return f"{value:.1%}" if key.endswith("率") else str(value)


def reason_text(reason):
    return {"isolated-small-graphic": "相对正文偏小、独立且形似图形的符号",
            "detached-graphic-column": "远离正文的近方形图标列，多个候选互相印证"}.get(reason, str(reason or "—"))


def split_text(split):
    return {"development": "开发集", "holdout": "留出集", "all": "合计"}.get(split, split)


def comparison_figure(report, corpus_root, target, font_path):
    """按数值坐标绘制可复核的数据图，保留原图并叠加识别框。"""
    from PIL import Image, ImageDraw, ImageFont

    attributes = getattr(font_path.stat(), "st_file_attributes", 0)
    if attributes & (0x1000 | 0x40000 | 0x400000):
        raise ValueError(f"跳过云端占位字体：{font_path}")
    lookup = {s["id"]: s for s in report["scenes"]}
    ids = [i for i in ("dev-article-light", "holdout-terminal-punctuation") if i in lookup]
    if not ids:
        return None
    font = ImageFont.truetype(str(font_path), 22)
    small = ImageFont.truetype(str(font_path), 17)
    title = ImageFont.truetype(str(font_path), 30)
    canvas = Image.new("RGB", (1800, 90 + 610 * len(ids) + 80), "#121722")
    draw = ImageDraw.Draw(canvas)
    draw.text((32, 23), "合成语料真实 OCR：开发命中与留出漏过对照", font=title, fill="#e3ebf8", anchor="lt")
    for row, scene_id in enumerate(ids):
        scene = lookup[scene_id]
        m = metrics(scene["counts"])
        yy = 90 + row * 610
        caption = f'{split_text(scene["split"])}：{scene["title"]}　正文误删 {m["正文误删"]}/{m["正文行"]}，杂项命中 {m["杂项命中"]}/{m["识别出的杂项行"]}'
        draw.text((32, yy), caption, font=font, fill="#d8e4f5", anchor="lt")
        original = Image.open(corpus_root / scene["image"]).convert("RGB")
        scale = min(830 / original.width, 500 / original.height)
        size = (round(original.width * scale), round(original.height * scale))
        for col, after in enumerate((False, True)):
            xx = 32 + col * 880
            draw.text((xx, yy + 38), "过滤后（紫色虚线为排除项）" if after else "过滤前（所有识别候选）", font=small, fill="#a7b8d0", anchor="lt")
            top = yy + 70
            canvas.paste(original.resize(size, Image.Resampling.LANCZOS), (xx, top))
            draw.rectangle((xx - 1, top - 1, xx + size[0], top + size[1]), outline="#536278", width=1)
            for line in scene["lines"]:
                if not line["rect"]:
                    continue
                x, y, w, h = line["rect"]
                box = [xx + x * scale, top + y * scale, xx + (x + w) * scale, top + (y + h) * scale]
                color = "#35bf86" if line["label"] == "keep" else "#e6a82f" if line["label"] == "drop" else "#ab87ee"
                if after and line["filtered"]:
                    color = "#ac7ef0"
                    left, upper, right, lower = box
                    for start in range(round(left), round(right), 10):
                        draw.line((start, upper, min(start + 6, right), upper), fill=color, width=3)
                        draw.line((start, lower, min(start + 6, right), lower), fill=color, width=3)
                    for start in range(round(upper), round(lower), 10):
                        draw.line((left, start, left, min(start + 6, lower)), fill=color, width=3)
                        draw.line((right, start, right, min(start + 6, lower)), fill=color, width=3)
                else:
                    draw.rectangle(box, outline=color, width=2)
                label_y = max(top, box[1] - 18)
                draw.text((box[0] + 2, label_y), str(line["index"]), font=small, fill=color, stroke_width=1, stroke_fill="#121722", anchor="lt")
    foot = 90 + 610 * len(ids)
    draw.text((32, foot), "绿色：实际文字　黄色：非文字误识别　紫色虚线：建议排除（原图保持）", font=font, fill="#afbed4", anchor="lt")
    draw.text((32, foot + 35), "冻结策略 contextual-symbol-v2；示例图用于复核，完整拆分指标见 HTML 报告。", font=small, fill="#92a4bd", anchor="lt")
    canvas.save(target)
    return str(target)


def image_svg(scene, after=False):
    width, height = scene["size"]
    parts = [f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="识别框复核"><image href="{html.escape(scene["image"])}" width="{width}" height="{height}"/>']
    for line in scene["lines"]:
        if not line["rect"]:
            continue
        x, y, w, h = line["rect"]
        label = line["label"]
        color = "#58d6a1" if label == "keep" else "#f0bb5c" if label == "drop" else "#b898f2"
        dash = ' stroke-dasharray="6 4"' if line["filtered"] else ""
        opacity = 0.15 if after and line["filtered"] else 0
        title = html.escape(f'{line["index"]}: {line["text"]} | {label} | {reason_text(line["reason"]) if line["reason"] else "保留"}')
        parts.append(f'<g><title>{title}</title><rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{color}" fill-opacity="{opacity}" stroke="{color}" stroke-width="2"{dash}/><text x="{x}" y="{max(12, y - 2)}" font-size="14" fill="{color}" stroke="#151a23" stroke-width="0.4">{line["index"]}</text></g>')
    return "".join(parts) + "</svg>"


def report_html(report):
    e = html.escape
    sections = []
    for split, group in report["summary"].items():
        cards = "".join(f'<div class="metric"><strong>{e(fmt(value, key))}</strong><span>{e(key)}</span></div>' for key, value in group["metrics"].items())
        sections.append(f'<h2>{e(split_text(split))}指标</h2><div class="metrics">{cards}</div>')
        if "baseline" in group:
            before = group["baseline"]["metrics"]
            after = group["metrics"]
            keys = ("正文误删", "杂项命中", "杂项漏过", "杂项过滤精确率", "已识别杂项召回率")
            changes = "".join(f'<tr><td>{e(key)}</td><td>{e(fmt(before[key], key))}</td><td>{e(fmt(after[key], key))}</td></tr>' for key in keys)
            sections.append(f'<h3>开发集策略对照</h3><p>同一批真实识别候选，比较初版局部符号规则与根据开发集增加图形列判断后的策略。该对照不是留出效果。</p><table><thead><tr><th>指标</th><th>v1 初版</th><th>v2 当前</th></tr></thead><tbody>{changes}</tbody></table>')
    overview = []
    for scene in report["scenes"]:
        m = metrics(scene["counts"])
        overview.append(f'<tr><td><a href="#{e(scene["id"])}">{e(scene["title"])}</a></td><td>{e(split_text(scene["split"]))}</td><td>{m["正文误删"]}/{m["正文行"]}</td><td>{m["杂项命中"]}/{m["识别出的杂项行"]}</td><td>{m["杂项漏过"]}</td><td>{m["正文区域未识别"]}</td></tr>')
    sections.append(f'<h2>场景总览</h2><div class="tablewrap"><table><thead><tr><th>场景（点击复核）</th><th>集合</th><th>正文误删 / 识别行</th><th>杂项命中 / 识别行</th><th>杂项漏过</th><th>正文区域未识别</th></tr></thead><tbody>{"".join(overview)}</tbody></table></div>')
    for scene in report["scenes"]:
        rows = []
        for line in scene["lines"]:
            is_error = (line["label"] == "keep" and line["filtered"]) or (line["label"] == "drop" and not line["filtered"])
            status = "过滤" if line["filtered"] else "保留"
            if line["baseline_filtered"] is not None:
                status = ("过滤" if line["baseline_filtered"] else "保留") + " → " + status
            label = {"keep": "真实文字", "drop": "非文字杂项", "ignore": "忽略", "ambiguous": "合并歧义", "unmatched": "未匹配"}[line["label"]]
            rows.append(f'<tr class="{"error" if is_error else ""}"><td>{line["index"]}</td><td>{e(line["text"])}</td><td>{e(label)}</td><td>{status}</td><td>{e(reason_text(line["reason"]))}</td></tr>')
        omitted = "".join(f'<li>{e(r["label"])}：{e(r["text"] or r["semantic"])}</li>' for r in scene["unrecognized_regions"])
        m = metrics(scene["counts"])
        sections.append(f'<section id="{e(scene["id"])}"><h2>{e(scene["title"])}</h2><p>{e(scene["id"])} · {e(split_text(scene["split"]))} · 正文误删 {m["正文误删"]}/{m["正文行"]} · 杂项命中 {m["杂项命中"]}/{m["识别出的杂项行"]}</p><div class="pair"><figure>{image_svg(scene)}<figcaption>过滤前：绿框真实文字，黄框非文字，紫框不计分</figcaption></figure><figure>{image_svg(scene, True)}<figcaption>过滤后：虚线和填色表示被建议过滤的原始行，编号对应下表</figcaption></figure></div><details open><summary>逐行识别与过滤理由</summary><div class="tablewrap"><table><thead><tr><th>行</th><th>OCR 原文</th><th>预标注真值</th><th>决策</th><th>原因</th></tr></thead><tbody>{"".join(rows)}</tbody></table></div></details><details><summary>未识别的标注区域（不算过滤成功）</summary><ul>{omitted or "<li>无</li>"}</ul></details></section>')
    styles = """html{color-scheme:dark light}body{--bg:#121722;--panel:#1c2432;--ink:#dfe6f2;--muted:#9eafc8;--border:#35435a;background:var(--bg);color:var(--ink);font:15px/1.6 system-ui,'Microsoft YaHei',sans-serif;margin:0 auto;max-width:1500px;padding:32px}h1{font-size:28px}h2{font-size:21px}p{color:var(--muted)}.notice{border-left:3px solid #c4a05d;padding:14px 20px;background:var(--panel)}.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}.metric{background:var(--panel);padding:15px;border:1px solid var(--border);border-radius:8px}.metric strong{display:block;font-size:24px}.metric span{color:var(--muted)}section{margin-top:34px;padding-top:14px;border-top:1px solid var(--border)}.pair{display:grid;grid-template-columns:1fr 1fr;gap:16px}figure{margin:0}svg{display:block;width:100%;height:auto;border:1px solid var(--border)}figcaption{font-size:13px;color:var(--muted);margin:8px 0 16px}table{width:100%;border-collapse:collapse;text-align:left}td,th{padding:9px;border-bottom:1px solid var(--border);vertical-align:top}td:nth-child(2){min-width:280px}.error{background:#7b3c242f}.tablewrap{overflow:auto}summary{cursor:pointer;padding:10px 0}a{color:#8ebdff}@media(max-width:900px){.pair{grid-template-columns:1fr}body{padding:16px}}@media(prefers-color-scheme:light){body{--bg:#f4f6fa;--panel:#fff;--ink:#172232;--muted:#526279;--border:#c9d2df}a{color:#275ba9}}"""
    note = e(report["limitations"])
    return f'<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>OCR 动态杂项过滤评测</title><style>{styles}</style><h1>OCR 动态杂项过滤评测</h1><div class="notice">{note}</div><p>算法：{e(", ".join(report["algorithms"]))}。{e(report["freeze_note"] or "本报告只用于开发集观察。")}</p>{"".join(sections)}</html>'


def load_or_run(scene, args, corpus_root):
    path = args.raw_dir / f"{scene['id']}.json"
    if args.cli:
        command = [str(args.cli.resolve()), "--ocr-file", str((corpus_root / scene["image"]).resolve()),
                   "--ocr-provider", args.provider, "--ocr-profile", args.profile]
        if args.provider == "windows":
            command = command[:-2]
        begin = time.perf_counter()
        process = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=args.timeout)
        if process.returncode:
            raise RuntimeError(f"{scene['id']} OCR 失败（{process.returncode}）：{process.stderr[:1200]}")
        output = process.stdout.lstrip("\ufeff").strip()
        try:
            raw = json.loads(output)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"{scene['id']} CLI 未返回有效 JSON：{output[:600]}") from error
        raw["evaluation_capture"] = {"producer_note": args.producer_note, "cli": str(args.cli.resolve()),
                                     "elapsed_seconds": round(time.perf_counter() - begin, 3)}
        write_json(path, raw)
        print(f"OCR {scene['id']}: {len(raw.get('lines', []))} lines", flush=True)
        return raw
    if not path.is_file():
        raise FileNotFoundError(f"缺少已缓存 OCR：{path}，请指定 --cli")
    return read_json(path)


def self_test():
    regions = [{"id": "a", "label": "keep", "rect": [10, 10, 100, 20]},
               {"id": "b", "label": "drop", "rect": [150, 10, 30, 20]}]
    assert classify((5, 5, 110, 30), regions)[0] == "keep"
    assert classify((148, 8, 35, 25), regions)[0] == "drop"
    assert classify((5, 5, 180, 30), regions)[0] == "ambiguous"
    assert classify((300, 300, 10, 10), regions)[0] == "unmatched"
    assert metrics({"keep_lines": 5, "keep_filtered": 1, "drop_filtered": 2, "drop_retained": 3})["已识别杂项召回率"] == 0.4
    scene = {"id": "evaluation-self-test", "split": "development", "title": "evaluation", "image": "unused", "size": [500, 400], "regions": regions}
    raw = {"filter": {"lines": [], "baseline_lines": []}, "lines": [{"text": "real text", "rect": [5, 5, 110, 30]}]}
    evaluated = evaluate(scene, raw)
    assert evaluated["counts"]["drop_regions_unrecognized"] == 1
    assert metrics(evaluated["counts"])["已识别杂项召回率"] is None
    raw["lines"] = [{"text": "merged text", "rect": [5, 5, 180, 30]}]
    raw["filter"]["lines"] = [{"index": 0, "reason": "synthetic test"}]
    evaluated = evaluate(scene, raw)
    assert evaluated["counts"]["ambiguous_filtered"] == 1
    assert metrics(evaluated["counts"])["杂项命中"] == 0
    try:
        evaluate(scene, {"lines": []})
    except ValueError:
        pass
    else:
        raise AssertionError("旧识别结果不能被当作已评估的过滤结果")
    print("geometry / ambiguity / unrecognized denominator / baseline / missing-filter checks passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=ROOT / "output/ocr-filter/manifest.json")
    parser.add_argument("--out", type=Path, default=ROOT / "output/ocr-filter")
    parser.add_argument("--raw-dir", type=Path)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--split", choices=["development", "holdout", "all"], default="development")
    parser.add_argument("--freeze-note", default="")
    parser.add_argument("--producer-note", default="本次 Dev CLI 真实文件识别")
    parser.add_argument("--cache-only", action="store_true")
    parser.add_argument("--provider", choices=["ppocr", "windows"], default="ppocr")
    parser.add_argument("--profile", choices=["tiny", "small"], default="tiny")
    parser.add_argument("--timeout", type=int, default=60)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--comparison", action="store_true", help="同时生成开发命中与留出漏过的数据对照图")
    parser.add_argument("--report-font", type=Path, default=Path("C:/Windows/Fonts/msyh.ttc"))
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.split in ("holdout", "all") and not args.freeze_note:
        parser.error("留出集仅在策略冻结后评估；请用 --freeze-note 明确冻结说明")
    manifest = read_json(args.manifest)
    selected = [s for s in manifest["scenes"] if args.split == "all" or s["split"] == args.split]
    if not selected:
        parser.error("manifest 没有对应拆分的场景")
    args.out.mkdir(parents=True, exist_ok=True)
    args.raw_dir = args.raw_dir or args.out / f"raw-{args.split}"
    args.raw_dir.mkdir(parents=True, exist_ok=True)
    evaluations = []
    for scene in selected:
        raw = load_or_run(scene, args, args.manifest.parent)
        if not args.cache_only:
            evaluations.append(evaluate(scene, raw))
    if args.cache_only:
        print(json.dumps({"raw_dir": str(args.raw_dir), "scenes": len(selected), "filter_evaluated": False}, ensure_ascii=False))
        return
    report = {"version": 1, "limitations": "这些结果来自预先语义标注的程序合成场景和真实 OCR 模型推理，不是真实用户截图，也不足以证明实际准确率。正文、小字、标点及真实无关文字均应保留；只有非文字误识别才应过滤。杂项召回率的分母仅包含已识别为文本的杂项，未识别区域单独列出；合并歧义与未匹配行不混入正确率。",
              "freeze_note": args.freeze_note, "algorithms": sorted({s["algorithm"] or "unknown" for s in evaluations}),
              "annotation_policy": manifest["annotation_policy"], "split_policy": manifest["split_policy"],
              "summary": summary(evaluations), "scenes": evaluations}
    json_path = args.out / f"evaluation-{args.split}.json"
    html_path = args.out / f"report-{args.split}.html"
    write_json(json_path, report)
    html_path.write_text(report_html(report), encoding="utf-8")
    figure = comparison_figure(report, args.manifest.parent, args.out / "comparison.png", args.report_font) if args.comparison else None
    print(json.dumps({"summary": report["summary"], "report": str(html_path), "json": str(json_path), "comparison": figure}, ensure_ascii=False))


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
