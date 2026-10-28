//! 二级栏控件清单：按工具、选中上下文和收起级别列出控件与估算宽度，布局估算与绘制共用一份规则。
//! 级别：0 全部展开；1 分段收成下拉；2 色块收成颜色按钮；3 工具专属组收进“样式”弹层。
//! 每一级都按“信息｜外观｜专属｜对象”整组换行，两行放得下就停在这一级。
use crate::annotation::Tool;

/// 二级栏行高、行距与控件间距（逻辑像素），与 toolbar_widgets 的绘制一致。
pub(crate) const ROW_HEIGHT: f32 = 32.;
pub(crate) const ROW_GAP: f32 = 4.;
pub(crate) const ITEM_GAP: f32 = 4.;
/// 组间竖线 1 + 两侧外边距各 4 + 两侧控件间距各 4。
pub(crate) const GROUP_SEPARATOR: f32 = 17.;
/// 边框 1 + 内边距 4，左右（上下）合计。
pub(crate) const BAR_INSET: f32 = 10.;
pub(crate) const ICON_BUTTON: f32 = 28.;
pub(crate) const SEGMENT: f32 = 26.;
pub(crate) const SWATCH: f32 = 26.;
/// 拆分开关、主栏分组右半的下拉角宽度。
pub(crate) const CARET: f32 = 14.;
/// 字体名超过这个宽度省略。
pub(crate) const FAMILY_MAX: f32 = 96.;
/// 数值框、下拉、颜色按钮的 1px 描边，左右合计。
const FIELD_BORDER: f32 = 2.;
pub(crate) const MAX_LEVEL: u8 = 3;

/// 常用 8 色；自定义色块接在后面。
pub(crate) const SWATCHES: [(&str, u32); 8] = [
    ("red", 0xff4b5c),
    ("orange", 0xff934f),
    ("yellow", 0xffd34d),
    ("green", 0x39c996),
    ("blue", 0x5796ff),
    ("purple", 0xbc8bff),
    ("white", 0xffffff),
    ("black", 0x161b22),
];

pub(crate) fn segmented_width(segments: usize) -> f32 {
    SEGMENT * segments as f32 + 2. * segments.saturating_sub(1) as f32 + 4.
}
/// 数值框：描边 2 + 左 6 + 图标 16 + 5 + 数字 + 5 + 下拉角 9 + 右 5。
pub(crate) fn value_field_width(text: f32) -> f32 {
    46. + FIELD_BORDER + text
}
/// 下拉：描边 2 + 左 6 + 内容 + 5 + 下拉角 9 + 右 5。
pub(crate) fn dropdown_width(content: f32) -> f32 {
    content + 25. + FIELD_BORDER
}
/// 数值框数字区预留宽度：按该项可能出现的最宽文本取整，绘制与估算共用。
pub(crate) fn value_text_width(samples: &[&str]) -> f32 {
    samples
        .iter()
        .map(|sample| text_width(sample, 12.))
        .fold(0., f32::max)
        .ceil()
}

/// 微软雅黑 UI 的 ASCII 字宽（千分之一字号，向上取整），覆盖 0x20–0x7E。
const ASCII_ADVANCE: [u16; 95] = [
    296, 313, 436, 639, 587, 890, 870, 257, 334, 334, 455, 742, 241, 433, 241, 428, //
    587, 587, 587, 587, 587, 587, 587, 587, 587, 587, 241, 241, 742, 742, 742, 483, //
    1032, 704, 628, 669, 762, 550, 532, 744, 774, 294, 396, 635, 514, 977, 813, 815, //
    612, 815, 653, 578, 574, 747, 677, 1018, 645, 604, 620, 334, 416, 334, 742, 449, //
    295, 553, 639, 502, 640, 568, 347, 640, 616, 266, 267, 545, 266, 937, 617, 636, //
    639, 640, 382, 463, 373, 617, 525, 790, 507, 530, 492, 334, 269, 334, 742,
];

