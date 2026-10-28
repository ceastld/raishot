# 富文本（HTML）贴图方案（2026-10-19）

状态：六个拍板项 10-19 已按推荐确认（见文末“已定”）；S1 模型与存储已进源码，S2 起未做。

## 现状与目标

- `pin_app.rs::clipboard` 依次认：剪贴板 PNG（含 RaiShot 自己写的屏幕位置）→ GPUI 图片 → `CF_HDROP` 图片文件 → 纯文本。GPUI 的 Windows 剪贴板不读 `HTML Format`。
- 文本走 `pin_text_source::prepare`：一个 `Tool::Text` 对象，固定 22 px 字号、700 px 宽卡片、18 px 留白，不看 DPI；文档不生成底图，纯色底与默认文字色随主题（`remap_theme` 只改仍为默认色的对象）。
- 文字格式只有整对象一份（`Style.font_size/bold/color` + `text_format::TextStyle`）；排版 `text_layout.rs` 用 Uniscribe `ScriptStringAnalyse`，一行一个字体，GDI 白字画进 DIB 取覆盖率再单色合成（`raster.rs`／`text_paint.rs`／`projected_text.rs`）。

目标：网页、Word／WPS、代码编辑器、表格软件复制的内容贴出来保留字体、字号、颜色、粗斜、下划线／删除线、背景、链接、列表、`pre`／`code`、简单表格；可选择、可复制回 HTML。

不做：浏览器／WebView 运行时、脚本、网络请求、通用 CSS 布局（float／flex／grid／定位）。

## 总体结构

```
剪贴板 HTML Format
  → html_fragment.rs   CF_HTML 头 + 宽容分词建树 + CSS 子集 → 计算样式（纯函数，无 Win32）
  → pin_html_source.rs 导入策略：DPI、最大宽度、整卡缩小、主题、降级
  → Annotation{tool: Text, rich: Some(Arc<RichText>)}   rich_text.rs 纯数据模型
  → Editor / Document  照旧：Replace 增量命令、撤销、工作区序列化
  → text_layout.rs     多片段行（纯文字 = 单片段，输出不变）
  → raster / projected_text 逐片段取覆盖率按片段色合成 → Scene / 导出
复制：rich_text::to_html + 纯文本切片，一次打开剪贴板写两种格式
```

GPUI 只接事件和显示文字预览图，不参与排版。

## (a) 数据模型

### 选型：单对象富文本（推荐） vs 拆成多个对象

- **A 单对象**：一个 `Tool::Text` 对象挂 `rich`，段落／表格结构在对象内部。整张卡片一起移动、缩放、复制成 HTML；撤销、序列化、选字沿用现有对象路径。
- **B 多对象**：每段／每个单元格一个文字对象，表格线用矩形对象。不用新排版，但单文档 64 对象上限很快用完；编辑会把结构拆散；复制 HTML 要从对象位置反推表格；选字顺序靠 y／x 排序，容易乱。

选 A。`rich: None` 时一切与现在逐字节相同；截图编辑器里的文字对象不受影响。

### 结构

