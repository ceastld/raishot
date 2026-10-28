//! 自定义颜色浮层：在截图画布（或贴图参数小窗）内绘制 HSV 选色器，不再弹出系统调色盘。
//! 只在“确定”时写回一次，拖动预览不提交撤销；编辑上下文变化时拒绝旧颜色。
use crate::{
    annotation::{Style, Tool},
    capture_view::CaptureView,
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    scene::SourceIdentity,
    text_field::{TextField, TextFieldEvent},
    toolbar_layout::{ColorTarget, ToolbarPanel},
    toolbar_widgets as w,
};
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, prelude::*, px, quad, rgb, size,
    BorderStyle, Bounds, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, Pixels,
    Subscription, WeakEntity, Window,
};
use std::{cell::Cell, rc::Rc};

/// 浮层内容尺寸（不含面板内边距 12）。
pub(crate) const PICKER_WIDTH: f32 = 228.;
pub(crate) const PICKER_HEIGHT: f32 = 332.;
const SV_HEIGHT: f32 = 132.;
pub(crate) const PRESETS: [u32; 10] = [
    0xff4b5c, 0xff934f, 0xffd34d, 0x39c996, 0x2bb8c8, 0x5796ff, 0xbc8bff, 0xffffff, 0x8a94a0,
    0x161b22,
];

#[derive(PartialEq)]
struct EditorContext {
    tool: Tool,
    style: Style,
    selection: Vec<ObjectId>,
    scene: (u64, u64),
    edit_revision: u64,
    crop: Option<Rect>,
}
impl EditorContext {
    fn capture(editor: &Editor) -> Self {
        Self {
            tool: editor.tool,
            style: editor.style,
            selection: editor.selected_ids(),
            scene: editor.scene_stamp(),
            edit_revision: editor.document.edit_revision(),
            crop: editor.crop,
        }
    }
    fn matches(&self, editor: &Editor) -> bool {
        *self == Self::capture(editor)
    }
}

pub(crate) fn rgb_to_hsv(color: u32) -> (f32, f32, f32) {
    let r = ((color >> 16) & 0xff) as f32 / 255.;
    let g = ((color >> 8) & 0xff) as f32 / 255.;
    let b = (color & 0xff) as f32 / 255.;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta <= f32::EPSILON {
        0.
    } else if max == r {
        60. * ((g - b) / delta).rem_euclid(6.)
    } else if max == g {
        60. * ((b - r) / delta + 2.)
    } else {
        60. * ((r - g) / delta + 4.)
    };
    let saturation = if max <= f32::EPSILON { 0. } else { delta / max };
    (hue, saturation, max)
}

