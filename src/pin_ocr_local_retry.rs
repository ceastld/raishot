//! 贴图局部行候选：同批实际输入／DTO，独立 TaskSlot，只读候选不写原图历史。
use super::*;
use crate::{
    capture_view::{
        local_retry::{
            candidate::{
                self, clamp_to_viewport, copy_notice, drag_tick, retained_image, retained_result,
                reveal_scroll, CandidateCommandKind, CandidatePanel as Panel, CandidateScrollDrag,
                RevealScroll,
            },
            Binding, Completion, LocalRetry, LocalTarget, SourceFrame, TargetError,
        },
        text_result::readonly_visual_navigation::{VerticalState, VisualLayout},
    },
    pin_view::find_ui::primary_button_down,
    ppocr::OcrProfile,
    recognition::{OcrProvider, RecognizedText},
};
use gpui::{AnyElement, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
use std::{cell::RefCell, ops::Range, rc::Rc, time::Instant};

#[path = "pin_ocr_click_intent.rs"]
mod click_intent;
use click_intent::{ClickIntent, PaintTicket};

#[derive(Clone, Copy)]
enum RetryClickOrigin {
    Initial,
    WithoutCandidate,
}

#[derive(Clone)]
struct RetryClickStamp {
    entry_epoch: Arc<()>,
    source_identity: Arc<()>,
    binding: Binding<TextSelectionStamp>,
    selected: Range<usize>,
    target: LocalTarget,
    visible: bool,
    origin: RetryClickOrigin,
}

struct RetryClick {
    stamp: RetryClickStamp,
    focus: FocusHandle,
    _blur: gpui::Subscription,
}

#[derive(Clone)]
struct RetryErrorStamp {
    entry_epoch: Arc<()>,
    source_identity: Arc<()>,
    binding: Binding<TextSelectionStamp>,
    selected: Option<Range<usize>>,
    visible: bool,
    error: TargetError,
}

pub(super) struct FixedInput {
    image: Arc<Image>,
    source: TextSelectionStamp,
    identity: Arc<()>,
    frame: SourceFrame,
    provider: OcrProvider,
    profile: OcrProfile,
}

impl FixedInput {
    #[cfg(test)]
    pub(super) fn copy_of(
        image: &Image,
        source: TextSelectionStamp,
        image_size: (u32, u32),
        provider: OcrProvider,
    ) -> Option<Self> {
        Self::copy_with_profile(image, source, image_size, provider, OcrProfile::Tiny)
    }
    pub(super) fn copy_with_profile(
        image: &Image,
        source: TextSelectionStamp,
        image_size: (u32, u32),
        provider: OcrProvider,
        profile: OcrProfile,
    ) -> Option<Self> {
        // Pin 的 ROI 在实际输出图内；frame 原点记录该图在贴图物理像素中的裁剪位置。
        // 不以窗口移动／缩放后的屏幕位置冒充原图像素，也不用于安装候选几何。
        let crop = source
            .3
            .unwrap_or_else(|| crate::geometry::Rect::full(image_size.0, image_size.1));
        if provider != OcrProvider::Ppocr || !source.4 || !crop.valid(image_size.0, image_size.1) {
            return None;
        }
        let origin = (i32::try_from(crop.x).ok()?, i32::try_from(crop.y).ok()?);
        origin.0.checked_add(i32::try_from(crop.width).ok()?)?;
        origin.1.checked_add(i32::try_from(crop.height).ok()?)?;
        let image = retained_image(image, (crop.width, crop.height))?;
        Some(Self {
            source,
            identity: Arc::new(()),
            frame: SourceFrame {
                width: image.width,
                height: image.height,
                origin,
            },
            image,
            provider,
            profile,
        })
    }
}

type CandidatePanel = Panel<CandidateScrolling>;
type CandidateButtonStamp = candidate::CandidateButtonStamp<TextSelectionStamp>;
type CandidateCommand = candidate::CandidateCommand<TextSelectionStamp>;
/// 贴图回执额外携带同次 paint 的行停点，供上下键行归属与 reveal 使用。
type CandidateScrollPaint = candidate::CandidateScrollPaint<Option<Arc<CandidateRows>>>;

#[derive(Default)]
struct CandidateScrolling {
    painted: Option<CandidateScrollPaint>,
    drag: Option<CandidateScrollDrag>,
    // 滚轮/取消可结束选字，但仍消费已经拥有的 Left Up，刷新捕获结束后的实际 paint。
    pressed: bool,
    scheduled: Option<Arc<()>>,
    reveal: bool,
    affinity: Option<CandidateRowAffinity>,
    command: Option<CandidateCommand>,
}

struct CandidateRowAffinity {
    geometry: Arc<CandidateRows>,
    row: usize,
    caret: usize,
}

struct CandidateRows {
    // 首末 byte 及横向停点直接来自当前实际 GPUI 折行，与导航的几何判等一致。
    stops: Vec<Vec<(usize, f32)>>,
    top: f32,
    line_height: f32,
}

impl CandidateRows {
    fn same_geometry(&self, other: &Self) -> bool {
        self.stops == other.stops
    }

    fn contains(&self, row: usize, caret: usize) -> bool {
        self.stops
            .get(row)
            .is_some_and(|stops| stops.iter().any(|(byte, _)| *byte == caret))
    }

    fn current_row(&self, caret: usize, affinity: Option<&CandidateRowAffinity>) -> Option<usize> {
        affinity
            .filter(|a| a.caret == caret && self.same_geometry(&a.geometry))
            .map(|a| a.row)
            .filter(|row| self.contains(*row, caret))
            .or_else(|| (0..self.stops.len()).rfind(|row| self.contains(*row, caret)))
    }
}

/// 只复用同次实际 paint 的有界导航停点，不再次扫描 GPUI 字形。
fn candidate_painted_rows(
    navigation: &VisualLayout,
    layout: &gpui::TextLayout,
) -> Option<CandidateRows> {
    let line_height = f32::from(layout.line_height());
    if !line_height.is_finite() || line_height <= 0. {
        return None;
    }
    Some(CandidateRows {
        stops: navigation
            .lines()
            .iter()
            .map(|line| line.stops.iter().map(|stop| (stop.byte, stop.x)).collect())
            .collect(),
        top: f32::from(layout.bounds().top()),
        line_height,
    })
}
struct SourcePaint {
    identity: Arc<()>,
    selected: Range<usize>,
    viewport: crate::viewport::Viewport,
    scale: f32,
}
pub(super) struct LocalOcrState {
    fixed: FixedInput,
    binding: Binding<TextSelectionStamp>,
    retry: LocalRetry<TextSelectionStamp>,
    plan: Option<(Range<usize>, std::result::Result<LocalTarget, TargetError>)>,
    intent_selection: Option<Range<usize>>,
    panel: Option<CandidatePanel>,
    source_painted: Rc<RefCell<Option<SourcePaint>>>,
    retry_click: Rc<RefCell<ClickIntent<RetryClick>>>,
    retry_epoch: Arc<()>,
    focus: FocusHandle,
    title: String,
    notice: String,
    visible: bool,
}

impl LocalOcrState {
    fn cancel_retry_click(&mut self) {
        self.retry_click.borrow_mut().cancel();
        // 持有旧代的 render stamp 保证地址不复用；同代 held redraw 保留按钮点击状态。
        self.retry_epoch = Arc::new(());
    }
}

pub(super) struct PinLocalPaint {
    owner: gpui::WeakEntity<PinView>,
    identity: Arc<()>,
    receipt: Rc<RefCell<Option<SourcePaint>>>,
    retry_click: Rc<RefCell<ClickIntent<RetryClick>>>,
    selected: Range<usize>,
    viewport: crate::viewport::Viewport,
    scale: f32,
}
impl PinLocalPaint {
    /// 只能由原图实际蓝色选字 canvas 在 paint_path 后调用，不用推理完成代替显示。
    pub(super) fn record(&self, w: &Window, cx: &App) {
        let current = self.owner.upgrade().is_some_and(|owner| {
            let view = owner.read(cx);
            w.is_window_active()
                && view.pin_local_focus_owned(w, cx)
                && view.pin_local_input_current(cx)
                && view.pin_local_idle(w, cx)
                && view.text_show_selection
                && view.selected.as_ref() == Some(&self.selected)
                && view.pin_local_viewport(w) == self.viewport
                && w.scale_factor() == self.scale
                && view
                    .pin_local_ocr
                    .as_ref()
                    .is_some_and(|state| Arc::ptr_eq(&state.fixed.identity, &self.identity))
        });
        *self.receipt.borrow_mut() = current.then(|| SourcePaint {
            identity: Arc::clone(&self.identity),
            selected: self.selected.clone(),
            viewport: self.viewport,
            scale: self.scale,
        });
        let click_current = current
            && self.owner.upgrade().is_some_and(|owner| {
                let view = owner.read(cx);
                let clicks = self.retry_click.borrow();
                clicks.intent_token().is_some()
                    && clicks.payload().is_some_and(|click| {
                        click.stamp.selected == self.selected
                            && click.focus.is_focused(w)
                            && view.pin_retry_click_current(&click.stamp, w, cx)
                    })
            });
        if let Some(ticket) = self.retry_click.borrow_mut().painted(click_current) {
            // 只能由这次原图蓝选真实 paint 出票；不可在 click 时猜测下一帧已画好。
            let owner = self.owner.clone();
            w.on_next_frame(move |w, cx| {
                let _ = owner.update(cx, |view, cx| {
                    view.run_pin_retry_click(&ticket, w, cx);
                });
            });
        }
    }
}

fn result_current(
    installed: &Binding<TextSelectionStamp>,
    result: Option<&Arc<RecognizedText>>,
    source: TextSelectionStamp,
    generation: u64,
) -> bool {
    source == installed.source
        && generation == installed.result_generation
        && result.is_some_and(|result| Arc::ptr_eq(result, &installed.result))
}

impl PinView {
    fn pin_local_viewport(&self, w: &Window) -> crate::viewport::Viewport {
        // 使用宿主当前实际内容视口；外观留白不成为 OCR 位置。
        self.content_viewport(w)
    }
    fn pin_local_chrome_bounds(&self, w: &Window) -> crate::annotation::Bounds {
        let viewport = self.pin_local_viewport(w);
        let dpi = w.scale_factor();
        let content = self.content_rect();
        let start = viewport.window_point(crate::annotation::Point {
            x: content.x as f32,
            y: content.y as f32,
        });
        let end = viewport.window_point(crate::annotation::Point {
            x: (content.x + content.width) as f32,
            y: (content.y + content.height) as f32,
        });
        let left = (start.x / dpi).max(0.);
        let top = (start.y / dpi).max(0.);
        let right = (end.x / dpi).min(f32::from(w.viewport_size().width));
        let bottom = (end.y / dpi).min(f32::from(w.viewport_size().height));
        crate::annotation::Bounds {
            x: left,
            y: top,
            width: (right - left).max(1.),
            height: (bottom - top).max(1.),
        }
    }
    pub(super) fn pin_local_paint_probe(
        &self,
        viewport: crate::viewport::Viewport,
        scale: f32,
        cx: &Context<Self>,
    ) -> Option<PinLocalPaint> {
        let state = self.pin_local_ocr.as_ref()?;
        Some(PinLocalPaint {
            owner: cx.entity().downgrade(),
            identity: Arc::clone(&state.fixed.identity),
            receipt: state.source_painted.clone(),
            retry_click: state.retry_click.clone(),
            selected: self.selected.clone()?,
            viewport,
            scale,
        })
    }
    pub(super) fn install_pin_local_input(
        &mut self,
        fixed: Option<FixedInput>,
        result: Arc<RecognizedText>,
        cx: &Context<Self>,
    ) {
        let Some(fixed) = fixed else { return };
        if !self.text_mode
            || self.text_source != ocr_clipboard::TextSource::Ocr
            || self.text_selection_source(cx) != fixed.source
            || self.copy_layout_generation == u64::MAX
            || !retained_result(&result)
            || self
                .text
                .as_ref()
                .is_none_or(|body| !Arc::ptr_eq(body, &result))
        {
            return;
        }
        // Pin 正文没有原地编辑器；不可变同批 DTO Arc 就是当前正文身份。
        // 新完整 OCR，即使字节相同，也换 DTO Arc；编辑图像／历史恢复走原来源失效门。
        let body_identity = Arc::new(());
        let binding = Binding {
            source: fixed.source,
            source_identity: Arc::clone(&fixed.identity),
            result,
            result_generation: self.copy_layout_generation,
            installed_body_identity: Arc::clone(&body_identity),
            body_identity,
            installed_body_revision: 0,
            body_revision: 0,
            frame: fixed.frame,
            provider: fixed.provider,
        };
        self.pin_local_ocr = Some(LocalOcrState {
            fixed,
            binding,
            retry: LocalRetry::default(),
            plan: None,
            intent_selection: None,
            panel: None,
            source_painted: Default::default(),
            retry_click: Default::default(),
            retry_epoch: Arc::new(()),
            focus: cx.focus_handle(),
            title: String::new(),
            notice: String::new(),
            visible: false,
        });
    }

    fn pin_local_input_current(&self, cx: &App) -> bool {
        self.text_mode
            && self.text_source == ocr_clipboard::TextSource::Ocr
            && self.text_selection_stamp == Some(self.text_selection_source(cx))
            && self.pin_local_ocr.as_ref().is_some_and(|state| {
                result_current(
                    &state.binding,
                    self.text.as_ref(),
                    self.text_selection_source(cx),
                    self.copy_layout_generation,
                )
            })
    }
    pub(super) fn pin_local_progress(
        &self,
        cx: &App,
    ) -> Option<(Arc<()>, u64, crate::geometry::Rect)> {
        if !self.pin_local_input_current(cx) {
            return None;
        }
        let state = self.pin_local_ocr.as_ref()?;
        let binding = &state.binding;
        let selected = self.selected.as_ref()?;
        if state.fixed.source != binding.source
            || !Arc::ptr_eq(&state.fixed.identity, &binding.source_identity)
            || binding.frame != state.fixed.frame
            || binding.provider != state.fixed.provider
            || !Arc::ptr_eq(&binding.body_identity, &binding.installed_body_identity)
            || binding.body_revision != binding.installed_body_revision
            || state.intent_selection.as_ref() != Some(selected)
            || selected.start >= selected.end
            || selected.end > binding.result.text.len()
            || !binding.result.text.is_char_boundary(selected.start)
            || !binding.result.text.is_char_boundary(selected.end)
        {
            return None;
        }
        let (task, target) = state.retry.pending_visual()?;
        let roi = target.roi();
        if target.selected() != *selected || !roi.valid(binding.frame.width, binding.frame.height) {
            return None;
        }
        // 固定裁剪图的原点属于原图像素范围，与窗口／虚拟桌面原点无关。
        let x = binding
            .frame
            .origin
            .0
            .checked_add(i32::try_from(roi.x).ok()?)?;
        let y = binding
            .frame
            .origin
            .1
            .checked_add(i32::try_from(roi.y).ok()?)?;
        let roi = crate::geometry::Rect {
            x: u32::try_from(x).ok()?,
            y: u32::try_from(y).ok()?,
            ..roi
        };
        roi.valid(self.image_size.0, self.image_size.1)
            .then(|| (Arc::clone(&state.fixed.identity), task, roi))
    }
    fn pin_local_body(&self) -> Option<&str> {
        self.text.as_ref().map(|text| text.text.as_str())
    }
    fn pin_local_binding(&self, cx: &App) -> Option<Binding<TextSelectionStamp>> {
        self.pin_local_input_current(cx)
            .then(|| self.pin_local_ocr.as_ref().unwrap().binding.clone())
    }
    pub(super) fn pin_local_candidate_focused(&self, w: &Window) -> bool {
        self.pin_local_ocr
            .as_ref()
            .is_some_and(|state| state.panel.is_some() && state.focus.is_focused(w))
    }
    fn pin_local_focus_owned(&self, w: &Window, cx: &App) -> bool {
        self.focus.is_focused(w)
            || self.ocr_find_input_focused(w, cx)
            || self.pin_local_candidate_focused(w)
    }
    fn pin_local_idle(&self, w: &Window, cx: &App) -> bool {
        self.pin_candidate_idle(w, cx, false)
    }
    fn pin_candidate_idle(&self, w: &Window, cx: &App, begin: bool) -> bool {
        // GPUI 在 body 的 MouseDown 前已捕获本窗口。只为当前候选左拖选放宽空捕获门，
        // 完整保留 Find 的其它宿主状态检查，不释放捕获来绕过原有保护。
        let candidate_capture = self.pin_local_ocr.as_ref().is_some_and(|state| {
            state.visible
                && state.panel.as_ref().is_some_and(|panel| {
                    (begin
                        || (state.focus.is_focused(w)
                            && panel.editor.dragging()
                            && panel.scrolling.borrow().drag.is_some()))
                        && unsafe { GetCapture() } == hwnd(w)
                        && primary_button_down()
                })
        });
        let candidate_idle = candidate_capture
            && self.text_mode
            && self.text_source == ocr_clipboard::TextSource::Ocr
            && !self.saving
            && !self.processing
            && !self.processing_image_unavailable
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
            && self.text_selection.is_some();
        // 实际宿主原门包含原生编辑、IME、参数、菜单、鼠标捕获和原图拖选。
        !self.presentation_geometry_adjusting
            && (self.pin_find_idle(cx) || candidate_idle)
            && !self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx))
    }
    fn pin_local_source_painted(&self, w: &Window) -> bool {
        self.pin_local_ocr.as_ref().is_some_and(|state| {
            state.source_painted.borrow().as_ref().is_some_and(|paint| {
                Arc::ptr_eq(&paint.identity, &state.fixed.identity)
                    && self.selected.as_ref() == Some(&paint.selected)
                    && paint.viewport == self.pin_local_viewport(w)
                    && paint.scale == w.scale_factor()
            })
        })
    }
    fn pin_local_action_ready(&self, w: &Window, cx: &App) -> bool {
        self.pin_candidate_action_ready(w, cx, false)
    }
    fn pin_candidate_action_ready(&self, w: &Window, cx: &App, begin: bool) -> bool {
        w.is_window_active()
            && self.pin_local_focus_owned(w, cx)
            && self.pin_local_input_current(cx)
            && self.pin_candidate_idle(w, cx, begin)
            && self.pin_local_source_painted(w)
            && self.pin_local_ocr.as_ref().is_some_and(|state| {
                state
                    .intent_selection
                    .as_ref()
                    .is_none_or(|range| self.selected.as_ref() == Some(range))
            })
    }
    pub(super) fn sync_pin_local_ocr(&mut self, w: &Window, cx: &App) {
        if self.pin_local_ocr.is_some() && !self.pin_local_input_current(cx) {
            self.pin_local_ocr = None;
            return;
        }
        if !w.is_window_active() {
            self.cancel_pin_local_intent("贴图失焦，撤销局部意图");
        }
        let invalid_click = self.pin_local_ocr.as_ref().is_some_and(|state| {
            state.retry_click.borrow().payload().is_some_and(|click| {
                !click.focus.is_focused(w) || !self.pin_retry_click_current(&click.stamp, w, cx)
            })
        });
        if invalid_click {
            self.pin_local_ocr.as_mut().unwrap().cancel_retry_click();
        }
        // 只在非空选择真正改变且宿主空闲时解析一次。平常帧不扫描正文或复制像素。
        if !self.pin_local_idle(w, cx) {
            return;
        }
        let Some(selected) = self.selected.clone().filter(|range| !range.is_empty()) else {
            return;
        };
        if let Some(state) = &mut self.pin_local_ocr {
            if state
                .plan
                .as_ref()
                .is_none_or(|(range, _)| *range != selected)
            {
                state.cancel_retry_click();
                state.plan = Some((
                    selected.clone(),
                    LocalTarget::resolve(&state.binding.result, selected, state.fixed.frame, 4),
                ));
            }
        }
    }
    pub(super) fn cancel_pin_local_intent(&mut self, reason: &str) {
        if let Some(state) = &mut self.pin_local_ocr {
            state.cancel_retry_click();
            state.retry.cancel(reason);
            state.panel = None;
            state.plan = None;
            state.intent_selection = None;
            state.visible = false;
            state.notice.clear();
            state.source_painted.borrow_mut().take();
        }
    }
    fn pin_retry_target_matches(left: &LocalTarget, right: &LocalTarget) -> bool {
        left.line_index() == right.line_index()
            && left.selected() == right.selected()
            && left.roi() == right.roi()
    }

    fn pin_retry_render_stamp(
        &self,
        selected: Range<usize>,
        target: &LocalTarget,
        origin: RetryClickOrigin,
    ) -> Option<RetryClickStamp> {
        let state = self.pin_local_ocr.as_ref().filter(|s| s.panel.is_none())?;
        Some(RetryClickStamp {
            entry_epoch: Arc::clone(&state.retry_epoch),
            source_identity: Arc::clone(&state.fixed.identity),
            binding: state.binding.clone(),
            selected,
            target: target.clone(),
            visible: state.visible,
            origin,
        })
    }

    fn pin_retry_click_current(&self, stamp: &RetryClickStamp, w: &Window, cx: &App) -> bool {
        if !w.is_window_active()
            || !self.pin_local_focus_owned(w, cx)
            || !self.pin_local_input_current(cx)
            || !self.pin_local_idle(w, cx)
            || !self.text_show_selection
            || self.text_selection.is_none()
            || stamp.selected.is_empty()
            || self.selected.as_ref() != Some(&stamp.selected)
        {
            return false;
        }
        self.pin_local_ocr.as_ref().is_some_and(|state| {
            let binding = &state.binding;
            if state.panel.is_some()
                || state.visible != stamp.visible
                || !Arc::ptr_eq(&state.retry_epoch, &stamp.entry_epoch)
                || !Self::pin_retry_binding_current(state, &stamp.source_identity, &stamp.binding)
            {
                return false;
            }
            let target = match stamp.origin {
                RetryClickOrigin::Initial => state.plan.as_ref().and_then(|(range, target)| {
                    (range == &stamp.selected)
                        .then(|| target.as_ref().ok())
                        .flatten()
                }),
                RetryClickOrigin::WithoutCandidate => {
                    state
                        .retry
                        .target(binding, self.pin_local_body().unwrap_or(""), true)
                }
            };
            target.is_some_and(|target| Self::pin_retry_target_matches(target, &stamp.target))
        })
    }

    fn pin_retry_binding_current(
        state: &LocalOcrState,
        source_identity: &Arc<()>,
        installed: &Binding<TextSelectionStamp>,
    ) -> bool {
        let binding = &state.binding;
        Arc::ptr_eq(&state.fixed.identity, source_identity)
            && state.fixed.source == installed.source
            && binding.source == installed.source
            && Arc::ptr_eq(&binding.source_identity, &installed.source_identity)
            && Arc::ptr_eq(&binding.result, &installed.result)
            && binding.result_generation == installed.result_generation
            && Arc::ptr_eq(&binding.body_identity, &installed.body_identity)
            && Arc::ptr_eq(
                &binding.installed_body_identity,
                &installed.installed_body_identity,
            )
            && binding.body_revision == installed.body_revision
            && binding.installed_body_revision == installed.installed_body_revision
            && binding.frame == installed.frame
            && binding.provider == installed.provider
            && state.fixed.frame == installed.frame
            && state.fixed.provider == installed.provider
    }

    fn report_pin_retry_error(
        &mut self,
        stamp: &RetryErrorStamp,
        w: &Window,
        cx: &mut Context<Self>,
    ) {
        if w.is_window_active()
            && self.pin_local_focus_owned(w, cx)
            && self.pin_local_input_current(cx)
            && self.pin_local_idle(w, cx)
            && self.selected == stamp.selected
            && self.pin_local_ocr.as_ref().is_some_and(|state| {
                state.panel.is_none()
                    && state.visible == stamp.visible
                    && Arc::ptr_eq(&state.retry_epoch, &stamp.entry_epoch)
                    && Self::pin_retry_binding_current(
                        state,
                        &stamp.source_identity,
                        &stamp.binding,
                    )
            })
        {
            self.status = target_error(stamp.error).into();
            cx.notify();
        }
    }

    fn queue_pin_retry_click(
        &mut self,
        stamp: &RetryClickStamp,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pin_retry_click_current(stamp, w, cx) {
            return;
        }
        if self
            .text_selection
            .as_ref()
            .is_none_or(|text| text.highlights(stamp.selected.clone()).is_empty())
        {
            // 空隙选择可能有合法行 ROI，却不会产生蓝字 paint；不留下无出票意图。
            self.status = target_error(TargetError::InvalidSelection).into();
            cx.notify();
            return;
        }
        // 仅实际 click 后解析一次，坏行不自动跑 worker，也不借旧 SourcePaint 放行。
        let target = match LocalTarget::resolve(
            &stamp.binding.result,
            stamp.selected.clone(),
            stamp.binding.frame,
            4,
        ) {
            Ok(target) if Self::pin_retry_target_matches(&target, &stamp.target) => target,
            Ok(_) => return,
            Err(error) => {
                self.status = target_error(error).into();
                cx.notify();
                return;
            }
        };
        let Some(focus) = w.focused(cx) else { return };
        let token = Arc::new(());
        let blur_token = Arc::clone(&token);
        let blur = cx.on_blur(&focus, w, move |view, _, cx| {
            if let Some(state) = &mut view.pin_local_ocr {
                let cancelled = state.retry_click.borrow_mut().cancel_token(&blur_token);
                if cancelled {
                    state.retry_epoch = Arc::new(());
                }
            }
            cx.notify();
        });
        let mut stamp = stamp.clone();
        stamp.target = target;
        self.pin_local_ocr
            .as_ref()
            .unwrap()
            .retry_click
            .borrow_mut()
            .queue(
                token,
                RetryClick {
                    stamp,
                    focus,
                    _blur: blur,
                },
            );
        cx.notify();
    }

    fn run_pin_retry_click(
        &mut self,
        ticket: &PaintTicket,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(clicks) = self.pin_local_ocr.as_ref().map(|s| s.retry_click.clone()) else {
            return;
        };
        let current = clicks.borrow().payload().is_some_and(|click| {
            click.focus.is_focused(w)
                && self.pin_local_source_painted(w)
                && self.pin_retry_click_current(&click.stamp, w, cx)
                && LocalTarget::resolve(
                    &click.stamp.binding.result,
                    click.stamp.selected.clone(),
                    click.stamp.binding.frame,
                    4,
                )
                .is_ok_and(|target| Self::pin_retry_target_matches(&target, &click.stamp.target))
        });
        let click = clicks.borrow_mut().take(ticket, current);
        if let Some(click) = click {
            // take 已先消费 intent 与真实 paint 双 token，再走原运行路线。
            self.run_pin_local_ocr(click.stamp.selected, w, cx);
        }
    }
    fn run_pin_local_ocr(
        &mut self,
        selected: Range<usize>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &mut self.pin_local_ocr {
            state.cancel_retry_click();
        }
        self.sync_pin_local_ocr(w, cx);
        if !self.pin_local_action_ready(w, cx)
            || self.selected.as_ref() != Some(&selected)
            || self
                .pin_local_ocr
                .as_ref()
                .and_then(|s| s.panel.as_ref())
                .is_some_and(|p| p.editor.dragging())
        {
            return;
        }
        let Some(binding) = self.pin_local_binding(cx) else {
            return;
        };
        let candidate_was_focused = self.pin_local_candidate_focused(w);
        let body = self.pin_local_body().expect("局部当前正文").to_owned();
        let state = self.pin_local_ocr.as_mut().expect("局部输入已核验");
        let request = match state
            .retry
            .begin(&binding, &body, selected.clone(), 4, true)
        {
            Ok(request) => request,
            Err(error) => {
                self.status = target_error(error).into();
                cx.notify();
                return;
            }
        };
        let image = match request.target().extract_pixels(&state.fixed.image) {
            Ok(image) => image,
            Err(error) => {
                state.retry.cancel("固定输入采样失败");
                state.notice = target_error(error).into();
                state.panel = None;
                state.visible = true;
                if candidate_was_focused && w.is_window_active() {
                    w.focus(&self.focus);
                }
                cx.notify();
                return;
            }
        };
        // 显式重试撤掉有焦点的候选前恢复本窗口 OCR 归属；回执绝不抢焦点。
        if candidate_was_focused && w.is_window_active() {
            w.focus(&self.focus);
        }
        state.title = trf!(
            "第{}行候选 · 原文保留",
            "Line {} candidate · original kept",
            request.target().line_index() + 1
        );
        state.notice = if state.retry.pending(&binding, &body, true) {
            tr!("正在本机重识别此行…", "Retrying this line locally…").into()
        } else {
            String::new()
        };
        state.panel = None;
        state.intent_selection = Some(selected);
        state.visible = true;
        let token = request.token().clone();
        let provider = request.provider();
        // 使用产生原 DTO 的固定输入档位；局部重试不读取后继设置或改用其它模型。
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
                view.sync_pin_local_ocr(w, cx);
                let Some(binding) = view.pin_local_binding(cx) else {
                    return;
                };
                let ready = view.pin_local_action_ready(w, cx);
                let body = view.pin_local_body().expect("候选回执当前正文").to_owned();
                let Some(state) = &mut view.pin_local_ocr else {
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
                        let identity = Arc::new(());
                        let blur_identity = Arc::clone(&identity);
                        let blur = cx.on_blur(&state.focus, w, move |view, _, cx| {
                            view.stop_pin_candidate_scrolling(&blur_identity);
                            cx.notify();
                        });
                        state.panel = Some(CandidatePanel::new(
                            identity,
                            candidate.text().into(),
                            Some(blur),
                        ));
                        state.notice = tr!(
                            "拖选候选后 Ctrl+C；原文与标记保留",
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
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn pin_candidate_frame_ready(&self, frame: &Arc<()>, w: &Window, cx: &App) -> bool {
        self.pin_candidate_frame_ready_for_drag(frame, w, cx, false)
    }
    fn pin_candidate_frame_ready_for_drag(
        &self,
        frame: &Arc<()>,
        w: &Window,
        cx: &App,
        begin: bool,
    ) -> bool {
        self.pin_candidate_action_ready(w, cx, begin)
            && self
                .pin_local_ocr
                .as_ref()
                .and_then(|s| s.panel.as_ref())
                .is_some_and(|p| p.painted_frame(frame))
    }

    fn copy_pin_local_candidate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(frame) = self
            .pin_local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .map(|p| Arc::clone(&p.identity))
        else {
            return;
        };
        if !self.pin_candidate_frame_ready(&frame, w, cx) {
            return;
        }
        let Some(binding) = self.pin_local_binding(cx) else {
            return;
        };
        let body = self.pin_local_body().expect("复制候选当前正文").to_owned();
        let state = self.pin_local_ocr.as_mut().expect("候选已核验");
        let panel = state.panel.as_ref().expect("候选已核验");
        if panel.editor.dragging() {
            return;
        }
        let range = panel.editor.input.selection.clone();
        let result = state.retry.copy_candidate(&binding, &body, true, |text| {
            let payload = candidate::candidate_payload(text, range);
            Self::publish_ocr_selected_text(payload, w).map(|_| ())
        });
        state.notice = copy_notice(
            result,
            tr!(
                "候选已复制；贴图保持打开",
                "Candidate copied; pin stays open"
            ),
        );
        cx.notify();
    }

    fn stop_pin_candidate_scrolling(&mut self, frame: &Arc<()>) {
        let Some(panel) = self
            .pin_local_ocr
            .as_mut()
            .and_then(|s| s.panel.as_mut())
            .filter(|p| Arc::ptr_eq(&p.identity, frame))
        else {
            return;
        };
        panel.editor.end_drag();
        panel.vertical = VerticalState::default();
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.drag = None;
        scrolling.pressed = false;
        scrolling.scheduled = None;
        scrolling.reveal = false;
        scrolling.affinity = None;
        scrolling.command = None;
    }

    fn stop_pin_candidate_for_wheel(&mut self, w: &Window, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.pin_local_ocr {
            state.cancel_retry_click();
        }
        if let Some(panel) = self.pin_local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            panel.editor.end_drag();
            panel.vertical = VerticalState::default();
            panel.visual.borrow_mut().invalidate();
            let pressed = panel.scrolling.borrow().pressed
                && unsafe { GetCapture() } == hwnd(w)
                && primary_button_down();
            *panel.scrolling.borrow_mut() = CandidateScrolling {
                pressed,
                ..Default::default()
            };
        }
        cx.notify();
    }

    fn pin_candidate_button_current(&self, stamp: &CandidateButtonStamp) -> bool {
        self.pin_local_ocr.as_ref().is_some_and(|state| {
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

    fn queue_pin_candidate_command(
        &mut self,
        kind: CandidateCommandKind,
        stamp: &CandidateButtonStamp,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Click 已释放 native capture；本处只记显式意图，绝不借旧 paint 执行命令。
        if !w.is_window_active()
            || !self.pin_local_focus_owned(w, cx)
            || !self.pin_local_input_current(cx)
            || !self.pin_local_idle(w, cx)
            || !self.pin_candidate_button_current(stamp)
        {
            return;
        }
        let Some(focus) = w.focused(cx) else { return };
        let token = Arc::new(());
        let blur_token = Arc::clone(&token);
        let blur_frame = Arc::clone(&stamp.frame);
        let blur = cx.on_blur(&focus, w, move |view, _, cx| {
            view.cancel_pin_candidate_command(&blur_frame, &blur_token);
            cx.notify();
        });
        let panel = self.pin_local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
        panel.scrolling.borrow_mut().command = Some(CandidateCommand {
            token,
            kind,
            stamp: stamp.clone(),
            focus,
            _blur: blur,
            scheduled: false,
        });
        cx.notify();
    }

    fn cancel_pin_candidate_command(&mut self, frame: &Arc<()>, token: &Arc<()>) {
        if let Some(panel) = self.pin_local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            if !Arc::ptr_eq(&panel.identity, frame) {
                return;
            }
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

    fn run_queued_pin_candidate_command(
        &mut self,
        frame: &Arc<()>,
        token: &Arc<()>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.pin_local_ocr.as_ref().and_then(|state| {
            let panel = state
                .panel
                .as_ref()
                .filter(|p| Arc::ptr_eq(&p.identity, frame))?;
            let scrolling = panel.scrolling.borrow();
            let command = scrolling
                .command
                .as_ref()
                .filter(|c| Arc::ptr_eq(&c.token, token))?;
            Some((command.kind, command.stamp.clone(), command.focus.clone()))
        });
        let Some((kind, stamp, focus)) = request else {
            return;
        };
        let ready = self.pin_candidate_frame_ready(frame, w, cx)
            && focus.is_focused(w)
            && self.pin_candidate_button_current(&stamp)
            && self
                .pin_local_ocr
                .as_ref()
                .and_then(|s| s.panel.as_ref())
                .is_some_and(|p| Self::pin_candidate_scroll_paint(p, w).is_some());
        // 先消费单次 token；原复制或重试即使通知/替换 panel 也不会再次执行。
        self.cancel_pin_candidate_command(frame, token);
        if !ready {
            return;
        }
        match kind {
            CandidateCommandKind::Copy => self.copy_pin_local_candidate(w, cx),
            CandidateCommandKind::Retry => self.retry_pin_local_candidate(w, cx),
        }
    }

    fn retry_pin_local_candidate(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let selected = self.pin_local_binding(cx).and_then(|binding| {
            self.pin_local_ocr.as_ref().and_then(|state| {
                state
                    .retry
                    .target(
                        &binding,
                        self.pin_local_body().unwrap_or(""),
                        self.pin_local_action_ready(w, cx),
                    )
                    .map(|target| target.selected())
            })
        });
        if let Some(selected) = selected {
            self.run_pin_local_ocr(selected, w, cx);
        }
    }

    fn pin_candidate_scroll_paint(
        panel: &CandidatePanel,
        w: &Window,
    ) -> Option<CandidateScrollPaint> {
        let paint = panel.scrolling.borrow().painted.clone()?;
        paint.current(panel, w).then_some(paint)
    }

    fn pin_candidate_scroll_tick(
        &mut self,
        frame: &Arc<()>,
        token: &Arc<()>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scheduled = self
            .pin_local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .filter(|p| Arc::ptr_eq(&p.identity, frame))
            .is_some_and(|p| {
                p.scrolling
                    .borrow()
                    .scheduled
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, token))
            });
        if !scheduled {
            return;
        }
        if !self.pin_candidate_frame_ready(frame, w, cx) || !self.pin_local_candidate_focused(w) {
            self.stop_pin_candidate_scrolling(frame);
            return;
        }
        let panel = self.pin_local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
        let Some(paint) = Self::pin_candidate_scroll_paint(panel, w) else {
            // 新 offset 尚未 paint 时撤本次续约，下一次真实 paint 会继续。
            panel.scrolling.borrow_mut().scheduled = None;
            cx.notify();
            return;
        };
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.scheduled = None;
        let top = f32::from(paint.viewport.top());
        let bottom = f32::from(paint.viewport.bottom());
        let offset_y = f32::from(paint.offset.y);
        let max_scroll_y = f32::from(panel.scroll.max_offset().height);
        let mut new_offset = None;
        let mut selection_changed = false;
        if scrolling.reveal {
            scrolling.reveal = false;
            let caret = panel.editor.caret();
            let row_top = paint.rows.as_ref().and_then(|rows| {
                scrolling
                    .affinity
                    .as_ref()
                    .filter(|a| a.caret == caret && rows.same_geometry(&a.geometry))
                    .filter(|a| rows.contains(a.row, caret))
                    .map(|a| a.row)
                    .map(|row| rows.top + row as f32 * rows.line_height)
            });
            let caret_top = row_top.or_else(|| {
                paint
                    .layout
                    .position_for_index(caret)
                    .map(|p| f32::from(p.y))
            });
            if let Some(caret_top) = caret_top {
                new_offset = reveal_scroll(RevealScroll {
                    viewport_top: top,
                    viewport_bottom: bottom,
                    offset_y,
                    max_scroll_y,
                    caret_top,
                    caret_bottom: caret_top + f32::from(paint.layout.line_height()),
                });
            }
        } else if let Some(drag) = scrolling.drag.as_mut().filter(|_| panel.editor.dragging()) {
            // 先用此 offset 的当前真实布局命中；改 offset 后等待下一个实际 paint。
            (selection_changed, new_offset) = drag_tick(
                &mut panel.editor,
                drag,
                &paint.layout,
                paint.viewport,
                offset_y,
                max_scroll_y,
            );
        }
        if let Some(y) = new_offset {
            panel.scroll.set_offset(gpui::point(paint.offset.x, px(y)));
            scrolling.painted = None;
            panel.visual.borrow_mut().invalidate();
        }
        if new_offset.is_some() || selection_changed {
            cx.notify();
        }
    }

    fn pin_candidate_drag_move(
        &mut self,
        frame: &Arc<()>,
        e: &MouseMoveEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let dragging = self
            .pin_local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .filter(|p| Arc::ptr_eq(&p.identity, frame))
            .is_some_and(|p| p.editor.dragging() && p.scrolling.borrow().drag.is_some());
        if !dragging {
            return false;
        }
        if e.pressed_button != Some(MouseButton::Left)
            || !self.pin_candidate_frame_ready(frame, w, cx)
            || !self.pin_local_candidate_focused(w)
        {
            self.stop_pin_candidate_scrolling(frame);
            return true;
        }
        let panel = self.pin_local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
        if let Some(drag) = panel.scrolling.borrow_mut().drag.as_mut() {
            drag.pointer = e.position;
        }
        let Some(paint) = Self::pin_candidate_scroll_paint(panel, w) else {
            // 高频鼠标可先到达；不拿旧布局命中，也不打断等待新 paint 的拖选。
            cx.notify();
            return true;
        };
        self.pin_local_candidate_selection(
            (frame, &paint.epoch),
            &paint.layout,
            e.position,
            (false, false),
            w,
            cx,
        );
        true
    }

    fn pin_candidate_drag_up(
        &mut self,
        frame: &Arc<()>,
        e: &MouseUpEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let panel = self
            .pin_local_ocr
            .as_ref()
            .and_then(|s| s.panel.as_ref())
            .filter(|p| Arc::ptr_eq(&p.identity, frame));
        if e.button != MouseButton::Left {
            return false;
        }
        if panel.is_none_or(|p| !p.scrolling.borrow().pressed) {
            // 已取消的 press 不消费后来按钮的 Up；原生捕获结束仍刷新当前来源 paint。
            if panel.is_some() {
                cx.notify();
            }
            return false;
        }
        if let Some(paint) = Self::pin_candidate_scroll_paint(panel.unwrap(), w) {
            self.pin_local_candidate_selection(
                (frame, &paint.epoch),
                &paint.layout,
                e.position,
                (false, true),
                w,
                cx,
            );
        }
        self.stop_pin_candidate_scrolling(frame);
        if let Some(panel) = self.pin_local_ocr.as_mut().and_then(|s| s.panel.as_mut()) {
            panel.scrolling.borrow_mut().pressed = false;
        }
        cx.notify();
        true
    }

    pub(super) fn pin_local_candidate_key(
        &mut self,
        e: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.pin_local_candidate_focused(w) {
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
                    self.cancel_pin_local_intent("显式丢弃候选");
                    if w.is_window_active() {
                        w.focus(&self.focus);
                    }
                }
                "c" if m.control && !m.shift => self.copy_pin_local_candidate(w, cx),
                key => {
                    let Some(frame) = self
                        .pin_local_ocr
                        .as_ref()
                        .and_then(|s| s.panel.as_ref())
                        .map(|p| Arc::clone(&p.identity))
                    else {
                        return true;
                    };
                    if !self.pin_candidate_frame_ready(&frame, w, cx) {
                        return true;
                    }
                    let panel = self.pin_local_ocr.as_mut().unwrap().panel.as_mut().unwrap();
                    let scroll_paint = Self::pin_candidate_scroll_paint(panel, w);
                    panel.editor.end_drag();
                    // 上下键要整体借用 panel；滚动状态改从同一 Rc 的副本借用。
                    let scrolling = Rc::clone(&panel.scrolling);
                    let mut scrolling = scrolling.borrow_mut();
                    scrolling.drag = None;
                    scrolling.command = None;
                    scrolling.scheduled = None;
                    scrolling.pressed &=
                        unsafe { GetCapture() } == hwnd(w) && primary_button_down();
                    match key {
                        "a" if m.control && !m.shift => {
                            panel.vertical = VerticalState::default();
                            panel.editor.select_all();
                            scrolling.affinity = None;
                            scrolling.reveal = true;
                        }
                        "left" | "right" | "home" | "end" => {
                            panel.vertical = VerticalState::default();
                            panel.editor.readonly_navigate(key, m.shift, m.control);
                            scrolling.affinity = None;
                            scrolling.reveal = true;
                        }
                        "up" | "down" if !m.control && !panel.editor.composing() => {
                            let rows = scroll_paint.and_then(|p| p.rows);
                            let row = rows.as_ref().and_then(|rows| {
                                rows.current_row(panel.editor.caret(), scrolling.affinity.as_ref())
                            });
                            if let Some(caret) = panel.move_vertical(key == "down", m.shift, w) {
                                scrolling.affinity = rows.zip(row).map(|(geometry, row)| {
                                    let destination = if key == "down" {
                                        (row + 1).min(geometry.stops.len().saturating_sub(1))
                                    } else {
                                        row.saturating_sub(1)
                                    };
                                    CandidateRowAffinity {
                                        geometry,
                                        row: destination,
                                        caret,
                                    }
                                });
                                scrolling.reveal = true;
                            } else {
                                scrolling.affinity = None;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        cx.notify();
        true
    }

    fn pin_local_candidate_selection(
        &mut self,
        paint_token: (&Arc<()>, &Arc<()>),
        layout: &gpui::TextLayout,
        mut position: gpui::Point<gpui::Pixels>,
        phase: (bool, bool),
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (frame, epoch) = paint_token;
        let (begin, end) = phase;
        if !self.pin_candidate_frame_ready_for_drag(frame, w, cx, begin) {
            return;
        }
        let state = self.pin_local_ocr.as_mut().expect("候选帧已核验");
        let panel = state.panel.as_mut().unwrap();
        if !panel.visual_current(epoch, w) {
            return;
        }
        let editor = &mut panel.editor;
        if !begin && !editor.dragging() {
            return;
        }
        panel.vertical = VerticalState::default();
        let mut scrolling = panel.scrolling.borrow_mut();
        scrolling.affinity = None;
        scrolling.command = None;
        scrolling.reveal = false;
        if begin {
            scrolling.pressed = true;
            scrolling.scheduled = None;
            scrolling.drag = Some(CandidateScrollDrag {
                pointer: position,
                last_tick: Instant::now(),
            });
        } else {
            if let Some(drag) = scrolling.drag.as_mut() {
                drag.pointer = position;
            }
            position = clamp_to_viewport(position, panel.scroll.bounds());
        }
        let index = layout
            .index_for_position(position)
            .unwrap_or_else(|index| index)
            .min(editor.text.len());
        if begin {
            editor.readonly_begin_drag(index);
            w.focus(&state.focus);
        } else if editor.dragging() {
            editor.readonly_extend_drag(index);
        }
        if end {
            editor.end_drag();
            scrolling.drag = None;
            scrolling.scheduled = None;
        }
        cx.notify();
    }

    fn render_pin_local_candidate(&self, w: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let state = self.pin_local_ocr.as_ref().filter(|s| s.visible)?;
        if !self.pin_local_input_current(cx) {
            return None;
        }
        let p = crate::theme::palette(cx);
        let bounds = self.pin_local_chrome_bounds(w);
        let width = (bounds.width - 16.).clamp(1., 320.);
        let x = bounds.x + (bounds.width - width - 8.).max(0.);
        let reserved: f32 = if self.ocr_find.is_some() { 48. } else { 4. };
        let y = (bounds.y + bounds.height - 230.)
            .max(bounds.y + reserved.min((bounds.height - 1.).max(0.)));
        let mut root = div()
            .id("pin-local-candidate")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .max_h(px((bounds.y + bounds.height - y - 4.).max(1.)))
            .overflow_y_scroll()
            .p_2()
            .rounded_md()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(cx.listener(|v, _, w, cx| {
                v.stop_pin_candidate_for_wheel(w, cx);
                cx.stop_propagation();
            }))
            .child(state.title.clone());
        if let Some(panel) = &state.panel {
            // Render 即撤掉上一帧的视觉行；仅本 epoch 的真实 paint 可以安装停点。
            let epoch = panel.visual.borrow_mut().begin_render();
            let text = panel.styled_text();
            let layout = text.layout().clone();
            let paint = panel.paint_stamp(Arc::clone(&epoch), layout.clone());
            let frame = Arc::clone(&panel.identity);
            let painted = panel.painted.clone();
            let paint_frame = Arc::clone(&frame);
            let visual = panel.visual.clone();
            let scroll = panel.scroll.clone();
            let scrolling = panel.scrolling.clone();
            let owner = cx.entity().downgrade();
            let body = div()
                .id("pin-local-candidate-text")
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
                        v.pin_local_candidate_selection(
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
                .on_scroll_wheel(cx.listener(|v, _, w, cx| {
                    v.stop_pin_candidate_for_wheel(w, cx);
                    cx.stop_propagation();
                }))
                .child(text)
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, w, cx| {
                            // 过期的 paint 不得清掉或覆盖后继 render 的布局或显示回执。
                            if !visual.borrow().epoch_is_current(&paint.epoch) {
                                return;
                            }
                            let current = owner.upgrade().is_some_and(|owner| {
                                let v = owner.read(cx);
                                v.pin_local_action_ready(w, cx)
                                    && v.pin_local_ocr
                                        .as_ref()
                                        .and_then(|s| s.panel.as_ref())
                                        .is_some_and(|p| {
                                            Arc::ptr_eq(&p.identity, &paint_frame)
                                                && paint.same_body(&p.editor)
                                        })
                            });
                            let Some(scroll_paint) =
                                paint.publish(&visual, current, &scroll, w, |rows, layout| {
                                    rows.and_then(|rows| candidate_painted_rows(rows, layout))
                                        .map(Arc::new)
                                })
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
                                if had_drag {
                                    let owner = owner.clone();
                                    let frame = Arc::clone(&paint_frame);
                                    // paint 中不重入 Pin 的可变状态；当前 effect 结束即取消。
                                    cx.defer(move |cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.stop_pin_candidate_scrolling(&frame);
                                            cx.notify();
                                        });
                                    });
                                }
                            } else if (scrolling.drag.is_some() || scrolling.reveal)
                                && scrolling.scheduled.is_none()
                            {
                                // 从真正完成的本次 paint 续约一次；回调不串接旧布局。
                                let token = Arc::new(());
                                scrolling.scheduled = Some(Arc::clone(&token));
                                let owner = owner.clone();
                                let frame = Arc::clone(&paint_frame);
                                w.on_next_frame(move |w, cx| {
                                    let _ = owner.update(cx, |view, cx| {
                                        view.pin_candidate_scroll_tick(&frame, &token, w, cx);
                                    });
                                });
                            }
                            if current {
                                if let Some(command) =
                                    scrolling.command.as_mut().filter(|c| !c.scheduled)
                                {
                                    command.scheduled = true;
                                    let token = Arc::clone(&command.token);
                                    let frame = Arc::clone(&paint_frame);
                                    let owner = owner.clone();
                                    // 只从 release 后真正 current 的 paint 续约按钮命令。
                                    w.on_next_frame(move |w, cx| {
                                        let _ = owner.update(cx, |view, cx| {
                                            view.run_queued_pin_candidate_command(
                                                &frame, &token, w, cx,
                                            );
                                        });
                                    });
                                }
                            }
                            drop(scrolling);
                            let move_owner = owner.clone();
                            let move_frame = Arc::clone(&paint_frame);
                            w.on_mouse_event(move |e: &MouseMoveEvent, phase, w, cx| {
                                if phase == gpui::DispatchPhase::Capture {
                                    let _ = move_owner.update(cx, |view, cx| {
                                        if view.pin_candidate_drag_move(&move_frame, e, w, cx) {
                                            cx.stop_propagation();
                                        }
                                    });
                                }
                            });
                            let up_owner = owner.clone();
                            let up_frame = Arc::clone(&paint_frame);
                            w.on_mouse_event(move |e: &MouseUpEvent, phase, w, cx| {
                                if phase == gpui::DispatchPhase::Capture {
                                    let _ = up_owner.update(cx, |view, cx| {
                                        if view.pin_candidate_drag_up(&up_frame, e, w, cx) {
                                            cx.stop_propagation();
                                        }
                                    });
                                }
                            });
                            let down_owner = owner.clone();
                            let down_frame = Arc::clone(&paint_frame);
                            w.on_mouse_event(move |e: &MouseDownEvent, phase, _, cx| {
                                if phase == gpui::DispatchPhase::Capture
                                    && e.button != MouseButton::Left
                                {
                                    let _ = down_owner.update(cx, |view, cx| {
                                        let owned = view
                                            .pin_local_ocr
                                            .as_ref()
                                            .and_then(|s| s.panel.as_ref())
                                            .filter(|p| Arc::ptr_eq(&p.identity, &down_frame))
                                            .is_some_and(|p| p.scrolling.borrow().pressed);
                                        if owned {
                                            view.stop_pin_candidate_scrolling(&down_frame);
                                            cx.notify();
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
        let button_stamp = state.panel.as_ref().map(|panel| {
            panel.button_stamp(
                &state.fixed.identity,
                state.binding.source,
                state.binding.result_generation,
            )
        });
        let retry_stamp = state
            .panel
            .is_none()
            .then(|| {
                state
                    .retry
                    .target(&state.binding, self.pin_local_body().unwrap_or(""), true)
                    .and_then(|target| {
                        self.pin_retry_render_stamp(
                            target.selected(),
                            target,
                            RetryClickOrigin::WithoutCandidate,
                        )
                    })
            })
            .flatten();
        let retry_error =
            (state.panel.is_none() && retry_stamp.is_none()).then(|| RetryErrorStamp {
                entry_epoch: Arc::clone(&state.retry_epoch),
                source_identity: Arc::clone(&state.fixed.identity),
                binding: state.binding.clone(),
                selected: self.selected.clone(),
                visible: state.visible,
                error: self
                    .selected
                    .as_ref()
                    .filter(|range| !range.is_empty())
                    .map_or(TargetError::InvalidSelection, |selected| {
                        state
                            .plan
                            .as_ref()
                            .filter(|(range, _)| range == selected)
                            .and_then(|(_, target)| target.as_ref().err().copied())
                            .unwrap_or(TargetError::Inactive)
                    }),
            });
        let mut buttons = div().flex().flex_wrap().gap_1();
        for (id, label, command, enabled) in [
            (
                "pin-local-copy",
                tr!("复制候选", "Copy candidate"),
                0,
                can_copy,
            ),
            ("pin-local-retry", tr!("重试此行", "Retry line"), 1, true),
            ("pin-local-discard", tr!("丢弃候选", "Discard"), 2, true),
        ] {
            let button_id = if command == 1 && state.panel.is_none() {
                gpui::ElementId::from((id, Arc::as_ptr(&state.retry_epoch) as usize))
            } else {
                gpui::ElementId::from(id)
            };
            let mut button = div()
                .id(button_id)
                .px_2()
                .py_1()
                .rounded_sm()
                .text_color(rgb(if enabled { p.foreground } else { p.muted }))
                .child(label);
            if enabled {
                let button_stamp = button_stamp.clone();
                let retry_stamp = retry_stamp.clone();
                let retry_error = retry_error.clone();
                button = button
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(p.hover)))
                    .on_click(cx.listener(move |v, _, w, cx| match command {
                        0 => {
                            if let Some(stamp) = button_stamp.as_ref() {
                                v.queue_pin_candidate_command(
                                    CandidateCommandKind::Copy,
                                    stamp,
                                    w,
                                    cx,
                                );
                            }
                        }
                        1 => {
                            if let Some(stamp) = button_stamp.as_ref() {
                                v.queue_pin_candidate_command(
                                    CandidateCommandKind::Retry,
                                    stamp,
                                    w,
                                    cx,
                                );
                            } else if let Some(stamp) = retry_stamp.as_ref() {
                                v.queue_pin_retry_click(stamp, w, cx);
                            } else if let Some(stamp) = retry_error.as_ref() {
                                v.report_pin_retry_error(stamp, w, cx);
                            }
                        }
                        _ => {
                            let focused = v.pin_local_candidate_focused(w);
                            v.cancel_pin_local_intent("显式丢弃候选");
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

    pub(super) fn render_pin_local_ocr(
        &self,
        w: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.text_mode || self.text_source != ocr_clipboard::TextSource::Ocr {
            return None;
        }
        if let Some(candidate) = self.render_pin_local_candidate(w, cx) {
            return Some(candidate);
        }
        let selected = self.selected.as_ref().filter(|range| !range.is_empty())?;
        let stamp = self.pin_local_ocr.as_ref().and_then(|state| {
            state
                .plan
                .as_ref()
                .and_then(|(range, target)| {
                    (range == selected).then(|| target.as_ref().ok()).flatten()
                })
                .and_then(|target| {
                    self.pin_retry_render_stamp(selected.clone(), target, RetryClickOrigin::Initial)
                })
        });
        // 鼠标按住导致 GPUI refresh 时，来源/行资格仍保留；仅执行门要求 released idle。
        let enabled = self.pin_local_input_current(cx)
            && self.text_show_selection
            && self.text_selection.is_some()
            && stamp.is_some();
        let entry_epoch = self
            .pin_local_ocr
            .as_ref()
            .map(|state| Arc::clone(&state.retry_epoch))?;
        let p = crate::theme::palette(cx);
        let bounds = self.pin_local_chrome_bounds(w);
        let label = tr!("重识别此行", "Retry this line");
        let hint = if enabled {
            tr!(
                "独立重识别候选，原文与标记保留",
                "Retry as a candidate; keep original text and marks"
            )
        } else if let Some(error) = self
            .pin_local_ocr
            .as_ref()
            .and_then(|state| state.plan.as_ref())
            .and_then(|(_, target)| target.as_ref().err())
        {
            target_error(*error)
        } else {
            tr!(
                "无法单独定位这一行，请重新识别后再试",
                "Can't locate this line on its own; recognize again and retry"
            )
        };
        Some(
            div()
                .id(("pin-local-start", Arc::as_ptr(&entry_epoch) as usize))
                .absolute()
                .left(px(bounds.x + (bounds.width - 106.).max(0.)))
                .top(px(bounds.y + (bounds.height - 36.).max(0.)))
                .max_w(px(bounds.width))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(p.background))
                .border_1()
                .border_color(rgb(p.border))
                .text_color(rgb(if enabled { p.foreground } else { p.muted }))
                .text_size(px(11.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(cx.listener(|v, _, _, cx| {
                    if let Some(state) = &mut v.pin_local_ocr {
                        state.cancel_retry_click();
                    }
                    cx.notify();
                    cx.stop_propagation();
                }))
                .tooltip(move |_, cx| cx.new(move |_| TextToolHint(hint)).into())
                .when(enabled, |button| {
                    button.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
                })
                .child(div().truncate().child(label))
                .on_click(cx.listener(move |v, _, w, cx| {
                    // disabled listener 也持有该代，防取消后地址复用旧 pending_mouse_down。
                    if enabled
                        && v.pin_local_ocr
                            .as_ref()
                            .is_some_and(|state| Arc::ptr_eq(&state.retry_epoch, &entry_epoch))
                    {
                        if let Some(stamp) = stamp.as_ref() {
                            v.queue_pin_retry_click(stamp, w, cx);
                        }
                    }
                    cx.stop_propagation();
                }))
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
            "正文已改变，请重新识别全文",
            "Text changed; recognize the full result again"
        ),
        TargetError::Budget => tr!("此行超出重识别上限", "Line exceeds the retry limit"),
        _ => tr!(
            "无法单独定位这一行，请只选同一行的文字",
            "Can't locate this line on its own; select text within a single line"
        ),
    }
}

#[cfg(test)]
#[path = "pin_capture_ocr_input_tests.rs"]
mod capture_source_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_view::text_result::ResultEditor;
    use std::{cell::Cell, io};

    fn stamp(crop: Option<crate::geometry::Rect>) -> TextSelectionStamp {
        (11, None, None, crop, true)
    }
    fn image() -> Image {
        Image {
            width: 2,
            height: 1,
            bgra: vec![0, 1, 2, 3, 252, 253, 254, 255],
            readback_ms: 1.25,
        }
    }
    fn result(text: &str) -> Arc<RecognizedText> {
        Arc::new(RecognizedText {
            text: text.into(),
            words: Vec::new(),
            lines: Vec::new(),
            regions: Vec::new(),
        })
    }
    fn binding(
        result: Arc<RecognizedText>,
        source: TextSelectionStamp,
    ) -> Binding<TextSelectionStamp> {
        let body_identity = Arc::new(());
        Binding {
            source,
            source_identity: Arc::new(()),
            result,
            result_generation: 7,
            installed_body_identity: Arc::clone(&body_identity),
            body_identity,
            installed_body_revision: 0,
            body_revision: 0,
            frame: SourceFrame {
                width: 2,
                height: 1,
                origin: (0, 0),
            },
            provider: OcrProvider::Ppocr,
        }
    }

    #[test]
    fn actual_input_copy_binds_crop_provider_and_has_fresh_identity() {
        let source = stamp(Some(crate::geometry::Rect {
            x: 3,
            y: 4,
            width: 2,
            height: 1,
        }));
        let mut input = image();
        let fixed = FixedInput::copy_of(&input, source, (10, 10), OcrProvider::Ppocr).unwrap();
        let next = FixedInput::copy_of(&input, source, (10, 10), OcrProvider::Ppocr).unwrap();
        assert_eq!(fixed.image.bgra, input.bgra);
        assert_ne!(fixed.image.bgra.as_ptr(), input.bgra.as_ptr());
        assert_eq!(fixed.source, source);
        assert_eq!(fixed.provider, OcrProvider::Ppocr);
        assert_eq!(fixed.frame.origin, (3, 4));
        assert!(!Arc::ptr_eq(&fixed.identity, &next.identity));
        input.bgra.fill(99);
        assert_eq!(fixed.image.bgra, [0, 1, 2, 3, 252, 253, 254, 255]);
    }

    #[test]
    fn actual_input_rejects_budget_crop_and_unbacked_provider() {
        let mut input = image();
        assert!(FixedInput::copy_of(&input, stamp(None), (1, 2), OcrProvider::Ppocr).is_none());
        assert!(FixedInput::copy_of(&input, stamp(None), (2, 1), OcrProvider::Windows).is_none());
        let mut hidden = stamp(None);
        hidden.4 = false;
        assert!(FixedInput::copy_of(&input, hidden, (2, 1), OcrProvider::Ppocr).is_none());
        input.bgra.pop();
        assert!(FixedInput::copy_of(&input, stamp(None), (2, 1), OcrProvider::Ppocr).is_none());
        let huge = Image {
            width: 8192,
            height: 8192,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        assert!(
            FixedInput::copy_of(&huge, stamp(None), (8192, 8192), OcrProvider::Ppocr).is_none()
        );
    }

    #[test]
    fn current_dto_arc_and_source_cannot_be_replaced_by_equal_bytes() {
        let dto = result(" 同一正文 e\u{301}\r\n ");
        let source = stamp(None);
        let installed = binding(Arc::clone(&dto), source);
        assert!(result_current(&installed, Some(&dto), source, 7));
        assert!(!result_current(
            &installed,
            Some(&result(&dto.text)),
            source,
            7
        ));
        assert!(!result_current(&installed, Some(&dto), source, 8));
        let mut changed = source;
        changed.0 += 1;
        assert!(!result_current(&installed, Some(&dto), changed, 7));
        assert!(!result_current(&installed, None, source, 7));
    }

    #[test]
    fn readonly_candidate_real_editor_keeps_original_bytes_and_body_identity() {
        let original = " 候选 e\u{301}\r\n尾行 ";
        let mut editor = ResultEditor::new(original.into(), 0..0);
        let identity = Arc::clone(&editor.body_identity);
        editor.readonly_begin_drag(original.len());
        editor.readonly_extend_drag(0);
        editor.end_drag();
        assert!(editor.input.reversed);
        assert_eq!(editor.selected_or_full().as_bytes(), original.as_bytes());
        editor.readonly_navigate("right", true, true);
        assert_eq!(editor.text.as_bytes(), original.as_bytes());
        assert!(Arc::ptr_eq(&identity, &editor.body_identity));
        assert_eq!(editor.revision, 0);
    }

    #[test]
    fn candidate_copy_error_keeps_real_selection_for_verbatim_retry() {
        let original = "正文\r\n \u{3000} ";
        let mut editor = ResultEditor::new(original.into(), 0..0);
        let start = "正文".len();
        editor.readonly_begin_drag(original.len());
        editor.readonly_extend_drag(start);
        editor.end_drag();
        let selection = editor.input.selection.clone();
        let calls = Cell::new(0);
        let payload = candidate::candidate_payload(&editor.text, editor.input.selection.clone());
        let error =
            ocr_clipboard::publish_text(payload, ocr_clipboard::Content::Selection, |text| {
                calls.set(calls.get() + 1);
                assert_eq!(text, "\r\n \u{3000} ");
                Err(io::Error::from_raw_os_error(5))
            })
            .expect_err("writer 失败不能清候选");
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(editor.input.selection, selection);
        assert_eq!(editor.text, original);
        assert!(ocr_clipboard::publish_text(
            candidate::candidate_payload(&editor.text, editor.input.selection.clone()),
            ocr_clipboard::Content::Selection,
            |text| {
                calls.set(calls.get() + 1);
                assert_eq!(text, payload);
                Ok::<(), io::Error>(())
            }
        )
        .unwrap());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn pin_readonly_candidate_reverse_words_grapheme_hits_keep_original_binding() {
        let original = result("原 OCR DTO e\u{301}\r\n尾行");
        let source = stamp(None);
        let installed = binding(Arc::clone(&original), source);
        let retained = installed.clone();
        let candidate = "中e\u{301}👩‍💻🇨🇳👍🏽\r\nalpha, 中文。 beta";
        let mut panel = CandidatePanel {
            identity: Arc::new(()),
            editor: ResultEditor::new(candidate.into(), 0..0),
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
        let emoji_start = "中e\u{301}".len();
        let flag_start = emoji_start + "👩‍💻".len();
        assert!(editor.readonly_begin_drag(emoji_start + "👩‍".len()));
        assert_eq!(editor.caret(), emoji_start);
        assert!(editor.readonly_extend_drag(flag_start + "🇨".len()));
        assert_eq!(editor.input.selection, emoji_start..flag_start);
        assert_eq!(editor.selected_or_full(), "👩‍💻");
        editor.end_drag();
        editor.readonly_begin_drag(flag_start);
        editor.readonly_extend_drag(emoji_start);
        editor.end_drag();
        assert!(editor.input.reversed);
        assert!(editor.readonly_navigate("left", true, false));
        assert_eq!(editor.selected_or_full(), "e\u{301}👩‍💻");
        assert!(editor.input.reversed);
        assert!(editor.readonly_navigate("right", true, false));
        assert_eq!(editor.selected_or_full(), "👩‍💻");
        assert!(editor.readonly_navigate("left", false, true));
        assert_eq!(editor.input.selection, emoji_start..emoji_start);
        let line_start = "中e\u{301}👩‍💻🇨🇳👍🏽\r\n".len();
        editor.readonly_begin_drag(candidate.len());
        editor.end_drag();
        assert!(editor.readonly_navigate("left", false, true));
        assert_eq!(editor.caret(), candidate.len() - "beta".len());
        assert!(editor.readonly_navigate("left", true, true));
        assert_eq!(editor.selected_or_full(), "。 ");
        assert!(editor.input.reversed);
        assert!(editor.readonly_navigate("left", true, true));
        assert_eq!(editor.selected_or_full(), "文。 ");
        assert!(editor.readonly_navigate("left", true, true));
        assert_eq!(editor.selected_or_full(), "中文。 ");
        assert!(editor.readonly_navigate("home", false, false));
        assert_eq!(editor.caret(), line_start);
        assert!(editor.readonly_navigate("left", true, false));
        assert_eq!(editor.selected_or_full(), "\r\n");
        assert!(editor.input.reversed);
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
        assert!(result_current(&installed, Some(&original), source, 7));
        assert_eq!(
            installed.result.text.as_bytes(),
            "原 OCR DTO e\u{301}\r\n尾行".as_bytes()
        );
        assert_eq!(installed.source, retained.source);
        assert_eq!(installed.frame, retained.frame);
        assert_eq!(installed.provider, retained.provider);
        assert_eq!(installed.result_generation, retained.result_generation);
        assert_eq!(installed.body_revision, retained.body_revision);
        assert_eq!(
            installed.installed_body_revision,
            retained.installed_body_revision
        );
        assert!(Arc::ptr_eq(&installed.result, &retained.result));
        assert!(Arc::ptr_eq(
            &installed.source_identity,
            &retained.source_identity
        ));
        assert!(Arc::ptr_eq(
            &installed.body_identity,
            &retained.body_identity
        ));
        assert!(Arc::ptr_eq(
            &installed.installed_body_identity,
            &retained.installed_body_identity
        ));
    }

    mod profile_request {
        //! 加入 pin_ocr_local_retry 的现有 tests 子模块，复用 image/stamp/result/binding。
        //! 仅定义固定 profile 与真实身份／Receipt 的组合合同，不运行 OCR worker。
        //! LocalRetry 核心不携带 profile；不把这些断言冒称为 Tiny fallback 调用验证。
        use super::*;
        use crate::{
            annotation::Point,
            capture_view::ocr_task::Receipt,
            geometry::Rect,
            ppocr::OcrProfile,
            preferences::Preferences,
            recognition::{HorizontalDirection, RecognizedLine, RecognizedWord, TextGeometry},
        };

        fn profile_line_result() -> Arc<RecognizedText> {
            let original = result("中");
            let rect = Rect::full(2, 1);
            let geometry = Some(TextGeometry {
                quad: [
                    Point { x: 0., y: 0. },
                    Point { x: 2., y: 0. },
                    Point { x: 2., y: 1. },
                    Point { x: 0., y: 1. },
                ],
            });
            Arc::new(RecognizedText {
                text: original.text.clone(),
                regions: vec![rect],
                lines: vec![RecognizedLine {
                    text: original.text.clone(),
                    rect: Some(rect),
                    horizontal_direction: HorizontalDirection::LeftToRight,
                    geometry,
                    graphemes: None,
                }],
                words: vec![RecognizedWord {
                    text: original.text.clone(),
                    rect,
                    line: 0,
                    horizontal_direction: HorizontalDirection::LeftToRight,
                    geometry,
                }],
            })
        }

        #[test]
        fn small_fixed_input_keeps_request_profile_and_installed_dto_identity() {
            let input = image();
            let source = stamp(Some(Rect {
                x: 3,
                y: 4,
                width: 2,
                height: 1,
            }));
            let mut preferences = Preferences {
                ocr_profile: OcrProfile::Small,
                ..Preferences::default()
            };
            let fixed = FixedInput::copy_with_profile(
                &input,
                source,
                (10, 10),
                OcrProvider::Ppocr,
                preferences.ocr_profile,
            )
            .unwrap();
            let dto = result(" 同批 Small 正文 e\u{301}\r\n ");
            let mut installed = binding(Arc::clone(&dto), source);
            installed.source_identity = Arc::clone(&fixed.identity);
            installed.frame = fixed.frame;
            let fixed_identity = Arc::clone(&fixed.identity);
            let body_identity = Arc::clone(&installed.body_identity);

            // 后继设置仅供后继请求读取；不写设置文件，也不重读正在使用的固定输入。
            preferences.ocr_profile = OcrProfile::Tiny;
            let successor = FixedInput::copy_with_profile(
                &input,
                source,
                (10, 10),
                OcrProvider::Ppocr,
                preferences.ocr_profile,
            )
            .unwrap();
            assert_eq!(fixed.profile, OcrProfile::Small);
            assert_eq!(successor.profile, OcrProfile::Tiny);
            assert!(Arc::ptr_eq(&fixed.identity, &fixed_identity));
            assert!(Arc::ptr_eq(&installed.source_identity, &fixed.identity));
            assert!(!Arc::ptr_eq(
                &installed.source_identity,
                &successor.identity
            ));
            assert_eq!(fixed.source, installed.source);
            assert_eq!(fixed.frame, installed.frame);
            assert!(Arc::ptr_eq(&installed.result, &dto));
            assert!(Arc::ptr_eq(&installed.body_identity, &body_identity));
            assert!(Arc::ptr_eq(
                &installed.installed_body_identity,
                &body_identity
            ));
            assert!(result_current(&installed, Some(&dto), source, 7));
            let equal_bytes_new_dto = result(&dto.text);
            assert!(!result_current(
                &installed,
                Some(&equal_bytes_new_dto),
                source,
                7,
            ));
            let mut changed_source = source;
            changed_source.0 += 1;
            assert!(!result_current(&installed, Some(&dto), changed_source, 7));
            assert_eq!(installed.result.text.as_bytes(), dto.text.as_bytes());
        }

        #[test]
        fn small_failed_or_cancelled_receipt_keeps_fixed_profile_and_original_binding() {
            for (receipt, expected) in [
                (
                    Receipt::<RecognizedText>::Failed("固定模型错误回执（模型输入）".into()),
                    Completion::Failed,
                ),
                (Receipt::Cancelled, Completion::Cancelled),
            ] {
                let input = image();
                let source = stamp(None);
                let fixed = FixedInput::copy_with_profile(
                    &input,
                    source,
                    (2, 1),
                    OcrProvider::Ppocr,
                    OcrProfile::Small,
                )
                .unwrap();
                let dto = profile_line_result();
                let mut installed = binding(Arc::clone(&dto), source);
                installed.source_identity = Arc::clone(&fixed.identity);
                installed.frame = fixed.frame;
                let original = installed.clone();
                let mut retry = LocalRetry::default();
                let request = retry
                    .begin(&installed, &dto.text, 0..dto.text.len(), 0, true)
                    .unwrap();
                // ROI 来自真实 DTO 行证据，采样仍使用该 Small 请求的固定输入。
                let roi = request.target().extract_pixels(&fixed.image).unwrap();
                assert_eq!((roi.width, roi.height), (2, 1));
                assert!(retry.pending(&installed, &dto.text, true));
                assert_eq!(
                    retry.complete(&request, &installed, &dto.text, true, receipt),
                    expected
                );
                assert!(retry.candidate(&installed, &dto.text, true).is_none());
                assert!(!retry.pending(&installed, &dto.text, true));
                assert!(retry.pending_visual().is_none());
                assert_eq!(fixed.profile, OcrProfile::Small);
                assert_eq!(fixed.source, original.source);
                assert_eq!(fixed.frame, original.frame);
                assert_eq!(fixed.provider, original.provider);
                assert!(Arc::ptr_eq(&fixed.identity, &original.source_identity));
                assert!(Arc::ptr_eq(
                    &installed.source_identity,
                    &original.source_identity
                ));
                assert!(Arc::ptr_eq(&installed.result, &original.result));
                assert!(Arc::ptr_eq(
                    &installed.body_identity,
                    &original.body_identity
                ));
                assert!(Arc::ptr_eq(
                    &installed.installed_body_identity,
                    &original.installed_body_identity,
                ));
                assert_eq!(installed.body_revision, original.body_revision);
                assert_eq!(
                    installed.installed_body_revision,
                    original.installed_body_revision
                );
                assert_eq!(installed.result.text.as_bytes(), "中".as_bytes());
                assert!(result_current(&installed, Some(&dto), source, 7));
            }
        }
    }
}