```rust
// rich_text.rs：无 GPUI、无 Win32，可全量单元测试
pub struct RichText {
    pub runs: Vec<Run>,              // 按 byte 升序、互不重叠；空隙继承对象 Style
    pub paragraphs: Vec<Paragraph>,  // 与 text.split('\n') 一一对应
    pub tables: Vec<Table>,
    pub links: Vec<Arc<str>>,        // 只留 http/https/mailto
    pub card_fill: Option<u32>,      // 来源整卡底色；Some 时卡片底色不随主题
    pub plain: bool,                 // “忽略格式”：数据保留，按对象基准样式绘制
}
pub struct Run { pub range: Range<u32>, pub style: RunStyle }
pub struct RunStyle {                // None = 继承对象样式
    pub font: Option<FontFamily>,    // 运行时字体索引，盘上存名称
    pub size: Option<f32>,           // 相对对象 font_size 的倍数
    pub color: Option<u32>,          // None = 默认色（随主题）
    pub bold: Option<bool>, pub italic: Option<bool>,
    pub underline: Option<bool>, pub strike: Option<bool>,
    pub background: Option<u32>,
    pub link: Option<u16>,
    pub baseline: Baseline,          // Normal / Sub / Super
}
pub struct Paragraph {               // 长度单位都是 em（对象 font_size）
    pub align: Option<Alignment>,
    pub indent: f32, pub first_indent: f32,      // first_indent 可负 = 悬挂缩进
    pub space_before: f32, pub space_after: f32,
    pub line_height: Option<f32>,                // 行高倍数
    pub wrap: bool,                               // pre / nowrap 为 false
    pub background: Option<u32>,                  // 段落底（代码块）
    pub quote_bar: Option<u32>, pub rule_after: Option<u32>,   // 引用条、分隔线
    pub table_row: Option<(u16, u16)>,            // (表, 行)；行内单元格以 '\t' 分隔
}
pub struct Table { pub columns: u16, pub border: Option<u32>, pub padding: f32, pub cells: Vec<Cell> }
pub struct Cell { pub row: u16, pub col: u16, pub col_span: u16, pub header: bool, pub background: Option<u32> }
```

规则：

- **长度全用 em**：`Annotation::scale` 的 `font_size *= scale`、拖边按高度改字号、字号滚轮都会让标题、缩进、行距一起等比，变换代码不用改。
- **列表符号、表格制表符写进 `text`**：纯文本复制、选字、查找、翻译天然带上；段落只管缩进对齐。
- 颜色仍是“浅色基准”：`None` 跟主题，显式色原样。`card_fill` 为 Some 的对象 `remap_theme` 跳过（整卡保留来源配色）。
- 不变量：range 落在字符边界；runs ≤ 16384、paragraphs ≤ 8192、links ≤ 1024（每条 ≤ 2 KiB）、tables ≤ 64、cells ≤ 4096；em 值有限，字号倍数 0.1–10。
- 规范化：合并相邻同样式片段，删掉与对象样式相同的覆盖和空片段。

### 编辑时片段跟着走

- `Editor::set_text(String)` 是整串替换。用新旧串的公共前缀／后缀求出编辑区间，再 `RichText::rebase(old, new)`：区间前不动，区间后平移，区间内收缩；插入文字继承插入点左边字符的样式（段首继承右边）；插入 `\n` 拆段，两段同段落样式；删掉 `\n` 合并段，保留前段样式。输入法组字同一路径。这样不需要改 `text_input.rs` 的输入接口。
- 对象级样式修改（工具栏颜色、字体、粗斜、下划线、删除线）＝设对象样式并清掉所有片段在该属性上的覆盖，相当于“全选后设置”；改字号改的是基准，标题按倍数跟随；描边、对象底色仍是对象级。在 `editor.rs` 现有 `TextStyle::updated(before, after)` 那一段一起处理。
- 局部格式（P2：选中一段设粗体／颜色）以后直接调 `RichText::apply(range, patch)`；本方案不含它的界面。

### Document / 撤销 / Scene

- `Annotation` 新增 `rich: Option<Arc<RichText>>`。Arc 让 Replace 前后快照和 Scene 缓存比较都便宜（先比指针）。需要补字段的结构体字面量约 7 处。
- 撤销沿用 `Change::Replace { before, after }`：一次输入、一次样式修改、一次“忽略格式”各一条，不加新命令种类。
- 历史预算（`MAX_HISTORY_PAYLOADS`、`pin_text_history` 的 `fit_budget`）要计入 rich 的字节数。
- Scene 层以 Annotation 等值为键，rich 变化自然失效；`raster::region` 的范围要包含片段底色、引用条、表格线。

### 序列化（V1／V2／V3）

