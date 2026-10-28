//! 仅 Capture：固定实际 OCR 输入、同提供者后台局部行任务及独立只读候选。
use super::*;
use crate::{
    capture::Image,
    capture_view::{
        local_retry::{
            candidate::{
                self, clamp_to_viewport, copy_notice, drag_tick, retained_image, retained_result,
                reveal_scroll, valid_scroll, CandidateCommandKind, CandidatePanel as Panel,
                CandidateScrollDrag, CandidateScrollPaint, EdgeScroll, RevealScroll,
            },
            Binding, Completion, LocalRetry, SourceFrame, TargetError,
        },
        text_result::readonly_visual_navigation::VerticalState,
    },
    pin_view::find_ui::primary_button_down,
    recognition::{OcrProvider, RecognizedText},
};
use gpui::{App, FocusHandle, Subscription};
use std::time::Instant;

pub(crate) struct FixedInput {
    image: Arc<Image>,
    source: OcrTarget,
    identity: Arc<()>,
    frame: SourceFrame,
    provider: OcrProvider,
    profile: crate::ppocr::OcrProfile,
}

impl FixedInput {
    #[cfg(test)]
    pub(crate) fn copy_of(image: &Image, source: OcrTarget, provider: OcrProvider) -> Option<Self> {
        Self::copy_of_profile(image, source, provider, crate::ppocr::OcrProfile::Tiny)
    }

    pub(crate) fn copy_of_profile(
        image: &Image,
        source: OcrTarget,
        provider: OcrProvider,
        profile: crate::ppocr::OcrProfile,
    ) -> Option<Self> {
        if !source
            .selected
            .valid(source.canvas.width, source.canvas.height)
        {
            return None;
        }
        let image = retained_image(image, (source.selected.width, source.selected.height))?;
        let x = source
            .canvas
            .left
            .checked_add(i32::try_from(source.selected.x).ok()?)?;
        let y = source
            .canvas
            .top
            .checked_add(i32::try_from(source.selected.y).ok()?)?;
        x.checked_add(i32::try_from(image.width).ok()?)?;
        y.checked_add(i32::try_from(image.height).ok()?)?;
        Some(Self {
            frame: SourceFrame {
                width: image.width,
                height: image.height,
                origin: (x, y),
            },
            image,
            source,
            identity: Arc::new(()),
            provider,
            profile,
        })
    }
}

type CandidatePanel = Panel<CandidateScrolling>;
type CandidateButtonStamp = candidate::CandidateButtonStamp<OcrTarget>;
type CandidateCommand = candidate::CandidateCommand<OcrTarget>;

#[derive(Default)]
struct CandidateScrolling {
    pointer: Option<CandidatePointer>,
    painted: Option<CandidateScrollPaint>,
    drag: Option<CandidateScrollDrag>,
    pressed: bool,
    scheduled: Option<Arc<()>>,
    reveal: bool,
    rehit: bool,
    command: Option<CandidateCommand>,
}

struct CandidatePointer {
    token: Arc<()>,
    down: inline_click::DownProof,
    kind: CandidateCommandKind,
    stamp: CandidateButtonStamp,
    projection: crate::ocr_projection::ProjectionPaint,
    focus: FocusHandle,
    _blur: Subscription,
    _activation: Subscription,
}

