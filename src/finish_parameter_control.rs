//! 选区外观的内联滑条和数值输入；有效值实时应用，无效草稿留在输入框。
use crate::{
    capture_toolbar::Palette,
    capture_view::CaptureView,
    selection_finish::{FinishField, SelectionFinishStyle},
    text_field::{TextField, TextFieldEvent},
    toolbar_layout::ToolbarPanel,
};
use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds,
    Context, Element, ElementId, Entity, FocusHandle, GlobalElementId, IntoElement, KeyDownEvent,
    LayoutId, MouseButton, Pixels, Render, Subscription, WeakEntity, Window,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use windows::Win32::{
    Foundation::HWND,
    UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture},
};

type Positions = Rc<RefCell<HashMap<&'static str, (f32, f32)>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Field(FinishField),
}
impl Target {
    fn key(self) -> &'static str {
        match self {
            Self::Field(FinishField::Radius) => "finish-inline-radius",
            Self::Field(FinishField::Blur) => "finish-inline-blur",
            Self::Field(FinishField::Opacity) => "finish-inline-opacity",
            Self::Field(FinishField::Thickness) => "finish-inline-thickness",
        }
    }
    fn probes(self) -> [&'static str; 5] {
        match self {
            Self::Field(FinishField::Radius) => [
                "finish-radius",
                "finish-radius-value",
                "finish-radius-slider",
                "finish-radius-start",
                "finish-radius-end",
            ],
            Self::Field(FinishField::Blur) => [
                "finish-blur",
                "finish-blur-value",
                "finish-blur-slider",
                "finish-blur-start",
                "finish-blur-end",
            ],
            Self::Field(FinishField::Opacity) => [
                "finish-opacity",
                "finish-opacity-value",
                "finish-opacity-slider",
                "finish-opacity-start",
                "finish-opacity-end",
            ],
            Self::Field(FinishField::Thickness) => [
                "finish-thickness",
                "finish-thickness-value",
                "finish-thickness-slider",
                "finish-thickness-start",
                "finish-thickness-end",
            ],
        }
    }
    fn unit(self) -> &'static str {
        match self {
            Self::Field(FinishField::Opacity) => "%",
            _ => "px",
        }
    }
    fn limits(self) -> (f32, f32) {
        match self {
            Self::Field(field) => field.limits(),
        }
    }
    fn value(self, style: SelectionFinishStyle) -> f32 {
        match self {
            Self::Field(field) => field.value(style),
        }
    }
    /// 圆角、扩散、粗细关闭时显示 0 且仍可调，拖离 0 即开启；浓度只在阴影开着时可调。
    fn enabled_in(self, style: SelectionFinishStyle) -> bool {
        match self {
            Self::Field(FinishField::Radius | FinishField::Blur | FinishField::Thickness) => true,
            Self::Field(FinishField::Opacity) => style.shadow_enabled,
        }
    }
    fn parse(self, text: &str) -> Result<f32, &'static str> {
        let value = text
            .trim()
            .parse::<f32>()
            .map_err(|_| tr!("请输入数值", "Enter a number"))?;
        let (min, max) = self.limits();
        if !value.is_finite() || !(min..=max).contains(&value) {
            return Err(tr!("超出范围", "Out of range"));
        }
        Ok(value)
    }
    fn at(self, fraction: f32) -> f32 {
        let (min, max) = self.limits();
        let fraction = if fraction.is_finite() {
            fraction.clamp(0., 1.)
        } else {
            0.
        };
        ((min + (max - min) * fraction) * 10.).round() / 10.
    }
    fn fraction(self, value: f32) -> f32 {
        let (min, max) = self.limits();
        ((value - min) / (max - min)).clamp(0., 1.)
    }
    fn stepped(self, value: f32, amount: f32) -> f32 {
        let (min, max) = self.limits();
        // 滚轮步进按 0.1 取整，避免浮点累加把尾数带进输入框。
        (((value + amount) * 10.).round() / 10.).clamp(min, max)
    }
}