- `pin_text_document.rs` 的 `WireTextObject` 与 `pin_raster_document.rs` 的 `WireObject` 各加可选 `rich`（`skip_serializing_if = "Option::is_none"`）。两栈历史用的是同一个 DTO，自动带上。
- **版本**（已实施，版本号必须与内容一致，不允许两种写法）：
  - 纯文字贴图：当前对象或两栈任一对象带 rich 写 **3**，否则 1。3 与 1 结构相同。
  - 截图／图片贴图的矢量文档（`WireRasterDocument`）：带 rich 写 **2**，否则 1。
  - 混合文字贴图仍是 2，局部样式由内层 `mixedDocument` 的 2 表达。
  - 裁剪最远历史后重新计算版本，所以最后一个带 rich 的历史步被裁掉时会降回 1。
  原因：旧 DTO 没有 `deny_unknown_fields`，旧版读到 `rich` 会静默丢掉格式，再保存就永久丢了；升版本让旧版明确拒读。`rich` 自身的 DTO 带 `deny_unknown_fields`，以后加字段也不会被本版静默丢弃。
  未做：旧版拒读新版本时，关闭恢复记录和工作区是否都保留原文件不删除，要等 S4 能产生 rich 贴图后实机核对。
- 盘上格式（camelCase，段落用稀疏数组）：

```json
"rich": {
  "runs": [{"start": 0, "end": 12, "size": 1.5, "bold": true},
           {"start": 30, "end": 41, "font": "Consolas", "color": 13238304, "background": 15921906}],
  "paragraphs": [{"index": 0, "spaceAfter": 0.5}, {"index": 3, "indent": 2, "firstIndent": -1.2}],
  "tables": [{"columns": 3, "border": 13421772, "padding": 0.3,
              "cells": [{"row": 0, "col": 0, "colSpan": 1, "header": true}]}],
  "links": ["https://example.com/"],
  "cardFill": 1973790,
  "plain": false
}
```

## (b) CF_HTML 解析

### 读取

- 格式 `HTML Format`（`RegisterClipboardFormatW`），UTF-8；头部 `Version/StartHTML/EndHTML/StartFragment/EndFragment[/SourceURL]`，偏移是 UTF-8 字节偏移，与 `table_export.rs` 写入时同一约定。
- 解析 StartHTML..EndHTML 整段（需要 `<style>` 与片段外祖先元素的继承样式），只输出 StartFragment..EndFragment 之间的内容。偏移缺失、为 -1、越界或不在字符边界时，改找 `<!--StartFragment-->`／`<!--EndFragment-->` 注释，再不行用整段。
- 原始数据上限 8 MiB，超出走纯文本。在 `pin_app.rs` 里自己开剪贴板读取，仿 `export::clipboard_bytes` 做有上限的复制。

### 分派顺序（拍板项 1）

建议：RaiShot 自己写的 PNG（带屏幕位置）→ **HTML 片段有可见文字时走富文本** → 其它图片 → 图片文件 → 纯文本。
浏览器“复制图片”给的 HTML 只有 `<img>`，没有可见文字，仍贴图片。表格软件复制单元格同时给 PNG 和 HTML 表格，按此顺序会贴成文字表格，不再是图片。

### 解析器选型（拍板项 2）

| 方案 | 许可证 | 依赖与体积 | 适配度 |
|---|---|---|---|
| html5ever 0.38 | MIT／Apache-2.0 | markup5ever、web_atoms、string_cache、phf（构建期生成原子表）、tendril 等；不带 DOM，要自写 TreeSink 或再引 rcdom（该 crate 自述不建议生产用） | 规范级容错；样式计算、Office 特判仍要自写 |
| scraper | MIT（依赖含 MPL-2.0 的 cssparser） | html5ever + selectors + cssparser + ego-tree… 最重 | 选择器引擎用不上 |
| 零拷贝小解析器（如 tl） | MIT | 小 | 不处理隐式闭合和条件注释，健壮性不如针对性规则 |
| **自写宽容子集** | 自有 | 零依赖 | 只服务“文字＋样式投影”；剪贴板 HTML 都是程序序列化出来的，结构规整 |

