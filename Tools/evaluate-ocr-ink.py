#!/usr/bin/env python3
"""离线对比 OCR 选字几何；不运行识别，不将框间变化视作墨迹覆盖率。

python Tools/evaluate-ocr-ink.py --image sample.png --before before.json \
    --after after.json --output report.html

仅使用标准库。HTML 自带原图，默认深色并跟随系统主题，不启动窗口。
lines[].selection_geometry 可由诊断测试给出；缺失时明确回退到行 geometry。
selection_reading_range 仅裁显示首尾；它不会重映射内部 u 或充当 CTC 停点证据。
"""
from __future__ import annotations

import argparse
import base64
import html
import json
import math
import mimetypes
from pathlib import Path
import statistics
import struct


def quad(value):
    """Accept the CLI geometry object and plain diagnostic quads."""
    if isinstance(value, dict):
        value = value.get("quad")
    if not isinstance(value, list) or len(value) != 4:
        return None
    try:
        result = [(float(p["x"]), float(p["y"])) if isinstance(p, dict)
                  else (float(p[0]), float(p[1])) for p in value]
    except (TypeError, KeyError, ValueError, IndexError):
        return None
    if not all(math.isfinite(n) for p in result for n in p):
        return None
    return result if area(result) > 1e-8 else None


def rect_quad(rect):
    if not isinstance(rect, dict):
        return None
    try:
        x, y, w, h = (float(rect[k]) for k in ("x", "y", "width", "height"))
    except (TypeError, ValueError, KeyError):
        return None
    if w <= 0 or h <= 0 or not all(math.isfinite(n) for n in (x, y, w, h)):
        return None
    return [(x, y), (x + w, y), (x + w, y + h), (x, y + h)]


def area(q):
    return abs(sum(q[i][0] * q[(i + 1) % 4][1] - q[(i + 1) % 4][0] * q[i][1]
                   for i in range(4))) / 2


def height(q):
    return (math.dist(q[0], q[3]) + math.dist(q[1], q[2])) / 2


def project(q, u, v):
    # Match the source-to-quad homography, including perspective quads.
    dx1, dx2 = q[1][0] - q[2][0], q[3][0] - q[2][0]
    dy1, dy2 = q[1][1] - q[2][1], q[3][1] - q[2][1]
    dx3 = q[0][0] - q[1][0] + q[2][0] - q[3][0]
    dy3 = q[0][1] - q[1][1] + q[2][1] - q[3][1]
    determinant = dx1 * dy2 - dx2 * dy1
    if abs(determinant) < 1e-8:
        return None
    g = (dx3 * dy2 - dx2 * dy3) / determinant
    h = (dx1 * dy3 - dx3 * dy1) / determinant
    denominator = g * u + h * v + 1
    if abs(denominator) < 1e-8:
        return None
    return (((q[1][0] - q[0][0] + g * q[1][0]) * u
             + (q[3][0] - q[0][0] + h * q[3][0]) * v + q[0][0]) / denominator,
            ((q[1][1] - q[0][1] + g * q[1][1]) * u
             + (q[3][1] - q[0][1] + h * q[3][1]) * v + q[0][1]) / denominator)


def center_delta(a, b):
    if not a or not b:
        return None
    pairs = [(project(a, u, .5), project(b, u, .5)) for u in (0, .25, .5, .75, 1)]
    if any(x is None or y is None for x, y in pairs):
        return None
    return max(math.dist(x, y) for x, y in pairs)


def selection(line):
    selected = quad(line.get("selection_geometry"))
    if selected:
        return selected, "selection_geometry"
    return quad(line.get("geometry")), "geometry（未提供独立选字几何）"


def reading_range(line):
    value = line.get("selection_reading_range")
    if value is None:
        return [0., 1.]
    if not isinstance(value, list) or len(value) != 2:
        raise ValueError("selection_reading_range 须为 [u0,u1]")
    try:
        lo, hi = (float(v) for v in value)
    except (TypeError, ValueError):
        raise ValueError("selection_reading_range 须为有限数值") from None
    if not math.isfinite(lo) or not math.isfinite(hi) or not 0 <= lo < hi <= 1:
        raise ValueError("selection_reading_range 须满足 0 <= u0 < u1 <= 1")
    return [lo, hi]


