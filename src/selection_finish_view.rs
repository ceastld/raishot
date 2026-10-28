//! 选区外观不改变标注文档；几何边带复用缓存，内容只重建四个小圆角。
use crate::{
    capture::Image,
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    selection_finish::{
        FinishEffect, FinishField, FinishPreset, RoundedGeometry, SelectionFinishStyle,
    },
    toolbar_layout::{ColorTarget, PanelRect, ToolbarPanel},
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, Bounds, Context, PathBuilder, RenderImage,
    ScrollWheelEvent, SharedString,
};
use std::sync::Arc;

struct RailHint(SharedString);
impl gpui::Render for RailHint {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .text_size(px(11.))
            .shadow_lg()
            .child(self.0.clone())
    }
}

pub(crate) const FINISH_POPUP_WIDTH: f32 = 296.;
/// 画廊里一个样式的高度：大缩略图 32 + 名字一行 + 上下留白。
const FINISH_TILE_HEIGHT: f32 = 64.;
const FINISH_SUBHEAD_HEIGHT: f32 = 20.;
const FINISH_RAW_HINT_HEIGHT: f32 = 40.;

/// 样式按钮的探针 id：`gallery` 为面板画廊里的大缩略图，否则为选区条上的缩略图。
pub(crate) fn look_id(look: FinishPreset, gallery: bool) -> &'static str {
    match (look, gallery) {
        (FinishPreset::Raw, false) => "selection-look-raw",
        (FinishPreset::Round, false) => "selection-look-round",
        (FinishPreset::Card, false) => "selection-look-card",
        (FinishPreset::Stroke, false) => "selection-look-stroke",
        (FinishPreset::Window, false) => "selection-look-window",
        (FinishPreset::Raw, true) => "finish-look-raw",
        (FinishPreset::Round, true) => "finish-look-round",
        (FinishPreset::Card, true) => "finish-look-card",
        (FinishPreset::Stroke, true) => "finish-look-stroke",
        (FinishPreset::Window, true) => "finish-look-window",
    }
}

/// 选区角上圆角控制点的一次拖动；半径按起点半径加对角线位移计算，不跳变。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FinishRadiusDrag {
    pub corner: u8,
    /// 该角向选区内部的方向。
    pub sign: (f32, f32),
    pub start: crate::annotation::Point,
    pub radius: f32,
    pub limit: f32,
}

/// 外观面板的内容：样式画廊 + 当前样式的调整项，或只有调整项。
/// 选区条“调整”按钮打开后者；收起成单按钮、更多菜单与属性栏入口打开前者。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FinishSection {
    #[default]
    Gallery,
    Tune,
}

// 长截图不受 GPU 单纹理尺寸限制；每条边继续分片，不上传巨型透明纹理。
const PREVIEW_TILE_SIZE: u32 = 4096;
const MASK_ALPHA: u32 = 0x77;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishLayer {
    Shadow,
    Corner,
    Border,
}

pub(crate) struct FinishPart {
    /// 坐标相对扩展后的输出左上角，CPU 像素只由 RenderImage 持有一份。
    pub rect: Rect,
    pub preview: Arc<RenderImage>,
    pub layer: FinishLayer,
}
pub(crate) struct FinishPreview {
    pub style: SelectionFinishStyle,
    pub width: u32,
    pub height: u32,
    /// 始终按 Shadow → Corner → Border 排列。
    pub parts: Vec<FinishPart>,
    corner_key: Option<(Rect, (u64, u64), bool)>,
}

#[derive(Clone, Copy)]
enum Change {
    /// 换成某个样式记住的外观。
    Look(FinishPreset),
    /// 属性栏芯片：开关该效果，不影响另一个效果。
    Mode(FinishEffect),
    Intensity(FinishEffect, f32),
    Parameter(FinishField, f32),
    Corner(bool),
    Inside(bool),
    Color(bool, u32),
    Reset,
}

fn preset_effect(preset: FinishPreset) -> Option<FinishEffect> {
    match preset {
        FinishPreset::Card => Some(FinishEffect::Shadow),
        FinishPreset::Stroke => Some(FinishEffect::Border),
        FinishPreset::Raw | FinishPreset::Round | FinishPreset::Window => None,
    }
}

/// 打开边框时宽度不足 1px 则按强度 30% 给默认宽度，与旧描边入口一致。
fn with_border_enabled(mut style: SelectionFinishStyle, on: bool) -> SelectionFinishStyle {
    if on && style.border_thickness < 1. {
        let shadow = style.shadow_enabled;
        style = FinishEffect::Border.with_intensity(style, 30.);
        style.shadow_enabled = shadow;
    }
    style.border_enabled = on;
    style
}