fn number(value: f32) -> String {
    // 原值使用 f32 的往返表示；打开和同值 Enter 不应把细小透明度舍入。
    value.to_string()
}

/// 只接受当前完整外观版本的草稿；own apply 更新基线，不误判为外部 Reset。
struct Draft {
    target: Target,
    baseline: SelectionFinishStyle,
    text: String,
    stale: bool,
    dragging: bool,
}
impl Draft {
    fn new(target: Target, baseline: SelectionFinishStyle) -> Self {
        Self {
            target,
            baseline,
            text: number(target.value(baseline)),
            stale: false,
            dragging: false,
        }
    }
    /// 返回是否应同步输入。IME 中只失效旧草稿，保留平台正在组合的内容。
    fn sync(&mut self, source: SelectionFinishStyle, composing: bool) -> bool {
        if source != self.baseline {
            self.baseline = source;
            self.dragging = false;
            self.stale = composing;
            if !composing {
                self.text = number(self.target.value(source));
            }
            return !composing;
        }
        if self.stale && !composing {
            self.stale = false;
            self.text = number(self.target.value(source));
            return true;
        }
        false
    }
    fn value(&self) -> Result<f32, &'static str> {
        if self.stale {
            Err(tr!("参数已更新", "Value updated"))
        } else {
            self.target.parse(&self.text)
        }
    }
    fn accepted(&mut self, source: SelectionFinishStyle) {
        self.baseline = source;
    }
    fn discard(&mut self, source: SelectionFinishStyle) {
        self.baseline = source;
        self.text = number(self.target.value(source));
        self.stale = false;
        self.dragging = false;
    }
    fn finish_drag(&mut self, valid_release: bool, fraction: f32) -> Option<f32> {
        let dragging = std::mem::take(&mut self.dragging);
        (dragging && valid_release && !self.stale).then(|| self.target.at(fraction))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn control(
    owner: WeakEntity<CaptureView>,
    field: FinishField,
    style: SelectionFinishStyle,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
    enabled: bool,
) -> impl IntoElement {
    InlineElement {
        owner,
        target: Target::Field(field),
        style,
        palette,
        positions,
        wheel_settings,
        enabled,
    }
}

struct InlineElement {
    owner: WeakEntity<CaptureView>,
    target: Target,
    style: SelectionFinishStyle,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
    enabled: bool,
}
impl IntoElement for InlineElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for InlineElement {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some(self.target.key().into())
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
    ) -> (LayoutId, AnyElement) {
        let owner = self.owner.clone();
        let target = self.target;
        let style = self.style;
        let palette = self.palette;
        let positions = self.positions.clone();
        let wheel_settings = self.wheel_settings;
        let enabled = self.enabled;
        let state = window.use_keyed_state(target.key(), cx, {
            let owner = owner.clone();
            let positions = positions.clone();
            move |window, cx| {
                InlineControl::new(
                    owner,
                    target,
                    style,
                    palette,
                    positions,
                    wheel_settings,
                    enabled,
                    window,
                    cx,
                )
            }
        });
        state.update(cx, |v, cx| {
            if v.owner.entity_id() != owner.entity_id() {
                v.end_drag(window);
                *v = InlineControl::new(
                    owner,
                    target,
                    style,
                    palette,
                    positions,
                    wheel_settings,
                    enabled,
                    window,
                    cx,
                );
            } else {
                v.palette = palette;
                v.positions = positions;
                v.wheel_settings = wheel_settings;
                v.sync(style, enabled, window, cx);
            }
        });
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

struct InlineControl {
    owner: WeakEntity<CaptureView>,
    draft: Draft,
    input: Entity<TextField>,
    focus: FocusHandle,
    palette: Palette,
    positions: Positions,
    wheel_settings: (f32, f32),
    enabled: bool,
    slider_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    drag_hwnd: Option<HWND>,
    error: &'static str,
    _subscriptions: Vec<Subscription>,
}
enum ApplyResult {
    Accepted(SelectionFinishStyle),
    Stale(SelectionFinishStyle),
    Unavailable,
}

impl InlineControl {
    #[allow(clippy::too_many_arguments)]
    fn new(
        owner: WeakEntity<CaptureView>,
        target: Target,
        style: SelectionFinishStyle,
        palette: Palette,
        positions: Positions,
        wheel_settings: (f32, f32),
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = Draft::new(target, style);
        let enabled = enabled && target.enabled_in(style);
        let input = cx.new(|cx| TextField::new(draft.text.clone(), tr!("数值", "Value"), cx));
        // 空 group 不会阻止其它字段获得焦点；Tab 显式返回宿主。
        input.update(cx, |v, cx| {
            v.configure_focus(Vec::new(), window, cx);
            v.set_enabled(enabled, cx);
        });
        let focus = cx.focus_handle();
        let input_focus = input.read(cx).focus_handle.clone();
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |v, _, event, w, cx| match event {
                TextFieldEvent::Changed => v.changed(w, cx),
                TextFieldEvent::Submit => v.submit(w, cx),
            }),
            cx.observe_window_activation(window, |v, w, cx| {
                if !w.is_window_active() {
                    v.discard(w, cx);
                }
            }),
            cx.on_blur(&focus, window, |v, w, _| v.end_drag(w)),
            cx.on_blur(&input_focus, window, |v, w, cx| {
                // 从数字框点击本行滑条不取消刚开始的滑动；其它失焦结束本轮。
                if v.focus.is_focused(w) {
                    if !v.composing(cx) {
                        v.draft.text = number(v.draft.target.value(v.draft.baseline));
                        v.update_input(cx);
                        v.error = "";
                    }
                } else {
                    v.discard(w, cx);
                }
            }),
        ];
        // 创建和重绘都不 focus；用户点击时才接管窗口内部焦点。
        Self {
            owner,
            draft,
            input,
            focus,
            palette,
            positions,
            wheel_settings,
            enabled,
            slider_bounds: Rc::new(RefCell::new(None)),
            drag_hwnd: None,
            error: "",
            _subscriptions: subscriptions,
        }
    }
    fn composing(&self, cx: &App) -> bool {
        self.input.read(cx).is_composing()
    }
    fn source(&self, cx: &App) -> Option<SelectionFinishStyle> {
        self.owner.upgrade().map(|v| v.read(cx).finish_style)
    }
    fn update_input(&self, cx: &mut Context<Self>) {
        if self.input.read(cx).text() != self.draft.text {
            self.input
                .update(cx, |v, cx| v.set_text(self.draft.text.clone(), cx));
        }
    }
    fn sync(
        &mut self,
        style: SelectionFinishStyle,
        enabled: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composing = self.composing(cx);
        let external = style != self.draft.baseline;
        if external {
            self.end_drag(w);
        }
        if self.draft.sync(style, composing) {
            self.update_input(cx);
            self.error = "";
        }
        let enabled = enabled && self.draft.target.enabled_in(style);
        if self.enabled != enabled {
            let return_focus = !enabled && self.input.read(cx).focus_handle.is_focused(w);
            if !enabled {
                self.end_drag(w);
            }
            self.enabled = enabled;
            self.input.update(cx, |v, cx| v.set_enabled(enabled, cx));
            if return_focus {
                // 0 会实时关闭效果；disabled TextField 会吞 Esc/Tab。
                // 只还回本窗口内的宿主焦点，不激活窗口或撤销已接受的值。
                self.host_focus(w, cx);
            }
        }
    }
    /// 返回 false 代表此次旧事件必须丢弃，不能在同步后继续写入旧值。
    fn reconcile(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(source) = self.source(cx) else {
            self.end_drag(w);
            return false;
        };
        let stale = self.draft.stale || source != self.draft.baseline;
        self.sync(source, self.enabled, w, cx);
        !stale && self.enabled && !self.composing(cx)
    }
    fn changed(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.reconcile(w, cx) {
            cx.notify();
            return;
        }
        self.draft.text = self.input.read(cx).text().to_owned();
        match self.draft.value() {
            Ok(value) => {
                self.error = "";
                self.apply(value, w, cx);
            }
            Err(error) => self.error = error,
        }
        cx.notify();
    }
    fn submit(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.reconcile(w, cx) {
            return;
        }
        self.draft.text = self.input.read(cx).text().to_owned();
        match self.draft.value() {
            Ok(value) => {
                if self.apply(value, w, cx) {
                    self.draft.text = number(self.draft.target.value(self.draft.baseline));
                    self.update_input(cx);
                }
            }
            Err(error) => self.error = error,
        }
        // Enter 保留面板和输入焦点，不进入宿主确定/导出路径。
        cx.notify();
    }
    fn apply(&mut self, value: f32, w: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.enabled || self.draft.stale || self.composing(cx) {
            return false;
        }
        let baseline = self.draft.baseline;
        let target = self.draft.target;
        let result = self.owner.update(cx, |v, cx| {
            if v.finish_style != baseline {
                return ApplyResult::Stale(v.finish_style);
            }
            if v.toolbar_panel != ToolbarPanel::Finish
                || !v.finish_form_available()
                || v.editor.lasso_draft.is_some()
                || v.editor.document.crop_mask.is_some()
                || v.pin_mode
                || v.processing
                || v.saving
                || !target.enabled_in(v.finish_style)
            {
                return ApplyResult::Unavailable;
            }
            let accepted = match target {
                Target::Field(field) => v.apply_finish_parameter(field, value, cx),
            };
            if accepted {
                ApplyResult::Accepted(v.finish_style)
            } else {
                ApplyResult::Unavailable
            }
        });
        match result {
            Ok(ApplyResult::Accepted(source)) => {
                self.draft.accepted(source);
                self.error = "";
                if !target.enabled_in(source) {
                    self.sync(source, false, w, cx);
                }
                true
            }
            Ok(ApplyResult::Stale(source)) => {
                self.sync(source, self.enabled, w, cx);
                false
            }
            _ => {
                self.end_drag(w);
                false
            }
        }
    }
    fn set_value(&mut self, value: f32, w: &mut Window, cx: &mut Context<Self>) {
        if !value.is_finite() || !self.reconcile(w, cx) {
            return;
        }
        if self.apply(value, w, cx) {
            self.draft.text = number(value);
            self.update_input(cx);
            cx.notify();
        }
    }
    fn discard(&mut self, w: &Window, cx: &mut Context<Self>) {
        self.end_drag(w);
        if self.composing(cx) {
            // TextField 的 blur/IME 订阅负责结束组合；不能从父组件清空 marked。
            self.draft.stale = true;
        } else if let Some(source) = self.source(cx) {
            self.draft.discard(source);
            self.update_input(cx);
        }
        self.error = "";
        cx.notify();
    }
    fn host_focus(&self, w: &mut Window, cx: &mut Context<Self>) {
        let _ = self.owner.update(cx, |v, _| w.focus(&v.focus));
    }
    fn end_drag(&mut self, w: &Window) {
        self.draft.dragging = false;
        if let Some(own) = self.drag_hwnd.take() {
            // 仅释放本轮捕获且仍归本窗口的 HWND，绝不释放其它窗口的捕获。
            unsafe {
                if own == crate::gpui_capture::hwnd(w) && GetCapture() == own {
                    let _ = ReleaseCapture();
                }
            }
        }
    }
    fn fraction_at(&self, point: gpui::Point<Pixels>) -> Option<f32> {
        self.slider_bounds.borrow().map(|b| {
            (f32::from(point.x - b.left()) / f32::from(b.size.width).max(1.)).clamp(0., 1.)
        })
    }
    fn at(&mut self, point: gpui::Point<Pixels>, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(fraction) = self.fraction_at(point) {
            self.set_value(self.draft.target.at(fraction), w, cx);
        }
    }
    fn drag_move(&mut self, event: &gpui::MouseMoveEvent, w: &mut Window, cx: &mut Context<Self>) {
        if !self.draft.dragging {
            return;
        }
        let owned = self.drag_hwnd.is_some_and(|own| {
            own == crate::gpui_capture::hwnd(w) && unsafe { GetCapture() == own }
        });
        if owned && event.pressed_button == Some(MouseButton::Left) {
            self.at(event.position, w, cx);
        } else {
            self.end_drag(w);
        }
        cx.stop_propagation();
    }
    fn drag_up(&mut self, event: &gpui::MouseUpEvent, w: &mut Window, cx: &mut Context<Self>) {
        if !self.draft.dragging || event.button != MouseButton::Left {
            return;
        }
        // GPUI Windows 在 dispatch 前 ReleaseCapture；null 是本轮正常 Up 的合法状态。
        let valid_release = self.drag_hwnd.is_some_and(|own| {
            own == crate::gpui_capture::hwnd(w)
                && unsafe {
                    let current = GetCapture();
                    current == own || current.0.is_null()
                }
        });
        let fraction = self.fraction_at(event.position).unwrap_or(0.);
        if let Some(value) = self.draft.finish_drag(valid_release, fraction) {
            self.set_value(value, w, cx);
        }
        self.end_drag(w);
        cx.stop_propagation();
    }
    fn wheel(&mut self, event: &gpui::ScrollWheelEvent, w: &mut Window, cx: &mut Context<Self>) {
        let amount = crate::capture_interaction::notches(event, self.wheel_settings);
        if amount.is_finite() && amount != 0. && self.reconcile(w, cx) {
            let step = if event.modifiers.control {
                0.1
            } else if event.modifiers.shift {
                5.
            } else {
                1.
            };
            let value = self
                .draft
                .target
                .stepped(self.draft.target.value(self.draft.baseline), amount * step);
            self.set_value(value, w, cx);
        }
        cx.stop_propagation();
    }
    fn capture_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        // 留下 TextField 自己的复制、粘贴、选择和导航；拦截修改过的 Enter/Esc。
        if (m.alt || m.control || m.platform || m.function) && matches!(key, "enter" | "escape") {
            cx.stop_propagation();
        }
    }
    fn key(&mut self, event: &KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            cx.stop_propagation();
            return;
        }
        if crate::text_contract::terminal_held(event.keystroke.key.as_str(), event.is_held) {
            cx.stop_propagation();
            return;
        }
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if key == "tab" && !m.control && !m.alt && !m.platform {
            self.discard(w, cx);
            self.host_focus(w, cx);
        } else if m.alt || m.control || m.platform || m.function {
            // 未被 TextField 消费的宿主快捷键留在局部控件，不穿透至截图操作。
        } else {
            match key {
                "escape" => {
                    self.discard(w, cx);
                    self.host_focus(w, cx);
                }
                "enter" => self.submit(w, cx),
                "left" | "down" if self.focus.is_focused(w) => {
                    let amount = if m.shift { -5. } else { -1. };
                    let value = self
                        .draft
                        .target
                        .stepped(self.draft.target.value(self.draft.baseline), amount);
                    self.set_value(value, w, cx);
                }
                "right" | "up" if self.focus.is_focused(w) => {
                    let amount = if m.shift { 5. } else { 1. };
                    let value = self
                        .draft
                        .target
                        .stepped(self.draft.target.value(self.draft.baseline), amount);
                    self.set_value(value, w, cx);
                }
                "home" if self.focus.is_focused(w) => {
                    self.set_value(self.draft.target.limits().0, w, cx)
                }
                "end" if self.focus.is_focused(w) => {
                    self.set_value(self.draft.target.limits().1, w, cx)
                }
                _ => {}
            }
        }
        cx.stop_propagation();
    }
    fn slider(&self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.draft.target;
        let [_, _, id, start, end] = target.probes();
        let bounds = self.slider_bounds.clone();
        let positions = self.positions.clone();
        let weak = cx.entity().downgrade();
        let palette = self.palette;
        let enabled = self.enabled;
        let fraction = target.fraction(target.value(self.draft.baseline));
        div()
            .id(id)
            .relative()
            .flex_1()
            .min_w(px(48.))
            .h(px(36.))
            .when(enabled, |v| v.cursor_pointer())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, event: &gpui::MouseDownEvent, w, cx| {
                    if v.reconcile(w, cx) {
                        w.focus(&v.focus);
                        v.draft.dragging = true;
                        v.drag_hwnd = Some(crate::gpui_capture::hwnd(w));
                        v.at(event.position, w, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |rect, _, w, _| {
                        let track = Bounds::new(
                            point(rect.left() + px(8.), rect.center().y - px(2.)),
                            size((rect.size.width - px(16.)).max(px(1.)), px(4.)),
                        );
                        *bounds.borrow_mut() = Some(track);
                        let y = f32::from(track.center().y);
                        let mut probes = positions.borrow_mut();
                        probes.insert(start, (f32::from(track.left()), y));
                        probes.insert(end, (f32::from(track.right()), y));
                        probes.insert(id, (f32::from(rect.center().x), f32::from(rect.center().y)));
                        drop(probes);
                        let weak_move = weak.clone();
                        w.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak_move.update(cx, |v, cx| v.drag_move(event, w, cx));
                            }
                        });
                        let weak_up = weak.clone();
                        w.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak_up.update(cx, |v, cx| v.drag_up(event, w, cx));
                            }
                        });
                        let color = if enabled {
                            palette.accent
                        } else {
                            palette.muted
                        };
                        w.paint_quad(fill(track, rgb(palette.border)).corner_radii(px(2.)));
                        w.paint_quad(
                            fill(
                                Bounds::new(
                                    track.origin,
                                    size(track.size.width * fraction, px(4.)),
                                ),
                                rgb(color),
                            )
                            .corner_radii(px(2.)),
                        );
                        let thumb = Bounds::new(
                            point(
                                track.left() + track.size.width * fraction - px(6.),
                                track.center().y - px(6.),
                            ),
                            size(px(12.), px(12.)),
                        );
                        w.paint_quad(fill(thumb, rgb(color)).corner_radii(px(6.)));
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

