//! Pin OCR 查询只读预览与显式选中／一次标记；不触及图片复制路线。
use super::{
    find_ui::{
        self, FindAction, FindButtonStamp, FindClickIntent, FindClickPhase, FindPaintTicket,
        FindQueryRefreshTicket, FindUi,
    },
    *,
};
use crate::pin_text_selection::find::{
    OcrFindAccess, OcrFindBatchTicket, OcrFindDocument, OcrFindStamp, OcrFindTicket,
};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq)]
struct PinFindPaintSource {
    source: TextSelectionStamp,
    generation: u64,
    clip: crate::annotation::Bounds,
    viewport: crate::viewport::Viewport,
    dpi: f32,
    image_size: (u32, u32),
}

struct PinFindClickTarget {
    source: PinFindPaintSource,
    document: OcrFindDocument<TextSelectionStamp>,
    selected: Option<Range<usize>>,
    keyboard_selection: Option<crate::pin_text_selection::KeyboardSelection>,
    current: Option<OcrFindTicket<TextSelectionStamp>>,
    all: Option<OcrFindBatchTicket<TextSelectionStamp>>,
}
impl PinFindClickTarget {
    fn matches_host(
        &self,
        source: PinFindPaintSource,
        selected: &Option<Range<usize>>,
        keyboard_selection: &Option<crate::pin_text_selection::KeyboardSelection>,
    ) -> bool {
        self.source == source
            && &self.selected == selected
            && &self.keyboard_selection == keyboard_selection
    }
}

fn release_matches_render(
    queued_at_render: Option<&Arc<()>>,
    current_token: Option<&Arc<()>>,
) -> bool {
    queued_at_render
        .zip(current_token)
        .is_some_and(|(rendered, current)| Arc::ptr_eq(rendered, current))
}

struct PinFindClick {
    button: FindButtonStamp<TextSelectionStamp>,
    target: PinFindClickTarget,
    mark_style: crate::annotation::Style,
    focus: FocusHandle,
    _blur: gpui::Subscription,
}

#[derive(Default)]
pub(super) struct PinFindClickState {
    intent: FindClickIntent<PinFindClick>,
    painted: Option<PinFindPaintSource>,
}

