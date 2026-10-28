//! 数值参数的局部草稿：滑动、滚轮和输入只编辑草稿，确认时提交一次对象事务。
use crate::{
    annotation::{Style, Tool},
    capture_toolbar::Palette,
    capture_view::CaptureView,
    editor::{RadiusSelection, RectangleCornerDraft, RectangleCornerSelection},
    interaction::{Parameter, WheelAccumulator},
    text_field::{TextField, TextFieldEvent},
    toolbar_layout::ToolbarPanel,
};
use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds,
    Context, Element, ElementId, Entity, FocusHandle, GlobalElementId, IntoElement, KeyDownEvent,
    LayoutId, MouseButton, Pixels, Render, Subscription, WeakEntity, Window,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Instant};

type Positions = Rc<RefCell<HashMap<&'static str, (f32, f32)>>>;

#[cfg(test)]
mod smart_parameter_tests {
    use super::*;

    #[test]
    fn smart_block_parameter_keeps_the_interactive_tool_identity_and_strength_limits() {
        let style = Style {
            block_size: 17,
            ..Default::default()
        };
        let spec = ParameterSpec::for_style(ToolbarPanel::Block, Tool::SmartMosaic, style).unwrap();
        assert_eq!(spec.tool, Tool::SmartMosaic);
        assert_eq!(spec.value, 17.);
        assert_eq!((spec.min, spec.max), (2., 64.));
        let mut changed = style;
        spec.store(&mut changed, 24.);
        assert_eq!(changed.block_size, 24);
        assert_eq!(
            Parameter::for_tool(Tool::SmartMosaic, false, true),
            Some(Parameter::Block)
        );
        assert!(!Tool::SmartMosaic.draws());
        assert!(!Tool::SmartMosaic.effect());
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ParameterSpec {
    pub(crate) panel: ToolbarPanel,
    pub(crate) tool: Tool,
    parameter: Parameter,
    label: &'static str,
    min: f32,
    max: f32,
    value: f32,
}
impl ParameterSpec {
    pub(crate) fn for_view(panel: ToolbarPanel, view: &CaptureView) -> Option<Self> {
        if panel == ToolbarPanel::Radius {
            let selection = view.editor.radius_selection();
            return Self::for_radius_selection(&selection).map(|spec| {
                spec.with_rectangle_selection(view.editor.rectangle_corner_selection().as_ref())
            });
        }
        let tool = view
            .editor
            .selected_annotation()
            .map_or(view.editor.tool, |a| a.tool);
        Self::for_style(panel, tool, view.editor.style)
    }
    fn with_rectangle_selection(mut self, selection: Option<&RectangleCornerSelection>) -> Self {
        if let Some(selection) = selection {
            self.max = self.max.max(selection.max_radius);
        }
        self
    }
    fn for_radius_selection(selection: &RadiusSelection) -> Option<Self> {
        if selection.targets.is_empty()
            && !(selection.selected_count == 0 && selection.tool.supports_radius())
        {
            return None;
        }
        let mut style = Style::default();
        style.set_radius_for(selection.tool, selection.value);
        let mut spec = Self::for_style(ToolbarPanel::Radius, selection.tool, style)?;
        spec.max = spec.max.max(selection.max_radius);
        Some(spec)
    }
    pub(crate) fn for_style(panel: ToolbarPanel, tool: Tool, style: Style) -> Option<Self> {
        let (parameter, label, value) = match panel {
            ToolbarPanel::Radius if tool.supports_radius() => (
                Parameter::Radius,
                tr!("圆角", "Radius"),
                style.radius_for(tool),
            ),
            ToolbarPanel::Width if !matches!(tool, Tool::Crop | Tool::Edit | Tool::Lasso) => (
                Parameter::Width,
                if tool == Tool::LocalFill {
                    tr!("大小", "Size")
                } else if tool == Tool::Spotlight {
                    tr!("边框线宽", "Border width")
                } else if matches!(tool, Tool::Eraser | Tool::PixelEraser) {
                    tr!("直径", "Diameter")
                } else if matches!(tool, Tool::MosaicBrush | Tool::BlurBrush) {
                    tr!("笔径", "Brush size")
                } else {
                    tr!("线宽", "Line width")
                },
                style.width,
            ),
            ToolbarPanel::Font if tool.text() || tool == Tool::Serial => {
                (Parameter::Font, tr!("字号", "Font size"), style.font_size)
            }
            ToolbarPanel::Block
                if (tool.effect() && tool != Tool::LocalFill) || tool == Tool::SmartMosaic =>
            {
                (
                    Parameter::Block,
                    tr!("强度", "Strength"),
                    style.block_size as f32,
                )
            }
            ToolbarPanel::Opacity => (
                Parameter::Opacity,
                if tool == Tool::Spotlight {
                    tr!("暗度", "Dimming")
                } else {
                    tr!("不透明度", "Opacity")
                },
                style.opacity * 100.,
            ),
            ToolbarPanel::TextOutline if tool.text() => (
                Parameter::TextOutline,
                tr!("文字描边宽度", "Text outline width"),
                style.text.outline_width,
            ),
            ToolbarPanel::TextBackgroundAlpha if tool == Tool::Text => (
                Parameter::TextBackgroundAlpha,
                tr!("文字背景不透明度", "Text background opacity"),
                style.text.background_opacity * 100.,
            ),
            ToolbarPanel::TextBackgroundRadius if tool == Tool::Text => (
                Parameter::TextBackgroundRadius,
                tr!("文字背景圆角", "Text background radius"),
                style.text.background_radius,
            ),
            _ => return None,
        };
        if !value.is_finite() {
            return None;
        }
        let (min, max) = parameter.limits(tool)?;
        // 效果强度不随对象缩放；旧预设可存到128，但当前渲染仅支持2–64。
        // 填充笔同样展示有效的1–50px大小，不能把范围外存储值当成可绘制的参数。
        let value = if parameter == Parameter::Block || tool == Tool::LocalFill {
            value.clamp(min, max)
        } else {
            value
        };
        Some(Self {
            panel,
            tool,
            parameter,
            label,
            // 缩放后的既有参数可以超出绘制范围，单纯打开/确认不改变它。
            min: min.min(value),
            max: max.max(value),
            value,
        })
    }
    fn limits(self) -> (f32, f32) {
        self.parameter
            .limits(self.tool)
            .unwrap_or((self.min, self.max))
    }
    fn key(self) -> &'static str {
        match self.panel {
            ToolbarPanel::Radius => "parameter-radius-control",
            ToolbarPanel::Width => "parameter-width-control",
            ToolbarPanel::Font => "parameter-font-control",
            ToolbarPanel::Block => "parameter-block-control",
            ToolbarPanel::TextOutline => "parameter-text-outline-control",
            ToolbarPanel::TextBackgroundAlpha => "parameter-text-background-alpha-control",
            ToolbarPanel::TextBackgroundRadius => "parameter-text-background-radius-control",
            _ => "parameter-opacity-control",
        }
    }
    fn parse(self, text: &str) -> Result<f32, String> {
        let value = text.trim().parse::<f32>().ok().filter(|v| v.is_finite());
        match value {
            Some(value)
                if (self.min..=self.max).contains(&value)
                    && (self.parameter != Parameter::Block || value.fract() == 0.) =>
            {
                Ok(value)
            }
            _ if self.parameter == Parameter::Block => Err(trf!(
                "请输入 {}–{} 的整数",
                "Enter a whole number from {} to {}",
                self.min,
                self.max
            )),
            _ => Err(trf!(
                "请输入 {}–{} 的数值",
                "Enter a number from {} to {}",
                self.min,
                self.max
            )),
        }
    }
    fn slider_value(self, fraction: f32) -> f32 {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0., 1.)
        } else {
            0.
        };
        if fraction == 0. {
            return self.min;
        }
        if fraction == 1. {
            return self.max;
        }
        let value =
            f64::from(self.min) + f64::from(fraction) * (f64::from(self.max) - f64::from(self.min));
        let units = if self.parameter == Parameter::Block {
            1.
        } else {
            10.
        };
        ((value * units).round() / units) as f32
    }
    fn fraction(self, value: f32) -> f32 {
        ((f64::from(value) - f64::from(self.min)) / (f64::from(self.max) - f64::from(self.min)))
            .clamp(0., 1.) as f32
    }
    fn store(self, style: &mut Style, value: f32) {
        if self.read(*style) == value {
            return;
        }
        match self.parameter {
            Parameter::Radius => style.set_radius_for(self.tool, value),
            Parameter::Width => style.width = value,
            Parameter::Font => style.font_size = value,
            Parameter::Block => style.block_size = value as u32,
            Parameter::Opacity => style.opacity = value / 100.,
            Parameter::TextOutline => style.text.outline_width = value,
            Parameter::TextBackgroundAlpha => style.text.background_opacity = value / 100.,
            Parameter::TextBackgroundRadius => style.text.background_radius = value,
            _ => {}
        }
    }
    fn read(self, style: Style) -> f32 {
        match self.parameter {
            Parameter::Radius => style.radius_for(self.tool),
            Parameter::Width => style.width,
            Parameter::Font => style.font_size,
            Parameter::Block => style.block_size as f32,
            Parameter::Opacity => style.opacity * 100.,
            Parameter::TextOutline => style.text.outline_width,
            Parameter::TextBackgroundAlpha => style.text.background_opacity * 100.,
            Parameter::TextBackgroundRadius => style.text.background_radius,
            _ => self.value,
        }
    }
    fn stepped(self, value: f32, steps: i32) -> f32 {
        let mut style = Style::default();
        self.store(&mut style, value);
        self.read(self.parameter.adjusted(style, self.tool, steps))
    }
}