pub(crate) fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> u32 {
    let h = hue.rem_euclid(360.) / 60.;
    let s = saturation.clamp(0., 1.);
    let v = value.clamp(0., 1.);
    let c = v * s;
    let x = c * (1. - (h % 2. - 1.).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    let m = v - c;
    let byte = |t: f32| ((t + m) * 255.).round().clamp(0., 255.) as u32;
    byte(r) << 16 | byte(g) << 8 | byte(b)
}

/// 接受 `#RRGGBB`、`RRGGBB`、`#RGB`；其它输入视为未完成。
pub(crate) fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.trim().trim_start_matches('#');
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match digits.len() {
        6 => u32::from_str_radix(digits, 16).ok(),
        3 => {
            let v = u32::from_str_radix(digits, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some((r * 17) << 16 | (g * 17) << 8 | b * 17)
        }
        _ => None,
    }
}

fn hsla(color: u32, alpha: f32) -> gpui::Hsla {
    let mut c: gpui::Hsla = rgb(color).into();
    c.a = alpha;
    c
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Field,
    Hue,
}

pub(crate) struct ColorPicker {
    owner: WeakEntity<CaptureView>,
    pub(crate) target: ColorTarget,
    pub(crate) anchor: Option<(f32, f32)>,
    initial: u32,
    hue: f32,
    saturation: f32,
    value: f32,
    hex: Entity<TextField>,
    editor: EditorContext,
    source: SourceIdentity,
    drag: Option<Drag>,
    field_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    hue_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    focus: FocusHandle,
    closed: bool,
    _subscriptions: Vec<Subscription>,
}

impl ColorPicker {
    fn color(&self) -> u32 {
        hsv_to_rgb(self.hue, self.saturation, self.value)
    }
    fn set_color(&mut self, color: u32, cx: &mut Context<Self>) {
        let (hue, saturation, value) = rgb_to_hsv(color);
        // 灰色没有色相：保留当前色相，避免拖到无彩区后色相条跳回红色。
        if saturation > 0. && value > 0. {
            self.hue = hue;
        }
        self.saturation = saturation;
        self.value = value;
        self.sync_hex(cx);
    }
    fn sync_hex(&mut self, cx: &mut Context<Self>) {
        let text = format!("#{:06X}", self.color());
        self.hex.update(cx, |field, cx| field.set_text(text, cx));
        cx.notify();
    }
    fn drag_to(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let ratio = |bounds: Bounds<Pixels>| {
            let x = (f32::from(position.x - bounds.left()) / f32::from(bounds.size.width).max(1.))
                .clamp(0., 1.);
            let y = (f32::from(position.y - bounds.top()) / f32::from(bounds.size.height).max(1.))
                .clamp(0., 1.);
            (x, y)
        };
        match self.drag {
            Some(Drag::Field) => {
                if let Some(bounds) = self.field_bounds.get() {
                    let (x, y) = ratio(bounds);
                    self.saturation = x;
                    self.value = 1. - y;
                }
            }
            Some(Drag::Hue) => {
                if let Some(bounds) = self.hue_bounds.get() {
                    self.hue = ratio(bounds).0 * 359.9;
                }
            }
            None => return,
        }
        self.sync_hex(cx);
    }
    fn end_drag(&mut self, window: &Window) {
        if self.drag.take().is_some() {
            unsafe {
                use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
                if GetCapture() == crate::gpui_capture::hwnd(window) {
                    let _ = ReleaseCapture();
                }
            }
        }
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.end_drag(window);
        self.closed = true;
        let panel = ToolbarPanel::CustomColor(self.target);
        let _ = self.owner.update(cx, |v, cx| {
            if v.toolbar_panel == panel {
                v.toolbar_panel = panel.parent();
                if !v.pin_mode {
                    window.focus(&v.focus);
                }
                cx.notify();
            }
        });
    }
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if self.hex.read(cx).is_composing() {
            return;
        }
        let color = parse_hex(self.hex.read(cx).text()).unwrap_or_else(|| self.color());
        let target = self.target;
        let _ = self.owner.update(cx, |v, cx| {
            let current = !v.processing
                && !v.saving
                && self.editor.matches(&v.editor)
                && self.source == SourceIdentity::of(&v.source_pixels());
            if current {
                v.apply_custom_color(target, color, cx);
            } else {
                v.status = tr!(
                    "编辑状态已改变，颜色未应用",
                    "Editing state changed; color not applied"
                )
                .into();
            }
        });
        self.close(window, cx);
    }
    fn capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if !matches!(key, "enter" | "escape") {
            return;
        }
        if !event.is_held
            && !event.keystroke.modifiers.modified()
            && !self.hex.read(cx).is_composing()
        {
            match key {
                "enter" => self.apply(window, cx),
                _ => self.close(window, cx),
            }
        }
        cx.stop_propagation();
    }
}

