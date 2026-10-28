/* 前端交互采用现有工具目录；原生接入复用 Editor / Document / Scene。 */
(function () {
  "use strict";
  window.LongEditorAnnotations = {
    create(ctx) {
      const {
        state,
        root,
        $,
        clone,
        clamp,
        remember,
        renderCrop,
        render,
        anchor,
        center,
        startGesture,
        toast,
        escapeText,
        changeTool,
      } = ctx;
      const { tools, groups } = window.LongEditorTools,
        renderer = window.LongEditorRenderer;
      const nativeIcon = (name) =>
        `<span class="native-icon" aria-hidden="true" style="--tool-icon:url('icons/${name}.svg')"></span>`;
      const defaults = Object.fromEntries(
        Object.values(tools).map((t) => [
          t.key,
          Object.fromEntries(t.properties.map((p) => [p.key, clone(p.value)])),
        ]),
      );
      defaults.text.fontSize = 36;
      defaults.watermark.text = "内部资料";
      defaults.watermark.watermarkAngle = -25;
      let lastTool = state.tool,
        propertyTab = "properties",
        paletteGroup = null,
        polyline = null,
        lassoDraft = null,
        propertyBefore = null,
        scope = null,
        nodeIndex = null;
      state.selection = [];
      state.lasso = null;
      const box = (a) => renderer.box(a),
        byId = (id) => state.annotations.find((a) => a.id === id);
      const selected = () => state.selection.map(byId).filter(Boolean);
      const sourceImage = () => $("source");
      const isEffect = (a) =>
        [
          "mosaic",
          "blur",
          "mosaicBrush",
          "blurBrush",
          "localFill",
          "pixelEraser",
        ].includes(a.type);
      function select(ids) {
        state.selection = [...new Set(ids)];
        state.selected = state.selection.at(-1) ?? null;
        nodeIndex = null;
      }
      function selectItem(a, append = false) {
        let ids =
          a.group && scope !== a.group
            ? state.annotations
                .filter((o) => o.group === a.group)
                .map((o) => o.id)
            : [a.id];
        if (append)
          ids = ids.every((id) => state.selection.includes(id))
            ? state.selection.filter((id) => !ids.includes(id))
            : [...state.selection, ...ids];
        select(ids);
      }
      const editable = () =>
        state.annotations.filter(
          (a) =>
            !["watermark", "localFill", "pixelEraser"].includes(a.type) &&
            (!scope || a.group === scope),
        );
      function selectionBox(items = selected()) {
        if (!items.length) return null;
        const bs = items.map(box),
          x = Math.min(...bs.map((b) => b.x)),
          y = Math.min(...bs.map((b) => b.y));
        return {
          x,
          y,
          w: Math.max(...bs.map((b) => b.x + b.w)) - x,
          h: Math.max(...bs.map((b) => b.y + b.h)) - y,
        };
      }
      function newAnnotation(type, p) {
        return {
          id: Date.now() + Math.random(),
          type,
          x: p.x,
          y: p.y,
          w: 0,
          h: 0,
          style: clone(defaults[type]),
          rotation: 0,
          ...(scope ? { group: scope } : {}),
        };
      }
      function commitAnnotations(before) {
        const after = clone(state.annotations);
        if (JSON.stringify(before) === JSON.stringify(after)) return false;
        state.annotations = clone(before);
        remember();
        state.annotations = after;
        return true;
      }
      function modify(fn) {
        const before = clone(state.annotations);
        fn();
        if (scope && !state.annotations.some((a) => a.group === scope))
          scope = null;
        if (commitAnnotations(before)) {
          renderCrop();
          draw();
        }
      }
      function draw() {
        ctx.renderAnnotations();
      }
      function hit(p) {
        return [...editable()].reverse().find((a) => {
          const b = box(a),
            pad = 8 / state.zoom;
          return (
            p.x >= b.x - pad &&
            p.x <= b.x + b.w + pad &&
            p.y >= b.y - pad &&
            p.y <= b.y + b.h + pad
          );
        });
      }
      function prepare(a) {
        if (isEffect(a)) {
          try {
            renderer.prepareEffect(a, sourceImage());
          } catch {
            toast("请等图片载入后再处理", true);
          }
        }
      }
      function styleTarget() {
        const list = selected();
        if (list.length === 1) return list[0];
        if (state.tool === "watermark")
          return state.annotations.find((a) => a.type === "watermark") ?? null;
        return null;
      }
      function toolInfo() {
        const a = styleTarget();
        return tools[a?.type] ?? tools[state.tool] ?? null;
      }
      function closePalette() {
        $("tool-palette").hidden = true;
        paletteGroup = null;
        root
          .querySelectorAll("[data-tool-group]")
          .forEach((b) => b.setAttribute("aria-expanded", "false"));
      }
      function openPalette(group) {
        paletteGroup = group;
        const palette = $("tool-palette");
        palette.hidden = false;
        palette.innerHTML = `<div class="palette-heading">${nativeIcon(group === "all" ? "toolbar" : groups.find((g) => g.key === group).icon)}<span>${group === "all" ? "全部标注工具" : groups.find((g) => g.key === group).label}</span><button class="icon-button" data-close-palette aria-label="关闭工具库">×</button></div>${group === "all" ? '<input class="tool-search" id="tool-search" placeholder="搜索工具" aria-label="搜索标注工具">' : ""}<div id="palette-items"></div>`;
        paintPalette();
        root
          .querySelectorAll("[data-tool-group]")
          .forEach((b) =>
            b.setAttribute("aria-expanded", b.dataset.toolGroup === group),
          );
        if (group === "all") $("tool-search").focus();
      }
      function paintPalette(query = "") {
        const filtered = groups.filter(
          (g) => paletteGroup === "all" || g.key === paletteGroup,
        );
        $("palette-items").innerHTML =
          filtered
            .map((g) => {
              const items = g.tools
                .map((key) => tools[key])
                .filter((t) => (t.label + t.hint).includes(query));
              if (!items.length) return "";
              return `${paletteGroup === "all" ? `<div class="palette-group-title">${g.label}</div>` : ""}<div class="palette-grid">${items.map((t) => `<button class="palette-tool ${state.tool === t.key ? "on" : ""}" data-pick-tool="${t.key}" title="${escapeText(t.hint)}">${nativeIcon(t.icon)}<span>${t.label}</span></button>`).join("")}</div>`;
            })
            .join("") || '<div class="object-hint">没有匹配的工具</div>';
      }
      function initUI() {
        // 宿主可替换工具栏开头（长图 v2 以“剪辑”取代“裁剪”并放在首位）
        root.querySelector(".tools").innerHTML =
          `${ctx.railHead ?? `<button class="tool" data-tool="select" title="选择、框选与编辑 · V">${nativeIcon("edit")}<span>选择</span></button><button class="tool" data-tool="crop" title="裁剪 · C">${nativeIcon("crop")}<span>裁剪</span></button>`}<div class="tool-separator"></div>${groups.map((g) => (g.standalone ? `<button class="tool" data-tool="lasso" title="套索透明裁剪">${nativeIcon(g.icon)}<span>套索</span></button>` : `<button class="tool" data-tool-group="${g.key}" aria-expanded="false" title="${g.label}工具">${nativeIcon(g.icon)}<span>${g.label}</span><i class="group-corner"></i></button>`)).join("")}<span class="spacer"></span><button class="tool" data-tool-group="all" aria-expanded="false" title="打开完整工具库">${nativeIcon("toolbar")}<span>工具库</span></button><button class="icon-button" data-action="help" aria-label="操作说明">${nativeIcon("keyboard")}</button>`;
        const palette = document.createElement("div");
        palette.id = "tool-palette";
        palette.className = "tool-palette";
        palette.hidden = true;
        root.querySelector(".canvas-region").appendChild(palette);
        const floating = document.createElement("div");
        floating.id = "annotation-floating";
        floating.className = "annotation-floating";
        floating.hidden = true;
        root.querySelector(".canvas-region").appendChild(floating);
        $("annotation-inspector").innerHTML =
          '<div id="annotation-panel"></div>';
        root.querySelector(".side-bottom").innerHTML =
          "<span>滚轮浏览 · Ctrl + 滚轮缩放</span><span>空格拖动画布 · Shift 加选对象</span>";
      }
      function formatValue(p, value) {
        if (p.key === "opacity" || p.key === "textBackgroundOpacity")
          return Math.round(value * 100) + "%";
        return Number.isInteger(Number(value))
          ? String(value)
          : Number(value).toFixed(2).replace(/0+$/, "").replace(/\.$/, "");
      }
      function visibleProperty(p, s) {
        if (
          p.key.startsWith("radius") &&
          p.key !== "radius" &&
          !s.independentCorners
        )
          return false;
        if (p.key === "radius" && s.independentCorners) return false;
        if (
          p.key.startsWith("textOutline") &&
          p.key !== "textOutline" &&
          !s.textOutline
        )
          return false;
        if (
          p.key.startsWith("textBackground") &&
          p.key !== "textBackground" &&
          !s.textBackground
        )
          return false;
        return true;
      }
      function propertyRow(p, s, mixed = false) {
        const value = s[p.key] ?? p.value,
          id = "ann-prop-" + p.key,
          disabled = (["effect", "region"].includes(p.key) && !!styleTarget()) ||
            (s.arrowHead === "dart" && p.key === "dashed");
        let control = "";
        const attrs = `id="${id}" data-ann-prop="${p.key}" ${disabled ? "disabled" : ""}`;
        if (p.type === "toggle")
          control = `<input type="checkbox" ${attrs} ${value ? "checked" : ""}>`;
        else if (p.type === "select")
          control = `<select ${attrs}>${p.options.map((o) => `<option value="${o.value}" ${String(value) === String(o.value) ? "selected" : ""}>${o.label}</option>`).join("")}</select>`;
        else if (p.type === "color")
          control = `<span class="color-code">${escapeText(value)}</span><input type="color" value="${value}" ${attrs}>`;
        else if (p.type === "range")
          control = `<input type="range" min="${p.min}" max="${p.max}" step="${p.step}" value="${value}" ${attrs}><output data-prop-output="${p.key}">${mixed ? "—" : formatValue(p, value)}</output>`;
        else
          control = `<input type="${p.type === "text" ? "text" : "number"}" ${p.type === "number" ? `min="${p.min}" max="${p.max}" step="${p.step}"` : ""} value="${escapeText(value)}" ${attrs}>`;
        return `<label class="inspector-property" for="${id}"><span>${p.label}</span>${control}</label>`;
      }
      function propertiesMarkup() {
        const list = selected(),
          a = styleTarget(),
          info = toolInfo(),
          s = a?.style ?? defaults[state.tool] ?? {};
        if (state.tool === "lasso")
          return `<div class="selection-summary">${nativeIcon("lasso")}套索透明裁剪</div><p class="object-hint">沿要保留的内容绘制闭合轮廓。与矩形裁剪范围共同决定输出。</p><div class="object-extra-actions"><button class="object-text-action" data-object-action="apply-lasso" ${!lassoDraft ? "disabled" : ""}>应用套索 ↵</button><button class="object-text-action" data-object-action="cancel-lasso" ${!lassoDraft ? "disabled" : ""}>取消</button><button class="object-text-action" data-object-action="clear-lasso" ${!state.lasso ? "disabled" : ""}>清除套索</button></div><p class="object-hint">${state.lasso ? "已应用套索，可清除后恢复。" : "尚未应用套索。"}</p>`;
        if (list.length > 1) {
          const radiusItems = list.filter((o) =>
            tools[o.type]?.properties.some(
              (p) => p.key === "radius" || p.key === "effectRadius",
            ),
          );
          return `<div class="selection-summary">${nativeIcon("layers")}已选择 ${list.length} 个对象</div>${radiusItems.length ? `<p class="object-hint">圆角可应用于其中 ${radiusItems.length} 个区域对象，其它样式保持各自设置。</p><label class="inspector-property"><span>区域圆角</span><input type="number" data-ann-prop="batchRadius" min="0" value="0"></label>` : '<div class="object-properties-empty">这些对象没有共同的区域圆角参数。可分别选择后调整样式。</div>'}${objectControls(list)}`;
        }
        if (!info)
          return '<div class="empty-object-state">选择对象调整现有样式，或从左侧工具库开始标注。<br>Shift 加选，拖动空白区域可框选。</div>';
        const common = ["color", "width", "fontSize", "opacity", "fontFamily"],
          props = info.properties.filter((p) => visibleProperty(p, s)),
          look = props.filter((p) => common.includes(p.key)),
          special = props.filter((p) => !common.includes(p.key));
        return `<div class="selection-summary">${nativeIcon(info.icon)}${escapeText(info.label)}<span class="selection-tag">${a ? "当前对象" : "下一笔样式"}</span></div>${look.length ? `<details class="property-section" open><summary>外观</summary>${look.map((p) => propertyRow(p, s)).join("")}${look.some((p) => p.key === "color") ? `<div class="property-color-palette">${["#ff4b5c", "#ff934f", "#ffd34d", "#39c996", "#5796ff", "#bc8bff", "#ffffff", "#161b22"].map((c) => `<button data-ann-color="${c}" class="${s.color === c ? "active" : ""}" style="--swatch:${c}" aria-label="颜色 ${c}"></button>`).join("")}</div>` : ""}</details>` : ""}${special.length ? `<details class="property-section" open><summary>工具设置</summary>${special.map((p) => propertyRow(p, s)).join("")}</details>` : ""}${a && ["effect", "region"].some((k) => s[k]) ? '<p class="object-hint">已有对象保留当前区域类型；切换效果与区域请取消选择后设置下一笔。</p>' : ""}${state.tool === "watermark" ? `<div class="object-extra-actions"><button class="object-text-action" data-object-action="watermark">${a ? "编辑水印文字" : "添加水印"}</button><button class="object-text-action" data-object-action="remove-watermark" ${!a ? "disabled" : ""}>移除水印</button></div>` : ""}${a && a.type !== "watermark" ? objectControls([a]) : ""}<p class="object-hint">${info.hint}</p>`;
      }
      function objectControls(list) {
        const one = list.length === 1 ? list[0] : null,
          rotatable = list.some(
            (a) =>
              !["arrow", "polyline", "serial", "watermark"].includes(a.type),
          );
        return `<details class="property-section" open><summary>对象</summary><div class="object-actions">${[
          ["duplicate", "duplicate", "复制对象 Ctrl+D"],
          ["front", "bring-front", "置顶"],
          ["back", "send-back", "置底"],
          ["group", "group", "组合 Ctrl+G"],
          ["ungroup", "ungroup", "拆分组合 Ctrl+Shift+G"],
          ["delete", "trash", "删除 Delete"],
        ]
          .map(
            ([action, ic, label]) =>
              `<button data-object-action="${action}" title="${label}" aria-label="${label}" ${action === "group" && list.length < 2 ? "disabled" : ""} ${action === "ungroup" && !list.some((a) => a.group) ? "disabled" : ""}>${nativeIcon(ic)}</button>`,
          )
          .join(
            "",
          )}</div>${rotatable ? '<label class="inspector-property"><span>旋转角度</span><input type="number" data-object-rotation min="-360" max="360" step="1" value="' + (one?.rotation ?? 0) + '"></label><div class="object-extra-actions"><button class="object-text-action" data-object-action="reset-angle">恢复角度</button></div>' : ""}${one?.type === "text" ? '<button class="object-text-action" data-object-action="edit-text">编辑文字</button>' : ""}${one?.type === "arrow" ? `<button class="object-text-action" data-object-action="straighten" ${!one.control ? "disabled" : ""}>拉直箭头</button>` : ""}${one?.type === "polyline" ? `<button class="object-text-action" data-object-action="delete-node" ${nodeIndex === null ? "disabled" : ""}>删除选中顶点</button>` : ""}</details>`;
      }
      function objectsMarkup() {
        return `${scope ? '<div class="scope-bar">正在编辑组合 <button data-object-action="leave-group">返回上层</button></div>' : ""}<div class="section-heading"><h2>画布对象 <span id="annotation-count">${state.annotations.length}</span></h2><span class="spacer"></span><button class="reset-button" data-object-action="select-all">全选</button><button class="reset-button" data-object-action="clear" ${!state.annotations.length ? "disabled" : ""}>清空</button></div><ul class="annotation-list" id="annotation-list">${[
          ...state.annotations,
        ]
          .reverse()
          .map(
            (a, i) =>
              `<li><button data-object-select="${a.id}" class="${state.selection.includes(a.id) ? "selected" : ""}" title="${a.group ? "双击进入组合" : "Shift 加选"}">${nativeIcon(tools[a.type]?.icon ?? "edit")}<span class="object-name">${escapeText(a.type === "text" || a.type === "watermark" ? a.text || a.style.text || tools[a.type].label : (tools[a.type]?.label ?? "对象"))}${a.group ? " · 组合" : ""}</span><span class="object-index">${state.annotations.length - i}</span></button></li>`,
          )
          .join(
            "",
          )}</ul>${!state.annotations.length ? '<div class="empty-object-state">还没有标注。选择左侧工具，在画布上开始绘制。</div>' : ""}`;
      }
      function renderPanel(force = false) {
        const panel = $("annotation-panel"),
          focused =
            panel.contains(document.activeElement) &&
            document.activeElement.matches("input,select,textarea");
        if (focused && !force) return;
        panel.innerHTML = `<div class="property-tabs"><button data-property-tab="properties" class="${propertyTab === "properties" ? "active" : ""}">属性</button><button data-property-tab="objects" class="${propertyTab === "objects" ? "active" : ""}">对象 <span>${state.annotations.length || ""}</span></button></div>${propertyTab === "objects" ? objectsMarkup() : propertiesMarkup()}`;
      }
      function selectionChrome() {
        const list = selected();
        if (!list.length || state.tool === "crop") return "";
        const b = selectionBox(),
          z = state.zoom,
          pad = 5 / z;
        let out = `<rect x="${b.x - pad}" y="${b.y - pad}" width="${b.w + pad * 2}" height="${b.h + pad * 2}" fill="none" stroke="#88bfff" stroke-width="${1 / z}" stroke-dasharray="${5 / z} ${3 / z}"/>`;
        if (list.length === 1 && ["arrow", "polyline"].includes(list[0].type)) {
          const a = list[0],
            points =
              a.type === "arrow"
                ? [
                    { x: 0, y: 0 },
                    { x: a.w, y: a.h },
                  ]
                : (a.points ?? []);
          points.forEach(
            (p, i) =>
              (out += `<circle data-annotation-node="${i}" cx="${a.x + p.x}" cy="${a.y + p.y}" r="${4 / z}" class="selection-grip node" stroke-width="${1 / z}"/>`),
          );
          if (a.type === "arrow") {
            const cp = a.control ?? { x: a.w / 2, y: a.h / 2 };
            out += `<circle data-arrow-control cx="${a.x + cp.x}" cy="${a.y + cp.y}" r="${4 / z}" class="selection-grip node control" stroke-width="${1 / z}"/>`;
          }
          return out;
        }
        for (const [name, x, y] of [
          ["nw", b.x, b.y],
          ["ne", b.x + b.w, b.y],
          ["sw", b.x, b.y + b.h],
          ["se", b.x + b.w, b.y + b.h],
        ])
          out += `<rect data-annotation-handle="${name}" x="${x - 4 / z}" y="${y - 4 / z}" width="${8 / z}" height="${8 / z}" class="selection-grip" stroke-width="${1 / z}"/>`;
        return out;
      }
      function lassoMarkup() {
        const pts = lassoDraft ?? state.lasso;
        if (!pts?.length) return "";
        const path =
          pts.map((p, i) => `${i ? "L" : "M"} ${p.x} ${p.y}`).join(" ") + " Z";
        if (lassoDraft)
          return `<path d="${path}" class="lasso-preview" stroke-width="${1.5 / state.zoom}"/>`;
        return `<path d="M0 0H${state.w}V${state.h}H0Z ${path}" fill="#10141999" fill-rule="evenodd" pointer-events="none"/><path d="${path}" fill="none" stroke="#62deb0" stroke-dasharray="${5 / state.zoom} ${4 / state.zoom}" stroke-width="${1 / state.zoom}"/>`;
      }
      function renderFloating() {
        const bar = $("annotation-floating");
        bar.hidden = state.tool === "crop" || (!polyline && !lassoDraft);
        if (polyline)
          bar.innerHTML =
            '<span class="pending-label">' +
            polyline.points.length +
            ' 个顶点</span><button class="button" data-object-action="cancel-polyline">取消</button><button class="button primary" data-object-action="finish-polyline">完成折线 ↵</button>';
        else if (lassoDraft)
          bar.innerHTML =
            '<span class="pending-label">套索预览</span><button class="button" data-object-action="cancel-lasso">取消</button><button class="button primary" data-object-action="apply-lasso">应用套索 ↵</button>';
      }
      function markup(a) {
        return `<g data-annotation-id="${a.id}" data-annotation-type="${a.type}">${renderer.markup(a, { sourceWidth: state.w, sourceHeight: state.h })}</g>`;
      }
      function contentMarkup(items = state.annotations) {
        const effects = items.filter(isEffect),
          normal = items.filter((a) => !isEffect(a) && a.type !== "spotlight"),
          spots = items.filter((a) => a.type === "spotlight");
        let result = [...effects, ...normal].map(markup).join("");
        if (spots.length) {
          const holes = spots
            .map((a) => {
              const b = box(a),
                r = a.style.radius ?? 0,
                angle = a.rotation ?? 0;
              return (
                '<rect x="' +
                b.x +
                '" y="' +
                b.y +
                '" width="' +
                b.w +
                '" height="' +
                b.h +
                '" rx="' +
                r +
                '" fill="black" transform="rotate(' +
                angle +
                " " +
                (b.x + b.w / 2) +
                " " +
                (b.y + b.h / 2) +
                ')"/>'
              );
            })
            .join("");
          const opacity = Math.max(...spots.map((a) => a.style.opacity ?? 0.5));
          result +=
            '<defs><mask id="all-spotlights"><rect width="' +
            state.w +
            '" height="' +
            state.h +
            '" fill="white"/>' +
            holes +
            '</mask></defs><rect width="' +
            state.w +
            '" height="' +
            state.h +
            '" fill="black" opacity="' +
            opacity +
            '" mask="url(#all-spotlights)"/>';
          result += spots
            .map((a) => {
              const b = box(a);
              return (
                '<rect x="' +
                b.x +
                '" y="' +
                b.y +
                '" width="' +
                b.w +
                '" height="' +
                b.h +
                '" rx="' +
                (a.style.radius ?? 0) +
                '" fill="none" stroke="' +
                (a.style.color ?? "#ffffff") +
                '" stroke-width="' +
                (a.style.width ?? 2) +
                '" transform="rotate(' +
                (a.rotation ?? 0) +
                " " +
                (b.x + b.w / 2) +
                " " +
                (b.y + b.h / 2) +
                ')"/>'
              );
            })
            .join("");
        }
        return result;
      }
      function renderAnnotations() {
        if (lastTool !== state.tool) {
          polyline = null;
          lassoDraft = null;
          state.pending = null;
          if (state.tool !== "select") select([]);
          lastTool = state.tool;
          closePalette();
        }
        root
          .querySelectorAll("[data-tool-group]")
          .forEach((b) =>
            b.classList.toggle(
              "active",
              b.dataset.toolGroup === tools[state.tool]?.group,
            ),
          );
        const all = [...state.annotations];
        if (state.pending) all.push(state.pending);
        if (polyline) all.push(polyline);
        $("annotations").innerHTML =
          contentMarkup(all) +
          lassoMarkup() +
          selectionChrome() +
          (state.gesture?.kind === "ann:marquee"
            ? `<rect x="${state.gesture.rect.x}" y="${state.gesture.rect.y}" width="${state.gesture.rect.w}" height="${state.gesture.rect.h}" class="marquee-selection" stroke-width="${1 / state.zoom}"/>`
            : "");
        const miniArtwork = $("mini-annotations");
        if (miniArtwork) miniArtwork.innerHTML = contentMarkup()
          .replace(/id="([^"]+)"/g, 'id="mini-$1"')
          .replace(/url\(#([^)]+)\)/g, 'url(#mini-$1)');
        renderPanel();
        renderFloating();
      }
      function begin(e, p) {
        const special = e.target.closest(
          "[data-annotation-handle],[data-annotation-node],[data-arrow-control]",
        );
        if (special && selected().length) {
          const kind = special.hasAttribute("data-annotation-handle")
            ? "ann:resize"
            : special.hasAttribute("data-arrow-control")
              ? "ann:control"
              : "ann:node";
          nodeIndex = special.hasAttribute("data-annotation-node")
            ? Number(special.dataset.annotationNode)
            : null;
          startGesture(e, {
            kind,
            edge: special.dataset.annotationHandle,
            index: nodeIndex,
            start: p,
            before: clone(state.annotations),
            items: clone(selected()),
            baseBox: selectionBox(),
          });
          return;
        }
        if (state.tool === "select") {
          const a = hit(p);
          if (a) {
            if (e.shiftKey || !state.selection.includes(a.id))
              selectItem(a, e.shiftKey);
            startGesture(e, {
              kind: "ann:move",
              start: p,
              before: clone(state.annotations),
              items: clone(selected()),
              moved: false,
            });
          } else {
            if (!e.shiftKey) select([]);
            startGesture(e, {
              kind: "ann:marquee",
              start: p,
              originalSelection: [...state.selection],
              rect: { x: p.x, y: p.y, w: 0, h: 0 },
            });
          }
          draw();
          return;
        }
        if (state.tool === "lasso") {
          lassoDraft = [p];
          startGesture(e, { kind: "ann:lasso", start: p });
          draw();
          return;
        }
        const info = tools[state.tool];
        if (!info) return;
        if (info.gesture === "eraser") {
          const before = clone(state.annotations);
          eraseAt(p);
          startGesture(e, { kind: "ann:erase", before });
          draw();
          return;
        }
        if (info.gesture === "text") {
          e.preventDefault();
          editText(
            p,
            state.tool === "watermark"
              ? state.annotations.find((a) => a.type === "watermark")
              : null,
          );
          return;
        }
        if (info.gesture === "point") {
          const a = newAnnotation(state.tool, p);
          a.number =
            state.annotations.filter((a) => a.type === "serial").length + 1;
          remember();
          state.annotations.push(a);
          select([a.id]);
          renderCrop();
          draw();
          return;
        }
        if (info.gesture === "polyline") {
          e.preventDefault();
          if (!polyline) {
            polyline = newAnnotation("polyline", p);
            polyline.points = [{ x: 0, y: 0 }];
          } else
            polyline.points.push({ x: p.x - polyline.x, y: p.y - polyline.y });
          draw();
          return;
        }
        const a = newAnnotation(state.tool, p);
        if (info.gesture === "freehand") a.points = [{ x: 0, y: 0 }];
        state.pending = a;
        startGesture(e, { kind: "ann:draw", start: p, annotation: a });
        draw();
      }
      function eraseAt(p) {
        const radius = (defaults.eraser.width ?? 48) / 2;
        state.annotations = state.annotations.filter((a) => {
          if (["watermark", "localFill", "pixelEraser"].includes(a.type))
            return true;
          const b = box(a);
          return !(
            p.x >= b.x - radius &&
            p.x <= b.x + b.w + radius &&
            p.y >= b.y - radius &&
            p.y <= b.y + b.h + radius
          );
        });
      }
      function move(e, g, p) {
        if (!g.kind.startsWith("ann:")) return false;
        if (g.kind === "ann:draw") {
          const a = state.pending;
          if (!a) return true;
          a.w = p.x - g.start.x;
          a.h = p.y - g.start.y;
          if (
            a.points &&
            Math.hypot(a.points.at(-1).x - a.w, a.points.at(-1).y - a.h) >
              1 / state.zoom
          )
            a.points.push({ x: a.w, y: a.h });
        } else if (g.kind === "ann:lasso") {
          if (!lassoDraft) return true;
          if (
            Math.hypot(p.x - lassoDraft.at(-1).x, p.y - lassoDraft.at(-1).y) >
            2 / state.zoom
          )
            lassoDraft.push(p);
        } else if (g.kind === "ann:erase") eraseAt(p);
        else if (g.kind === "ann:marquee")
          g.rect = {
            x: Math.min(g.start.x, p.x),
            y: Math.min(g.start.y, p.y),
            w: Math.abs(p.x - g.start.x),
            h: Math.abs(p.y - g.start.y),
          };
        else if (g.kind === "ann:move") {
          if (
            Math.hypot(
              e.clientX - g.startClient.x,
              e.clientY - g.startClient.y,
            ) > 3
          )
            g.moved = true;
          if (g.moved)
            for (const base of g.items) {
              const a = byId(base.id);
              if (a) {
                a.x = base.x + p.x - g.start.x;
                a.y = base.y + p.y - g.start.y;
              }
            }
        } else if (g.kind === "ann:node") {
          const base = g.items[0],
            a = byId(base.id);
          if (!a) return true;
          if (a.type === "arrow") {
            if (g.index === 0) {
              const end = { x: base.x + base.w, y: base.y + base.h };
              a.x = p.x;
              a.y = p.y;
              a.w = end.x - p.x;
              a.h = end.y - p.y;
            } else {
              a.w = p.x - a.x;
              a.h = p.y - a.y;
            }
          } else a.points[g.index] = { x: p.x - a.x, y: p.y - a.y };
        } else if (g.kind === "ann:control") {
          const a = byId(g.items[0].id);
          if (!a) return true;
          a.control = { x: p.x - a.x, y: p.y - a.y };
        } else if (g.kind === "ann:resize") {
          const b = g.baseBox,
            fixed = {
              x: g.edge.includes("w") ? b.x + b.w : b.x,
              y: g.edge.includes("n") ? b.y + b.h : b.y,
            },
            next = {
              x: Math.min(p.x, fixed.x),
              y: Math.min(p.y, fixed.y),
              w: Math.max(8, Math.abs(p.x - fixed.x)),
              h: Math.max(8, Math.abs(p.y - fixed.y)),
            },
            sx = next.w / Math.max(1, b.w),
            sy = next.h / Math.max(1, b.h);
          for (const base of g.items) {
            const a = byId(base.id);
            if (!a) continue;
            a.x = next.x + (base.x - b.x) * sx;
            a.y = next.y + (base.y - b.y) * sy;
            a.w = base.w * sx;
            a.h = base.h * sy;
            if (base.points)
              a.points = base.points.map((p) => ({ x: p.x * sx, y: p.y * sy }));
            if (base.control)
              a.control = { x: base.control.x * sx, y: base.control.y * sy };
            if (["text", "serial"].includes(a.type))
              a.style.fontSize = Math.max(8, (base.style.fontSize ?? 28) * sy);
          }
        }
        draw();
        return true;
      }
      function finish(e, g) {
        if (!g.kind.startsWith("ann:")) return false;
        if (g.kind === "ann:draw" && state.pending) {
          const a = state.pending,
            b = box(a);
          if (b.w > 4 || b.h > 4) {
            remember();
            prepare(a);
            state.annotations.push(a);
            select([a.id]);
          }
          state.pending = null;
        } else if (g.kind === "ann:lasso") {
          if (lassoDraft.length < 3) lassoDraft = null;
        } else if (g.kind === "ann:marquee") {
          const r = g.rect,
            ids = editable()
              .filter((a) => {
                const b = box(a);
                return (
                  b.x >= r.x &&
                  b.y >= r.y &&
                  b.x + b.w <= r.x + r.w &&
                  b.y + b.h <= r.y + r.h
                );
              })
              .map((a) => a.id);
          select([...g.originalSelection, ...ids]);
        } else if (g.before) {
          if (g.kind === "ann:resize") selected().forEach(prepare);
          commitAnnotations(g.before);
        }
        return true;
      }
      function cancel(g) {
        if (!g.kind.startsWith("ann:")) return false;
        if (g.before) state.annotations = clone(g.before);
        if (g.kind === "ann:lasso") lassoDraft = null;
        state.pending = null;
        return true;
      }
      function editText(p, existing = null) {
        if ($("text-editor")) return;
        const type = existing?.type ?? state.tool,
          a = existing ?? newAnnotation(type, p),
          input = document.createElement("textarea");
        input.id = "text-editor";
        input.className = "annotation-input";
        input.setAttribute(
          "aria-label",
          type === "watermark" ? "水印文字" : "标注文字",
        );
        input.placeholder = "输入文字，Enter 完成";
        input.value =
          existing?.text ?? existing?.style.text ?? defaults[type]?.text ?? "";
        // 删段后原图坐标到屏幕不再是线性缩放，由宿主定位
        if (ctx.placeTextInput) ctx.placeTextInput(input, p);
        else {
          input.style.left = p.x * state.zoom + "px";
          input.style.top = p.y * state.zoom - 26 + "px";
          $("surface").appendChild(input);
        }
        input.focus();
        input.select();
        let done = false;
        const end = (save) => {
          if (done) return;
          done = true;
          if (save && input.value.trim()) {
            const value = input.value.trim();
            modify(() => {
              a.text = value;
              a.style.text = value;
              if (!existing) {
                if (type === "watermark")
                  state.annotations = state.annotations.filter(
                    (o) => o.type !== "watermark",
                  );
                state.annotations.push(a);
              }
              select(type === "watermark" ? [] : [a.id]);
            });
          }
          input.remove();
          draw();
        };
        input.addEventListener("keydown", (e) => {
          e.stopPropagation();
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            end(true);
          }
          if (e.key === "Escape") {
            e.preventDefault();
            end(false);
          }
        });
        input.addEventListener("blur", () => end(true));
      }
      function finishPolyline() {
        if (!polyline) return;
        polyline.points = polyline.points.filter(
          (p, i, all) =>
            !i || Math.hypot(p.x - all[i - 1].x, p.y - all[i - 1].y) > 0.5,
        );
        if (polyline.points.length >= 2) {
          remember();
          state.annotations.push(polyline);
          select([polyline.id]);
        }
        polyline = null;
        renderCrop();
        draw();
      }
      function applyLasso() {
        if (!lassoDraft || lassoDraft.length < 3) return;
        remember();
        state.lasso = clone(lassoDraft);
        lassoDraft = null;
        renderCrop();
        draw();
      }
      function action(name) {
        const list = selected(),
          one = list.length === 1 ? list[0] : null;
        switch (name) {
          case "select-all":
            select(editable().map((a) => a.id));
            draw();
            return;
          case "leave-group":
            scope = null;
            select([]);
            draw();
            return;
          case "apply-lasso":
            applyLasso();
            return;
          case "cancel-lasso":
            lassoDraft = null;
            draw();
            return;
          case "clear-lasso":
            if (state.lasso) {
              remember();
              state.lasso = null;
              renderCrop();
              draw();
            }
            return;
          case "finish-polyline":
            finishPolyline();
            return;
          case "cancel-polyline":
            polyline = null;
            draw();
            return;
          case "edit-text":
            if (one) editText({ x: one.x, y: one.y }, one);
            return;
          case "watermark": {
            const current = state.annotations.find(
              (a) => a.type === "watermark",
            );
            editText(center(), current);
            return;
          }
          case "remove-watermark":
            modify(
              () =>
                (state.annotations = state.annotations.filter(
                  (a) => a.type !== "watermark",
                )),
            );
            return;
          case "clear":
            modify(() => {
              state.annotations = [];
              select([]);
            });
            return;
          case "duplicate":
            if (list.length)
              modify(() => {
                const copiedGroups = new Map();
                const copies = list.map((a) => {
                  if (a.group && !copiedGroups.has(a.group))
                    copiedGroups.set(a.group, "g" + Date.now() + Math.random());
                  return {
                    ...clone(a),
                    id: Date.now() + Math.random(),
                    x: a.x + 24,
                    y: a.y + 24,
                    group: a.group ? copiedGroups.get(a.group) : undefined,
                  };
                });
                state.annotations.push(...copies);
                select(copies.map((a) => a.id));
              });
            return;
          case "delete":
            if (list.length)
              modify(() => {
                state.annotations = state.annotations.filter(
                  (a) => !state.selection.includes(a.id),
                );
                select([]);
              });
            return;
          case "front":
          case "back":
            if (list.length)
              modify(() => {
                const rest = state.annotations.filter(
                  (a) => !state.selection.includes(a.id),
                );
                state.annotations =
                  name === "front" ? [...rest, ...list] : [...list, ...rest];
              });
            return;
          case "group":
            if (list.length >= 2)
              modify(() => {
                const id = "g" + Date.now();
                list.forEach((a) => (a.group = id));
              });
            return;
          case "ungroup":
            if (list.length) modify(() => list.forEach((a) => delete a.group));
            return;
          case "reset-angle":
            modify(() => list.forEach((a) => (a.rotation = 0)));
            return;
          case "straighten":
            if (one?.control) modify(() => delete one.control);
            return;
          case "delete-node":
            if (
              one?.type === "polyline" &&
              nodeIndex !== null &&
              one.points.length > 2
            )
              modify(() => {
                one.points.splice(nodeIndex, 1);
                nodeIndex = null;
              });
            return;
        }
      }
      function normalizeArrow(style, key) {
        if (style.arrowHead !== "dart") return;
        style.dashed = false;
        if (key === "arrowStart" && style.arrowStart) style.arrowEnd = false;
        else if (key === "arrowEnd" && style.arrowEnd) style.arrowStart = false;
        else if (style.arrowStart && style.arrowEnd) style.arrowStart = false;
      }
      function changeProperty(input) {
        const info = toolInfo(),
          p = info?.properties.find((p) => p.key === input.dataset.annProp),
          key = input.dataset.annProp,
          value =
            input.type === "checkbox"
              ? input.checked
              : ["number", "range"].includes(input.type)
                ? Number(input.value)
                : input.value;
        if (input.type === "number" && (!input.value || !input.checkValidity()))
          return;
        if (key === "batchRadius") {
          if (!propertyBefore) propertyBefore = clone(state.annotations);
          for (const a of selected()) {
            const k = tools[a.type]?.properties.some(
              (p) => p.key === "effectRadius",
            )
              ? "effectRadius"
              : tools[a.type]?.properties.some((p) => p.key === "radius")
                ? "radius"
                : null;
            if (k) {
              a.style[k] = value;
              prepare(a);
            }
          }
          draw();
          return;
        }
        if (!p) return;
        const a = styleTarget();
        if (a) {
          if (!propertyBefore) propertyBefore = clone(state.annotations);
          a.style[key] = value;
          normalizeArrow(a.style, key);
          if (key === "text") a.text = value;
          prepare(a);
        } else {
          defaults[state.tool][key] = value;
          normalizeArrow(defaults[state.tool], key);
          if (key === "effect" || key === "region") {
            const current = defaults[state.tool],
              effect = key === "effect" ? value : current.effect,
              region = key === "region" ? value : current.region,
              next =
                effect === "localFill"
                  ? "localFill"
                  : region === "brush"
                    ? effect === "blur"
                      ? "blurBrush"
                      : "mosaicBrush"
                    : effect === "blur"
                      ? "blur"
                      : "mosaic";
            defaults[next] = {
              ...defaults[next],
              ...current,
              effect,
              region: effect === "localFill" ? "brush" : region,
            };
            changeTool(next);
          }
        }
        const output = root.querySelector(`[data-prop-output="${key}"]`);
        if (output) output.textContent = formatValue(p, value);
        draw();
      }
      function endProperty() {
        if (propertyBefore) {
          commitAnnotations(propertyBefore);
          propertyBefore = null;
          renderCrop();
        }
        renderPanel();
      }
      function keydown(e) {
        if (state.tool === "crop") return false;
        const ctrl = e.ctrlKey || e.metaKey;
        if (
          state.gesture &&
          (["Delete", "Backspace"].includes(e.key) ||
            e.key.startsWith("Arrow") ||
            (ctrl && ["a", "d", "g"].includes(e.key.toLowerCase())))
        )
          ctx.cancelGesture();
        if (e.key === "Escape") {
          if (polyline || lassoDraft) {
            polyline = null;
            lassoDraft = null;
            draw();
            return true;
          }
          closePalette();
          select([]);
          draw();
          return true;
        }
        if (e.key === "Enter") {
          if (polyline) {
            finishPolyline();
            return true;
          }
          if (lassoDraft) {
            applyLasso();
            return true;
          }
        }
        if (ctrl && e.key.toLowerCase() === "a") {
          action("select-all");
          return true;
        }
        if (ctrl && e.key.toLowerCase() === "d") {
          action("duplicate");
          return true;
        }
        if (ctrl && e.key.toLowerCase() === "g") {
          action(e.shiftKey ? "ungroup" : "group");
          return true;
        }
        if (e.key === "Delete" || e.key === "Backspace") {
          if (nodeIndex !== null && selected()[0]?.type === "polyline")
            action("delete-node");
          else action("delete");
          return true;
        }
        if (e.key.startsWith("Arrow") && selected().length) {
          const dx =
              e.key === "ArrowLeft" ? -1 : e.key === "ArrowRight" ? 1 : 0,
            dy = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0,
            n = e.shiftKey ? 10 : 1;
          modify(() =>
            selected().forEach((a) => {
              a.x += dx * n;
              a.y += dy * n;
            }),
          );
          return true;
        }
        return false;
      }
      function applyExportMask(context, crop) {
        if (!state.lasso?.length) return;
        context.save();
        context.globalCompositeOperation = "destination-in";
        context.beginPath();
        state.lasso.forEach((p, i) =>
          i
            ? context.lineTo(p.x - crop.x, p.y - crop.y)
            : context.moveTo(p.x - crop.x, p.y - crop.y),
        );
        context.closePath();
        context.fill();
        context.restore();
      }
      root.addEventListener("click", (e) => {
        const group = e.target.closest("[data-tool-group]");
        if (group) {
          paletteGroup === group.dataset.toolGroup
            ? closePalette()
            : openPalette(group.dataset.toolGroup);
          return;
        }
        if (e.target.closest("[data-close-palette]")) {
          closePalette();
          return;
        }
        const pick = e.target.closest("[data-pick-tool]");
        if (pick) {
          changeTool(pick.dataset.pickTool);
          if (state.tool === pick.dataset.pickTool) closePalette();
          return;
        }
        const tab = e.target.closest("[data-property-tab]");
        if (tab) {
          propertyTab = tab.dataset.propertyTab;
          renderPanel();
          return;
        }
        const color = e.target.closest("[data-ann-color]");
        if (color) {
          const a = styleTarget();
          if (a) modify(() => (a.style.color = color.dataset.annColor));
          else {
            defaults[state.tool].color = color.dataset.annColor;
            draw();
          }
          return;
        }
        const item = e.target.closest("[data-object-select]");
        if (item) {
          const a = byId(Number(item.dataset.objectSelect));
          if (!a) return;
          selectItem(a, e.shiftKey);
          if (a.type === "watermark") {
            changeTool("watermark");
            select([]);
          }
          const b = box(a);
          anchor({ x: b.x + b.w / 2, y: b.y + b.h / 2 });
          draw();
          return;
        }
        const act = e.target.closest("[data-object-action]");
        if (act) {
          action(act.dataset.objectAction);
          return;
        }
        if (!e.target.closest("#tool-palette") && !e.target.closest(".tools"))
          closePalette();
      });
      root.addEventListener("input", (e) => {
        if (e.target.id === "tool-search") paintPalette(e.target.value);
        if (e.target.matches("[data-ann-prop]")) changeProperty(e.target);
        if (e.target.matches("[data-object-rotation]")) {
          if (!e.target.value || !e.target.checkValidity()) return;
          if (!propertyBefore) propertyBefore = clone(state.annotations);
          selected().forEach((a) => {
            if (!["arrow", "polyline", "serial", "watermark"].includes(a.type))
              a.rotation = Number(e.target.value);
          });
          draw();
        }
      });
      root.addEventListener("change", (e) => {
        if (e.target.matches("[data-ann-prop],[data-object-rotation]")) {
          endProperty();
          setTimeout(() => renderPanel(true), 0);
        }
      });
      root.addEventListener("focusout", (e) => {
        if (e.target.matches("[data-ann-prop],[data-object-rotation]")) {
          endProperty();
          setTimeout(() => renderPanel(true), 0);
        }
      });
      $("viewport").addEventListener("dblclick", (e) => {
        if (state.tool === "polyline" && polyline) {
          finishPolyline();
          return;
        }
        if (state.tool !== "select") return;
        const a = hit(ctx.originalPoint(e));
        if (!a) return;
        if (a.group && scope !== a.group) {
          scope = a.group;
          select([]);
          draw();
          return;
        }
        if (a.type === "text") {
          e.preventDefault();
          editText({ x: a.x, y: a.y }, a);
        }
      });
      $("viewport").addEventListener("contextmenu", (e) => {
        if (state.tool === "lasso" || polyline) {
          e.preventDefault();
          if (state.gesture) ctx.cancelGesture();
          lassoDraft = null;
          polyline = null;
          draw();
        }
      });
      initUI();
      return {
        box,
        markup,
        exportMarkup: contentMarkup,
        prepareOutput() {
          if (polyline) finishPolyline();
          if (lassoDraft) applyLasso();
        },
        render: renderAnnotations,
        begin,
        move,
        finish,
        cancel,
        keydown,
        applyExportMask,
        historyRestored() {
          select([]);
          if (scope && !state.annotations.some((a) => a.group === scope))
            scope = null;
          polyline = null;
          lassoDraft = null;
          propertyBefore = null;
          state.pending = null;
        },
        reset() {
          select([]);
          state.lasso = null;
          scope = null;
          polyline = null;
          lassoDraft = null;
          propertyBefore = null;
          closePalette();
        },
        getSelection: () => state.selection,
      };
    },
  };
})();