impl CaptureView {
    pub(crate) fn reset_finish_for_image(&mut self) {
        // 文件编辑以无效果开始，参数保留，且不算用户修改。
        self.finish_style.corner_enabled = false;
        self.finish_style.shadow_enabled = false;
        self.finish_style.border_enabled = false;
        self.finish_preset = FinishPreset::Raw;
        self.finish_hover = None;
        self.finish_store.reset_session_style(self.finish_style);
        self.finish_store.reset_session_preset(self.finish_preset);
        self.finish_effect_intent = None;
        self.finish_preview = None;
    }
    pub(crate) fn effective_finish_style(&self) -> SelectionFinishStyle {
        if self.pin_mode
            || self.editor.lasso_draft.is_some()
            || self.editor.document.crop_mask.is_some()
        {
            SelectionFinishStyle::default()
        } else {
            self.finish_style.normalized()
        }
    }
    /// 画面上的外观：指针停在某个样式上时预览它，其余时候与导出一致。
    pub(crate) fn shown_finish_style(&self) -> SelectionFinishStyle {
        match self.finish_hover {
            Some(look) if look != self.finish_preset && self.finish_form_available() => {
                SelectionFinishStyle {
                    remember_enabled: self.finish_style.remember_enabled,
                    ..self.finish_looks.get(look)
                }
            }
            _ => self.effective_finish_style(),
        }
    }
    pub(crate) fn finished_origin(&self, content: Rect) -> (i32, i32) {
        crate::capture_pin_output::finished_origin(
            self.source.bounds,
            content,
            self.effective_finish_style(),
        )
    }
    pub(crate) fn set_finish_color(&mut self, border: bool, color: u32, cx: &mut Context<Self>) {
        self.change_finish(Change::Color(border, color), cx);
    }
    fn change_finish(&mut self, change: Change, cx: &mut Context<Self>) {
        if !self.finish_form_available() {
            return;
        }
        let old = (self.finish_style, self.finish_preset);
        let style = self.finish_style;
        // 样式里的圆角、阴影、边框各自独立；调一个效果的强度不改另一个效果。
        match change {
            Change::Look(look) => {
                self.finish_preset = look;
                self.finish_style = SelectionFinishStyle {
                    remember_enabled: style.remember_enabled,
                    ..self.finish_looks.get(look)
                };
            }
            Change::Mode(effect) => {
                self.finish_effect = effect;
                self.finish_style = match effect {
                    FinishEffect::Shadow => style.with_shadow_enabled(!style.shadow_enabled),
                    FinishEffect::Border => with_border_enabled(style, !style.border_enabled),
                };
            }
            Change::Intensity(effect, value) => {
                self.finish_effect = effect;
                self.finish_style = effect.with_intensity(style, value);
                match effect {
                    FinishEffect::Shadow => self.finish_style.border_enabled = style.border_enabled,
                    FinishEffect::Border => self.finish_style.shadow_enabled = style.shadow_enabled,
                }
            }
            Change::Parameter(field, value) => {
                self.finish_style = field.applied(style, value);
            }
            Change::Corner(on) => self.finish_style.corner_enabled = on,
            // 内外侧和颜色只属于对应效果；调整它们即开启该效果。
            Change::Inside(inside) => {
                self.finish_style = with_border_enabled(style, true);
                self.finish_style.border_inside = inside;
            }
            Change::Color(border, color) => {
                if border {
                    self.finish_style = with_border_enabled(style, true);
                    self.finish_style.border_color = color;
                } else {
                    self.finish_style = style.with_shadow_enabled(true);
                    self.finish_style.shadow_color = color;
                }
            }
            // 当前样式回到出厂外观；原样即全关。
            Change::Reset => {
                self.finish_style = SelectionFinishStyle {
                    remember_enabled: style.remember_enabled,
                    ..self.finish_preset.defaults()
                };
                self.finish_effect = FinishEffect::Shadow;
            }
        }
        self.finish_style = self.finish_style.normalized();
        // 原样上调出效果（拖圆角柄、属性栏开关）就切到相符的样式；调整都记进当前样式。
        self.finish_preset = self.finish_preset.settle(self.finish_style);
        self.finish_looks.set(self.finish_preset, self.finish_style);
        // 不写 INI，不创建标注撤销记录；达到滚轮边界时也不重复刷新。
        if (self.finish_style, self.finish_preset) != old {
            if let Some(effect) = preset_effect(self.finish_preset) {
                self.finish_effect = effect;
            }
            match change {
                Change::Mode(_) | Change::Intensity(_, _) => {
                    self.finish_effect_intent = Some(self.finish_effect);
                }
                Change::Look(_)
                | Change::Inside(_)
                | Change::Color(_, _)
                | Change::Reset
                | Change::Parameter(
                    FinishField::Blur | FinishField::Opacity | FinishField::Thickness,
                    _,
                ) => {
                    self.finish_effect_intent = None;
                }
                _ => {}
            }
            cx.notify();
        }
    }
    /// 选区条与画廊：单击样式即换成它记住的外观；外观表单不可用时忽略。
    pub(crate) fn select_finish_look(&mut self, look: FinishPreset, cx: &mut Context<Self>) {
        self.change_finish(Change::Look(look), cx);
    }
    /// 样式上的滚轮：向下换下一个、向上换上一个，到头不循环；触控板小步累积到一格再换。
    pub(crate) fn finish_look_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if !self.finish_form_available() {
            return;
        }
        let delta = crate::capture_interaction::notches(event, self.wheel_settings);
        if !delta.is_finite() || delta == 0. {
            return;
        }
        if self.finish_look_wheel * delta < 0. {
            self.finish_look_wheel = 0.;
        }
        self.finish_look_wheel += delta;
        let steps = self.finish_look_wheel.trunc();
        if steps == 0. {
            return;
        }
        self.finish_look_wheel -= steps;
        let all = FinishPreset::ALL;
        let index = all
            .iter()
            .position(|look| *look == self.finish_preset)
            .unwrap_or(0) as i32;
        let next = (index - steps as i32).clamp(0, all.len() as i32 - 1) as usize;
        // 滚轮换的是当前样式，悬停预览让位给它。
        self.finish_hover = None;
        self.change_finish(Change::Look(all[next]), cx);
    }
    /// 指针进出某个样式：进入时在选区上预览，离开时恢复；不提交、不写盘。
    pub(crate) fn hover_finish_look(
        &mut self,
        look: FinishPreset,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let next = if hovered {
            Some(look)
        } else if self.finish_hover == Some(look) {
            None
        } else {
            self.finish_hover
        };
        if next != self.finish_hover {
            self.finish_hover = next;
            cx.notify();
        }
    }
    /// 样式缩略图：按该样式记住的外观画出圆角、阴影与边框，缩略图就是导出效果的示意。
    pub(crate) fn finish_thumb(&self, preset: FinishPreset, large: bool) -> gpui::Div {
        // 代表截图内容的面，颜色不随主题；底槽随主题。
        const FACE: u32 = 0x7aa0c8;
        let p = Palette::new(self.light_theme);
        let style = self.finish_looks.get(preset);
        let k = if large { 1.8 } else { 1. };
        let (width, height) = (15. * k, 10. * k);
        let mut face = div().w(px(width)).h(px(height)).bg(rgb(FACE));
        if style.corner_enabled {
            face = face.rounded(px(
                (style.corner_radius * 0.25 * k).clamp(1.5 * k, height / 2.)
            ));
        }
        if style.has_shadow() {
            let mut color: gpui::Hsla = rgb(style.shadow_color & 0xff_ffff).into();
            color.a = (style.shadow_opacity + 0.3).min(0.95);
            face = face.shadow(vec![gpui::BoxShadow {
                color,
                offset: point(px(0.), px(1.2 * k)),
                blur_radius: px((style.shadow_blur * 0.18 * k).clamp(2. * k, 5. * k)),
                spread_radius: px(0.),
            }]);
        }
        if style.has_border() {
            let color = style.border_color;
            face = if large && style.border_thickness >= 3. {
                face.border_2()
            } else {
                face.border_1()
            }
            .border_color(rgba(((color & 0xff_ffff) << 8) | (color >> 24)));
        }
        div()
            .w(px(24. * k))
            .h(px(18. * k))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4. * k))
            .bg(rgb(p.track))
            .child(face)
    }
    pub(crate) fn apply_finish_intensity(
        &mut self,
        effect: FinishEffect,
        value: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.pin_mode
            || self.processing
            || self.saving
            || !self.finish_form_available()
            || self.editor.lasso_draft.is_some()
            || self.editor.document.crop_mask.is_some()
            || !value.is_finite()
            || !(0. ..=100.).contains(&value)
        {
            return false;
        }
        if value != effect.intensity(self.finish_style) {
            self.change_finish(Change::Intensity(effect, value), cx);
        }
        true
    }
    fn finish_intensity_wheel(
        &mut self,
        effect: FinishEffect,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let delta = crate::capture_interaction::notches(event, self.wheel_settings);
        if !delta.is_finite() || delta == 0. {
            return;
        }
        let step = if event.modifiers.control {
            0.1
        } else if event.modifiers.shift {
            5.
        } else {
            1.
        };
        let value = (effect.intensity(self.finish_style) + delta * step).clamp(0., 100.);
        self.apply_finish_intensity(effect, value, cx);
    }
    /// true 表示参数已接受，包含同值确认；编辑只改会话外观，不写盘或标注历史。
    pub(crate) fn apply_finish_parameter(
        &mut self,
        field: FinishField,
        value: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        let (min, max) = field.limits();
        if self.pin_mode
            || !self.finish_form_available()
            || self.processing
            || self.saving
            || !value.is_finite()
            || !(min..=max).contains(&value)
        {
            return false;
        }
        if value != field.value(self.finish_style) {
            self.change_finish(Change::Parameter(field, value), cx);
        }
        true
    }
    fn finish_wheel(
        &mut self,
        field: FinishField,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let delta = crate::capture_interaction::notches(event, self.wheel_settings);
        if !delta.is_finite() || delta == 0. {
            return;
        }
        let multiplier = if event.modifiers.control {
            0.1
        } else if event.modifiers.shift {
            5.
        } else {
            1.
        };
        let delta = delta * multiplier;
        let style = self.finish_style.normalized();
        let (min, max) = field.limits();
        let value = (((field.value(style) + delta) * 10.).round() / 10.).clamp(min, max);
        self.apply_finish_parameter(field, value, cx);
    }
    fn finish_chip(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
        change: Change,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        self.chip(id, label, active)
            .on_click(cx.listener(move |view, _, _, cx| view.change_finish(change, cx)))
    }
    pub(crate) fn finish_properties(
        &self,
        rect: PanelRect,
        above: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let style = self.finish_style.normalized();
        let mut row = div()
            .flex()
            .flex_wrap()
            .when(above, |row| row.flex_wrap_reverse())
            .flex_shrink_0()
            .gap_1()
            .items_center()
            .child(tr!("选区", "Selection"));
        for (id, label, active, change, field) in [
            (
                "finish-corner-toggle",
                tr!("圆角", "Corners"),
                style.corner_enabled,
                Change::Corner(!style.corner_enabled),
                FinishField::Radius,
            ),
            (
                "finish-shadow-toggle",
                tr!("阴影", "Shadow"),
                style.shadow_enabled,
                Change::Mode(FinishEffect::Shadow),
                FinishField::Blur,
            ),
            (
                "finish-border-toggle",
                tr!("边框", "Border"),
                style.border_enabled,
                Change::Mode(FinishEffect::Border),
                FinishField::Thickness,
            ),
        ] {
            row = row.child(
                self.finish_chip(id, label, active, change, cx)
                    .on_scroll_wheel(cx.listener(move |view, event, _, cx| match field {
                        FinishField::Blur => {
                            view.finish_intensity_wheel(FinishEffect::Shadow, event, cx)
                        }
                        FinishField::Thickness => {
                            view.finish_intensity_wheel(FinishEffect::Border, event, cx)
                        }
                        _ => view.finish_wheel(field, event, cx),
                    })),
            );
        }
        row = row.child(
            self.chip(
                "finish-menu",
                if above {
                    tr!("外观参数 ▴", "Style options ▴")
                } else {
                    tr!("外观参数 ▾", "Style options ▾")
                },
                self.toolbar_panel == ToolbarPanel::Finish,
            )
            .on_click(cx.listener(|view, _, window, cx| {
                view.perform(Action::Panel(ToolbarPanel::Finish), window, cx)
            })),
        );
        self.panel("property-toolbar", rect)
            .p_2()
            .when(above, |panel| panel.flex().flex_col().justify_end())
            .child(row)
            .child(self.probe("property-toolbar"))
    }

    pub(crate) fn finish_form_available(&self) -> bool {
        !self.pin_mode
            && !self.processing
            && !self.saving
            && self.refresh_request.is_none()
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && self.editor.text_draft.is_none()
            && self.result_editor.is_none()
            && self.editor.local_fill_candidate().is_none()
            && !self.editor.watermark.has_preview()
            && self.pointer_down_start.is_none()
            && self.editor.lasso_draft.is_none()
            && self.editor.document.crop_mask.is_none()
    }
    fn finish_inline(
        &self,
        field: FinishField,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        div()
            .flex()
            .flex_shrink_0()
            .items_start()
            .gap(px(8.))
            .child(
                div()
                    .w(px(44.))
                    .h(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .text_size(px(11.5))
                    .text_color(rgb(p.muted))
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(crate::finish_parameter_control::control(
                        cx.entity().downgrade(),
                        field,
                        self.finish_style.normalized(),
                        p,
                        self.ui_positions.clone(),
                        self.wheel_settings,
                        enabled && self.finish_form_available(),
                    )),
            )
    }
    fn finish_segment(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        change: Change,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        crate::finish_widgets::segment(id, label, active, Palette::new(self.light_theme))
            .on_click(cx.listener(move |v, _, _, cx| {
                if v.finish_form_available() {
                    v.change_finish(change, cx);
                }
            }))
            .child(self.probe(id))
    }
    fn finish_colors(&self, border: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let value = if border {
            self.finish_style.border_color
        } else {
            self.finish_style.shadow_color
        };
        let choices = if border {
            [
                ("finish-border-white", 0xffff_ffff),
                ("finish-border-black", 0xff00_0000),
                ("finish-border-blue", 0xff42_91ff),
                ("finish-border-red", 0xffff_5368),
            ]
        } else {
            [
                ("finish-shadow-black", 0xff00_0000),
                ("finish-shadow-white", 0xffff_ffff),
                ("finish-shadow-blue", 0xff42_91ff),
                ("finish-shadow-red", 0xffff_5368),
            ]
        };
        let mut row = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .w(px(44.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("颜色", "Color")),
            );
        for (id, color) in choices {
            row = row.child(
                crate::finish_widgets::swatch(id, color, value == color, p)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if v.finish_form_available() {
                            v.change_finish(Change::Color(border, color), cx);
                        }
                    }))
                    .child(self.probe(id)),
            );
        }
        let id = if border {
            "finish-border-custom"
        } else {
            "finish-shadow-custom"
        };
        row.child(
            div()
                .id(id)
                .focusable()
                .tab_index(0)
                .relative()
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(28.))
                .px(px(7.))
                .rounded_md()
                .border_1()
                .border_color(rgb(p.border))
                .text_size(px(10.))
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(p.hover)))
                .child(
                    div()
                        .size(px(10.))
                        .rounded_sm()
                        .bg(rgba(((value & 0xff_ffff) << 8) | (value >> 24))),
                )
                .child(format!("#{:06X}", value & 0xff_ffff))
                .tooltip(|_, cx| {
                    cx.new(|_| RailHint(tr!("自定义颜色", "Custom color").into()))
                        .into()
                })
                .on_click(cx.listener(move |v, _, w, cx| {
                    if !v.finish_form_available() {
                        return;
                    }
                    let (target, current) = if border {
                        (ColorTarget::FinishBorder, v.finish_style.border_color)
                    } else {
                        (ColorTarget::FinishShadow, v.finish_style.shadow_color)
                    };
                    v.open_color_picker(target, current, w, cx);
                }))
                .child(self.probe(id)),
        )
    }
    /// 面板按当前样式实际展开的内容定高；只有显示器放不下时中段才滚动。
    /// 数值与 `finish_popup` 的行高、间距一致：参数行 36、颜色与位置行 28、行距 6。
    pub(crate) fn finish_popup_height(&self) -> f32 {
        const HEADER: f32 = 36.;
        const FOOTER: f32 = 45.;
        const PARAM: f32 = 36.;
        const ROW: f32 = 28.;
        const GAP: f32 = 6.;
        // 正文上下内边距 8 + 12。
        const PADDING: f32 = 20.;
        // 数值草稿出错时参数行下方多一行提示。
        const SLACK: f32 = 16.;
        let style = self.finish_style.normalized();
        let mut rows = Vec::new();
        if self.finish_section == FinishSection::Gallery {
            rows.extend([FINISH_TILE_HEIGHT, 1., FINISH_SUBHEAD_HEIGHT]);
        }
        if self.finish_preset == FinishPreset::Raw {
            rows.push(FINISH_RAW_HINT_HEIGHT);
        } else {
            rows.extend([PARAM, PARAM]);
            if style.shadow_enabled {
                rows.extend([PARAM, ROW]);
            }
            rows.push(PARAM);
            if style.border_enabled {
                rows.extend([ROW, ROW]);
            }
        }
        if !self.finish_form_available() {
            rows.push(32.);
        }
        let body = rows.iter().sum::<f32>() + GAP * rows.len().saturating_sub(1) as f32;
        HEADER + body + PADDING + FOOTER + SLACK
    }
    /// 画廊里一个样式：大缩略图 + 名字；悬停预览，单击换上。
    fn finish_tile(&self, look: FinishPreset, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let on = look == self.finish_preset;
        let available = self.finish_form_available();
        let id = look_id(look, true);
        div()
            .id(id)
            .flex_1()
            .min_w_0()
            .h(px(FINISH_TILE_HEIGHT))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(4.))
            .rounded_md()
            .text_size(px(11.5))
            .text_color(rgb(if on { p.accent } else { p.muted }))
            .when(on, |tile| {
                tile.bg(rgb(p.selected))
                    .border_1()
                    .border_color(rgb(p.accent))
            })
            .when(!available, |tile| tile.opacity(0.4))
            .when(available && !on, |tile| {
                tile.cursor_pointer()
                    .hover(move |s| s.bg(rgb(p.hover)).text_color(rgb(p.foreground)))
            })
            .child(self.finish_thumb(look, true))
            .child(look.label())
            .on_hover(cx.listener(move |v, hovered: &bool, _, cx| {
                v.hover_finish_look(look, *hovered, cx);
            }))
            .on_click(cx.listener(move |v, _, _, cx| v.select_finish_look(look, cx)))
            .on_scroll_wheel(cx.listener(|v, event, _, cx| v.finish_look_wheel(event, cx)))
            .child(self.probe(id))
    }
    fn finish_label(&self, label: &'static str) -> impl IntoElement {
        div()
            .w(px(44.))
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .text_size(px(11.5))
            .text_color(rgb(Palette::new(self.light_theme).muted))
            .child(label)
    }
    pub(crate) fn finish_popup(
        &self,
        rect: PanelRect,
        selected: Rect,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let style = self.finish_style.normalized();
        let p = Palette::new(self.light_theme);
        let available = self.finish_form_available();
        let look = self.finish_preset;
        let gallery = self.finish_section == FinishSection::Gallery;

        let mut body = div()
            .id("finish-popup-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pt(px(8.))
            .pb(px(12.))
            .gap(px(6.));
        if gallery {
            let mut tiles = div().flex().flex_shrink_0().gap(px(4.));
            for item in FinishPreset::ALL {
                tiles = tiles.child(self.finish_tile(item, cx));
            }
            body = body
                .child(tiles)
                .child(div().flex_shrink_0().h(px(1.)).bg(rgb(p.border)))
                .child(
                    div()
                        .flex_shrink_0()
                        .h(px(FINISH_SUBHEAD_HEIGHT))
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(trf!("调整“{}”", "Adjust {}", look.label())),
                );
        }
        if look == FinishPreset::Raw {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .h(px(FINISH_RAW_HINT_HEIGHT))
                    .text_size(px(11.5))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "原样不加任何效果。要圆角、阴影或边框，先选其它样式，或直接拖选区角上的菱形。",
                        "Plain adds no effects. Pick another style, or drag a corner diamond on the selection."
                    )),
            );
        } else {
            body = body
                .child(self.finish_inline(FinishField::Radius, tr!("圆角", "Corners"), true, cx))
                .child(self.finish_inline(FinishField::Blur, tr!("阴影", "Shadow"), true, cx));
            if style.shadow_enabled {
                body = body
                    .child(self.finish_inline(
                        FinishField::Opacity,
                        tr!("浓度", "Opacity"),
                        true,
                        cx,
                    ))
                    .child(self.finish_colors(false, cx));
            }
            body = body.child(self.finish_inline(
                FinishField::Thickness,
                tr!("边框", "Border"),
                true,
                cx,
            ));
            if style.border_enabled {
                body = body
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(8.))
                            .child(self.finish_label(tr!("位置", "Side")))
                            .child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .gap(px(2.))
                                    .p(px(2.))
                                    .rounded_md()
                                    .bg(rgb(p.hover))
                                    .child(
                                        self.finish_segment(
                                            "finish-border-outside",
                                            tr!("外侧", "Outside"),
                                            !style.border_inside,
                                            Change::Inside(false),
                                            cx,
                                        )
                                        .h(px(22.))
                                        .px(px(10.)),
                                    )
                                    .child(
                                        self.finish_segment(
                                            "finish-border-inside",
                                            tr!("内侧", "Inside"),
                                            style.border_inside,
                                            Change::Inside(true),
                                            cx,
                                        )
                                        .h(px(22.))
                                        .px(px(10.)),
                                    ),
                            ),
                    )
                    .child(self.finish_colors(true, cx));
            }
        }
        if !available {
            body = body.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "请先结束当前编辑；套索选区不支持这些效果",
                        "Finish the edit first; lasso selections do not support these effects"
                    )),
            );
        }
        let output = self
            .effective_finish_style()
            .layout(selected.width, selected.height)
            .ok()
            .map_or_else(
                || format!("{} × {}", selected.width, selected.height),
                |layout| format!("{} × {}", layout.output_width, layout.output_height),
            );
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.5))
                    .child(
                        div()
                            .text_color(rgb(p.muted))
                            .child(tr!("导出尺寸", "Export size")),
                    )
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(output)),
            )
            .when(look != FinishPreset::Raw, |footer| {
                footer.child(
                    div()
                        .id("finish-reset")
                        .focusable()
                        .tab_index(0)
                        .relative()
                        .h(px(28.))
                        .px_2()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .hover(move |s| s.bg(rgb(p.hover)).text_color(rgb(p.foreground)))
                        .child(tr!("恢复默认", "Reset"))
                        .tooltip(move |_, cx| {
                            cx.new(|_| {
                                RailHint(
                                    trf!(
                                        "把“{}”恢复为出厂外观",
                                        "Restore {} to its default look",
                                        look.label()
                                    )
                                    .into(),
                                )
                            })
                            .into()
                        })
                        .on_click(cx.listener(|v, _, _, cx| {
                            if v.finish_form_available() {
                                v.change_finish(Change::Reset, cx);
                            }
                        }))
                        .child(self.probe("finish-reset")),
                )
            });
        let title = if gallery {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(px(13.))
                        .child(tr!("选区外观", "Selection style")),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(tr!("悬停预览 · 单击应用", "Hover to preview")),
                )
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(self.finish_thumb(look, false))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(px(13.))
                        .child(look.label()),
                )
                .when(look != FinishPreset::Raw, |title| {
                    title.child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(tr!("只改这个样式", "This style only")),
                    )
                })
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .h(px(36.))
            .pl_3()
            .pr(px(4.))
            .child(title)
            .child(
                div()
                    .id("finish-close")
                    .focusable()
                    .tab_index(0)
                    .h(px(28.))
                    .w(px(28.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(p.hover)))
                    .child(crate::toolbar_icons::action(
                        crate::toolbar_icons::Icon::Close,
                        p.muted,
                    ))
                    .on_click(cx.listener(|v, _, w, cx| {
                        v.perform(Action::Panel(ToolbarPanel::None), w, cx);
                        w.focus(&v.focus);
                    })),
            );
        let positions = self.ui_positions.clone();
        let viewport = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(body.size_full())
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, _, _| {
                        let x = f32::from(bounds.center().x);
                        let mut probes = positions.borrow_mut();
                        probes.insert("finish-popup-scroll", (x, f32::from(bounds.center().y)));
                        probes.insert(
                            "finish-popup-scroll-top",
                            (f32::from(bounds.left()) + 2., f32::from(bounds.top()) + 2.),
                        );
                        probes.insert(
                            "finish-popup-scroll-bottom",
                            (
                                f32::from(bounds.left()) + 2.,
                                f32::from(bounds.bottom()) - 2.,
                            ),
                        );
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        self.panel("finish-popup", rect)
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .child(viewport)
            .child(footer)
            .child(self.probe("finish-popup"))
            .child(self.probe("toolbar-popup"))
    }

    pub(crate) fn prepare_finish_preview(&mut self, selected: Rect) -> Result<()> {
        let mut style = self.shown_finish_style();
        // remember 不改变像素，不因这个开关重新上传边带。
        style.remember_enabled = false;
        if !style.has_effect() || self.editor.is_changing_crop() {
            self.finish_preview = None;
            return Ok(());
        }
        if self.finish_preview.as_ref().is_none_or(|cache| {
            cache.style != style || cache.width != selected.width || cache.height != selected.height
        }) {
            let layout = style.layout(selected.width, selected.height)?;
            let band = layout.padding.left
                + layout.geometry.radius.ceil() as u32
                + if style.border_enabled {
                    style.border_thickness.ceil() as u32
                } else {
                    0
                }
                + 2;
            let mut shadows = Vec::new();
            let mut borders = Vec::new();
            for region in edge_regions(layout.output_width, layout.output_height, band) {
                if style.shadow_enabled {
                    let mut shadow =
                        style.shadow_region(selected.width, selected.height, region)?;
                    // AABB 内阴影在小圆角块中合成，不叠在未裁剪内容上。
                    clear_content(&mut shadow, region, layout.content_rect);
                    if let Some(part) = preview_part(region, shadow, FinishLayer::Shadow)? {
                        shadows.push(part);
                    }
                }
                if style.border_enabled {
                    let border = style.border_region(selected.width, selected.height, region)?;
                    if let Some(part) = preview_part(region, border, FinishLayer::Border)? {
                        borders.push(part);
                    }
                }
            }
            shadows.extend(borders);
            self.finish_preview = Some(FinishPreview {
                style,
                width: selected.width,
                height: selected.height,
                parts: shadows,
                corner_key: None,
            });
        }
        let key = (selected, self.editor.scene_stamp(), self.include_cursor);
        if self
            .finish_preview
            .as_ref()
            .is_some_and(|cache| cache.corner_key == Some(key))
        {
            return Ok(());
        }
        let geometry = style.geometry(selected.width, selected.height);
        let padding = style.padding();
        let mut corners = Vec::new();
        if geometry.radius > 0. {
            let scene = self.prepared_scene();
            let source = self.source_pixels();
            for local in corner_regions(selected.width, selected.height, geometry.radius) {
                let global = Rect {
                    x: selected.x + local.x,
                    y: selected.y + local.y,
                    ..local
                };
                let region = Rect {
                    x: padding.left + local.x,
                    y: padding.top + local.y,
                    ..local
                };
                // 预览读取已经冻结的 CPU 原图，不增加 staging/readback。
                let mut content = source.read(global);
                scene.try_apply(&mut content, global)?;
                let mut corner =
                    corner_content(style, selected.width, selected.height, region, &content)?;
                flatten_corner(
                    &mut corner,
                    &content,
                    geometry,
                    local,
                    global,
                    self.source.is_image(),
                );
                if let Some(part) = preview_part(region, corner, FinishLayer::Corner)? {
                    corners.push(part);
                }
            }
        }
        if let Some(cache) = &mut self.finish_preview {
            cache.parts.retain(|part| part.layer != FinishLayer::Corner);
            let index = cache
                .parts
                .iter()
                .position(|part| part.layer == FinishLayer::Border)
                .unwrap_or(cache.parts.len());
            cache.parts.splice(index..index, corners);
            cache.corner_key = Some(key);
        }
        Ok(())
    }
    /// 真实样式边框已画出时让选区轮廓退场；框选或预览失败时仍保留定位框。
    pub(crate) fn finish_has_visible_border(&self) -> bool {
        self.shown_finish_style().has_border()
            && self.finish_preview.as_ref().is_some_and(|cache| {
                cache
                    .parts
                    .iter()
                    .any(|part| part.layer == FinishLayer::Border)
            })
    }

    /// 外框跟随的圆角半径（选区像素）；圆角关闭时为 0。
    pub(crate) fn finish_frame_radius(&self, rect: Rect) -> f32 {
        let style = self.shown_finish_style();
        if style.corner_enabled {
            style.geometry(rect.width, rect.height).radius as f32
        } else {
            0.
        }
    }

    /// 选区四角的圆角控制点：指针进入选区（外扩 10 逻辑像素）时四个一起淡入，
    /// 悬停或拖动时旁边标出数值，拖动沿对角线调半径；
    /// 拖动中盖一层全窗透明层接管移动与抬起，不经过画布的选区／标注手势。
    pub(crate) fn finish_radius_knobs(
        &self,
        selected: Rect,
        window: &gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layer = div().absolute().top_0().left_0().size_full();
        let crop = self.editor.tool == crate::annotation::Tool::Crop;
        if !crop || self.pin_mode || !self.finish_form_available() {
            return layer;
        }
        let viewport = self.canvas_viewport(window);
        let ui = window.scale_factor();
        let origin = self.canvas_offset(window);
        let zoom = viewport.zoom;
        let (left, top) = (
            viewport.offset.x + selected.x as f32 * zoom,
            viewport.offset.y + selected.y as f32 * zoom,
        );
        let (width, height) = (selected.width as f32 * zoom, selected.height as f32 * zoom);
        // 太小的选区放不下四个控制点，避免盖住缩放控制柄。
        if width.min(height) < 64. * ui {
            return layer;
        }
        let radius = self.finish_frame_radius(selected) * zoom;
        // 控制点在圆弧圆心；半径很小时至少离角 14 逻辑像素，避开角上的缩放圆点。
        let inset = radius.max(14. * ui).min(width.min(height) / 2.);
        let dragging = self.finish_radius_drag.map(|drag| drag.corner);
        let to_local =
            |x: f32, y: f32| (x / ui - f32::from(origin.x), y / ui - f32::from(origin.y));
        let mouse = window.mouse_position();
        let mouse = (
            f32::from(mouse.x) - f32::from(origin.x),
            f32::from(mouse.y) - f32::from(origin.y),
        );
        // 显示区只用于四点一起淡入，不接收点击；控制点相对它定位。
        let (zx, zy) = to_local(left - 10. * ui, top - 10. * ui);
        let group = "selection-radius-zone";
        let mut zone = div()
            .absolute()
            .left(px(zx))
            .top(px(zy))
            .w(px(width / ui + 20.))
            .h(px(height / ui + 20.))
            .group(group);
        let mut label = None;
        for (corner, (cx_, cy, sx, sy)) in [
            (left, top, 1., 1.),
            (left + width, top, -1., 1.),
            (left + width, top + height, -1., -1.),
            (left, top + height, 1., -1.),
        ]
        .into_iter()
        .enumerate()
        {
            let id: &'static str = match corner {
                0 => "selection-radius-top-left",
                1 => "selection-radius-top-right",
                2 => "selection-radius-bottom-right",
                _ => "selection-radius-bottom-left",
            };
            let (kx, ky) = to_local(cx_ + sx * inset, cy + sy * inset);
            let hovered =
                dragging.is_none() && (mouse.0 - kx).abs() <= 8. && (mouse.1 - ky).abs() <= 8.;
            let state = if dragging == Some(corner as u8) {
                crate::capture_canvas::RadiusKnob::Drag
            } else if hovered {
                crate::capture_canvas::RadiusKnob::Hover
            } else {
                crate::capture_canvas::RadiusKnob::Idle
            };
            if state != crate::capture_canvas::RadiusKnob::Idle {
                label = Some(((kx, ky), (sx, sy), hovered));
            }
            zone = zone.child(
                div()
                    .id(id)
                    .absolute()
                    .left(px(kx - 8. - zx))
                    .top(px(ky - 8. - zy))
                    .size(px(16.))
                    // 与该角缩放光标同向，提示沿对角线拖动。
                    .cursor(if corner % 2 == 0 {
                        gpui::CursorStyle::ResizeUpLeftDownRight
                    } else {
                        gpui::CursorStyle::ResizeUpRightDownLeft
                    })
                    .when(dragging.is_none(), |knob| {
                        knob.opacity(0.).group_hover(group, |s| s.opacity(1.))
                    })
                    .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
                    .child(crate::capture_canvas::radius_knob(
                        state,
                        crate::selection_chrome::COLOR,
                    ))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |v, e: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            if !v.finish_form_available() {
                                return;
                            }
                            v.finish_radius_drag = Some(FinishRadiusDrag {
                                corner: corner as u8,
                                sign: (sx, sy),
                                start: v.world_point(e.position, window),
                                radius: v.finish_frame_radius(selected),
                                limit: selected.width.min(selected.height) as f32 / 2.,
                            });
                            cx.notify();
                        }),
                    )
                    .child(self.probe(id)),
            );
        }
        let mut layer = layer.child(zone);
        if let Some((knob, inward, hovering)) = label {
            let (_, max) = FinishField::Radius.limits();
            let limit = (selected.width.min(selected.height) as f32 / 2.).min(max);
            layer = layer.child(crate::capture_canvas::radius_value_pill(
                knob,
                inward,
                self.finish_frame_radius(selected),
                limit,
                hovering,
                Some(tr!("关闭圆角", "Off")),
                Palette::new(self.light_theme),
            ));
        }
        if let Some(drag) = self.finish_radius_drag {
            layer = layer.child(
                div()
                    .id("selection-radius-drag")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .cursor(if drag.corner % 2 == 0 {
                        gpui::CursorStyle::ResizeUpLeftDownRight
                    } else {
                        gpui::CursorStyle::ResizeUpRightDownLeft
                    })
                    .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, window, cx| {
                        cx.stop_propagation();
                        let Some(drag) = v.finish_radius_drag else {
                            return;
                        };
                        if e.pressed_button != Some(gpui::MouseButton::Left) {
                            v.finish_radius_drag = None;
                            cx.notify();
                            return;
                        }
                        let now = v.world_point(e.position, window);
                        // 沿对角线向内为正，取两轴位移的平均，手感与标注矩形的圆角柄一致。
                        let delta = ((now.x - drag.start.x) * drag.sign.0
                            + (now.y - drag.start.y) * drag.sign.1)
                            / 2.;
                        let (_, max) = FinishField::Radius.limits();
                        let value = (drag.radius + delta).clamp(0., drag.limit.min(max)).round();
                        v.apply_finish_parameter(FinishField::Radius, value, cx);
                    }))
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(|v, _, _, cx| {
                            cx.stop_propagation();
                            v.finish_radius_drag = None;
                            cx.notify();
                        }),
                    ),
            );
        }
        layer
    }

    pub(crate) fn finish_chrome(
        &self,
        selected: Rect,
        scale: f32,
        origin: gpui::Point<gpui::Pixels>,
    ) -> impl IntoElement {
        let style = self.shown_finish_style();
        let radius = style.geometry(selected.width, selected.height).radius as f32;
        let padding = style.padding();
        let mut layer = div().absolute().top_0().left_0().size_full();
        if radius > 0. {
            // 贝塞尔轮廓仍是矢量。小块随后替换透明边缘，不重复压暗原图。
            layer = layer.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        for (right, bottom) in
                            [(false, false), (true, false), (true, true), (false, true)]
                        {
                            let at = |x: f32, y: f32| {
                                origin
                                    + point(
                                        px((selected.x as f32
                                            + if right { selected.width as f32 - x } else { x })
                                            / scale),
                                        px((selected.y as f32
                                            + if bottom {
                                                selected.height as f32 - y
                                            } else {
                                                y
                                            })
                                            / scale),
                                    )
                            };
                            let mut path = PathBuilder::fill();
                            path.move_to(at(0., 0.));
                            path.line_to(at(radius, 0.));
                            let k = 0.552_284_8;
                            path.cubic_bezier_to(
                                at(0., radius),
                                at(radius * (1. - k), 0.),
                                at(0., radius * (1. - k)),
                            );
                            path.close();
                            if let Ok(path) = path.build() {
                                window.paint_path(path, rgba(MASK_ALPHA));
                            }
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        }
        if let Some(cache) = &self.finish_preview {
            // 阴影在 AABB 外；圆角块内部先阴影再裁剪内容；边框最后。
            for part in &cache.parts {
                let preview = part.preview.clone();
                let rect = part.rect;
                layer = layer.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            // 与底图共用精确视口，避免布局圆整后四角块错开、露出直角内容。
                            let bounds = Bounds::new(
                                origin
                                    + point(
                                        px((selected.x as f32 - padding.left as f32
                                            + rect.x as f32)
                                            / scale),
                                        px((selected.y as f32 - padding.top as f32
                                            + rect.y as f32)
                                            / scale),
                                    ),
                                size(
                                    px(rect.width as f32 / scale),
                                    px(rect.height as f32 / scale),
                                ),
                            );
                            if let Err(error) = window.paint_image_exact(
                                bounds,
                                Default::default(),
                                preview.clone(),
                                0,
                                false,
                            ) {
                                eprintln!("绘制选区外观失败：{error}");
                            }
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
            }
        }
        layer
    }
}