pub(crate) struct LocalOcrState {
    fixed: FixedInput,
    binding: Binding<OcrTarget>,
    retry: LocalRetry<OcrTarget>,
    panel: Option<CandidatePanel>,
    focus: FocusHandle,
    title: String,
    notice: String,
    visible: bool,
    _activation: Option<Subscription>, // 持有至该固定输入失效，Drop 注销观察器。
    retry_clicks: Rc<RefCell<RetryClicks>>,
    retry_line: Option<(Range<usize>, usize, crate::geometry::Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryClickKind {
    Selection,
    WithoutPanel,
}

#[derive(Clone)]
pub(super) struct RetryClickStamp {
    kind: RetryClickKind,
    session: Arc<()>,
    source: OcrTarget,
    source_identity: Arc<()>,
    body_identity: Arc<()>,
    revision: u64,
    generation: u64,
    ui_generation: u64,
    overlay: Rc<RefCell<Option<OcrTarget>>>,
    selection: Option<Range<usize>>,
    keyboard_selection: Option<KeyboardSelection>,
    selected: Range<usize>,
    retry_line: Option<(Range<usize>, usize, crate::geometry::Rect)>,
}

fn retry_click_stamp_matches(a: &RetryClickStamp, b: &RetryClickStamp) -> bool {
    a.kind == b.kind
        && Arc::ptr_eq(&a.session, &b.session)
        && a.source == b.source
        && Arc::ptr_eq(&a.source_identity, &b.source_identity)
        && Arc::ptr_eq(&a.body_identity, &b.body_identity)
        && a.revision == b.revision
        && a.generation == b.generation
        && a.ui_generation == b.ui_generation
        && Rc::ptr_eq(&a.overlay, &b.overlay)
        && a.selection == b.selection
        && a.keyboard_selection == b.keyboard_selection
        && a.selected == b.selected
        && a.retry_line == b.retry_line
}

#[derive(Default)]
struct RetryClicks {
    session: Arc<()>,
    epoch: Arc<()>,
    painted: Option<RetryClickPaint>,
    armed: Option<RetryClick>,
    command: Option<RetryClick>,
}

impl RetryClicks {
    fn cancel(&mut self) {
        // 换选 A→B→A 也不能复用旧 Down，而非只比较最终相同的 byte 范围。
        self.session = Arc::new(());
        self.epoch = Arc::new(());
        self.armed = None;
        self.command = None;
        self.painted = None;
    }

    fn begin_render(&mut self) -> Arc<()> {
        self.epoch = Arc::new(());
        self.painted = None;
        Arc::clone(&self.epoch)
    }
}

#[derive(Clone)]
struct RetryClickPaint {
    epoch: Arc<()>,
    size: [f32; 2],
    scale: f32,
    projection: Option<crate::ocr_projection::OcrProjection>,
    offset: [f32; 2],
}

impl RetryClickPaint {
    fn current(
        &self,
        epoch: &Arc<()>,
        size: [f32; 2],
        scale: f32,
        projection: Option<crate::ocr_projection::OcrProjection>,
        offset: [f32; 2],
    ) -> bool {
        Arc::ptr_eq(&self.epoch, epoch)
            && self.size == size
            && self.scale == scale
            && self.projection == projection
            && projection.is_some()
            && self.offset == offset
            && size.into_iter().chain(offset).all(f32::is_finite)
            && size.into_iter().all(|v| v > 0.)
            && scale.is_finite()
            && scale > 0.
    }
}

struct RetryClick {
    down: inline_click::DownProof,
    token: Arc<()>,
    stamp: RetryClickStamp,
    focus: FocusHandle,
    _blur: Subscription,
    scheduled: bool,
}

impl CaptureView {
    pub(crate) fn install_local_ocr_input(
        &mut self,
        fixed: Option<FixedInput>,
        result: RecognizedText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fixed) = fixed else {
            return;
        };
        if !self.inline_ocr_host()
            || self.ocr_overlay.is_none()
            || self.current_ocr_target().as_ref() != Some(&fixed.source)
            || self.current_result_text() != Some(result.text.as_str())
            || !retained_result(&result)
        {
            return;
        }
        let body_identity = self.current_result_body_identity();
        let (_, revision) = self.ocr_find_result_identity();
        let binding = Binding {
            source: fixed.source.clone(),
            source_identity: Arc::clone(&fixed.identity),
            result: Arc::new(result),
            result_generation: self.ocr_result_installation,
            installed_body_identity: Arc::clone(&body_identity),
            body_identity,
            installed_body_revision: revision,
            body_revision: revision,
            frame: fixed.frame,
            provider: fixed.provider,
        };
        self.local_ocr = Some(LocalOcrState {
            fixed,
            binding,
            retry: LocalRetry::default(),
            panel: None,
            focus: cx.focus_handle(),
            title: String::new(),
            notice: String::new(),
            visible: false,
            _activation: None,
            retry_clicks: Default::default(),
            retry_line: None,
        });
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            view.sync_local_ocr(window, cx);
            cx.notify();
        });
        self.local_ocr.as_mut().expect("局部输入已安装")._activation = Some(activation);
    }

    fn local_binding(&self) -> Option<Binding<OcrTarget>> {
        let state = self.local_ocr.as_ref()?;
        let mut binding = state.binding.clone();
        binding.source = self.current_ocr_target()?;
        binding.result_generation = self.ocr_result_installation;
        binding.body_revision = self.ocr_find_result_identity().1;
        binding.body_identity = self.current_result_body_identity();
        binding.provider = OcrProvider::default();
        Some(binding)
    }

    fn local_input_current(&self) -> bool {
        let Some(state) = &self.local_ocr else {
            return false;
        };
        let identity = self.current_result_body_identity();
        self.inline_ocr_host()
            && self.has_ocr_result()
            && self.current_ocr_target().as_ref() == Some(&state.fixed.source)
            && self.ocr_result_installation == state.binding.result_generation
            && self.ocr_find_result_identity().1 == state.binding.installed_body_revision
            && Arc::ptr_eq(&identity, &state.binding.installed_body_identity)
            && state.fixed.provider == OcrProvider::default()
    }

    pub(crate) fn local_ocr_candidate_focused(&self, w: &Window) -> bool {
        self.local_ocr
            .as_ref()
            .is_some_and(|s| s.panel.is_some() && s.focus.is_focused(w))
    }

    fn local_action_ready(&self, w: &Window, cx: &App) -> bool {
        self.local_source_action_ready(w, cx)
            && (unsafe { GetCapture() }.0.is_null()
                || (unsafe { GetCapture() } == hwnd(w)
                    && primary_button_down()
                    && self
                        .local_ocr
                        .as_ref()
                        .and_then(|s| s.panel.as_ref())
                        .is_some_and(|p| p.scrolling.borrow().pressed && p.editor.dragging())))
    }

    fn local_source_action_ready(&self, w: &Window, cx: &App) -> bool {
        w.is_window_active()
            && self.ocr_find_focus_owned(w, cx)
            && self.local_input_current()
            && self.ocr_overlay_ready()
            && self.result_selection_pointer_ready()
            && !self.result_selection_dragging(self.result_selection_generation())
            && self.text_copy_completion_is_idle()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && !self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx))
            && self.ocr_overlay.as_ref().is_some_and(|o| {
                o.anchor.is_none() && o.capture.is_none() && o.keyboard_target_painted()
            })
    }

    pub(crate) fn sync_local_ocr(&mut self, w: &Window, _: &App) {
        // Render 只核真实身份／版本；不逐帧复制输入、比较全文或分析行词。
        if self.local_ocr.is_some() && !self.local_input_current() {
            self.local_ocr = None;
        } else if !w.is_window_active() {
            self.cancel_local_ocr_intent("窗口失活，撤销独立局部候选");
        }
    }

    /// 已保存单行 ROI 从固定输入坐标转为全屏坐标，动效不采样或复制像素。
    pub(crate) fn local_ocr_progress(&self) -> Option<(u64, crate::geometry::Rect)> {
        if !self.local_input_current() {
            return None;
        }
        let state = self.local_ocr.as_ref().filter(|state| state.visible)?;
        let (id, target) = state.retry.pending_visual()?;
        let selected = state.fixed.source.selected;
        let roi = target.roi();
        let projected =
            crate::capture_view::ocr_progress::local_roi(selected, roi, self.source.full())?;
        Some((id, projected))
    }

    pub(crate) fn cancel_local_ocr_intent(&mut self, reason: &str) {
        if let Some(state) = &mut self.local_ocr {
            state.retry_clicks.borrow_mut().cancel();
            state.retry_line = None;
            state.retry.cancel(reason);
            state.panel = None;
            state.visible = false;
            state.notice.clear();
        }
    }

    pub(super) fn local_ocr_line_click_stamp(&self) -> Option<RetryClickStamp> {
        self.retry_click_stamp(RetryClickKind::Selection)
    }

    fn retry_click_stamp(&self, kind: RetryClickKind) -> Option<RetryClickStamp> {
        if !self.local_input_current() || self.result_editor.is_some() {
            return None;
        }
        let state = self.local_ocr.as_ref()?;
        let overlay = self.ocr_overlay.as_ref()?;
        let selected = match kind {
            RetryClickKind::Selection => overlay.selected.clone().filter(|r| !r.is_empty())?,
            RetryClickKind::WithoutPanel => {
                if !state.visible || state.panel.is_some() {
                    return None;
                }
                state.retry_line.as_ref()?.0.clone()
            }
        };
        Some(RetryClickStamp {
            kind,
            session: Arc::clone(&state.retry_clicks.borrow().session),
            source: state.fixed.source.clone(),
            source_identity: Arc::clone(&state.fixed.identity),
            body_identity: self.current_result_body_identity(),
            revision: self.ocr_find_result_identity().1,
            generation: self.ocr_result_installation,
            ui_generation: self.result_selection_generation(),
            overlay: Rc::clone(&overlay.painted_keyboard_target),
            selection: overlay.selected.clone(),
            keyboard_selection: overlay.keyboard_selection,
            selected,
            retry_line: (kind == RetryClickKind::WithoutPanel)
                .then(|| state.retry_line.clone())
                .flatten(),
        })
    }

    fn retry_click_current(&self, stamp: &RetryClickStamp) -> bool {
        self.retry_click_stamp(stamp.kind)
            .is_some_and(|current| retry_click_stamp_matches(stamp, &current))
    }

    pub(super) fn cancel_pending_local_retry_click(&mut self) {
        self.cancel_ocr_local_retry_pointer_click();
        if let Some(state) = &self.local_ocr {
            state.retry_clicks.borrow_mut().cancel();
            if let Some(panel) = &state.panel {
                panel.visual.borrow_mut().begin_render();
                panel.painted.borrow_mut().take();
                panel.scrolling.borrow_mut().painted = None;
            }
        }
    }
    pub(crate) fn cancel_ocr_local_retry_pointer_click(&self) {
        if let Some(state) = &self.local_ocr {
            let mut clicks = state.retry_clicks.borrow_mut();
            clicks.armed = None;
            clicks.command = None;
            if let Some(panel) = &state.panel {
                let mut scrolling = panel.scrolling.borrow_mut();
                scrolling.pointer = None;
                scrolling.command = None;
                scrolling.scheduled = None;
            }
        }
    }

    fn cancel_local_retry_click_token(&mut self, token: &Arc<()>) {
        if let Some(state) = &self.local_ocr {
            let mut clicks = state.retry_clicks.borrow_mut();
            if clicks
                .armed
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(&c.token, token))
                || clicks
                    .command
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(&c.token, token))
            {
                clicks.cancel();
            }
        }
    }

    pub(super) fn arm_local_retry_click(
        &mut self,
        stamp: &RetryClickStamp,
        event: &MouseDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Windows Down 已 SetCapture；只授权这个按钮的本窗口主键按下。
        self.cancel_ocr_local_retry_pointer_click();
        if event.first_mouse
            || event.button != MouseButton::Left
            || !self.local_source_action_ready(w, cx)
            || unsafe { GetCapture() } != hwnd(w)
            || !primary_button_down()
            || !self.retry_click_current(stamp)
        {
            return;
        }
        let Some(focus) = w.focused(cx) else { return };
        let token = Arc::new(());
        let blur_token = Arc::clone(&token);
        let blur = cx.on_blur(&focus, w, move |view, _, cx| {
            view.cancel_local_retry_click_token(&blur_token);
            cx.notify();
        });
        let state = self.local_ocr.as_ref().unwrap();
        if let Some(panel) = &state.panel {
            panel.scrolling.borrow_mut().command = None;
        }
        let mut clicks = state.retry_clicks.borrow_mut();
        clicks.command = None;
        clicks.armed = Some(RetryClick {
            down: pointer_down_proof(event),
            token,
            stamp: stamp.clone(),
            focus,
            _blur: blur,
            scheduled: false,
        });
    }

    pub(super) fn queue_local_retry_click(
        &mut self,
        stamp: &RetryClickStamp,
        event: &gpui::ClickEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (actual, left_pair) = pointer_click_proof(event);
        let armed = self
            .local_ocr
            .as_ref()
            .and_then(|state| state.retry_clicks.borrow_mut().armed.take());
        let Some(armed) = armed.filter(|armed| left_pair && actual.as_ref() == Some(&armed.down))
        else {
            self.cancel_ocr_local_retry_pointer_click();
            return;
        };
        if !self.local_source_action_ready(w, cx)
            || !unsafe { GetCapture() }.0.is_null()
            || primary_button_down()
            || !self.retry_click_current(stamp)
        {
            self.cancel_pending_local_retry_click();
            return;
        }
        let clicks = &self.local_ocr.as_ref().unwrap().retry_clicks;
        let mut clicks = clicks.borrow_mut();
        if !retry_click_stamp_matches(&armed.stamp, stamp) || !armed.focus.is_focused(w) {
            return;
        }
        clicks.painted = None;
        clicks.command = Some(armed);
        cx.notify();
    }

    fn run_queued_local_retry_click(
        &mut self,
        token: &Arc<()>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &self.local_ocr else { return };
        let command = {
            let mut clicks = state.retry_clicks.borrow_mut();
            if !clicks
                .command
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(&c.token, token))
            {
                return;
            }
            clicks.command.take()
        };
        let Some(command) = command else { return };
        let offset = self.canvas_offset(w);
        let painted = self.local_ocr.as_ref().is_some_and(|state| {
            let clicks = state.retry_clicks.borrow();
            clicks.painted.as_ref().is_some_and(|painted| {
                painted.current(
                    &clicks.epoch,
                    [
                        f32::from(w.viewport_size().width),
                        f32::from(w.viewport_size().height),
                    ],
                    w.scale_factor(),
                    self.ocr_projection(w),
                    [f32::from(offset.x), f32::from(offset.y)],
                )
            })
        });
        if !painted
            || !self.local_action_ready(w, cx)
            || !unsafe { GetCapture() }.0.is_null()
            || primary_button_down()
            || !command.focus.is_focused(w)
            || !self.retry_click_current(&command.stamp)
        {
            return;
        }
        if command.stamp.kind == RetryClickKind::WithoutPanel {
            let Some(binding) = self.local_binding() else {
                return;
            };
            let actual = self.local_ocr.as_ref().and_then(|state| {
                state
                    .retry
                    .target(&binding, self.current_result_text()?, true)
                    .map(|target| (target.selected(), target.line_index(), target.roi()))
            });
            if actual != command.stamp.retry_line {
                return;
            }
        }
        // 当前 token 已消费；仍由原 begin/固定输入/同 provider 流程验证并启动。
        self.run_local_ocr(command.stamp.selected, w, cx);
    }

    pub(super) fn render_local_retry_click_paint(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.local_ocr.as_ref()?;
        let projection_paint = self.ocr_overlay_projection_paint(window)?;
        let clicks = Rc::clone(&state.retry_clicks);
        let epoch = clicks.borrow_mut().begin_render();
        let owner = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, w, cx| {
                    if !owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        view.ocr_overlay_projection_paint_current(&projection_paint, w)
                            && view
                                .local_ocr
                                .as_ref()
                                .is_some_and(|state| Rc::ptr_eq(&state.retry_clicks, &clicks))
                    }) {
                        return;
                    }
                    if !Arc::ptr_eq(&epoch, &clicks.borrow().epoch) {
                        return;
                    }
                    let intent = {
                        let clicks = clicks.borrow();
                        clicks
                            .command
                            .as_ref()
                            .or(clicks.armed.as_ref())
                            .map(|command| {
                                (
                                    command.stamp.clone(),
                                    command.focus.clone(),
                                    Arc::clone(&command.token),
                                )
                            })
                    };
                    let source_ready = owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        view.local_source_action_ready(w, cx)
                            && view
                                .local_ocr
                                .as_ref()
                                .is_some_and(|state| Rc::ptr_eq(&state.retry_clicks, &clicks))
                            && intent.as_ref().is_none_or(|(stamp, focus, _)| {
                                view.retry_click_current(stamp) && focus.is_focused(w)
                            })
                    });
                    let frame = owner.upgrade().map(|owner| {
                        let view = owner.read(cx);
                        (view.canvas_offset(w), view.ocr_projection(w))
                    });
                    let mut queue = clicks.borrow_mut();
                    if !source_ready {
                        queue.cancel();
                        return;
                    }
                    let Some((offset, projection)) = frame else {
                        return;
                    };
                    let painted = RetryClickPaint {
                        epoch: Arc::clone(&epoch),
                        size: [
                            f32::from(w.viewport_size().width),
                            f32::from(w.viewport_size().height),
                        ],
                        scale: w.scale_factor(),
                        projection,
                        offset: [f32::from(offset.x), f32::from(offset.y)],
                    };
                    let allowed = unsafe { GetCapture() }.0.is_null()
                        && !primary_button_down()
                        && painted.current(
                            &epoch,
                            painted.size,
                            painted.scale,
                            painted.projection,
                            painted.offset,
                        );
                    queue.painted = allowed.then_some(painted);
                    if allowed {
                        if let Some(command) =
                            queue.command.as_mut().filter(|command| !command.scheduled)
                        {
                            command.scheduled = true;
                            let token = Arc::clone(&command.token);
                            let owner = owner.clone();
                            w.on_next_frame(move |w, cx| {
                                let _ = owner.update(cx, |view, cx| {
                                    view.run_queued_local_retry_click(&token, w, cx)
                                });
                            });
                            w.request_animation_frame();
                        }
                    }
                    drop(queue);
                    if let Some((_, _, token)) = intent {
                        let up_owner = owner.clone();
                        let up_token = Arc::clone(&token);
                        w.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase.bubble() && event.button == MouseButton::Left {
                                let owner = up_owner.clone();
                                let token = Arc::clone(&up_token);
                                // 普通 on_click 已将有效 arm 移到 command；松开到按钮外仅清 arm。
                                cx.defer(move |cx| {
                                    let _ = owner.update(cx, |view, _| {
                                        if let Some(state) = &view.local_ocr {
                                            let mut queue = state.retry_clicks.borrow_mut();
                                            if queue
                                                .armed
                                                .as_ref()
                                                .is_some_and(|c| Arc::ptr_eq(&c.token, &token))
                                            {
                                                queue.armed = None;
                                            }
                                        }
                                    });
                                });
                            }
                        });
                        let owner = owner.clone();
                        w.on_key_event(move |_: &KeyDownEvent, phase, _, cx| {
                            if phase.capture() {
                                let _ = owner.update(cx, |view, cx| {
                                    view.cancel_local_retry_click_token(&token);
                                    cx.notify();
                                });
                            }
                        });
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element(),
        )
    }

    fn run_local_ocr(&mut self, selected: Range<usize>, w: &mut Window, cx: &mut Context<Self>) {
        self.sync_local_ocr(w, cx);
        if !self.local_action_ready(w, cx) {
            return;
        }
        if self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .is_some_and(|p| p.editor.dragging())
        {
            return;
        }
        let Some(binding) = self.local_binding() else {
            return;
        };
        let candidate_was_focused = self.local_ocr_candidate_focused(w);
        let body = self.current_result_text().expect("局部当前正文").to_owned();
        let state = self.local_ocr.as_mut().expect("局部输入已核验");
        state.retry_clicks.borrow_mut().cancel();
        let request = match state.retry.begin(&binding, &body, selected, 4, true) {
            Ok(request) => request,
            Err(error) => {
                self.status = target_error(error).into();
                cx.notify();
                return;
            }
        };
        // 来自本次 DTO 请求保存的实际固定 Vec：维度校验只是最后一道预算门。
        let image = match request.target().extract_pixels(&state.fixed.image) {
            Ok(image) => image,
            Err(error) => {
                state.retry.cancel("固定输入采样失败");
                state.notice = target_error(error).into();
                state.panel = None;
                state.visible = true;
                cx.notify();
                return;
            }
        };
        state.title = trf!(
            "第{}行候选 · 不替换原文",
            "Line {} candidate · keeps original",
            request.target().line_index() + 1
        );
        state.retry_line = Some((
            request.target().selected(),
            request.target().line_index(),
            request.target().roi(),
        ));
        // 显式重试撤掉有焦点的候选；把本窗口输入归属还给原 OCR 层。
        // 后台回执只安装候选，不再改变焦点。
        if candidate_was_focused && w.is_window_active() {
            w.focus(&self.focus);
        }
        state.notice = if state.retry.pending(&binding, &body, true) {
            tr!("正在本机重识别此行…", "Retrying this line locally…").into()
        } else {
            String::new()
        };
        state.panel = None;
        state.visible = true;
        let token = request.token().clone();
        let provider = request.provider();
        // 行候选复用产生原 DTO 的固定输入档位；设置变化不替换引擎或回退 Tiny。
        let profile = state.fixed.profile;
        let task = cx.background_executor().spawn(async move {
            crate::capture_view::ocr_task::Receipt::from_result(
                crate::recognition::recognize_with_profile_cancellable(
                    image, provider, profile, &token,
                ),
            )
        });
        cx.spawn_in(w, async move |view, cx| {
            let receipt = task.await;
            let _ = view.update_in(cx, |view, w, cx| {
                view.sync_local_ocr(w, cx);
                let Some(binding) = view.local_binding() else {
                    return;
                };
                let ready = view.local_action_ready(w, cx);
                let body = view
                    .current_result_text()
                    .expect("候选回执当前正文")
                    .to_owned();
                let Some(state) = &mut view.local_ocr else {
                    return;
                };
                match state
                    .retry
                    .complete(&request, &binding, &body, ready, receipt)
                {
                    Completion::Stale => return,
                    Completion::Ready => {
                        let Some(candidate) = state.retry.candidate(&binding, &body, ready) else {
                            return;
                        };
                        state.panel = Some(CandidatePanel::new(
                            Arc::new(()),
                            candidate.text().into(),
                            None,
                        ));
                        state.notice = tr!(
                            "拖选候选后 Ctrl+C；原文和标记保持",
                            "Select candidate, then Ctrl+C; original kept"
                        )
                        .into();
                    }
                    Completion::Failed => {
                        state.notice = trf!(
                            "重识别失败：{}",
                            "Retry failed: {}",
                            state.retry.error(&binding, &body, ready).unwrap_or("")
                        )
                    }
                    Completion::Cancelled | Completion::Invalidated => {
                        state.panel = None;
                        state.visible = false;
                    }
                }
                // 返回只安装局部只读候选：不抢焦点，不提交原文／Scene／选择／历史。
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn candidate_frame_ready(&self, frame: &Arc<()>, w: &Window, cx: &App) -> bool {
        self.local_action_ready(w, cx)
            && self
                .local_ocr
                .as_ref()
                .and_then(|s| s.panel.as_ref())
                .is_some_and(|p| p.painted_frame(frame))
    }

    fn copy_local_candidate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !unsafe { GetCapture() }.0.is_null() {
            return;
        }
        let Some(frame) = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .map(|p| Arc::clone(&p.identity))
        else {
            return;
        };
        if !self.candidate_frame_ready(&frame, w, cx) {
            return;
        }
        let Some(binding) = self.local_binding() else {
            return;
        };
        let body = self
            .current_result_text()
            .expect("复制候选当前正文")
            .to_owned();
        let state = self.local_ocr.as_mut().expect("候选已核验");
        let panel = state.panel.as_ref().expect("候选已核验");
        if panel.editor.dragging() {
            return;
        }
        let range = panel.editor.input.selection.clone();
        let result = state.retry.copy_candidate(&binding, &body, true, |text| {
            crate::export::copy_text(candidate::candidate_payload(text, range), hwnd(w))
        });
        state.notice = copy_notice(
            result,
            tr!(
                "候选已复制；截图保持打开",
                "Candidate copied; capture stays open"
            ),
        );
        cx.notify();
    }

    fn stop_local_candidate_scroll(&mut self, frame: &Arc<()>) {
        if let Some(panel) = self.local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            if Arc::ptr_eq(frame, &panel.identity) {
                panel.editor.end_drag();
                panel.vertical = VerticalState::default();
                let mut scrolling = panel.scrolling.borrow_mut();
                scrolling.drag = None;
                scrolling.pressed = false;
                scrolling.scheduled = None;
                scrolling.reveal = false;
                scrolling.rehit = false;
                scrolling.command = None;
            }
        }
    }

    fn local_candidate_button_current(&self, stamp: &CandidateButtonStamp) -> bool {
        self.local_ocr.as_ref().is_some_and(|state| {
            state.visible
                && state.panel.as_ref().is_some_and(|panel| {
                    stamp.matches(
                        panel,
                        &state.fixed.identity,
                        &state.binding.source,
                        state.binding.result_generation,
                    )
                })
        })
    }

    fn retry_local_candidate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let selected = self.local_binding().and_then(|binding| {
            self.local_ocr.as_ref().and_then(|state| {
                state
                    .retry
                    .target(
                        &binding,
                        self.current_result_text().unwrap_or(""),
                        self.local_action_ready(w, cx),
                    )
                    .map(|target| target.selected())
            })
        });
        if let Some(selected) = selected {
            self.run_local_ocr(selected, w, cx);
        }
    }

    fn arm_local_candidate_pointer(
        &self,
        kind: CandidateCommandKind,
        stamp: &CandidateButtonStamp,
        event: &MouseDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_ocr_local_retry_pointer_click();
        if event.first_mouse
            || event.button != MouseButton::Left
            || !self.local_source_action_ready(w, cx)
            || !self.ocr_typed_pointer_capture_current(w, true)
            || !self.local_candidate_button_current(stamp)
        {
            return;
        }
        let Some(projection) = self.ocr_overlay_projection_paint(w) else {
            return;
        };
        let Some(panel) = self
            .local_ocr
            .as_ref()
            .and_then(|state| state.panel.as_ref())
        else {
            return;
        };
        if !panel
            .painted
            .borrow()
            .as_ref()
            .is_some_and(|frame| Arc::ptr_eq(frame, &stamp.frame))
        {
            return;
        }
        let Some(focus) = w.focused(cx) else {
            return;
        };
        let token = Arc::new(());
        let scrolling = Rc::downgrade(&panel.scrolling);
        let blur_token = token.clone();
        let blur = cx.on_blur(&focus, w, move |_, _, _| {
            if let Some(scrolling) = scrolling.upgrade() {
                let mut scrolling = scrolling.borrow_mut();
                if scrolling
                    .pointer
                    .as_ref()
                    .is_some_and(|pointer| Arc::ptr_eq(&pointer.token, &blur_token))
                {
                    scrolling.pointer = None;
                }
            }
        });
        let scrolling = Rc::downgrade(&panel.scrolling);
        let activation_token = token.clone();
        let activation = cx.observe_window_activation(w, move |_, w, _| {
            if !w.is_window_active() {
                if let Some(scrolling) = scrolling.upgrade() {
                    let mut scrolling = scrolling.borrow_mut();
                    if scrolling
                        .pointer
                        .as_ref()
                        .is_some_and(|pointer| Arc::ptr_eq(&pointer.token, &activation_token))
                    {
                        scrolling.pointer = None;
                    }
                }
            }
        });
        panel.scrolling.borrow_mut().pointer = Some(CandidatePointer {
            token,
            down: pointer_down_proof(event),
            kind,
            stamp: stamp.clone(),
            projection,
            focus,
            _blur: blur,
            _activation: activation,
        });
    }
    fn queue_local_candidate_command(
        &mut self,
        kind: CandidateCommandKind,
        stamp: &CandidateButtonStamp,
        event: &gpui::ClickEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pointer = self
            .local_ocr
            .as_ref()
            .and_then(|state| state.panel.as_ref())
            .and_then(|panel| panel.scrolling.borrow_mut().pointer.take());
        let (actual, left_pair) = pointer_click_proof(event);
        let Some(pointer) = pointer.filter(|pointer| {
            left_pair
                && actual.as_ref() == Some(&pointer.down)
                && pointer.kind == kind
                && pointer.focus.is_focused(w)
                && self.local_candidate_button_current(&pointer.stamp)
                && self.ocr_overlay_projection_paint_current(&pointer.projection, w)
        }) else {
            return;
        };
        if !self.local_source_action_ready(w, cx)
            || !unsafe { GetCapture() }.0.is_null()
            || !self.local_candidate_button_current(stamp)
        {
            return;
        }
        let stamp = &pointer.stamp;
        let Some(focus) = w.focused(cx) else { return };
        let token = Arc::new(());
        let blur_token = Arc::clone(&token);
        let blur_frame = Arc::clone(&stamp.frame);
        let blur = cx.on_blur(&focus, w, move |view, _, cx| {
            view.cancel_local_candidate_command(&blur_frame, &blur_token);
            cx.notify();
        });
        let panel = self.local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.drag = None;
        scrolling.reveal = false;
        scrolling.rehit = false;
        scrolling.scheduled = None;
        scrolling.command = Some(CandidateCommand {
            token,
            kind,
            stamp: stamp.clone(),
            focus,
            _blur: blur,
            scheduled: false,
        });
        cx.notify();
    }

    fn cancel_local_candidate_command(&mut self, frame: &Arc<()>, token: &Arc<()>) {
        if let Some(panel) = self.local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            if Arc::ptr_eq(&panel.identity, frame) {
                let mut scrolling = panel.scrolling.borrow_mut();
                if scrolling
                    .command
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(&c.token, token))
                {
                    scrolling.command = None;
                }
            }
        }
    }

    fn run_queued_local_candidate_command(
        &mut self,
        frame: &Arc<()>,
        token: &Arc<()>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = self.local_ocr.as_ref().and_then(|state| {
            let panel = state
                .panel
                .as_ref()
                .filter(|p| Arc::ptr_eq(&p.identity, frame))?;
            let mut scrolling = panel.scrolling.borrow_mut();
            if !scrolling
                .command
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(&c.token, token))
            {
                return None;
            }
            scrolling.command.take()
        });
        let Some(command) = command else { return };
        // 先消费当前 token，再重核原来源、当前真实 paint 和显式选择；旧回调不能重发。
        let ready = self.candidate_frame_ready(frame, w, cx)
            && unsafe { GetCapture() }.0.is_null()
            && command.focus.is_focused(w)
            && self.local_candidate_button_current(&command.stamp)
            && self
                .local_ocr
                .as_ref()
                .and_then(|s| s.panel.as_ref())
                .is_some_and(|p| Self::local_candidate_scroll_paint(p, w).is_some());
        if ready {
            match command.kind {
                CandidateCommandKind::Copy => self.copy_local_candidate(w, cx),
                CandidateCommandKind::Retry => self.retry_local_candidate(w, cx),
            }
        }
    }

    fn local_candidate_scroll_paint(
        panel: &CandidatePanel,
        w: &Window,
    ) -> Option<CandidateScrollPaint> {
        let paint = panel.scrolling.borrow().painted.clone()?;
        (valid_scroll(
            f32::from(paint.viewport.top()),
            f32::from(paint.viewport.bottom()),
            f32::from(paint.offset.y),
            f32::from(panel.scroll.max_offset().height),
        ) && f32::from(paint.viewport.left()).is_finite()
            && f32::from(paint.viewport.right()).is_finite()
            && paint.viewport.right() > paint.viewport.left()
            && paint.current(panel, w))
        .then_some(paint)
    }

    fn local_candidate_scroll_tick(
        &mut self,
        frame: &Arc<()>,
        token: &Arc<()>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scheduled = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .filter(|p| Arc::ptr_eq(&p.identity, frame))
            .is_some_and(|p| {
                p.scrolling
                    .borrow()
                    .scheduled
                    .as_ref()
                    .is_some_and(|t| Arc::ptr_eq(t, token))
            });
        if !scheduled {
            return;
        }
        if !self.candidate_frame_ready(frame, w, cx) || !self.local_ocr_candidate_focused(w) {
            self.stop_local_candidate_scroll(frame);
            return;
        }
        let panel = self.local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
        let Some(paint) = Self::local_candidate_scroll_paint(panel, w) else {
            panel.scrolling.borrow_mut().scheduled = None;
            return;
        };
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.scheduled = None;
        scrolling.rehit = false;
        let top = f32::from(paint.viewport.top());
        let bottom = f32::from(paint.viewport.bottom());
        let offset = f32::from(paint.offset.y);
        let max = f32::from(panel.scroll.max_offset().height);
        let mut changed = false;
        let next = if scrolling.reveal {
            scrolling.reveal = false;
            let caret = panel.editor.caret();
            let height = f32::from(paint.layout.line_height());
            paint.layout.position_for_index(caret).and_then(|position| {
                let mut caret_top = f32::from(position.y);
                let caret_bottom = caret_top + height;
                // 共享软边界同时让前/后两行可见，不反转现有 VerticalState 行归属。
                let hard_start = panel
                    .editor
                    .text
                    .get(..caret)?
                    .rfind('\n')
                    .map_or(0, |byte| byte + 1);
                let shaped = paint.layout.line_layout_for_index(caret)?;
                let shared = shaped.wrap_boundaries().iter().any(|boundary| {
                    shaped
                        .unwrapped_layout
                        .runs
                        .get(boundary.run_ix)
                        .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                        .is_some_and(|glyph| glyph.index == caret - hard_start)
                });
                if shared && bottom - top >= height * 2. {
                    caret_top = (caret_top - height).max(f32::from(paint.layout.bounds().top()));
                }
                reveal_scroll(RevealScroll {
                    viewport_top: top,
                    viewport_bottom: bottom,
                    offset_y: offset,
                    max_scroll_y: max,
                    caret_top,
                    caret_bottom,
                })
            })
        } else if let Some(drag) = scrolling.drag.as_mut().filter(|_| panel.editor.dragging()) {
            if !primary_button_down() || unsafe { GetCapture() } != hwnd(w) {
                drop(scrolling);
                self.stop_local_candidate_scroll(frame);
                cx.notify();
                return;
            }
            let (moved, next) = drag_tick(
                &mut panel.editor,
                drag,
                &paint.layout,
                paint.viewport,
                offset,
                max,
            );
            changed = moved;
            next
        } else {
            None
        };
        if let Some(y) = next {
            panel.scroll.set_offset(gpui::point(paint.offset.x, px(y)));
            scrolling.painted = None;
            scrolling.rehit = scrolling.drag.is_some();
            panel.visual.borrow_mut().invalidate();
        }
        if next.is_some() || changed {
            cx.notify();
        }
    }

    fn local_candidate_drag_move(
        &mut self,
        frame: &Arc<()>,
        position: gpui::Point<gpui::Pixels>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let owned = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .is_some_and(|p| Arc::ptr_eq(&p.identity, frame) && p.scrolling.borrow().pressed);
        if !owned {
            return false;
        }
        if !self.local_source_action_ready(w, cx)
            || !primary_button_down()
            || unsafe { GetCapture() } != hwnd(w)
            || !self.local_ocr_candidate_focused(w)
        {
            self.stop_local_candidate_scroll(frame);
            cx.notify();
            return true;
        }
        let paint = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .and_then(|p| Self::local_candidate_scroll_paint(p, w));
        if let Some(paint) = paint {
            self.local_candidate_selection(
                (frame, &paint.epoch),
                &paint.layout,
                position,
                (false, false),
                w,
                cx,
            );
        } else if let Some(panel) = self.local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            if let Some(drag) = panel.scrolling.borrow_mut().drag.as_mut() {
                drag.pointer = position;
            }
            panel.scrolling.borrow_mut().rehit = true;
            cx.notify();
        }
        true
    }

    fn local_candidate_release(
        &mut self,
        frame: &Arc<()>,
        position: gpui::Point<gpui::Pixels>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let owned = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .is_some_and(|p| Arc::ptr_eq(&p.identity, frame) && p.scrolling.borrow().pressed);
        if !owned {
            return false;
        }
        let paint = self
            .local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .and_then(|p| Self::local_candidate_scroll_paint(p, w));
        if let Some(paint) = paint {
            self.local_candidate_selection(
                (frame, &paint.epoch),
                &paint.layout,
                position,
                (false, true),
                w,
                cx,
            );
        }
        self.stop_local_candidate_scroll(frame);
        cx.notify();
        true
    }

    pub(crate) fn local_ocr_candidate_key(
        &mut self,
        e: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.local_ocr_candidate_focused(w) {
            return false;
        }
        cx.stop_propagation(); // 只读候选拥有焦点时，未处理键也不成为图片输出或绘图。
        let m = e.keystroke.modifiers;
        if !m.alt
            && !m.platform
            && !m.function
            && crate::capture_view::text_result::candidate_key_repeat_allowed(
                e.keystroke.key.as_str(),
                e.is_held,
            )
        {
            match e.keystroke.key.as_str() {
                "escape" if !m.control && !m.shift => {
                    self.cancel_local_ocr_intent("显式丢弃候选");
                    if w.is_window_active() {
                        w.focus(&self.focus);
                    }
                }
                "c" if m.control && !m.shift => self.copy_local_candidate(w, cx),
                key => {
                    let Some(frame) = self
                        .local_ocr
                        .as_ref()
                        .and_then(|s| s.panel.as_ref())
                        .map(|p| Arc::clone(&p.identity))
                    else {
                        return true;
                    };
                    if !self.candidate_frame_ready(&frame, w, cx)
                        || !unsafe { GetCapture() }.0.is_null()
                    {
                        return true;
                    }
                    let panel = self.local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
                    if panel.editor.composing() {
                        return true;
                    }
                    let reveal = match key {
                        "a" if m.control && !m.shift => {
                            panel.vertical = VerticalState::default();
                            panel.editor.select_all();
                            true
                        }
                        "left" | "right" | "home" | "end" => {
                            panel.vertical = VerticalState::default();
                            panel.editor.readonly_navigate(key, m.shift, m.control)
                        }
                        "up" | "down" if !m.control && !panel.editor.composing() => {
                            panel.move_vertical(key == "down", m.shift, w).is_some()
                        }
                        _ => false,
                    };
                    if reveal {
                        panel.editor.end_drag();
                        let mut scrolling = panel.scrolling.borrow_mut();
                        scrolling.drag = None;
                        scrolling.pressed = false;
                        scrolling.scheduled = None;
                        scrolling.command = None;
                        scrolling.reveal = true;
                        scrolling.rehit = false;
                    }
                }
            }
        }
        cx.notify();
        true
    }

    fn local_candidate_selection(
        &mut self,
        paint_token: (&Arc<()>, &Arc<()>),
        layout: &gpui::TextLayout,
        position: gpui::Point<gpui::Pixels>,
        phase: (bool, bool),
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (frame, epoch) = paint_token;
        let (begin, end) = phase;
        let ready = if begin {
            // Windows GPUI Down 先 SetCapture；只授权本次真实候选 Down 的旧有效绘制。
            self.local_source_action_ready(w, cx)
                && unsafe { GetCapture() } == hwnd(w)
                && primary_button_down()
                && self
                    .local_ocr
                    .as_ref()
                    .and_then(|s| s.panel.as_ref())
                    .is_some_and(|p| p.painted_frame(frame))
        } else {
            self.candidate_frame_ready(frame, w, cx)
        };
        if !ready {
            return;
        }
        let state = self.local_ocr.as_mut().expect("候选帧已核验");
        let panel = state.panel.as_mut().unwrap();
        if !panel.visual_current(epoch, w) {
            return;
        }
        let Some(paint) = Self::local_candidate_scroll_paint(panel, w) else {
            return;
        };
        let editor = &mut panel.editor;
        if !begin && !editor.dragging() {
            return;
        }
        panel.vertical = VerticalState::default();
        let index = layout
            .index_for_position(clamp_to_viewport(position, paint.viewport))
            .unwrap_or_else(|index| index)
            .min(editor.text.len());
        if begin {
            if !editor.readonly_begin_drag(index) {
                return;
            }
            w.focus(&state.focus);
            let blur_frame = Arc::clone(frame);
            panel._blur = Some(cx.on_blur(&state.focus, w, move |view, _, cx| {
                view.stop_local_candidate_scroll(&blur_frame);
                cx.notify();
            }));
        } else if editor.dragging() {
            editor.readonly_extend_drag(index);
        }
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.command = None;
        scrolling.reveal = false;
        scrolling.rehit = false;
        scrolling.pressed = !end;
        if !end {
            if let Some(drag) = scrolling.drag.as_mut() {
                drag.pointer = position;
            } else {
                scrolling.drag = Some(CandidateScrollDrag {
                    pointer: position,
                    last_tick: Instant::now(),
                });
            }
        } else {
            scrolling.drag = None;
            scrolling.scheduled = None;
        }
        if end {
            editor.end_drag();
        }
        cx.notify();
    }

    pub(super) fn render_local_ocr_candidate(
        &self,
        scale: f32,
        w: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.local_ocr.as_ref().filter(|s| s.visible)?;
        if !self.local_input_current() || self.result_editor.is_some() {
            return None;
        }
        let p = crate::theme::palette(cx);
        let mut root = div().id("ocr-local-candidate").absolute();
        // 结果面板存在时贴在面板底栏上方，向上生长；否则沿用显示器右下角。
        if let Some(layout) = self.ocr_panel_frame(w) {
            let sheet = layout.sheet();
            let viewport = f32::from(w.viewport_size().height);
            root = root
                .left(px(sheet.x))
                .bottom(px((viewport - layout.footer_top() + 6.).max(0.)))
                .w(px(sheet.width))
                .max_h(px(sheet.height))
                .shadow_lg();
        } else {
            let work = if self.image_canvas.is_some() {
                self.ocr_projection(w)?.client_work(92., 8.)
            } else {
                let monitor = self.ui_monitor(self.toolbar_monitor);
                PanelRect {
                    x: monitor.x as f32 / scale,
                    y: monitor.y as f32 / scale,
                    width: monitor.width as f32 / scale,
                    height: monitor.height as f32 / scale,
                }
            };
            let width = (work.width - 16.).clamp(1., 360.);
            let x = work.x + (work.width - width - 8.).max(0.);
            let y = work.y + (work.height - 230.).max(8.);
            root = root.left(px(x)).top(px(y)).w(px(width));
        }
        let mut root = root
            .p_2()
            .rounded_md()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(state.title.clone());
        if let Some(panel) = &state.panel {
            // Render 即撤掉上一帧的视觉行；仅本 epoch 的真实 paint 可以安装停点。
            let epoch = panel.visual.borrow_mut().begin_render();
            let projection_paint = self.ocr_overlay_projection_paint(w)?;
            panel.scrolling.borrow_mut().painted = None;
            let text = panel.styled_text();
            let layout = text.layout().clone();
            let paint = panel.paint_stamp(Arc::clone(&epoch), layout.clone());
            let frame = Arc::clone(&panel.identity);
            let painted = panel.painted.clone();
            let paint_frame = Arc::clone(&frame);
            let visual = panel.visual.clone();
            let scrolling = panel.scrolling.clone();
            let scroll = panel.scroll.clone();
            let owner = cx.entity().downgrade();
            let body = div()
                .id("ocr-local-candidate-text")
                .relative()
                .w_full()
                .text_left()
                .max_h(px(120.))
                .overflow_y_scroll()
                .track_scroll(&panel.scroll)
                .track_focus(&state.focus)
                .cursor(gpui::CursorStyle::IBeam)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, e: &MouseDownEvent, w, cx| {
                        v.local_candidate_selection(
                            (&frame, &epoch),
                            &layout,
                            e.position,
                            (true, false),
                            w,
                            cx,
                        );
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(cx.listener(|v, _, _, cx| {
                    if let Some(p) = v.local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
                        p.editor.end_drag();
                        p.vertical = VerticalState::default();
                        p.visual.borrow_mut().invalidate();
                        let pressed = p.scrolling.borrow().pressed;
                        *p.scrolling.borrow_mut() = CandidateScrolling {
                            pressed,
                            ..Default::default()
                        };
                    }
                    cx.notify();
                    cx.stop_propagation();
                }))
                .child(text)
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, w, cx| {
                            // 过期的 paint 不得清掉或覆盖后继 render 的布局或显示回执。
                            if !owner.upgrade().is_some_and(|owner| {
                                let view = owner.read(cx);
                                view.ocr_overlay_projection_paint_current(&projection_paint, w)
                                    && view
                                        .local_ocr
                                        .as_ref()
                                        .and_then(|state| state.panel.as_ref())
                                        .is_some_and(|panel| {
                                            Arc::ptr_eq(&panel.identity, &paint_frame)
                                                && Rc::ptr_eq(&panel.visual, &visual)
                                                && Rc::ptr_eq(&panel.painted, &painted)
                                                && Rc::ptr_eq(&panel.scrolling, &scrolling)
                                        })
                            }) {
                                return;
                            }
                            if !visual.borrow().epoch_is_current(&paint.epoch) {
                                return;
                            }
                            let current = owner.upgrade().is_some_and(|owner| {
                                let v = owner.read(cx);
                                v.local_action_ready(w, cx)
                                    && v.local_ocr
                                        .as_ref()
                                        .and_then(|s| s.panel.as_ref())
                                        .is_some_and(|p| {
                                            Arc::ptr_eq(&p.identity, &paint_frame)
                                                && paint.same_body(&p.editor)
                                        })
                            });
                            let Some(scroll_paint) =
                                paint.publish(&visual, current, &scroll, w, |_, _| ())
                            else {
                                return;
                            };
                            *painted.borrow_mut() = current.then(|| Arc::clone(&paint_frame));
                            let mut scrolling = scrolling.borrow_mut();
                            scrolling.painted = scroll_paint;
                            if !current {
                                let had_drag = scrolling.drag.take().is_some();
                                scrolling.command = None;
                                scrolling.scheduled = None;
                                scrolling.reveal = false;
                                scrolling.rehit = false;
                                if had_drag {
                                    let owner = owner.clone();
                                    let frame = Arc::clone(&paint_frame);
                                    cx.defer(move |cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.stop_local_candidate_scroll(&frame);
                                            cx.notify();
                                        });
                                    });
                                }
                            } else {
                                let can_scroll = scrolling.drag.as_ref().is_some_and(|drag| {
                                    candidate::edge_scroll(EdgeScroll {
                                        viewport_top: f32::from(scroll.bounds().top()),
                                        viewport_bottom: f32::from(scroll.bounds().bottom()),
                                        offset_y: f32::from(scroll.offset().y),
                                        max_scroll_y: f32::from(scroll.max_offset().height),
                                        pointer_y: f32::from(drag.pointer.y),
                                        elapsed_seconds: 1. / 60.,
                                    })
                                    .is_some()
                                });
                                if (can_scroll || scrolling.reveal || scrolling.rehit)
                                    && scrolling.scheduled.is_none()
                                {
                                    let token = Arc::new(());
                                    scrolling.scheduled = Some(Arc::clone(&token));
                                    let owner = owner.clone();
                                    let frame = Arc::clone(&paint_frame);
                                    // 只由真正 current paint 续约；on_next_frame 在下一次 draw 前执行。
                                    w.on_next_frame(move |w, cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.local_candidate_scroll_tick(&frame, &token, w, cx)
                                        });
                                    });
                                    w.request_animation_frame();
                                }
                                if let Some(command) =
                                    scrolling.command.as_mut().filter(|c| !c.scheduled)
                                {
                                    command.scheduled = true;
                                    let token = Arc::clone(&command.token);
                                    let frame = Arc::clone(&paint_frame);
                                    let owner = owner.clone();
                                    w.on_next_frame(move |w, cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.run_queued_local_candidate_command(
                                                &frame, &token, w, cx,
                                            )
                                        });
                                    });
                                    w.request_animation_frame();
                                }
                            }
                            drop(scrolling);
                            let move_owner = owner.clone();
                            let move_frame = Arc::clone(&paint_frame);
                            w.on_mouse_event(move |e: &MouseMoveEvent, phase, w, cx| {
                                if phase.bubble() {
                                    let _ = move_owner.update(cx, |view, cx| {
                                        if view.local_candidate_drag_move(
                                            &move_frame,
                                            e.position,
                                            w,
                                            cx,
                                        ) {
                                            cx.stop_propagation();
                                        }
                                    });
                                }
                            });
                            let up_owner = owner.clone();
                            let up_frame = Arc::clone(&paint_frame);
                            w.on_mouse_event(move |e: &MouseUpEvent, phase, w, cx| {
                                if phase.capture() && e.button == MouseButton::Left {
                                    let _ = up_owner.update(cx, |view, cx| {
                                        if view
                                            .local_candidate_release(&up_frame, e.position, w, cx)
                                        {
                                            cx.stop_propagation();
                                        }
                                    });
                                }
                            });
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
            root = root.child(body);
        }
        let can_copy = state.panel.is_some();
        let retry_click_stamp = self.retry_click_stamp(RetryClickKind::WithoutPanel);
        let button_stamp = state.panel.as_ref().map(|panel| {
            panel.button_stamp(
                &state.fixed.identity,
                state.binding.source.clone(),
                state.binding.result_generation,
            )
        });
        let mut buttons = div().flex().flex_wrap().gap_1();
        for (id, label, command, enabled) in [
            (
                "ocr-local-copy",
                tr!("复制候选", "Copy candidate"),
                0,
                can_copy,
            ),
            ("ocr-local-retry", tr!("重试此行", "Retry line"), 1, true),
            ("ocr-local-discard", tr!("丢弃候选", "Discard"), 2, true),
        ] {
            let mut button = div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_sm()
                .text_color(rgb(if enabled { p.foreground } else { p.muted }))
                .child(label);
            if enabled {
                let button_stamp = button_stamp.clone();
                let click_stamp = retry_click_stamp.clone();
                if command == 1 && button_stamp.is_none() {
                    let down_stamp = click_stamp.clone();
                    button = button.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, event, w, cx| {
                            if let Some(stamp) = &down_stamp {
                                v.arm_local_retry_click(stamp, event, w, cx);
                            }
                            cx.stop_propagation();
                        }),
                    );
                } else if let Some(stamp) = button_stamp.clone() {
                    let kind = if command == 0 {
                        CandidateCommandKind::Copy
                    } else {
                        CandidateCommandKind::Retry
                    };
                    if command <= 1 {
                        button = button.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |v, event, w, cx| {
                                v.arm_local_candidate_pointer(kind, &stamp, event, w, cx);
                                cx.stop_propagation();
                            }),
                        );
                    }
                }
                button = button
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(p.hover)))
                    .on_click(cx.listener(move |v, event, w, cx| match command {
                        0 => {
                            if let Some(stamp) = &button_stamp {
                                v.queue_local_candidate_command(
                                    CandidateCommandKind::Copy,
                                    stamp,
                                    event,
                                    w,
                                    cx,
                                );
                            }
                        }
                        1 => {
                            if let Some(stamp) = &button_stamp {
                                v.queue_local_candidate_command(
                                    CandidateCommandKind::Retry,
                                    stamp,
                                    event,
                                    w,
                                    cx,
                                );
                            } else if let Some(stamp) = &click_stamp {
                                v.queue_local_retry_click(stamp, event, w, cx);
                            }
                        }
                        _ => {
                            let focused = v.local_ocr_candidate_focused(w);
                            v.cancel_local_ocr_intent("显式丢弃候选");
                            if focused && w.is_window_active() {
                                w.focus(&v.focus);
                            }
                            cx.notify();
                        }
                    }));
            }
            buttons = buttons.child(button);
        }
        Some(
            root.child(div().text_size(px(11.)).child(state.notice.clone()))
                .child(buttons)
                .into_any_element(),
        )
    }
}