fn probe(id: &'static str, positions: Positions) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, _, _| {
            positions.borrow_mut().insert(
                id,
                (f32::from(bounds.center().x), f32::from(bounds.center().y)),
            );
        },
    )
    .absolute()
    .size_full()
}

impl Render for InlineControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let [id, value_id, _, _, _] = self.draft.target.probes();
        let error = if self.composing(cx) { "" } else { self.error };
        div()
            .id(id)
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::capture_key))
            .on_key_down(cx.listener(Self::key))
            .on_scroll_wheel(cx.listener(Self::wheel))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .w_full()
                    .child(self.slider(cx))
                    .child(
                        div()
                            .id(value_id)
                            .relative()
                            .w(px(64.))
                            .flex_shrink_0()
                            .when(!error.is_empty(), |v| {
                                v.rounded_md()
                                    .border_1()
                                    .border_color(rgb(self.palette.danger))
                            })
                            .child(self.input.clone())
                            .child(probe(value_id, self.positions.clone())),
                    )
                    .child(
                        div()
                            .w(px(18.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(self.palette.muted))
                            .child(self.draft.target.unit()),
                    ),
            )
            .when(!error.is_empty(), |v| {
                v.child(
                    div()
                        .h(px(15.))
                        .text_size(px(10.))
                        .text_color(rgb(self.palette.danger))
                        .child(error),
                )
            })
            .child(probe(id, self.positions.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn style() -> SelectionFinishStyle {
        SelectionFinishStyle {
            corner_enabled: true,
            shadow_enabled: true,
            ..SelectionFinishStyle::default()
        }
    }
    #[test]
    fn invalid_draft_does_not_change_source_and_own_live_value_preserves_typing() {
        let source = style();
        let target = Target::Field(FinishField::Radius);
        let mut draft = Draft::new(target, source);
        for invalid in ["", "-", "NaN", "inf", "61"] {
            draft.text = invalid.into();
            assert!(draft.value().is_err());
            assert_eq!(draft.baseline, source);
        }
        draft.text = "24.".into();
        let accepted = FinishField::Radius.applied(source, draft.value().unwrap());
        draft.accepted(accepted);
        assert!(!draft.sync(accepted, false));
        assert_eq!(draft.text, "24.");
        draft.text = "bad".into();
        draft.discard(accepted);
        assert_eq!(draft.value(), Ok(24.));
        assert_eq!(draft.baseline, accepted);
        // 0 只关闭效果、显示 0 且仍可从 0 拖起；记住的半径在再开启时作为新 source 恢复。
        let off = FinishField::Radius.applied(accepted, 0.);
        draft.text = "0".into();
        draft.accepted(off);
        assert!(target.enabled_in(off));
        assert_eq!(target.value(off), 0.);
        assert_eq!(off.corner_radius, 24.);
        let on = SelectionFinishStyle {
            corner_enabled: true,
            ..off
        };
        assert!(draft.sync(on, false));
        assert_eq!(draft.value(), Ok(24.));
    }
    #[test]
    fn external_reset_retires_dirty_drag_before_submit() {
        let mut draft = Draft::new(Target::Field(FinishField::Radius), style());
        draft.text = "47".into();
        draft.dragging = true;
        let reset = SelectionFinishStyle::default();
        assert!(draft.sync(reset, false));
        assert!(!draft.dragging);
        // 重置后圆角关闭，显示 0。
        assert_eq!(draft.value(), Ok(0.));
        assert!(draft.finish_drag(true, 1.).is_none());
    }
    #[test]
    fn external_change_during_ime_defers_sync_and_rejects_old_composition() {
        let mut draft = Draft::new(Target::Field(FinishField::Opacity), style());
        draft.text = "９０".into();
        draft.dragging = true;
        let reset = SelectionFinishStyle::default();
        assert!(!draft.sync(reset, true));
        assert_eq!(draft.text, "９０");
        assert!(draft.stale);
        assert!(!draft.dragging);
        draft.text = "90".into();
        assert_eq!(draft.value(), Err("参数已更新"));
        assert!(draft.sync(reset, false));
        assert_eq!(
            draft.value(),
            Ok(Target::Field(FinishField::Opacity).value(reset))
        );
        assert!(!draft.stale);
    }
    #[test]
    fn final_up_applies_final_point_and_lost_capture_preserves_last_value() {
        let mut draft = Draft::new(
            Target::Field(FinishField::Thickness),
            SelectionFinishStyle {
                border_enabled: true,
                ..style()
            },
        );
        draft.dragging = true;
        assert_eq!(draft.finish_drag(true, 1.), Some(8.));
        assert!(!draft.dragging);
        assert_eq!(draft.finish_drag(true, 0.), None);
        draft.dragging = true;
        let baseline = draft.baseline;
        assert_eq!(draft.finish_drag(false, 0.), None);
        assert_eq!(draft.baseline, baseline);
        assert!(!draft.dragging);
    }
    #[test]
    fn zero_off_fields_stay_adjustable_and_dependent_parameters_need_their_effect() {
        let disabled = SelectionFinishStyle::default();
        for (target, adjustable) in [
            (Target::Field(FinishField::Radius), true),
            (Target::Field(FinishField::Blur), true),
            (Target::Field(FinishField::Opacity), false),
            (Target::Field(FinishField::Thickness), true),
        ] {
            assert_eq!(target.enabled_in(disabled), adjustable, "{target:?}");
            let (min, max) = target.limits();
            assert_eq!(target.at(-1.), min);
            assert_eq!(target.at(2.), max);
            assert_eq!(target.stepped(max, 5.), max);
            assert_eq!(target.stepped(min, -0.1), min);
        }
    }
}