fn preview_part(rect: Rect, image: Image, layer: FinishLayer) -> Result<Option<FinishPart>> {
    if !image.bgra.chunks_exact(4).any(|pixel| pixel[3] != 0) {
        return Ok(None);
    }
    let pixels = image::RgbaImage::from_raw(image.width, image.height, image.bgra)
        .ok_or("选区边带像素尺寸无效")?;
    Ok(Some(FinishPart {
        rect,
        preview: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        layer,
    }))
}

/// 四条互不重叠的边带；窄选区允许覆盖短边，但每块仍不超过 GPU 纹理上限。
fn edge_regions(width: u32, height: u32, band: u32) -> Vec<Rect> {
    let top = band.min(height.div_ceil(2));
    let bottom = band.min(height - top);
    let middle = height - top - bottom;
    let left = band.min(width.div_ceil(2));
    let right = band.min(width - left);
    let mut regions = Vec::new();
    for region in [
        Rect {
            x: 0,
            y: 0,
            width,
            height: top,
        },
        Rect {
            x: 0,
            y: height - bottom,
            width,
            height: bottom,
        },
        Rect {
            x: 0,
            y: top,
            width: left,
            height: middle,
        },
        Rect {
            x: width - right,
            y: top,
            width: right,
            height: middle,
        },
    ] {
        for y in (region.y..region.y + region.height).step_by(PREVIEW_TILE_SIZE as usize) {
            for x in (region.x..region.x + region.width).step_by(PREVIEW_TILE_SIZE as usize) {
                regions.push(Rect {
                    x,
                    y,
                    width: (region.x + region.width - x).min(PREVIEW_TILE_SIZE),
                    height: (region.y + region.height - y).min(PREVIEW_TILE_SIZE),
                });
            }
        }
    }
    regions
}
fn corner_regions(width: u32, height: u32, radius: f64) -> Vec<Rect> {
    let span = radius.ceil() as u32;
    let left = span.min(width.div_ceil(2));
    let right = span.min(width - left);
    let top = span.min(height.div_ceil(2));
    let bottom = span.min(height - top);
    [
        Rect {
            x: 0,
            y: 0,
            width: left,
            height: top,
        },
        Rect {
            x: width - right,
            y: 0,
            width: right,
            height: top,
        },
        Rect {
            x: width - right,
            y: height - bottom,
            width: right,
            height: bottom,
        },
        Rect {
            x: 0,
            y: height - bottom,
            width: left,
            height: bottom,
        },
    ]
    .into_iter()
    .filter(|rect| rect.width > 0 && rect.height > 0)
    .collect()
}
fn clear_content(image: &mut Image, region: Rect, content: Rect) {
    if let Some(overlap) = region.intersection(content) {
        for y in overlap.y..overlap.y + overlap.height {
            let start = ((y - region.y) as usize * region.width as usize
                + (overlap.x - region.x) as usize)
                * 4;
            image.bgra[start..start + overlap.width as usize * 4].fill(0);
        }
    }
}

