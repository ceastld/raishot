//! 主栏管理标注/输出，属性栏显示工具参数，截图选区外观位于侧栏。
use crate::{
    annotation::Tool,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    toolbar_groups::{self, Flyout, RecognizeMode, Slot, ToolGroup, GRIP_WIDTH, GROUP_WIDTH},
    toolbar_icons::{self, Icon},
    toolbar_layout::{PanelRect, ToolbarLayout, ToolbarPanel},
};
use gpui::{div, prelude::*, px, rgb, rgba, Context, MouseButton, SharedString, Window};

#[path = "tool_shortcut_hints.rs"]
pub(crate) mod shortcut_hints;
#[path = "ui_kit.rs"]
pub(crate) mod ui_kit;

/// (id, 中文说明, English hint, 工具)
pub(crate) const TOOLS: &[(&str, &str, &str, Tool)] = &[
    ("crop", "选区", "Selection", Tool::Crop),
    (
        "edit",
        "鼠标 / 对象编辑 · Ctrl 点击多选 · 再次点击调整范围",
        "Pointer / edit objects · Ctrl+click to multi-select · click again to adjust the area",
        Tool::Edit,
    ),
    (
        "rectangle",
        "矩形 · Shift 正方形",
        "Rectangle · Shift for square",
        Tool::Rectangle,
    ),
    (
        "arrow",
        "箭头 · Shift 吸附方向",
        "Arrow · Shift snaps direction",
        Tool::Arrow,
    ),
    (
        "pen",
        "画笔 · Shift 直线 · V 编辑笔迹",
        "Pen · Shift for straight line · V edits strokes",
        Tool::Pen,
    ),
    ("text", "文字 · F2 编辑", "Text · F2 to edit", Tool::Text),
    ("mosaic", "马赛克", "Mosaic", Tool::Mosaic),
];
const EXTRA: &[(&str, &str, &str, Tool)] = &[
    (
        "ellipse",
        "椭圆 · Shift 正圆",
        "Ellipse · Shift for circle",
        Tool::Ellipse,
    ),
    (
        "triangle",
        "三角形 · Shift 等宽高",
        "Triangle · Shift for equal width and height",
        Tool::Triangle,
    ),
    (
        "diamond",
        "菱形 · Shift 正菱形",
        "Diamond · Shift for a square diamond",
        Tool::Diamond,
    ),
    (
        "star",
        "五角星 · Shift 等宽高",
        "Star · Shift for equal width and height",
        Tool::Star,
    ),
    ("highlighter", "荧光笔", "Highlighter", Tool::Highlighter),
    (
        "highlighter_rect",
        "高亮框",
        "Highlight box",
        Tool::HighlighterRect,
    ),
    ("polyline", "折线", "Polyline", Tool::Polyline),
    ("watermark", "水印", "Watermark", Tool::Watermark),
    ("serial", "序号", "Number", Tool::Serial),
    ("blur", "模糊", "Blur", Tool::Blur),
    ("mosaic_brush", "打码笔", "Mosaic brush", Tool::MosaicBrush),
    ("blur_brush", "模糊笔", "Blur brush", Tool::BlurBrush),
    (
        "local_fill",
        "局部颜色填充 · 周围背景取色",
        "Local fill · samples surrounding background",
        Tool::LocalFill,
    ),
    ("spotlight", "聚光灯", "Spotlight", Tool::Spotlight),
    (
        "pixel-eraser",
        "局部橡皮擦 · 滚轮改直径",
        "Pixel eraser · wheel changes diameter",
        Tool::PixelEraser,
    ),
    ("eraser", "对象橡皮擦", "Object eraser", Tool::Eraser),
    (
        "lasso",
        "套索裁剪 · Enter 应用",
        "Lasso crop · Enter applies",
        Tool::Lasso,
    ),
    (
        "smart-regions",
        "智能马赛克 · 点击切换 · 框选添加 · Ctrl+A 全选",
        "Smart mosaic · click toggles · drag adds · Ctrl+A selects all",
        Tool::SmartMosaic,
    ),
];

/// 主栏按钮的 GPUI 悬停组名；角标只在所属按钮悬停时显示。
const BUTTON_GROUP: &str = "toolbar-button";

/// “更多”弹层顶部页签高度与对象图标按钮边长。
const MORE_TAB_HEIGHT: f32 = 26.;
const OBJECT_BUTTON: f32 = 30.;
/// “绘制工具”页按用途分组；只列 `TOOLS`／`EXTRA` 里已有的工具。
const MORE_TOOL_GROUPS: &[(crate::i18n::Text, &[Tool])] = &[
    (
        crate::i18n::Text("选择与裁剪", "Select and crop"),
        &[Tool::Crop, Tool::Edit, Tool::Lasso],
    ),
    (
        crate::i18n::Text("线条与形状", "Lines and shapes"),
        &[
            Tool::Rectangle,
            Tool::Ellipse,
            Tool::Triangle,
            Tool::Diamond,
            Tool::Star,
            Tool::Arrow,
            Tool::Polyline,
            Tool::Pen,
            Tool::Serial,
        ],
    ),
    (
        crate::i18n::Text("文字与强调", "Text and emphasis"),
        &[
            Tool::Text,
            Tool::Watermark,
            Tool::Highlighter,
            Tool::HighlighterRect,
            Tool::Spotlight,
        ],
    ),
    (
        crate::i18n::Text("遮挡与擦除", "Redact and erase"),
        &[
            Tool::Mosaic,
            Tool::Blur,
            Tool::MosaicBrush,
            Tool::BlurBrush,
            Tool::SmartMosaic,
            Tool::LocalFill,
            Tool::PixelEraser,
            Tool::Eraser,
        ],
    ),
];

/// “更多操作”的一项：固定 UI 标识、图标、名称、快捷键与可选的补充提示。
#[derive(Clone, Copy)]
struct MoreItem {
    id: &'static str,
    icon: Icon,
    label: &'static str,
    key: Option<&'static str>,
    hint: Option<&'static str>,
    action: Action,
}
impl MoreItem {
    fn new(
        id: &'static str,
        icon: Icon,
        label: &'static str,
        key: Option<&'static str>,
        action: Action,
    ) -> Self {
        Self {
            id,
            icon,
            label,
            key,
            hint: None,
            action,
        }
    }
    fn hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }
    /// 图标按钮的悬停提示：名称 · 快捷键。
    fn tooltip(&self) -> SharedString {
        match self.key {
            Some(key) => format!("{} · {key}", self.label).into(),
            None => self.label.into(),
        }
    }
}