def displayed_quad(source, interval):
    # This quad is for drawing/area only. Never use it to map internal caret u.
    if source is None:
        return None
    lo, hi = interval
    points = [project(source, u, v) for u, v in ((lo, 0), (hi, 0), (hi, 1), (lo, 1))]
    return points if all(p is not None for p in points) else None


def reading_width(source, interval):
    if source is None:
        return None
    a, b = (project(source, u, .5) for u in interval)
    return math.dist(a, b) if a is not None and b is not None else None


def angle(source):
    if source is None:
        return None
    a, b = project(source, 0, .5), project(source, 1, .5)
    return math.degrees(math.atan2(b[1] - a[1], b[0] - a[0])) if a and b else None


def endpoint_metrics(a, b, ra, rb):
    output = {"before_display_width": reading_width(a, ra),
              "after_display_width": reading_width(b, rb),
              "width_reduction": None, "start_endpoint_inward_px": None,
              "end_endpoint_inward_px": None, "start_endpoint_inward_x_px": None,
              "end_endpoint_inward_x_px": None, "start_endpoint_shift_2d_px": None,
              "end_endpoint_shift_2d_px": None, "horizontal_width_reduction": None}
    if a is None or b is None:
        return output
    origin, end = project(a, 0, .5), project(a, 1, .5)
    start_a, end_a = (project(a, u, .5) for u in ra)
    start_b, end_b = (project(b, u, .5) for u in rb)
    if any(p is None for p in (origin, end, start_a, end_a, start_b, end_b)):
        return output
    length = math.dist(origin, end)
    if length <= 1e-8:
        return output
    tangent = [(end[k] - origin[k]) / length for k in (0, 1)]
    output.update({"width_reduction": 1 - output["after_display_width"] / output["before_display_width"],
                   "start_endpoint_inward_px": sum((start_b[k] - start_a[k]) * tangent[k] for k in (0, 1)),
                   "end_endpoint_inward_px": sum((end_a[k] - end_b[k]) * tangent[k] for k in (0, 1)),
                   "start_endpoint_shift_2d_px": math.dist(start_a, start_b),
                   "end_endpoint_shift_2d_px": math.dist(end_a, end_b)})
    horizontal_width = abs(end_a[0] - start_a[0])
    if horizontal_width > 1e-8:
        sign = 1 if end_a[0] > start_a[0] else -1
        output.update({"horizontal_width_reduction": 1 - abs(end_b[0] - start_b[0]) / horizontal_width,
                       "start_endpoint_inward_x_px": (start_b[0] - start_a[0]) * sign,
                       "end_endpoint_inward_x_px": (end_a[0] - end_b[0]) * sign})
    return output


def cell_evidence(line):
    values = line.get("selection_cells")
    if not isinstance(values, list):
        return None
    cells = []
    for i, value in enumerate(values):
        if not isinstance(value, dict):
            return None
        q = quad(value.get("quad")) or quad(value.get("geometry"))
        span = value.get("range")
        if q is None or not isinstance(span, list) or len(span) != 2:
            return None
        cells.append({"range": span, "quad": q})
    return cells