impl Render for ColorPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let color = self.color();
        let pure = hsv_to_rgb(self.hue, 1., 1.);
        let view = cx.entity().downgrade();
        let (hue, saturation, value) = (self.hue, self.saturation, self.value);
        let field_bounds = self.field_bounds.clone();
        let hue_bounds = self.hue_bounds.clone();

        // 色相条：高 12、圆角 6，六段双色渐变拼接；滑块是带白边的当前色相圆点。
        let stops = [
            0xff0000, 0xffff00, 0x00ff00, 0x00ffff, 0x0000ff, 0xff00ff, 0xff0000,
        ];
        let mut hue_bar = div().relative().flex().w_full().h(px(12.)).rounded(px(6.));
        for (i, pair) in stops.windows(2).enumerate() {
            let segment = div().flex_1().h_full().bg(linear_gradient(
                90.,
                linear_color_stop(rgb(pair[0]), 0.),
                linear_color_stop(rgb(pair[1]), 1.),
            ));
            hue_bar = hue_bar.child(match i {
                0 => segment.rounded_l(px(6.)),
                5 => segment.rounded_r(px(6.)),
                _ => segment,
            });
        }
        // 圆形滑块：外圈 1px 半透明黑、2px 白边。
        let knob = |window: &mut Window, center: gpui::Point<Pixels>, fill: u32| {
            let ring = |r: f32| {
                Bounds::new(
                    point(center.x - px(r), center.y - px(r)),
                    size(px(r * 2.), px(r * 2.)),
                )
            };
            window.paint_quad(quad(
                ring(8.),
                px(8.),
                gpui::transparent_black(),
                px(1.),
                hsla(0x000000, 0.5),
                BorderStyle::Solid,
            ));
            window.paint_quad(quad(
                ring(7.),
                px(7.),
                rgb(fill),
                px(2.),
                rgb(0xffffff),
                BorderStyle::Solid,
            ));
        };
        let field_view = view.clone();
        let hue_view = view.clone();
        let hue_bar = hue_bar
            .id("custom-color-hue")
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    w.focus(&v.focus);
                    v.drag = Some(Drag::Hue);
                    v.drag_to(e.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |rect, _, window, _| {
                        hue_bounds.set(Some(rect));
                        let center = point(
                            rect.left() + rect.size.width * (hue / 360.),
                            rect.center().y,
                        );
                        knob(window, center, pure);
                        let weak = hue_view.clone();
                        window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak.update(cx, |v, cx| {
                                    if v.drag == Some(Drag::Hue) {
                                        if e.pressed_button == Some(MouseButton::Left) {
                                            v.drag_to(e.position, cx);
                                        } else {
                                            v.end_drag(w);
                                        }
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );

        // 饱和度／明度区：高 132、圆角 8；纯色底 + 白色横向渐变 + 黑色纵向渐变。
        let sv = div()
            .id("custom-color-field")
            .relative()
            .w_full()
            .h(px(SV_HEIGHT))
            .rounded(px(8.))
            .bg(rgb(pure))
            .cursor(gpui::CursorStyle::Crosshair)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    w.focus(&v.focus);
                    v.drag = Some(Drag::Field);
                    v.drag_to(e.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(px(8.))
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(hsla(0xffffff, 1.), 0.),
                        linear_color_stop(hsla(0xffffff, 0.), 1.),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(px(8.))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(hsla(0x000000, 0.), 0.),
                        linear_color_stop(hsla(0x000000, 1.), 1.),
                    )),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |rect, _, window, _| {
                        field_bounds.set(Some(rect));
                        let center = point(
                            rect.left() + rect.size.width * saturation,
                            rect.top() + rect.size.height * (1. - value),
                        );
                        knob(window, center, color);
                        let weak = field_view.clone();
                        window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak.update(cx, |v, cx| {
                                    if v.drag == Some(Drag::Field) {
                                        if e.pressed_button == Some(MouseButton::Left) {
                                            v.drag_to(e.position, cx);
                                        } else {
                                            v.end_drag(w);
                                        }
                                    }
                                });
                            }
                        });
                        let weak = field_view.clone();
                        window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && e.button == MouseButton::Left
                            {
                                let _ = weak.update(cx, |v, cx| {
                                    if v.drag.is_some() {
                                        v.drag_to(e.position, cx);
                                        v.end_drag(w);
                                        cx.stop_propagation();
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );

        // 10 个预设：两行五列色块，选中带外环。
        let mut presets = div().flex().flex_col().gap(px(2.));
        for (row, chunk) in PRESETS.chunks(5).enumerate() {
            let mut line = div().flex().justify_between();
            for (column, &preset) in chunk.iter().enumerate() {
                line = line.child(
                    w::swatch(
                        ("custom-color-preset", row * 5 + column),
                        preset,
                        preset == color,
                        p,
                    )
                    .on_click(cx.listener(move |v, _, _, cx| v.set_color(preset, cx))),
                );
            }
            presets = presets.child(line);
        }

        // 原色／新色对比（点原色恢复）+ HEX 输入。
        let initial = self.initial;
        let compare = div()
            .flex_shrink_0()
            .w(px(44.))
            .h(px(26.))
            .flex()
            .rounded(px(6.))
            .overflow_hidden()
            .border_1()
            .border_color(gpui::rgba(p.swatch_edge))
            .child(
                div()
                    .id("custom-color-initial")
                    .flex_1()
                    .h_full()
                    .bg(rgb(initial))
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| v.set_color(initial, cx))),
            )
            .child(div().flex_1().h_full().bg(rgb(color)));

        div()
            .id("custom-color-picker")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .gap(px(10.))
            .text_size(px(12.))
            .capture_key_down(cx.listener(Self::capture_key))
            .on_key_down(cx.listener(|_, _: &KeyDownEvent, _, cx| cx.stop_propagation()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .min_h(px(26.))
                    .child(w::popup_title(
                        match self.target {
                            ColorTarget::TextOutline => tr!("描边颜色", "Outline color"),
                            ColorTarget::TextBackground => tr!("底色颜色", "Background color"),
                            _ => tr!("自定义颜色", "Custom color"),
                        },
                        p,
                    ))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(p.muted))
                            .child(tr!("Enter 确定 · Esc 返回", "Enter OK · Esc Back")),
                    ),
            )
            .child(sv)
            .child(hue_bar)
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .items_center()
                    .child(compare)
                    .child(div().flex_1().min_w_0().child(self.hex.clone())),
            )
            .child(presets)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.))
                    .child(
                        w::text_button(
                            "custom-color-cancel",
                            tr!("取消", "Cancel"),
                            false,
                            true,
                            p,
                        )
                        .on_click(cx.listener(|v, _, w, cx| {
                            v.close(w, cx);
                            cx.stop_propagation();
                        })),
                    )
                    .child(
                        w::text_button("custom-color-apply", tr!("确定", "OK"), true, true, p)
                            .on_click(cx.listener(|v, _, w, cx| {
                                v.apply(w, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
    }
}

impl CaptureView {
    /// 打开自定义颜色浮层；颜色只在确定时按 `target` 写回一次。
    pub(crate) fn open_color_picker(
        &mut self,
        target: ColorTarget,
        color: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing || self.saving {
            return;
        }
        let panel = ToolbarPanel::CustomColor(target);
        if self.toolbar_panel == panel {
            self.toolbar_panel = panel.parent();
            cx.notify();
            return;
        }
        let color = color & 0xff_ffff;
        let anchor = self.ui_positions.borrow().get(panel.anchor()).copied();
        let owner = cx.entity().downgrade();
        let editor = EditorContext::capture(&self.editor);
        let source = SourceIdentity::of(&self.source_pixels());
        let picker = cx.new(|cx: &mut Context<ColorPicker>| {
            let hex = cx.new(|cx| {
                TextField::new_settings(
                    format!("#{color:06X}"),
                    crate::i18n::Text("#RRGGBB", "#RRGGBB"),
                    cx,
                )
            });
            let subscription =
                cx.subscribe_in(&hex, window, |v, field, event, w, cx| match event {
                    TextFieldEvent::Changed => {
                        if let Some(color) = parse_hex(field.read(cx).text()) {
                            let (hue, saturation, value) = rgb_to_hsv(color);
                            if saturation > 0. && value > 0. {
                                v.hue = hue;
                            }
                            v.saturation = saturation;
                            v.value = value;
                            cx.notify();
                        }
                    }
                    TextFieldEvent::Submit => v.apply(w, cx),
                });
            let (hue, saturation, value) = rgb_to_hsv(color);
            ColorPicker {
                owner,
                target,
                anchor,
                initial: color,
                hue,
                saturation,
                value,
                hex,
                editor,
                source,
                drag: None,
                field_bounds: Rc::new(Cell::new(None)),
                hue_bounds: Rc::new(Cell::new(None)),
                focus: cx.focus_handle(),
                closed: false,
                _subscriptions: vec![subscription],
            }
        });
        window.focus(&picker.read(cx).focus);
        self.color_picker = Some(picker);
        self.toolbar_panel = panel;
        cx.notify();
    }

    pub(crate) fn apply_custom_color(
        &mut self,
        target: ColorTarget,
        color: u32,
        cx: &mut Context<Self>,
    ) {
        match target {
            ColorTarget::Annotation => self.editor.set_color(color),
            ColorTarget::TextOutline | ColorTarget::TextBackground => {
                let mut style = self.editor.style;
                if target == ColorTarget::TextOutline {
                    style.text.outline_color = color;
                } else {
                    style.text.background_color = color;
                }
                self.editor.set_style(style);
            }
            ColorTarget::FinishBorder => self.set_finish_color(true, 0xff00_0000 | color, cx),
            ColorTarget::FinishShadow => self.set_finish_color(false, 0xff00_0000 | color, cx),
        }
        cx.notify();
    }

    /// 浮层内容；面板位置优先用打开时记下的按钮位置，避免父面板关闭后锚点丢失。
    pub(crate) fn color_picker_popup(
        &self,
        target: ColorTarget,
        layout: crate::toolbar_layout::ToolbarLayout,
        anchor: Option<(f32, f32)>,
        selection_actions: Option<crate::selection_action_layout::SelectionActionLayout>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let picker = self
            .color_picker
            .as_ref()
            .filter(|picker| picker.read(cx).target == target);
        let (width, height) = (PICKER_WIDTH + 24., PICKER_HEIGHT + 24.);
        let anchor = if self.pin_mode {
            anchor
        } else {
            picker.and_then(|p| p.read(cx).anchor).or(anchor)
        };
        let rect = match (target, selection_actions) {
            (ColorTarget::FinishBorder | ColorTarget::FinishShadow, Some(actions)) => {
                actions.popup(width, height)
            }
            _ => layout.popup(width, height, anchor, true),
        };
        let panel = self
            .panel("toolbar-popup", rect)
            .rounded(px(10.))
            .p(px(12.));
        match picker {
            Some(picker) => panel.child(picker.clone()),
            None => panel.child(tr!("调色盘已关闭", "Color picker closed")),
        }
        .child(self.probe("toolbar-popup"))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected_rectangle() -> Editor {
        let mut editor = Editor::new(Rect::full(400, 300), Some(Rect::full(400, 300)));
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((20, 30), 4.);
        editor.pointer_up((100, 90));
        editor.select_tool(Tool::Edit);
        editor.pointer_down((20, 60), 4.);
        editor.pointer_up((20, 60));
        assert_eq!(editor.selected_ids().len(), 1);
        editor
    }

    #[test]
    fn hsv_round_trips_common_colors() {
        for color in [
            0xff4b5c, 0x39c996, 0x5796ff, 0xffffff, 0x000000, 0x808080, 0x123456,
        ] {
            let (h, s, v) = rgb_to_hsv(color);
            assert_eq!(hsv_to_rgb(h, s, v), color, "{color:06X}");
        }
        assert_eq!(hsv_to_rgb(0., 1., 1.), 0xff0000);
        assert_eq!(hsv_to_rgb(120., 1., 1.), 0x00ff00);
        assert_eq!(hsv_to_rgb(240., 1., 1.), 0x0000ff);
    }

    #[test]
    fn hex_input_accepts_long_and_short_forms_only() {
        assert_eq!(parse_hex("#12ABef"), Some(0x12abef));
        assert_eq!(parse_hex(" 12abef "), Some(0x12abef));
        assert_eq!(parse_hex("#f0a"), Some(0xff00aa));
        assert_eq!(parse_hex("#12ab"), None);
        assert_eq!(parse_hex("#12abeg"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn color_panels_return_to_their_parent() {
        assert_eq!(
            ToolbarPanel::CustomColor(ColorTarget::Annotation).parent(),
            ToolbarPanel::None
        );
        assert_eq!(
            ToolbarPanel::CustomColor(ColorTarget::TextOutline).parent(),
            ToolbarPanel::TextOutlineStyle
        );
        assert_eq!(
            ToolbarPanel::CustomColor(ColorTarget::FinishShadow).parent(),
            ToolbarPanel::Finish
        );
    }

    #[test]
    fn unchanged_dialog_context_allows_color_and_preserves_undo() {
        let mut editor = selected_rectangle();
        let context = EditorContext::capture(&editor);
        assert!(context.matches(&editor));
        let before = editor.document.items()[0].annotation.style.color;
        editor.set_color(0x123456);
        assert_eq!(editor.document.items()[0].annotation.style.color, 0x123456);
        editor.undo();
        assert_eq!(editor.document.items()[0].annotation.style.color, before);
        editor.redo();
        assert_eq!(editor.document.items()[0].annotation.style.color, 0x123456);
    }

    #[test]
    fn selection_or_tool_change_rejects_old_color_even_without_document_change() {
        let mut editor = selected_rectangle();
        let context = EditorContext::capture(&editor);
        let revision = editor.document.revision();
        editor.selected = None;
        editor.selection.clear();
        assert_eq!(editor.document.revision(), revision);
        assert!(!context.matches(&editor));
        let context = EditorContext::capture(&editor);
        editor.select_tool(Tool::Text);
        assert!(!context.matches(&editor));
    }

    #[test]
    fn changed_style_crop_or_undo_history_rejects_old_color() {
        let mut editor = selected_rectangle();
        let context = EditorContext::capture(&editor);
        editor.style.text.outline_color = 0xabcdef;
        assert!(!context.matches(&editor));
        let context = EditorContext::capture(&editor);
        editor.crop = Some(Rect::full(200, 100));
        assert!(!context.matches(&editor));
        let context = EditorContext::capture(&editor);
        editor.undo();
        assert!(!context.matches(&editor));
    }
}