/// 下拉列表中各工具的固定 UI 标识；与主栏按钮区分，避免同帧探针重名。
fn flyout_option_id(tool: Tool) -> &'static str {
    match tool {
        Tool::Rectangle => "flyout-rectangle",
        Tool::Ellipse => "flyout-ellipse",
        Tool::Triangle => "flyout-triangle",
        Tool::Diamond => "flyout-diamond",
        Tool::Star => "flyout-star",
        Tool::Polyline => "flyout-polyline",
        Tool::Pen => "flyout-pen",
        Tool::Highlighter => "flyout-highlighter",
        Tool::HighlighterRect => "flyout-highlighter-rect",
        Tool::Text => "flyout-text",
        Tool::Watermark => "flyout-watermark",
        Tool::Mosaic => "flyout-mosaic",
        Tool::Blur => "flyout-blur",
        Tool::MosaicBrush => "flyout-mosaic-brush",
        Tool::BlurBrush => "flyout-blur-brush",
        Tool::LocalFill => "flyout-local-fill",
        Tool::SmartMosaic => "flyout-smart-mosaic",
        Tool::PixelEraser => "flyout-pixel-eraser",
        Tool::Eraser => "flyout-eraser",
        _ => "flyout-tool",
    }
}

fn flyout_root_id(flyout: Flyout) -> &'static str {
    match flyout {
        Flyout::Tools(group) => group.id(),
        Flyout::Recognize => "group-recognize",
    }
}

fn recognize_icon(mode: RecognizeMode) -> Icon {
    match mode {
        RecognizeMode::Text => Icon::Ocr,
        RecognizeMode::TextTranslate => Icon::OcrTranslate,
        RecognizeMode::Table => Icon::Table,
        RecognizeMode::Formula => Icon::Formula,
        RecognizeMode::ImageTranslate => Icon::Translate,
        RecognizeMode::Qr => Icon::Qr,
    }
}

