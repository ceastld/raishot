//! 工具栏布局与临时面板状态；只使用逻辑坐标，不依赖 GPUI。
use crate::{annotation::Tool, geometry::Rect, toolbar_groups::Slot};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolbarPanel {
    #[default]
    None,
    Tools,
    Actions,
    Width,
    Font,
    Opacity,
    Radius,
    Arrow,
    Block,
    FontFamily,
    TextFormat,
    Finish,
    SelectionSize,
    Help,
    TextOutline,
    TextBackgroundAlpha,
    TextBackgroundRadius,
    /// 自定义颜色浮层；关闭后回到打开它的面板。
    CustomColor(ColorTarget),
    /// 选中对象（或水印）的角度。
    Angle,
    /// 组合、层级与删除等对象操作。
    Arrange,
    /// 色块收起（第 2 级起）后的颜色弹层：10 色、自定义，第 3 级附不透明度。
    Colors,
    /// 分段收起（第 1 级起）后的单选菜单。
    Choice(ChoiceKind),
    /// 工具专属组收起（第 3 级）后的“样式”弹层。
    ToolStyle,
    /// 文字描边、底色拆分开关右半 ▾ 的细项。
    TextOutlineStyle,
    TextBackgroundStyle,
}
/// 收成下拉的分段控件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceKind {
    LineStyle,
    Fill,
    Blend,
    Stroke,
}
/// 自定义颜色写回的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTarget {
    Annotation,
    TextOutline,
    TextBackground,
    FinishBorder,
    FinishShadow,
}
impl ToolbarPanel {
    pub fn more(self) -> bool {
        matches!(self, Self::Tools | Self::Actions)
    }
    pub(crate) fn parent(self) -> Self {
        match self {
            Self::TextOutline | Self::CustomColor(ColorTarget::TextOutline) => {
                Self::TextOutlineStyle
            }
            Self::TextBackgroundAlpha
            | Self::TextBackgroundRadius
            | Self::CustomColor(ColorTarget::TextBackground) => Self::TextBackgroundStyle,
            Self::CustomColor(ColorTarget::FinishBorder | ColorTarget::FinishShadow) => {
                Self::Finish
            }
            _ => Self::None,
        }
    }
    pub fn anchor(self) -> &'static str {
        match self {
            Self::Width => "width-menu",
            Self::Font => "font-menu",
            Self::Opacity => "opacity-menu",
            Self::Radius => "radius-menu",
            Self::Arrow => "arrow-menu",
            Self::Block => "block-menu",
            Self::FontFamily => "font-family-menu",
            Self::TextFormat => "text-format-menu",
            Self::Finish => "finish-menu",
            Self::SelectionSize => "selection-size-menu",
            Self::Help => "more",
            Self::Angle => "angle-menu",
            Self::Arrange => "arrange-menu",
            Self::Colors => "color-menu",
            Self::Choice(kind) => match kind {
                ChoiceKind::LineStyle => "line-style-menu",
                ChoiceKind::Fill => "fill-menu",
                ChoiceKind::Blend => "blend-menu",
                ChoiceKind::Stroke => "stroke-menu",
            },
            Self::ToolStyle => "tool-style-menu",
            Self::TextOutlineStyle => "text-outline-menu",
            Self::TextBackgroundStyle => "text-background-menu",
            Self::TextOutline => "text-outline-width",
            Self::TextBackgroundAlpha => "text-background-alpha",
            Self::TextBackgroundRadius => "text-background-radius",
            Self::CustomColor(target) => match target {
                ColorTarget::Annotation => "custom-color",
                ColorTarget::TextOutline => "outline-custom",
                ColorTarget::TextBackground => "background-custom",
                ColorTarget::FinishBorder => "finish-border-custom",
                ColorTarget::FinishShadow => "finish-shadow-custom",
            },
            _ => "more",
        }
    }
}