选用第三方组件时须保留其许可证与通知，MPL-2.0 另有文件级义务。以上方案还需按实际用途核对许可、体积和适配度；体积没有实测，选第三方时用 `cargo bloat` 测一次再定。

推荐自写，入口 `html_fragment::parse(html, fragment) -> Result<RichImport>` 隔离；真实样本里遇到自写处理不了的结构，再把分词换成 html5ever，下游不动。

### 自写解析器规则

- **分词**：标签、属性（引号、无引号、无值）、注释、`<!DOCTYPE>`、CDATA；字符引用支持数字和约 60 个常用命名实体（nbsp amp lt gt quot apos copy reg trade hellip mdash ndash 各种引号 bull middot times divide deg plusmn ensp emsp thinsp zwj zwnj shy euro yen pound cent sect para…），未知实体原样保留。
- **建树**：元素栈；空元素（br img hr meta link input col wbr area base source）；遇块元素隐式闭合 `p`，`li`、`dt`／`dd`、`tr`／`td`／`th`、`option`、`thead`／`tbody` 互相隐式闭合；不配对的结束标签向上找同名，找不到就忽略。深度上限 256，元素上限 50 000，超限走纯文本。
- **Office 特判**：`<!--[if …]>…<![endif]-->` 当注释丢弃；`<![if !supportLists]>`／`<![endif]>` 当无效标签忽略、保留内容（列表符号在里面）；`o:p` 等带命名空间的标签只留内容；`mso-hide:all`、`display:none`、`visibility:hidden` 跳过；Symbol／Wingdings 字体的列表符号映射为 •、▪、◦。

### CSS 子集

- 来源：`style` 属性；`<style>` 只取简单选择器（`tag`、`.cls`、`tag.cls`，可逗号分组），带组合符、伪类的规则和 `@media` 等 at 规则整条忽略。优先级：内联 > 类 > 标签 > 继承 > 内置默认样式；`!important` 只在同一级里压过。
- **颜色**：`color`、`background-color`、`background`（只取颜色）、`<font color>`、`bgcolor`。支持 #rgb／#rgba／#rrggbb／#rrggbbaa、`rgb()`／`rgba()`（逗号或空格写法）、`hsl()`、148 个命名色、`transparent`、`windowtext`／`currentColor`／`inherit`。半透明色按所在底色预混成不透明值。
- **字体**：`font-family`、`<font face>` 按顺序找已安装字体；内置中英别名（宋体／SimSun、黑体／SimHei、微软雅黑／Microsoft YaHei、等线／DengXian、楷体／KaiTi、仿宋／FangSong…）；通用族 monospace→Consolas，serif→SimSun／Times New Roman，sans-serif／system-ui→对象默认字体。
- **字号**：px、pt（×4/3）、em／rem／%、cm／mm／in、xx-small…xxx-large、smaller／larger、`<font size=1..7>`，结果限制在 6–144 px。
- **字形**：`font-weight`（bold／bolder／≥600）、`font-style`（italic／oblique）、`text-decoration`／`text-decoration-line`（underline、line-through；none 清除）、`vertical-align: sub／super`（0.75 倍字号、基线偏移）。
- **块级**：`text-align`（justify 按左对齐）、`margin-top／bottom`、`margin-left`／`padding-left`、`text-indent`（可负）、`line-height`（normal／数字／px／%／em）、`white-space`（normal／nowrap／pre／pre-wrap／pre-line）、块 `background-color`、`border-left`（引用条）、`width`（只认最外层容器、`table`、`img`，见 (c)）。
- 其余（float、position、flex、grid、transform、阴影、letter-spacing、分栏…）忽略，按普通块流排。

### 元素映射

