/*
 * 长图编辑前端工具目录：仅映射 RaiShot 已有工具和参数语义。
 * 原型效果用于交互设计，不等同原生 Scene 的生产渲染，也不引入新的原生能力。
 * 参考：toolbar_groups.rs / property_items.rs / tool_presets.rs / pin_tool_picker.rs。
 * color 为 CSS 十六进制；opacity、textBackgroundOpacity 保持原生 0–1。
 * camelCase 参数对应 Style / TextStyle 同义字段；text 映射 Annotation.text，
 * watermarkAngle 映射水印 Annotation.rotation，普通对象角度由对象操作维护。
 * independentCorners 与四个 radius* 字段映射 rectangle_radii 的 None / TL,TR,BR,BL。
 * effect 与 region 只用于切换下一笔原生工具，不能强行转换已画对象的区域类型；
 * LocalFill 仅涂抹，按周围图像局部填充去除内容，不能作为普通颜色油漆桶。
 * 水印文字由专用文字输入处理，是唯一平铺层；不参与普通对象全选。
 * 折线删顶点、箭头拉直、文字重编辑与对象排列等属于对象操作，不伪装成样式开关。
 */
(function () {
  "use strict";

  const option = (value, label) => ({ value, label });
  const property = (
    key,
    label,
    type,
    value,
    min = null,
    max = null,
    step = null,
    options = [],
  ) => ({ key, label, type, min, max, step, value, options });
  const color = (value = "#ff4b5c", key = "color", label = "颜色") =>
    property(key, label, "color", value);
  const range = (key, label, value, min, max, step = 1) =>
    property(key, label, "range", value, min, max, step);
  const number = (key, label, value, min, max, step = 1) =>
    property(key, label, "number", value, min, max, step);
  const toggle = (key, label, value = false) =>
    property(key, label, "toggle", value);
  const select = (key, label, value, choices) =>
    property(
      key,
      label,
      "select",
      value,
      null,
      null,
      null,
      choices.map((c) => option(c[0], c[1])),
    );
  const opacity = (value = 1, label = "不透明度") =>
    range("opacity", label, value, 0, 1, 0.01);
  const width = (value = 4, min = 1, max = 50, label = "粗细") =>
    range("width", label, value, min, max, 0.25);
  const radius = (key = "radius", label = "圆角") =>
    number(key, label, 0, 0, 2048, 0.25);
  const strokeStyle = () => toggle("dashed", "虚线");
  const fontSize = () => number("fontSize", "字号", 28, 1, 1024);
  const fontFamily = () =>
    select("fontFamily", "字体", "Microsoft YaHei UI", [
      ["Microsoft YaHei UI", "微软雅黑"],
      ["Segoe UI", "Segoe UI"],
      ["Consolas", "Consolas"],
      ["Arial", "Arial"],
      ["SimSun", "宋体"],
      ["Times New Roman", "Times New Roman"],
    ]);
  const basicShape = () => [
    color(),
    width(),
    opacity(),
    toggle("filled", "实心填充"),
    strokeStyle(),
  ];
  const blend = () =>
    select("highlighterBlend", "合成方式", "multiply", [
      ["multiply", "正片叠底"],
      ["alpha", "半透明"],
    ]);
  const strength = (blur = false) =>
    range("blockSize", blur ? "模糊强度" : "色块大小", 16, 2, 64);
  const redactionMode = (effect, region) => [
    select("effect", "效果", effect, [
      ["mosaic", "马赛克"],
      ["blur", "模糊"],
      ["localFill", "局部填充"],
    ]),
    select(
      "region",
      "区域",
      region,
      region === "brush" && effect === "localFill"
        ? [["brush", "涂抹"]]
        : [
            ["box", "框选"],
            ["brush", "涂抹"],
          ],
    ),
  ];
  const tool = (key, label, group, icon, hint, gesture, properties) => ({
    key,
    label,
    group,
    icon,
    hint,
    gesture,
    properties,
  });

  const groups = [
    {
      key: "shape",
      label: "形状",
      icon: "rectangle",
      tools: ["rect", "ellipse", "triangle", "diamond", "star"],
    },
    { key: "line", label: "线条", icon: "arrow", tools: ["arrow", "polyline"] },
    {
      key: "mark",
      label: "标记",
      icon: "markup",
      tools: ["pen", "highlighter", "highlightRect", "spotlight"],
    },
    {
      key: "text",
      label: "文字",
      icon: "text",
      tools: ["text", "serial", "watermark"],
    },
    {
      key: "redact",
      label: "打码",
      icon: "mosaic",
      tools: ["mosaic", "blur", "mosaicBrush", "blurBrush", "localFill"],
    },
    {
      key: "erase",
      label: "擦除",
      icon: "eraser",
      tools: ["pixelEraser", "eraser"],
    },
    {
      key: "lasso",
      label: "套索",
      icon: "lasso",
      tools: ["lasso"],
      standalone: true,
    },
  ];

  const tools = {
    rect: tool(
      "rect",
      "矩形",
      "shape",
      "rectangle",
      "拖动绘制矩形；支持描边、实心填充与独立四角圆角。",
      "box",
      [
        ...basicShape(),
        radius(),
        toggle("independentCorners", "四角独立"),
        radius("radiusTopLeft", "左上圆角"),
        radius("radiusTopRight", "右上圆角"),
        radius("radiusBottomRight", "右下圆角"),
        radius("radiusBottomLeft", "左下圆角"),
      ],
    ),
    ellipse: tool(
      "ellipse",
      "椭圆",
      "shape",
      "ellipse",
      "拖动绘制椭圆；支持描边和实心填充。",
      "box",
      basicShape(),
    ),
    triangle: tool(
      "triangle",
      "三角形",
      "shape",
      "triangle",
      "拖动外接框绘制三角形。",
      "box",
      basicShape(),
    ),
    diamond: tool(
      "diamond",
      "菱形",
      "shape",
      "diamond",
      "拖动外接框绘制菱形。",
      "box",
      basicShape(),
    ),
    star: tool(
      "star",
      "五角星",
      "shape",
      "star",
      "拖动外接框绘制五角星。",
      "box",
      basicShape(),
    ),

    arrow: tool(
      "arrow",
      "箭头 / 直线",
      "line",
      "arrow",
      "拖动绘制；关闭两个箭头端点即为直线，选中后可调整曲线控制点。",
      "box",
      [
        color(),
        width(),
        opacity(),
        toggle("arrowStart", "起点箭头"),
        toggle("arrowEnd", "终点箭头", true),
        select("arrowHead", "箭头形状", "open", [
          ["open", "开口"],
          ["triangle", "实心"],
          ["dart", "尖尾"],
        ]),
        strokeStyle(),
      ],
    ),
    polyline: tool(
      "polyline",
      "折线",
      "line",
      "polyline",
      "连续单击添加顶点，完成后可拖动或删除顶点；支持平滑曲线与闭合填充。",
      "polyline",
      [
        color(),
        width(),
        opacity(),
        strokeStyle(),
        toggle("polylineSmooth", "平滑曲线"),
        toggle("filled", "闭合填充"),
      ],
    ),

    pen: tool(
      "pen",
      "画笔",
      "mark",
      "pen",
      "按住拖动自由绘制；保角平滑保留笔迹转折。",
      "freehand",
      [
        color(),
        width(),
        opacity(),
        strokeStyle(),
        toggle("pencilSmooth", "保角平滑", true),
      ],
    ),
    highlighter: tool(
      "highlighter",
      "荧光笔",
      "mark",
      "highlighter",
      "拖动标记内容；可切换手绘或拉直笔迹。",
      "freehand",
      [
        color("#ffd43b"),
        width(24, 4, 48, "笔宽"),
        opacity(),
        blend(),
        select("highlighterShape", "笔迹", "freehand", [
          ["freehand", "手绘"],
          ["line", "拉直"],
        ]),
      ],
    ),
    highlightRect: tool(
      "highlightRect",
      "高亮框",
      "mark",
      "highlighter-rect",
      "拖动框选需要强调的区域，保留下面的图像细节。",
      "box",
      [color("#ffd43b"), opacity(), blend(), radius()],
    ),
    spotlight: tool(
      "spotlight",
      "聚光灯",
      "mark",
      "spotlight",
      "框选要突出的区域，压暗其余内容；边框和圆角可调。",
      "box",
      [
        color("#ffffff", "color", "边框颜色"),
        width(2, 0, 20, "边框粗细"),
        opacity(0.5, "背景暗化"),
        radius(),
      ],
    ),

    text: tool(
      "text",
      "文字",
      "text",
      "text",
      "单击输入文字；再次编辑可修改内容及文字样式。",
      "text",
      [
        property("text", "文字内容", "text", ""),
        color(),
        fontSize(),
        opacity(),
        fontFamily(),
        toggle("bold", "粗体"),
        toggle("italic", "斜体"),
        toggle("underline", "下划线"),
        toggle("strike", "删除线"),
        select("textAlignment", "文字对齐", "left", [
          ["left", "左对齐"],
          ["center", "居中"],
          ["right", "右对齐"],
        ]),
        toggle("textOutline", "文字描边"),
        color("#ffffff", "textOutlineColor", "描边颜色"),
        number("textOutlineWidth", "描边粗细", 3, 0, 128, 0.25),
        toggle("textBackground", "文字背景"),
        color("#ffffff", "textBackgroundColor", "背景颜色"),
        range("textBackgroundOpacity", "背景不透明度", 0.85, 0, 1, 0.01),
        number("textBackgroundRadius", "背景圆角", 4, 0, 2048, 0.25),
      ],
    ),
    serial: tool(
      "serial",
      "序号",
      "text",
      "serial",
      "单击放置递增序号，编号沿用当前文档已有序号数量。",
      "point",
      [color(), fontSize(), opacity(), fontFamily(), toggle("bold", "粗体")],
    ),
    watermark: tool(
      "watermark",
      "水印",
      "text",
      "watermark",
      "输入文字后平铺整张画布；水印为专用平铺层，可直接调整角度。",
      "text",
      [
        property("text", "水印文字", "text", ""),
        color(),
        fontSize(),
        opacity(0.35),
        number("watermarkAngle", "水印角度", 0, -180, 180),
      ],
    ),

    mosaic: tool(
      "mosaic",
      "马赛克",
      "redact",
      "mosaic",
      "拖动框选打码区域；圆角只影响该区域边界。",
      "box",
      [strength(), radius("effectRadius"), ...redactionMode("mosaic", "box")],
    ),
    blur: tool(
      "blur",
      "模糊",
      "redact",
      "blur",
      "拖动框选需要模糊的区域。",
      "box",
      [strength(true), radius("effectRadius"), ...redactionMode("blur", "box")],
    ),
    mosaicBrush: tool(
      "mosaicBrush",
      "打码笔",
      "redact",
      "mosaic-brush",
      "按住涂抹打码，色块大小与笔宽分别调整。",
      "freehand",
      [
        strength(),
        width(48, 4, 200, "笔宽"),
        ...redactionMode("mosaic", "brush"),
      ],
    ),
    blurBrush: tool(
      "blurBrush",
      "模糊笔",
      "redact",
      "blur-brush",
      "按住涂抹模糊，强度与笔宽分别调整。",
      "freehand",
      [
        strength(true),
        width(48, 4, 200, "笔宽"),
        ...redactionMode("blur", "brush"),
      ],
    ),
    localFill: tool(
      "localFill",
      "局部填充",
      "redact",
      "local-fill",
      "涂抹不需要的内容，按周围图像局部填充；仅支持涂抹。",
      "freehand",
      [width(10, 1, 50, "填充大小"), ...redactionMode("localFill", "brush")],
    ),

    pixelEraser: tool(
      "pixelEraser",
      "局部橡皮擦",
      "erase",
      "eraser",
      "涂抹恢复原图，保留已有标注；不删除整个对象。",
      "freehand",
      [width(16, 4, 80, "直径")],
    ),
    eraser: tool(
      "eraser",
      "对象橡皮擦",
      "erase",
      "object-eraser",
      "点中或擦过即删除整个标注对象，可撤销。",
      "eraser",
      [width(48, 12, 200, "直径")],
    ),
    lasso: tool(
      "lasso",
      "套索裁剪",
      "lasso",
      "lasso",
      "手绘闭合轮廓；Enter 应用，Esc 或右键取消；清除套索可撤销。",
      "lasso",
      [],
    ),
  };

  window.LongEditorTools = { groups, tools };
})();
