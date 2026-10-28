/* 长图前端原型的绘制演示。生产接入复用现有 Annotation / Scene，
 * 不把这里的 SVG 或低分辨率效果缓存带入原生文档。全部坐标均为原图像素。
 */
(() => {
  "use strict";
  const EFFECTS = new Set([
    "mosaic",
    "blur",
    "mosaicBrush",
    "blurBrush",
    "localFill",
    "pixelEraser",
  ]);
  const BRUSHES = new Set([
    "mosaicBrush",
    "blurBrush",
    "localFill",
    "pixelEraser",
  ]);
  const PATHS = new Set(["pen", "polyline", "highlighter"]);
  const number = (value, fallback = 0) =>
    Number.isFinite(Number(value)) ? Number(value) : fallback;
  const limit = (value, min, max) => Math.min(max, Math.max(min, value));
  const round = (value) => Math.round(number(value) * 1000) / 1000;
  const xml = (value) =>
    String(value ?? "").replace(
      /[&<>"']/g,
      (c) =>
        ({
          "&": "&amp;",
          "<": "&lt;",
          ">": "&gt;",
          '"': "&quot;",
          "'": "&#39;",
        })[c],
    );
  const color = (value, fallback = "#ff8b7b") =>
    /^(#[\da-f]{3,8}|rgba?\([\d\s.,%]+\)|hsla?\([\d\s.,%+-]+\)|transparent|none|white|black)$/i.test(
      String(value),
    )
      ? String(value)
      : fallback;
  const alpha = (value, fallback = 1) => {
    const n = number(value, fallback);
    return limit(n > 1 ? n / 100 : n, 0, 1);
  };
  const styleOf = (a) => a.style || {};
  const textOf = (a) => String(styleOf(a).text ?? a.text ?? "");
  const fontSize = (a) => limit(number(styleOf(a).fontSize, 32), 1, 2048);
  const brushWidth = (a) =>
    Math.max(0.25, number(styleOf(a).brushSize ?? styleOf(a).width, 32));
  const pointsOf = (a) =>
    (Array.isArray(a.points) && a.points.length
      ? a.points.slice(0, 20000)
      : [
          { x: 0, y: 0 },
          { x: number(a.w), y: number(a.h) },
        ]
    ).map((p) => ({
      x: number(a.x) + number(p.x),
      y: number(a.y) + number(p.y),
    }));

  function textMetrics(a) {
    const size = fontSize(a),
      lines = textOf(a).split(/\r?\n/);
    const estimate = Math.max(
      1,
      ...lines.map((line) =>
        [...line].reduce(
          (w, c) => w + (/[^\u0000-\u00ff]/.test(c) ? size : size * 0.58),
          0,
        ),
      ),
    );
    return {
      lines,
      size,
      lineHeight: size * 1.3,
      width: Math.max(estimate, number(a.w) > 0 ? number(a.w) : 0),
    };
  }

  function box(a) {
    const x = number(a.x),
      y = number(a.y),
      w = number(a.w),
      h = number(a.h);
    if (a.type === "text") {
      const m = textMetrics(a);
      return {
        x,
        y: y - m.size,
        w: m.width,
        h: Math.max(m.size * 1.3, m.lines.length * m.lineHeight),
      };
    }
    if (a.type === "serial") {
      const radius = Math.max(14, fontSize(a) * 0.72);
      return { x: x - radius, y: y - radius, w: radius * 2, h: radius * 2 };
    }
    if (
      (PATHS.has(a.type) || BRUSHES.has(a.type)) &&
      Array.isArray(a.points) &&
      a.points.length
    ) {
      const p = pointsOf(a),
        pad = BRUSHES.has(a.type) ? brushWidth(a) / 2 : 0;
      const left = Math.min(...p.map((q) => q.x)),
        top = Math.min(...p.map((q) => q.y));
      return {
        x: left - pad,
        y: top - pad,
        w: Math.max(0.25, Math.max(...p.map((q) => q.x)) - left) + pad * 2,
        h: Math.max(0.25, Math.max(...p.map((q) => q.y)) - top) + pad * 2,
      };
    }
    if (a.type === "arrow" && a.control) {
      const xs = [x, x + w, x + number(a.control.x)],
        ys = [y, y + h, y + number(a.control.y)];
      return {
        x: Math.min(...xs),
        y: Math.min(...ys),
        w: Math.max(...xs) - Math.min(...xs),
        h: Math.max(...ys) - Math.min(...ys),
      };
    }
    return {
      x: Math.min(x, x + w),
      y: Math.min(y, y + h),
      w: Math.abs(w),
      h: Math.abs(h),
    };
  }

  function pathData(points, smooth = false, closed = false) {
    if (!points.length) return "";
    const p = points.map((q) => ({ x: round(q.x), y: round(q.y) }));
    if (p.length === 1) return `M${p[0].x} ${p[0].y}l.01 0`;
    if (!smooth || p.length < 3)
      return (
        `M${p[0].x} ${p[0].y}` +
        p
          .slice(1)
          .map((q) => `L${q.x} ${q.y}`)
          .join("") +
        (closed ? "Z" : "")
      );
    let result = `M${p[0].x} ${p[0].y}`;
    for (let i = 1; i < p.length - 1; i++) {
      const middle = {
        x: (p[i].x + p[i + 1].x) / 2,
        y: (p[i].y + p[i + 1].y) / 2,
      };
      result += `Q${p[i].x} ${p[i].y} ${round(middle.x)} ${round(middle.y)}`;
    }
    result += `L${p[p.length - 1].x} ${p[p.length - 1].y}`;
    return result + (closed ? "Z" : "");
  }

  function roundedPath(b, radii) {
    const x = round(b.x),
      y = round(b.y),
      w = Math.max(0, b.w),
      h = Math.max(0, b.h);
    const r = radii.map((v) => limit(number(v), 0, Math.min(w, h) / 2));
    return `M${x + r[0]} ${y}H${x + w - r[1]}Q${x + w} ${y} ${x + w} ${y + r[1]}V${y + h - r[2]}Q${x + w} ${y + h} ${x + w - r[2]} ${y + h}H${x + r[3]}Q${x} ${y + h} ${x} ${y + h - r[3]}V${y + r[0]}Q${x} ${y} ${x + r[0]} ${y}Z`;
  }
  function radiusValues(s, effect = false) {
    const r = number(effect ? (s.effectRadius ?? s.radius) : s.radius);
    return s.independentCorners && !effect
      ? [
          s.radiusTopLeft,
          s.radiusTopRight,
          s.radiusBottomRight,
          s.radiusBottomLeft,
        ].map((v) => number(v, r))
      : [r, r, r, r];
  }
  function dashAttributes(s, width) {
    const mode = s.lineStyle || (s.dashed ? "dashed" : "solid");
    return mode === "dotted" || mode === "dot"
      ? ` stroke-dasharray="${Math.max(0.01, width * 0.08)} ${width * 2.1}"`
      : mode === "dashed" || mode === "dash"
        ? ` stroke-dasharray="${width * 4} ${width * 2.5}"`
        : "";
  }
  function rotation(a, b, override) {
    const degrees = number(override ?? a.rotation);
    return degrees
      ? ` transform="rotate(${round(degrees)} ${round(b.x + b.w / 2)} ${round(b.y + b.h / 2)})"`
      : "";
  }
  function shapePaint(s) {
    const c = color(s.color),
      width = Math.max(0.25, number(s.width, 4));
    const mode = s.fillMode || (s.filled ? "fill" : "stroke");
    return `fill="${mode === "stroke" || mode === "none" ? "none" : c}" stroke="${mode === "fill" ? "none" : c}" stroke-width="${width}" stroke-linejoin="round"${dashAttributes(s, width)}`;
  }
  function polygonPoints(type, b) {
    if (type === "triangle")
      return [
        { x: b.x + b.w / 2, y: b.y },
        { x: b.x + b.w, y: b.y + b.h },
        { x: b.x, y: b.y + b.h },
      ];
    if (type === "diamond")
      return [
        { x: b.x + b.w / 2, y: b.y },
        { x: b.x + b.w, y: b.y + b.h / 2 },
        { x: b.x + b.w / 2, y: b.y + b.h },
        { x: b.x, y: b.y + b.h / 2 },
      ];
    return Array.from({ length: 10 }, (_, i) => {
      const angle = -Math.PI / 2 + (i * Math.PI) / 5,
        r = i % 2 ? 0.43 : 1;
      return {
        x: b.x + b.w / 2 + ((Math.cos(angle) * b.w) / 2) * r,
        y: b.y + b.h / 2 + ((Math.sin(angle) * b.h) / 2) * r,
      };
    });
  }

  function arrowMarkup(a) {
    const s = styleOf(a),
      width = Math.max(0.25, number(s.width, 4)),
      c = color(s.color);
    const start = { x: number(a.x), y: number(a.y) },
      end = { x: start.x + number(a.w), y: start.y + number(a.h) };
    const control = a.control
      ? { x: start.x + number(a.control.x), y: start.y + number(a.control.y) }
      : null;
    const d = control
      ? `M${start.x} ${start.y}Q${control.x} ${control.y} ${end.x} ${end.y}`
      : `M${start.x} ${start.y}L${end.x} ${end.y}`;
    let result = `<path d="${d}" fill="none" stroke="${c}" stroke-width="${width}" stroke-linecap="round"${dashAttributes(s, width)}/>`;
    const head = (tip, other) => {
      const angle = Math.atan2(tip.y - other.y, tip.x - other.x),
        length = Math.max(13, width * 4.4),
        spread = 0.48;
      const left = {
          x: tip.x - Math.cos(angle - spread) * length,
          y: tip.y - Math.sin(angle - spread) * length,
        },
        right = {
          x: tip.x - Math.cos(angle + spread) * length,
          y: tip.y - Math.sin(angle + spread) * length,
        };
      const open = !s.arrowHead || s.arrowHead === "open";
      if (open)
        return `<path d="M${left.x} ${left.y}L${tip.x} ${tip.y}L${right.x} ${right.y}" fill="none" stroke="${c}" stroke-width="${width}" stroke-linejoin="round" stroke-linecap="round"/>`;
      const notch =
        s.arrowHead === "dart"
          ? `L${tip.x - Math.cos(angle) * length * 0.58} ${tip.y - Math.sin(angle) * length * 0.58}`
          : "";
      return `<path d="M${tip.x} ${tip.y}L${left.x} ${left.y}${notch}L${right.x} ${right.y}Z" fill="${c}"/>`;
    };
    if (s.arrowStart && s.arrowStart !== "none")
      result += head(start, control || end);
    if (s.arrowEnd !== false && s.arrowEnd !== "none")
      result += head(end, control || start);
    return result;
  }

  function textMarkup(a) {
    const s = styleOf(a),
      b = box(a),
      m = textMetrics(a),
      pad = Math.max(4, m.size * 0.16),
      alignment = s.textAlignment || s.align || "left";
    const anchor =
      alignment === "center"
        ? "middle"
        : alignment === "right"
          ? "end"
          : "start";
    const x =
      alignment === "center"
        ? b.x + b.w / 2
        : alignment === "right"
          ? b.x + b.w
          : b.x;
    let result = "";
    if (s.textBackground)
      result += `<rect x="${b.x - pad}" y="${b.y - pad}" width="${b.w + pad * 2}" height="${b.h + pad * 2}" rx="${Math.max(0, number(s.textBackgroundRadius, 4))}" fill="${color(s.textBackgroundColor, "#ffffff")}" fill-opacity="${alpha(s.textBackgroundOpacity, 0.85)}"/>`;
    const decoration = [s.underline && "underline", s.strike && "line-through"]
      .filter(Boolean)
      .join(" ");
    const outline = s.textOutline
      ? ` stroke="${color(s.textOutlineColor, "#ffffff")}" stroke-width="${Math.max(0, number(s.textOutlineWidth, 2))}" paint-order="stroke fill" stroke-linejoin="round"`
      : "";
    result += `<text x="${round(x)}" y="${number(a.y)}" text-anchor="${anchor}" fill="${color(s.color)}" font-family="${xml(s.fontFamily || "Segoe UI,Microsoft YaHei UI,sans-serif")}" font-size="${m.size}" font-weight="${s.bold ? 700 : 400}" font-style="${s.italic ? "italic" : "normal"}"${decoration ? ` text-decoration="${decoration}"` : ""}${outline}>`;
    result += m.lines
      .map(
        (line, i) =>
          `<tspan x="${round(x)}" y="${round(number(a.y) + i * m.lineHeight)}">${xml(line) || "&#160;"}</tspan>`,
      )
      .join("");
    return result + "</text>";
  }

  function spotlightMarkup(a, context, id) {
    const s = styleOf(a),
      b = box(a),
      width = Math.max(0, number(s.width, 2));
    const fullWidth = Math.max(1, number(context.sourceWidth, b.x + b.w)),
      fullHeight = Math.max(1, number(context.sourceHeight, b.y + b.h));
    const hole = roundedPath(b, radiusValues(s)),
      transform = rotation(a, b);
    return `<defs><mask id="${id}-spot" maskUnits="userSpaceOnUse" x="0" y="0" width="${fullWidth}" height="${fullHeight}"><rect width="${fullWidth}" height="${fullHeight}" fill="white"/><path d="${hole}" fill="black"${transform}/></mask></defs><rect width="${fullWidth}" height="${fullHeight}" fill="black" opacity="${alpha(s.opacity ?? s.dim, 0.5)}" mask="url(#${id}-spot)"/>${width ? `<path d="${hole}" fill="none" stroke="${color(s.color, "#ffffff")}" stroke-width="${width}"${transform}/>` : ""}`;
  }

  function watermarkMarkup(a, context, id) {
    const s = styleOf(a),
      text = textOf(a);
    if (!text) return "";
    const size = fontSize(a),
      tileWidth = Math.max(size * 4, [...text].length * size * 0.9 + size * 2),
      tileHeight = size * 4;
    const width = Math.max(1, number(context.sourceWidth, number(a.w, 1080))),
      height = Math.max(1, number(context.sourceHeight, number(a.h, 4320)));
    const angle = number(s.watermarkAngle ?? a.rotation);
    return `<defs><pattern id="${id}-water" width="${tileWidth}" height="${tileHeight}" patternUnits="userSpaceOnUse" patternTransform="rotate(${angle} ${width / 2} ${height / 2})"><text x="${size}" y="${tileHeight / 2}" fill="${color(s.color)}" font-family="${xml(s.fontFamily || "Segoe UI,Microsoft YaHei UI,sans-serif")}" font-size="${size}" font-weight="${s.bold ? 700 : 400}">${xml(text)}</text></pattern></defs><rect width="${width}" height="${height}" fill="url(#${id}-water)" opacity="${alpha(s.opacity, 0.15)}"/>`;
  }

  function effectMarkup(a, id) {
    const s = styleOf(a),
      b = box(a),
      brush = BRUSHES.has(a.type) || s.region === "brush";
    const geometry = brush
      ? `<path d="${pathData(pointsOf(a), s.pencilSmooth || s.smooth)}" fill="none" stroke="white" stroke-width="${brushWidth(a)}" stroke-linecap="round" stroke-linejoin="round"/>`
      : `<path d="${roundedPath(b, radiusValues(s, true))}" fill="white"/>`;
    const patch = /^data:image\/(png|jpeg|webp);base64,/i.test(a.patch || "")
      ? a.patch
      : null;
    if (!patch)
      return brush
        ? `<path d="${pathData(pointsOf(a))}" fill="none" stroke="#738980" stroke-opacity=".4" stroke-width="${brushWidth(a)}" stroke-linecap="round"/>`
        : `<path d="${roundedPath(b, radiusValues(s, true))}" fill="#738980" fill-opacity=".4"/>`;
    const p = a.patchBounds || { x: 0, y: 0, w: b.w, h: b.h };
    const patchRect = {
      x: b.x + number(p.x),
      y: b.y + number(p.y),
      w: Math.max(0.1, number(p.w, b.w)),
      h: Math.max(0.1, number(p.h, b.h)),
    };
    // mask 中的描边参与覆盖，避免 SVG clipPath 忽略笔刷描边。
    return `<defs><mask id="${id}-effect" maskUnits="userSpaceOnUse" x="${patchRect.x - 1}" y="${patchRect.y - 1}" width="${patchRect.w + 2}" height="${patchRect.h + 2}">${geometry}</mask></defs><image x="${patchRect.x}" y="${patchRect.y}" width="${patchRect.w}" height="${patchRect.h}" href="${xml(patch)}" preserveAspectRatio="none" mask="url(#${id}-effect)"/>`;
  }

  function markup(a, context = {}) {
    if (!a || a.hidden || a.type === "eraser" || a.type === "lasso") return "";
    const s = styleOf(a),
      b = box(a),
      c = color(s.color),
      width = Math.max(0.25, number(s.width, 4));
    const id =
      "long-mark-" + String(a.id ?? a.type).replace(/[^a-zA-Z0-9_-]/g, "-");
    if (a.type === "watermark") return watermarkMarkup(a, context, id);
    if (a.type === "spotlight") return spotlightMarkup(a, context, id);
    let result = "";
    if (a.type === "rect")
      result = `<path d="${roundedPath(b, radiusValues(s))}" ${shapePaint(s)}/>`;
    else if (a.type === "ellipse")
      result = `<ellipse cx="${b.x + b.w / 2}" cy="${b.y + b.h / 2}" rx="${b.w / 2}" ry="${b.h / 2}" ${shapePaint(s)}/>`;
    else if (["triangle", "diamond", "star"].includes(a.type))
      result = `<path d="${pathData(polygonPoints(a.type, b), false, true)}" ${shapePaint(s)}/>`;
    else if (a.type === "arrow") result = arrowMarkup(a);
    else if (a.type === "text") result = textMarkup(a);
    else if (a.type === "serial") {
      const size = fontSize(a),
        radius = b.w / 2,
        label = s.serialNumber ?? a.number ?? a.text ?? 1;
      result = `<circle cx="${number(a.x)}" cy="${number(a.y)}" r="${radius}" fill="${c}"/><text x="${number(a.x)}" y="${number(a.y) + size * 0.35}" text-anchor="middle" fill="${color(s.serialTextColor, "#ffffff")}" font-size="${size}" font-family="${xml(s.fontFamily || "Segoe UI,Microsoft YaHei UI,sans-serif")}" font-weight="${s.bold ? 700 : 400}">${xml(label)}</text>`;
    } else if (a.type === "highlightRect") {
      result = `<path d="${roundedPath(b, radiusValues(s))}" fill="${c}"/>`;
    } else if (PATHS.has(a.type)) {
      let points = pointsOf(a);
      if (
        a.type === "highlighter" &&
        s.highlighterShape === "line" &&
        points.length > 1
      )
        points = [points[0], points[points.length - 1]];
      const closed = a.type === "polyline" && (s.closed || s.filled),
        smooth =
          a.type === "polyline"
            ? s.polylineSmooth || s.smooth
            : s.pencilSmooth || s.smooth;
      result = `<path d="${pathData(points, smooth, closed)}" fill="${closed && s.filled ? c : "none"}" stroke="${c}" stroke-width="${width}" stroke-linecap="${a.type === "highlighter" && s.highlighterShape === "line" ? "butt" : "round"}" stroke-linejoin="round"${a.type === "highlighter" ? "" : dashAttributes(s, width)}/>`;
    } else if (EFFECTS.has(a.type)) result = effectMarkup(a, id);
    const highlighter = a.type === "highlighter" || a.type === "highlightRect";
    const blend =
      highlighter && s.highlighterBlend !== "alpha"
        ? ' style="mix-blend-mode:multiply"'
        : "";
    return `<g${rotation(a, b)} opacity="${alpha(s.opacity, highlighter ? 0.38 : 1)}"${blend}>${result}</g>`;
  }

  function prepareEffect(a, image) {
    if (!a || !EFFECTS.has(a.type) || !image) return false;
    const s = styleOf(a),
      b = box(a),
      sourceWidth = number(
        image.naturalWidth || image.videoWidth || image.width,
      ),
      sourceHeight = number(
        image.naturalHeight || image.videoHeight || image.height,
      );
    if (sourceWidth <= 0 || sourceHeight <= 0 || b.w <= 0 || b.h <= 0)
      return false;
    const effect =
      a.type === "pixelEraser"
        ? "restore"
        : s.effect ||
          (a.type.includes("blur")
            ? "blur"
            : a.type === "localFill"
              ? "localFill"
              : "mosaic");
    const block = limit(number(s.blockSize ?? s.strength, 16), 1, 256);
    let left = b.x,
      top = b.y,
      right = b.x + b.w,
      bottom = b.y + b.h;
    if (effect === "mosaic") {
      left = Math.floor(left / block) * block;
      top = Math.floor(top / block) * block;
      right = Math.ceil(right / block) * block;
      bottom = Math.ceil(bottom / block) * block;
    }
    if (effect === "blur") {
      const pad = block * 2;
      left -= pad;
      top -= pad;
      right += pad;
      bottom += pad;
    }
    left = limit(Math.floor(left), 0, sourceWidth);
    top = limit(Math.floor(top), 0, sourceHeight);
    right = limit(Math.ceil(right), left, sourceWidth);
    bottom = limit(Math.ceil(bottom), top, sourceHeight);
    const w = right - left,
      h = bottom - top;
    if (!w || !h) return false;
    const scale = Math.min(1, 1024 / Math.max(w, h));
    try {
      const canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.ceil(w * scale));
      canvas.height = Math.max(1, Math.ceil(h * scale));
      const ctx = canvas.getContext("2d", {
        willReadFrequently: effect === "localFill",
      });
      if (!ctx) return false;
      if (effect === "mosaic") {
        const small = document.createElement("canvas");
        small.width = Math.max(1, Math.ceil(w / block));
        small.height = Math.max(1, Math.ceil(h / block));
        small
          .getContext("2d")
          .drawImage(image, left, top, w, h, 0, 0, small.width, small.height);
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(small, 0, 0, canvas.width, canvas.height);
      } else if (effect === "localFill") {
        // 以邻近底色中位数演示恢复；原生局部填充仍由现有 Scene 算法完成。
        const sample = document.createElement("canvas");
        sample.width = 8;
        sample.height = 1;
        const sampleCtx = sample.getContext("2d", { willReadFrequently: true });
        const ring = [
          [b.x - 2, b.y - 2],
          [b.x + b.w / 2, b.y - 2],
          [b.x + b.w + 2, b.y - 2],
          [b.x - 2, b.y + b.h / 2],
          [b.x + b.w + 2, b.y + b.h / 2],
          [b.x - 2, b.y + b.h + 2],
          [b.x + b.w / 2, b.y + b.h + 2],
          [b.x + b.w + 2, b.y + b.h + 2],
        ];
        ring.forEach((p, i) =>
          sampleCtx.drawImage(
            image,
            limit(p[0], 0, sourceWidth - 1),
            limit(p[1], 0, sourceHeight - 1),
            1,
            1,
            i,
            0,
            1,
            1,
          ),
        );
        const data = sampleCtx.getImageData(0, 0, 8, 1).data;
        const median = (channel) =>
          Array.from({ length: 8 }, (_, i) => data[i * 4 + channel]).sort(
            (a, b) => a - b,
          )[4];
        ctx.fillStyle = `rgb(${median(0)},${median(1)},${median(2)})`;
        ctx.fillRect(0, 0, canvas.width, canvas.height);
      } else if (effect === "blur") {
        const original = document.createElement("canvas");
        original.width = canvas.width;
        original.height = canvas.height;
        original
          .getContext("2d")
          .drawImage(image, left, top, w, h, 0, 0, canvas.width, canvas.height);
        ctx.filter = `blur(${Math.max(0.5, block * scale * 0.5)}px)`;
        ctx.drawImage(original, 0, 0);
        ctx.filter = "none";
      } else
        ctx.drawImage(
          image,
          left,
          top,
          w,
          h,
          0,
          0,
          canvas.width,
          canvas.height,
        );
      a.patch = canvas.toDataURL("image/png");
      // 相对几何边界记录缓存，平移对象不会把缓存留在旧坐标。
      a.patchBounds = { x: left - b.x, y: top - b.y, w, h };
      delete a.patchError;
      return true;
    } catch (error) {
      delete a.patch;
      delete a.patchBounds;
      a.patchError = String(error?.message || error);
      return false;
    }
  }

  window.LongEditorRenderer = { box, markup, prepareEffect };
})();