pub(crate) fn popup(
    owner: WeakEntity<CaptureView>,
    spec: ParameterSpec,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
) -> impl IntoElement {
    ParameterElement {
        owner,
        spec,
        palette,
        positions,
        wheel_settings,
    }
}

struct ParameterElement {
    owner: WeakEntity<CaptureView>,
    spec: ParameterSpec,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
}
impl IntoElement for ParameterElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ParameterElement {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some(self.spec.key().into())
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let owner = self.owner.clone();
        let spec = self.spec;
        let palette = self.palette;
        let positions = self.positions.clone();
        let wheel_settings = self.wheel_settings;
        let state = window.use_keyed_state(spec.key(), cx, {
            let owner = owner.clone();
            let positions = positions.clone();
            move |window, cx| {
                ParameterControl::new(owner, spec, palette, positions, wheel_settings, window, cx)
            }
        });
        // 同一帧内关闭后重开仍可能复用这个 Entity；只重建已关闭的草稿。
        if state.read(cx).closed {
            state.update(cx, |control, cx| {
                *control = ParameterControl::new(
                    owner,
                    spec,
                    palette,
                    positions,
                    wheel_settings,
                    window,
                    cx,
                );
            });
        } else if state.read(cx).palette.background != palette.background
            || state.read(cx).palette.accent != palette.accent
        {
            // 主题切换后刷新缓存的配色，避免弹窗沿用旧主题。
            state.update(cx, |control, _| control.palette = palette);
        }
        let mut element = state.into_any_element();
        (element.request_layout(window, cx), element)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        element: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

struct ParameterControl {
    owner: WeakEntity<CaptureView>,
    spec: ParameterSpec,
    radius_selection: Option<RadiusSelection>,
    rectangle_draft: Option<RectangleCornerDraft>,
    rectangle_mode: bool,
    input: Entity<TextField>,
    focus: FocusHandle,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
    wheel: WheelAccumulator,
    slider_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    dragging: bool,
    closed: bool,
    error: String,
    _input_subscription: Subscription,
    _activation_subscription: Subscription,
    _focus_subscription: Subscription,
}
fn radius_input(selection: Option<&RadiusSelection>, value: f32) -> (String, &'static str) {
    if selection.is_some_and(|s| s.mixed) {
        (
            String::new(),
            tr!("混合，输入统一值", "Mixed; enter one value for all"),
        )
    } else {
        (format_value(value), tr!("数值", "Value"))
    }
}
fn empty_mixed_radius(selection: Option<&RadiusSelection>, text: &str) -> bool {
    selection.is_some_and(|s| s.mixed) && text.trim().is_empty()
}
impl ParameterControl {
    fn new(
        owner: WeakEntity<CaptureView>,
        spec: ParameterSpec,
        palette: Palette,
        positions: Positions,
        wheel_settings: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 弹窗持有打开时的选择，不随主选/重绘更新；确认由模型校验快照再提交。
        let radius_selection = (spec.panel == ToolbarPanel::Radius)
            .then(|| {
                owner
                    .upgrade()
                    .map(|v| v.read(cx).editor.radius_selection())
            })
            .flatten();
        let rectangle_draft = (spec.panel == ToolbarPanel::Radius)
            .then(|| {
                owner
                    .upgrade()
                    .and_then(|v| v.read(cx).editor.rectangle_corner_selection())
                    .map(RectangleCornerDraft::new)
            })
            .flatten();
        let rectangle_mode = rectangle_draft
            .as_ref()
            .is_some_and(|d| d.selection.has_independent());
        let mut spec = spec;
        if let Some(d) = &rectangle_draft {
            spec.max = spec.max.max(d.selection.max_radius);
        }
        let (initial, placeholder) = if rectangle_mode {
            (
                rectangle_draft.as_ref().unwrap().texts[0].clone(),
                tr!("角半径", "Corner radius"),
            )
        } else {
            radius_input(radius_selection.as_ref(), spec.value)
        };
        let input = cx.new(|cx| TextField::new(initial, placeholder, cx));
        let group = vec![input.downgrade()];
        input.update(cx, |v, cx| v.configure_focus(group, window, cx));
        let subscription = cx.subscribe_in(&input, window, |v, _, event, window, cx| match event {
            TextFieldEvent::Changed => {
                if v.rectangle_mode {
                    let text = v.input.read(cx).text().to_owned();
                    if let Some(draft) = &mut v.rectangle_draft {
                        draft.store_text(&text);
                    }
                }
                v.error.clear();
                v.wheel.reset();
                cx.notify();
            }
            TextFieldEvent::Submit => v.apply(window, cx),
        });
        let focus = cx.focus_handle();
        let activation_subscription = cx.observe_window_activation(window, |v, window, _| {
            if !window.is_window_active() {
                v.end_drag(window);
                v.wheel.reset();
            }
        });
        let focus_subscription = cx.on_blur(&focus, window, |v, window, _| {
            v.end_drag(window);
            v.wheel.reset();
        });
        window.focus(&input.read(cx).focus_handle);
        Self {
            owner,
            spec,
            radius_selection,
            rectangle_draft,
            rectangle_mode,
            input,
            focus,
            palette,
            positions,
            wheel_settings,
            wheel: WheelAccumulator::default(),
            slider_bounds: Rc::new(RefCell::new(None)),
            dragging: false,
            closed: false,
            error: String::new(),
            _input_subscription: subscription,
            _activation_subscription: activation_subscription,
            _focus_subscription: focus_subscription,
        }
    }
    fn composing(&self, cx: &App) -> bool {
        self.input.read(cx).is_composing()
    }
    fn value(&self, cx: &App) -> f32 {
        self.spec
            .parse(self.input.read(cx).text())
            .unwrap_or_else(|_| {
                self.rectangle_draft
                    .as_ref()
                    .filter(|_| self.rectangle_mode)
                    .map_or(self.spec.value, |d| d.selection.values[d.active])
            })
    }
    fn change(&mut self, value: f32, cx: &mut Context<Self>) {
        if self.closed || self.composing(cx) || !value.is_finite() {
            return;
        }
        let value = value.clamp(self.spec.min, self.spec.max);
        self.input
            .update(cx, |v, cx| v.set_text(format_value(value), cx));
        self.error.clear();
        cx.notify();
    }
    fn at(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let bounds = *self.slider_bounds.borrow();
        if let Some(bounds) = bounds {
            let fraction = (f32::from(position.x - bounds.left())
                / f32::from(bounds.size.width).max(1.))
            .clamp(0., 1.);
            self.wheel.reset();
            self.change(self.spec.slider_value(fraction), cx);
        }
    }
    fn end_drag(&mut self, window: &Window) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        // GPUI 在鼠标按下时捕获本 HWND；取消时只释放这个窗口自己的捕获。
        // WM_CAPTURECHANGED 不进入 GPUI input 回调，不调用会重入 App 的窗口操作。
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture};
            if GetCapture() == crate::gpui_capture::hwnd(window) {
                let _ = ReleaseCapture();
            }
        }
    }
    fn drag_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closed || !self.dragging {
            return;
        }
        if event.pressed_button == Some(MouseButton::Left) {
            self.at(event.position, cx);
        } else {
            self.end_drag(window);
        }
        cx.stop_propagation();
    }
    fn drag_up(&mut self, event: &gpui::MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || !self.dragging || event.button != MouseButton::Left {
            return;
        }
        self.at(event.position, cx);
        self.end_drag(window);
        // 松在确定/快选上也只结束滑动，不把这次 up 变成另一个按钮操作。
        cx.stop_propagation();
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.end_drag(window);
        self.wheel.reset();
        self.closed = true;
        let _ = self.owner.update(cx, |v, cx| {
            if v.toolbar_panel == self.spec.panel {
                v.toolbar_panel = self.spec.panel.parent();
                if !v.pin_mode {
                    if v.editor.tool == Tool::Watermark {
                        if let Some(field) = &v.watermark_field {
                            window.focus(&field.read(cx).focus_handle);
                        } else {
                            window.focus(&v.focus);
                        }
                    } else {
                        window.focus(&v.focus);
                    }
                }
                cx.notify();
            }
        });
    }
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.composing(cx) {
            return;
        }
        if !self.rectangle_mode
            && empty_mixed_radius(self.radius_selection.as_ref(), self.input.read(cx).text())
        {
            // 混合值没有可代表全部对象的初值；未输入就确认不统一，也不写历史/预设。
            self.close(window, cx);
            return;
        }
        let rectangle_change = if self.rectangle_mode {
            let draft = self
                .rectangle_draft
                .as_mut()
                .expect("四角页具有固定矩形选择");
            draft.store_text(self.input.read(cx).text());
            match draft.changes() {
                Ok(change) => Some(change),
                Err(error) => {
                    self.error = error.to_owned();
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let value = match if rectangle_change.is_some() {
            Ok(self.spec.value)
        } else {
            self.spec.parse(self.input.read(cx).text())
        } {
            Ok(value) => value,
            Err(error) => {
                self.error = error;
                cx.notify();
                return;
            }
        };
        let mut radius_error = None;
        let result = self.owner.update(cx, |v, cx| {
            if v.saving || v.processing {
                return false;
            }
            if v.toolbar_panel != self.spec.panel {
                return false;
            }
            if let (Some(change), Some(draft)) = (rectangle_change, &self.rectangle_draft) {
                return match v.editor.apply_rectangle_corners(&draft.selection, change) {
                    Ok(_) => {
                        cx.notify();
                        true
                    }
                    Err(error) => {
                        radius_error = Some(error.to_owned());
                        false
                    }
                };
            }
            if let Some(selection) = &self.radius_selection {
                return match v.editor.apply_radius(selection, value) {
                    Ok(_) => {
                        cx.notify();
                        true
                    }
                    Err(error) => {
                        radius_error = Some(error.to_owned());
                        false
                    }
                };
            }
            let tool = v
                .editor
                .selected_annotation()
                .map_or(v.editor.tool, |a| a.tool);
            if v.toolbar_panel != self.spec.panel || tool != self.spec.tool {
                return false;
            }
            let mut style = v.editor.style;
            self.spec.store(&mut style, value);
            if style != v.editor.style {
                if v.editor.tool == Tool::SmartMosaic {
                    return v.apply_smart_mosaic_style(style, cx);
                }
                v.editor.set_style(style);
            }
            cx.notify();
            true
        });
        if result.unwrap_or(false) {
            self.close(window, cx);
        } else if let Some(error) = radius_error {
            self.error = error;
            cx.notify();
        }
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        // 数字框确认返回父页后，长按终端键不能继续确认或关闭下一层。
        if crate::text_contract::terminal_held(event.keystroke.key.as_str(), event.is_held) {
            cx.stop_propagation();
            return;
        }
        if self.closed {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.close(window, cx),
            "enter" => self.apply(window, cx),
            "left" | "down" if self.focus.is_focused(window) => {
                self.wheel.reset();
                self.change(self.spec.stepped(self.value(cx), -1), cx);
            }
            "right" | "up" if self.focus.is_focused(window) => {
                self.wheel.reset();
                self.change(self.spec.stepped(self.value(cx), 1), cx);
            }
            "home" if self.focus.is_focused(window) => {
                self.wheel.reset();
                self.change(self.spec.min, cx);
            }
            "end" if self.focus.is_focused(window) => {
                self.wheel.reset();
                self.change(self.spec.max, cx);
            }
            // 字符仍走 GPUI/IME 原生输入；宿主在参数面板打开时屏蔽绘图快捷键。
            _ => return,
        }
        cx.stop_propagation();
    }
    fn wheel(&mut self, event: &gpui::ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.closed && !self.composing(cx) {
            let amount = crate::capture_interaction::notches(event, self.wheel_settings);
            let (steps, _) = self.wheel.push(self.spec.parameter, amount, Instant::now());
            if steps != 0 {
                self.change(self.spec.stepped(self.value(cx), steps), cx);
            }
        }
        cx.stop_propagation();
    }
    fn slider(&self, cx: &mut Context<Self>) -> AnyElement {
        let bounds = self.slider_bounds.clone();
        let positions = self.positions.clone();
        let view = cx.entity().downgrade();
        let palette = self.palette;
        let fraction = self.spec.fraction(self.value(cx));
        div()
            .id("parameter-slider")
            .relative()
            .w_full()
            .h(px(18.))
            .flex_shrink_0()
            .cursor_pointer()
            .on_scroll_wheel(cx.listener(Self::wheel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    if !v.closed && !v.composing(cx) {
                        w.focus(&v.focus);
                        v.dragging = true;
                        v.at(e.position, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |rect, _, window, _| {
                        *bounds.borrow_mut() = Some(rect);
                        let y = f32::from(rect.center().y);
                        let mut probes = positions.borrow_mut();
                        probes.insert("parameter-slider-start", (f32::from(rect.left()), y));
                        probes.insert("parameter-slider-end", (f32::from(rect.right()), y));
                        drop(probes);
                        // 每帧只绑定当前窗口。GPUI 原生捕获继续提供窗外坐标，
                        // Capture-phase 避免离开滑条后被其它控件截走 move/up。
                        let weak = view.clone();
                        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak.update(cx, |v, cx| v.drag_move(event, w, cx));
                            }
                        });
                        let weak = view.clone();
                        window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak.update(cx, |v, cx| v.drag_up(event, w, cx));
                            }
                        });
                        let track = Bounds::new(
                            point(rect.left(), rect.center().y - px(2.)),
                            size(rect.size.width, px(4.)),
                        );
                        window
                            .paint_quad(fill(track, rgb(palette.track_hover)).corner_radii(px(2.)));
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    track.origin,
                                    size(track.size.width * fraction, px(4.)),
                                ),
                                rgb(palette.accent),
                            )
                            .corner_radii(px(2.)),
                        );
                        let knob = point(
                            rect.left() + px(7.) + (rect.size.width - px(14.)) * fraction,
                            rect.center().y,
                        );
                        window.paint_quad(gpui::quad(
                            Bounds::new(
                                point(knob.x - px(7.), knob.y - px(7.)),
                                size(px(14.), px(14.)),
                            ),
                            px(7.),
                            rgb(0xffffff),
                            px(2.),
                            rgb(palette.accent),
                            gpui::BorderStyle::Solid,
                        ));
                    },
                )
                .size_full(),
            )
            .child(probe("parameter-slider", self.positions.clone()))
            .into_any_element()
    }
    fn button(&self, id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
        let p = self.palette;
        div()
            .id(id)
            .relative()
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .bg(rgb(p.track))
            .hover(move |s| s.bg(rgb(p.track_hover)))
            .cursor_pointer()
            .child(label)
            .child(probe(id, self.positions.clone()))
    }
    fn corner_controls(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.rectangle_draft.as_ref()?;
        let linked = !self.rectangle_mode || draft.linked;
        let mut row = div()
            .flex()
            .flex_wrap()
            .gap(px(4.))
            .flex_shrink_0()
            .items_center();
        row = row.child(
            self.button(
                "rectangle-radius-link",
                if linked {
                    tr!("分离四角", "Separate")
                } else {
                    tr!("四角联动", "Link")
                },
            )
            .on_click(cx.listener(|v, _, _, cx| {
                if v.closed || v.composing(cx) {
                    return;
                }
                v.wheel.reset();
                let text = v.input.read(cx).text().to_owned();
                if let Some(draft) = &mut v.rectangle_draft {
                    if v.rectangle_mode {
                        draft.store_text(&text);
                    }
                    draft.set_linked(v.rectangle_mode && !draft.linked);
                    v.rectangle_mode = true;
                    let text = draft.texts[draft.active].clone();
                    v.input.update(cx, |field, cx| field.set_text(text, cx));
                }
                v.error.clear();
                cx.notify();
            })),
        );
        if self.rectangle_mode && !draft.linked {
            for (corner, id, label) in [
                (0, "rectangle-radius-tl", tr!("左上", "TL")),
                (1, "rectangle-radius-tr", tr!("右上", "TR")),
                (2, "rectangle-radius-br", tr!("右下", "BR")),
                (3, "rectangle-radius-bl", tr!("左下", "BL")),
            ] {
                row = row.child(
                    self.button(id, label)
                        .when(draft.active == corner, |b| b.bg(rgb(self.palette.selected)))
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if v.closed || v.composing(cx) {
                                return;
                            }
                            v.wheel.reset();
                            let text = v.input.read(cx).text().to_owned();
                            if let Some(draft) = &mut v.rectangle_draft {
                                draft.store_text(&text);
                                draft.active = corner;
                                let text = draft.texts[corner].clone();
                                v.input.update(cx, |field, cx| field.set_text(text, cx));
                            }
                            v.error.clear();
                            cx.notify();
                        })),
                );
            }
        }
        Some(row.into_any_element())
    }
    fn presets(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.rectangle_mode {
            return None;
        }
        let values: &[(&'static str, &'static str, f32)] = match self.spec.panel {
            ToolbarPanel::Width if self.spec.tool == Tool::LocalFill => &[
                ("fill1", "1", 1.),
                ("fill5", "5", 5.),
                ("fill10", "10", 10.),
                ("fill25", "25", 25.),
                ("fill50", "50", 50.),
            ],
            ToolbarPanel::Width if self.spec.tool == Tool::PixelEraser => &[
                ("erase4", "4", 4.),
                ("erase8", "8", 8.),
                ("erase16", "16", 16.),
                ("erase32", "32", 32.),
                ("erase64", "64", 64.),
            ],
            ToolbarPanel::Width => &[
                ("thin", "2", 2.),
                ("medium", "4", 4.),
                ("thick", "8", 8.),
                ("brush", "24", 24.),
                ("largebrush", "48", 48.),
            ],
            ToolbarPanel::Font => &[
                ("font18", "18", 18.),
                ("font28", "28", 28.),
                ("font40", "40", 40.),
                ("font56", "56", 56.),
            ],
            ToolbarPanel::Block => &[
                ("block8", "8", 8.),
                ("block16", "16", 16.),
                ("block32", "32", 32.),
            ],
            ToolbarPanel::Radius => &[
                ("corner0", tr!("直角", "Square"), 0.),
                ("corner8", "8", 8.),
                ("corner20", "20", 20.),
            ],
            ToolbarPanel::Opacity => &[
                ("alpha35", "35%", 35.),
                ("alpha65", "65%", 65.),
                ("alpha100", "100%", 100.),
            ],
            ToolbarPanel::TextOutline => &[
                ("outline-1", "1", 1.),
                ("outline-3", "3", 3.),
                ("outline-6", "6", 6.),
                ("outline-12", "12", 12.),
            ],
            ToolbarPanel::TextBackgroundAlpha => &[
                ("background-alpha35", "35%", 35.),
                ("background-alpha65", "65%", 65.),
                ("background-alpha100", "100%", 100.),
            ],
            ToolbarPanel::TextBackgroundRadius => &[
                ("background-radius0", "0", 0.),
                ("background-radius8", "8", 8.),
                ("background-radius20", "20", 20.),
            ],
            _ => return None,
        };
        let value = self.value(cx);
        let known_value =
            !empty_mixed_radius(self.radius_selection.as_ref(), self.input.read(cx).text());
        let mut row = div().flex().gap(px(4.)).flex_shrink_0();
        for &(id, label, preset) in values {
            let (min, max) = self.spec.limits();
            if !(min..=max).contains(&preset) {
                continue;
            }
            let on = known_value && (value - preset).abs() < 0.01;
            row = row.child(
                self.button(id, label)
                    .flex_1()
                    .min_w(px(0.))
                    .when(on, |b| {
                        b.bg(rgb(self.palette.selected))
                            .text_color(rgb(self.palette.accent))
                    })
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if !v.closed && !v.composing(cx) {
                            v.wheel.reset();
                            v.change(preset, cx);
                            v.apply(w, cx);
                        }
                    })),
            );
        }
        Some(row.into_any_element())
    }
}
impl Render for ParameterControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let presets = self.presets(cx);
        let corners = self.corner_controls(cx);
        let mut label = self
            .radius_selection
            .as_ref()
            .filter(|s| s.selected_count > 1)
            .map_or_else(
                || self.spec.label.to_owned(),
                |s| {
                    trf!(
                        "圆角 · 适用 {}/{}{}",
                        "Radius · applies to {}/{}{}",
                        s.targets.len(),
                        s.selected_count,
                        if s.mixed {
                            tr!(" · 混合", " · mixed")
                        } else {
                            ""
                        }
                    )
                },
            );
        if self.rectangle_mode {
            if let Some(draft) = &self.rectangle_draft {
                let count = draft.selection.count();
                label = if count > 0 {
                    trf!(
                        "矩形圆角 · 适用 {count} 个",
                        "Rectangle corners · {count} objects"
                    )
                } else {
                    tr!("矩形圆角 · 下一笔", "Rectangle corners · next shape").to_owned()
                };
            }
        }
        let p = self.palette;
        div()
            .id("parameter-control")
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .gap(px(10.))
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_scroll_wheel(cx.listener(Self::wheel))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .min_h(px(26.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(p.muted))
                            .child(label),
                    )
                    .child(
                        div()
                            .id("parameter-value")
                            .relative()
                            .w(px(72.))
                            .flex_shrink_0()
                            .on_scroll_wheel(cx.listener(Self::wheel))
                            .child(self.input.clone())
                            .child(probe("parameter-value", self.positions.clone())),
                    ),
            )
            .child(self.slider(cx))
            .when_some(corners, |view, corners| view.child(corners))
            .when_some(presets, |view, presets| view.child(presets))
            .child(
                div()
                    .min_h(px(14.))
                    .flex_shrink_0()
                    .text_size(px(11.5))
                    .text_color(rgb(if self.error.is_empty() {
                        p.muted
                    } else {
                        p.danger
                    }))
                    .child(if self.error.is_empty() {
                        tr!(
                            "在栏上滚轮也能调，Shift 加快。",
                            "Scroll on the bar to adjust; Shift is faster."
                        )
                        .to_owned()
                    } else {
                        self.error.clone()
                    }),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.))
                    .flex_shrink_0()
                    .child(
                        crate::toolbar_widgets::text_button(
                            "parameter-cancel",
                            tr!("取消", "Cancel"),
                            false,
                            true,
                            p,
                        )
                        .child(probe("parameter-cancel", self.positions.clone()))
                        .on_click(cx.listener(|v, _, w, cx| {
                            if !v.composing(cx) {
                                v.close(w, cx);
                            }
                        })),
                    )
                    .child(
                        crate::toolbar_widgets::text_button(
                            "parameter-apply",
                            tr!("确定", "OK"),
                            true,
                            true,
                            p,
                        )
                        .child(probe("parameter-apply", self.positions.clone()))
                        .on_click(cx.listener(Self::apply_click)),
                    ),
            )
    }
}
impl ParameterControl {
    fn apply_click(&mut self, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.apply(window, cx);
    }
}
fn probe(id: &'static str, positions: Positions) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, _, _| {
            positions.borrow_mut().insert(
                id,
                (
                    f32::from(bounds.origin.x + bounds.size.width / 2.),
                    f32::from(bounds.origin.y + bounds.size.height / 2.),
                ),
            );
        },
    )
    .absolute()
    .left(px(0.))
    .top(px(0.))
    .size_full()
    .into_any_element()
}
fn format_value(value: f32) -> String {
    // 与 f32 round-trip 相同的有效数字，不隐藏用户输入的小数精度。
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_fill_parameter_uses_size_popup_without_invisible_effect_strength() {
        let style = crate::tool_presets::default_style(Tool::LocalFill);
        let spec = ParameterSpec::for_style(ToolbarPanel::Width, Tool::LocalFill, style).unwrap();
        assert_eq!(
            (spec.label, spec.min, spec.max, spec.value),
            ("大小", 1., 50., 10.)
        );
        assert_eq!(spec.parse("1"), Ok(1.));
        assert_eq!(spec.parse("17.5"), Ok(17.5));
        assert_eq!(spec.parse("50"), Ok(50.));
        assert!(spec.parse("0.99").is_err());
        assert!(spec.parse("50.1").is_err());
        assert_eq!(spec.stepped(10., 1), 11.);
        assert_eq!(spec.stepped(1., -1), 1.);
        assert_eq!(spec.stepped(50., 1), 50.);
        assert!(ParameterSpec::for_style(ToolbarPanel::Block, Tool::LocalFill, style).is_none());
        let outside = ParameterSpec::for_style(
            ToolbarPanel::Width,
            Tool::LocalFill,
            Style {
                width: 80.,
                ..style
            },
        )
        .unwrap();
        assert_eq!((outside.min, outside.max, outside.value), (1., 50., 50.));
    }
    fn radius_bulk_editor() -> crate::editor::Editor {
        use crate::{
            annotation::{Annotation, Point},
            geometry::Rect,
        };
        let area = Rect::full(1000, 700);
        let mut editor = crate::editor::Editor::new(area, Some(area));
        let mut ids = Vec::new();
        for (tool, radius) in [
            (Tool::Rectangle, 17.25),
            (Tool::Spotlight, 80.5),
            (Tool::Arrow, 31.),
        ] {
            let mut a = Annotation::new(
                tool,
                Point { x: 100., y: 120. },
                Style {
                    radius,
                    ..Default::default()
                },
            );
            a.update(Point { x: 500., y: 420. });
            ids.push(editor.document.insert(a));
        }
        editor.select_tool(Tool::Edit);
        editor.selected = ids.last().copied();
        editor.selection = ids;
        editor.style = editor.selected_annotation().unwrap().style;
        editor
    }
    #[test]
    fn radius_bulk_ui_mixed_supported_scope_overrides_unsupported_primary_and_keeps_scaled_range() {
        let editor = radius_bulk_editor();
        assert_eq!(editor.selected_annotation().unwrap().tool, Tool::Arrow);
        let selection = editor.radius_selection();
        assert_eq!((selection.targets.len(), selection.selected_count), (2, 3));
        assert!(selection.mixed);
        let spec = ParameterSpec::for_radius_selection(&selection).unwrap();
        assert_eq!(spec.tool, Tool::Rectangle);
        assert_eq!(spec.value, 17.25);
        assert_eq!(spec.parse("80.5").unwrap(), 80.5);
        assert!(spec.parse("80.6").is_err());
        assert_eq!(
            crate::toolbar_layout::parameter_popup_size(spec.panel, spec.tool),
            Some((300., 232.)),
        );
        let (text, placeholder) = radius_input(Some(&selection), spec.value);
        assert!(text.is_empty());
        assert!(placeholder.contains("混合"));
    }
    #[test]
    fn radius_bulk_ui_zero_support_is_unavailable_and_mixed_blank_differs_from_explicit_seed() {
        use crate::annotation::{Annotation, Point};
        let mut editor = radius_bulk_editor();
        let mixed = editor.radius_selection();
        assert!(empty_mixed_radius(Some(&mixed), "  "));
        // 输入主选可用对象的旧数值是显式统一，不能和未输入就确认混为一谈。
        assert!(!empty_mixed_radius(
            Some(&mixed),
            &format_value(mixed.value)
        ));
        let arrow = editor.selected.unwrap();
        let ellipse = editor.document.insert(Annotation::new(
            Tool::Ellipse,
            Point { x: 1., y: 1. },
            Style::default(),
        ));
        editor.selection = vec![arrow, ellipse];
        let unsupported = editor.radius_selection();
        assert_eq!(unsupported.selected_count, 2);
        assert!(unsupported.targets.is_empty());
        assert!(ParameterSpec::for_radius_selection(&unsupported).is_none());
        assert!(!empty_mixed_radius(Some(&unsupported), ""));
        editor.select_tool(Tool::Rectangle);
        let drawing = editor.radius_selection();
        assert_eq!(drawing.selected_count, 0);
        assert!(ParameterSpec::for_radius_selection(&drawing).is_some());
        assert!(!radius_input(Some(&drawing), drawing.value).0.is_empty());
    }
    #[test]
    fn finish_text_parameter_specs_preserve_scaled_fields_and_percent_precision() {
        let mut style = Style::default();
        style.text.outline_width = 17.25;
        style.text.background_radius = 77.5;
        style.text.background_opacity = 0.334572;
        for (panel, value, min, max) in [
            (ToolbarPanel::TextOutline, 17.25, 1., 17.25),
            (ToolbarPanel::TextBackgroundRadius, 77.5, 0., 77.5),
            (
                ToolbarPanel::TextBackgroundAlpha,
                style.text.background_opacity * 100.,
                0.,
                100.,
            ),
        ] {
            let spec = ParameterSpec::for_style(panel, Tool::Text, style).unwrap();
            assert_eq!((spec.min, spec.max, spec.value), (min, max, value));
            let mut unchanged = style;
            spec.store(&mut unchanged, spec.parse(&format_value(value)).unwrap());
            assert_eq!(unchanged, style);
            // 描边粗细回到描边细项，底色不透明度与圆角回到底色细项。
            assert_eq!(
                panel.parent(),
                if panel == ToolbarPanel::TextOutline {
                    ToolbarPanel::TextOutlineStyle
                } else {
                    ToolbarPanel::TextBackgroundStyle
                }
            );
            assert!(ParameterSpec::for_style(panel, Tool::Serial, style).is_none());
            assert_eq!(
                crate::toolbar_layout::parameter_popup_size(panel, Tool::Text),
                Some((300., 232.))
            );
        }
        assert!(
            ParameterSpec::for_style(ToolbarPanel::TextOutline, Tool::Watermark, style).is_some()
        );
        assert!(ParameterSpec::for_style(
            ToolbarPanel::TextBackgroundRadius,
            Tool::Watermark,
            style
        )
        .is_none());
        let radius =
            ParameterSpec::for_style(ToolbarPanel::TextBackgroundRadius, Tool::Text, style)
                .unwrap();
        assert_eq!(radius.stepped(77.5, -1), 76.5);
    }

    #[test]
    fn highlight_radius_numeric_slider_and_presets_share_the_shape_parameter() {
        let style = Style {
            radius: 17.25,
            opacity: 0.35,
            ..Style::default()
        };
        let spec =
            ParameterSpec::for_style(ToolbarPanel::Radius, Tool::HighlighterRect, style).unwrap();
        assert_eq!((spec.min, spec.max, spec.value), (0., 50., 17.25));
        assert_eq!(spec.parse("17.25"), Ok(17.25));
        assert_eq!(spec.slider_value(0.35), 17.5);
        assert_eq!(spec.stepped(17.25, 1), 18.25);
        for value in [0., 8., 20.] {
            let mut changed = style;
            spec.store(&mut changed, value);
            assert_eq!(changed.radius, value);
            assert_eq!(changed.opacity, style.opacity);
        }
        for tool in [Tool::Ellipse, Tool::Highlighter, Tool::Mosaic] {
            assert!(ParameterSpec::for_style(ToolbarPanel::Radius, tool, style).is_none());
        }
    }

    #[test]
    fn parameter_spec_uses_tool_limits_and_supports_precision_for_width_and_font() {
        let spotlight =
            ParameterSpec::for_style(ToolbarPanel::Width, Tool::Spotlight, Style::default())
                .unwrap();
        assert_eq!((spotlight.min, spotlight.max), (0., 20.));
        assert_eq!(spotlight.parse("0"), Ok(0.));
        assert!(spotlight.parse("20.1").is_err());
        let width =
            ParameterSpec::for_style(ToolbarPanel::Width, Tool::Pen, Style::default()).unwrap();
        assert_eq!(width.parse(" 5.125 "), Ok(5.125));
        let font =
            ParameterSpec::for_style(ToolbarPanel::Font, Tool::Text, Style::default()).unwrap();
        assert_eq!((font.min, font.max), (8., 72.));
        assert_eq!(font.parse("23.75"), Ok(23.75));
        assert!(
            ParameterSpec::for_style(ToolbarPanel::Font, Tool::Rectangle, Style::default())
                .is_none()
        );
        assert!(
            ParameterSpec::for_style(ToolbarPanel::Block, Tool::Rectangle, Style::default())
                .is_none()
        );
    }

    #[test]
    fn parameter_spec_rejects_nonfinite_and_fractional_effect_strength() {
        let block =
            ParameterSpec::for_style(ToolbarPanel::Block, Tool::Mosaic, Style::default()).unwrap();
        for valid in ["2", "3", "64", "16.0"] {
            assert!(block.parse(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "NaN", "inf", "-inf", "1", "2.5", "65"] {
            assert!(block.parse(invalid).is_err(), "{invalid}");
        }
        let legacy = ParameterSpec::for_style(
            ToolbarPanel::Block,
            Tool::Mosaic,
            Style {
                block_size: 128,
                ..Style::default()
            },
        )
        .unwrap();
        assert_eq!((legacy.min, legacy.max, legacy.value), (2., 64., 64.));
        assert!(legacy.parse("128").is_err());
        let mut stored = Style {
            block_size: 128,
            ..Style::default()
        };
        legacy.store(&mut stored, legacy.value);
        assert_eq!(stored.block_size, 64);
        assert!(ParameterSpec::for_style(
            ToolbarPanel::Width,
            Tool::Pen,
            Style {
                width: f32::NAN,
                ..Style::default()
            }
        )
        .is_none());
        assert!(ParameterSpec::for_style(
            ToolbarPanel::Opacity,
            Tool::Rectangle,
            Style {
                opacity: f32::INFINITY,
                ..Style::default()
            }
        )
        .is_none());
    }

    #[test]
    fn parameter_spec_preserves_scaled_values_and_moves_towards_normal_range() {
        let style = Style {
            width: 84.125,
            font_size: 112.,
            radius: 63.75,
            ..Style::default()
        };
        for (panel, tool, value) in [
            (ToolbarPanel::Width, Tool::Pen, 84.125),
            (ToolbarPanel::Font, Tool::Text, 112.),
            (ToolbarPanel::Radius, Tool::Rectangle, 63.75),
        ] {
            let spec = ParameterSpec::for_style(panel, tool, style).unwrap();
            assert_eq!(spec.parse(&format_value(value)), Ok(value));
            assert_eq!(spec.slider_value(1.), value);
            assert_eq!(spec.stepped(value, 1), value);
            assert_eq!(spec.stepped(value, -1), value - 1.);
            let mut unchanged = style;
            spec.store(&mut unchanged, spec.value);
            assert_eq!(unchanged, style);
        }
        let small = ParameterSpec::for_style(
            ToolbarPanel::Width,
            Tool::Pen,
            Style {
                width: 0.125,
                ..style
            },
        )
        .unwrap();
        assert_eq!(small.slider_value(0.), 0.125);
        assert_eq!(small.stepped(0.125, -1), 0.125);
        assert_eq!(small.stepped(0.125, 1), 1.125);
    }

    #[test]
    fn parameter_slider_maps_clamped_edges_and_integer_effect_steps() {
        let radius =
            ParameterSpec::for_style(ToolbarPanel::Radius, Tool::Rectangle, Style::default())
                .unwrap();
        assert_eq!(radius.slider_value(-0.5), 0.);
        assert_eq!(radius.slider_value(1.5), 50.);
        assert!((radius.slider_value(0.316) - 15.8).abs() < 0.001);
        assert_eq!(radius.fraction(25.), 0.5);
        let block =
            ParameterSpec::for_style(ToolbarPanel::Block, Tool::BlurBrush, Style::default())
                .unwrap();
        assert_eq!(block.slider_value(0.25), 18.);
        assert_eq!(block.slider_value(0.), 2.);
        assert_eq!(block.slider_value(1.), 64.);
        assert_eq!(block.slider_value(0.123).fract(), 0.);
    }

    #[test]
    fn unchanged_opacity_confirmation_does_not_roundtrip_the_original_style() {
        for opacity in [0.12345678, 0.67890125, 0.9345878] {
            let style = Style {
                opacity,
                ..Style::default()
            };
            let spec =
                ParameterSpec::for_style(ToolbarPanel::Opacity, Tool::Spotlight, style).unwrap();
            let mut result = style;
            spec.store(&mut result, spec.parse(&format_value(spec.value)).unwrap());
            assert_eq!(result, style);
        }
    }

    #[test]
    fn popup_wheel_uses_canvas_direction_and_fractional_step_semantics() {
        let style = Style {
            width: 4.25,
            opacity: 0.504,
            ..Style::default()
        };
        let width = ParameterSpec::for_style(ToolbarPanel::Width, Tool::Pen, style).unwrap();
        let opacity =
            ParameterSpec::for_style(ToolbarPanel::Opacity, Tool::Spotlight, style).unwrap();
        assert_eq!(width.stepped(4.25, 1), 5.25);
        assert_eq!(opacity.stepped(50.4, 1), 51.);
        let mut wheel = WheelAccumulator::default();
        let now = Instant::now();
        assert_eq!(wheel.push(width.parameter, 0.5, now).0, 0);
        assert_eq!(wheel.push(width.parameter, -0.5, now).0, 0);
        let steps = wheel.push(width.parameter, -0.5, now).0;
        assert_eq!(width.stepped(4.25, steps), 3.25);
    }

    #[test]
    fn effect_radius_parameter_spec_routes_numeric_slider_and_wheel_without_legacy_change() {
        for tool in [Tool::Mosaic, Tool::Blur] {
            let mut style = Style {
                radius: 8.,
                effect_radius: 17.25,
                ..Default::default()
            };
            let spec = ParameterSpec::for_style(ToolbarPanel::Radius, tool, style).unwrap();
            assert_eq!(spec.value, 17.25);
            assert_eq!(spec.parse("17.5"), Ok(17.5));
            assert!(spec.parse("NaN").is_err());
            let chosen = spec.slider_value(0.25);
            assert_eq!(chosen, 12.5);
            spec.store(&mut style, chosen);
            assert_eq!(spec.read(style), 12.5);
            assert_eq!(style.effect_radius, 12.5);
            assert_eq!(style.radius, 8.);
            let stepped = spec.stepped(spec.read(style), 3);
            assert_eq!(stepped, 15.5);
            spec.store(&mut style, stepped);
            assert_eq!(style.effect_radius, 15.5);
            assert_eq!(style.radius, 8.);
        }
        let style = Style {
            radius: 8.,
            effect_radius: 17.25,
            ..Default::default()
        };
        let spec = ParameterSpec::for_style(ToolbarPanel::Radius, Tool::Rectangle, style).unwrap();
        assert_eq!(spec.value, 8.);
        let mut changed = style;
        spec.store(&mut changed, 20.);
        assert_eq!(changed.radius, 20.);
        assert_eq!(changed.effect_radius, 17.25);
    }

    #[test]
    fn effect_radius_parameter_selection_uses_effect_field_and_rejects_compound_target() {
        use crate::{
            annotation::{Annotation, Bounds, Point},
            editor::Editor,
            geometry::Rect,
        };
        let area = Rect::full(900, 700);
        let mut editor = Editor::new(area, Some(area));
        let mut a = Annotation::new(
            Tool::Mosaic,
            Point { x: 100., y: 120. },
            Style {
                radius: 8.,
                effect_radius: 17.25,
                ..Default::default()
            },
        );
        a.update(Point { x: 400., y: 300. });
        let id = editor.document.insert(a.clone());
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        editor.style = a.style;
        let selection = editor.radius_selection();
        let spec = ParameterSpec::for_radius_selection(&selection).unwrap();
        assert_eq!(spec.tool, Tool::Mosaic);
        assert_eq!(spec.value, 17.25);
        let value = spec.parse("20").unwrap();
        assert_eq!(editor.apply_radius(&selection, value), Ok(true));
        assert_eq!(editor.document.get(id).unwrap().style.effect_radius, 20.);
        assert_eq!(editor.document.get(id).unwrap().style.radius, 8.);
        a.set_effect_regions(
            vec![Bounds {
                x: 100.,
                y: 120.,
                width: 300.,
                height: 180.,
            }],
            true,
        )
        .unwrap();
        editor.document.replace(id, a);
        let excluded = editor.radius_selection();
        assert_eq!(excluded.selected_count, 1);
        assert!(excluded.targets.is_empty());
        assert!(ParameterSpec::for_radius_selection(&excluded).is_none());
    }
}

#[cfg(test)]
mod rectangle_corner_spec_tests {
    use super::*;
    #[test]
    fn rectangle_corner_spec_preserves_scaled_per_corner_range_and_exact_input_slider_values() {
        use crate::{
            annotation::{Annotation, Point},
            editor::Editor,
            geometry::Rect,
        };
        let canvas = Rect::full(400, 300);
        let mut e = Editor::new(canvas, Some(canvas));
        let style = Style {
            radius: 3.,
            rectangle_radii: Some([3., 8., 8192., 22.]),
            ..Default::default()
        };
        let mut a = Annotation::new(Tool::Rectangle, Point { x: 20., y: 30. }, style);
        a.update(Point { x: 250., y: 200. });
        let id = e.document.insert(a);
        e.select_tool(Tool::Edit);
        e.selected = Some(id);
        e.style = style;
        let radius = e.radius_selection();
        let corners = e.rectangle_corner_selection().unwrap();
        let spec = ParameterSpec::for_radius_selection(&radius)
            .unwrap()
            .with_rectangle_selection(Some(&corners));
        assert_eq!((spec.min, spec.max, spec.value), (0., 8192., 3.));
        assert_eq!(spec.parse("8192"), Ok(8192.));
        assert!(spec.parse("8193").is_err());
        assert!(spec.parse("NaN").is_err());
        assert_eq!(spec.slider_value(0.), 0.);
        assert_eq!(spec.slider_value(1.), 8192.);
        let mut stored = style;
        spec.store(&mut stored, 3.);
        assert_eq!(stored, style);
        spec.store(&mut stored, 4.);
        assert_eq!(stored.rectangle_radii, Some([4.; 4]));
        assert_eq!(stored.effect_radius, style.effect_radius);
    }
}