/// 截图与贴图使用同一个参数内容尺寸；外围阴影空间由各窗口布局处理。
pub(crate) fn parameter_popup_size(panel: ToolbarPanel, tool: Tool) -> Option<(f32, f32)> {
    match panel {
        ToolbarPanel::Radius if tool.supports_radius() => Some((300., 232.)),
        ToolbarPanel::Width | ToolbarPanel::Font | ToolbarPanel::Block | ToolbarPanel::Opacity => {
            Some((300., 232.))
        }
        ToolbarPanel::TextOutline if tool.text() => Some((300., 232.)),
        ToolbarPanel::TextBackgroundAlpha | ToolbarPanel::TextBackgroundRadius
            if tool == Tool::Text =>
        {
            Some((300., 232.))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl PanelRect {
    pub(crate) fn from_physical(rect: Rect, scale: f32) -> Self {
        Self {
            x: rect.x as f32 / scale,
            y: rect.y as f32 / scale,
            width: rect.width as f32 / scale,
            height: rect.height as f32 / scale,
        }
    }
    pub fn right(self) -> f32 {
        self.x + self.width
    }
    pub fn bottom(self) -> f32 {
        self.y + self.height
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ToolbarLayout {
    pub primary: PanelRect,
    pub properties: PanelRect,
    /// 一级栏靠近选区，属性和展开内容朝远离选区的一侧排列。
    pub properties_above: bool,
    monitor: PanelRect,
    /// 二级栏对应的工具；选中对象时按同一工具追加对象段重新排布。
    tool: Tool,
    /// 二级栏收起级别（0–3），绘制时按同一级别列控件。
    property_level: u8,
    /// 二级栏可用宽度：至少与主栏同宽，选区更宽时跟随选区，受屏幕限制。
    property_budget: f32,
}
impl ToolbarLayout {
    pub fn new(
        monitor: Rect,
        selected: Rect,
        scale: f32,
        position: Option<(f32, f32)>,
        tool: Tool,
    ) -> Self {
        let part = monitor.intersection(selected).unwrap_or(monitor);
        Self::place(
            PanelRect::from_physical(monitor, scale),
            PanelRect::from_physical(part, scale),
            position,
            tool,
        )
    }

    /// 图片编辑保留选区/编辑入口，其余动作参与真实换行计算。
    pub fn new_with_tools(
        monitor: Rect,
        selected: Rect,
        scale: f32,
        position: Option<(f32, f32)>,
        tool: Tool,
        slots: &[Slot],
    ) -> Self {
        Self::with_tools(monitor, selected, scale, position, tool, slots, true)
    }

    /// 普通截图的范围操作位于选区侧栏，主栏只计算标注和输出动作。
    pub fn new_capture_with_tools(
        monitor: Rect,
        selected: Rect,
        scale: f32,
        position: Option<(f32, f32)>,
        tool: Tool,
        slots: &[Slot],
    ) -> Self {
        Self::with_tools(monitor, selected, scale, position, tool, slots, false)
    }

    fn with_tools(
        monitor: Rect,
        selected: Rect,
        scale: f32,
        position: Option<(f32, f32)>,
        tool: Tool,
        slots: &[Slot],
        fixed_controls: bool,
    ) -> Self {
        let part = monitor.intersection(selected).unwrap_or(monitor);
        let items = primary_items(slots, fixed_controls);
        let width = primary_width(&items);
        Self::place_with_items(
            PanelRect::from_physical(monitor, scale),
            PanelRect::from_physical(part, scale),
            position,
            tool,
            width,
            &items,
        )
    }

    /// 长图剪辑的停靠属性栏：一级与二级栏同为属性栏内的卡片，弹层在卡片下方或侧旁展开。
    pub(crate) fn docked(monitor: PanelRect, card: PanelRect, tool: Tool, level: u8) -> Self {
        Self {
            primary: card,
            properties: card,
            properties_above: false,
            monitor,
            tool,
            property_level: level,
            property_budget: card.width,
        }
    }

    /// 未受显示器夹紧的逻辑宽度，供图片编辑视口计算默认居中位置。
    pub fn primary_width_with_tools(slots: &[Slot]) -> f32 {
        primary_width(&primary_items(slots, true))
    }

    fn place(
        monitor: PanelRect,
        part: PanelRect,
        position: Option<(f32, f32)>,
        tool: Tool,
    ) -> Self {
        // 旧 API 保留原工具集合，固定长截图入口增加一个按钮，并参与窄屏换行。
        Self::place_with_items(
            monitor,
            part,
            position,
            tool,
            602.,
            &[
                12., 32., 32., 32., 32., 32., 32., 32., 9., 32., 32., 32., 9., 32., 32., 32., 32.,
                32., 32.,
            ],
        )
    }

    fn place_with_items(
        monitor: PanelRect,
        part: PanelRect,
        position: Option<(f32, f32)>,
        tool: Tool,
        target_width: f32,
        items: &[f32],
    ) -> Self {
        let available = (monitor.width - 24.).max(64.);
        let width = target_width.min(available);
        let mut rows = 1_f32;
        let mut used = 0_f32;
        // 按实际项逐个换行，不能只用总宽度相除；窄屏会留下不足一个按钮的空隙。
        for &item in items {
            let next = if used == 0. { item } else { used + 2. + item };
            if next > width - 18. && used > 0. {
                rows += 1.;
                used = item;
            } else {
                used = next;
            }
        }
        let main_height = rows * 32. + (rows - 1.) * 2. + 16.;
        let budget = part.width.max(width).min(available);
        // 始终为最高的属性栏（含对象段）保留空间；选工具、悬停、打开面板不会挪动主栏。
        let reserve_height =
            reserved_property_height(budget).min((monitor.height - main_height - 32.).max(28.));
        let group_height = main_height + 8. + reserve_height;
        let below_fits = part.bottom() + 10. + group_height <= monitor.bottom() - 12.;
        let preferred_y = if below_fits {
            part.bottom() + 10.
        } else {
            part.y - main_height - 10.
        };
        let (px, py) = position.unwrap_or((part.right() - width, preferred_y));
        let properties_above = position.map_or(!below_fits, |(_, y)| {
            y + main_height / 2. < part.y + part.height / 2.
        });
        let x = px.clamp(
            monitor.x + 12.,
            (monitor.right() - width - 12.).max(monitor.x + 12.),
        );
        let min_y = monitor.y
            + 12.
            + if properties_above {
                reserve_height + 8.
            } else {
                0.
            };
        let max_y = monitor.bottom()
            - main_height
            - 12.
            - if properties_above {
                0.
            } else {
                reserve_height + 8.
            };
        let y = py.clamp(min_y, max_y.max(min_y));
        // 先换行、后收起：控件按真实尺寸估算，两行放不下才逐级收起。
        let (prop_width, prop_height, property_level) = property_size(tool, 0, budget);
        let prop_width = prop_width.min(available);
        let prop_height = prop_height.min((monitor.height - main_height - 32.).max(28.));
        let prop_x = (x + width - prop_width).clamp(
            monitor.x + 12.,
            (monitor.right() - prop_width - 12.).max(monitor.x + 12.),
        );
        Self {
            primary: PanelRect {
                x,
                y,
                width,
                height: main_height,
            },
            properties: PanelRect {
                x: prop_x,
                y: if properties_above {
                    y - prop_height - 8.
                } else {
                    y + main_height + 8.
                },
                width: prop_width,
                height: prop_height,
            },
            properties_above,
            monitor,
            tool,
            property_level,
            property_budget: budget,
        }
    }
    /// 选中对象时在二级栏末尾追加对象段（编辑、角度、排列、删除），与样式同排按组换行。
    pub fn with_transform_context(mut self, count: usize) -> Self {
        if count == 0 {
            return self;
        }
        // 多选只有圆角适用提示/入口与对象段，不随主选工具切换整排样式尺寸。
        let (width, height, level) = property_size(self.tool, count, self.property_budget);
        self.property_level = level;
        self.properties.width = width.min((self.monitor.width - 24.).max(64.));
        self.properties.x = (self.primary.right() - self.properties.width).clamp(
            self.monitor.x + 12.,
            (self.monitor.right() - self.properties.width - 12.).max(self.monitor.x + 12.),
        );
        self.properties.height =
            height.min((self.monitor.height - self.primary.height - 32.).max(28.));
        if self.properties_above {
            self.properties.y = self.primary.y - self.properties.height - 8.;
        }
        self
    }
    /// 二级栏收起级别；绘制按同一级别列控件，保证与估算尺寸一致。
    pub fn property_level(&self) -> u8 {
        self.property_level
    }
    pub fn popup(
        &self,
        width: f32,
        height: f32,
        anchor: Option<(f32, f32)>,
        properties: bool,
    ) -> PanelRect {
        let width = width.min((self.monitor.width - 24.).max(64.));
        let height = height.min((self.monitor.height - 24.).max(48.));
        let x = anchor
            .map_or(self.primary.right() - width, |p| p.0 - 16.)
            .clamp(
                self.monitor.x + 12.,
                (self.monitor.right() - width - 12.).max(self.monitor.x + 12.),
            );
        let before = if properties {
            self.properties.y.min(self.primary.y)
        } else {
            self.primary.y
        } - height
            - 8.;
        let after = if properties {
            self.properties.bottom().max(self.primary.bottom())
        } else {
            self.primary.bottom()
        } + 8.;
        let before_fits = before >= self.monitor.y + 12.;
        let after_fits = after + height <= self.monitor.bottom() - 12.;
        let y = match (self.properties_above, before_fits, after_fits) {
            (true, true, _) | (false, true, false) => before,
            (false, _, true) | (true, false, true) => after,
            _ => {
                // 上下都容不下时移到整组旁边，避免夹在两级栏之间。
                let left = self.primary.x.min(self.properties.x) - width - 8.;
                let right = self.primary.right().max(self.properties.right()) + 8.;
                let lateral = if left >= self.monitor.x + 12. {
                    Some(left)
                } else if right + width <= self.monitor.right() - 12. {
                    Some(right)
                } else {
                    None
                };
                if let Some(side_x) = lateral {
                    return PanelRect {
                        x: side_x,
                        y: self.properties.y.clamp(
                            self.monitor.y + 12.,
                            (self.monitor.bottom() - height - 12.).max(self.monitor.y + 12.),
                        ),
                        width,
                        height,
                    };
                }
                if self.properties_above {
                    before
                } else {
                    after
                }
            }
        }
        .clamp(
            self.monitor.y + 12.,
            (self.monitor.bottom() - height - 12.).max(self.monitor.y + 12.),
        );
        PanelRect {
            x,
            y,
            width,
            height,
        }
    }

    /// 主栏按钮的下拉：左缘对齐按钮，按候选顺序取第一个放得下的位置，尽量不遮挡两级栏。
    /// 有二级栏时：贴主栏朝选区一侧 → 二级栏外侧 → 覆盖二级栏；没有时先朝远离选区一侧。
    /// `anchor` 为按钮中心，`button` 为按钮逻辑宽高，`properties` 为二级栏是否显示。
    pub fn flyout(
        &self,
        anchor: (f32, f32),
        button: (f32, f32),
        width: f32,
        height: f32,
        properties: bool,
    ) -> PanelRect {
        let width = width.min((self.monitor.width - 24.).max(64.));
        let height = height.min((self.monitor.height - 24.).max(48.));
        let x = (anchor.0 - button.0 / 2.).clamp(
            self.monitor.x + 12.,
            (self.monitor.right() - width - 12.).max(self.monitor.x + 12.),
        );
        let top = self.monitor.y + 12.;
        let bottom = self.monitor.bottom() - 12.;
        let above = self.primary.y - 4. - height;
        let below = self.primary.bottom() + 4.;
        // inner 朝选区，outer 朝二级栏；beyond 越过二级栏放在整组外侧。
        let (inner, outer, beyond) = if self.properties_above {
            (below, above, self.properties.y - 8. - height)
        } else {
            (above, below, self.properties.bottom() + 8.)
        };
        let candidates = if properties {
            [inner, beyond, outer]
        } else {
            [outer, inner, outer]
        };
        let y = candidates
            .into_iter()
            .find(|&y| y >= top && y + height <= bottom)
            .unwrap_or(outer)
            .clamp(top, (bottom - height).max(top));
        PanelRect {
            x,
            y,
            width,
            height,
        }
    }
}

/// 二级栏（宽，高，收起级别）；没有控件时宽为 0、高为一行。
/// 图片编辑的选区外观栏由 selection_finish_view 自行排布，沿用固定尺寸。
pub(crate) fn property_size(tool: Tool, count: usize, budget: f32) -> (f32, f32, u8) {
    use crate::property_items::{bar_height, plan, PropertyContext, BAR_INSET};
    if tool == Tool::Crop && count == 0 {
        return (340., 48., 0);
    }
    let plan = plan(&PropertyContext::estimate(tool, count), budget);
    let width = if plan.rows > 0 {
        plan.width + BAR_INSET
    } else {
        0.
    };
    (width, bar_height(plan.rows), plan.level)
}

/// 所有工具（含单选对象段与多选）里最高的二级栏，主栏据此预留位置。
fn reserved_property_height(budget: f32) -> f32 {
    use crate::toolbar_config::AVAILABLE_TOOLS;
    AVAILABLE_TOOLS
        .iter()
        .chain(&[Tool::SmartMosaic, Tool::Lasso, Tool::Crop])
        .flat_map(|&tool| [1, 2].map(|count| property_size(tool, count, budget).1))
        .fold(property_size(Tool::Crop, 0, budget).1, f32::max)
}

fn primary_items(slots: &[Slot], fixed_controls: bool) -> Vec<f32> {
    use crate::toolbar_groups::{GRIP_WIDTH, GROUP_WIDTH, TOOL_WIDTH};
    let mut items = Vec::with_capacity(slots.len() + 15);
    items.push(GRIP_WIDTH); // 拖拽把手
                            // 截图用一个鼠标模式切换项；图片编辑保留独立 Crop/Edit。
    items.extend(std::iter::repeat_n(
        TOOL_WIDTH,
        if fixed_controls { 2 } else { 1 },
    ));
    // 分组按钮带下拉箭头，比单个工具宽。
    items.extend(slots.iter().map(|slot| slot.width()));
    // 撤销/重做 | 长截图/录屏 | 识别（带下拉） | 贴图/保存/取消/完成；
    // 不受绘制工具配置影响。
    items.extend([9., TOOL_WIDTH, TOOL_WIDTH]);
    if fixed_controls {
        // 图片编辑没有上方选区条，保留主栏“更多”。
        items.push(TOOL_WIDTH);
    }
    items.extend([
        9.,
        TOOL_WIDTH,
        TOOL_WIDTH,
        9.,
        GROUP_WIDTH,
        9.,
        TOOL_WIDTH,
        TOOL_WIDTH,
        TOOL_WIDTH,
        TOOL_WIDTH,
    ]);
    items
}

/// 选区跨屏时工具栏锚定结束点所在显示器；但那块屏只沾到选区一条窄边（重叠不到选区宽或高的
/// 15%）时，改用与选区重叠面积最大的显示器，避免工具栏和结果面板跑到几乎没有选区的屏上。
pub fn anchor_monitor(crop: Rect, end: usize, screens: &[Rect]) -> usize {
    let overlap = |index: usize| {
        screens
            .get(index)
            .and_then(|screen| screen.intersection(crop))
    };
    let sliver = overlap(end).is_none_or(|part| {
        u64::from(part.width) * 100 < u64::from(crop.width) * 15
            || u64::from(part.height) * 100 < u64::from(crop.height) * 15
    });
    if !sliver {
        return end;
    }
    (0..screens.len())
        .filter_map(|index| {
            overlap(index).map(|part| (u64::from(part.width) * u64::from(part.height), index))
        })
        .max_by_key(|(area, index)| (*area, std::cmp::Reverse(*index)))
        .map_or(end, |(_, index)| index)
}

fn primary_width(items: &[f32]) -> f32 {
    // 与 primary-toolbar 的 border + px_2 + gap(2) 一致。
    items.iter().sum::<f32>() + items.len().saturating_sub(1) as f32 * 2. + 18.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property_items::bar_height;
    /// 对象段（角度、排列、删除）的宽度；多选计数在信息组，不算在内。
    fn object_width() -> f32 {
        use crate::property_items::{Item, ITEM_GAP};
        Item::Angle.width() + Item::Arrange.width() + Item::Delete.width() + 2. * ITEM_GAP
    }
    /// 既有宽度算式按逐个工具计算；分组另有专门用例。
    fn separate(tools: &[Tool]) -> Vec<Slot> {
        crate::toolbar_groups::slots(tools, false)
    }
    fn assert_stacked(layout: ToolbarLayout) {
        if layout.properties_above {
            assert!((layout.properties.bottom() + 8. - layout.primary.y).abs() < 0.01);
        } else {
            assert!((layout.properties.y - layout.primary.bottom() - 8.).abs() < 0.01);
        }
    }

    #[test]
    fn toolbar_above_primary_nearest_selection_and_properties_grow_outward() {
        let monitor = Rect::full(1920, 1440);
        let selected = Rect {
            x: 320,
            y: 1020,
            width: 1200,
            height: 320,
        };
        let base = ToolbarLayout::new(monitor, selected, 1., None, Tool::Crop);
        assert!(base.properties_above);
        assert_eq!(base.primary.bottom(), 1010.);
        for tool in [Tool::Rectangle, Tool::Text, Tool::Polyline, Tool::Spotlight] {
            let layout = ToolbarLayout::new(monitor, selected, 1., None, tool);
            assert_eq!(layout.primary, base.primary);
            assert_stacked(layout);
            let expanded = layout.with_transform_context(1);
            assert_eq!(expanded.primary, base.primary);
            assert_stacked(expanded);
            // 对象段与样式同排换行，可能不增加行数，但不会向选区一侧伸。
            assert!(expanded.properties.y <= layout.properties.y);
            assert_eq!(expanded.properties.bottom(), layout.properties.bottom());
            let small = expanded.popup(300., 232., None, true);
            let large = expanded.popup(300., 440., None, true);
            assert_eq!(small.bottom(), expanded.properties.y - 8.);
            assert_eq!(large.bottom(), small.bottom());
            assert!(large.y < small.y);
        }
    }

    #[test]
    fn toolbar_above_fullscreen_dpi_and_negative_monitor_keep_both_bars_inside() {
        for scale in [1., 1.25, 2., 3.] {
            let monitor = PanelRect {
                x: -1920. / scale,
                y: -120. / scale,
                width: 1080. / scale,
                height: 1920. / scale,
            };
            for position in [None, Some((-9999., -9999.)), Some((9999., 9999.))] {
                let base = ToolbarLayout::place(monitor, monitor, position, Tool::Polyline);
                for count in [0, 1, 3] {
                    let layout = base.with_transform_context(count);
                    assert_eq!(layout.primary, base.primary);
                    assert_stacked(layout);
                    for r in [
                        layout.primary,
                        layout.properties,
                        layout.popup(360., 336., None, true),
                    ] {
                        assert!(r.x >= monitor.x && r.right() <= monitor.right());
                        assert!(r.y >= monitor.y && r.bottom() <= monitor.bottom());
                    }
                }
            }
        }
    }

    #[test]
    fn toolbar_above_below_selection_keeps_original_order_and_inline_pin_popup_origin() {
        let monitor = Rect::full(1920, 1080);
        let selected = Rect {
            x: 300,
            y: 30,
            width: 1200,
            height: 180,
        };
        let layout = ToolbarLayout::new(monitor, selected, 1., None, Tool::Rectangle);
        assert!(!layout.properties_above);
        assert_eq!(layout.primary.y, 220.);
        assert_stacked(layout);
        let menu = layout.popup(300., 232., None, true);
        assert_eq!(menu.y, layout.properties.bottom() + 8.);
        // 贴图参数小窗只复用面板内容尺寸；局部内边距不能受屏幕堆叠方向影响。
        let mut local = ToolbarLayout::new(
            Rect::full(324, 256),
            Rect::full(324, 1),
            1.,
            Some((12., 12.)),
            Tool::Rectangle,
        );
        local.primary = PanelRect {
            x: 12.,
            y: -256.,
            width: 300.,
            height: 0.,
        };
        local.properties = local.primary;
        let inline = local.popup(300., 232., Some((28., 0.)), true);
        assert_eq!((inline.x, inline.y), (12., 12.));
    }

    #[test]
    fn radius_bulk_ui_multi_scope_and_transform_fit_without_moving_primary_on_narrow_dpi() {
        let monitor = Rect {
            x: 1280,
            y: 120,
            width: 1280,
            height: 1440,
        };
        for scale in [1., 2., 3.] {
            let mut multi_size = None;
            for tool in [Tool::Rectangle, Tool::Arrow, Tool::Mosaic, Tool::Edit] {
                let base = ToolbarLayout::new(monitor, monitor, scale, None, tool);
                let multi = base.with_transform_context(3);
                assert_eq!(multi.primary, base.primary);
                assert!(multi.properties.height >= bar_height(1));
                assert!(multi.properties.x >= monitor.x as f32 / scale);
                assert!(multi.properties.right() <= (monitor.x + monitor.width) as f32 / scale);
                assert!(multi.properties.bottom() <= (monitor.y + monitor.height) as f32 / scale);
                let size = (multi.properties.width, multi.properties.height);
                if let Some(previous) = multi_size {
                    assert_eq!(size, previous);
                }
                multi_size = Some(size);
            }
        }
    }
    #[test]
    fn highlight_radius_popup_and_property_space_keep_primary_fixed() {
        let monitor = Rect::full(1920, 1080);
        let base = ToolbarLayout::new(monitor, monitor, 1., None, Tool::Crop);
        for tool in [Tool::Rectangle, Tool::Spotlight, Tool::HighlighterRect] {
            assert_eq!(
                parameter_popup_size(ToolbarPanel::Radius, tool),
                Some((300., 232.))
            );
            assert_eq!(
                ToolbarLayout::new(monitor, monitor, 1., None, tool).primary,
                base.primary
            );
        }
        assert_eq!(
            parameter_popup_size(ToolbarPanel::Radius, Tool::Ellipse),
            None
        );
        let highlight = ToolbarLayout::new(monitor, monitor, 1., None, Tool::HighlighterRect);
        assert_eq!(
            (highlight.properties.width, highlight.properties.height),
            (479., 42.)
        );
        for scale in [1., 1.25, 2., 3.] {
            let work = Rect {
                x: 0,
                y: 120,
                width: 1080,
                height: 1920,
            };
            let crop = ToolbarLayout::new(work, work, scale, None, Tool::Crop);
            let highlight = ToolbarLayout::new(work, work, scale, None, Tool::HighlighterRect);
            assert_eq!(highlight.primary, crop.primary);
            let r = highlight.properties;
            assert!(r.x >= work.x as f32 / scale);
            assert!(r.right() <= (work.x + work.width) as f32 / scale);
            assert_stacked(highlight);
            assert!(r.bottom() <= (work.y + work.height) as f32 / scale);
        }
    }

    #[test]
    fn highlighter_modes_layout_reserves_controls_without_moving_primary() {
        for scale in [1., 1.25, 2., 3.] {
            let work = Rect::full(1920, 1080);
            let base = ToolbarLayout::new(work, work, scale, None, Tool::Crop);
            for tool in [Tool::Highlighter, Tool::HighlighterRect] {
                let layout = ToolbarLayout::new(work, work, scale, None, tool);
                assert_eq!(base.primary, layout.primary);
                assert!(layout.properties.width <= 1920. / scale - 24.);
                assert_stacked(layout);
                let selected = layout.with_transform_context(1);
                assert_eq!(base.primary, selected.primary);
                assert_stacked(selected);
            }
        }
    }

    #[test]
    fn anchor_monitor_ignores_end_screen_that_only_touches_a_sliver() {
        let screens = [
            Rect::full(1920, 1080),
            Rect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        // 选区几乎都在左屏，只越过分界 8px：工具栏留在左屏。
        let crop = Rect {
            x: 1000,
            y: 100,
            width: 928,
            height: 400,
        };
        assert_eq!(anchor_monitor(crop, 1, &screens), 0);
        // 真正跨到右屏一半：仍锚定结束显示器。
        let crop = Rect {
            x: 1400,
            y: 100,
            width: 1000,
            height: 400,
        };
        assert_eq!(anchor_monitor(crop, 1, &screens), 1);
        assert_eq!(anchor_monitor(crop, 0, &screens), 0);
        // 结束点不在任何屏上的选区内：取重叠最多的屏。
        assert_eq!(
            anchor_monitor(
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100
                },
                1,
                &screens
            ),
            0
        );
    }

    #[test]
    fn configurable_tools_keep_fixed_actions_and_sanitize_counts() {
        let monitor = Rect::full(1920, 1080);
        let empty = ToolbarLayout::new_with_tools(monitor, monitor, 1., None, Tool::Crop, &[]);
        // 识别与图片翻译合为一个带下拉的识别按钮，固定区比两个独立按钮窄 22。
        assert_eq!(empty.primary.width, 496.);
        assert_eq!(empty.primary.height, 48.);
        let invalid = ToolbarLayout::new_with_tools(
            monitor,
            monitor,
            1.,
            None,
            Tool::Crop,
            &separate(&[Tool::Crop, Tool::Pen, Tool::Edit, Tool::Pen]),
        );
        assert_eq!(invalid.primary.width, 530.);
        let defaults = ToolbarLayout::new_with_tools(
            monitor,
            monitor,
            1.,
            None,
            Tool::Crop,
            &separate(&crate::toolbar_config::DEFAULT_TOOLS),
        );
        assert_eq!(defaults.primary.width, 734.);
        assert_eq!(defaults.primary.height, 48.);
        assert_eq!(
            defaults.primary.width,
            ToolbarLayout::primary_width_with_tools(&separate(
                &crate::toolbar_config::DEFAULT_TOOLS
            )),
        );
        // 默认合并：形状/线条/标记/文字/打码/橡皮擦各一组；六个下拉箭头各加 12，少一个按钮。
        let grouped = ToolbarLayout::new_with_tools(
            monitor,
            monitor,
            1.,
            None,
            Tool::Crop,
            &crate::toolbar_config::ToolbarConfig::default().slots(),
        );
        assert_eq!(grouped.primary.width, 734. - 34. + 72.);
        assert_eq!(
            ToolbarLayout::new(monitor, monitor, 1., None, Tool::Crop)
                .primary
                .width,
            602.
        );
    }

    #[test]
    fn capture_selection_bar_actions_reduce_main_width_without_changing_image_controls() {
        let monitor = Rect::full(1920, 1080);
        let tools = &separate(&crate::toolbar_config::DEFAULT_TOOLS);
        let image = ToolbarLayout::new_with_tools(monitor, monitor, 1., None, Tool::Crop, tools);
        let capture =
            ToolbarLayout::new_capture_with_tools(monitor, monitor, 1., None, Tool::Crop, tools);
        assert_eq!(image.primary.width - capture.primary.width, 68.);
        assert_eq!(image.primary.width, 734.);
        assert_eq!(capture.primary.width, 666.);
        let narrow = Rect::full(701, 1080);
        let image = ToolbarLayout::new_with_tools(narrow, narrow, 1., None, Tool::Edit, tools);
        let capture =
            ToolbarLayout::new_capture_with_tools(narrow, narrow, 1., None, Tool::Edit, tools);
        assert!(image.primary.width <= 701. - 24.);
        assert!(capture.primary.width <= 701. - 24.);
        assert!(image.primary.height > capture.primary.height);
        assert_stacked(capture);
    }

    #[test]
    fn configurable_many_tools_wrap_without_overlap_on_high_dpi_monitors() {
        for scale in [1., 1.25, 2., 3.] {
            let monitor = Rect {
                x: 2560,
                y: 120,
                width: 1080,
                height: 1920,
            };
            let crop = ToolbarLayout::new_with_tools(
                monitor,
                monitor,
                scale,
                None,
                Tool::Crop,
                &separate(&crate::toolbar_config::AVAILABLE_TOOLS),
            );
            let text = ToolbarLayout::new_with_tools(
                monitor,
                monitor,
                scale,
                None,
                Tool::Text,
                &separate(&crate::toolbar_config::AVAILABLE_TOOLS),
            );
            assert_eq!(crop.primary, text.primary);
            assert_stacked(crop);
            for rect in [crop.primary, crop.properties, text.properties] {
                assert!(rect.x >= monitor.x as f32 / scale);
                assert!(rect.right() <= (monitor.x + monitor.width) as f32 / scale);
                assert!(rect.y >= monitor.y as f32 / scale);
                assert!(rect.bottom() <= (monitor.y + monitor.height) as f32 / scale);
            }
            if scale >= 1.25 {
                assert!(crop.primary.height > 48.);
            }
        }
    }
    #[test]
    fn primary_stays_fixed_across_tools_and_popup_and_drag_stays_on_monitor() {
        let monitor = PanelRect {
            x: -1920.,
            y: -240.,
            width: 1920.,
            height: 1080.,
        };
        let part = PanelRect {
            x: -1800.,
            y: 560.,
            width: 900.,
            height: 250.,
        };
        let crop = ToolbarLayout::place(monitor, part, None, Tool::Crop);
        let shape = ToolbarLayout::place(monitor, part, None, Tool::Rectangle);
        assert_eq!(crop.primary, shape.primary);
        for layout in [
            shape,
            ToolbarLayout::place(monitor, part, Some((9999., -9999.)), Tool::Rectangle),
        ] {
            let menu = layout.popup(360., 336., None, true);
            for r in [layout.primary, layout.properties, menu] {
                assert!(r.x >= monitor.x && r.right() <= monitor.right());
                assert!(r.y >= monitor.y && r.bottom() <= monitor.bottom());
            }
            assert_stacked(layout);
        }
    }
    #[test]
    fn narrow_width_accounts_for_unfillable_gaps_between_buttons() {
        let layout = ToolbarLayout::place(
            PanelRect {
                x: 0.,
                y: 0.,
                width: 320.,
                height: 800.,
            },
            PanelRect {
                x: 20.,
                y: 20.,
                width: 280.,
                height: 600.,
            },
            None,
            Tool::Rectangle,
        );
        assert_eq!(layout.primary.height, 116.);
        assert!(layout.properties.bottom() <= 788.);
    }
    #[test]
    fn high_dpi_and_narrow_monitor_keep_bars_and_popup_inside_anchor() {
        for scale in [1., 1.25, 2., 3.] {
            let monitor = Rect {
                x: 2560,
                y: 120,
                width: 1080,
                height: 1920,
            };
            let layout = ToolbarLayout::new(
                monitor,
                Rect {
                    x: 2200,
                    y: 150,
                    width: 1400,
                    height: 1500,
                },
                scale,
                None,
                Tool::Rectangle,
            );
            let menu = layout.popup(360., 336., Some((9999., 9999.)), true);
            for r in [layout.primary, layout.properties, menu] {
                assert!(r.x >= monitor.x as f32 / scale);
                assert!(r.right() <= (monitor.x + monitor.width) as f32 / scale);
                assert!(r.y >= monitor.y as f32 / scale);
                assert!(r.bottom() <= (monitor.y + monitor.height) as f32 / scale);
            }
        }
    }
    #[test]
    fn transform_context_stays_on_high_dpi_narrow_monitor_without_moving_primary() {
        for scale in [1., 1.25, 2., 3.] {
            let monitor = Rect {
                x: 2560,
                y: 120,
                width: 1080,
                height: 1920,
            };
            for tool in [Tool::Mosaic, Tool::Text, Tool::Polyline] {
                let base = ToolbarLayout::new(monitor, monitor, scale, None, tool);
                for count in [1, 2, 5] {
                    let context = base.with_transform_context(count);
                    assert_eq!(context.primary, base.primary);
                    let r = context.properties;
                    assert!(r.x >= monitor.x as f32 / scale);
                    assert!(r.right() <= (monitor.x + monitor.width) as f32 / scale);
                    assert!(r.bottom() <= (monitor.y + monitor.height) as f32 / scale);
                    let menu = context.popup(360., 336., None, true);
                    assert!(menu.bottom() <= (monitor.y + monitor.height) as f32 / scale);
                }
            }
        }
    }

    #[test]
    fn toolbar_flyout_avoids_both_bars_before_covering_properties() {
        let monitor = Rect::full(1920, 1080);
        let overlaps = |a: PanelRect, b: PanelRect| {
            a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
        };
        // 二级栏在下：朝选区（上方）贴主栏展开，不压两级栏。
        let below = ToolbarLayout::new(
            monitor,
            Rect {
                x: 300,
                y: 300,
                width: 1200,
                height: 180,
            },
            1.,
            None,
            Tool::Rectangle,
        );
        assert!(!below.properties_above);
        let anchor = (below.primary.x + 60., below.primary.y + 24.);
        let menu = below.flyout(anchor, (44., 32.), 180., 200., true);
        assert_eq!(menu.x, anchor.0 - 22.);
        assert_eq!(menu.bottom(), below.primary.y - 4.);
        assert!(!overlaps(menu, below.properties));
        // 没有二级栏时仍朝远离选区一侧。
        let bare = below.flyout(anchor, (44., 32.), 180., 200., false);
        assert_eq!(bare.y, below.primary.bottom() + 4.);
        // 选区贴近屏幕顶端、上方放不下：放到二级栏外侧。
        let top = ToolbarLayout::new(
            monitor,
            Rect {
                x: 300,
                y: 10,
                width: 1200,
                height: 150,
            },
            1.,
            None,
            Tool::Rectangle,
        );
        assert!(!top.properties_above);
        let anchor = (top.primary.x + 60., top.primary.y + 24.);
        let menu = top.flyout(anchor, (44., 32.), 180., 200., true);
        assert_eq!(menu.y, top.properties.bottom() + 8.);
        assert!(!overlaps(menu, top.primary) && !overlaps(menu, top.properties));
        // 二级栏在上：朝选区（下方）展开。
        let above = ToolbarLayout::new(
            monitor,
            Rect {
                x: 300,
                y: 700,
                width: 1200,
                height: 300,
            },
            1.,
            None,
            Tool::Rectangle,
        );
        assert!(above.properties_above);
        let anchor = (above.primary.x + 60., above.primary.y + 24.);
        let menu = above.flyout(anchor, (44., 32.), 180., 200., true);
        assert_eq!(menu.y, above.primary.bottom() + 4.);
        assert!(!overlaps(menu, above.properties));
        // 屏幕边缘：左右上下都夹紧在显示器内。
        for scale in [1., 1.25, 2., 3.] {
            let layout = ToolbarLayout::new(monitor, monitor, scale, Some((-50., 0.)), Tool::Crop);
            let anchor = (layout.primary.x, layout.primary.y + 24.);
            for properties in [true, false] {
                let menu = layout.flyout(anchor, (44., 32.), 180., 260., properties);
                assert!(menu.x >= 12. && menu.y >= 12.);
                assert!(menu.right() <= 1920. / scale - 12.);
                assert!(menu.bottom() <= 1080. / scale - 12.);
            }
        }
    }

    #[test]
    fn nested_navigation_transform_rows_fit_narrow_negative_monitors_without_moving_primary() {
        for scale in [1., 1.25, 2., 3.] {
            let monitor = PanelRect {
                x: -1920. / scale,
                y: -120. / scale,
                width: 1080. / scale,
                height: 1920. / scale,
            };
            for position in [None, Some((-9999., -9999.)), Some((9999., 9999.))] {
                let base = ToolbarLayout::place(monitor, monitor, position, Tool::Crop);
                for tool in [Tool::Rectangle, Tool::Text, Tool::Polyline] {
                    let original = ToolbarLayout::place(monitor, monitor, position, tool);
                    for count in [1, 4] {
                        let layout = original.with_transform_context(count);
                        assert_eq!(layout.primary, base.primary);
                        assert_stacked(layout);
                        let available = (monitor.width - 24.).max(64.);
                        // 对象段整组放得下，不被挤成半段。
                        assert!(
                            layout.properties.width
                                >= (object_width() + crate::property_items::BAR_INSET)
                                    .min(available)
                        );
                        assert!(layout.properties.x >= monitor.x);
                        assert!(layout.properties.right() <= monitor.right());
                        assert!(layout.properties.y >= monitor.y);
                        assert!(layout.properties.bottom() <= monitor.bottom());
                        assert!(
                            layout.properties.height
                                >= bar_height(1)
                                    .min((monitor.height - layout.primary.height - 32.).max(28.))
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn property_groups_wrap_whole_groups_and_follow_selection_width() {
        // 整组放得下就同行；放不下整组换行，不在组内拆开（规则细节见 property_items 测试）。
        assert_eq!(property_size(Tool::Rectangle, 1, 2000.).1, bar_height(1));
        assert_eq!(property_size(Tool::Rectangle, 1, 600.).1, bar_height(2));
        let monitor = PanelRect {
            x: 0.,
            y: 0.,
            width: 1920.,
            height: 1080.,
        };
        let small = PanelRect {
            x: 700.,
            y: 200.,
            width: 240.,
            height: 160.,
        };
        let wide = PanelRect {
            x: 100.,
            y: 200.,
            width: 1600.,
            height: 400.,
        };
        let narrow =
            ToolbarLayout::place(monitor, small, None, Tool::Rectangle).with_transform_context(1);
        let roomy =
            ToolbarLayout::place(monitor, wide, None, Tool::Rectangle).with_transform_context(1);
        // 小选区：宽度不超过主栏，样式与对象段换成多行。
        assert!(narrow.properties.width <= narrow.primary.width + 0.5);
        assert!(narrow.properties.height >= bar_height(2));
        // 宽选区：一行放下全部，高度只有一行。
        assert_eq!(roomy.properties.height, bar_height(1));
        assert!(roomy.properties.width > narrow.properties.width);
    }
}