/// 估算栏内文字宽度：ASCII 用界面字体实宽，中日韩字符按字号，其余符号按 0.6 字号。
/// 估算偏窄会让控件超出窗口被裁，所以每个字宽都向上取。
pub(crate) fn text_width(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|c| match c as u32 {
            code @ 0x20..=0x7e => size * ASCII_ADVANCE[(code - 0x20) as usize] as f32 / 1000.,
            0xb0 => size * 0.408, // °
            0xb7 => size * 0.241, // ·
            0x2e80.. => size,
            _ => size * 0.6,
        })
        .sum()
}
fn text_button_width(text: &str) -> f32 {
    text_width(text, 12.) + 22.
}
fn hint_width(text: &str) -> f32 {
    text_width(text, 12.) + 12.
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Group {
    Info,
    Look,
    Tool,
    Object,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hint {
    Smart,
    PixelEraser,
    Eraser,
    Lasso,
    LassoCancel,
    /// 多选：（支持圆角的数量，选中数量）。
    RadiusApplies(usize, usize),
    RadiusNone(usize),
}
impl Hint {
    pub(crate) fn text(self) -> String {
        match self {
            Self::Smart => tr!(
                "点击切换区域 · 拖动多选 · Ctrl+A 全选",
                "Click to toggle · drag to add · Ctrl+A all"
            )
            .to_owned(),
            Self::PixelEraser => {
                tr!("恢复原图，保留标注", "Restores image, keeps marks").to_owned()
            }
            Self::Eraser => tr!("点中即删除整个对象", "Deletes the whole object hit").to_owned(),
            Self::Lasso => tr!("手绘闭合轮廓", "Draw closed outline").to_owned(),
            Self::LassoCancel => tr!("Esc / 右键取消", "Esc / right-click: cancel").to_owned(),
            Self::RadiusApplies(supported, count) => trf!(
                "圆角适用 {supported}/{count}",
                "Radius applies to {supported}/{count}"
            ),
            Self::RadiusNone(count) => trf!(
                "所选 {count} 个对象均不支持圆角",
                "None of the {count} objects support radius"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    // 信息
    Count(usize),
    /// 水印文字输入与清空；宽形态用于第 0–1 级。
    WatermarkInput(bool),
    // 外观
    Swatches,
    /// 收起后的颜色按钮；第 3 级附带不透明度。
    ColorButton(bool),
    Width,
    SpotBorder,
    Font,
    Opacity,
    Dim,
    Strength,
    /// 打码笔笔宽、填充大小、橡皮直径。
    BrushSize,
    // 工具专属
    FillMode,
    FillMenu,
    LineStyle,
    LineStyleMenu,
    /// 端点 无/单/双 + 头型 开口/实心/尖尾两组分段。
    ArrowEnds,
    ArrowMenu,
    Straighten,
    PolylineSmooth,
    PolylineClosed,
    DeleteNode,
    PenSmooth,
    Blend,
    BlendMenu,
    /// 荧光笔笔迹 手绘/拉直。
    Stroke,
    StrokeMenu,
    Radius,
    FontFamily,
    /// 粗体、斜体、下划线、删除线（多选分段）。
    Glyphs,
    /// 第 1 级起只留粗体，其余字形与对齐收进“文字样式”。
    BoldOnly,
    TextStyleMenu,
    Align,
    TextOutline,
    TextBackground,
    SerialBold,
    WatermarkAngle,
    /// 打码效果 马赛克/模糊/填充。
    Effect,
    /// 打码区域 框选/涂抹。
    Region,
    SmartEffect,
    Smart,
    SmartDone,
    LassoApply,
    LassoClear,
    Hint(Hint),
    StyleMenu,
    // 对象
    EditText,
    Angle,
    Arrange,
    Delete,
}
impl Item {
    pub(crate) fn width(self) -> f32 {
        match self {
            Self::Count(count) => text_width(&trf!("{count} 个对象", "{count} objects"), 12.) + 18.,
            Self::WatermarkInput(wide) => (if wide { 168. } else { 120. }) + 2. + ICON_BUTTON,
            Self::Swatches => SWATCH * (SWATCHES.len() + 1) as f32,
            Self::ColorButton(opacity) => {
                // 描边 2 + 左 6 + 色点 18 + 5 + 下拉角 9 + 右 5；不透明度再加 5 + 数字。
                let base = 43. + FIELD_BORDER;
                if opacity {
                    base + 5. + percent_text_width()
                } else {
                    base
                }
            }
            Self::Width
            | Self::SpotBorder
            | Self::Font
            | Self::Strength
            | Self::BrushSize
            | Self::Opacity
            | Self::Dim
            | Self::Radius
            | Self::Angle
            | Self::WatermarkAngle => value_field_width(self.value_text_width()),
            Self::FillMode | Self::LineStyle | Self::Blend | Self::Stroke => segmented_width(2),
            Self::Region | Self::SmartEffect => segmented_width(2),
            Self::Effect | Self::Align => segmented_width(3),
            Self::ArrowEnds => segmented_width(3) * 2. + ITEM_GAP,
            Self::Glyphs => segmented_width(4),
            Self::BoldOnly => segmented_width(1),
            Self::FillMenu
            | Self::LineStyleMenu
            | Self::BlendMenu
            | Self::StrokeMenu
            | Self::TextStyleMenu
            | Self::StyleMenu
            | Self::Arrange => dropdown_width(16.),
            Self::ArrowMenu => dropdown_width(30.),
            Self::FontFamily => dropdown_width(FAMILY_MAX),
            Self::TextOutline | Self::TextBackground => ICON_BUTTON + CARET,
            Self::Straighten
            | Self::PolylineSmooth
            | Self::PolylineClosed
            | Self::DeleteNode
            | Self::PenSmooth
            | Self::SerialBold
            | Self::Smart
            | Self::EditText
            | Self::Delete => ICON_BUTTON,
            Self::SmartDone => text_button_width(tr!("完成选择", "Done")),
            Self::LassoApply => text_button_width(tr!("应用 · Enter", "Apply · Enter")),
            Self::LassoClear => text_button_width(tr!("清除套索", "Clear lasso")),
            Self::Hint(hint) => hint_width(&hint.text()),
        }
    }
}

impl Item {
    /// 数值框数字区的预留宽度；非数值项为 0。
    pub(crate) fn value_text_width(self) -> f32 {
        match self {
            // 取值上限两位整数，滚轮半档出现一位小数。
            Self::Width | Self::SpotBorder | Self::Font => value_text_width(&["99", "9.5"]),
            Self::Strength => value_text_width(&["99"]),
            Self::BrushSize => value_text_width(&["200"]),
            Self::Opacity | Self::Dim => percent_text_width(),
            Self::Radius => value_text_width(&[
                "99",
                "9.5",
                radius_independent_label(),
                radius_mixed_label(),
            ]),
            Self::Angle | Self::WatermarkAngle => value_text_width(&["-180°"]),
            _ => 0.,
        }
    }
}

/// 百分比数值（最宽 100%）的预留宽度。
pub(crate) fn percent_text_width() -> f32 {
    value_text_width(&["100%"])
}

/// 多选圆角不一致时数值框显示的文字。
pub(crate) fn radius_mixed_label() -> &'static str {
    tr!("混合", "Mixed")
}

/// 矩形四角分设时数值框显示的文字。
pub(crate) fn radius_independent_label() -> &'static str {
    tr!("四角", "4 sides")
}

/// 决定控件清单的上下文；布局只知道工具和数量时用 `estimate` 做保守估计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PropertyContext {
    pub tool: Tool,
    /// 选中对象数；0 表示为下一笔设置样式。
    pub count: usize,
    /// 多选时支持圆角的对象数。
    pub radius_targets: usize,
    pub rotatable: bool,
    pub edit_text: bool,
    pub curved_arrow: bool,
    pub radius: bool,
    /// 正在输入文字时不显示对象段。
    pub text_draft: bool,
}
impl PropertyContext {
    /// 布局估算：按最宽的可能形态预留（对象段含编辑、角度，箭头含拉直，多选全部支持圆角）。
    pub(crate) fn estimate(tool: Tool, count: usize) -> Self {
        Self {
            tool,
            count,
            radius_targets: count,
            rotatable: count > 1
                || !matches!(
                    tool,
                    Tool::Arrow | Tool::Polyline | Tool::Serial | Tool::Watermark
                ),
            edit_text: count == 1 && tool == Tool::Text,
            curved_arrow: count == 1 && tool == Tool::Arrow,
            radius: tool.supports_radius(),
            text_draft: false,
        }
    }
}

fn colored(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::Pen
            | Tool::Rectangle
            | Tool::Ellipse
            | Tool::Triangle
            | Tool::Diamond
            | Tool::Star
            | Tool::Arrow
            | Tool::Polyline
            | Tool::Highlighter
            | Tool::HighlighterRect
            | Tool::Text
            | Tool::Serial
            | Tool::Spotlight
            | Tool::Watermark
    )
}

/// 工具专属控件：（展开形态，第 1–2 级形态，是否永不收进“样式”）。
type ToolControl = (&'static [Item], &'static [Item], bool);

fn tool_controls(ctx: &PropertyContext) -> Vec<ToolControl> {
    use Item::*;
    let line_style: ToolControl = (&[LineStyle], &[LineStyleMenu], false);
    let radius: ToolControl = (&[Radius], &[Radius], false);
    let mut out: Vec<ToolControl> = Vec::new();
    match ctx.tool {
        Tool::Rectangle | Tool::Ellipse | Tool::Triangle | Tool::Diamond | Tool::Star => {
            out.push((&[FillMode], &[FillMenu], false));
            out.push(line_style);
            if ctx.radius {
                out.push(radius);
            }
        }
        Tool::Arrow => {
            out.push((&[ArrowEnds], &[ArrowMenu], false));
            out.push(line_style);
            if ctx.curved_arrow {
                out.push((&[Straighten], &[Straighten], false));
            }
        }
        Tool::Polyline => {
            out.push(line_style);
            out.push((&[PolylineSmooth], &[PolylineSmooth], false));
            out.push((&[PolylineClosed], &[PolylineClosed], false));
            out.push((&[DeleteNode], &[DeleteNode], false));
        }
        Tool::Pen => {
            out.push(line_style);
            out.push((&[PenSmooth], &[PenSmooth], false));
        }
        Tool::Highlighter => {
            out.push((&[Blend], &[BlendMenu], false));
            out.push((&[Stroke], &[StrokeMenu], false));
        }
        Tool::HighlighterRect => {
            out.push((&[Blend], &[BlendMenu], false));
            if ctx.radius {
                out.push(radius);
            }
        }
        Tool::Spotlight => {
            if ctx.radius {
                out.push(radius);
            }
        }
        Tool::Text => {
            out.push((&[FontFamily], &[FontFamily], false));
            out.push((&[Glyphs], &[BoldOnly, TextStyleMenu], false));
            out.push((&[Align], &[], false));
            out.push((&[TextOutline], &[TextOutline], false));
            out.push((&[TextBackground], &[TextBackground], false));
        }
        Tool::Serial => {
            out.push((&[FontFamily], &[FontFamily], false));
            out.push((&[SerialBold], &[SerialBold], false));
        }
        Tool::Watermark => out.push((&[WatermarkAngle], &[WatermarkAngle], false)),
        Tool::Mosaic | Tool::Blur | Tool::MosaicBrush | Tool::BlurBrush | Tool::LocalFill => {
            out.push((&[Effect], &[Effect], false));
            out.push((&[Region], &[Region], false));
            if ctx.radius && matches!(ctx.tool, Tool::Mosaic | Tool::Blur) {
                out.push(radius);
            }
            out.push((&[Smart], &[Smart], false));
        }
        Tool::SmartMosaic => {
            out.push((&[SmartEffect], &[SmartEffect], false));
            out.push((&[SmartDone], &[SmartDone], true));
            out.push((&[Hint(self::Hint::Smart)], &[Hint(self::Hint::Smart)], true));
        }
        Tool::PixelEraser => out.push((
            &[Hint(self::Hint::PixelEraser)],
            &[Hint(self::Hint::PixelEraser)],
            true,
        )),
        Tool::Eraser => out.push((
            &[Hint(self::Hint::Eraser)],
            &[Hint(self::Hint::Eraser)],
            true,
        )),
        Tool::Lasso => {
            out.push((&[Hint(self::Hint::Lasso)], &[Hint(self::Hint::Lasso)], true));
            out.push((&[LassoApply], &[LassoApply], true));
            out.push((&[LassoClear], &[LassoClear], true));
            out.push((
                &[Hint(self::Hint::LassoCancel)],
                &[Hint(self::Hint::LassoCancel)],
                true,
            ));
        }
        Tool::Crop | Tool::Edit => {}
    }
    out
}

/// 某一级的控件清单，按组顺序排列。
pub(crate) fn items(ctx: &PropertyContext, level: u8) -> Vec<(Group, Item)> {
    let mut out = Vec::new();
    let tool = ctx.tool;
    let objects = |out: &mut Vec<(Group, Item)>| {
        if ctx.count > 0 && !ctx.text_draft && tool != Tool::Watermark {
            if ctx.edit_text {
                out.push((Group::Object, Item::EditText));
            }
            if ctx.rotatable {
                out.push((Group::Object, Item::Angle));
            }
            out.push((Group::Object, Item::Arrange));
            out.push((Group::Object, Item::Delete));
        }
    };
    if ctx.count > 1 {
        out.push((Group::Info, Item::Count(ctx.count)));
        if ctx.radius_targets > 0 {
            out.push((Group::Tool, Item::Radius));
            out.push((
                Group::Tool,
                Item::Hint(Hint::RadiusApplies(ctx.radius_targets, ctx.count)),
            ));
        } else {
            out.push((Group::Tool, Item::Hint(Hint::RadiusNone(ctx.count))));
        }
        objects(&mut out);
        return out;
    }
    if tool == Tool::Watermark {
        out.push((Group::Info, Item::WatermarkInput(level <= 1)));
    }
    let look = |item| (Group::Look, item);
    if colored(tool) {
        out.push(look(if level <= 1 {
            Item::Swatches
        } else {
            Item::ColorButton(level >= 3 && tool != Tool::Spotlight)
        }));
    }
    if matches!(
        tool,
        Tool::Rectangle
            | Tool::Ellipse
            | Tool::Triangle
            | Tool::Diamond
            | Tool::Star
            | Tool::Arrow
            | Tool::Polyline
            | Tool::Pen
            | Tool::Highlighter
    ) {
        out.push(look(Item::Width));
    }
    if tool == Tool::Spotlight {
        out.push(look(Item::SpotBorder));
    }
    if matches!(tool, Tool::Text | Tool::Serial | Tool::Watermark) {
        out.push(look(Item::Font));
    }
    if colored(tool) && tool != Tool::Spotlight && level < 3 {
        out.push(look(Item::Opacity));
    }
    if tool == Tool::Spotlight {
        out.push(look(Item::Dim));
    }
    if matches!(
        tool,
        Tool::Mosaic | Tool::Blur | Tool::MosaicBrush | Tool::BlurBrush | Tool::SmartMosaic
    ) {
        out.push(look(Item::Strength));
    }
    if matches!(
        tool,
        Tool::MosaicBrush | Tool::BlurBrush | Tool::LocalFill | Tool::PixelEraser | Tool::Eraser
    ) {
        out.push(look(Item::BrushSize));
    }
    let controls = tool_controls(ctx);
    if level >= 3 && controls.iter().any(|c| !c.2) {
        out.push((Group::Tool, Item::StyleMenu));
        for control in controls.iter().filter(|c| c.2) {
            out.extend(control.0.iter().map(|&item| (Group::Tool, item)));
        }
    } else {
        for control in &controls {
            let list = if level == 0 { control.0 } else { control.1 };
            out.extend(list.iter().map(|&item| (Group::Tool, item)));
        }
    }
    if ctx.count == 1 {
        objects(&mut out);
    }
    out
}

/// “样式”弹层里逐行显示的专属控件（展开形态）。
pub(crate) fn style_rows(ctx: &PropertyContext) -> Vec<Item> {
    tool_controls(ctx)
        .into_iter()
        .filter(|c| !c.2)
        .flat_map(|c| c.0.iter().copied())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Item(usize),
    Separator,
}

/// 按组换行：整组放得下就同行（组间竖线），否则整组换到下一行；单组超宽才在组内断开。
pub(crate) fn pack(items: &[(Group, Item)], inner: f32) -> Vec<Vec<Slot>> {
    let widths: Vec<f32> = items.iter().map(|(_, item)| item.width()).collect();
    pack_widths(items, &widths, inner)
}

fn pack_widths(items: &[(Group, Item)], widths: &[f32], inner: f32) -> Vec<Vec<Slot>> {
    let mut rows: Vec<Vec<Slot>> = vec![Vec::new()];
    let mut used = 0_f32;
    let mut start = 0;
    while start < items.len() {
        let group = items[start].0;
        let end = items[start..]
            .iter()
            .position(|(g, _)| *g != group)
            .map_or(items.len(), |offset| start + offset);
        let span = &widths[start..end];
        let group_width = span.iter().sum::<f32>() + ITEM_GAP * (span.len() - 1) as f32;
        let row_empty = rows.last().is_none_or(Vec::is_empty);
        let need = if row_empty {
            group_width
        } else {
            GROUP_SEPARATOR + group_width
        };
        if used + need <= inner {
            let row = rows.last_mut().expect("至少一行");
            if !row_empty {
                row.push(Slot::Separator);
            }
            row.extend((start..end).map(Slot::Item));
            used += need;
        } else {
            if !row_empty {
                rows.push(Vec::new());
                used = 0.;
            }
            for index in start..end {
                let row_empty = rows.last().is_none_or(Vec::is_empty);
                let add = if row_empty {
                    widths[index]
                } else {
                    ITEM_GAP + widths[index]
                };
                if used + add > inner && !row_empty {
                    rows.push(vec![Slot::Item(index)]);
                    used = widths[index];
                } else {
                    rows.last_mut().expect("至少一行").push(Slot::Item(index));
                    used += add;
                }
            }
        }
        start = end;
    }
    rows.retain(|row| !row.is_empty());
    rows
}

/// 一行内容宽度（不含栏内边距）。
pub(crate) fn row_width(items: &[(Group, Item)], row: &[Slot]) -> f32 {
    let mut width = 0_f32;
    let mut previous_separator = false;
    for (i, slot) in row.iter().enumerate() {
        match *slot {
            Slot::Separator => {
                width += GROUP_SEPARATOR;
                previous_separator = true;
            }
            Slot::Item(index) => {
                if i > 0 && !previous_separator {
                    width += ITEM_GAP;
                }
                width += items[index].1.width();
                previous_separator = false;
            }
        }
    }
    width
}

/// 二级栏行数对应的高度（含边框与内边距）。
pub(crate) fn bar_height(rows: usize) -> f32 {
    let rows = rows.max(1) as f32;
    rows * ROW_HEIGHT + (rows - 1.) * ROW_GAP + BAR_INSET
}

/// 选定的收起级别与它的排布结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PropertyPlan {
    pub level: u8,
    pub rows: usize,
    /// 最宽一行的内容宽度，不含内边距；没有控件时为 0。
    pub width: f32,
}