- **块**：p div section article header footer main nav aside figure figcaption address center details summary dl dt dd → 段落；h1–h6 → 加粗，字号倍数 2／1.5／1.17／1／0.83／0.67；blockquote → 左缩进＋引用条；hr → 分隔线；pre → 等宽、不换行、保留空白，tab 按 `tab-size`（默认 8）。
- **列表**：ul／ol／li 每级缩进约 2em；符号按层级 •、◦、▪；ol 数字支持 `start` 和 `type`（1／a／A／i／I）；`list-style-type: none` 不出符号。符号＋制表位写进文字，悬挂缩进对齐正文。
- **内联**：b／strong；i／em／cite／var／dfn；u／ins；s／strike／del；code／kbd／samp／tt（等宽＋浅底）；mark（黄底）；small／big；sub／sup；font；a（来源没给样式时用链接色＋下划线）；q（加引号）；ruby（基字后接括号注音）；abbr／span／label。
- `br` → 段内换行（新段落，同段样式，段前距 0）。
- **表格**：table／thead／tbody／tfoot／tr／td／th／caption／colgroup，见 (c)；th 加粗居中。
- **图片** `img`：接入阶段先显示占位（alt 或“[图片]”，次要色）；后续只渲染 `data:` 和本地 `file:` 图片，**不取 http(s)**。
- **丢弃**：script、style（解析完）、template、noscript、head、title、meta、link、iframe、object、embed、video、audio、canvas；svg／math → alt 或“[图形]”；input → value 或 placeholder；select → 选中项；textarea、button → 其文字。

### 空白

- normal：连续空白合成一个，块首尾去掉，跨内联元素合并。U+00A0 保留，排版时不在此断行，纯文本复制时换成普通空格。
- pre／pre-wrap 保留空格和换行；片段末尾的空行去掉。

### 降级与保护

- 解析失败、超限、没有可见文字 → 走现有纯文本路径，不报错。
- **内容校验**：导入结果去掉空白和列表符号后的字符数，与剪贴板纯文本相差超过 10% 时视为解析可疑，退回纯文本，防止解析器 bug 吞掉内容。剪贴板没有纯文本时不校验。
- 不执行脚本、不发网络请求、不读 SourceURL；链接只留 http／https／mailto。
- 有些浏览器复制的 HTML 只带类名、不带样式表，这时只能得到标签语义（粗体、标题、列表、代码），属于来源限制。

## (c) 排版

### 引擎（拍板项 3，技术项，有推荐）

- **扩展现有 Uniscribe 排版（推荐）**：纯文字是单片段特例，可以加“输出逐字节不变”的对照测试；字体回退（`SSA_FALLBACK`）、光标停点、分块 `draw_between`、世界变换投影、覆盖率合成都复用。缺点：跨片段的双向文字重排不做（单片段内 Uniscribe 仍处理），彩色 emoji 仍是单色（与现在一致）。
- **DirectWrite `IDWriteTextLayout`**：天然支持多字体／字号区间、双向文字、命中与选区矩形、最小宽度、内联对象、彩色字形。`windows` 0.61.3 已在依赖图里开了 `Win32_Graphics_DirectWrite`，不增加体积。缺点：是第二套排版，字形灰度与现在的 GDI 输出不同，同一贴图里新旧文字观感不一；要实现 COM 渲染回调；光标、选字、分块绘制要重接。

先用 Uniscribe；以后需要双向文字或彩色 emoji 时再评估 DirectWrite。

### 多片段行（`text_layout.rs`）