/// 与 compose 共用物理像素覆盖率及 straight-alpha 层序；边框另在上层绘制。
fn corner_content(
    style: SelectionFinishStyle,
    width: u32,
    height: u32,
    region: Rect,
    content: &Image,
) -> Result<Image> {
    let layout = style.layout(width, height)?;
    let mut result = style.shadow_region(width, height, region)?;
    for (index, (destination, source)) in result
        .bgra
        .chunks_exact_mut(4)
        .zip(content.bgra.chunks_exact(4))
        .enumerate()
    {
        let x = region.x + index as u32 % region.width - layout.padding.left;
        let y = region.y + index as u32 / region.width - layout.padding.top;
        let coverage = layout.geometry.coverage_at(x as f64 + 0.5, y as f64 + 0.5);
        let alpha = (source[3] as f64 * coverage).round() as u8;
        blend_over(destination, [source[0], source[1], source[2], alpha]);
    }
    Ok(result)
}

/// 小块替换下面未裁剪的内容，不再次合成到原来的 PNG 上。
/// 图片透明处用原图坐标的棋盘格；桌面圆角外仍展示压暗的冻结场景。
fn flatten_corner(
    image: &mut Image,
    original: &Image,
    geometry: RoundedGeometry,
    local: Rect,
    global: Rect,
    is_image: bool,
) {
    for (index, (finished, raw)) in image
        .bgra
        .chunks_exact_mut(4)
        .zip(original.bgra.chunks_exact(4))
        .enumerate()
    {
        let x = index as u32 % local.width;
        let y = index as u32 / local.width;
        let mut background = if is_image {
            let gray = if ((global.x + x) / 16 + (global.y + y) / 16).is_multiple_of(2) {
                224_u32
            } else {
                192
            };
            let coverage =
                geometry.coverage_at((local.x + x) as f64 + 0.5, (local.y + y) as f64 + 0.5);
            let shade = (MASK_ALPHA as f64 * (1. - coverage)).round() as u32;
            let gray = ((gray * (255 - shade) + 127) / 255) as u8;
            [gray, gray, gray, 255]
        } else {
            let dim = |channel: u8| {
                ((channel as u32 * raw[3] as u32 * (255 - MASK_ALPHA) + 32_512) / 65_025) as u8
            };
            [dim(raw[0]), dim(raw[1]), dim(raw[2]), 255]
        };
        blend_over(&mut background, finished.try_into().expect("BGRA pixel"));
        finished.copy_from_slice(&background);
    }
}
// 与 selection_finish::compose 的整数 source-over 相同，先合成再量化。
fn blend_over(destination: &mut [u8], source: [u8; 4]) {
    let source_alpha = source[3] as u32;
    if source_alpha == 0 {
        return;
    }
    let destination_alpha = destination[3] as u32;
    if source_alpha == 255 || destination_alpha == 0 {
        destination.copy_from_slice(&source);
        return;
    }
    let inverse = 255 - source_alpha;
    let alpha = source_alpha * 255 + destination_alpha * inverse;
    for channel in 0..3 {
        let numerator = source[channel] as u32 * source_alpha * 255
            + destination[channel] as u32 * destination_alpha * inverse;
        destination[channel] = ((numerator + alpha / 2) / alpha) as u8;
    }
    destination[3] = ((alpha + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_edge_tiles_are_bounded_disjoint_and_skip_the_middle() {
        for (width, height, band) in [(20_000, 200, 48), (200, 20_000, 48), (9, 3, 8)] {
            let regions = edge_regions(width, height, band);
            for (index, region) in regions.iter().enumerate() {
                assert!(region.valid(width, height));
                assert!(region.width <= PREVIEW_TILE_SIZE && region.height <= PREVIEW_TILE_SIZE);
                for other in &regions[index + 1..] {
                    assert!(region.intersection(*other).is_none());
                }
            }
            let area: u64 = regions
                .iter()
                .map(|rect| rect.width as u64 * rect.height as u64)
                .sum();
            let center_width = width.saturating_sub(2 * band);
            let center_height = height.saturating_sub(2 * band);
            assert_eq!(
                area,
                width as u64 * height as u64 - center_width as u64 * center_height as u64
            );
        }
    }
    #[test]
    fn corner_layer_matches_export_for_partial_alpha_and_both_border_sides() {
        let width = 11;
        let height = 9;
        for inside in [false, true] {
            let style = SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 4.,
                shadow_enabled: true,
                shadow_blur: 3.,
                shadow_opacity: 0.55,
                border_enabled: true,
                border_inside: inside,
                border_thickness: 2.,
                border_color: 0x9933_5577,
                ..SelectionFinishStyle::default()
            };
            let source = Image {
                width,
                height,
                bgra: [50, 90, 170, 97].repeat(width as usize * height as usize),
                readback_ms: 0.,
            };
            let output = style
                .compose(Image {
                    width,
                    height,
                    bgra: source.bgra.clone(),
                    readback_ms: 0.,
                })
                .unwrap();
            let padding = style.padding();
            for local in corner_regions(width, height, 4.) {
                let region = Rect {
                    x: local.x + padding.left,
                    y: local.y + padding.top,
                    ..local
                };
                let part = crate::scene::SourcePixels {
                    fill: None,
                    canvas: Rect::full(width, height),
                    planes: vec![crate::scene::PixelPlane {
                        rect: Rect::full(width, height),
                        bgra: &source.bgra,
                    }],
                    overlay: None,
                }
                .read(local);
                let mut preview = corner_content(style, width, height, region, &part).unwrap();
                let border = style.border_region(width, height, region).unwrap();
                for (pixel, top) in preview
                    .bgra
                    .chunks_exact_mut(4)
                    .zip(border.bgra.chunks_exact(4))
                {
                    blend_over(pixel, top.try_into().unwrap());
                }
                for y in 0..region.height as usize {
                    let start =
                        ((region.y as usize + y) * output.width as usize + region.x as usize) * 4;
                    assert_eq!(
                        &preview.bgra
                            [y * region.width as usize * 4..(y + 1) * region.width as usize * 4],
                        &output.bgra[start..start + region.width as usize * 4]
                    );
                }
            }
        }
    }
    #[test]
    fn tiny_odd_corner_regions_do_not_apply_coverage_twice() {
        let regions = corner_regions(3, 3, 1.5);
        assert_eq!(
            regions
                .iter()
                .map(|rect| rect.width * rect.height)
                .sum::<u32>(),
            9
        );
        for (index, region) in regions.iter().enumerate() {
            for other in &regions[index + 1..] {
                assert!(region.intersection(*other).is_none());
            }
        }
    }
}