impl PinView {
    pub(super) fn cancel_pin_find_click(&mut self) {
        self.ocr_find_click.borrow_mut().intent.cancel();
        // 同时使旧按钮 stamp 失效；A→B→A 不得借新的 Up listener 复活。
        if let Some(ui) = &mut self.ocr_find {
            ui.query_event();
        }
    }
    pub(super) fn discard_pin_find_release(&self) {
        // 每个新 Down 清上一轮 intent；不换 UI stamp，当前按钮仍能登记自己的 Down。
        self.ocr_find_click.borrow_mut().intent.cancel();
    }
    pub(super) fn schedule_pin_find_release_cleanup(&self, w: &mut Window, cx: &Context<Self>) {
        let token = {
            let state = self.ocr_find_click.borrow();
            if !state.intent.is_armed() {
                return;
            }
            state.intent.token().cloned()
        };
        let Some(token) = token else { return };
        let weak = cx.entity().downgrade();
        // Capture phase 排队，等 Bubble 的真实按钮 Up 有机会 queue 后再清理越界释放。
        w.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |view, cx| {
                let current = {
                    let state = view.ocr_find_click.borrow();
                    state.intent.is_armed()
                        && state
                            .intent
                            .token()
                            .is_some_and(|actual| Arc::ptr_eq(actual, &token))
                };
                if current {
                    view.ocr_find_click
                        .borrow_mut()
                        .intent
                        .release_outside(&token);
                    view.cancel_pin_find_click();
                    cx.notify();
                }
            });
        });
    }
    fn pin_find_paint_source(&self, w: &Window, cx: &App) -> PinFindPaintSource {
        PinFindPaintSource {
            source: self.text_selection_source(cx),
            generation: self.copy_layout_generation,
            clip: self.pin_find_clip(w),
            viewport: self.content_viewport(w),
            dpi: w.scale_factor(),
            image_size: self.image_size,
        }
    }
    /// 与 mark_text_quads 的计划使用相同样式，不能在按下后替换颜色或线宽。
    fn pin_find_mark_style(&self, mark: TextMark, cx: &App) -> crate::annotation::Style {
        let mut style = self
            .canvas
            .as_ref()
            .map_or_else(crate::annotation::Style::default, |canvas| {
                canvas.read(cx).editor.style
            });
        style.color = self
            .text_mark_color
            .unwrap_or(if mark == TextMark::Highlight {
                0xffd54f
            } else {
                style.color
            });
        style
    }
    // 只核宿主输入资格，忽略 source paint 的时序；执行仍使用 pin_find_access。
    fn pin_find_click_access(&self, w: &Window, cx: &App, held: bool) -> OcrFindAccess {
        let mut access = self.pin_find_access(w, cx);
        access.idle = if held {
            self.pin_find_input_idle(cx)
                && unsafe { GetCapture() == hwnd(w) }
                && find_ui::primary_button_down()
        } else {
            self.pin_find_idle(cx) && !find_ui::primary_button_down()
        };
        access
    }
    fn pin_find_click_current(
        &self,
        click: &PinFindClick,
        w: &Window,
        cx: &App,
        held: bool,
    ) -> bool {
        click.focus.is_focused(w)
            && click.target.matches_host(
                self.pin_find_paint_source(w, cx),
                &self.selected,
                &self.text_keyboard_selection,
            )
            && (click.button.action != FindAction::MarkAll || !self.state.locked)
            && self.ocr_find.as_ref().is_some_and(|ui| {
                click.mark_style == self.pin_find_mark_style(ui.mark_kind(), cx)
                    && ui.button_current(&click.button, self.pin_find_click_access(w, cx, held), cx)
            })
    }
    fn pin_find_body_current(&self, target: &PinFindClickTarget) -> bool {
        // 仅 Down/Up/执行事件核正文；paint 不逐帧扫描全文。
        self.text_selection
            .as_ref()
            .is_some_and(|text| text.text.as_bytes() == target.document.text().as_bytes())
    }
    fn pin_find_click(
        &mut self,
        phase: FindClickPhase,
        button: &FindButtonStamp<TextSelectionStamp>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match phase {
            FindClickPhase::Down => {
                let access = self.pin_find_click_access(w, cx, true);
                let source = self.pin_find_paint_source(w, cx);
                let painted = self.ocr_find_click.borrow().painted == Some(source)
                    && *self.ocr_find_painted.borrow() == Some((source.source, source.generation));
                let Some(ui) = &self.ocr_find else { return };
                if !painted
                    || !ui.button_current(button, access, cx)
                    || button.action == FindAction::MarkAll && self.state.locked
                {
                    return;
                }
                let mark_style = self.pin_find_mark_style(ui.mark_kind(), cx);
                let target = PinFindClickTarget {
                    source,
                    document: ui.document.clone(),
                    selected: self.selected.clone(),
                    keyboard_selection: self.text_keyboard_selection,
                    current: ui.model.current_ticket(&ui.document, access),
                    all: ui.model.all_ticket(&ui.document, access),
                };
                if !self.pin_find_body_current(&target) {
                    return;
                }
                let Some(focus) = w.focused(cx) else { return };
                let token = Arc::new(());
                let blur_token = token.clone();
                let blur = cx.on_blur(&focus, w, move |view, _, cx| {
                    let current = view
                        .ocr_find_click
                        .borrow()
                        .intent
                        .token()
                        .is_some_and(|token| Arc::ptr_eq(token, &blur_token));
                    if current {
                        view.cancel_pin_find_click();
                        cx.notify();
                    }
                });
                self.ocr_find_click.borrow_mut().intent.arm(
                    token,
                    PinFindClick {
                        button: button.clone(),
                        target,
                        mark_style,
                        focus,
                        _blur: blur,
                    },
                );
            }
            FindClickPhase::Up => {
                let current = self
                    .ocr_find_click
                    .borrow()
                    .intent
                    .payload()
                    .is_some_and(|click| {
                        click.button.action == button.action
                            && self.pin_find_click_current(click, w, cx, false)
                            && self.pin_find_body_current(&click.target)
                            && self.ocr_find.as_ref().is_some_and(|ui| {
                                ui.button_current(
                                    button,
                                    self.pin_find_click_access(w, cx, false),
                                    cx,
                                )
                            })
                    });
                if current {
                    self.ocr_find_click.borrow_mut().intent.queue();
                } else {
                    self.cancel_pin_find_click();
                }
            }
        }
        cx.notify();
    }
    fn run_pin_find_click(
        &mut self,
        ticket: &FindPaintTicket,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let click = self.ocr_find_click.borrow_mut().intent.take(ticket);
        let Some(click) = click else { return };
        let access = self.pin_find_access(w, cx);
        if !access.active
            || !access.idle
            || !access.source_current
            || access.composing
            || !self.pin_find_click_current(&click, w, cx, false)
            || !self.pin_find_body_current(&click.target)
            || self.ocr_find_click.borrow().painted != Some(click.target.source)
        {
            return;
        }
        // 已先消费同次 release + 实际 paint 的票，再使用 Down 时的原票据执行。
        self.apply_pin_find_action(click.button.action, Some(&click.target), w, cx);
    }
    pub(super) fn pin_find_idle(&self, cx: &App) -> bool {
        self.pin_find_input_idle(cx) && unsafe { GetCapture() }.0.is_null()
    }
    // 登记按下仅接 GPUI 在该次 Down 前取得的本窗口捕获；执行仍用 pin_find_idle。
    fn pin_find_input_idle(&self, cx: &App) -> bool {
        self.text_mode
            && self.text_source == ocr_clipboard::TextSource::Ocr
            && !self.saving
            && !self.processing
            && !self.processing_image_unavailable
            && !self.presentation_geometry_adjusting
            && !self.color_pick
            && self.pending_placement.is_none()
            && self.gesture.is_none()
            && self.drag_request.is_none()
            && self.text_anchor.is_none()
            && self.text_capture.is_none()
            && !self.context_menu_press
            && !self.color_context_menu_open(cx)
            && self.annotation_preview.visible()
            && self.text_mark_canvas_idle(cx)
            && self.text_selection_stamp == Some(self.text_selection_source(cx))
            && self.text_selection.is_some()
    }
    fn pin_find_source_current(&self, cx: &App) -> bool {
        self.ocr_find.as_ref().is_none_or(|ui| {
            self.text_selection.as_ref().is_some_and(|_| {
                let source = self.text_selection_source(cx);
                ui.current(&source, self.copy_layout_generation, source.2.unwrap_or(0))
            })
        })
    }
    pub(super) fn ocr_find_input_focused(&self, w: &Window, cx: &App) -> bool {
        self.ocr_find.as_ref().is_some_and(|ui| ui.focused(w, cx))
    }
    fn pin_find_access(&self, w: &Window, cx: &App) -> OcrFindAccess {
        let source = self.text_selection_source(cx);
        OcrFindAccess {
            active: w.is_window_active()
                && (self.focus.is_focused(w)
                    || self.ocr_find_input_focused(w, cx)
                    || self.pin_local_candidate_focused(w)),
            idle: self.pin_find_idle(cx)
                && *self.ocr_find_painted.borrow() == Some((source, self.copy_layout_generation)),
            source_current: self.pin_find_source_current(cx),
            composing: self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx)),
        }
    }
    fn pin_find_clip(&self, w: &Window) -> crate::annotation::Bounds {
        let a = self.text_point(point(px(0.), px(0.)), w);
        let b = self.text_point(point(w.viewport_size().width, w.viewport_size().height), w);
        let x = a.x.max(0.);
        let y = a.y.max(0.);
        crate::annotation::Bounds {
            x,
            y,
            width: (b.x.min(self.image_size.0 as f32) - x).max(0.),
            height: (b.y.min(self.image_size.1 as f32) - y).max(0.),
        }
    }
    pub(super) fn validate_ocr_find(&mut self, cx: &App) {
        if !self.text_mode || !self.pin_find_source_current(cx) {
            self.cancel_pin_find_click();
            self.ocr_find = None;
        }
    }
    pub(super) fn clear_find_preview(&mut self) {
        self.cancel_pin_find_click();
        // 真正原图选字事件撤销旧局部意图；字节范围 ABA 也不能复活回执。
        self.cancel_pin_local_intent("原图选字意图改变");
        if let Some(ui) = &mut self.ocr_find {
            ui.clear_preview();
        }
    }
    pub(super) fn pin_find_refresh(&mut self, w: &Window, cx: &mut Context<Self>) {
        self.validate_ocr_find(cx);
        let access = self.pin_find_access(w, cx);
        let clip = self.pin_find_clip(w);
        if let (Some(ui), Some(text)) = (&mut self.ocr_find, &self.text_selection) {
            ui.refresh(text, clip, access, cx);
        }
        cx.notify();
    }
    fn refresh_pin_find_query(
        &mut self,
        ticket: &FindQueryRefreshTicket,
        epoch: &Arc<()>,
        source: PinFindPaintSource,
        w: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = &self.ocr_find else { return };
        // 先释放同查询预约；门暂时不满足时保留 pending，旧票不清新预约。
        ui.release_query_refresh(ticket);
        let access = self.pin_find_access(w, cx);
        if ui.query_refresh_current(ticket)
            && access.active
            && access.idle
            && access.source_current
            && !access.composing
            && self.ocr_find_click.borrow().intent.epoch_is_current(epoch)
            && self.ocr_find_click.borrow().painted == Some(source)
            && self.pin_find_paint_source(w, cx) == source
        {
            // 只更新已取消点击后的查询缓存／locate，不重取或执行旧按钮票据。
            self.pin_find_refresh(w, cx);
        }
    }
    pub(super) fn ocr_find_capture_key(
        &mut self,
        e: &gpui::KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let focused = self.ocr_find_input_focused(w, cx);
        let Some(action) =
            find_ui::shortcut(e.keystroke.key.as_str(), e.keystroke.modifiers, focused)
        else {
            return false;
        };
        // 菜单／其它输入先拥有自己的键；查找不关闭这些界面再穿透。
        if self.context_menu_press || self.color_context_menu_open(cx) {
            // Esc 沿原菜单关闭路线；下一次 Esc 才由查询栏处理。
            return self.text_mode && action != FindAction::Close;
        }
        if !self.text_mark_canvas_idle(cx)
            || self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx))
        {
            return false;
        }
        if action == FindAction::Open && !self.text_mode {
            return false;
        }
        if action != FindAction::Open && self.ocr_find.is_none() {
            return false;
        }
        if !e.is_held
            || matches!(action, FindAction::Next | FindAction::Previous) && e.keystroke.key == "f3"
        {
            self.ocr_find_action(action, w, cx);
        }
        true
    }
    fn ocr_find_action(&mut self, action: FindAction, w: &mut Window, cx: &mut Context<Self>) {
        // F3/Enter 和关闭沿原路线；它们先撤销尚未释放的鼠标点击。
        self.cancel_pin_find_click();
        if action == FindAction::Close {
            let focused = self.ocr_find_input_focused(w, cx);
            self.ocr_find = None;
            if focused && w.is_window_active() {
                w.focus(&self.focus);
            }
            cx.notify();
            return;
        }
        self.validate_text_overlay(cx);
        self.validate_ocr_find(cx);
        let access = self.pin_find_access(w, cx);
        if !access.active || !access.idle || !access.source_current || access.composing {
            return;
        }
        if action == FindAction::Open {
            if self.ocr_find.is_none() {
                let source = self.text_selection_source(cx);
                let text = &self.text_selection.as_ref().expect("查找来源已核验").text;
                let ui = FindUi::open(
                    OcrFindStamp {
                        source,
                        result_generation: self.copy_layout_generation,
                        content_revision: source.2.unwrap_or(0),
                    },
                    text,
                    w,
                    cx,
                );
                let Ok(ui) = ui else {
                    self.status = tr!("文字超出查找上限", "Text exceeds the find limit").into();
                    return;
                };
                let field = ui.field.clone();
                self.ocr_find = Some(ui);
                let changed = cx.observe_in(&field, w, |v: &mut Self, _, w, cx| {
                    // 忙碌／捕获／IME 时 refresh 可拒绝；取消不能等待 refresh 成功。
                    v.cancel_pin_find_click();
                    v.pin_find_refresh(w, cx)
                });
                // Changed 与 notify/unmark 都落此 observer，不把候选文字当已提交查询。
                self.ocr_find
                    .as_mut()
                    .expect("查询栏已安装")
                    .subscriptions
                    .push(changed);
            }
            let field = self.ocr_find.as_ref().expect("查询栏已安装").field.clone();
            w.focus(&field.read(cx).focus_handle);
            cx.notify();
            return;
        }
        self.pin_find_refresh(w, cx);
        self.apply_pin_find_action(action, None, w, cx);
    }
    fn apply_pin_find_action(
        &mut self,
        action: FindAction,
        pressed: Option<&PinFindClickTarget>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let access = self.pin_find_access(w, cx);
        let clip = self.pin_find_clip(w);
        if !access.active || !access.idle || !access.source_current || access.composing {
            return;
        }
        let Some(mark_choice) = self.ocr_find.as_ref().map(FindUi::mark_choice_stamp) else {
            return;
        };
        let mark = mark_choice.kind();
        let mark_style = self.pin_find_mark_style(mark, cx);
        let Some(ui) = &mut self.ocr_find else {
            return;
        };
        let text = self.text_selection.as_ref().expect("查找选字来源已核验");
        match action {
            FindAction::ToggleMarkKinds | FindAction::SetMarkKind(_) => {
                ui.apply_mark_choice(action);
            }
            FindAction::Previous | FindAction::Next => {
                ui.cycle(action == FindAction::Next, text, clip, access);
            }
            FindAction::Select => {
                let ticket = pressed
                    .and_then(|pressed| pressed.current.clone())
                    .or_else(|| {
                        (pressed.is_none())
                            .then(|| ui.model.current_ticket(&ui.document, access))
                            .flatten()
                    });
                let Some(ticket) = ticket else {
                    return;
                };
                // byte 票据足以授权普通文字选择；缺图上几何只拒绝定位／标记。
                let result = ui.model.resolve_ticket(&ticket, &ui.document, access);
                match result {
                    Ok(range) => {
                        let tools_ready = text
                            .selected(range.clone())
                            .is_some_and(|text| !text.is_empty());
                        self.text_keyboard_selection =
                            text.keyboard_selection_from_range(range.clone());
                        self.selected = Some(range);
                        self.text_show_selection = true;
                        self.text_tools_ready = tools_ready;
                        ui.clear_preview();
                        // 当前票据已解析，才撤销局部意图；失败不丢候选或其焦点归属。
                        self.cancel_pin_local_intent("查找显式选择改变局部意图");
                        if w.is_window_active() {
                            w.focus(&self.focus);
                        }
                    }
                    Err(error) => ui.error = find_ui::error_text(error).into(),
                }
            }
            FindAction::MarkAll => {
                if self.state.locked {
                    return;
                }
                let ticket = pressed.and_then(|pressed| pressed.all.clone()).or_else(|| {
                    (pressed.is_none())
                        .then(|| ui.model.all_ticket(&ui.document, access))
                        .flatten()
                });
                let Some(ticket) = ticket else {
                    return;
                };
                let quads = ui
                    .model
                    .resolve_all_quads(&ticket, &ui.document, text, clip, access)
                    .and_then(|quads| {
                        ui.model
                            .resolve_all_ticket(&ticket, &ui.document, access)
                            .map(|_| quads)
                    });
                match quads {
                    Ok(quads) => {
                        if let Err(error) = self.mark_text_quads(mark, &quads, w, cx, |v, w, cx| {
                            let fresh = v.pin_find_access(w, cx);
                            !v.state.locked
                                && v.pin_find_clip(w) == clip
                                && v.pin_find_mark_style(mark, cx) == mark_style
                                && pressed.is_none_or(|pressed| {
                                    pressed.matches_host(
                                        v.pin_find_paint_source(w, cx),
                                        &v.selected,
                                        &v.text_keyboard_selection,
                                    ) && v.pin_find_body_current(pressed)
                                })
                                && v.ocr_find.as_ref().is_some_and(|ui| {
                                    ui.mark_choice_current(&mark_choice)
                                        && ui
                                            .model
                                            .resolve_all_ticket(&ticket, &ui.document, fresh)
                                            .is_ok()
                                })
                        }) {
                            if let Some(ui) = &mut self.ocr_find {
                                ui.error = error.to_string();
                            }
                        } else {
                            let focused = self.ocr_find_input_focused(w, cx);
                            self.validate_ocr_find(cx);
                            if focused && self.ocr_find.is_none() && w.is_window_active() {
                                w.focus(&self.focus);
                            }
                        }
                    }
                    Err(error) => ui.error = find_ui::error_text(error).into(),
                }
            }
            _ => {}
        }
        cx.notify();
    }
    pub(super) fn render_ocr_find(
        &self,
        w: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.text_mode || self.text_source != ocr_clipboard::TextSource::Ocr {
            self.ocr_find_click.borrow_mut().intent.cancel();
            return None;
        }
        let click_state = self.ocr_find_click.clone();
        {
            let state = click_state.borrow();
            let current = state.intent.payload().is_none_or(|click| {
                self.pin_find_click_current(click, w, cx, state.intent.is_armed())
            });
            drop(state);
            if !current {
                click_state.borrow_mut().intent.cancel();
            }
        }
        let epoch = click_state.borrow_mut().intent.begin_render();
        let retained = click_state
            .borrow()
            .intent
            .payload()
            .map(|click| click.button.clone());
        // render 必须开始于 Up 已排队之后；旧 held render 的延后 paint 不能出执行票。
        let release_token = {
            let state = click_state.borrow();
            (!state.intent.is_armed())
                .then(|| state.intent.token().cloned())
                .flatten()
        };
        let access = self.pin_find_access(w, cx);
        let clip = self.pin_find_clip(w);
        let quads = self
            .ocr_find
            .as_ref()
            .and_then(|ui| ui.model.location(&ui.document, clip, access))
            .map(|location| location.quads.clone())
            .unwrap_or_default();
        let preview = self.ocr_find.as_ref().and_then(|ui| {
            ui.model
                .current_ticket(&ui.document, access)
                .map(|ticket| (ticket, ui.document.clone()))
        });
        let viewport = self.content_viewport(w);
        let dpi = w.scale_factor();
        let source = self.text_selection_source(cx);
        let generation = self.copy_layout_generation;
        let paint_source = self.pin_find_paint_source(w, cx);
        let query_refresh = self
            .ocr_find
            .as_ref()
            .and_then(|ui| ui.query_refresh_ticket());
        let painted = self.ocr_find_painted.clone();
        let owner = cx.entity().downgrade();
        let mut root = div().absolute().top_0().left_0().size_full().child(
            canvas(
                |_, _, _| (),
                move |_, _, window, cx| {
                    if !click_state.borrow().intent.epoch_is_current(&epoch) {
                        return;
                    }
                    let current = owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        window.is_window_active()
                            && (view.focus.is_focused(window)
                                || view.ocr_find_input_focused(window, cx)
                                || view.pin_local_candidate_focused(window))
                            && view.pin_find_idle(cx)
                            && view.text_selection_source(cx) == source
                            && view.copy_layout_generation == generation
                            && view.pin_find_paint_source(window, cx) == paint_source
                    });
                    *painted.borrow_mut() = current.then_some((source, generation));
                    let recovered = {
                        let mut state = click_state.borrow_mut();
                        let recovered = current && state.painted != Some(paint_source);
                        state.painted = current.then_some(paint_source);
                        recovered
                    };
                    if recovered {
                        let weak = owner.clone();
                        let state = click_state.clone();
                        window.on_next_frame(move |window, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                if state.borrow().painted == Some(paint_source)
                                    && view.pin_find_paint_source(window, cx) == paint_source
                                    && view.pin_find_idle(cx)
                                {
                                    // held/越界松开后的第一张合格图恢复完整按钮，随后不重复排队。
                                    cx.notify();
                                }
                            });
                        });
                    }
                    if current {
                        let refresh = query_refresh.as_ref().and_then(|ticket| {
                            let owner = owner.upgrade()?;
                            let view = owner.read(cx);
                            let access = view.pin_find_access(window, cx);
                            let ui = view.ocr_find.as_ref()?;
                            (access.active
                                && access.idle
                                && access.source_current
                                && !access.composing
                                && ui.query_refresh_current(ticket)
                                && ui.queue_query_refresh(ticket))
                            .then(|| ticket.clone())
                        });
                        if let Some(ticket) = refresh {
                            let weak = owner.clone();
                            let epoch = epoch.clone();
                            window.on_next_frame(move |window, cx| {
                                let _ = weak.update(cx, |view, cx| {
                                    view.refresh_pin_find_query(
                                        &ticket,
                                        &epoch,
                                        paint_source,
                                        window,
                                        cx,
                                    );
                                });
                            });
                            window.request_animation_frame();
                        }
                    }
                    let click_current = current
                        && owner.upgrade().is_some_and(|owner| {
                            let view = owner.read(cx);
                            let state = click_state.borrow();
                            !state.intent.is_armed()
                                && release_matches_render(
                                    release_token.as_ref(),
                                    state.intent.token(),
                                )
                                && state.intent.payload().is_some_and(|click| {
                                    view.pin_find_click_current(click, window, cx, false)
                                })
                        });
                    if click_current {
                        let ticket = click_state.borrow_mut().intent.painted(&epoch);
                        if let Some(ticket) = ticket {
                            let weak = owner.clone();
                            window.on_next_frame(move |window, cx| {
                                let _ = weak.update(cx, |view, cx| {
                                    view.run_pin_find_click(&ticket, window, cx);
                                });
                            });
                        }
                    }
                    let preview_current = owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        let access = view.pin_find_access(window, cx);
                        view.content_viewport(window) == viewport
                            && window.scale_factor() == dpi
                            && view.pin_find_clip(window) == clip
                            && preview.as_ref().is_some_and(|(ticket, document)| {
                                view.ocr_find.as_ref().is_some_and(|ui| {
                                    ui.model.resolve_ticket(ticket, document, access).is_ok()
                                })
                            })
                    });
                    if !current || !preview_current {
                        return;
                    }
                    for quad in &quads {
                        let points = quad.map(|p| {
                            point(
                                px((viewport.offset.x + p.x * viewport.zoom) / dpi),
                                px((viewport.offset.y + p.y * viewport.zoom) / dpi),
                            )
                        });
                        let mut path = gpui::Path::new(points[0]);
                        let uv = point(0., 1.);
                        path.push_triangle((points[0], points[1], points[2]), (uv, uv, uv));
                        path.push_triangle((points[0], points[2], points[3]), (uv, uv, uv));
                        window.paint_path(path, rgba(0xffc24755));
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        if let Some(ui) = &self.ocr_find {
            root = root.child(find_ui::bar(
                ui,
                access,
                (
                    8.,
                    8.,
                    (f32::from(w.viewport_size().width) - 16.).clamp(1., 420.),
                ),
                !self.state.locked,
                cx,
                find_ui::FindBarBindings {
                    action: Self::ocr_find_action,
                    key: Self::ocr_find_capture_key,
                    retained: retained.as_ref(),
                    click: Self::pin_find_click,
                    docked: false,
                    available_height: (f32::from(w.viewport_size().height) - 16.).max(1.),
                },
            ));
        }
        Some(root.into_any_element())
    }
}

#[cfg(test)]
#[path = "pin_find_click_tests.rs"]
mod click_tests;