- 段落按 run 切片段，每段用自己的 `FontContext` 测宽；断行机会用段落级 `ScriptItemize`＋`ScriptBreak`（与字体无关）。超宽时普通文字在任意字素处断（与现在一致，避免一个长 URL 让整卡缩小），`wrap = false` 的段落不断。
- 行高 = 各片段最大上升 + 最大下降，再乘行高倍数；片段按基线对齐，sub／super 偏移。行内片段按逻辑顺序从左往右排。
- `Row` 扩成多个片段；`caret_stops`／`position`／`hit`／`selection`／`visual_line_at` 接口不变，所以选字、光标、命中代码不用改。
- **表格行**：列宽 = 该列各单元格内容最宽者＋左右内边距，跨列平分；第一版单元格不换行。表格线和单元格底色作为装饰矩形。
- **装饰**：片段底色、段落底色、引用条、分隔线、表格线统一成装饰矩形列表，绘制时先画。

### 绘制

- 每个片段在自己的小临时面上画白字取覆盖率（框外扩半个行高，容纳斜体和下伸部分），再按片段颜色合成。像素量约等于文字行面积，不随颜色数增长；分块和世界变换投影照旧。
- 对象透明度、描边、对象底色仍在最后按对象统一合成一次（`text_paint` 现有规则）；描边对整个覆盖率掩码膨胀，颜色是对象级。

### 卡片尺寸

- **最大宽度默认 800 DIP，含左右留白**（留白沿用 18 DIP，内容最宽 764 DIP）。
- 内容最长行比上限短时卡片收窄到内容宽＋留白（最小 160 DIP），短片段不再是一张大空卡。
- **不可断内容**（pre 行、nowrap、表格、有固定宽度的最外层容器或图片）宽度 U 大于可用宽 A：整卡等比缩小 f = A/U（基准字号带着所有 em 一起缩），重排一次，必要时再迭代一次消化字形取整；f 下限 0.5，到下限仍放不下就让卡片超过 800 DIP，交给现有 `pin_model::fit` 的初始缩放适配屏幕。不截断、不报错。
- 最外层容器有显式 `width`（如 600px 的邮件模板）时按该宽度排，超过上限同上缩小。
- 高度 = 排版高度＋留白，仍受现有 16000 px／3200 万像素上限，过长提示分段（与纯文字一致）。

### DPI

- HTML 的 px 视为 DIP；导入时乘以贴图出现处所在显示器的缩放，文档按物理像素保存，初始缩放 100%。之后跨屏移动不重排（与图片贴图一致，矢量文字在任意倍率都清晰）。
- 现有纯文字贴图固定 700 px、22 px、不看 DPI，是否统一见拍板项 5。

### 主题（拍板项 4）

- **A 混合（推荐）**：正文主色（按字符数最多的颜色）若是低饱和近黑、且来源底为白或透明，视为默认色（`color = None`，随主题）；其它显式颜色原样。来源有整卡底色（最外层容器或 body 的非白底，例如代码编辑器暗底）时，`card_fill` 记下来源底色，所有颜色显式化，整卡不随主题。行内 code、mark、单元格等局部底色原样。
- **B 完全保留来源**：底色＝来源底色（缺省白），颜色全原样，像网页截图，不随主题。
- **C 完全跟随主题**：丢弃来源颜色和背景，只留字号、粗斜、字体、结构。

A 符合“只有纯色底和默认色文字随主题”的现有约束。它的缺口：深色主题下，来源里的深色强调色（如深蓝链接）落在深底上对比度不足。要不要在深色主题下把对比度低于 3:1 的显式色提亮（显示和导出都提亮，存盘值不变），需要放宽这条约束，单独拍板。

### 忽略格式

- 贴图右键“忽略格式／显示格式”切换 `rich.plain`：按对象基准字体、字号、默认色绘制，文字里的列表符号和表格制表符保留。一次切换＝一条 Replace 命令，可撤销。
- 矢量文字贴图的文档尺寸目前创建后不变，切换后排版高度会变。实现时先确认能否原位改文档尺寸；不能的话，切换就在同一位置按新尺寸重贴一张并关闭原贴图（不进撤销，菜单可再切回）。已有其它标注时，标注位置不跟着重排。
- 设置项（默认显示 HTML 样式、最大宽度、留白）留到后续；第一阶段用常量。