/// 从第 0 级逐级尝试，第一个两行内放得下的级别胜出；最后一级不限行数。
pub(crate) fn plan(ctx: &PropertyContext, budget: f32) -> PropertyPlan {
    let inner = (budget - BAR_INSET).max(32.);
    let mut result = PropertyPlan {
        level: 0,
        rows: 0,
        width: 0.,
    };
    for level in 0..=MAX_LEVEL {
        let list = items(ctx, level);
        if list.is_empty() {
            return result;
        }
        let rows = pack(&list, inner);
        let width = rows
            .iter()
            .map(|row| row_width(&list, row))
            .fold(0., f32::max)
            .min(inner);
        result = PropertyPlan {
            level,
            rows: rows.len(),
            width,
        };
        if rows.len() <= 2 {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(ctx: &PropertyContext, level: u8) -> Vec<Item> {
        items(ctx, level)
            .into_iter()
            .map(|(_, item)| item)
            .collect()
    }

    #[test]
    fn property_items_control_widths_follow_spec() {
        assert_eq!(Item::Swatches.width(), 234.);
        assert_eq!(segmented_width(2), 58.);
        assert_eq!(segmented_width(3), 86.);
        assert_eq!(Item::ArrowEnds.width(), 176.);
        // 数值框、下拉都带 1px 描边；数字区按最宽文本（9.5、100%）预留。
        assert_eq!(Item::Width.width(), 65.);
        assert_eq!(Item::Opacity.width(), 80.);
        assert_eq!(Item::TextOutline.width(), 42.);
        assert_eq!(Item::FontFamily.width(), 123.);
        assert_eq!(Item::ColorButton(false).width(), 45.);
        assert_eq!(bar_height(1), 42.);
        assert_eq!(bar_height(2), 78.);
    }

    #[test]
    fn property_items_text_estimates_never_undercut_ui_font() {
        // 微软雅黑 UI 12px 实测字宽；估算偏窄会让最右控件被窗口裁掉。
        for (text, measured) in [
            ("100%", 31.8),
            ("-180°", 31.2),
            ("9.5", 16.97),
            ("Mixed", 35.48),
            ("4 sides", 39.37),
            ("Done", 30.97),
            ("12 个对象", 53.63),
            ("所选 2 个对象均不支持圆角", 146.14),
        ] {
            assert!(text_width(text, 12.) >= measured, "{text}");
            assert!(text_width(text, 12.) <= measured + 2., "{text}");
        }
        assert!(Item::Opacity.value_text_width() >= 31.8);
        assert!(Item::Angle.value_text_width() >= 31.2);
    }

    #[test]
    fn property_items_levels_collapse_segments_colors_then_tool_group() {
        let arrow = PropertyContext::estimate(Tool::Arrow, 0);
        assert!(kinds(&arrow, 0).contains(&Item::ArrowEnds));
        assert!(kinds(&arrow, 0).contains(&Item::LineStyle));
        let compact = kinds(&arrow, 1);
        assert!(compact.contains(&Item::ArrowMenu) && compact.contains(&Item::LineStyleMenu));
        assert!(compact.contains(&Item::Swatches));
        let colors = kinds(&arrow, 2);
        assert!(colors.contains(&Item::ColorButton(false)) && colors.contains(&Item::Opacity));
        let style = kinds(&arrow, 3);
        assert!(style.contains(&Item::StyleMenu) && style.contains(&Item::ColorButton(true)));
        assert!(!style.contains(&Item::Opacity) && !style.contains(&Item::ArrowMenu));
        // 文字第 1 级只留粗体，其余字形与对齐收进“文字样式”。
        let text = kinds(&PropertyContext::estimate(Tool::Text, 0), 1);
        assert!(text.contains(&Item::BoldOnly) && text.contains(&Item::TextStyleMenu));
        assert!(!text.contains(&Item::Align) && !text.contains(&Item::Glyphs));
        // 说明文字与“完成选择”不收。
        let smart = kinds(&PropertyContext::estimate(Tool::SmartMosaic, 0), 3);
        assert!(smart.contains(&Item::SmartDone) && smart.contains(&Item::Hint(Hint::Smart)));
        assert!(smart.contains(&Item::StyleMenu));
        let eraser = kinds(&PropertyContext::estimate(Tool::PixelEraser, 0), 3);
        assert!(!eraser.contains(&Item::StyleMenu));
    }

    #[test]
    fn property_items_object_group_and_multi_selection() {
        let single = PropertyContext::estimate(Tool::Rectangle, 1);
        let list = items(&single, 0);
        let objects: Vec<_> = list
            .iter()
            .filter(|(g, _)| *g == Group::Object)
            .map(|(_, i)| *i)
            .collect();
        assert_eq!(objects, [Item::Angle, Item::Arrange, Item::Delete]);
        let arrow = items(&PropertyContext::estimate(Tool::Arrow, 1), 0);
        assert!(arrow.iter().any(|(_, i)| *i == Item::Straighten));
        assert!(!arrow.iter().any(|(_, i)| *i == Item::Angle));
        let multi = items(&PropertyContext::estimate(Tool::Rectangle, 3), 0);
        assert_eq!(multi[0], (Group::Info, Item::Count(3)));
        assert!(multi.iter().any(|(_, i)| *i == Item::Radius));
        let mut draft = PropertyContext::estimate(Tool::Text, 1);
        draft.text_draft = true;
        assert!(!items(&draft, 0).iter().any(|(g, _)| *g == Group::Object));
    }

    #[test]
    fn property_items_pack_whole_groups_and_pick_first_two_row_level() {
        let ctx = PropertyContext::estimate(Tool::Text, 1);
        let wide = plan(&ctx, 1400.);
        assert_eq!((wide.level, wide.rows), (0, 1));
        // 收窄后先换行，两行放不下才逐级收起。
        let mid = plan(&ctx, 760.);
        assert_eq!((mid.level, mid.rows), (0, 2));
        let narrow = plan(&ctx, 420.);
        assert!(narrow.level > 0 && narrow.rows <= 2);
        assert!(narrow.width <= 410.);
        // 整组换行：外观组不与专属组拆开。
        let list = items(&ctx, 0);
        let rows = pack(&list, 750.);
        for row in &rows {
            let groups: std::collections::HashSet<_> = row
                .iter()
                .filter_map(|s| match s {
                    Slot::Item(i) => Some(list[*i].0),
                    Slot::Separator => None,
                })
                .collect();
            for group in groups {
                let in_row = row
                    .iter()
                    .filter(|s| matches!(s, Slot::Item(i) if list[*i].0 == group))
                    .count();
                let total = list.iter().filter(|(g, _)| *g == group).count();
                assert_eq!(in_row, total);
            }
        }
        assert!(plan(&PropertyContext::estimate(Tool::Edit, 0), 800.).rows == 0);
    }
}