def internal_boundaries(old, new):
    a, b = cell_evidence(old), cell_evidence(new)
    if a is None or b is None:
        return {"status": "unavailable", "reason": "缺少真实 selection_cells，无法证明内部逐字边界不变"}
    if len(a) != len(b) or [c["range"] for c in a] != [c["range"] for c in b]:
        return {"status": "unpaired", "reason": "前后消费单元数量或正文范围不同，不能配对内部边界"}
    if len(a) < 2:
        return {"status": "not_applicable", "reason": "没有两个以上实际选字单元，无内部边界"}
    edges = []
    for i, (ca, cb) in enumerate(zip(a, b)):
        for side, indices in (("start", (0, 3)), ("end", (1, 2))):
            if side == "start" and i == 0 or side == "end" and i == len(a) - 1:
                continue
            pa, pb = [[c["quad"][j] for j in indices] for c in (ca, cb)]
            ma, mb = [[sum(p[k] for p in points) / 2 for k in (0, 1)] for points in (pa, pb)]
            edges.append({"cell_range": ca["range"], "side": side,
                          "midpoint_delta_x_px": mb[0] - ma[0], "midpoint_delta_y_px": mb[1] - ma[1],
                          "midpoint_shift_2d_px": math.dist(ma, mb),
                          "corner_max_delta_x_px": max(abs(y[0] - x[0]) for x, y in zip(pa, pb)),
                          "corner_max_shift_2d_px": max(math.dist(x, y) for x, y in zip(pa, pb))})
    return {"status": "available", "cell_count": len(a), "internal_edge_count": len(edges),
            "max_midpoint_delta_x_px": max(abs(e["midpoint_delta_x_px"]) for e in edges),
            "max_midpoint_shift_2d_px": max(e["midpoint_shift_2d_px"] for e in edges),
            "max_corner_delta_x_px": max(e["corner_max_delta_x_px"] for e in edges),
            "max_corner_shift_2d_px": max(e["corner_max_shift_2d_px"] for e in edges),
            "selection_rectified": bool(new.get("selection_rectified", False)), "edges": edges}


def image_size(data):
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return struct.unpack(">II", data[16:24])
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return struct.unpack("<HH", data[6:10])
    if data[:2] == b"\xff\xd8":
        i = 2
        while i + 8 < len(data):
            if data[i] != 0xff:
                i += 1
                continue
            while i < len(data) and data[i] == 0xff:
                i += 1
            marker = data[i]
            i += 1
            if marker in (0xd8, 0xd9, 0x01) or 0xd0 <= marker <= 0xd7:
                continue
            length = int.from_bytes(data[i:i + 2], "big")
            if marker in (0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf):
                h, w = struct.unpack(">HH", data[i + 3:i + 7])
                return w, h
            if length < 2:
                break
            i += length
    raise ValueError("原图须为有效 PNG、JPEG 或 GIF；脚本不依赖图片库")


def polygon(q, color):
    if not q:
        return ""
    points = " ".join(f"{x:.4f},{y:.4f}" for x, y in q)
    return f'<polygon points="{points}" fill="{color}" fill-opacity=".04" stroke="{color}" stroke-width="1" vector-effect="non-scaling-stroke"/>'


def picture(width, image_height, overlays, crop=None):
    if crop:
        points = [p for q in crop if q for p in q]
        if points:
            x = max(0, min(p[0] for p in points) - 5)
            y = max(0, min(p[1] for p in points) - 5)
            r = min(width, max(p[0] for p in points) + 5)
            b = min(image_height, max(p[1] for p in points) + 5)
            view = (x, y, max(1, r - x), max(1, b - y))
        else:
            view = (0, 0, width, image_height)
    else:
        view = (0, 0, width, image_height)
    value = " ".join(f"{n:.4f}" for n in view)
    return (f'<svg viewBox="{value}" role="img" aria-label="OCR 几何叠图">'
            '<use href="#source-image"/>' + overlays + '</svg>')


def fmt(value, suffix=""):
    return "未提供" if value is None else f"{value:.3f}{suffix}"


def metric(a, b):
    if not a or not b:
        return {"before_height": None, "after_height": None, "height_reduction": None,
                "area_reduction": None, "centerline_max_shift_px": None}
    return {"before_height": height(a), "after_height": height(b),
            "height_reduction": 1 - height(b) / height(a),
            "area_reduction": 1 - area(b) / area(a),
            "centerline_max_shift_px": center_delta(a, b)}