### 滚轮缩放

保持现状：显示倍率变化只做投影重绘（`text_preview.rs`，不重新换行），富文本不另加“停稳后重排”，因为本来就不按倍率重排。

## (d) 复制、拖出、选字

- **选字**：原生选字（`pin_text_selection::SelectionText::from_annotations`）读的是 `TextLayout` 的行和光标停点。多片段行接口不变，所以按视觉行选择、双击选词、Ctrl+A、查找、翻译、复制段落直接可用。富文本贴图不跑 OCR（与纯文字贴图一致）。
- **复制**（拍板项 6）：
  - 选中文字后 Ctrl+C／“复制所选”、Shift+C“复制文字”：一次打开剪贴板，同时写 `CF_UNICODETEXT` 和 `HTML Format`（选区内的片段切出来序列化）。`table_clipboard.rs` 已有一次发布多种格式的实现可复用。
  - 无选区 Ctrl+C：仍只复制图片（现状），不夹带 HTML，否则粘贴到 Word／WPS 时会优先取 HTML，拿不到图。
- **HTML 生成**（`rich_text::to_html`）：段落 `<p style>`、片段 `<span style>`、链接 `<a href>`；列表段落按缩进和符号还原 `<ul>`／`<ol><li>`（符号不重复输出）；表格还原 `<table>`。默认色不写 color（粘贴后用目标程序的默认色），显式色写十六进制；字号换回 DIP（em × 基准 ÷ DPI 缩放）。CF_HTML 头部从 `table_export.rs` 抽成共用函数。
- **纯文本**：直接切 `text`（U+00A0 换成空格）；表格是 TSV，列表带符号。
- **拖出**：Ctrl 拖出 PNG 文件不变；拖动已选文字（新增，后续阶段）时数据对象带 `CF_UNICODETEXT`＋`HTML Format`。
- **往返**：复制出的 HTML 再贴图，应还原成同样的富文本贴图（测试覆盖）。

## (e) 分步计划与验证

| 步 | 内容 | 主要文件 | 验证 |
|---|---|---|---|
| S0 | 等文字渲染提速的未提交改动落地（它在改 `text_layout.rs`、`text_paint.rs`、`projected_text.rs`、`text_preview.rs`），避免同文件并改 | — | `git diff` 这四个文件为空或已提交 |
| S1 模型与存储 | `RichText` 类型、校验、规范化、`rebase`、`apply`、`plain_text`、`slice`；`Annotation.rich`；对象级样式清覆盖；Wire `rich` 字段与 v3 规则；历史预算 | 新 `rich_text.rs`；`annotation.rs`、`editor.rs`、`pin_text_document.rs`、`pin_text_history.rs`、`pin_raster_document.rs`、`lib.rs` | `cargo test --lib rich_text`、`pin_text_document`、`pin_raster_document`；无 rich 的 v1／v2 输出字节不变；v3 往返；模拟旧版拒读 v3；rebase 随机编辑与逐字重建对照 |
| S2 排版与绘制 | 多片段行、基线、段落缩进／间距／行高、不换行段、表格列、装饰矩形；逐片段覆盖率合成；`region` 范围 | `text_layout.rs`、`raster.rs`、`projected_text.rs`、`text_paint.rs` | 单片段 rich 与纯文字逐像素一致；现有 `text_`、`projected_text`、`raster`、`scene`、`pin_text` 过滤测试；新增混合字号基线、跨片段断行、pre 不断行、表格列宽、多色合成测试 |
| S3 HTML 导入 | CF_HTML 头、分词、建树、Office 特判、CSS 子集、元素映射、空白、内容校验 → `RichImport` | 新 `html_fragment.rs`；夹具 `fixtures/clipboard-html/` | `cargo test --lib html_fragment`。夹具**自行编写**，模拟各类来源的结构（带内联计算样式的网页、只有类名的网页、Office 列表＋条件注释、Office 表格、代码编辑器暗底 pre、表格软件单元格、坏偏移、缺片段注释、深层嵌套、实体）；忽略测试测 200 KB 片段解析＋排版耗时 |
| S4 接入贴图 | 剪贴板读取与分派；`pin_html_source::prepare`（DPI、800 DIP、收窄／整卡缩小、主题策略 A、内容校验）；`card_fill` 接到贴图底色与导出；“忽略格式”；复制所选／复制文字写 HTML＋纯文本 | `pin_app.rs`、新 `pin_html_source.rs`、`pin_text_source.rs`、`pin_view.rs`、`pin_menu.rs`、`table_export.rs`（抽 CF_HTML 头） | `cargo check`；分派决策写成纯函数并测试；HTML 往返测试；调试构建换进 tray-run。真实来源实测由维护者复制后贴图，做不了就写“真实界面未验” |
| S5 增强 | 单元格换行表格（最小／最大内容宽分配列宽）、`data:`／本地 `file:` 内联图片、拖出选中文字、链接右键“复制链接／打开链接”、设置项 | 视实现 | 各自过滤测试；图片解码有尺寸上限 |
| S6（P2，可选） | 局部文字格式界面：选中一段设颜色、粗斜、字号 | `editor.rs`、二级栏 | 复用 S1 的 `apply` 和撤销测试 |

