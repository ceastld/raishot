//! 长图剪辑的停靠界面，按 docs/design/long-editor-v2.html 排布：顶栏｜左侧工具栏、画布、缩略图与
//! 全长尺、属性栏｜状态栏。辅助线、标签和面板不参与像素导出；剪辑命令仍与原生标注共用撤销历史。
use crate::{
    annotation::{Point, Tool},
    capture_view::{Action, CaptureView},
    geometry::Rect,
    long_clip::LongClip,
    long_clip_navigation::{NavRect, NARROW_WIDTH},
    long_clip_ui::{ClipDrag, ClipEdge},
    scene::SourceIdentity,
    stitch::ScrollAxis,
    toolbar_groups::{Slot, ToolGroup},
    toolbar_icons::{self, Icon},
    toolbar_layout::{PanelRect, ToolbarLayout},
};
use gpui::{
    canvas, div, fill, img, point, prelude::*, px, rgb, rgba, size, AnyElement, Bounds,
    ContentMask, Context, Div, ElementId, FontWeight, MouseButton, ObjectFit, PathBuilder,
    RenderImage, SharedString, Stateful, Window,
};
use std::{ops::Range, sync::Arc, time::Duration};

/// 剪辑轨外缘到图片边的距离（含 6 DIP 间隙）；绘制与输入命中共用。
pub(crate) const GUTTER: f32 = 46.;
pub(crate) const GUTTER_GAP: f32 = 6.;
/// 属性栏内边距与标题高度；原生属性卡按同一位置停靠。
const SIDE_PAD: f32 = 14.;
const SIDE_HEADING: f32 = 37.;

/// 前端稿的颜色变量，深浅两套与原型 `:root` 一致。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClipTheme {
    pub(crate) panel: u32,
    pub(crate) well: u32,
    pub(crate) canvas: u32,
    pub(crate) gutter: u32,
    pub(crate) hover: u32,
    pub(crate) border: u32,
    pub(crate) fg: u32,
    pub(crate) muted: u32,
    pub(crate) faint: u32,
    pub(crate) accent: u32,
    pub(crate) accent_soft: u32,
    pub(crate) on_accent: u32,
    pub(crate) blue: u32,
    pub(crate) cut: u32,
    pub(crate) cut_soft: u32,
    /// 删除色与边框色 45% 混合，用于有删除时的芯片与数值框描边。
    pub(crate) cut_border: u32,
}

impl ClipTheme {
    pub(crate) fn new(light: bool) -> Self {
        if light {
            Self {
                panel: 0xffffff,
                well: 0xe9edf1,
                canvas: 0xdce2e7,
                gutter: 0xe3e8ed,
                hover: 0xedf1f5,
                border: 0xd8dfe6,
                fg: 0x293744,
                muted: 0x647381,
                faint: 0x84919d,
                accent: 0x177653,
                accent_soft: 0xe2f3ea,
                on_accent: 0xffffff,
                blue: 0x247fc4,
                cut: 0xd4424e,
                cut_soft: 0xfbe3e5,
                cut_border: 0xd698a2,
            }
        } else {
            Self {
                panel: 0x1e242c,
                well: 0x14181d,
                canvas: 0x101419,
                gutter: 0x191e25,
                hover: 0x2b333d,
                border: 0x303943,
                fg: 0xdce5ed,
                muted: 0x8b9aa9,
                faint: 0x647381,
                accent: 0x53d5a6,
                accent_soft: 0x203c35,
                on_accent: 0x10281f,
                blue: 0x81b7ee,
                cut: 0xf27c81,
                cut_soft: 0x3a2328,
                cut_border: 0x87575f,
            }
        }
    }
}

pub(crate) fn alpha(color: u32, a: f32) -> gpui::Rgba {
    rgba((color << 8) | (a.clamp(0., 1.) * 255.).round() as u32)
}

/// 千分位整数，与原型的 `toLocaleString("en-US")` 一致。
pub(crate) fn thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 接缝标签的短格式：1.5k、12k。
pub(crate) fn short(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", (n as f32 / 1000.).round())
    } else if n >= 1000 {
        let text = format!("{:.1}", n as f32 / 1000.);
        format!("{}k", text.trim_end_matches(".0"))
    } else {
        n.to_string()
    }
}

#[derive(Clone)]
struct PreviewThumb {
    image: Arc<RenderImage>,
    size: (f32, f32),
}

/// 每个剪辑宿主最多缓存一次悬停的首尾小块，不持有整图的第二份副本。
pub(crate) struct LongClipHoverPreview {
    identity: SourceIdentity,
    range: Range<u32>,
    axis: ScrollAxis,
    thumbs: Vec<PreviewThumb>,
    in_tag: bool,
    in_card: bool,
}

#[derive(Clone, Copy)]
enum ChromeAction {
    Delete,
    Keep,
    Cancel,
    Restore(usize),
    Toggle(usize),
    Locate(usize),
    RestoreAll,
    ToggleAll,
    Snap,
    /// 顶栏剪辑记录：切到剪辑工具并展开属性栏。
    Edits,
    Side,
    FitWidth,
    Zoom(f32),
    Return,
    Palette(Option<ToolGroup>),
    PickTool(Tool),
    Editor(Action),
}

/// 停靠面板：不透明底、吞掉画布手势，首击把焦点还给编辑器。
fn dock(id: &'static str, rect: NavRect, bg: u32) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width.max(0.)))
        .h(px(rect.height.max(0.)))
        .bg(rgb(bg))
        .cursor(gpui::CursorStyle::Arrow)
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            if let Some(Some(view)) = window.root::<CaptureView>() {
                view.update(cx, |view, _| window.focus(&view.focus));
            }
            cx.stop_propagation();
        })
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

/// 浮在画布上的卡片（选段操作条、接缝预览、工具面板）。
fn card(id: impl Into<ElementId>, t: ClipTheme) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(t.border))
        .bg(rgb(t.panel))
        .text_color(rgb(t.fg))
        .text_size(px(13.))
        .shadow_lg()
        .cursor(gpui::CursorStyle::Arrow)
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            if let Some(Some(view)) = window.root::<CaptureView>() {
                view.update(cx, |view, _| window.focus(&view.focus));
            }
            cx.stop_propagation();
        })
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

/// 可点击控件：外观由调用方决定，这里统一悬停、停用和点击分发。
fn clickable(
    id: usize,
    action: ChromeAction,
    enabled: bool,
    t: ClipTheme,
    cx: &mut Context<CaptureView>,
) -> Stateful<Div> {
    clickable_base(id, action, enabled, cx).when(enabled, |d| d.hover(move |s| s.bg(rgb(t.hover))))
}

/// 自定义悬停样式的控件共用事件处理，悬停样式由调用方设置一次。
fn clickable_base(
    id: usize,
    action: ChromeAction,
    enabled: bool,
    cx: &mut Context<CaptureView>,
) -> Stateful<Div> {
    div()
        .id(("long-clip-button", id))
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .when(enabled, |d| d.cursor_pointer())
        .when(!enabled, |d| d.opacity(0.35))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _, window, cx| {
                // Down 先结束参数输入；随后 Click 才复制、保存或执行其它剪辑动作。
                window.focus(&view.focus);
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |view, _, window, cx| {
            cx.stop_propagation();
            if enabled {
                view.long_clip_chrome_action(action, window, cx);
            }
        }))
}

/// 顶栏按钮：30 高、6 圆角；按下态为强调浅底。
fn top_button(
    id: usize,
    action: ChromeAction,
    enabled: bool,
    pressed: bool,
    t: ClipTheme,
    cx: &mut Context<CaptureView>,
) -> Stateful<Div> {
    top_button_style(clickable(id, action, enabled, t, cx), pressed, t)
}

fn top_button_style(button: Stateful<Div>, pressed: bool, t: ClipTheme) -> Stateful<Div> {
    button
        .h(px(30.))
        .px(px(10.))
        .gap(px(6.))
        .rounded(px(6.))
        .text_color(rgb(if pressed { t.accent } else { t.fg }))
        .when(pressed, |d| d.bg(rgb(t.accent_soft)))
}

fn icon(icon: Icon, color: u32, size: f32) -> gpui::Svg {
    toolbar_icons::action(icon, color).size(px(size))
}

fn vsep(t: ClipTheme) -> Div {
    div()
        .w(px(1.))
        .h(px(22.))
        .mx(px(4.))
        .flex_shrink_0()
        .bg(rgb(t.border))
}