fn target_error(error: TargetError) -> &'static str {
    match error {
        TargetError::Inactive => tr!(
            "请结束当前操作后重试",
            "Finish the current action, then retry"
        ),
        TargetError::EditedBody => tr!(
            "正文已编辑，请重新识别全文",
            "Text edited; recognize the full result again"
        ),
        TargetError::Budget => tr!(
            "此行超出局部重识别上限",
            "Line exceeds the local retry limit"
        ),
        _ => tr!(
            "无法单独定位这一行，请只选同一行的文字",
            "Can't locate this line on its own; select text within a single line"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_view::text_result::ResultEditor;

    /// 真实组件组合，不启动OCR提供者；识别DTO固定为本次输入内的两行完整几何。
    #[test]
    fn projected_raw_alpha_nonzero_lasso_fixed_input_marks_share_output_and_one_history_step_per_kind(
    ) {
        use crate::{
            annotation::{Annotation, Style, Tool},
            canvas_source::CanvasSource,
            capture_pin_output::{content_image, output_rect},
            crop_mask::Polygon,
            document::Document,
            ocr_markup::TextMark,
            recognition::{RecognizedLine, RecognizedWord},
            scene::{PixelPlane, Scene, SourceIdentity, SourcePixels},
        };

        // 仅把真实CanvasSource原图平面接入Scene；不复制合成/遮罩/采样算法。
        fn pixels<'a>(
            source: &'a CanvasSource,
            previews: &'a [Arc<gpui::RenderImage>],
        ) -> SourcePixels<'a> {
            SourcePixels {
                canvas: source.full(),
                fill: source.solid_fill(),
                overlay: None,
                planes: previews
                    .iter()
                    .enumerate()
                    .map(|(i, preview)| PixelPlane {
                        rect: source.original_rect(i),
                        bgra: source.original(preview).as_bytes(0).unwrap(),
                    })
                    .collect(),
            }
        }
        fn annotations(document: &Document) -> Vec<Annotation> {
            document
                .items()
                .iter()
                .map(|object| object.annotation.clone())
                .collect()
        }
        fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
            let start = ((y * image.width + x) * 4) as usize;
            image.bgra[start..start + 4].try_into().unwrap()
        }
        fn assert_png_roundtrip(image: &Image) {
            let mut encoded = Vec::new();
            crate::export::write_png_pixels(
                image.width,
                image.height,
                &image.bgra,
                &mut encoded,
                png::Compression::Fast,
            )
            .unwrap();
            let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)
                .unwrap()
                .into_rgba8();
            assert_eq!(decoded.dimensions(), (image.width, image.height));
            for (rgba, bgra) in decoded
                .as_raw()
                .chunks_exact(4)
                .zip(image.bgra.chunks_exact(4))
            {
                assert_eq!(rgba, [bgra[2], bgra[1], bgra[0], bgra[3]]);
            }
        }

        let width = 17_000;
        let height = 64;
        let raw: Vec<u8> = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    let alpha = [0, 64, 128, 255][((x + 3 * y) % 4) as usize];
                    [
                        (x * 17 + y * 3) as u8,
                        (x / 251 + y * 11) as u8,
                        (x * 7 + y * 19) as u8,
                        alpha,
                    ]
                })
            })
            .collect();
        let (mut source, mut previews) = CanvasSource::editor_image(Image {
            width,
            height,
            bgra: raw.clone(),
            readback_ms: 0.,
        })
        .unwrap();
        assert!(source.is_projected_image());
        assert!(previews[0]
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 255));
        let host = crate::geometry::Rect {
            x: 16_375,
            y: 4,
            width: 50,
            height: 48,
        };
        let selected = crate::geometry::Rect {
            x: 16_380,
            y: 8,
            width: 40,
            height: 40,
        };
        let mask = Polygon::new(&[
            Point {
                x: 16_380.25,
                y: 8.25,
            },
            Point {
                x: 16_419.75,
                y: 8.25,
            },
            Point {
                x: 16_380.25,
                y: 47.75,
            },
        ])
        .unwrap();
        assert_eq!(
            output_rect(source.full(), host, Some(&mask)).unwrap(),
            selected
        );
        let coverage = mask.coverage(selected);
        assert!(coverage.contains(&0) && coverage.contains(&255));
        assert!(coverage
            .iter()
            .any(|&coverage| coverage > 0 && coverage < 255));
        let raw_crop = source.read_region(selected).unwrap();
        let raw_start = ((33 * width + 16_382) * 4) as usize;
        assert_eq!(pixel(&raw_crop, 2, 25), raw[raw_start..raw_start + 4]);
        assert_eq!(pixel(&raw_crop, 2, 25)[3], 64);

        let value = RecognizedText {
            text: "alpha\nbeta".into(),
            regions: Vec::new(),
            words: [("alpha", 4), ("beta", 14)]
                .into_iter()
                .enumerate()
                .map(|(line, (text, y))| RecognizedWord {
                    text: text.into(),
                    rect: crate::geometry::Rect {
                        x: 4,
                        y,
                        width: 12,
                        height: 6,
                    },
                    line,
                    horizontal_direction: Default::default(),
                    geometry: None,
                })
                .collect(),
            lines: ["alpha", "beta"]
                .into_iter()
                .map(|text| RecognizedLine {
                    graphemes: None,
                    text: text.into(),
                    rect: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                })
                .collect(),
        };
        let full_range = 0..value.text.len();
        let expected_quads = [
            [
                Point { x: 16_384., y: 12. },
                Point { x: 16_396., y: 12. },
                Point { x: 16_396., y: 18. },
                Point { x: 16_384., y: 18. },
            ],
            [
                Point { x: 16_384., y: 22. },
                Point { x: 16_396., y: 22. },
                Point { x: 16_396., y: 28. },
                Point { x: 16_384., y: 28. },
            ],
        ];

        let mut retained_inputs = Vec::new();
        for mark in [
            TextMark::Highlight,
            TextMark::Underline,
            TextMark::Wave,
            TextMark::Strike,
        ] {
            let mut document = Document::default();
            let mut lower = Annotation::new(
                Tool::Rectangle,
                Point { x: 16_382., y: 10. },
                Style {
                    filled: true,
                    color: 0x2c76e3,
                    width: 1.,
                    ..Style::default()
                },
            );
            lower.points.push(Point { x: 16_400., y: 20. });
            document.insert(lower);
            document.set_crop_mask(Some(mask.clone()));
            let source_pixels = pixels(&source, &previews);
            let identity = SourceIdentity::of(&source_pixels);
            let underlay = Scene::prepare(&annotations(&document), &source_pixels);
            assert!(underlay.errors.is_empty(), "{:?}", underlay.errors);
            let mut input = content_image(
                &source,
                &underlay,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert_eq!(
                (input.width, input.height),
                (selected.width, selected.height)
            );
            assert_ne!(
                input.bgra, raw_crop.bgra,
                "输入含真实下层标注和套索，不只是裁剪原图"
            );
            assert_eq!(pixel(&input, 6, 6), [0xe3, 0x76, 0x2c, 255]);
            assert_eq!(pixel(&input, 2, 25), pixel(&raw_crop, 2, 25));
            assert_eq!(pixel(&input, 39, 39), [0; 4]);
            let target = OcrTarget {
                canvas: source.bounds,
                selected,
                revisions: (document.revision(), document.edit_revision()),
                stamp: (document.revision(), 0),
                identity: identity.clone(),
            };
            let fixed = FixedInput::copy_of_profile(
                &input,
                target.clone(),
                OcrProvider::Ppocr,
                crate::ppocr::OcrProfile::Small,
            )
            .unwrap();
            let before_pixels = input.bgra.clone();
            assert_eq!(fixed.image.bgra, before_pixels);
            assert_ne!(fixed.image.bgra.as_ptr(), input.bgra.as_ptr());
            assert_eq!(
                (fixed.frame.width, fixed.frame.height, fixed.frame.origin),
                (40, 40, (16_380, 8))
            );
            assert!(fixed.source == target);
            assert_eq!(fixed.provider, OcrProvider::Ppocr);
            assert_eq!(fixed.profile, crate::ppocr::OcrProfile::Small);
            input.bgra.fill(0xa5);
            assert_eq!(fixed.image.bgra, before_pixels, "固定输入独占请求时像素");

            let mut overlay = OcrOverlay::new(&value, target).unwrap();
            overlay.select_range(full_range.clone());
            let quads = overlay.mark_selection_quads().unwrap();
            assert_eq!(quads, expected_quads);
            assert_eq!(
                overlay.selected_text().as_deref(),
                Some(value.text.as_str())
            );
            let before_items = document.items().to_vec();
            let before_history = document.bounded_history_snapshot().unwrap();
            let before_edit_revision = document.edit_revision();
            let edit = overlay
                .ledger
                .prepare(
                    &document,
                    mark,
                    &quads,
                    Style {
                        color: 0xff8b20,
                        width: 3.,
                        opacity: 0.65,
                        ..Style::default()
                    },
                )
                .unwrap();
            assert!(!edit.is_empty());
            let planned_scene = underlay.updated(&edit.annotations(&document), &source_pixels);
            assert!(
                planned_scene.errors.is_empty(),
                "{:?}",
                planned_scene.errors
            );
            let planned = content_image(
                &source,
                &planned_scene,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert!(overlay.ledger.commit(&mut document, edit));
            assert_eq!(document.edit_revision(), before_edit_revision + 1);
            let history = document.bounded_history_snapshot().unwrap();
            assert_eq!(history.undo.len(), before_history.undo.len() + 1);
            assert!(history.redo.is_empty());
            assert_eq!(
                document.items().len(),
                before_items.len() + expected_quads.len()
            );
            let committed_items = document.items().to_vec();
            let committed_scene = underlay.updated(&annotations(&document), &source_pixels);
            let committed = content_image(
                &source,
                &committed_scene,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert_eq!(
                committed.bgra, planned.bgra,
                "计划Scene和真实提交文档输出同一组标记"
            );
            assert_ne!(
                committed.bgra, before_pixels,
                "每种持久标记在真实输出中可见"
            );
            assert_eq!(pixel(&committed, 2, 25)[3], 64);
            assert_eq!(pixel(&committed, 39, 39), [0; 4]);
            assert_png_roundtrip(&committed);

            // 这些只是OCR辅助状态；共用输出函数仍仅合成源、Scene和文档套索。
            overlay.show_selection = false;
            overlay.panel.set_layer(false);
            overlay.panel.set_menu(true);
            overlay.color = Some(0x123456);
            overlay.select_range(0..1);
            let ui_changed = content_image(
                &source,
                &committed_scene,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert_eq!(ui_changed.bgra, committed.bgra);
            assert_eq!(document.items(), committed_items);
            document.undo();
            assert_eq!(document.items(), before_items);
            assert_eq!(document.crop_mask.as_ref(), Some(&mask));
            assert_eq!(document.edit_revision(), before_edit_revision + 1);
            let undone_scene = committed_scene.updated(&annotations(&document), &source_pixels);
            let undone = content_image(
                &source,
                &undone_scene,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert_eq!(undone.bgra, before_pixels);
            let undone_history = document.bounded_history_snapshot().unwrap();
            assert_eq!(undone_history.undo.len(), before_history.undo.len());
            assert_eq!(undone_history.redo.len(), 1);
            document.redo();
            assert_eq!(document.items(), committed_items);
            assert_eq!(document.edit_revision(), before_edit_revision + 1);
            let redone_scene = undone_scene.updated(&annotations(&document), &source_pixels);
            let redone = content_image(
                &source,
                &redone_scene,
                selected,
                None,
                document.crop_mask.as_ref(),
            )
            .unwrap();
            assert_eq!(redone.bgra, committed.bgra);
            assert_png_roundtrip(&redone);
            assert!(overlay.ledger.matches_underlay(&document));
            assert_eq!(fixed.image.bgra, before_pixels);
            assert!(fixed.source.identity == identity);
            retained_inputs.push((fixed, before_pixels, identity));
        }

        previews[0] = source
            .refresh_shared_preview_at(selected, 2.)
            .unwrap()
            .unwrap();
        assert_eq!(source.read_region(selected).unwrap().bgra, raw_crop.bgra);
        assert_eq!(source.read_region(source.full()).unwrap().bgra, raw);
        let current_identity = SourceIdentity::of(&pixels(&source, &previews));
        for (fixed, original_input, identity) in retained_inputs {
            assert_eq!(fixed.image.bgra, original_input);
            assert_eq!(fixed.frame.origin, (16_380, 8));
            assert!(fixed.source.identity == identity);
            assert!(identity == current_identity);
        }
    }

    #[test]
    fn retry_click_stamp_rejects_cancelled_selection_round_trip_and_source_replacement() {
        let mut clicks = RetryClicks::default();
        let stamp = RetryClickStamp {
            kind: RetryClickKind::Selection,
            session: Arc::clone(&clicks.session),
            source: source_target(),
            source_identity: Arc::new(()),
            body_identity: Arc::new(()),
            revision: 0,
            generation: 7,
            ui_generation: 9,
            overlay: Rc::new(RefCell::new(None)),
            selection: Some(0..3),
            keyboard_selection: None,
            selected: 0..3,
            retry_line: None,
        };
        let mut current = stamp.clone();
        assert!(retry_click_stamp_matches(&stamp, &current));
        current.selection = Some(3..6);
        assert!(!retry_click_stamp_matches(&stamp, &current));
        // 实际换选取消 API 换代，范围恢复 A 也仍拒绝旧按钮 Down。
        clicks.cancel();
        current.selection = stamp.selection.clone();
        current.session = Arc::clone(&clicks.session);
        assert!(!retry_click_stamp_matches(&stamp, &current));
        current = stamp.clone();
        current.ui_generation += 1;
        assert!(!retry_click_stamp_matches(&stamp, &current));
        current = stamp.clone();
        current.body_identity = Arc::new(());
        assert!(!retry_click_stamp_matches(&stamp, &current));
        current = stamp.clone();
        current.source_identity = Arc::new(());
        assert!(!retry_click_stamp_matches(&stamp, &current));
        current = stamp.clone();
        current.overlay = Rc::new(RefCell::new(None));
        assert!(!retry_click_stamp_matches(&stamp, &current));
        current = stamp.clone();
        current.source.canvas.left -= 1;
        assert!(!retry_click_stamp_matches(&stamp, &current));
        let mut retry = stamp.clone();
        retry.kind = RetryClickKind::WithoutPanel;
        retry.retry_line = Some((0..3, 1, crate::geometry::Rect::full(2, 1)));
        assert!(!retry_click_stamp_matches(&stamp, &retry));
        let mut other_line = retry.clone();
        other_line.retry_line.as_mut().unwrap().1 += 1;
        assert!(!retry_click_stamp_matches(&retry, &other_line));
        assert!(retry_click_stamp_matches(&retry, &retry.clone()));
    }

    #[test]
    fn retry_click_paint_requires_current_epoch_viewport_scale_and_canvas_offset() {
        let mut clicks = RetryClicks::default();
        let epoch = clicks.begin_render();
        let paint = RetryClickPaint {
            epoch: Arc::clone(&epoch),
            size: [1920., 1080.],
            scale: 1.25,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport {
                    zoom: 1.,
                    offset: Point { x: -10., y: 2.5 },
                },
                1.25,
                [1920., 1080.],
            ),
            offset: [-8., 2.],
        };
        assert!(paint.current(
            &epoch,
            paint.size,
            paint.scale,
            paint.projection,
            paint.offset
        ));
        assert!(!paint.current(
            &epoch,
            [1921., 1080.],
            paint.scale,
            paint.projection,
            paint.offset
        ));
        assert!(!paint.current(&epoch, paint.size, 1.5, paint.projection, paint.offset));
        assert!(!paint.current(&epoch, paint.size, paint.scale, paint.projection, [0., 2.]));
        assert!(!paint.current(
            &epoch,
            [f32::NAN, 1080.],
            paint.scale,
            paint.projection,
            paint.offset
        ));
        let next = clicks.begin_render();
        assert!(!paint.current(
            &next,
            paint.size,
            paint.scale,
            paint.projection,
            paint.offset
        ));
        clicks.painted = Some(paint);
        clicks.cancel();
        assert!(clicks.painted.is_none());
    }

    #[test]
    fn retry_cancel_revokes_old_render_epoch_before_any_successor_render() {
        let mut clicks = RetryClicks::default();
        let old_epoch = clicks.begin_render();
        let old = RetryClickPaint {
            epoch: old_epoch.clone(),
            size: [800., 600.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [800., 600.],
            ),
            offset: [0., 0.],
        };
        clicks.painted = Some(old.clone());
        clicks.cancel();
        assert!(!Arc::ptr_eq(&old_epoch, &clicks.epoch));
        assert!(!old.current(
            &clicks.epoch,
            old.size,
            old.scale,
            old.projection,
            old.offset
        ));
        assert!(clicks.painted.is_none());
        let fresh_epoch = clicks.begin_render();
        let fresh = RetryClickPaint {
            epoch: fresh_epoch.clone(),
            ..old.clone()
        };
        clicks.painted = Some(fresh);
        assert!(!old.current(
            &fresh_epoch,
            old.size,
            old.scale,
            old.projection,
            old.offset
        ));
        assert!(clicks.painted.as_ref().unwrap().current(
            &fresh_epoch,
            old.size,
            old.scale,
            old.projection,
            old.offset
        ));
    }
    #[test]
    fn retry_click_paint_rejects_image_zoom_even_when_dpi_offset_and_client_are_unchanged() {
        let epoch = Arc::new(());
        let projection = |zoom| {
            crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport {
                    zoom,
                    offset: Point { x: 0., y: 0. },
                },
                1.5,
                [900., 600.],
            )
        };
        let paint = RetryClickPaint {
            epoch: Arc::clone(&epoch),
            size: [900., 600.],
            scale: 1.5,
            projection: projection(0.5),
            offset: [0., 0.],
        };
        assert!(paint.current(
            &epoch,
            paint.size,
            paint.scale,
            projection(0.5),
            paint.offset
        ));
        assert!(!paint.current(
            &epoch,
            paint.size,
            paint.scale,
            projection(1.),
            paint.offset
        ));
        for zoom in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(!paint.current(
                &epoch,
                paint.size,
                paint.scale,
                projection(zoom),
                paint.offset
            ));
        }
        assert!(!paint.current(&epoch, paint.size, paint.scale, None, paint.offset));
        let unknown = RetryClickPaint {
            projection: None,
            ..paint.clone()
        };
        assert!(!unknown.current(&epoch, paint.size, paint.scale, None, paint.offset));
    }

    #[test]
    fn candidate_explicit_button_stamp_rejects_foreign_source_body_or_selection() {
        let source = source_target();
        let source_identity = Arc::new(());
        let mut panel = CandidatePanel {
            identity: Arc::new(()),
            editor: ResultEditor::new("e\u{301}\r\n 候选".into(), 0..0),
            painted: Default::default(),
            visual: Default::default(),
            vertical: Default::default(),
            scroll: Default::default(),
            scrolling: Default::default(),
            _blur: None,
        };
        let stamp = CandidateButtonStamp {
            frame: Arc::clone(&panel.identity),
            body_identity: Arc::clone(&panel.editor.body_identity),
            revision: panel.editor.revision,
            selection: panel.editor.input.selection.clone(),
            reversed: panel.editor.input.reversed,
            source_identity: Arc::clone(&source_identity),
            source: source.clone(),
            generation: 7,
        };
        assert!(stamp.matches(&panel, &source_identity, &source, 7));
        assert!(!stamp.matches(&panel, &Arc::new(()), &source, 7));
        assert!(!stamp.matches(&panel, &source_identity, &source, 8));
        let mut foreign_source = source.clone();
        foreign_source.selected.x += 1;
        assert!(!stamp.matches(&panel, &source_identity, &foreign_source, 7));
        panel.editor.readonly_begin_drag(0);
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.readonly_extend_drag(3);
        panel.editor.end_drag();
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.readonly_set_selection(0, 0);
        assert!(stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.input.reversed = true;
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.input.reversed = false;
        panel.editor.revision += 1;
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.revision -= 1;
        panel.editor.input.marked = Some(0..3);
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.input.marked = None;
        panel.editor.body_identity = Arc::new(());
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        panel.editor.body_identity = Arc::clone(&stamp.body_identity);
        panel.identity = Arc::new(());
        assert!(!stamp.matches(&panel, &source_identity, &source, 7));
        assert_eq!(panel.editor.text.as_bytes(), "e\u{301}\r\n 候选".as_bytes());
        assert_eq!(panel.editor.revision, 0);
    }

    fn source_target() -> OcrTarget {
        let pixels = crate::scene::SourcePixels {
            canvas: crate::geometry::Rect::full(10, 10),
            fill: Some([0, 0, 0, 255]),
            planes: Vec::new(),
            overlay: None,
        };
        OcrTarget {
            canvas: crate::geometry::ScreenRect {
                left: -100,
                top: -80,
                width: 10,
                height: 10,
            },
            selected: crate::geometry::Rect {
                x: 3,
                y: 4,
                width: 2,
                height: 1,
            },
            revisions: (0, 0),
            stamp: (0, 0),
            identity: crate::scene::SourceIdentity::of(&pixels),
        }
    }

    #[test]
    fn retained_input_is_an_owned_byte_exact_snapshot_of_actual_request() {
        let mut input = Image {
            width: 2,
            height: 1,
            bgra: vec![0, 1, 2, 3, 252, 253, 254, 255],
            readback_ms: 1.25,
        };
        let retained = retained_image(&input, (2, 1)).unwrap();
        assert_eq!(retained.bgra, input.bgra);
        assert_ne!(retained.bgra.as_ptr(), input.bgra.as_ptr());
        input.bgra.fill(99);
        assert_eq!(retained.bgra, [0, 1, 2, 3, 252, 253, 254, 255]);
        assert_eq!(retained.readback_ms, 1.25);
        let fixed = FixedInput::copy_of(&input, source_target(), OcrProvider::Ppocr).unwrap();
        let next = FixedInput::copy_of(&input, source_target(), OcrProvider::Ppocr).unwrap();
        assert_eq!(fixed.frame.origin, (-97, -76));
        assert_eq!((fixed.frame.width, fixed.frame.height), (2, 1));
        assert!(fixed.source == next.source);
        assert!(!Arc::ptr_eq(&fixed.identity, &next.identity));
    }

    #[test]
    fn retained_input_rejects_mismatch_and_budget_before_copy() {
        let input = Image {
            width: 2,
            height: 1,
            bgra: vec![0; 8],
            readback_ms: 0.,
        };
        assert!(retained_image(&input, (1, 2)).is_none());
        let malformed = Image {
            width: 2,
            height: 1,
            bgra: vec![0; 7],
            readback_ms: 0.,
        };
        assert!(retained_image(&malformed, (2, 1)).is_none());
        let oversized = Image {
            width: 65_535,
            height: 65_535,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        assert!(retained_image(&oversized, (65_535, 65_535)).is_none());
        let mut source = source_target();
        source.selected.x = 10;
        assert!(FixedInput::copy_of(&input, source, OcrProvider::Ppocr).is_none());
    }

    #[test]
    fn fixed_image_editor_crop_uses_raw_alpha_pixels_and_survives_projection_preview_change() {
        let width = 17_000;
        let height = 4;
        let bytes: Vec<u8> = (0..width * height)
            .flat_map(|i| [i as u8, (i / width) as u8, (i / 251) as u8, (i % 255) as u8])
            .collect();
        let (mut source, mut previews) = crate::canvas_source::CanvasSource::editor_image(Image {
            width,
            height,
            bgra: bytes.clone(),
            readback_ms: 0.,
        })
        .unwrap();
        assert!(source.is_projected_image());
        let selected = crate::geometry::Rect {
            x: 16_380,
            y: 1,
            width: 9,
            height: 2,
        };
        let identity = |source: &crate::canvas_source::CanvasSource,
                        preview: &Arc<gpui::RenderImage>| {
            crate::scene::SourceIdentity::of(&crate::scene::SourcePixels {
                canvas: source.full(),
                fill: None,
                planes: vec![crate::scene::PixelPlane {
                    rect: source.original_rect(0),
                    bgra: source.original(preview).as_bytes(0).unwrap(),
                }],
                overlay: None,
            })
        };
        let target = OcrTarget {
            canvas: source.bounds,
            selected,
            revisions: (0, 0),
            stamp: (0, 0),
            identity: identity(&source, &previews[0]),
        };
        let input = source.read_region(selected).unwrap();
        let fixed = FixedInput::copy_of_profile(
            &input,
            target.clone(),
            OcrProvider::Ppocr,
            crate::ppocr::OcrProfile::Small,
        )
        .unwrap();
        let expected: Vec<u8> = (selected.y..selected.y + selected.height)
            .flat_map(|y| {
                let start = ((y * width + selected.x) * 4) as usize;
                bytes[start..start + selected.width as usize * 4]
                    .iter()
                    .copied()
            })
            .collect();
        assert_eq!(fixed.image.bgra, expected);
        assert_eq!(fixed.frame.origin, (16_380, 1));
        assert_eq!((fixed.frame.width, fixed.frame.height), (9, 2));
        assert_eq!(fixed.profile, crate::ppocr::OcrProfile::Small);
        assert_ne!(fixed.image.bgra.as_ptr(), input.bgra.as_ptr());
        previews[0] = source
            .refresh_shared_preview_at(selected, 2.)
            .unwrap()
            .unwrap();
        assert!(identity(&source, &previews[0]) == target.identity);
        assert_eq!(source.read_region(selected).unwrap().bgra, expected);
        assert_eq!(fixed.image.bgra, expected);
        assert_eq!(source.read_region(source.full()).unwrap().bgra, bytes);
    }

    #[test]
    fn candidate_real_read_only_editor_selects_verbatim_without_body_write() {
        let text = " 候选 e\u{301}\r\n  ";
        let mut editor = ResultEditor::new(text.into(), 0..0);
        let identity = Arc::clone(&editor.body_identity);
        editor.readonly_begin_drag(text.len());
        editor.readonly_extend_drag(0);
        assert!(editor.input.reversed);
        editor.end_drag();
        assert_eq!(editor.selected_or_full().as_bytes(), text.as_bytes());
        editor.readonly_navigate("right", false, false);
        assert_eq!(editor.text.as_bytes(), text.as_bytes());
        assert!(Arc::ptr_eq(&identity, &editor.body_identity));
        assert_eq!(editor.revision, 0);
    }

    #[test]
    fn capture_readonly_candidate_graphemes_words_keep_binding_and_empty_undo() {
        let original = Arc::new(RecognizedText {
            text: " 原 OCR 正文\r\n ".into(),
            words: Vec::new(),
            lines: Vec::new(),
            regions: Vec::new(),
        });
        let source_identity = Arc::new(());
        let installed_body_identity = Arc::new(());
        let installed = Binding {
            source: source_target(),
            source_identity: Arc::clone(&source_identity),
            result: Arc::clone(&original),
            result_generation: 7,
            installed_body_identity: Arc::clone(&installed_body_identity),
            body_identity: Arc::clone(&installed_body_identity),
            installed_body_revision: 0,
            body_revision: 0,
            frame: SourceFrame {
                width: 2,
                height: 1,
                origin: (-97, -76),
            },
            provider: OcrProvider::Ppocr,
        };
        let retained = installed.clone();
        let clusters = ["中", "e\u{301}", "👩‍💻", "🇨🇳", "👍🏽", "\r\n"];
        let line_start = clusters.concat().len();
        let candidate = format!("{}alpha, beta!", clusters.concat());
        let mut panel = CandidatePanel {
            identity: Arc::new(()),
            editor: ResultEditor::new(candidate.clone(), 0..0),
            painted: Rc::new(RefCell::new(None)),
            visual: Default::default(),
            vertical: Default::default(),
            scroll: Default::default(),
            scrolling: Default::default(),
            _blur: None,
        };
        let frame_identity = Arc::clone(&panel.identity);
        let candidate_body_identity = Arc::clone(&panel.editor.body_identity);
        let editor = &mut panel.editor;
        let mut start = 0;
        for cluster in clusters {
            let end = start + cluster.len();
            // 实际 TextLayout 返回的是字节 index；内部 UTF-8 或 CRLF 命中也夹到完整字素首。
            assert!(editor.readonly_begin_drag(end - 1));
            assert_eq!(editor.caret(), start);
            assert!(editor.readonly_extend_drag(end));
            assert_eq!(editor.selected_or_full(), cluster);
            assert!(editor.end_drag());
            start = end;
        }
        editor.readonly_begin_drag(line_start);
        editor.end_drag();
        for stop in [
            line_start + 5,
            line_start + 6,
            line_start + 11,
            candidate.len(),
        ] {
            assert!(editor.readonly_navigate("right", false, true));
            assert_eq!(editor.caret(), stop);
        }
        editor.readonly_begin_drag(candidate.len());
        editor.readonly_extend_drag(line_start);
        editor.end_drag();
        assert!(editor.input.reversed);
        assert!(editor.readonly_navigate("right", true, true));
        assert_eq!(editor.input.selection, line_start + 5..candidate.len());
        assert!(editor.input.reversed);
        assert!(editor.readonly_navigate("right", false, true));
        assert_eq!(editor.input.selection, candidate.len()..candidate.len());
        assert!(editor.readonly_navigate("home", false, false));
        assert_eq!(editor.caret(), line_start);
        assert!(editor.readonly_navigate("home", false, true));
        assert_eq!(editor.caret(), 0);
        assert!(editor.readonly_navigate("end", false, false));
        assert_eq!(editor.caret(), line_start - 2);
        assert!(editor.readonly_navigate("end", false, true));
        assert_eq!(editor.caret(), candidate.len());
        let selection = editor.input.selection.clone();
        editor.undo();
        editor.redo();
        assert_eq!(editor.input.selection, selection);
        assert_eq!(editor.text.as_bytes(), candidate.as_bytes());
        assert_eq!(editor.revision, 0);
        assert!(!editor.composing());
        assert!(Arc::ptr_eq(&candidate_body_identity, &editor.body_identity));
        assert!(Arc::ptr_eq(&frame_identity, &panel.identity));
        assert!(panel.painted.borrow().is_none());
        assert!(installed.source == retained.source);
        assert_eq!(installed.frame, retained.frame);
        assert_eq!(installed.provider, retained.provider);
        assert_eq!(installed.result_generation, retained.result_generation);
        assert_eq!(installed.body_revision, retained.body_revision);
        assert_eq!(
            installed.installed_body_revision,
            retained.installed_body_revision
        );
        assert_eq!(
            installed.result.text.as_bytes(),
            " 原 OCR 正文\r\n ".as_bytes()
        );
        assert!(Arc::ptr_eq(&installed.result, &original));
        assert!(Arc::ptr_eq(&installed.source_identity, &source_identity));
        assert!(Arc::ptr_eq(
            &installed.body_identity,
            &installed_body_identity
        ));
        assert!(Arc::ptr_eq(
            &installed.installed_body_identity,
            &installed_body_identity
        ));
    }

    #[test]
    fn fixed_small_request_keeps_profile_when_settings_or_original_pixels_change() {
        let mut preferences = crate::preferences::Preferences {
            ocr_profile: crate::ppocr::OcrProfile::Small,
            ..Default::default()
        };
        let mut input = Image {
            width: 2,
            height: 1,
            bgra: vec![0, 1, 2, 3, 252, 253, 254, 255],
            readback_ms: 0.,
        };
        let fixed = FixedInput::copy_of_profile(
            &input,
            source_target(),
            OcrProvider::Ppocr,
            preferences.ocr_profile,
        )
        .unwrap();
        preferences.ocr_profile = crate::ppocr::OcrProfile::Tiny;
        input.bgra.fill(99);
        assert_eq!(fixed.profile, crate::ppocr::OcrProfile::Small);
        assert_ne!(fixed.profile, preferences.ocr_profile);
        assert_eq!(fixed.image.bgra, [0, 1, 2, 3, 252, 253, 254, 255]);
        assert_eq!(fixed.provider, OcrProvider::Ppocr);
        assert_eq!(fixed.frame.origin, (-97, -76));
    }

    #[test]
    fn tiny_test_compatibility_and_small_request_have_distinct_fixed_identity() {
        let input = Image {
            width: 2,
            height: 1,
            bgra: vec![0; 8],
            readback_ms: 0.,
        };
        let tiny = FixedInput::copy_of(&input, source_target(), OcrProvider::Ppocr).unwrap();
        let small = FixedInput::copy_of_profile(
            &input,
            source_target(),
            OcrProvider::Ppocr,
            crate::ppocr::OcrProfile::Small,
        )
        .unwrap();
        assert_eq!(tiny.profile, crate::ppocr::OcrProfile::Tiny);
        assert_eq!(small.profile, crate::ppocr::OcrProfile::Small);
        assert!(tiny.source == small.source);
        assert_eq!(tiny.image.bgra, small.image.bgra);
        assert!(!Arc::ptr_eq(&tiny.identity, &small.identity));
    }
}
