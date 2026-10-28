//! 全屏 OCR 原位查找；与原位层共用真实来源／蓝层 paint 回执。
use super::*;
use crate::{
    pin_text_selection::find::{OcrFindAccess, OcrFindStamp},
    pin_view::find_ui::{self, FindAction, FindUi},
};

#[derive(Clone)]
struct CaptureFindStamp {
    button: find_ui::FindButtonStamp<OcrTarget>,
    body: Arc<()>,
    generation: u64,
    revision: u64,
    ui_generation: u64,
    overlay: Rc<RefCell<Option<OcrTarget>>>,
    selection: Option<Range<usize>>,
    cursor: Option<KeyboardSelection>,
    clip: ShapeBounds,
    offset: gpui::Point<gpui::Pixels>,
    viewport: [f32; 2],
    scale: f32,
    projection: crate::ocr_projection::OcrProjection,
    mark_style: crate::annotation::Style,
}
pub(crate) struct CaptureFindClick {
    stamp: CaptureFindStamp,
    focus: gpui::FocusHandle,
    _blur: gpui::Subscription,
    _activation: gpui::Subscription,
}

impl CaptureView {
    pub(crate) fn ocr_find_input_focused(&self, w: &Window, cx: &gpui::App) -> bool {
        self.ocr_find.as_ref().is_some_and(|ui| ui.focused(w, cx))
    }
    pub(crate) fn ocr_find_focus_owned(&self, w: &Window, cx: &gpui::App) -> bool {
        self.focus.is_focused(w)
            || self.ocr_find_input_focused(w, cx)
            || self.local_ocr_candidate_focused(w)
    }
    fn capture_find_current(&self) -> bool {
        self.ocr_find.as_ref().is_none_or(|ui| {
            self.ocr_overlay.as_ref().is_some_and(|overlay| {
                let (generation, revision) = self.ocr_find_result_identity();
                ui.current(&overlay.target, generation, revision)
            })
        })
    }
    pub(super) fn validate_ocr_find(&mut self) {
        if self.result_editor.is_some()
            || self.ocr_overlay.is_none()
            || !self.capture_find_current()
        {
            self.cancel_capture_find_click();
            self.ocr_find = None;
        }
    }
    fn capture_find_access(&self, w: &Window, cx: &gpui::App) -> OcrFindAccess {
        let mut access = self.capture_find_access_without_capture(w, cx);
        access.idle &= unsafe { GetCapture() }.0.is_null();
        access
    }
    /// 只为真实按钮 Down/held paint 验证元数据；动作仍用完整空捕获门。
    fn capture_find_access_without_capture(&self, w: &Window, cx: &gpui::App) -> OcrFindAccess {
        OcrFindAccess {
            active: w.is_window_active() && self.ocr_find_focus_owned(w, cx),
            idle: self.ocr_overlay_ready()
                && self.result_selection_pointer_ready()
                && !self.result_selection_dragging(self.result_selection_generation())
                && self.text_copy_completion_is_idle()
                && !self.preview_text_replay
                && !self.color_picker_pending
                && self.ocr_overlay.as_ref().is_some_and(|o| {
                    o.anchor.is_none() && o.capture.is_none() && o.keyboard_target_painted()
                }),
            source_current: self.capture_find_current()
                && self.ocr_overlay.as_ref().is_some_and(|o| {
                    o.current(
                        self.current_ocr_target().as_ref(),
                        self.current_result_text(),
                    )
                }),
            composing: self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx)),
        }
    }
    fn capture_find_clip(&self) -> Option<ShapeBounds> {
        let roi = self.ocr_overlay.as_ref()?.target.selected;
        Some(ShapeBounds {
            x: roi.x as f32,
            y: roi.y as f32,
            width: roi.width as f32,
            height: roi.height as f32,
        })
    }
    /// 与原位标记 helper 使用相同颜色规则，按下后不能改用新的绘制样式。
    fn capture_find_mark_style(&self, mark: TextMark) -> crate::annotation::Style {
        let mut style = self.editor.style;
        style.color = self
            .ocr_overlay
            .as_ref()
            .and_then(|overlay| overlay.color)
            .unwrap_or(if mark == TextMark::Highlight {
                0xffd54f
            } else {
                style.color
            });
        style
    }
    fn capture_find_refresh(&mut self, w: &Window, cx: &mut Context<Self>) {
        self.validate_ocr_find();
        let access = self.capture_find_access(w, cx);
        let Some(clip) = self.capture_find_clip() else {
            return;
        };
        if let (Some(ui), Some(overlay)) = (&mut self.ocr_find, &self.ocr_overlay) {
            ui.refresh(&overlay.text, clip, access, cx);
        }
        cx.notify();
    }

    fn cancel_capture_find_click(&self) {
        self.capture_find_click.borrow_mut().cancel();
    }
    fn capture_find_query_event(&mut self, w: &Window, cx: &mut Context<Self>) {
        self.cancel_capture_find_click();
        if let Some(ui) = &mut self.ocr_find {
            ui.query_event();
        }
        self.capture_find_refresh(w, cx);
    }
    fn refresh_capture_find_query(
        &mut self,
        ticket: &find_ui::FindQueryRefreshTicket,
        w: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = &self.ocr_find else { return };
        ui.release_query_refresh(ticket);
        let access = self.capture_find_access(w, cx);
        if ui.query_refresh_current(ticket)
            && access.active
            && access.idle
            && access.source_current
            && !access.composing
        {
            self.capture_find_refresh(w, cx);
        }
    }
    fn capture_find_click_current(
        &self,
        stamp: &CaptureFindStamp,
        w: &Window,
        cx: &gpui::App,
        held: bool,
    ) -> bool {
        let access = if held {
            self.capture_find_access_without_capture(w, cx)
        } else {
            self.capture_find_access(w, cx)
        };
        access.active
            && access.idle
            && access.source_current
            && !access.composing
            && (!held || unsafe { GetCapture() } == hwnd(w) && find_ui::primary_button_down())
            && self.ocr_find_result_identity() == (stamp.generation, stamp.revision)
            && self.result_selection_generation() == stamp.ui_generation
            && Arc::ptr_eq(&self.current_result_body_identity(), &stamp.body)
            && self.capture_find_clip() == Some(stamp.clip)
            && self.canvas_offset(w) == stamp.offset
            && w.scale_factor() == stamp.scale
            && [
                f32::from(w.viewport_size().width),
                f32::from(w.viewport_size().height),
            ] == stamp.viewport
            && self.ocr_projection(w) == Some(stamp.projection)
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                Rc::ptr_eq(&overlay.painted_keyboard_target, &stamp.overlay)
                    && overlay.selected == stamp.selection
                    && overlay.keyboard_selection == stamp.cursor
                    && overlay.painted_projection.get() == Some(stamp.projection)
            })
            && self.ocr_find.as_ref().is_some_and(|ui| {
                self.capture_find_mark_style(ui.mark_kind()) == stamp.mark_style
                    && ui.button_current(&stamp.button, access, cx)
            })
    }
    fn capture_find_click(
        &mut self,
        phase: find_ui::FindClickPhase,
        button: &find_ui::FindButtonStamp<OcrTarget>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match phase {
            find_ui::FindClickPhase::Down => {
                let Some(projection) = self.ocr_projection(w) else {
                    return;
                };
                let Some(overlay) = &self.ocr_overlay else {
                    return;
                };
                let Some(clip) = self.capture_find_clip() else {
                    return;
                };
                let Some(ui) = &self.ocr_find else { return };
                let (generation, revision) = self.ocr_find_result_identity();
                let stamp = CaptureFindStamp {
                    button: button.clone(),
                    body: self.current_result_body_identity(),
                    generation,
                    revision,
                    ui_generation: self.result_selection_generation(),
                    overlay: overlay.painted_keyboard_target.clone(),
                    selection: overlay.selected.clone(),
                    cursor: overlay.keyboard_selection,
                    clip,
                    offset: self.canvas_offset(w),
                    viewport: [
                        f32::from(w.viewport_size().width),
                        f32::from(w.viewport_size().height),
                    ],
                    scale: w.scale_factor(),
                    projection,
                    mark_style: self.capture_find_mark_style(ui.mark_kind()),
                };
                if !self.capture_find_click_current(&stamp, w, cx, true) {
                    return;
                }
                let Some(focus) = w.focused(cx) else { return };
                let token = Arc::new(());
                let click = self.capture_find_click.clone();
                let blur_token = Arc::clone(&token);
                let blur = cx.on_blur(&focus, w, move |_, _, cx| {
                    click.borrow_mut().cancel_token(&blur_token);
                    cx.notify();
                });
                let click = self.capture_find_click.clone();
                let activation_token = Arc::clone(&token);
                let activation = cx.observe_window_activation(w, move |_, w, cx| {
                    if !w.is_window_active() {
                        click.borrow_mut().cancel_token(&activation_token);
                        cx.notify();
                    }
                });
                self.capture_find_click.borrow_mut().arm(
                    token,
                    CaptureFindClick {
                        stamp,
                        focus,
                        _blur: blur,
                        _activation: activation,
                    },
                );
            }
            find_ui::FindClickPhase::Up => {
                let current = {
                    let click = self.capture_find_click.borrow();
                    click.is_armed()
                        && click.payload().is_some_and(|payload| {
                            payload.stamp.button.action == button.action
                                && payload.focus.is_focused(w)
                                && self.capture_find_click_current(&payload.stamp, w, cx, false)
                        })
                };
                if !current
                    || find_ui::primary_button_down()
                    || !unsafe { GetCapture() }.0.is_null()
                {
                    self.cancel_capture_find_click();
                    return;
                }
                if self.capture_find_click.borrow_mut().queue() {
                    cx.notify();
                }
            }
        }
    }
    fn run_capture_find_click(
        &mut self,
        ticket: &find_ui::FindPaintTicket,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = self.capture_find_click.borrow_mut().take(ticket);
        let Some(command) = command else { return };
        if !find_ui::primary_button_down()
            && unsafe { GetCapture() }.0.is_null()
            && command.focus.is_focused(w)
            && self.capture_find_click_current(&command.stamp, w, cx, false)
        {
            // 不先 query_event：提交仍要解析 Down 时的查询、种类和整批票据。
            self.apply_capture_find_action(
                command.stamp.button.action,
                Some(&command.stamp),
                w,
                cx,
            );
        }
    }
    pub(crate) fn ocr_find_capture_key(
        &mut self,
        event: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let focused = self.ocr_find_input_focused(w, cx);
        let Some(action) = find_ui::shortcut(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers,
            focused,
        ) else {
            return false;
        };
        if self.editor.text_draft.is_some()
            || self.watermark_input_focused
            || self.result_editor.is_some()
            || self.toolbar_panel != ToolbarPanel::None
            || self.ocr_overlay_host_blocked()
        {
            return false;
        }
        if self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx)) {
            return false;
        }
        if action == FindAction::Open && self.ocr_overlay.is_none() {
            return false;
        }
        if action != FindAction::Open && self.ocr_find.is_none() {
            return false;
        }
        if !event.is_held
            || matches!(action, FindAction::Next | FindAction::Previous)
                && event.keystroke.key == "f3"
        {
            self.ocr_find_action(action, w, cx);
        }
        cx.stop_propagation();
        true
    }
    pub(super) fn ocr_find_action(
        &mut self,
        action: FindAction,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_capture_find_click();
        if let Some(ui) = &mut self.ocr_find {
            ui.query_event();
        }
        if action == FindAction::Close {
            let focused = self.ocr_find_input_focused(w, cx);
            self.ocr_find = None;
            if focused && w.is_window_active() {
                w.focus(&self.focus);
            }
            cx.notify();
            return;
        }
        self.validate_ocr_overlay();
        self.validate_ocr_find();
        let access = self.capture_find_access(w, cx);
        if !access.active || !access.idle || !access.source_current || access.composing {
            return;
        }
        if action == FindAction::Open {
            if self.ocr_find.is_none() {
                let overlay = self.ocr_overlay.as_ref().expect("查找原位来源已核验");
                let (result_generation, content_revision) = self.ocr_find_result_identity();
                let ui = FindUi::open(
                    OcrFindStamp {
                        source: overlay.target.clone(),
                        result_generation,
                        content_revision,
                    },
                    &overlay.text.text,
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
                    v.capture_find_query_event(w, cx)
                });
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
        self.capture_find_refresh(w, cx);
        self.apply_capture_find_action(action, None, w, cx);
    }
    fn apply_capture_find_action(
        &mut self,
        action: FindAction,
        pressed: Option<&CaptureFindStamp>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let access = self.capture_find_access(w, cx);
        if !access.active
            || !access.idle
            || !access.source_current
            || access.composing
            || pressed.is_some_and(|stamp| !self.capture_find_click_current(stamp, w, cx, false))
        {
            return;
        }
        let Some(clip) = self.capture_find_clip() else {
            return;
        };
        let Some(mark_choice) = self.ocr_find.as_ref().map(FindUi::mark_choice_stamp) else {
            return;
        };
        let mark = mark_choice.kind();
        let mark_style = self.capture_find_mark_style(mark);
        let Some(ui) = &mut self.ocr_find else {
            return;
        };
        let text = &self.ocr_overlay.as_ref().expect("原位来源已核验").text;
        match action {
            FindAction::ToggleMarkKinds | FindAction::SetMarkKind(_) => {
                ui.apply_mark_choice(action);
            }
            FindAction::Previous | FindAction::Next => {
                ui.cycle(action == FindAction::Next, text, clip, access);
            }
            FindAction::Select => {
                let Some(ticket) = ui.model.current_ticket(&ui.document, access) else {
                    return;
                };
                // 缺几何仍可选中原文复制；预览和持久标记继续严格核完整四角。
                let result = ui.model.resolve_ticket(&ticket, &ui.document, access);
                match result {
                    Ok(range) => {
                        ui.clear_preview();
                        self.ocr_overlay
                            .as_mut()
                            .expect("原位来源已核验")
                            .select_range(range);
                        self.cancel_local_ocr_intent("显式查找选择改变局部意图");
                        if w.is_window_active() {
                            w.focus(&self.focus);
                        }
                    }
                    Err(error) => ui.error = find_ui::error_text(error).into(),
                }
            }
            FindAction::MarkAll => {
                let Some(ticket) = ui.model.all_ticket(&ui.document, access) else {
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
                        if let Err(error) = self.apply_ocr_overlay_quads(mark, &quads, |v| {
                            let fresh = v.capture_find_access(w, cx);
                            v.capture_find_clip() == Some(clip)
                                && v.capture_find_mark_style(mark) == mark_style
                                && pressed.is_none_or(|stamp| {
                                    v.capture_find_click_current(stamp, w, cx, false)
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
                            self.validate_ocr_find();
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
    pub(crate) fn clear_find_preview(&mut self) {
        self.cancel_capture_find_click();
        self.cancel_local_ocr_intent("原结果手动选择改变局部意图");
        if let Some(ui) = &mut self.ocr_find {
            ui.clear_preview();
        }
    }
    /// 返回（图上命中预览与点击回执的绘制钩子，查找栏）；`dock` 为宿主面板内的
    /// （左，上，宽，可用高度），给定时查找栏嵌入面板布局流。
    pub(super) fn render_ocr_find(
        &self,
        scale: f32,
        w: &Window,
        cx: &Context<Self>,
        dock: Option<(f32, f32, f32, f32)>,
    ) -> Option<(AnyElement, AnyElement)> {
        if !scale.is_finite() || scale <= 0. || scale != w.scale_factor() {
            return None;
        }
        let projection = self.ocr_projection(w)?;
        let projection_paint = self.ocr_overlay_projection_paint(w)?;
        let work = self.ocr_ui_work(w)?;
        let ui = self.ocr_find.as_ref()?;
        let access = self.capture_find_access(w, cx);
        let clip = self.capture_find_clip()?;
        let quads = ui
            .model
            .location(&ui.document, clip, access)
            .map(|location| location.quads.clone())
            .unwrap_or_default();
        let preview = ui
            .model
            .current_ticket(&ui.document, access)
            .map(|ticket| (ticket, ui.document.clone()));
        let owner = cx.entity().downgrade();
        let click = self.capture_find_click.clone();
        let epoch = click.borrow_mut().begin_render();
        let retained = click
            .borrow()
            .payload()
            .map(|payload| payload.stamp.button.clone());
        // 只有 render 开始时已松开排队的 intent，才可领取该次后继 paint。
        let release_token = {
            let click = click.borrow();
            (!click.is_armed())
                .then(|| click.token().cloned())
                .flatten()
        };
        let target = self.ocr_overlay.as_ref()?.target.clone();
        let position = dock.map_or_else(
            || (work.x + 8., work.y + 8., (work.width - 16.).clamp(1., 420.)),
            |(x, y, width, _)| (x, y, width),
        );
        let available_height = dock.map_or_else(
            || (work.bottom() - position.1 - 8.).max(1.),
            |(_, _, _, height)| height.max(1.),
        );
        let bar = find_ui::bar(
            ui,
            access,
            position,
            true,
            cx,
            find_ui::FindBarBindings {
                action: Self::ocr_find_action,
                key: Self::ocr_find_capture_key,
                retained: retained.as_ref(),
                click: Self::capture_find_click,
                available_height,
                docked: dock.is_some(),
            },
        );
        Some((
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, cx| {
                            if !owner.upgrade().is_some_and(|owner| {
                                let view = owner.read(cx);
                                view.ocr_overlay_projection_paint_current(&projection_paint, window)
                                    && Rc::ptr_eq(&view.capture_find_click, &click)
                            }) {
                                return;
                            }
                            if !click.borrow().epoch_is_current(&epoch) {
                                return;
                            }
                            // 真正可操作的 paint 只预约缓存刷新；不会执行或复活旧按钮意图。
                            let query_refresh = owner.upgrade().and_then(|owner| {
                                let view = owner.read(cx);
                                let access = view.capture_find_access(window, cx);
                                if !access.active
                                    || !access.idle
                                    || !access.source_current
                                    || access.composing
                                    || view.current_ocr_target().as_ref() != Some(&target)
                                    || view.ocr_projection(window) != Some(projection)
                                    || view.ocr_overlay.as_ref().is_none_or(|overlay| {
                                        overlay.painted_projection.get() != Some(projection)
                                    })
                                    || window.scale_factor() != scale
                                    || view.capture_find_clip() != Some(clip)
                                {
                                    return None;
                                }
                                let ui = view.ocr_find.as_ref()?;
                                let ticket = ui.query_refresh_ticket()?;
                                ui.queue_query_refresh(&ticket).then_some(ticket)
                            });
                            if let Some(ticket) = query_refresh {
                                let owner = owner.clone();
                                window.on_next_frame(move |w, cx| {
                                    let _ = owner.update(cx, |view, cx| {
                                        view.refresh_capture_find_query(&ticket, w, cx)
                                    });
                                });
                                window.request_animation_frame();
                            }
                            let intent = click
                                .borrow()
                                .payload()
                                .map(|payload| (payload.stamp.clone(), payload.focus.clone()));
                            let held = unsafe { GetCapture() } == hwnd(window)
                                && find_ui::primary_button_down();
                            let click_current = intent.as_ref().is_some_and(|(stamp, focus)| {
                                owner.upgrade().is_some_and(|owner| {
                                    let view = owner.read(cx);
                                    focus.is_focused(window)
                                        && view.capture_find_click_current(stamp, window, cx, held)
                                })
                            });
                            if intent.is_some() && !click_current {
                                click.borrow_mut().cancel();
                            }
                            let release_current = {
                                let click = click.borrow();
                                find_ui::release_current_for_render(&click, release_token.as_ref())
                            };
                            if click_current
                                && release_current
                                && !held
                                && unsafe { GetCapture() }.0.is_null()
                                && !find_ui::primary_button_down()
                            {
                                if let Some(ticket) = click.borrow_mut().painted(&epoch) {
                                    let owner = owner.clone();
                                    window.on_next_frame(move |w, cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.run_capture_find_click(&ticket, w, cx)
                                        });
                                    });
                                    window.request_animation_frame();
                                }
                            }
                            if let Some(token) = click.borrow().token().cloned() {
                                let click = click.clone();
                                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                    if phase.bubble() && event.button == MouseButton::Left {
                                        let click = click.clone();
                                        let token = Arc::clone(&token);
                                        cx.defer(move |_| {
                                            click.borrow_mut().release_outside(&token)
                                        });
                                    }
                                });
                            }
                            let allowed = owner.upgrade().is_some_and(|owner| {
                                let view = owner.read(cx);
                                let a = view.capture_find_access(window, cx);
                                a.active
                                    && a.idle
                                    && a.source_current
                                    && !a.composing
                                    && view.current_ocr_target().as_ref() == Some(&target)
                                    && view.ocr_projection(window) == Some(projection)
                                    && view.ocr_overlay.as_ref().is_some_and(|overlay| {
                                        overlay.painted_projection.get() == Some(projection)
                                    })
                                    && window.scale_factor() == scale
                                    && view.capture_find_clip() == Some(clip)
                                    && preview.as_ref().is_some_and(|(ticket, document)| {
                                        view.ocr_find.as_ref().is_some_and(|ui| {
                                            ui.model.resolve_ticket(ticket, document, a).is_ok()
                                        })
                                    })
                            });
                            if !allowed {
                                return;
                            }
                            for quad in &quads {
                                let points = quad.map(|p| {
                                    let p = projection.point(p);
                                    point(px(p.x), px(p.y))
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
                )
                .into_any_element(),
            bar,
        ))
    }
}