fn kbd(text: &'static str, color: u32, border: gpui::Rgba) -> Div {
    div()
        .px(px(4.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(border)
        .text_size(px(10.5))
        .line_height(px(12.))
        .text_color(rgb(color))
        .child(text)
}

fn group_icon(group: ToolGroup) -> Icon {
    match group {
        ToolGroup::Shape => Icon::Rectangle,
        ToolGroup::Line => Icon::Arrow,
        ToolGroup::Mark => Icon::Markup,
        ToolGroup::Text => Icon::Text,
        ToolGroup::Mosaic => Icon::Mosaic,
        ToolGroup::Eraser => Icon::Eraser,
    }
}

fn group_label(group: ToolGroup) -> SharedString {
    match group {
        ToolGroup::Eraser => tr!("擦除", "Erase").into(),
        group => group.label().get().into(),
    }
}

fn source_point(axis: ScrollAxis, long: u32, cross: u32) -> Point {
    match axis {
        ScrollAxis::Vertical => Point {
            x: cross as f32,
            y: long as f32,
        },
        ScrollAxis::Horizontal => Point {
            x: long as f32,
            y: cross as f32,
        },
    }
}

fn hidden_annotations(view: &CaptureView, clip: &LongClip) -> usize {
    let cross = clip.cross_range();
    let kept: Vec<_> = clip
        .segments()
        .into_iter()
        .map(|segment| match clip.axis() {
            ScrollAxis::Vertical => Rect {
                x: cross.start,
                y: segment.source.start,
                width: cross.end - cross.start,
                height: segment.source.end - segment.source.start,
            },
            ScrollAxis::Horizontal => Rect {
                x: segment.source.start,
                y: cross.start,
                width: segment.source.end - segment.source.start,
                height: cross.end - cross.start,
            },
        })
        .collect();
    view.editor
        .document
        .items()
        .iter()
        .filter(|item| {
            let a = &item.annotation;
            if matches!(a.tool, Tool::Watermark | Tool::Spotlight) {
                return false;
            }
            let bounds = a.render_bounds();
            let padding = a.style.width.max(0.) / 2. + 1.;
            !kept.iter().any(|rect| {
                bounds.x - padding < (rect.x + rect.width) as f32
                    && bounds.y - padding < (rect.y + rect.height) as f32
                    && bounds.x + bounds.width + padding > rect.x as f32
                    && bounds.y + bounds.height + padding > rect.y as f32
            })
        })
        .count()
}

/// 四边裁掉的像素，顺序为上、右、下、左。
fn trims(clip: &LongClip) -> [u32; 4] {
    let (width, height) = clip.source_size();
    let (long, cross) = match clip.axis() {
        ScrollAxis::Vertical => (height, width),
        ScrollAxis::Horizontal => (width, height),
    };
    let first = clip
        .removed()
        .first()
        .filter(|r| r.start == 0)
        .map_or(0, |r| r.end);
    let last = clip
        .removed()
        .last()
        .filter(|r| r.end == long)
        .map_or(0, |r| long - r.start);
    let keep = clip.cross_range();
    match clip.axis() {
        ScrollAxis::Vertical => [first, cross - keep.end, last, keep.start],
        ScrollAxis::Horizontal => [keep.start, last, cross - keep.end, first],
    }
}

/// 画布叠加层的图元，坐标为窗口逻辑像素；按长轴／跨轴书写，绘制时换成 x／y。
#[derive(Clone, Copy)]
struct Frame {
    axis: ScrollAxis,
}

impl Frame {
    fn xy(self, long: f32, cross: f32) -> gpui::Point<gpui::Pixels> {
        match self.axis {
            ScrollAxis::Vertical => point(px(cross), px(long)),
            ScrollAxis::Horizontal => point(px(long), px(cross)),
        }
    }
    fn rect(self, l0: f32, l1: f32, c0: f32, c1: f32) -> Bounds<gpui::Pixels> {
        let a = self.xy(l0.min(l1), c0.min(c1));
        let (w, h) = match self.axis {
            ScrollAxis::Vertical => ((c1 - c0).abs(), (l1 - l0).abs()),
            ScrollAxis::Horizontal => ((l1 - l0).abs(), (c1 - c0).abs()),
        };
        Bounds::new(a, size(px(w), px(h)))
    }
    fn along(self, p: Point) -> (f32, f32) {
        match self.axis {
            ScrollAxis::Vertical => (p.y, p.x),
            ScrollAxis::Horizontal => (p.x, p.y),
        }
    }
}

enum Mark {
    Quad(Bounds<gpui::Pixels>, gpui::Rgba, f32),
    Line(Vec<gpui::Point<gpui::Pixels>>, gpui::Rgba, f32),
    Masked(Bounds<gpui::Pixels>, Vec<Mark>),
}

fn paint_marks(window: &mut Window, marks: &[Mark]) {
    for mark in marks {
        match mark {
            Mark::Quad(bounds, color, radius) => {
                window.paint_quad(fill(*bounds, *color).corner_radii(px(*radius)));
            }
            Mark::Line(points, color, width) => {
                let mut path = PathBuilder::stroke(px(*width));
                if let Some(first) = points.first() {
                    path.move_to(*first);
                    for p in &points[1..] {
                        path.line_to(*p);
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, *color);
                    }
                }
            }
            Mark::Masked(bounds, inner) => {
                window.with_content_mask(Some(ContentMask { bounds: *bounds }), |window| {
                    paint_marks(window, inner)
                });
            }
        }
    }
}

/// 虚线：前端稿用 6 实 5 空（起点线 5／4）。
#[allow(clippy::too_many_arguments)]
fn dashed(
    frame: Frame,
    l: f32,
    c0: f32,
    c1: f32,
    on: f32,
    off: f32,
    color: gpui::Rgba,
    width: f32,
) -> Vec<Mark> {
    let mut marks = Vec::new();
    let mut at = c0;
    while at < c1 {
        let end = (at + on).min(c1);
        marks.push(Mark::Line(
            vec![frame.xy(l, at), frame.xy(l, end)],
            color,
            width,
        ));
        at += on + off;
    }
    marks
}

impl CaptureView {
    fn long_clip_chrome_hover(
        &mut self,
        index: usize,
        hovered: bool,
        card: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.long_clip_ui.as_mut() {
            if hovered {
                ui.hover = Some(index);
            }
            if let Some(preview) = ui.hover_preview.as_mut() {
                if card {
                    preview.in_card = hovered;
                } else {
                    preview.in_tag = hovered;
                }
            }
        }
        if !hovered {
            // 留出从接缝标签移到预览卡的小间隙；卡片上的恢复与展开按钮可直接点击。
            cx.spawn_in(window, async move |view, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
                let _ = view.update(cx, |view, cx| {
                    if let Some(ui) = view.long_clip_ui.as_mut() {
                        let active = ui
                            .hover_preview
                            .as_ref()
                            .is_some_and(|p| p.in_card || p.in_tag);
                        if ui.hover == Some(index) && !active {
                            ui.hover = None;
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn long_clip_hover_thumbs(&mut self, clip: &LongClip, index: usize) -> Vec<PreviewThumb> {
        let Some(range) = clip.removed().get(index).cloned() else {
            return Vec::new();
        };
        let identity = SourceIdentity::of(&self.source_pixels());
        if let Some(cached) = self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.hover_preview.as_ref())
        {
            if cached.identity == identity && cached.range == range && cached.axis == clip.axis() {
                return cached.thumbs.clone();
            }
        }
        let cross = clip.cross_range();
        let cross_len = (cross.end - cross.start).min(768);
        let cross_start = cross.start + (cross.end - cross.start - cross_len) / 2;
        let edge = (range.end - range.start).min(96);
        let mut starts = vec![range.start];
        if range.end - range.start > edge {
            starts.push(range.end - edge);
        }
        let mut thumbs = Vec::with_capacity(starts.len());
        for start in starts {
            let rect = match clip.axis() {
                ScrollAxis::Vertical => Rect {
                    x: cross_start,
                    y: start,
                    width: cross_len,
                    height: edge,
                },
                ScrollAxis::Horizontal => Rect {
                    x: start,
                    y: cross_start,
                    width: edge,
                    height: cross_len,
                },
            };
            let Ok(image) = self.source.read_region(rect) else {
                continue;
            };
            let limit = match clip.axis() {
                ScrollAxis::Vertical => (256., 88.),
                ScrollAxis::Horizontal => (112., 168.),
            };
            let density = (limit.0 / image.width as f32)
                .min(limit.1 / image.height as f32)
                .min(1.);
            let width = (image.width as f32 * density).round().max(1.) as u32;
            let height = (image.height as f32 * density).round().max(1.) as u32;
            let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                let source_y =
                    ((u64::from(y) * u64::from(image.height)) / u64::from(height)) as usize;
                for x in 0..width {
                    let source_x =
                        ((u64::from(x) * u64::from(image.width)) / u64::from(width)) as usize;
                    let at = (source_y * image.width as usize + source_x) * 4;
                    bgra.extend_from_slice(&image.bgra[at..at + 4]);
                }
            }
            thumbs.push(PreviewThumb {
                image: crate::canvas_source::render_image(width, height, bgra),
                size: (width as f32, height as f32),
            });
        }
        if let Some(ui) = self.long_clip_ui.as_mut() {
            let old = ui.hover_preview.replace(LongClipHoverPreview {
                identity,
                range,
                axis: clip.axis(),
                thumbs: thumbs.clone(),
                in_tag: true,
                in_card: false,
            });
            if let Some(old) = old {
                self.retired_raster_previews
                    .extend(old.thumbs.into_iter().map(|t| t.image));
            }
        }
        thumbs
    }

    fn long_clip_chrome_action(
        &mut self,
        action: ChromeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.parameters_pending);
        if pending
            && matches!(
                action,
                ChromeAction::Delete
                    | ChromeAction::Keep
                    | ChromeAction::Restore(_)
                    | ChromeAction::RestoreAll
                    | ChromeAction::Return
            )
        {
            self.status = tr!(
                "请先修正裁边数值，或按 Esc 取消输入",
                "Fix the trim value, or press Esc to cancel"
            )
            .into();
            cx.notify();
            return;
        }
        match action {
            ChromeAction::Delete => self.long_clip_apply_selection(false, window, cx),
            ChromeAction::Keep => self.long_clip_apply_selection(true, window, cx),
            ChromeAction::Restore(index) => self.long_clip_restore(index, window, cx),
            ChromeAction::Toggle(index) => self.long_clip_toggle(index, window, cx),
            ChromeAction::Locate(index) => self.long_clip_locate(index, window, cx),
            ChromeAction::RestoreAll => self.long_clip_restore_all(window, cx),
            ChromeAction::ToggleAll => self.long_clip_toggle_all(window, cx),
            ChromeAction::FitWidth => self.long_clip_fit_width(window, cx),
            ChromeAction::Zoom(ratio) => self.perform(Action::ZoomImage(ratio), window, cx),
            ChromeAction::Return => self.long_clip_return(window, cx),
            ChromeAction::Editor(action) => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.tool_palette = None;
                }
                self.perform(action, window, cx);
            }
            ChromeAction::Cancel => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.selection = None;
                    ui.anchor = None;
                }
                cx.notify();
            }
            ChromeAction::Edits => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.panel = true;
                    ui.tool_palette = None;
                }
                if !pending {
                    self.perform(Action::Tool(Tool::Crop), window, cx);
                }
                cx.notify();
            }
            ChromeAction::Side => {
                let fitted = self.long_clip_is_fit_width(window);
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.panel = pending || !ui.panel;
                }
                if fitted {
                    self.long_clip_fit_width(window, cx);
                }
                cx.notify();
            }
            ChromeAction::Snap => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.snap = !ui.snap;
                }
                cx.notify();
            }
            ChromeAction::Palette(group) => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.tool_palette = (ui.tool_palette != Some(group)).then_some(group);
                }
                cx.notify();
            }
            ChromeAction::PickTool(tool) => {
                if let Some(ui) = self.long_clip_ui.as_mut() {
                    ui.tool_palette = None;
                }
                self.perform(Action::Tool(tool), window, cx);
            }
        }
    }

    fn long_clip_fit_zoom(&self, window: &Window) -> Option<f32> {
        let projection = self.long_clip_projection()?;
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let host = (
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        let area = self.long_clip_fit_area(host, dpi)?;
        Some(crate::long_clip_canvas::long_clip_fit_zoom(
            projection.axis(),
            projection.display_size(),
            area,
        ))
    }

    fn long_clip_is_fit_width(&self, window: &Window) -> bool {
        self.long_clip_fit_zoom(window)
            .is_some_and(|zoom| (self.canvas_viewport(window).zoom - zoom).abs() <= zoom * 0.002)
    }

    /// 原生属性卡停靠在属性栏标题之下；弹层按卡片向下或侧旁展开。
    pub(crate) fn long_clip_property_dock(&self, window: &Window) -> Option<ToolbarLayout> {
        self.long_clip_ui.as_ref()?;
        let layout = self.long_clip_layout(window)?;
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        let count = self.editor.selected_ids().len();
        let width = (layout.side.width - SIDE_PAD * 2.).max(0.);
        let (_, height, level) = crate::toolbar_layout::property_size(
            if count > 1 { Tool::Rectangle } else { tool },
            count,
            width,
        );
        let size = window.viewport_size();
        let monitor = PanelRect {
            x: 0.,
            y: 0.,
            width: f32::from(size.width),
            height: f32::from(size.height),
        };
        let card = PanelRect {
            x: layout.side.x + SIDE_PAD,
            y: layout.side.y + SIDE_PAD + SIDE_HEADING,
            width,
            height,
        };
        Some(ToolbarLayout::docked(monitor, card, tool, level))
    }

    pub(crate) fn long_clip_chrome(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(ui) = self.long_clip_ui.as_ref() else {
            return div().into_any_element();
        };
        let Some(clip) = ui
            .draft
            .as_ref()
            .or(self.editor.document.long_clip())
            .cloned()
        else {
            return div().into_any_element();
        };
        let selection = ui.selection.clone();
        let anchor = ui.anchor;
        let expanded = ui.expanded.clone();
        let snap = ui.snap;
        let hover = ui.hover;
        let tool_palette = ui.tool_palette;
        let trim_edge = match &ui.drag {
            Some(ClipDrag::Trim { edge, moved, .. }) if *moved => Some(*edge),
            _ => None,
        };
        let band_drag = matches!(
            ui.drag,
            Some(
                ClipDrag::Select { moved: true, .. } | ClipDrag::SelectionEdge { moved: true, .. }
            )
        );
        let adjusting = matches!(ui.drag, Some(ClipDrag::Adjust { moved: true, .. }));
        let panning = matches!(ui.drag, Some(ClipDrag::Pan { .. }));
        let (Some(projection), Some(layout)) =
            (self.long_clip_projection(), self.long_clip_layout(window))
        else {
            return div().into_any_element();
        };
        let t = ClipTheme::new(self.light_theme);
        let dpi = window.scale_factor();
        let host = window.viewport_size();
        let host_w = f32::from(host.width);
        let host_h = f32::from(host.height);
        let axis = clip.axis();
        let vertical = axis == ScrollAxis::Vertical;
        let frame = Frame { axis };
        let cross = clip.cross_range();
        let viewport = self.canvas_viewport(window);
        let zoom = viewport.zoom / dpi;
        let (source_long, source_cross) = {
            let (w, h) = clip.source_size();
            frame.along(Point {
                x: w as f32,
                y: h as f32,
            })
        };
        let source_long = source_long as u32;
        let display_long = frame
            .along(Point {
                x: projection.display_size().0 as f32,
                y: projection.display_size().1 as f32,
            })
            .0;
        let (origin_long, origin_cross) = frame.along(viewport.offset);
        // 与前端稿同名：x0 为图片跨轴起点，w 为跨轴宽，top／bot 为长轴首尾（逻辑像素）。
        let x0 = origin_cross / dpi;
        let w = source_cross * zoom;
        let top = origin_long / dpi;
        let bot = top + display_long * zoom;
        let main = layout.main;
        let (m_l0, m_l1, m_c0, m_c1) = if vertical {
            (main.y, main.y + main.height, main.x, main.x + main.width)
        } else {
            (main.x, main.x + main.width, main.y, main.y + main.height)
        };
        let long_of = |value: u32| {
            frame
                .along(self.clip_view_point(source_point(axis, value, 0), window))
                .0
                / dpi
        };
        let v_top = top.max(m_l0 - 40.);
        let v_bot = bot.min(m_l1 + 40.);
        let mut marks = Vec::<Mark>::new();
        let mut tick_labels = Vec::<(f32, String)>::new();

        // 剪辑轨与刻度。
        if v_bot > v_top {
            marks.push(Mark::Quad(
                frame.rect(v_top, v_bot, x0 - GUTTER, x0 - GUTTER_GAP),
                if band_drag || trim_edge.is_some() {
                    alpha(t.cut, 0.08)
                } else {
                    rgb(t.gutter)
                },
                0.,
            ));
            let step = [100_u32, 200, 500, 1000, 2000, 5000, 10000]
                .into_iter()
                .find(|s| *s as f32 * zoom >= 60.)
                .unwrap_or(10000);
            let minor = step / 5;
            let d_a = ((m_l0 - top) / zoom).max(0.);
            let d_b = ((m_l1 - top) / zoom).min(display_long);
            let mut d = ((d_a / minor as f32).ceil() as u32) * minor;
            while (d as f32) <= d_b {
                let y = (top + d as f32 * zoom).round() + 0.5;
                let major = d % step == 0;
                marks.push(Mark::Quad(
                    frame.rect(
                        y - 0.5,
                        y + 0.5,
                        x0 - if major { 16. } else { 11. },
                        x0 - 8.,
                    ),
                    alpha(t.fg, if major { 0.28 } else { 0.12 }),
                    0.,
                ));
                if major && d > 0 && vertical {
                    tick_labels.push((y, short(d)));
                }
                d += minor;
            }
        }

        // 原位展开的删除段：压暗、红色斜纹，轨上红条，首尾红线。
        for piece in projection.pieces().iter().filter(|piece| piece.removed) {
            let (start, _) = frame.along(Point {
                x: piece.display.x as f32,
                y: piece.display.y as f32,
            });
            let (length, _) = frame.along(Point {
                x: piece.display.width as f32,
                y: piece.display.height as f32,
            });
            let (source_start, _) = frame.along(Point {
                x: piece.source.x as f32,
                y: piece.source.y as f32,
            });
            let ys = top + start * zoom;
            let ye = ys + length * zoom;
            if ye < m_l0 - 4. || ys > m_l1 + 4. {
                continue;
            }
            let bounds = frame.rect(ys, ye, x0, x0 + w);
            marks.push(Mark::Quad(bounds, rgba(0x080a0d99), 0.));
            let (left, right) = (f32::from(bounds.left()), f32::from(bounds.right()));
            let (upper, lower) = (f32::from(bounds.top()), f32::from(bounds.bottom()));
            let mut hatch = Vec::new();
            let mut c = ((left + upper) / 12.).floor() * 12.;
            while c <= right + lower {
                hatch.push(Mark::Line(
                    vec![
                        point(px(c - lower), px(lower)),
                        point(px(c - upper), px(upper)),
                    ],
                    alpha(t.cut, 0.5),
                    1.5,
                ));
                c += 12.;
            }
            marks.push(Mark::Masked(bounds, hatch));
            marks.push(Mark::Quad(
                frame.rect(ys, ye, x0 - 12., x0 - 8.),
                alpha(t.cut, 0.4),
                0.,
            ));
            if source_start > 0. {
                marks.push(Mark::Quad(
                    frame.rect(ys - 1., ys + 1., x0 - GUTTER + 2., x0 + w),
                    rgb(t.cut),
                    0.,
                ));
            }
            if (source_start + length) < source_long as f32 {
                marks.push(Mark::Quad(
                    frame.rect(ye - 1., ye + 1., x0 - GUTTER + 2., x0 + w),
                    rgb(t.cut),
                    0.,
                ));
            }
        }

        // 左右裁剪保留压暗区，可直接拖回。
        let lx = x0 + cross.start as f32 * zoom;
        let rx = x0 + cross.end as f32 * zoom;
        let cross_drag = matches!(trim_edge, Some(ClipEdge::CrossStart | ClipEdge::CrossEnd));
        if cross.start > 0 {
            marks.push(Mark::Quad(
                frame.rect(v_top, v_bot, x0, lx),
                rgba(0x060809a8),
                0.,
            ));
        }
        if (cross.end as f32) < source_cross {
            marks.push(Mark::Quad(
                frame.rect(v_top, v_bot, rx, x0 + w),
                rgba(0x060809a8),
                0.,
            ));
        }
        for (edge, at, trimmed) in [
            (ClipEdge::CrossStart, lx, cross.start > 0),
            (ClipEdge::CrossEnd, rx, (cross.end as f32) < source_cross),
        ] {
            if trimmed || trim_edge == Some(edge) {
                marks.push(Mark::Quad(
                    frame.rect(v_top, v_bot, at - 0.5, at + 0.5),
                    if cross_drag {
                        rgb(t.cut)
                    } else {
                        alpha(t.fg, 0.75)
                    },
                    0.,
                ));
            }
        }

        // 收起的删除段：内部为锯齿接缝，首尾裁剪为图片边缘红色虚线。
        for seam in projection.seams() {
            let y = top + seam.display_at as f32 * zoom;
            if y < m_l0 - 10. || y > m_l1 + 10. {
                continue;
            }
            if seam.source.start == 0 || seam.source.end == source_long {
                marks.extend(dashed(frame, y, x0, x0 + w, 6., 5., rgb(t.cut), 2.));
            } else {
                let mut points = vec![frame.xy(y, x0)];
                let mut up = true;
                let mut x = x0 + 5.;
                while x < x0 + w {
                    points.push(frame.xy(y + if up { -3. } else { 3. }, x));
                    up = !up;
                    x += 9.;
                }
                points.push(frame.xy(y, x0 + w));
                marks.push(Mark::Line(points.clone(), rgba(0x0000008c), 4.));
                marks.push(Mark::Line(points, rgb(t.cut), 1.6));
                marks.push(Mark::Quad(
                    frame.rect(y - 1., y + 1., x0 - 12., x0),
                    rgb(t.cut),
                    0.,
                ));
            }
        }

        // 选段：红色半透明带、轨上实带、两条红线与右侧拖柄。
        let band = selection
            .as_ref()
            .map(|range| (long_of(range.start), long_of(range.end)));
        if let Some((ya, yb)) = band {
            marks.push(Mark::Quad(
                frame.rect(ya, yb, x0, x0 + w),
                alpha(t.cut, 0.22),
                0.,
            ));
            marks.push(Mark::Quad(
                frame.rect(ya, yb, x0 - GUTTER + 2., x0 - 8.),
                alpha(t.cut, 0.55),
                0.,
            ));
            for y in [ya, yb] {
                marks.push(Mark::Quad(
                    frame.rect(y - 1., y + 1., x0 - GUTTER + 2., x0 + w),
                    rgb(t.cut),
                    0.,
                ));
                marks.push(Mark::Quad(
                    frame.rect(y - 8., y + 8., x0 + w + 3., x0 + w + 10.),
                    rgb(t.cut),
                    3.5,
                ));
            }
        } else if let Some(at) = anchor {
            let y = long_of(at).round() + 0.5;
            marks.extend(dashed(
                frame,
                y,
                x0 - GUTTER + 2.,
                x0 + w,
                5.,
                4.,
                rgb(t.blue),
                1.,
            ));
        }

        // 图片外框与四边把手。
        let edge_line = alpha(t.fg, 0.16);
        marks.push(Mark::Quad(
            frame.rect(top - 1., top, x0 - 1., x0 + w + 1.),
            edge_line,
            0.,
        ));
        marks.push(Mark::Quad(
            frame.rect(bot, bot + 1., x0 - 1., x0 + w + 1.),
            edge_line,
            0.,
        ));
        marks.push(Mark::Quad(frame.rect(top, bot, x0 - 1., x0), edge_line, 0.));
        marks.push(Mark::Quad(
            frame.rect(top, bot, x0 + w, x0 + w + 1.),
            edge_line,
            0.,
        ));
        let long_start = clip
            .removed()
            .first()
            .filter(|range| range.start == 0)
            .map_or(0, |range| range.end);
        let long_end = clip
            .removed()
            .last()
            .filter(|range| range.end == source_long)
            .map_or(source_long, |range| range.start);
        let ty = long_of(long_start);
        let by = long_of(long_end);
        let handle = |edge: ClipEdge| {
            if trim_edge == Some(edge) {
                rgb(t.cut)
            } else {
                alpha(t.fg, 0.5)
            }
        };
        let center = (lx + rx) / 2.;
        if ty > m_l0 - 20. {
            marks.push(Mark::Quad(
                frame.rect(ty - 13., ty - 7., center - 26., center + 26.),
                handle(ClipEdge::LongStart),
                3.,
            ));
        }
        if by < m_l1 + 20. {
            marks.push(Mark::Quad(
                frame.rect(by + 7., by + 13., center - 26., center + 26.),
                handle(ClipEdge::LongEnd),
                3.,
            ));
        }
        let mid = ((m_l0 + m_l1) / 2.).clamp(
            top.max(m_l0) + 34.,
            (bot.min(m_l1) - 34.).max(top.max(m_l0) + 34.),
        );
        marks.push(Mark::Quad(
            frame.rect(mid - 26., mid + 26., lx - 3., lx + 3.),
            handle(ClipEdge::CrossStart),
            3.,
        ));
        marks.push(Mark::Quad(
            frame.rect(mid - 26., mid + 26., rx - 3., rx + 3.),
            handle(ClipEdge::CrossEnd),
            3.,
        ));

        let main_bounds = Bounds::new(
            point(px(main.x), px(main.y)),
            size(px(main.width), px(main.height)),
        );
        let marks = vec![Mark::Masked(main_bounds, marks)];
        let mut root = div().absolute().top_0().left_0().size_full().child(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| paint_marks(window, &marks),
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        // 剪辑轨的命中位置供原生验收读取；视觉在上面的叠加层里。
        if bot > top {
            let rail = frame.rect(top.max(m_l0), bot.min(m_l1), x0 - GUTTER, x0 - GUTTER_GAP);
            root = root.child(
                div()
                    .absolute()
                    .left(rail.origin.x)
                    .top(rail.origin.y)
                    .w(rail.size.width)
                    .h(rail.size.height)
                    .child(self.probe("long-clip-rail")),
            );
        }
        let in_main = |l: f32| l >= m_l0 && l <= m_l1;
        for (y, label) in tick_labels {
            if y - 7. < m_l0 || y + 7. > m_l1 {
                continue;
            }
            root = root.child(
                div()
                    .absolute()
                    .left(px(x0 - GUTTER + 3.))
                    .top(px(y - 7.))
                    .text_size(px(10.))
                    .line_height(px(14.))
                    .text_color(alpha(t.fg, 0.32))
                    .child(label),
            );
        }

        // 接缝标签贴在剪辑轨上，右缘对齐图片左侧 8 DIP；纵向以右缘定位免去测量宽度。
        let chip_x = x0 - 8.;
        let place = |el: Stateful<Div>, l: f32, c_right: f32, height: f32| {
            if vertical {
                el.right(px(host_w - c_right)).top(px(l - height / 2.))
            } else {
                el.left(px(l - 24.)).bottom(px(host_h - c_right))
            }
        };
        let seams = projection.seams().to_vec();
        for (index, range) in clip.removed().iter().enumerate() {
            let length = range.end - range.start;
            let open = expanded.contains(range);
            let (ys, ye) = if open {
                (long_of(range.start), long_of(range.end))
            } else {
                let Some(seam) = seams.iter().find(|seam| seam.source == *range) else {
                    continue;
                };
                let y = top + seam.display_at as f32 * zoom;
                (y, y)
            };
            if ye < m_l0 - 12. || ys > m_l1 + 12. {
                continue;
            }
            let head = range.start == 0;
            let tail = range.end == source_long;
            let chip_y = if open {
                ys.clamp(m_l0 + 12., (ye - 12.).max(m_l0 + 12.))
            } else if head {
                ys.max(m_l0 + 12.)
            } else {
                ys
            };
            if !in_main(chip_y) {
                continue;
            }
            let fg = if open { 0xffffff } else { t.cut };
            let lead: AnyElement = if head || tail {
                div()
                    .text_size(px(11.))
                    .child(if head { "⤒" } else { "⤓" })
                    .into_any_element()
            } else {
                icon(Icon::Cut, fg, 13.).into_any_element()
            };
            let chip = div()
                .id(("long-clip-seam-tag", index))
                .absolute()
                .h(px(20.))
                .pl(px(5.))
                .pr(px(7.))
                .flex()
                .items_center()
                .gap(px(3.))
                .rounded(px(10.))
                .border_1()
                .border_color(alpha(t.cut, 0.55))
                .bg(rgb(if open { t.cut } else { t.cut_soft }))
                .text_color(rgb(fg))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .shadow_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(t.cut)).text_color(rgb(0xffffff)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_hover(cx.listener(move |view, hovered: &bool, window, cx| {
                    if !open {
                        view.long_clip_chrome_hover(index, *hovered, false, window, cx);
                    }
                }))
                .on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.long_clip_chrome_action(ChromeAction::Toggle(index), window, cx);
                }))
                .child(lead)
                .child(short(length));
            root = root.child(place(chip, chip_y, chip_x, 20.));
            if open {
                let kind = match (head, tail, vertical) {
                    (true, _, true) => tr!("顶部已裁", "Top trimmed"),
                    (_, true, true) => tr!("底部已裁", "Bottom trimmed"),
                    (true, _, false) => tr!("左侧已裁", "Left trimmed"),
                    (_, true, false) => tr!("右侧已裁", "Right trimmed"),
                    _ => tr!("已删除", "Removed"),
                };
                let bar_l = (ys + 8.).clamp(m_l0 + 8., (ye - 44.).max(m_l0 + 8.));
                let mut bar = card(("long-clip-expanded", index), t)
                    .p(px(3.))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(
                        div()
                            .pl(px(6.))
                            .pr(px(8.))
                            .flex()
                            .gap(px(4.))
                            .text_color(rgb(t.muted))
                            .child(kind)
                            .child(
                                div()
                                    .text_color(rgb(t.cut))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(thousands(length)),
                            )
                            .child(tr!("px · 拖动红线调整", "px · drag the red lines")),
                    )
                    .child(
                        clickable(200 + index * 2, ChromeAction::Restore(index), true, t, cx)
                            .h(px(28.))
                            .px(px(9.))
                            .rounded(px(6.))
                            .child(tr!("恢复", "Restore")),
                    );
                let all_open = clip.removed().iter().all(|r| expanded.contains(r));
                if !all_open {
                    bar = bar.child(
                        clickable(201 + index * 2, ChromeAction::Toggle(index), true, t, cx)
                            .h(px(28.))
                            .px(px(9.))
                            .rounded(px(6.))
                            .child(tr!("收起", "Collapse")),
                    );
                }
                let bar = if vertical {
                    bar.left(px(x0 + 10.)).top(px(bar_l))
                } else {
                    bar.left(px(bar_l)).top(px(x0 + 10.))
                };
                root = root.child(bar);
            }
        }

        if let Some((ya, yb)) = band {
            let range = selection.clone().unwrap_or(0..0);
            let removed: u32 = clip
                .removed()
                .iter()
                .map(|deleted| {
                    deleted
                        .end
                        .min(range.end)
                        .saturating_sub(deleted.start.max(range.start))
                })
                .sum();
            let actual = (range.end - range.start).saturating_sub(removed);
            let label_l = ((ya + yb) / 2.).clamp(m_l0 + 12., m_l1 - 12.);
            let label = div()
                .id("long-clip-band-label")
                .absolute()
                .px(px(7.))
                .py(px(2.))
                .rounded(px(4.))
                .bg(rgb(t.cut))
                .text_color(rgb(0xffffff))
                .text_size(px(11.))
                .line_height(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .shadow_sm()
                .child(thousands(actual));
            root = root.child(place(label, label_l, chip_x, 18.));
            if !band_drag {
                let verb = if range.start <= long_start && range.end >= long_end {
                    tr!("删除", "Delete")
                } else if range.start <= long_start {
                    tr!("裁掉开头", "Trim start")
                } else if range.end >= long_end {
                    tr!("裁掉结尾", "Trim end")
                } else {
                    tr!("删除", "Delete")
                };
                let by_ = if yb - ya >= 56. {
                    (ya + 8.).clamp(m_l0 + 8., (yb - 46.).max(m_l0 + 8.))
                } else if yb + 44. < m_l1 {
                    yb + 8.
                } else {
                    (ya - 46.).max(m_l0 + 8.)
                }
                .clamp(m_l0 + 8., (m_l1 - 46.).max(m_l0 + 8.));
                let border = alpha(0xffffff, 0.4);
                let bar = card("long-clip-selection", t)
                    .p(px(3.))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(
                        clickable_base(20, ChromeAction::Delete, true, cx)
                            .h(px(28.))
                            .px(px(9.))
                            .gap(px(6.))
                            .rounded(px(6.))
                            .bg(rgb(t.cut))
                            .hover(move |s| s.bg(rgb(t.cut)))
                            .text_color(rgb(0xffffff))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(icon(Icon::Trash, 0xffffff, 18.))
                            .child(verb)
                            .child(kbd("Del", 0xffffff, border)),
                    )
                    .child(
                        clickable(21, ChromeAction::Keep, true, t, cx)
                            .h(px(28.))
                            .px(px(9.))
                            .gap(px(6.))
                            .rounded(px(6.))
                            .child(tr!("仅保留", "Keep only"))
                            .child(kbd("K", t.faint, rgb(t.border))),
                    )
                    .child(
                        clickable(22, ChromeAction::Cancel, true, t, cx)
                            .h(px(28.))
                            .px(px(9.))
                            .rounded(px(6.))
                            .child(tr!("取消", "Cancel")),
                    );
                let bar = if vertical {
                    bar.right(px(host_w - (x0 + w - 10.).min(m_c1 - 10.)))
                        .top(px(by_))
                } else {
                    bar.left(px(by_)).top(px((x0 + 10.).max(m_c0 + 8.)))
                };
                root = root.child(bar);
            }
        } else if let Some(at) = anchor {
            let l = long_of(at);
            if in_main(l) {
                let label = div()
                    .id("long-clip-anchor-label")
                    .absolute()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .bg(rgb(t.blue))
                    .text_color(rgb(0x0d1b2a))
                    .text_size(px(11.))
                    .line_height(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("起点", "Start"));
                root = root.child(place(label, l, chip_x, 16.));
            }
        }

        if let Some(edge) = trim_edge {
            let [trim_top, trim_right, trim_bottom, trim_left] = trims(&clip);
            let (text, l, c) = match (edge, vertical) {
                (ClipEdge::LongStart, true) => (
                    trf!("裁掉顶部 {} px", "Trim top {} px", thousands(trim_top)),
                    ty + 10.,
                    x0 + w / 2. + 90.,
                ),
                (ClipEdge::LongEnd, true) => (
                    trf!(
                        "裁掉底部 {} px",
                        "Trim bottom {} px",
                        thousands(trim_bottom)
                    ),
                    by - 38.,
                    x0 + w / 2. + 90.,
                ),
                (ClipEdge::CrossStart, true) => (
                    trf!("裁掉左侧 {} px", "Trim left {} px", thousands(trim_left)),
                    (m_l0 + m_l1) / 2. + 40.,
                    lx + 20.,
                ),
                (ClipEdge::CrossEnd, true) => (
                    trf!("裁掉右侧 {} px", "Trim right {} px", thousands(trim_right)),
                    (m_l0 + m_l1) / 2. + 40.,
                    rx - 140.,
                ),
                (ClipEdge::LongStart, false) => (
                    trf!("裁掉左侧 {} px", "Trim left {} px", thousands(trim_left)),
                    ty + 10.,
                    x0 + w / 2.,
                ),
                (ClipEdge::LongEnd, false) => (
                    trf!("裁掉右侧 {} px", "Trim right {} px", thousands(trim_right)),
                    by - 150.,
                    x0 + w / 2.,
                ),
                (ClipEdge::CrossStart, false) => (
                    trf!("裁掉顶部 {} px", "Trim top {} px", thousands(trim_top)),
                    (m_l0 + m_l1) / 2.,
                    lx + 12.,
                ),
                (ClipEdge::CrossEnd, false) => (
                    trf!(
                        "裁掉底部 {} px",
                        "Trim bottom {} px",
                        thousands(trim_bottom)
                    ),
                    (m_l0 + m_l1) / 2.,
                    rx - 40.,
                ),
            };
            let l = l.clamp(m_l0 + 8., (m_l1 - 36.).max(m_l0 + 8.));
            let label = div()
                .absolute()
                .px(px(9.))
                .py(px(3.))
                .rounded(px(5.))
                .bg(rgb(t.cut))
                .text_color(rgb(0xffffff))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .shadow_md()
                .child(text);
            let pos = frame.xy(l, c);
            root = root.child(label.left(pos.x).top(pos.y));
        }

        if let Some(index) = hover.filter(|index| *index < clip.removed().len()) {
            root = root.child(self.long_clip_peek(
                &clip,
                index,
                &expanded,
                t,
                x0,
                cx,
                |range| {
                    let seam = seams.iter().find(|seam| seam.source == *range);
                    seam.map(|seam| top + seam.display_at as f32 * zoom)
                },
                (m_l0, m_l1),
                vertical,
            ));
        }

        // 停靠面板：顶栏、左侧工具栏、属性栏、状态栏；缩略图与全长尺由导航层绘制。
        root = root
            .child(self.long_clip_top_bar(&clip, &expanded, &layout.top, t, window, cx))
            .child(self.long_clip_rail(&layout.rail, tool_palette, t, cx));
        if layout.side.width > 0. {
            root = root.child(self.long_clip_side(
                &clip,
                snap,
                &expanded,
                &layout.side,
                t,
                window,
                cx,
            ));
        }
        root = root.child(self.long_clip_status(
            &clip,
            &layout.status,
            selection.is_some(),
            band_drag || adjusting,
            trim_edge.is_some(),
            panning,
            t,
            window,
        ));
        if let Some(group) = tool_palette {
            root = root.child(self.long_clip_tool_palette(group, &main, t, cx));
        }
        root.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn long_clip_peek(
        &mut self,
        clip: &LongClip,
        index: usize,
        expanded: &[Range<u32>],
        t: ClipTheme,
        x0: f32,
        cx: &mut Context<Self>,
        seam_at: impl Fn(&Range<u32>) -> Option<f32>,
        main: (f32, f32),
        vertical: bool,
    ) -> AnyElement {
        let range = clip.removed()[index].clone();
        let Some(at) = seam_at(&range) else {
            return div().into_any_element();
        };
        let thumbs = self.long_clip_hover_thumbs(clip, index);
        let long = clip.source_size();
        let source_long = if vertical { long.1 } else { long.0 };
        let title = if range.start == 0 {
            tr!("开头已裁", "Start trimmed")
        } else if range.end == source_long {
            tr!("结尾已裁", "End trimmed")
        } else {
            tr!("已删除", "Removed")
        };
        let mut peek = card("long-clip-hover-preview", t)
            .p(px(8.))
            .rounded(px(10.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .on_hover(cx.listener(move |view, hovered: &bool, window, cx| {
                view.long_clip_chrome_hover(index, *hovered, true, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap(px(12.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .text_color(rgb(t.cut))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(trf!(
                                "{} {} px",
                                "{} {} px",
                                title,
                                thousands(range.end - range.start)
                            )),
                    )
                    .child(div().text_color(rgb(t.faint)).child(trf!(
                        "原图 {} – {}",
                        "Source {} – {}",
                        thousands(range.start),
                        thousands(range.end)
                    ))),
            );
        let mut images = div().flex().gap(px(4.)).when(vertical, |d| d.flex_col());
        for thumb in thumbs {
            images = images.child(
                img(thumb.image)
                    .w(px(thumb.size.0))
                    .h(px(thumb.size.1))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .object_fit(ObjectFit::Fill),
            );
        }
        let open = expanded.contains(&range);
        peek = peek.child(images).child(
            div()
                .flex()
                .gap(px(6.))
                .child(
                    clickable(30, ChromeAction::Toggle(index), true, t, cx)
                        .flex_1()
                        .h(px(28.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(if open {
                            tr!("收起", "Collapse")
                        } else {
                            tr!("原位展开调整", "Expand in place")
                        }),
                )
                .child(
                    clickable(31, ChromeAction::Restore(index), true, t, cx)
                        .flex_1()
                        .h(px(28.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(tr!("恢复", "Restore")),
                ),
        );
        let (l0, l1) = main;
        if vertical {
            peek.left(px(x0 + 12.))
                .top(px((at + 14.).clamp(l0 + 8., (l1 - 260.).max(l0 + 8.))))
                .into_any_element()
        } else {
            peek.left(px((at + 14.).clamp(l0 + 8., (l1 - 300.).max(l0 + 8.))))
                .top(px(x0 + 12.))
                .into_any_element()
        }
    }

    fn long_clip_top_bar(
        &self,
        clip: &LongClip,
        expanded: &[Range<u32>],
        rect: &NavRect,
        t: ClipTheme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let narrow = rect.width < 960.;
        let tiny = rect.width < NARROW_WIDTH;
        let (out_w, out_h) = clip.output_size();
        let (src_w, src_h) = clip.source_size();
        let over = clip.exceeds_output_limit();
        let long_len = match clip.axis() {
            ScrollAxis::Vertical => src_h,
            ScrollAxis::Horizontal => src_w,
        };
        let inner = clip
            .removed()
            .iter()
            .filter(|range| range.start > 0 && range.end < long_len)
            .count();
        let sides = trims(clip).iter().filter(|value| **value > 0).count();
        let parts: Vec<String> = [
            (inner > 0).then(|| trf!("删 {} 段", "{} cuts", inner)),
            (sides > 0).then(|| trf!("裁 {} 边", "{} edges", sides)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let edited = !parts.is_empty();
        let edits = if edited {
            parts.join(" · ")
        } else {
            tr!("未剪辑", "No cuts").to_owned()
        };
        let all_open =
            !clip.removed().is_empty() && clip.removed().iter().all(|r| expanded.contains(r));
        let fitted = self.long_clip_is_fit_width(window);
        let zoom = (self.canvas_viewport(window).zoom / window.scale_factor() * 100.).round();
        let can_undo = self.editor.document.can_undo();
        let can_redo = self.editor.document.can_redo();
        let side = self.long_clip_side_open();
        let src_label = if (out_w, out_h) != (src_w, src_h) {
            trf!(
                "原图 {} × {}",
                "Source {} × {}",
                thousands(src_w),
                thousands(src_h)
            )
        } else {
            tr!("原图", "Source").to_owned()
        };
        dock("long-clip-top", *rect, t.panel)
            .border_b_1()
            .border_color(rgb(t.border))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .text_size(px(13.))
            .text_color(rgb(t.fg))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(8.))
                    .mr(px(4.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(if over { t.cut } else { t.fg }))
                            .child(format!("{} × {}", thousands(out_w), thousands(out_h)))
                            .child(self.probe("long-clip-summary")),
                    )
                    .when(!narrow, |d| {
                        d.child(
                            div()
                                .pb(px(1.))
                                .text_size(px(12.))
                                .text_color(rgb(t.muted))
                                .child(src_label),
                        )
                    }),
            )
            .child(
                clickable(1, ChromeAction::Edits, true, t, cx)
                    .h(px(28.))
                    .px(px(10.))
                    .gap(px(6.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(if edited { t.cut_border } else { t.border }))
                    .text_color(rgb(if edited { t.cut } else { t.fg }))
                    .child(icon(Icon::History, if edited { t.cut } else { t.fg }, 18.))
                    .when(!tiny, |d| d.child(edits)),
            )
            .child(vsep(t))
            .child(
                top_button(
                    2,
                    ChromeAction::Editor(Action::Undo),
                    can_undo,
                    false,
                    t,
                    cx,
                )
                .w(px(30.))
                .px(px(0.))
                .child(icon(Icon::Undo, t.fg, 18.))
                .child(self.probe("long-clip-undo")),
            )
            .child(
                top_button(
                    3,
                    ChromeAction::Editor(Action::Redo),
                    can_redo,
                    false,
                    t,
                    cx,
                )
                .w(px(30.))
                .px(px(0.))
                .child(icon(Icon::Redo, t.fg, 18.))
                .child(self.probe("long-clip-redo")),
            )
            .child(
                top_button(
                    4,
                    ChromeAction::ToggleAll,
                    !clip.removed().is_empty(),
                    all_open,
                    t,
                    cx,
                )
                .child(icon(
                    if all_open { Icon::EyeOff } else { Icon::Eye },
                    if all_open { t.accent } else { t.fg },
                    18.,
                ))
                .when(!narrow, |d| d.child(tr!("显示已删除", "Show removed"))),
            )
            .child(div().flex_1())
            .child(
                top_button(5, ChromeAction::Zoom(1. / 1.1), true, false, t, cx)
                    .w(px(30.))
                    .px(px(0.))
                    .child("−"),
            )
            .when(!tiny, |d| {
                d.child(
                    div()
                        .min_w(px(46.))
                        .flex()
                        .justify_center()
                        .text_color(rgb(t.muted))
                        .child(format!("{zoom}%")),
                )
            })
            .child(
                top_button(6, ChromeAction::Zoom(1.1), true, false, t, cx)
                    .w(px(30.))
                    .px(px(0.))
                    .child("+"),
            )
            .child(
                top_button(7, ChromeAction::FitWidth, true, fitted, t, cx)
                    .child(tr!("适宽", "Fit"))
                    .child(self.probe("long-clip-fit-width")),
            )
            .child(
                top_button(
                    8,
                    ChromeAction::Editor(Action::ActualImage),
                    true,
                    false,
                    t,
                    cx,
                )
                .child("1:1")
                .child(self.probe("long-clip-actual")),
            )
            .when(!tiny, |d| {
                d.child(
                    top_button(9, ChromeAction::Side, true, side, t, cx)
                        .child(icon(Icon::Sliders, if side { t.accent } else { t.fg }, 18.))
                        .when(!narrow, |d| d.child(tr!("属性", "Properties"))),
                )
            })
            .child(vsep(t))
            .child(
                top_button(10, ChromeAction::Return, true, false, t, cx)
                    .child(icon(Icon::LongCapture, t.fg, 18.))
                    .when(!narrow, |d| d.child(tr!("继续采集", "Continue")))
                    .child(self.probe("long-clip-return")),
            )
            .child(
                top_button(11, ChromeAction::Editor(Action::Pin), true, false, t, cx)
                    .child(icon(Icon::Pin, t.fg, 18.))
                    .when(!tiny, |d| d.child(tr!("贴图", "Pin")))
                    .child(self.probe("long-clip-pin")),
            )
            .child(
                top_button(12, ChromeAction::Editor(Action::Save), true, false, t, cx)
                    .child(icon(Icon::Save, t.fg, 18.))
                    .when(!tiny, |d| d.child(tr!("保存", "Save")))
                    .child(self.probe("long-clip-save")),
            )
            .child(
                top_button_style(
                    clickable_base(13, ChromeAction::Editor(Action::Copy), true, cx),
                    false,
                    t,
                )
                .bg(rgb(t.accent))
                .hover(move |s| s.bg(rgb(t.accent)).opacity(0.92))
                .text_color(rgb(t.on_accent))
                .font_weight(FontWeight::SEMIBOLD)
                .child(icon(Icon::Copy, t.on_accent, 18.))
                .child(tr!("复制", "Copy"))
                .child(self.probe("long-clip-copy")),
            )
    }

    fn long_clip_rail(
        &self,
        rect: &NavRect,
        palette: Option<Option<ToolGroup>>,
        t: ClipTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let current = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        let button_w = (rect.width - 14.).min(56.);
        let item = |id: usize,
                    glyph: Icon,
                    label: SharedString,
                    tip: SharedString,
                    active: bool,
                    open: bool,
                    group: bool,
                    action: ChromeAction,
                    cx: &mut Context<Self>| {
            let color = if active { t.accent } else { t.muted };
            clickable(id, action, true, t, cx)
                .w(px(button_w))
                .h(px(49.))
                .flex_col()
                .gap(px(3.))
                .rounded(px(7.))
                .text_size(px(10.))
                .text_color(rgb(color))
                .when(active, |d| d.bg(rgb(t.accent_soft)))
                .when(open && !active, |d| d.bg(rgb(t.hover)))
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(-5.))
                            .top(px(14.5))
                            .w(px(3.))
                            .h(px(20.))
                            .rounded_r(px(3.))
                            .bg(rgb(t.accent)),
                    )
                })
                .child(icon(glyph, color, 20.))
                .child(label)
                .when(group, |d| {
                    d.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let o = bounds.origin;
                                let mut path = PathBuilder::fill();
                                path.move_to(point(o.x + px(4.), o.y));
                                path.line_to(point(o.x + px(4.), o.y + px(4.)));
                                path.line_to(point(o.x, o.y + px(4.)));
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, alpha(color, 0.5));
                                }
                            },
                        )
                        .absolute()
                        .right(px(5.))
                        .bottom(px(8.))
                        .w(px(4.))
                        .h(px(4.)),
                    )
                })
                .tooltip(move |_, cx| {
                    cx.new(|_| crate::capture_toolbar::ui_kit::Tip(tip.clone()))
                        .into()
                })
        };
        let mut rail = dock("long-clip-tools", *rect, t.panel)
            .border_r_1()
            .border_color(rgb(t.border))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(3.))
            .py(px(10.))
            .overflow_hidden()
            .child(item(
                50,
                Icon::Cut,
                tr!("剪辑", "Clip").into(),
                tr!(
                    "剪辑 · 拖选删段，拖边裁剪 · 1",
                    "Clip · drag bands or edges · 1"
                )
                .into(),
                current == Tool::Crop && self.editor.selected_annotation().is_none(),
                false,
                false,
                ChromeAction::PickTool(Tool::Crop),
                cx,
            ))
            .child(item(
                51,
                Icon::Edit,
                tr!("选择", "Select").into(),
                tr!(
                    "选择、框选与编辑标注 · 2",
                    "Select and edit annotations · 2"
                )
                .into(),
                self.editor.tool == Tool::Edit,
                false,
                false,
                ChromeAction::PickTool(Tool::Edit),
                cx,
            ))
            .child(
                div()
                    .w(px(28.))
                    .h(px(1.))
                    .my(px(6.))
                    .flex_shrink_0()
                    .bg(rgb(t.border)),
            );
        for (index, slot) in self.toolbar_config.slots().into_iter().enumerate() {
            let key = index + 3;
            rail = rail.child(match slot {
                Slot::Group(group) => {
                    let label = group_label(group);
                    item(
                        60 + index,
                        group_icon(group),
                        label.clone(),
                        format!("{label}{} · {key}", tr!("工具", " tools")).into(),
                        group.options(false).any(|tool| tool == current),
                        palette == Some(Some(group)),
                        true,
                        ChromeAction::Palette(Some(group)),
                        cx,
                    )
                }
                Slot::Tool(tool) => {
                    let label: SharedString = if tool == Tool::Lasso {
                        tr!("套索", "Lasso").into()
                    } else {
                        tool.label().into()
                    };
                    let glyph = match tool {
                        Tool::Lasso => Icon::Lasso,
                        _ => Icon::Edit,
                    };
                    item(
                        60 + index,
                        glyph,
                        label.clone(),
                        format!("{label} · {}", if tool == Tool::Lasso { 9 } else { key }).into(),
                        current == tool,
                        false,
                        false,
                        ChromeAction::PickTool(tool),
                        cx,
                    )
                }
            });
        }
        rail.child(div().flex_1().min_h(px(8.))).child(item(
            90,
            Icon::Toolbar,
            tr!("工具库", "Library").into(),
            tr!("打开完整工具库 · 0", "All annotation tools · 0").into(),
            false,
            palette == Some(None),
            false,
            ChromeAction::Palette(None),
            cx,
        ))
    }

    fn long_clip_tool_palette(
        &self,
        group: Option<ToolGroup>,
        main: &NavRect,
        t: ClipTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let current = self.editor.tool;
        let width = 288_f32.min(main.width - 24.).max(160.);
        let cell = (width - 28. - 4.) / 2.;
        let grid = |tools: Vec<Tool>, base: usize, cx: &mut Context<Self>| {
            let mut grid = div().flex().flex_wrap().gap(px(4.));
            for (offset, tool) in tools.into_iter().enumerate() {
                let on = tool == current;
                grid = grid.child(
                    clickable(base + offset, ChromeAction::PickTool(tool), true, t, cx)
                        .w(px(cell))
                        .h(px(39.))
                        .px(px(9.))
                        .gap(px(9.))
                        .justify_start()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(if on { rgb(t.accent) } else { rgba(0) })
                        .when(on, |d| d.bg(rgb(t.accent_soft)))
                        .text_size(px(12.))
                        .text_color(rgb(if on { t.accent } else { t.fg }))
                        .whitespace_nowrap()
                        .child(
                            toolbar_icons::icon(tool, if on { t.accent } else { t.fg })
                                .size(px(17.)),
                        )
                        .child(tool.label()),
                );
            }
            grid
        };
        let (heading_icon, heading) = match group {
            Some(group) => (group_icon(group), group_label(group)),
            None => (
                Icon::Toolbar,
                tr!("全部标注工具", "All annotation tools").into(),
            ),
        };
        let mut body = div()
            .id("long-clip-tool-palette-scroll")
            .flex()
            .flex_col()
            .min_h(px(0.))
            .overflow_y_scroll();
        match group {
            Some(group) => body = body.child(grid(group.options(false).collect(), 300, cx)),
            None => {
                for (index, group) in crate::toolbar_groups::GROUPS.into_iter().enumerate() {
                    body = body
                        .child(
                            div()
                                .mt(px(if index == 0 { 0. } else { 13. }))
                                .mb(px(6.))
                                .text_size(px(10.))
                                .text_color(rgb(t.muted))
                                .child(group_label(group)),
                        )
                        .child(grid(group.options(false).collect(), 300 + index * 10, cx));
                }
                body = body
                    .child(
                        div()
                            .mt(px(13.))
                            .mb(px(6.))
                            .text_size(px(10.))
                            .text_color(rgb(t.muted))
                            .child(tr!("套索", "Lasso")),
                    )
                    .child(grid(vec![Tool::Lasso], 390, cx));
            }
        }
        card("long-clip-tool-palette", t)
            .left(px(main.x + 12.))
            .top(px(main.y + 12.))
            .w(px(width))
            .max_h(px((main.height - 24.).max(80.)))
            .p(px(14.))
            .rounded(px(10.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mb(px(10.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(icon(heading_icon, t.fg, 18.))
                    .child(heading)
                    .child(div().flex_1())
                    .child(
                        clickable(299, ChromeAction::Palette(group), true, t, cx)
                            .size(px(26.))
                            .rounded(px(5.))
                            .text_color(rgb(t.muted))
                            .child("×"),
                    ),
            )
            .child(body)
    }

    #[allow(clippy::too_many_arguments)]
    fn long_clip_side(
        &mut self,
        clip: &LongClip,
        snap: bool,
        expanded: &[Range<u32>],
        rect: &NavRect,
        t: ClipTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        let mut content = div()
            .id("long-clip-properties-scroll")
            .size_full()
            .px(px(SIDE_PAD))
            .pt(px(SIDE_PAD))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .text_size(px(13.))
            .text_color(rgb(t.fg));
        let heading = |glyph: gpui::Svg, title: SharedString| {
            div()
                .h(px(25.))
                .mb(px(12.))
                .flex()
                .items_center()
                .gap(px(6.))
                .flex_shrink_0()
                .child(glyph)
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(div().flex_1())
        };
        let hint = |text: &'static str| {
            div()
                .mt(px(9.))
                .mb(px(13.))
                .text_size(px(10.))
                .line_height(px(16.5))
                .text_color(rgb(t.muted))
                .child(text)
        };
        let section = |title: SharedString| {
            div()
                .mb(px(9.))
                .flex()
                .items_center()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(t.muted))
                .child(title)
                .child(div().flex_1())
                .child(div().text_color(rgb(t.faint)).child("⌄"))
        };
        if tool != Tool::Crop {
            // 标注工具：原生属性卡停靠在标题下方，由工具栏层绘制。
            let docked = self.long_clip_property_dock(window);
            let has_properties =
                tool.draws() || matches!(tool, Tool::Lasso | Tool::Eraser | Tool::SmartMosaic);
            let card_height = docked
                .filter(|_| has_properties)
                .map_or(0., |docked| docked.properties.height + 12.);
            content = content
                .child(heading(
                    toolbar_icons::icon(tool, t.fg).size(px(18.)),
                    tool.label().into(),
                ))
                .child(div().h(px(card_height)).flex_shrink_0())
                .child(hint(if has_properties {
                    tr!(
                        "样式作用于选中对象；没有选中时设置下一笔。标注时图片左侧的剪辑轨仍可拖动选段。",
                        "Styles apply to the selection, or to the next stroke. The clip rail beside the image still selects bands."
                    )
                } else {
                    tr!(
                        "单击或框选标注后在这里调整样式；Shift 加选，双击文字编辑。",
                        "Select annotations to edit their style here; Shift adds, double-click edits text."
                    )
                }));
            return dock("long-clip-properties", *rect, t.panel)
                .border_l_1()
                .border_color(rgb(t.border))
                .child(content);
        }
        let (out_w, out_h) = clip.output_size();
        let (src_w, src_h) = clip.source_size();
        let source_long = match clip.axis() {
            ScrollAxis::Vertical => src_h,
            ScrollAxis::Horizontal => src_w,
        };
        let out_long = match clip.axis() {
            ScrollAxis::Vertical => out_h,
            ScrollAxis::Horizontal => out_w,
        };
        let untouched = clip.removed().is_empty() && trims(clip).iter().all(|v| *v == 0);
        let removed_total = source_long.saturating_sub(out_long);
        let mut summary = trf!(
            "成品像素 · 原图 {} × {}",
            "Output pixels · source {} × {}",
            thousands(src_w),
            thousands(src_h)
        );
        if removed_total > 0 {
            summary.push_str(&trf!(
                " · 去掉 {}",
                " · {} removed",
                thousands(removed_total)
            ));
        }
        let fields = self.long_clip_parameter_fields(clip, window, cx);
        content = content
            .child(
                heading(icon(Icon::Cut, t.fg, 18.), tr!("剪辑", "Clip").into()).child(
                    clickable(40, ChromeAction::RestoreAll, !untouched, t, cx)
                        .px(px(6.))
                        .py(px(2.))
                        .rounded(px(4.))
                        .text_size(px(11.))
                        .text_color(rgb(t.muted))
                        .child(tr!("全部恢复", "Restore all")),
                ),
            )
            .child(
                div()
                    .mb(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if clip.exceeds_output_limit() {
                                t.cut
                            } else {
                                t.fg
                            }))
                            .child(format!("{} × {}", thousands(out_w), thousands(out_h))),
                    )
                    .child(div().text_size(px(11.)).text_color(rgb(t.muted)).child(summary))
                    .when(clip.exceeds_output_limit(), |d| {
                        d.child(div().text_size(px(11.)).text_color(rgb(t.cut)).child(tr!(
                            "超过 3200 万像素或单边 65535，请继续删段或裁边",
                            "Over 32 MP or 65535 px; cut or trim further"
                        )))
                    }),
            )
            .child(
                div()
                    .pb(px(9.))
                    .mb(px(13.))
                    .border_b_1()
                    .border_color(rgb(t.border))
                    .flex_shrink_0()
                    .child(section(tr!("四边裁剪", "Edge trims").into()))
                    .child(fields)
                    .child(hint(tr!(
                        "也可直接拖图片外缘；拖首尾时已裁部分会露出来，可以拖回。",
                        "Or drag the image edges; trimmed parts reappear while dragging and can be restored."
                    ))),
            );
        let count = clip.removed().len();
        let mut records = div()
            .id("long-clip-records")
            .flex()
            .flex_col()
            .flex_shrink_0();
        if count == 0 {
            records = records.child(hint(tr!(
                "还没有删除。在图上上下拖动，选中不要的部分后按 Delete。",
                "Nothing removed yet. Drag along the image, then press Delete."
            )));
        }
        for (index, range) in clip.removed().iter().enumerate() {
            let kind = if range.start == 0 {
                tr!("开头裁剪", "Start trim")
            } else if range.end == source_long {
                tr!("结尾裁剪", "End trim")
            } else {
                tr!("删除段", "Cut")
            };
            let open = expanded.contains(range);
            records =
                records.child(
                    div()
                        .id(("long-clip-record", index))
                        .px(px(4.))
                        .py(px(5.))
                        .rounded(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .hover(move |s| s.bg(rgb(t.hover)))
                        .when(open, |d| d.bg(rgb(t.hover)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(4.))
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format!("{kind} ·"))
                                        .child(
                                            div()
                                                .text_color(rgb(t.cut))
                                                .child(thousands(range.end - range.start)),
                                        )
                                        .child("px"),
                                )
                                .child(div().text_size(px(10.5)).text_color(rgb(t.faint)).child(
                                    trf!(
                                        "原图 {} – {}",
                                        "Source {} – {}",
                                        thousands(range.start),
                                        thousands(range.end)
                                    ),
                                )),
                        )
                        .child(
                            clickable(1_000 + index * 2, ChromeAction::Locate(index), true, t, cx)
                                .h(px(24.))
                                .px(px(7.))
                                .rounded(px(5.))
                                .text_size(px(11.))
                                .text_color(rgb(t.muted))
                                .child(tr!("定位", "Locate")),
                        )
                        .child(
                            clickable(1_001 + index * 2, ChromeAction::Restore(index), true, t, cx)
                                .h(px(24.))
                                .px(px(7.))
                                .rounded(px(5.))
                                .text_size(px(11.))
                                .text_color(rgb(t.muted))
                                .child(tr!("恢复", "Restore")),
                        ),
                );
        }
        let all_open = count > 0 && clip.removed().iter().all(|r| expanded.contains(r));
        let option = |id: usize,
                      label: &'static str,
                      checked: bool,
                      action: ChromeAction,
                      cx: &mut Context<Self>| {
            clickable_base(id, action, true, cx)
                .min_h(px(35.))
                .w_full()
                .justify_start()
                .gap(px(8.))
                .rounded(px(5.))
                .hover(move |s| s.bg(rgba(0)))
                .text_size(px(11.))
                .child(div().flex_1().text_color(rgb(t.muted)).child(label))
                .child(
                    div()
                        .size(px(15.))
                        .mr(px(2.))
                        .rounded(px(3.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(checked, |d| d.bg(rgb(t.accent)))
                        .when(!checked, |d| d.border_1().border_color(rgb(t.muted)))
                        .when(checked, |d| d.child(icon(Icon::Check, t.on_accent, 12.))),
                )
        };
        content = content
            .child(
                div()
                    .pb(px(9.))
                    .mb(px(13.))
                    .border_b_1()
                    .border_color(rgb(t.border))
                    .flex_shrink_0()
                    .child(section(if count > 0 {
                        trf!("删除记录 {}", "Cuts {}", count).into()
                    } else {
                        tr!("删除记录", "Cuts").into()
                    }))
                    .child(records),
            )
            .child(
                div()
                    .pb(px(9.))
                    .mb(px(4.))
                    .flex_shrink_0()
                    .child(section(tr!("选项", "Options").into()))
                    .child(option(
                        41,
                        tr!("吸附空白间隙（Alt 暂停）", "Snap to blank gaps (Alt pauses)"),
                        snap,
                        ChromeAction::Snap,
                        cx,
                    ))
                    .child(option(
                        42,
                        tr!("原位显示已删除（H）", "Show removed in place (H)"),
                        all_open,
                        ChromeAction::ToggleAll,
                        cx,
                    )),
            )
            .child(hint(tr!(
                "剪辑作用于图片和标注叠好的画面：跨接缝的标注会像图片一样被剪开。切到标注工具后，图片左侧的剪辑轨仍可拖动选段。",
                "Clipping applies to the image with its annotations: marks crossing a seam are cut like the image. With annotation tools, the clip rail beside the image still selects bands."
            )));
        dock("long-clip-properties", *rect, t.panel)
            .border_l_1()
            .border_color(rgb(t.border))
            .child(content)
    }

    #[allow(clippy::too_many_arguments)]
    fn long_clip_status(
        &self,
        clip: &LongClip,
        rect: &NavRect,
        selected: bool,
        band_drag: bool,
        trimming: bool,
        panning: bool,
        t: ClipTheme,
        window: &Window,
    ) -> Stateful<Div> {
        let vertical = clip.axis() == ScrollAxis::Vertical;
        let snap = self.long_clip_ui.as_ref().is_some_and(|ui| ui.snap);
        let hint: SharedString = if !self.status.is_empty() {
            self.status.clone().into()
        } else if self.editor.has_arrow_draft() {
            tr!(
                "单击确定箭头终点 · Shift 约束角度 · Esc / 右键取消",
                "Click to set the arrow end · Shift constrains angle · Esc / right-click cancels"
            )
            .into()
        } else if self.editor.has_polyline() {
            tr!(
                "单击增加折线节点 · Shift 约束角度 · Enter / 右键完成",
                "Click to add polyline points · Shift constrains angle · Enter / right-click finishes"
            )
            .into()
        } else if band_drag {
            if snap {
                tr!(
                    "拖到画布边缘自动滚动 · 按住 Alt 暂停吸附 · Esc 放弃本次拖动",
                    "Drag to the canvas edge to scroll · hold Alt to pause snapping · Esc cancels"
                )
            } else {
                tr!(
                    "拖到画布边缘自动滚动 · Esc 放弃本次拖动",
                    "Drag to the canvas edge to scroll · Esc cancels"
                )
            }
            .into()
        } else if trimming {
            tr!(
                "松开即裁掉压暗部分，之后仍可拖回 · Esc 放弃",
                "Release to trim the dimmed part; drag back any time · Esc cancels"
            )
            .into()
        } else if panning {
            tr!("平移", "Panning").into()
        } else if selected {
            tr!(
                "Delete 删除 · K 仅保留 · 拖红线调整 · ↑↓ 微调 · 单击或 Esc 取消",
                "Delete removes · K keeps · drag the red lines · arrows nudge · click or Esc cancels"
            )
            .into()
        } else {
            match self.editor.tool {
                Tool::Crop if vertical => tr!(
                    "上下拖动选择要删除的部分 · 单击放起点，Shift+单击选到这里 · 滚轮浏览 · H 显示已删除部分",
                    "Drag up or down to select a band · click sets a start, Shift+click extends · H shows removed parts"
                )
                .into(),
                Tool::Crop => tr!(
                    "左右拖动选择要删除的部分 · 单击放起点，Shift+单击选到这里 · H 显示已删除部分",
                    "Drag sideways to select a band · click sets a start, Shift+click extends · H shows removed parts"
                )
                .into(),
                Tool::Edit => tr!(
                    "单击或框选标注，Shift 加选，拖动移动 · 双击文字编辑、双击组合进入",
                    "Click or box-select annotations, Shift adds · double-click edits text or enters a group"
                )
                .into(),
                tool => trf!(
                    "{}：在图上绘制 · 图片左侧的剪辑轨仍可拖动选段",
                    "{}: draw on the image · the clip rail still selects bands",
                    tool.label()
                )
                .into(),
            }
        };
        let hidden = hidden_annotations(self, clip);
        let zoom = (self.canvas_viewport(window).zoom / window.scale_factor() * 100.).round();
        dock("long-clip-status", *rect, t.panel)
            .border_t_1()
            .border_color(rgb(t.border))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(14.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(12.))
            .text_color(rgb(t.muted))
            .child(
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(hint),
            )
            .child(div().flex_1())
            .when(hidden > 0, |d| {
                d.child(div().text_color(rgb(t.cut)).child(trf!(
                    "{} 个标注随删除段隐藏",
                    "{} annotations hidden with cuts",
                    hidden
                )))
            })
            .child(div().text_color(rgb(t.faint)).child(format!("{zoom}%")))
    }
}