pub(crate) fn tool_description(tool: Tool) -> (&'static str, &'static str) {
    TOOLS
        .iter()
        .chain(EXTRA.iter())
        .find(|item| item.3 == tool)
        .map(|item| (item.0, crate::i18n::tr(item.1, item.2)))
        .expect("全部工具都有显示描述")
}

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub background: u32,
    pub foreground: u32,
    pub muted: u32,
    pub border: u32,
    pub hover: u32,
    pub selected: u32,
    pub accent: u32,
    pub on_accent: u32,
    pub danger: u32,
    /// 分段槽、数值框、下拉与输入框底；悬停与滑杆未填充段用 `track_hover`。
    pub track: u32,
    pub track_hover: u32,
    /// 选中色块的外环；不用强调绿，免得和绿色色块混淆。
    pub ring: u32,
    /// 色点内描边，RGBA（`0xRRGGBBAA`），用 `rgba` 绘制。
    pub swatch_edge: u32,
    pub tip_bg: u32,
    pub tip_fg: u32,
    pub tip_muted: u32,
    pub accent_hover: u32,
    /// 删除悬停底，RGBA（`0xRRGGBBAA`）。
    pub danger_bg: u32,
    /// 截图左下提示卡（快捷键、状态）用中性石墨面，不随工具栏的蓝灰底；均为 RGBA。
    pub hint_bg: u32,
    pub hint_border: u32,
    pub hint_fg: u32,
    pub hint_muted: u32,
    /// 按键芯片底；鼠标操作芯片不填底，用 `hint_muted` 虚线描边。
    pub hint_chip: u32,
}
impl Palette {
    pub fn new(light: bool) -> Self {
        if light {
            Self {
                background: 0xfafcfe,
                foreground: 0x374351,
                muted: 0x667482,
                border: 0xdce3e9,
                hover: 0xeaf0f3,
                selected: 0xdcefe7,
                accent: 0x126d4e,
                on_accent: 0xffffff,
                danger: 0xc62828,
                track: 0xeef2f5,
                track_hover: 0xe1e8ed,
                ring: 0x374351,
                swatch_edge: 0x0000_0029,
                tip_bg: 0x2b333c,
                tip_fg: 0xf4f7f9,
                tip_muted: 0xb3bec9,
                accent_hover: 0x0e5a40,
                danger_bg: 0xc628_281a,
                hint_bg: 0xfbfbfb_e6,
                hint_border: 0x000000_14,
                hint_fg: 0x1c1c1c_ff,
                hint_muted: 0x1c1c1c_8f,
                hint_chip: 0x000000_0f,
            }
        } else {
            Self {
                background: 0x1d222a,
                foreground: 0xd9e2e9,
                muted: 0x8997a6,
                border: 0x3d4651,
                hover: 0x303a45,
                selected: 0x28473d,
                accent: 0x40ce9d,
                on_accent: 0x10231c,
                danger: 0xff737f,
                track: 0x262c35,
                track_hover: 0x323b47,
                ring: 0xe6edf3,
                swatch_edge: 0xffff_ff33,
                tip_bg: 0x0b0e12,
                tip_fg: 0xe6edf3,
                tip_muted: 0x8f9cab,
                accent_hover: 0x5bd8ac,
                danger_bg: 0xff73_7f24,
                hint_bg: 0x262628_dc,
                hint_border: 0xffffff_12,
                hint_fg: 0xf3f3f3_ff,
                hint_muted: 0xf3f3f3_94,
                hint_chip: 0xffffff_1a,
            }
        }
    }
}
use ui_kit::Tip as Hint;
impl CaptureView {
    pub(crate) fn probe(&self, id: &'static str) -> impl IntoElement {
        let positions = self.ui_positions.clone();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, _, _| {
                positions.borrow_mut().insert(
                    id,
                    (
                        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
                        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
                    ),
                );
            },
        )
        .absolute()
        .left(px(0.))
        .top(px(0.))
        .size_full()
    }
    pub(crate) fn chip(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
    ) -> gpui::Stateful<gpui::Div> {
        self.chip_enabled(id, label, active, true)
    }
    /// 不可用的小按钮变暗，且无手型光标与悬停高亮；调用方负责忽略其点击。
    pub(crate) fn chip_enabled(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
        enabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let p = Palette::new(self.light_theme);
        div()
            .relative()
            .id(id)
            .px(px(5.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .flex_shrink_0()
            .text_size(px(12.))
            .text_color(rgb(if active { p.accent } else { p.foreground }))
            .bg(if active { rgb(p.selected) } else { rgba(0) })
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
            })
            .when(!enabled, |d| d.opacity(0.4))
            .child(label.into())
            .child(self.probe(id))
    }
    pub(crate) fn panel(&self, id: &'static str, rect: PanelRect) -> gpui::Stateful<gpui::Div> {
        let p = Palette::new(self.light_theme);
        div()
            .relative()
            .id(id)
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .shadow_lg()
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .cursor(gpui::CursorStyle::Arrow)
            .capture_any_mouse_down(|_, window, cx| {
                // 子控件可能消费 Down；在捕获阶段撤销跨交接首击，避免串到下一次画布点击。
                if let Some(Some(view)) = window.root::<CaptureView>() {
                    view.update(cx, |view, _| view.invalidate_click_sequence());
                }
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, window, cx| {
                if let Some(Some(view)) = window.root::<CaptureView>() {
                    view.update(cx, |view, _| view.invalidate_click_sequence());
                }
                cx.stop_propagation();
            })
    }
    pub(crate) fn separator(&self) -> gpui::Div {
        div()
            .w(px(1.))
            .h(px(18.))
            .mx(px(4.))
            .flex_shrink_0()
            .bg(rgb(Palette::new(self.light_theme).border))
    }
    /// 主栏按钮右下角的快捷键角标，仅在悬停该按钮时显示。
    fn shortcut_badge(&self, text: SharedString) -> gpui::Div {
        div()
            .absolute()
            .right(px(1.))
            .bottom(px(0.))
            .text_size(px(9.))
            .line_height(px(10.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(Palette::new(self.light_theme).muted))
            .opacity(0.)
            .group_hover(BUTTON_GROUP, |s| s.opacity(1.))
            .child(text)
    }
    fn tool_button(
        &self,
        id: &'static str,
        hint: &'static str,
        tool: Tool,
        shortcut: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.editor.tool == tool;
        let p = Palette::new(self.light_theme);
        let color = if active { p.accent } else { p.foreground };
        let hint = if self.editor.long_clip().is_some() && tool == Tool::Crop {
            tr!("剪辑 · 拖选删段，拖边裁剪", "Clip · drag bands or edges")
        } else {
            hint
        };
        let tip: SharedString = if self.editor.long_clip().is_some() {
            shortcut
                .as_ref()
                .map_or_else(|| hint.to_owned(), |key| format!("{hint} · {key}"))
        } else {
            shortcut_hints::tooltip(
                hint,
                shortcut_hints::for_tool(tool, self.shortcut_context(), &self.toolbar_config),
            )
        }
        .into();
        div()
            .relative()
            .id(id)
            .group(BUTTON_GROUP)
            .h(px(32.))
            .w(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .bg(if active { rgb(p.selected) } else { rgba(0) })
            .hover(move |s| s.bg(rgb(p.hover)))
            .text_color(rgb(color))
            .child(toolbar_icons::icon(tool, color))
            .when_some(shortcut, |d, key| d.child(self.shortcut_badge(key)))
            .child(self.probe(id))
            .tooltip(move |_, cx| cx.new(|_| Hint(tip.clone())).into())
            .on_click(cx.listener(move |v, _, w, cx| {
                let tool = if v.image_canvas.is_none() && !v.pin_mode && tool == Tool::Edit {
                    crate::selection_action_layout::mouse_mode_tool(v.editor.tool)
                } else {
                    tool
                };
                v.perform(Action::Tool(tool), w, cx)
            }))
    }
    #[allow(clippy::too_many_arguments)]
    fn action_button(
        &self,
        id: &'static str,
        icon: Icon,
        hint: &'static str,
        action: Action,
        enabled: bool,
        badge: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let active = id == "copy"
            || (id == "more"
                && (self.toolbar_panel.more()
                    || !matches!(self.editor.tool, Tool::Crop | Tool::Edit)
                        && !self
                            .toolbar_config
                            .slots()
                            .iter()
                            .any(|slot| slot.contains(self.editor.tool))));
        div()
            .relative()
            .id(id)
            .group(BUTTON_GROUP)
            .size(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .bg(if active { rgb(p.selected) } else { rgba(0) })
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
            })
            .child(toolbar_icons::action(
                icon,
                if active { p.accent } else { p.foreground },
            ))
            .when_some(badge, |d, badge| d.child(self.shortcut_badge(badge.into())))
            .child(self.probe(id))
            .tooltip(move |_, cx| cx.new(|_| Hint(hint.into())).into())
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.perform(action, w, cx);
                }
            }))
    }
    /// 普通截图才有智能马赛克；图片编辑与贴图的打码组不列出。
    pub(crate) fn smart_mosaic_available(&self) -> bool {
        !self.pin_mode && self.image_canvas.is_none()
    }
    fn shortcut_context(&self) -> shortcut_hints::Context {
        if self.pin_mode {
            shortcut_hints::Context::Pin
        } else {
            shortcut_hints::Context::Capture {
                smart_mosaic: self.smart_mosaic_available(),
            }
        }
    }
    /// 拖动、绘制或已有菜单时不因悬停弹出下拉。
    fn flyout_hover_allowed(&self) -> bool {
        self.toolbar_panel == ToolbarPanel::None
            && self.toolbar_drag.is_none()
            && self.pointer_down_start.is_none()
            && !self.editor.is_dragging()
    }
    fn schedule_flyout(
        &mut self,
        timer: Option<(u64, std::time::Duration)>,
        cx: &mut Context<Self>,
    ) {
        let Some((generation, delay)) = timer else {
            return;
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            executor.timer(delay).await;
            let _ = view.update(cx, |view, cx| {
                if view.flyout.fire(generation) {
                    cx.notify();
                }
            });
        })
        .detach();
    }
    fn flyout_root_hover(&mut self, flyout: Flyout, hovered: bool, cx: &mut Context<Self>) {
        let timer = if hovered {
            let allowed = self.flyout_hover_allowed();
            self.flyout.enter_root(flyout, allowed)
        } else {
            self.flyout.leave_root(flyout)
        };
        self.schedule_flyout(timer, cx);
    }
    /// 分组按钮：左侧执行当前显示的项，右侧箭头展开下拉；悬停稍停也会展开。
    #[allow(clippy::too_many_arguments)]
    fn split_button(
        &self,
        flyout: Flyout,
        main_id: &'static str,
        main_icon: gpui::Svg,
        active: bool,
        enabled: bool,
        tip: SharedString,
        badge: Option<SharedString>,
        on_main: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let open = self.flyout.open() == Some(flyout);
        let root_id = flyout_root_id(flyout);
        let chevron_id = match flyout {
            Flyout::Tools(ToolGroup::Shape) => "group-shape-menu",
            Flyout::Tools(ToolGroup::Line) => "group-line-menu",
            Flyout::Tools(ToolGroup::Mark) => "group-mark-menu",
            Flyout::Tools(ToolGroup::Text) => "group-text-menu",
            Flyout::Tools(ToolGroup::Mosaic) => "group-mosaic-menu",
            Flyout::Tools(ToolGroup::Eraser) => "group-eraser-menu",
            Flyout::Recognize => "group-recognize-menu",
        };
        let main = div()
            .relative()
            .id(main_id)
            .group(BUTTON_GROUP)
            .h_full()
            .w(px(GROUP_WIDTH - 14.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_l_md()
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
            })
            .child(main_icon)
            .when_some(badge, |d, key| d.child(self.shortcut_badge(key)))
            .child(self.probe(main_id))
            .when(!open, |d| {
                d.tooltip(move |_, cx| cx.new(|_| Hint(tip.clone())).into())
            })
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    on_main(v, w, cx);
                }
            }));
        let chevron = div()
            .relative()
            .id(chevron_id)
            .h_full()
            .w(px(14.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_r_md()
            .bg(if open { rgb(p.hover) } else { rgba(0) })
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
            })
            .child(
                toolbar_icons::action(Icon::Chevron, if open { p.accent } else { p.muted })
                    .size(px(10.)),
            )
            .child(self.probe(chevron_id))
            .when(!open, |d| {
                d.tooltip(move |_, cx| cx.new(|_| Hint(tr!("更多", "More").into())).into())
            })
            .on_click(cx.listener(move |v, _, _, cx| {
                if enabled {
                    v.toolbar_panel = ToolbarPanel::None;
                    v.flyout.click(flyout);
                    cx.notify();
                }
            }));
        div()
            .relative()
            .id(root_id)
            .h(px(32.))
            .w(px(GROUP_WIDTH))
            .flex_shrink_0()
            .flex()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .bg(if active { rgb(p.selected) } else { rgba(0) })
            .on_hover(cx.listener(move |v, hovered: &bool, _, cx| {
                v.flyout_root_hover(flyout, *hovered, cx)
            }))
            .child(main)
            .child(chevron)
            .child(self.probe(root_id))
    }
    /// 当前宿主可用的识别项；贴图没有原位结果面板，只保留文字与二维码识别。
    pub(crate) fn recognize_modes(&self) -> impl Iterator<Item = RecognizeMode> + '_ {
        let text_host = self.inline_ocr_host();
        toolbar_groups::RECOGNIZE_MODES
            .into_iter()
            .filter(move |mode| text_host || !mode.needs_text_host())
    }
    fn shown_recognize_mode(&self) -> RecognizeMode {
        let mode = self.recognize_mode;
        if self.recognize_modes().any(|m| m == mode) {
            mode
        } else {
            RecognizeMode::Text
        }
    }
    fn recognize_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let shown = self.shown_recognize_mode();
        let enabled = !self.processing;
        let tip: SharedString = shown.hint().get().into();
        self.split_button(
            Flyout::Recognize,
            "ocr",
            toolbar_icons::action(recognize_icon(shown), p.foreground),
            false,
            enabled,
            tip,
            shown.key().map(SharedString::from),
            move |v, w, cx| v.perform(Action::Recognize(shown), w, cx),
            cx,
        )
    }
    fn group_button(
        &self,
        group: ToolGroup,
        shortcut: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let smart = self.smart_mosaic_available();
        let shown = self.tool_groups.shown(group, self.editor.tool, smart);
        let active = group.options(smart).any(|tool| tool == self.editor.tool);
        let p = Palette::new(self.light_theme);
        let (id, hint) = tool_description(shown);
        let tip: SharedString = shortcut_hints::group_tooltip(
            hint,
            group,
            shown,
            self.shortcut_context(),
            &self.toolbar_config,
        )
        .into();
        self.split_button(
            Flyout::Tools(group),
            id,
            toolbar_icons::icon(shown, if active { p.accent } else { p.foreground }),
            active,
            true,
            tip,
            shortcut,
            move |v, w, cx| v.perform(Action::Tool(shown), w, cx),
            cx,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn flyout_row(
        &self,
        id: &'static str,
        icon: gpui::Svg,
        label: &'static str,
        key: Option<&'static str>,
        checked: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        ui_kit::menu_row(
            p,
            id,
            Some(icon),
            label,
            key.map(|key| ui_kit::trailing_text(p, key)),
            checked,
            true,
        )
        .child(self.probe(id))
        .on_click(cx.listener(move |v, _, w, cx| {
            v.flyout.close();
            on_click(v, w, cx);
            cx.notify();
        }))
    }
    fn flyout_panel(
        &self,
        flyout: Flyout,
        layout: ToolbarLayout,
        anchor: (f32, f32),
        properties: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let width = match (flyout, crate::i18n::is_english()) {
            (Flyout::Recognize, true) => 232.,
            (_, true) => 196.,
            (_, false) => 168.,
        };
        let mut rows = div().flex().flex_col().gap(px(2.));
        let count = match flyout {
            Flyout::Recognize => {
                let shown = self.shown_recognize_mode();
                let mut count = 0;
                for mode in self.recognize_modes() {
                    count += 1;
                    let color = if mode == shown { p.accent } else { p.muted };
                    rows = rows.child(self.flyout_row(
                        mode.id(),
                        toolbar_icons::action(recognize_icon(mode), color),
                        mode.label().get(),
                        mode.key(),
                        mode == shown,
                        move |v, w, cx| v.perform(Action::Recognize(mode), w, cx),
                        cx,
                    ));
                }
                count
            }
            Flyout::Tools(group) => {
                let smart = self.smart_mosaic_available();
                let shown = self.tool_groups.shown(group, self.editor.tool, smart);
                let mut count = 0;
                for tool in group.options(smart) {
                    count += 1;
                    let color = if tool == shown { p.accent } else { p.muted };
                    rows = rows.child(self.flyout_row(
                        flyout_option_id(tool),
                        toolbar_icons::icon(tool, color),
                        tool.label(),
                        shortcut_hints::letter(tool, self.shortcut_context()),
                        tool == shown,
                        move |v, w, cx| v.perform(Action::Tool(tool), w, cx),
                        cx,
                    ));
                }
                count
            }
        };
        let height = count as f32 * ui_kit::MENU_ROW + (count as f32 - 1.) * 2. + 12.;
        let rect = layout.flyout(anchor, (GROUP_WIDTH, 32.), width, height, properties);
        self.panel("toolbar-flyout", rect)
            .p(px(5.))
            .overflow_y_scroll()
            .on_hover(cx.listener(|v, hovered: &bool, _, cx| {
                if *hovered {
                    v.flyout.enter_panel();
                } else {
                    let timer = v.flyout.leave_panel();
                    v.schedule_flyout(timer, cx);
                }
            }))
            .child(rows)
            .child(self.probe("toolbar-flyout"))
    }
    fn primary_toolbar(&self, layout: ToolbarLayout, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let origin = layout.primary;
        let mut bar = self
            .panel("primary-toolbar", origin)
            .px_2()
            .py(px(7.))
            .flex()
            .flex_wrap()
            .gap(px(2.))
            .items_center();
        bar = bar.child(
            div()
                .id("toolbar-grip")
                .group("toolbar-grip")
                .w(px(GRIP_WIDTH))
                .h(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor(gpui::CursorStyle::ResizeAll)
                .hover(move |s| s.bg(rgb(p.hover)))
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        Hint(
                            tr!(
                                "拖动调整工具栏位置 · 双击恢复默认",
                                "Drag to move the toolbar · double-click to reset"
                            )
                            .into(),
                        )
                    })
                    .into()
                })
                .child(
                    toolbar_icons::action(Icon::Grip, p.muted)
                        .w(px(10.))
                        .h(px(20.))
                        .group_hover("toolbar-grip", move |s| s.text_color(rgb(p.foreground))),
                )
                .child(self.probe("toolbar-grip"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, e: &gpui::MouseDownEvent, w, cx| {
                        cx.stop_propagation();
                        if v.refresh_request.is_some() {
                            return;
                        }
                        v.invalidate_click_sequence();
                        v.toolbar_panel = ToolbarPanel::None;
                        v.flyout.close();
                        if e.click_count == 2 {
                            // 双击把手恢复默认位置（跟随选区）。
                            v.follow_toolbar();
                            cx.notify();
                            return;
                        }
                        v.begin_toolbar_drag(e.position, (origin.x, origin.y), w);
                        unsafe {
                            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(
                                crate::gpui_capture::hwnd(w),
                            );
                        }
                        cx.notify();
                    }),
                ),
        );
        for tool in [Tool::Crop, Tool::Edit]
            .into_iter()
            .filter(|tool| *tool != Tool::Crop || self.image_canvas.is_some() || self.pin_mode)
        {
            let (id, hint) = tool_description(tool);
            let badge = if self.editor.long_clip().is_some() {
                Some(SharedString::from(if tool == Tool::Crop {
                    "1"
                } else {
                    "2"
                }))
            } else {
                (tool == Tool::Edit).then(|| SharedString::from("0"))
            };
            bar = bar.child(self.tool_button(id, hint, tool, badge, cx));
        }
        for (index, slot) in self.toolbar_config.slots().into_iter().enumerate() {
            let shortcut = if self.editor.long_clip().is_some() {
                Some(SharedString::from((index + 3).to_string()))
            } else {
                toolbar_groups::shortcut_number(index).map(|n| SharedString::from(n.to_string()))
            };
            bar = match slot {
                Slot::Tool(tool) => {
                    let (id, hint) = tool_description(tool);
                    bar.child(self.tool_button(id, hint, tool, shortcut, cx))
                }
                Slot::Group(group) => bar.child(self.group_button(group, shortcut, cx)),
            };
        }
        bar = bar.child(self.separator());
        for (id, icon, hint, action, enabled) in [
            (
                "undo",
                Icon::Undo,
                tr!("撤销 · Ctrl+Z", "Undo · Ctrl+Z"),
                Action::Undo,
                self.editor.document.can_undo(),
            ),
            (
                "redo",
                Icon::Redo,
                tr!("重做 · Ctrl+Y", "Redo · Ctrl+Y"),
                Action::Redo,
                self.editor.document.can_redo(),
            ),
            (
                "more",
                Icon::More,
                tr!("全部工具与更多操作", "All tools and more actions"),
                Action::More,
                true,
            ),
        ]
        .into_iter()
        .filter(|(id, ..)| *id != "more" || self.image_canvas.is_some() || self.pin_mode)
        {
            bar = bar.child(self.action_button(id, icon, hint, action, enabled, None, cx));
        }
        // 按用途分组：采集模式 | 识别与翻译 | 贴图／保存 ✕ ✓；“完成”放最右，
        // 与主流截图工具一致，取消与完成相邻但完成在末端。
        let output = self.image_canvas.is_none() && !self.pin_mode && !self.processing;
        for (id, icon, hint, action, enabled, badge) in [
            (
                "long-capture-primary",
                Icon::LongCapture,
                tr!(
                    "长截图 · L · 框选滚动内容后连续采集",
                    "Long capture · L · select scrolling content to capture continuously"
                ),
                Action::LongCapture,
                output,
                Some("L"),
            ),
            (
                "record-primary",
                Icon::Video,
                tr!(
                    "录屏 · G · 框选范围后录制 MP4 / GIF",
                    "Record · G · record the selected area as MP4 / GIF"
                ),
                Action::Record,
                output,
                Some("G"),
            ),
            (
                "pin",
                Icon::Pin,
                if self.t_translates() {
                    tr!("贴图 · 中键", "Pin · middle-click")
                } else {
                    tr!("贴图 · T", "Pin · T")
                },
                Action::Pin,
                true,
                (!self.t_translates()).then_some("T"),
            ),
            (
                "save",
                Icon::Save,
                if self.image_canvas.is_some() {
                    tr!("另存为 · Ctrl+S", "Save as · Ctrl+S")
                } else {
                    tr!("保存 · Ctrl+S", "Save · Ctrl+S")
                },
                Action::Save,
                true,
                None,
            ),
            (
                "cancel",
                Icon::Close,
                if self.image_canvas.is_some() {
                    tr!("关闭当前图片 · Esc", "Close image · Esc")
                } else {
                    tr!("取消 · Esc", "Cancel · Esc")
                },
                Action::Cancel,
                true,
                None,
            ),
            (
                "copy",
                Icon::Confirm,
                if self.image_canvas.is_some() {
                    tr!("复制图片 · Enter / Ctrl+C", "Copy image · Enter / Ctrl+C")
                } else {
                    tr!(
                        "完成 · Enter（Ctrl+C 复制图片）",
                        "Finish · Enter (Ctrl+C copies image)"
                    )
                },
                Action::Confirm,
                true,
                None,
            ),
        ] {
            if matches!(id, "long-capture-primary" | "pin") {
                bar = bar.child(self.separator());
            }
            bar = bar.child(self.action_button(id, icon, hint, action, enabled, badge, cx));
            if id == "record-primary" {
                // 识别、翻译、表格与二维码合为一个带下拉的按钮。
                bar = bar.child(self.separator()).child(self.recognize_button(cx));
            }
        }
        bar.child(self.probe("primary-toolbar"))
    }
    /// “更多”弹层中对象操作的图标条；名称与快捷键放在悬停提示里。
    fn more_object_items(&self) -> [MoreItem; 10] {
        [
            MoreItem::new(
                "delete",
                Icon::Trash,
                tr!("删除对象", "Delete"),
                Some("Del"),
                Action::Delete,
            ),
            MoreItem::new(
                "duplicate",
                Icon::Duplicate,
                tr!("复制对象", "Duplicate"),
                Some("Ctrl+D"),
                Action::Duplicate,
            ),
            MoreItem::new(
                "group-objects",
                Icon::Group,
                tr!("组合", "Group"),
                Some("Ctrl+G"),
                Action::Group,
            ),
            MoreItem::new(
                "ungroup-objects",
                Icon::Ungroup,
                tr!("拆分", "Ungroup"),
                Some("Ctrl+Shift+G"),
                Action::Ungroup,
            ),
            MoreItem::new(
                "enter-group-more",
                Icon::EnterGroup,
                tr!("进入组合", "Enter group"),
                None,
                Action::EnterGroup,
            ),
            MoreItem::new(
                "exit-group-more",
                Icon::ParentGroup,
                tr!("返回上层", "Parent group"),
                None,
                Action::ExitGroup,
            ),
            MoreItem::new(
                "root-group-more",
                Icon::Layers,
                tr!("最外层", "Root level"),
                None,
                Action::RootGroup,
            ),
            MoreItem::new(
                "front",
                Icon::BringFront,
                tr!("置顶层", "Bring to front"),
                Some("Ctrl+]"),
                Action::Front,
            ),
            MoreItem::new(
                "back",
                Icon::SendBack,
                tr!("置底层", "Send to back"),
                Some("Ctrl+["),
                Action::Back,
            ),
            MoreItem::new(
                "clear",
                Icon::Eraser,
                tr!("清空标注", "Clear all"),
                Some("Shift+Del"),
                Action::Clear,
            ),
        ]
    }
    /// “更多操作”的分组；按当前模式过滤，尺寸估算与绘制共用。
    fn more_sections(&self) -> Vec<(&'static str, Vec<MoreItem>)> {
        let image = self.image_canvas.is_some();
        let capture = !self.pin_mode && !image;
        let area = vec![
            MoreItem::new(
                "monitor",
                Icon::Monitor,
                tr!("当前屏", "Current screen"),
                capture.then_some("Alt+A"),
                Action::Current,
            ),
            MoreItem::new(
                "all",
                Icon::Monitors,
                if image {
                    tr!("完整图片", "Whole image")
                } else {
                    tr!("全部屏", "All screens")
                },
                capture.then_some("Ctrl+Shift+A"),
                Action::All,
            ),
            MoreItem::new(
                "reselect",
                Icon::Reselect,
                tr!("重选范围", "Reselect area"),
                None,
                Action::Reselect,
            ),
            MoreItem::new(
                "refresh",
                Icon::Refresh,
                tr!("刷新截图", "Refresh"),
                Some("F5"),
                Action::Refresh,
            )
            .hint(tr!(
                "刷新截图 · F5 · 保留选区和标注",
                "Refresh · F5 · keeps selection and annotations"
            )),
            MoreItem::new(
                "toolbar-follow",
                Icon::Toolbar,
                tr!("工具栏跟随选区", "Follow selection"),
                None,
                Action::FollowToolbar,
            ),
            MoreItem::new(
                "selection-finish",
                Icon::Sliders,
                tr!("选区外观", "Selection style"),
                None,
                Action::Panel(ToolbarPanel::Finish),
            ),
            MoreItem::new(
                "quick-save",
                Icon::Save,
                tr!("快速保存", "Quick save"),
                Some("Ctrl+Shift+S"),
                Action::QuickSave,
            ),
        ];
        let tools = vec![
            MoreItem::new(
                "record",
                Icon::Video,
                tr!("录屏", "Record"),
                Some("G"),
                Action::Record,
            ),
            MoreItem::new(
                "long-capture",
                Icon::LongCapture,
                tr!("长截图", "Long capture"),
                Some("L"),
                Action::LongCapture,
            ),
            MoreItem::new(
                "window-pin",
                Icon::Pin,
                tr!("实时窗口贴图", "Live window pin"),
                Some("Shift+T"),
                Action::WindowPin,
            ),
            MoreItem::new(
                "translate-image",
                Icon::Translate,
                tr!("图片翻译", "Translate image"),
                Some("Y"),
                Action::TranslateImage,
            ),
            MoreItem::new(
                "qr",
                Icon::Qr,
                tr!("二维码识别", "Scan QR code"),
                None,
                Action::Qr,
            ),
            MoreItem::new(
                "smart-mosaic",
                Icon::SmartMosaic,
                tr!("智能马赛克", "Smart mosaic"),
                None,
                Action::SmartMosaic,
            ),
            MoreItem::new(
                "control_text",
                Icon::Text,
                tr!("控件文字", "Control text"),
                Some("Shift+C"),
                Action::ControlText,
            ),
        ];
        let view = vec![
            MoreItem::new(
                "magnifier",
                Icon::Search,
                tr!("放大镜", "Magnifier"),
                None,
                Action::Magnifier,
            ),
            MoreItem::new(
                "guides",
                Icon::Guides,
                tr!("辅助线", "Guides"),
                None,
                Action::Guides,
            ),
            MoreItem::new(
                "cursor",
                Icon::Edit,
                tr!("鼠标指针", "Mouse pointer"),
                None,
                Action::Cursor,
            ),
            MoreItem::new(
                "theme",
                Icon::Theme,
                tr!("主题", "Theme"),
                None,
                Action::Theme,
            ),
            MoreItem::new(
                "capture-help",
                Icon::Keyboard,
                tr!("快捷键帮助", "Shortcut help"),
                Some("Shift+F1"),
                Action::Help,
            ),
            MoreItem::new(
                "history",
                Icon::History,
                tr!("历史记录", "History"),
                None,
                Action::History,
            ),
            MoreItem::new(
                "settings",
                Icon::Settings,
                tr!("设置", "Settings"),
                None,
                Action::Settings,
            ),
        ];
        // 图片编辑与贴图没有冻结桌面可刷新，也没有可查询的界面控件；
        // 控件文字和智能马赛克在这里只会退化成普通 OCR，与识别按钮重复。
        let keep = |item: &MoreItem| {
            !(self.pin_mode && matches!(item.action, Action::WindowPin)
                || image
                    && matches!(
                        item.action,
                        Action::Current
                            | Action::WindowPin
                            | Action::Record
                            | Action::LongCapture
                            | Action::Magnifier
                            | Action::Cursor
                            | Action::Refresh
                            | Action::ControlText
                            | Action::SmartMosaic
                    ))
        };
        [
            (tr!("范围与输出", "Area and output"), area),
            (tr!("采集与识别", "Capture and recognize"), tools),
            (tr!("显示与设置", "View and settings"), view),
        ]
        .into_iter()
        .map(|(title, items)| (title, items.into_iter().filter(keep).collect::<Vec<_>>()))
        .filter(|(_, items)| !items.is_empty())
        .collect()
    }
    /// “绘制工具”的分组；与主栏下拉同一批工具，按用途排列。
    fn more_tool_sections(&self) -> Vec<(&'static str, Vec<Tool>)> {
        let smart = self.smart_mosaic_available();
        MORE_TOOL_GROUPS
            .iter()
            .map(|(title, tools)| {
                (
                    title.get(),
                    tools
                        .iter()
                        .copied()
                        .filter(|tool| *tool != Tool::SmartMosaic || smart)
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
    /// “更多”弹层尺寸：按当前页的分组与行数计算，放不下由布局压到显示器内并滚动。
    pub(crate) fn more_popup_size(&self) -> (f32, f32) {
        let width = if crate::i18n::is_english() {
            392.
        } else {
            360.
        };
        let sections: Vec<usize> = if self.toolbar_panel == ToolbarPanel::Actions {
            self.more_sections()
                .iter()
                .map(|(_, items)| items.len())
                .collect()
        } else {
            self.more_tool_sections()
                .iter()
                .map(|(_, tools)| tools.len())
                .collect()
        };
        let mut body: f32 = sections
            .iter()
            .map(|&count| ui_kit::MENU_SECTION + ui_kit::two_columns_height(count))
            .sum();
        if self.toolbar_panel == ToolbarPanel::Actions {
            body += ui_kit::MENU_SECTION + OBJECT_BUTTON;
        }
        // 页签区：上内边距、页签（含 2px 槽边）、下留白；外加正文底边距与 1px 描边。
        let header = ui_kit::MENU_PAD + MORE_TAB_HEIGHT + 4. + 2.;
        (width, (header + body + ui_kit::MENU_PAD + 2.).min(560.))
    }
    fn more_tabs(&self, actions: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let raised = if self.light_theme {
            p.background
        } else {
            p.track_hover
        };
        let tab = |id: &'static str, label: &'static str, on: bool, panel: ToolbarPanel| {
            div()
                .relative()
                .id(id)
                .flex_1()
                .h(px(MORE_TAB_HEIGHT))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_size(px(12.))
                .when(on, |d| {
                    d.bg(rgb(raised))
                        .text_color(rgb(p.foreground))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .shadow_sm()
                })
                .when(!on, |d| {
                    d.text_color(rgb(p.muted))
                        .cursor_pointer()
                        .hover(move |s| s.text_color(rgb(p.foreground)))
                })
                .child(label)
                .child(self.probe(id))
                .on_click(cx.listener(move |v, _, w, cx| v.perform(Action::Panel(panel), w, cx)))
        };
        div()
            .flex()
            .flex_shrink_0()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(7.))
            .bg(rgb(p.track))
            .child(tab(
                "more-tools-tab",
                tr!("绘制工具", "Drawing tools"),
                !actions,
                ToolbarPanel::Tools,
            ))
            .child(tab(
                "more-actions-tab",
                tr!("更多操作", "More actions"),
                actions,
                ToolbarPanel::Actions,
            ))
    }
    fn more_enabled(&self, action: Action) -> bool {
        match action {
            Action::Group => self.editor.can_group_selection(),
            Action::Ungroup => self
                .editor
                .selected_ids()
                .iter()
                .any(|id| self.editor.grouped_in_current_scope(*id)),
            Action::EnterGroup => self.editor.can_enter_group_scope(),
            Action::ExitGroup | Action::RootGroup => self.editor.group_scope_depth() > 0,
            _ => true,
        }
    }
    fn more_object_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let mut strip = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(2.))
            .flex_shrink_0();
        for (index, item) in self.more_object_items().into_iter().enumerate() {
            // 删除复制 | 组合层级 | 前后 | 清空
            if matches!(index, 2 | 7 | 9) {
                strip = strip.child(div().w(px(1.)).h(px(16.)).bg(rgb(p.border)));
            }
            let enabled = self.more_enabled(item.action);
            let action = item.action;
            let danger = matches!(item.action, Action::Clear);
            strip = strip.child(
                div()
                    .relative()
                    .id(item.id)
                    .size(px(OBJECT_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .hover(move |s| s.bg(rgb(if danger { p.danger_bg } else { p.hover })))
                    })
                    .when(!enabled, |d| d.opacity(0.32))
                    .child(toolbar_icons::action(item.icon, p.foreground).size(px(16.)))
                    .child(self.probe(item.id))
                    .tooltip(ui_kit::tip(item.tooltip()))
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if enabled {
                            v.perform(action, w, cx);
                        }
                    })),
            );
        }
        strip
    }
    fn more_popup(&self, rect: PanelRect, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let actions = self.toolbar_panel == ToolbarPanel::Actions;
        let mut body = div()
            .id("toolbar-popup-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(px(ui_kit::MENU_PAD))
            .pb(px(ui_kit::MENU_PAD))
            .flex()
            .flex_col();
        if actions {
            body = body
                .child(ui_kit::section_title(p, tr!("对象", "Objects")))
                .child(self.more_object_strip(cx));
            for (title, items) in self.more_sections() {
                let mut cells = Vec::with_capacity(items.len());
                for item in items {
                    let enabled = self.more_enabled(item.action);
                    let active = match item.action {
                        Action::Magnifier => self.magnifier,
                        Action::Guides => self.guides,
                        Action::Cursor => self.include_cursor,
                        Action::SmartMosaic => self.editor.tool == Tool::SmartMosaic,
                        _ => false,
                    };
                    let trailing = if item.id == "theme" {
                        Some(ui_kit::trailing_text(p, crate::theme::mode(cx).label()))
                    } else if active {
                        Some(
                            toolbar_icons::action(Icon::Check, p.accent)
                                .size(px(14.))
                                .into_any_element(),
                        )
                    } else {
                        item.key.map(|key| ui_kit::trailing_text(p, key))
                    };
                    let icon =
                        toolbar_icons::action(item.icon, if active { p.accent } else { p.muted });
                    let action = item.action;
                    cells.push(
                        ui_kit::menu_row(
                            p,
                            item.id,
                            Some(icon),
                            item.label,
                            trailing,
                            active,
                            enabled,
                        )
                        .when_some(item.hint, |row, hint| row.tooltip(ui_kit::tip(hint)))
                        .child(self.probe(item.id))
                        .on_click(cx.listener(move |v, _, w, cx| {
                            if enabled {
                                v.perform(action, w, cx);
                            }
                        }))
                        .into_any_element(),
                    );
                }
                body = body
                    .child(ui_kit::section_title(p, title))
                    .child(ui_kit::two_columns(cells));
            }
        } else {
            let context = self.shortcut_context();
            for (title, tools) in self.more_tool_sections() {
                let mut cells = Vec::with_capacity(tools.len());
                for tool in tools {
                    let (id, hint) = tool_description(tool);
                    let active = self.editor.tool == tool;
                    let icon = toolbar_icons::icon(tool, if active { p.accent } else { p.muted });
                    cells.push(
                        ui_kit::menu_row(
                            p,
                            id,
                            Some(icon),
                            tool.label(),
                            shortcut_hints::letter(tool, context)
                                .map(|key| ui_kit::trailing_text(p, key)),
                            active,
                            true,
                        )
                        .when(hint != tool.label(), |row| row.tooltip(ui_kit::tip(hint)))
                        .child(self.probe(id))
                        .on_click(
                            cx.listener(move |v, _, w, cx| v.perform(Action::Tool(tool), w, cx)),
                        )
                        .into_any_element(),
                    );
                }
                body = body
                    .child(ui_kit::section_title(p, title))
                    .child(ui_kit::two_columns(cells));
            }
        }
        self.panel("toolbar-popup", rect)
            .flex()
            .flex_col()
            .child(
                div()
                    .p(px(ui_kit::MENU_PAD))
                    .pb(px(2.))
                    .flex_shrink_0()
                    .child(self.more_tabs(actions, cx)),
            )
            .child(body)
            .child(self.probe("toolbar-popup"))
    }
    pub fn toolbar(
        &self,
        selected: Rect,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let anchor = self
            .ui_positions
            .borrow()
            .get(self.toolbar_panel.anchor())
            .copied();
        // 下拉对齐上一帧绘制的分组按钮；按钮由悬停或点击触发，必然已绘制过。
        let flyout = self.flyout.open().and_then(|flyout| {
            self.ui_positions
                .borrow()
                .get(flyout_root_id(flyout))
                .map(|anchor| (flyout, *anchor))
        });
        self.ui_positions.borrow_mut().clear();
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        // 长图剪辑：工具在左侧工具栏，属性卡停靠在右侧属性栏，弹层围绕属性卡展开。
        let docked = self.long_clip_property_dock(window);
        let layout = docked.unwrap_or_else(|| self.toolbar_layout(selected, window.scale_factor()));
        let selection_bar = self.capture_selection_bar_layout(selected, window);
        let selection_actions = selection_bar.map(|bar| bar.actions());
        let mut layer = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .when(docked.is_none(), |layer| {
                layer.child(self.primary_toolbar(layout, cx))
            });
        if let Some(preview) = self.toolbar_snap_preview {
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(preview.x))
                    .top(px(preview.y))
                    .w(px(preview.width))
                    .h(px(preview.height))
                    .rounded_lg()
                    .border_2()
                    .border_color(rgb(Palette::new(self.light_theme).accent)),
            );
        }
        let has_finish = tool == Tool::Crop
            && !self.pin_mode
            && self.image_canvas.is_some()
            && self.editor.long_clip().is_none();
        let has_properties = has_finish
            || tool.draws()
            || matches!(tool, Tool::Lasso | Tool::Eraser | Tool::SmartMosaic);
        if has_finish {
            layer =
                layer.child(self.finish_properties(layout.properties, layout.properties_above, cx));
        } else if docked.is_some() {
            if has_properties && layout.properties.width > 0. {
                layer = layer.child(self.property_toolbar_direction(
                    tool,
                    layout.properties,
                    false,
                    layout.property_level(),
                    cx,
                ));
            }
        } else if has_properties {
            layer = layer.child(self.property_toolbar_direction(
                tool,
                layout.properties,
                layout.properties_above,
                layout.property_level(),
                cx,
            ));
        }
        if let Some(bar) = selection_bar {
            layer = layer.child(self.selection_bar(selected, bar, cx));
        }
        if self.toolbar_panel != ToolbarPanel::None {
            if self.toolbar_panel == ToolbarPanel::Finish {
                let width = crate::selection_finish_view::FINISH_POPUP_WIDTH;
                let height = self.finish_popup_height();
                layer = layer.child(self.finish_popup(
                    selection_bar.map_or_else(
                        || layout.popup(width, height, anchor, true),
                        |bar| {
                            // 面板左缘对齐选区条上的样式组。
                            let x = bar.panel.x + self.selection_bar_style_x(selected, window);
                            bar.style_popup(width, height, x)
                        },
                    ),
                    selected,
                    cx,
                ));
            } else if self.toolbar_panel == ToolbarPanel::Help {
                layer = layer.child(self.help_popup(layout.popup(520., 520., anchor, false), cx));
            } else if self.toolbar_panel == ToolbarPanel::SelectionSize {
                let rect = selection_actions.map_or_else(
                    || layout.popup(312., 252., anchor, false),
                    |actions| actions.popup(312., 252.),
                );
                layer = layer.child(
                    self.panel("selection-size-popup", rect)
                        .p_2()
                        .child(self.render_selection_size_popup()),
                );
            } else if self.toolbar_panel.more() {
                let (width, height) = self.more_popup_size();
                let rect = selection_bar.map_or_else(
                    || layout.popup(width, height, anchor, has_properties),
                    |bar| bar.style_popup(width, height, bar.panel.right() - width),
                );
                layer = layer.child(self.more_popup(rect, cx));
            } else {
                layer = layer.child(self.property_popup(layout, anchor, selection_actions, cx));
            }
        }
        if let Some((flyout, anchor)) = flyout {
            layer = layer.child(self.flyout_panel(flyout, layout, anchor, has_properties, cx));
        }
        layer
    }

    pub(crate) fn capture_selection_bar_layout(
        &self,
        selected: Rect,
        window: &Window,
    ) -> Option<crate::capture_selection_bar::SelectionBarLayout> {
        if self.pin_mode || self.image_canvas.is_some() {
            return None;
        }
        let toolbar = self.toolbar_layout(selected, window.scale_factor());
        let mut avoid = vec![toolbar.primary];
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        if tool.draws() || matches!(tool, Tool::Lasso | Tool::Eraser | Tool::SmartMosaic) {
            avoid.push(toolbar.properties);
        }
        if let Some(ocr) = self.ocr_panel_frame(window) {
            avoid.push(ocr.panel);
        }
        let viewport = self.canvas_viewport(window);
        let chrome = crate::selection_chrome::SelectionChrome::projected(
            selected,
            f64::from(viewport.zoom),
            (f64::from(viewport.offset.x), f64::from(viewport.offset.y)),
        );
        Some(crate::capture_selection_bar::SelectionBarLayout::new(
            self.ui_monitor(self.toolbar_monitor),
            &chrome,
            window.scale_factor(),
            &avoid,
            self.selection_bar_widths(selected, window),
        ))
    }
}