def run(args):
    summary_path = args.output.with_suffix(".json")
    inputs = {p.resolve() for p in (args.image, args.before, args.after)}
    if args.output.resolve() in inputs or summary_path.resolve() in inputs:
        raise ValueError("报告或摘要路径与输入文件相同，请换一个 --output 文件名")
    if args.output.resolve() == summary_path.resolve():
        raise ValueError("--output 应为 HTML 文件，不能使用 .json 后缀")
    before = json.loads(args.before.read_text(encoding="utf-8-sig"))
    after = json.loads(args.after.read_text(encoding="utf-8-sig"))
    if not isinstance(before, dict) or not isinstance(after, dict):
        raise ValueError("before/after 须为 CLI OCR JSON 对象")
    image_data = args.image.read_bytes()
    width, image_height = image_size(image_data)
    mime = mimetypes.guess_type(args.image.name)[0] or "image/png"
    image_uri = f"data:{mime};base64," + base64.b64encode(image_data).decode("ascii")
    old_lines, new_lines = before.get("lines", []), after.get("lines", [])
    old_words, new_words = before.get("words", []), after.get("words", [])
    summary = {
        "measurement": "OCR 几何变化；没有独立墨迹真值，不测像素覆盖率",
        "comparison_note": args.comparison_note,
        "image_size": [width, image_height],
        "text_unchanged": before.get("text") == after.get("text"),
        "line_count_unchanged": len(old_lines) == len(new_lines),
        "word_count_unchanged": len(old_words) == len(new_words),
        "word_text_and_line_unchanged": [(w.get("text"), w.get("line")) for w in old_words]
        == [(w.get("text"), w.get("line")) for w in new_words],
        "word_rects_unchanged": [w.get("rect") for w in old_words] == [w.get("rect") for w in new_words],
        "word_geometry_missing_before": sum(quad(w.get("geometry")) is None for w in old_words),
        "word_geometry_missing_after": sum(quad(w.get("geometry")) is None for w in new_words),
        "lines": [], "words": [],
    }
    all_overlays, cards = [], []
    for i in range(max(len(old_lines), len(new_lines))):
        old = old_lines[i] if i < len(old_lines) else {}
        new = new_lines[i] if i < len(new_lines) else {}
        raw = quad(old.get("geometry")) or rect_quad(old.get("rect"))
        a, source_a = selection(old)
        b, source_b = selection(new)
        ra, rb = reading_range(old), reading_range(new)
        da, db = displayed_quad(a, ra), displayed_quad(b, rb)
        endpoints = endpoint_metrics(a, b, ra, rb)
        internal = internal_boundaries(old, new)
        measured = metric(a, b)
        measured["area_reduction"] = 1 - area(db) / area(da) if da and db else None
        item = {"index": i, "before_text": old.get("text"), "after_text": new.get("text"),
                "before_geometry_source": source_a, "after_geometry_source": source_b,
                "before_selection_reading_range": ra, "after_selection_reading_range": rb,
                "before_display_quad": da, "after_display_quad": db,
                "selection_geometry_changed": da != db, "selection_mapping_changed": a != b,
                "selection_reading_range_changed": ra != rb,
                "original_centerline_angle_degrees": angle(raw),
                "before_selection_angle_degrees": angle(a), "after_selection_angle_degrees": angle(b),
                "before_selection_rectified": bool(old.get("selection_rectified", False)),
                "after_selection_rectified": bool(new.get("selection_rectified", False)),
                "internal_boundary_evidence": internal,
                "text_unchanged": old.get("text") == new.get("text"), **measured, **endpoints}
        summary["lines"].append(item)
        overlay = polygon(raw, "#ff6868") + polygon(da, "#57a6ff") + polygon(db, "#45d68b")
        for line, color in ((old, "#57a6ff"), (new, "#45d68b")):
            for cell in cell_evidence(line) or []:
                overlay += polygon(cell["quad"], color)
        all_overlays.append(overlay)
        text = html.escape(str(new.get("text", old.get("text", "缺失行"))))
        text_diff = "" if item["text_unchanged"] else '<strong class="bad">前后文字不同，不能直接视为同一行</strong>'
        rows = []
        for j in range(max(len(old_words), len(new_words))):
            wa = old_words[j] if j < len(old_words) else {}
            wb = new_words[j] if j < len(new_words) else {}
            if wa.get("line", wb.get("line")) != i:
                continue
            qa, qb = quad(wa.get("geometry")), quad(wb.get("geometry"))
            wm = metric(qa, qb)
            word_info = {"index": j, "line": i, "before_text": wa.get("text"),
                         "after_text": wb.get("text"), "rect_unchanged": wa.get("rect") == wb.get("rect"),
                         "text_unchanged": wa.get("text") == wb.get("text"), **wm}
            summary["words"].append(word_info)
            wr = rect_quad(wa.get("rect"))
            word_overlay = polygon(wr, "#ff6868") + polygon(qa, "#57a6ff") + polygon(qb, "#45d68b")
            preview = '<details><summary>查看词</summary>' + picture(width, image_height, word_overlay, [wr, qa, qb]) + '</details>'
            rows.append(f'<tr><td>{j + 1}</td><td>{html.escape(str(wb.get("text", wa.get("text", ""))))}</td>'
                        f'<td>{"是" if word_info["rect_unchanged"] else "否"}</td>'
                        f'<td>{fmt(wm["centerline_max_shift_px"], " px")}</td><td>{preview}</td></tr>')
        table = ('<table><thead><tr><th>词</th><th>文字</th><th>rect 不变</th><th>中心线最大位移</th><th>词几何</th></tr></thead>'
                 '<tbody>' + ''.join(rows) + '</tbody></table>')
        reduction = measured["height_reduction"]
        if internal["status"] == "available":
            internal_text = (f'真实消费单元 {internal["cell_count"]} 个；内部截面中点最大 Δx '
                             f'{fmt(internal["max_midpoint_delta_x_px"], " px")}，二维位移 '
                             f'{fmt(internal["max_midpoint_shift_2d_px"], " px")}；'
                             f'内部边角最大 Δx {fmt(internal["max_corner_delta_x_px"], " px")}，'
                             f'二维位移 {fmt(internal["max_corner_shift_2d_px"], " px")}。')
        else:
            internal_text = html.escape(internal["reason"])
        cards.append(f'<details class="line"><summary>第 {i + 1} 行 · {text} · 高度收缩 '
                     f'{fmt(None if reduction is None else reduction * 100, "%")} · 宽度收缩 '
                     f'{fmt(None if endpoints["width_reduction"] is None else endpoints["width_reduction"] * 100, "%")}</summary>{text_diff}'
                     f'<p>选字几何来源：前 {html.escape(source_a)}；后 {html.escape(source_b)}。'
                     f'高度 {fmt(measured["before_height"])} → {fmt(measured["after_height"])} px；'
                     f'原 u 映射中心线抽样位移 {fmt(measured["centerline_max_shift_px"], " px")}。</p>'
                     f'<p>显示范围 {html.escape(str(ra))} → {html.escape(str(rb))}；首端内移 '
                     f'{fmt(endpoints["start_endpoint_inward_px"], " px")}，尾端内移 '
                     f'{fmt(endpoints["end_endpoint_inward_px"], " px")}（沿原阅读轴，负值为外扩）。'
                     f'仅横向内移为 {fmt(endpoints["start_endpoint_inward_x_px"], " px")} / '
                     f'{fmt(endpoints["end_endpoint_inward_x_px"], " px")}。'
                     f'中心线角度 {fmt(angle(a), "°")} → {fmt(angle(b), "°")}；'
                     f'候选拉平标记：{"是" if item["after_selection_rectified"] else "否"}。</p>'
                     f'<p>{internal_text}</p>'
                     + picture(width, image_height, overlay, [raw, da, db])
                     + '<p>图中细分框来自真实 selection_cells（若有）；下方词图仅检查提供者词几何，不能替代逐字消费单元。</p>'
                     + table + '</details>')
    heights = [v["height_reduction"] for v in summary["lines"] if v["height_reduction"] is not None]
    areas = [v["area_reduction"] for v in summary["lines"] if v["area_reduction"] is not None]
    shifts = [v["centerline_max_shift_px"] for v in summary["words"] if v["centerline_max_shift_px"] is not None]
    widths = [v["width_reduction"] for v in summary["lines"] if v["width_reduction"] is not None]
    start_moves = [v["start_endpoint_inward_px"] for v in summary["lines"] if v["start_endpoint_inward_px"] is not None]
    end_moves = [v["end_endpoint_inward_px"] for v in summary["lines"] if v["end_endpoint_inward_px"] is not None]
    internals = [v["internal_boundary_evidence"] for v in summary["lines"]
                 if v["internal_boundary_evidence"]["status"] == "available"]
    summary.update({"measured_lines": len(heights),
                    "changed_selection_lines": sum(v["selection_geometry_changed"] for v in summary["lines"]),
                    "changed_height_lines": sum(abs(v) > 1e-6 for v in heights),
                    "line_text_mismatch_count": sum(not v["text_unchanged"] for v in summary["lines"]),
                    "median_line_height_reduction": statistics.median(heights) if heights else None,
                    "median_line_area_reduction": statistics.median(areas) if areas else None,
                    "median_line_width_reduction": statistics.median(widths) if widths else None,
                    "changed_reading_range_lines": sum(v["selection_reading_range_changed"] for v in summary["lines"]),
                    "median_start_endpoint_inward_px": statistics.median(start_moves) if start_moves else None,
                    "median_end_endpoint_inward_px": statistics.median(end_moves) if end_moves else None,
                    "rectified_lines_after": sum(v["after_selection_rectified"] for v in summary["lines"]),
                    "internal_boundary_provenance": "仅比较真实 selection_cells 的内部截面；缺失时不可证。词框/原映射抽样不是字素停点证据。",
                    "internal_boundary_evidence_lines": len(internals),
                    "internal_boundary_max_midpoint_delta_x_px": max((v["max_midpoint_delta_x_px"] for v in internals), default=None),
                    "internal_boundary_max_midpoint_shift_2d_px": max((v["max_midpoint_shift_2d_px"] for v in internals), default=None),
                    "word_centerline_max_shift_px": max(shifts) if shifts else None,
                    "word_centerline_sample_u": [0, .25, .5, .75, 1],
                    "word_centerline_shift_over_0_05px": sum(v > .05 for v in shifts)})
    checks = [("全文不变", summary["text_unchanged"]), ("行数不变", summary["line_count_unchanged"]),
              ("词数不变", summary["word_count_unchanged"]),
              ("词文字及所属行不变", summary["word_text_and_line_unchanged"]),
              ("词 rect 不变", summary["word_rects_unchanged"])]
    checks_html = ''.join(f'<li class="{"good" if ok else "bad"}">{label}：{"是" if ok else "否"}</li>' for label, ok in checks)
    document = '''<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>OCR 选字几何对比</title><style>
:root{color-scheme:dark light;--bg:#10151d;--panel:#18202b;--text:#e3eaf4;--border:#394657;--muted:#a6b4c8}
@media(prefers-color-scheme:light){:root{--bg:#f2f4f7;--panel:#fff;--text:#172333;--border:#c7d0dd;--muted:#526479}}
*{box-sizing:border-box}body{margin:0;padding:24px;background:var(--bg);color:var(--text);font:14px/1.6 system-ui,"Microsoft YaHei",sans-serif}
main{max-width:1400px;margin:auto}h1{font-size:24px}p{color:var(--muted)}svg{display:block;width:100%;max-height:620px;background:#222a34;margin:12px 0;border:1px solid var(--border)}
.line{padding:12px;margin:12px 0;background:var(--panel);border:1px solid var(--border);border-radius:8px}.line>summary{cursor:pointer;font-weight:600;overflow-wrap:anywhere}
table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid var(--border);text-align:left;padding:7px}td:last-child{width:45%}td svg{min-height:70px;max-height:180px}.good{color:#45d68b}.bad{color:#ff6868}
.legend span{white-space:nowrap;margin-right:20px}.note{padding:12px;background:var(--panel);border-left:3px solid #d4ac59}a{color:#57a6ff}</style><main><h1>OCR 选字几何对比</h1>'''
    document += (f'<p class="note">对照口径：{html.escape(args.comparison_note)}</p>'
                 '<p class="note">这里只测几何变化，没有独立文字像素真值，不能据此声称墨迹覆盖率、定位准确率或识别准确率。'
                 '前后行／词按原索引配对；若内容或顺序改变，应先核对配对。中心线在原 u=0、¼、½、¾、1，v=½ 采样，不能代替全部 CTC 字素停点检查。'
                 '显示首尾使用 selection_reading_range 裁边，绘出的截短四边形仅供显示和面积统计，不重映射内部 u。'
                 '内部边界须有真实 selection_cells 才可比较；拉平后同时报告横向 Δx 和二维差，不能把正常高度变化造成的边角位移冒充停点变化。</p>'
                 '<p class="legend"><span style="color:#ff6868">红：原行框（词视图为原 rect）</span>'
                 '<span style="color:#57a6ff">蓝：优化前选字几何</span><span style="color:#45d68b">绿：优化后选字几何</span></p>'
                 f'<ul>{checks_html}</ul><p>有双侧几何的配对行 {len(heights)}，几何变化行 {summary["changed_selection_lines"]}，'
                 f'高度变化行 {summary["changed_height_lines"]}；'
                 f'行高中位收缩 {fmt(None if not heights else statistics.median(heights) * 100, "%")}；'
                 f'显示宽度中位收缩 {fmt(None if not widths else statistics.median(widths) * 100, "%")}；'
                 f'面积中位收缩 {fmt(None if not areas else statistics.median(areas) * 100, "%")}；'
                 f'词中心线最大位移 {fmt(summary["word_centerline_max_shift_px"], " px")}。'
                 f'缺失词几何：前 {summary["word_geometry_missing_before"]}，后 {summary["word_geometry_missing_after"]}。</p>'
                 f'<p>端部范围变化 {summary["changed_reading_range_lines"]} 行，候选拉平 {summary["rectified_lines_after"]} 行。'
                 f'有实际内部单元证据 {len(internals)} 行；内部截面中点最大 Δx '
                 f'{fmt(summary["internal_boundary_max_midpoint_delta_x_px"], " px")}，二维位移 '
                 f'{fmt(summary["internal_boundary_max_midpoint_shift_2d_px"], " px")}。'
                 '缺失证据的行不计为通过。</p>')
    document += (f'<svg width="0" height="0" style="position:absolute;width:0;height:0;border:0" aria-hidden="true"><defs>'
                 f'<image id="source-image" href="{image_uri}" width="{width}" height="{image_height}"/></defs></svg>'
                 + picture(width, image_height, ''.join(all_overlays))
                 + '<p>逐行展开可查看放大叠图和逐词几何；所有内容已嵌入，可离线浏览。</p>'
                 + ''.join(cards) + '</main></html>')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(document, encoding="utf-8")
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    print(json.dumps({"html": str(args.output.resolve()), "summary": str(summary_path.resolve()),
                      "measured_lines": len(heights), "text_unchanged": summary["text_unchanged"]}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("image", "before", "after", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--comparison-note", default="输入 JSON 的几何对照；未记录推理是否来自同一次识别。",
                        help="写入 HTML 和 JSON 的对照来源说明，不自动推断模型是否重跑")
    args = parser.parse_args()
    try:
        run(args)
    except (OSError, ValueError, TypeError, KeyError, struct.error) as error:
        parser.exit(1, f"评估失败：{error}\n")


if __name__ == "__main__":
    main()