另外可加一个开发用命令行开关，把当前剪贴板 `HTML Format` 原样存成文件，方便收集真实样本对照；真实样本只放本机，不进仓库。

## 已定（10-19 用户确认，全部按推荐）

1. **分派优先级**：HTML 有可见文字就贴富文本，哪怕剪贴板同时有第三方图片（表格软件复制单元格会变成文字表格贴图）。
2. **解析器**：自写宽容子集，零依赖。
3. **排版引擎**：扩展现有 Uniscribe。
4. **主题策略**：A 混合。深色主题下低对比显式色不提亮，保持“主题只改纯色底和默认色文字”的约束；真实样本里问题明显时再单独提。
5. **纯文字贴图**：S4 之后单独一步再统一 DPI 与 800 DIP 规则。
6. **无选区 Ctrl+C**：保持只复制图片。

## S1 实施记录（10-19）

- `rich_text.rs`（经 `annotation.rs` 以 `annotation::rich` 引入，不改 `lib.rs`）：`RichText`／`Run`／`RunStyle`／`Paragraph`／`Table`／`Cell`，`validate`、`normalize`、`replace`（精确区间）、`rebase`（整串替换按公共前后缀推区间）、`apply`、`clear`、`slice`、`map_fonts`。
- `Annotation.rich: Option<Arc<RichText>>`；`valid()` 校验 rich 只在文字对象上且与正文一致；`clear_rich_overrides` 在 `Editor::set_style` 里调用（字体、颜色、粗斜、下划线、删除线）。
- 编辑：新增 `Editor::replace_text(range, inserted)`，`text_input.rs` 的输入改走它（精确区间，插入继承插入点样式）；其它调用 `set_text` 的地方走 `rebase`。
- 主题：`remap_theme` 跳过 `card_fill` 为 Some 的对象。
- 存储：`rich_text_wire.rs`（`pin_raster_document::rich_wire`）是两种文档共用的 DTO；三处严格解码（纯文字历史、混合文字桥、图片贴图）都用同一套字体符号规则解析片段字体，验证完再映射本机字体。
- 盘上上限：rich 计入现有文档字节上限（纯文字 240／480 KiB 等）。样式很碎的长网页可能先撞到这个上限而保存失败；S4 导入时要控制片段数。

## 已知限制

- 跨片段双向文字不重排；彩色 emoji 仍单色（与现有纯文字贴图一致）。
- 第一版表格单元格不换行，宽表格靠整卡缩小。
- 只有类名、没有样式表的 HTML 只能保留标签语义。
- 不渲染远程图片。
