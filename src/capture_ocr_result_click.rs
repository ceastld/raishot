//! 全文／缺几何结果的独立 Down 意图与实际 StyledText 布局租约；不要求原位 Overlay。
use crate::{
    capture_ocr_overlay::inline_click::{ClickSlot, DownProof},
    capture_view::{
        text_result::{OcrTarget, ResultFrame},
        CaptureView,
    },
    ocr_projection::OcrProjection,
};
use gpui::{
    prelude::*, Bounds, ClickEvent, Context, DispatchPhase, Entity, EntityInputHandler,
    FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, ScrollWheelEvent, Subscription,
    UTF16Selection, WeakEntity, Window,
};
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};
use windows::Win32::UI::Input::KeyboardAndMouse::GetCapture;

#[derive(Clone, PartialEq)]
pub(crate) struct ResultSourceIdentity {
    // QR／控件纯文本结果没有 OCR origin，仍保存真实画布／Document 身份。
    target: Option<OcrTarget>,
    canvas: crate::geometry::ScreenRect,
    revisions: (u64, u64),
    stamp: (u64, u64),
    identity: crate::scene::SourceIdentity,
}

#[cfg(test)]
mod key_lineage_tests {
    use super::*;
    use crate::capture_view::text_result::ResultEditor;

    type Lease = ResultLease<u64, [u32; 3]>;
    type Shared = Rc<RefCell<WriterLease<u64, [u32; 3], crate::text_input::TextInputState>>>;

    fn source(editor: &ResultEditor, domain: &Lease) -> Lease {
        let mut source = domain.clone();
        source.frame.revision = Some(editor.revision);
        source.body = Arc::clone(&editor.body_identity);
        source.text = Arc::from(editor.text.as_str());
        source
    }
    fn fixture(text: &str, selection: Range<usize>) -> (ResultEditor, Lease, Shared) {
        let editor = ResultEditor::new(text.into(), selection);
        let domain = Lease {
            frame: ResultFrame {
                generation: 7,
                revision: Some(editor.revision),
            },
            body: Arc::clone(&editor.body_identity),
            source: 7,
            text: Arc::from(editor.text.as_str()),
            epoch: Arc::new(()),
            projection: [125, 800, 600],
        };
        let writer = Rc::new(RefCell::new(WriterLease::new(
            domain.clone(),
            editor.input.clone(),
            Arc::new(()),
        )));
        (editor, domain, writer)
    }
    fn authorized(
        editor: &mut ResultEditor,
        domain: &Lease,
        writer: &Shared,
        registered: &Option<Shared>,
        command: impl FnOnce(&mut ResultEditor),
    ) -> bool {
        if !same_writer_registration(registered, writer) {
            return false;
        }
        let before = source(editor, domain);
        let input_before = editor.input.clone();
        if !writer
            .borrow()
            .text_current(&before, &input_before, &writer.borrow().layout_token)
        {
            return false;
        }
        command(editor);
        writer.borrow_mut().advance_owned(
            &before,
            &input_before,
            source(editor, domain),
            editor.input.clone(),
        )
    }

    #[test]
    fn actual_ctrl_a_then_native_character_without_paint_uses_shared_selection() {
        let (mut editor, domain, writer) = fixture("中文😀", 0..0);
        let proxy = writer.clone();
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.select_all()
        ));
        assert!(proxy.borrow().text_current(
            &source(&editor, &domain),
            &editor.input,
            &proxy.borrow().layout_token
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &proxy,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "新");
        assert!(!writer.borrow().geometry_current());
    }

    #[test]
    fn actual_left_then_character_without_paint_preserves_unicode_caret() {
        let (mut editor, domain, writer) = fixture("a😀b", 6..6);
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.navigate("left", false, false)
        ));
        assert_eq!(editor.caret(), 5);
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "中", false, None);
            }
        ));
        assert_eq!(editor.text, "a😀中b");
    }

    #[test]
    fn actual_failed_cut_preserves_selection_and_next_character_without_paint() {
        let (mut editor, domain, writer) = fixture("中文😀", 0..10);
        let registered = Some(writer.clone());
        let before = editor.input.clone();
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                assert_eq!(
                    editor
                        .publish_text_copy(true, |_| Err("占用".into()))
                        .unwrap_err()
                        .to_string(),
                    "占用"
                );
            }
        ));
        assert_eq!(editor.text, "中文😀");
        assert_eq!(editor.input, before);
        assert!(writer.borrow().geometry_current());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "新");
    }

    #[test]
    fn actual_successful_cut_then_character_without_paint_keeps_publication_order() {
        let (mut editor, domain, writer) = fixture("中文😀", 0..10);
        let registered = Some(writer.clone());
        let mut copied = String::new();
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                assert!(editor
                    .publish_text_copy(true, |text| {
                        copied = text.into();
                        Ok(())
                    })
                    .unwrap());
            }
        ));
        assert_eq!(copied, "中文😀");
        assert_eq!(editor.text, "");
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "新");
    }

    #[test]
    fn actual_paste_enter_delete_and_character_share_one_no_paint_owner() {
        let (mut editor, domain, writer) = fixture("甲乙", 3..3);
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "😀", false, None);
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "\n", false, None);
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "x", false, None);
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.delete(true)
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "甲😀\n新乙");
        assert!(!writer.borrow().geometry_current());
    }

    #[test]
    fn actual_authorized_undo_then_character_advances_but_external_undo_aba_rejects() {
        let (mut editor, domain, writer) = fixture("abc", 0..0);
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "X", false, None);
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.undo()
        ));
        assert_eq!(editor.text, "abc");
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "Y", false, None);
            }
        ));
        assert_eq!(editor.text, "Yabc");
        editor.undo();
        editor.redo();
        assert_eq!(editor.text, "Yabc");
        assert!(!authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "bad", false, None);
            }
        ));
        assert_eq!(editor.text, "Yabc");
    }

    #[test]
    fn old_proxy_is_rejected_when_new_registered_writer_reuses_same_layout_token() {
        let (mut editor, domain, old) = fixture("中文", 0..0);
        let new = Rc::new(RefCell::new(WriterLease::new(
            domain.clone(),
            editor.input.clone(),
            old.borrow().layout_token.clone(),
        )));
        let registered = Some(new.clone());
        assert!(!same_writer_registration(&registered, &old));
        assert!(same_writer_registration(&registered, &new));
        assert!(!authorized(
            &mut editor,
            &domain,
            &old,
            &registered,
            |editor| {
                editor.replace(None, "旧", false, None);
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &new,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "新中文");
    }

    #[test]
    fn stale_key_ticket_cannot_adopt_intervening_native_writer_change() {
        let (mut editor, domain, writer) = fixture("中文", 0..0);
        let registered = Some(writer.clone());
        let before = source(&editor, &domain);
        let input_before = editor.input.clone();
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert!(!writer.borrow_mut().advance_owned(
            &before,
            &input_before,
            source(&editor, &domain),
            editor.input.clone()
        ));
        assert!(!writer.borrow().valid);
    }

    #[test]
    fn actual_pointer_caret_and_up_then_character_continue_without_paint() {
        let (mut editor, domain, writer) = fixture("甲乙", 0..0);
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.begin_drag(3)
        ));
        assert!(writer.borrow().geometry_current());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.end_drag();
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "甲新乙");
    }

    #[test]
    fn actual_drag_extend_and_up_use_same_writer_and_external_selection_rejects() {
        let (mut editor, domain, writer) = fixture("甲😀乙", 0..0);
        let registered = Some(writer.clone());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.begin_drag(3)
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| editor.extend_drag(7)
        ));
        assert!(writer.borrow().geometry_current());
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.extend_drag(7);
                editor.end_drag();
            }
        ));
        assert!(authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "新", false, None);
            }
        ));
        assert_eq!(editor.text, "甲新乙");
        editor.select_all();
        assert!(!authorized(
            &mut editor,
            &domain,
            &writer,
            &registered,
            |editor| {
                editor.replace(None, "拒绝", false, None);
            }
        ));
        assert_eq!(editor.text, "甲新乙");
    }
}

#[derive(Clone)]
pub(crate) struct ResultLease<S, P> {
    frame: ResultFrame,
    body: Arc<()>,
    source: S,
    text: Arc<str>,
    epoch: Arc<()>,
    projection: P,
}

impl<S: PartialEq, P: PartialEq> PartialEq for ResultLease<S, P> {
    fn eq(&self, other: &Self) -> bool {
        self.current(
            other.frame,
            &other.body,
            &other.source,
            &other.text,
            &other.epoch,
            &other.projection,
        )
    }
}

impl<S: PartialEq, P: PartialEq> ResultLease<S, P> {
    fn current(
        &self,
        frame: ResultFrame,
        body: &Arc<()>,
        source: &S,
        text: &str,
        epoch: &Arc<()>,
        projection: &P,
    ) -> bool {
        self.frame == frame
            && Arc::ptr_eq(&self.body, body)
            && self.source == *source
            && self.text.as_ref() == text
            && Arc::ptr_eq(&self.epoch, epoch)
            && self.projection == *projection
    }
}

pub(crate) type ResultBodySource = ResultLease<ResultSourceIdentity, OcrProjection>;

/// 只有本代理实际获准执行的写入才能推进正文；paint租约始终停留在原真实帧。
struct WriterLease<S, P, I> {
    painted: ResultLease<S, P>,
    expected: ResultLease<S, P>,
    input: I,
    layout_token: Arc<()>,
    valid: bool,
}

impl<S: Clone + PartialEq, P: Clone + PartialEq, I: Clone + PartialEq> WriterLease<S, P, I> {
    fn new(source: ResultLease<S, P>, input: I, layout_token: Arc<()>) -> Self {
        Self {
            painted: source.clone(),
            expected: source,
            input,
            layout_token,
            valid: true,
        }
    }

    fn text_current(&self, source: &ResultLease<S, P>, input: &I, token: &Arc<()>) -> bool {
        self.valid
            && self.expected == *source
            && self.input == *input
            && Arc::ptr_eq(&self.layout_token, token)
    }

    fn geometry_current(&self) -> bool {
        self.valid && self.expected == self.painted
    }

    fn advance_owned(
        &mut self,
        before: &ResultLease<S, P>,
        input_before: &I,
        after: ResultLease<S, P>,
        input_after: I,
    ) -> bool {
        let same_domain = self.valid
            && self.expected == *before
            && self.input == *input_before
            && before.frame.generation == after.frame.generation
            && before.frame.revision.is_some()
            && after.frame.revision.is_some()
            && before.source == after.source
            && before.projection == after.projection
            && Arc::ptr_eq(&before.epoch, &after.epoch);
        if !same_domain {
            self.valid = false;
            return false;
        }
        self.expected = after;
        self.input = input_after;
        true
    }
}

impl ResultBodySource {
    pub(crate) fn current_in(&self, view: &CaptureView, window: &Window) -> bool {
        view.inline_ocr_host()
            && view.recognition_result.is_some()
            && (view.ocr_overlay.is_none() || view.result_editor.is_some())
            && view.ocr_projection(window).is_some_and(|projection| {
                self.current(
                    view.result_output_frame(),
                    &view.current_result_body_identity(),
                    &view.result_source_identity(),
                    view.current_result_text().unwrap_or_default(),
                    &view.ocr_projection_epoch,
                    &projection,
                )
            })
    }
}

#[derive(Clone)]
pub(crate) struct BodyPaintLease<S, P, B> {
    source: ResultLease<S, P>,
    layout_token: Arc<()>,
    bounds: B,
    editor_scroll: [f32; 2],
    writer: Option<Rc<RefCell<ActiveResultWriter>>>,
}

struct ActiveResultWriter {
    owner: WeakEntity<CaptureView>,
    focus: FocusHandle,
    lease: WriterLease<ResultSourceIdentity, OcrProjection, crate::text_input::TextInputState>,
}

fn same_writer_registration<T>(
    registered: &Option<Rc<RefCell<T>>>,
    reader: &Rc<RefCell<T>>,
) -> bool {
    registered
        .as_ref()
        .is_some_and(|current| Rc::ptr_eq(current, reader))
}

/// 仅由同 Owner 的真实按键入口签发；消费前后的 expected 不能由新状态重采。
pub(crate) struct ResultKeyCommand {
    writer: Rc<RefCell<ActiveResultWriter>>,
    before: ResultBodySource,
    input_before: crate::text_input::TextInputState,
    pointer: bool,
}

pub(crate) type ResultBodyPaint =
    BodyPaintLease<ResultSourceIdentity, OcrProjection, Bounds<Pixels>>;

/// 旧 prepaint／paint 不得清理或覆盖新 layout。完整来源核验由调用者当前 UI 状态提供。
fn publish_body_paint<T>(slot: &RefCell<Option<T>>, paint: T, source_current: bool) -> bool {
    if !source_current {
        return false;
    }
    *slot.borrow_mut() = Some(paint);
    true
}

fn revoke_layout<S, P, B>(
    slot: &RefCell<Option<BodyPaintLease<S, P, B>>>,
    token: &Arc<()>,
) -> bool {
    if slot
        .borrow()
        .as_ref()
        .is_some_and(|paint| Arc::ptr_eq(&paint.layout_token, token))
    {
        slot.borrow_mut().take();
        return true;
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultIntent {
    Copy,
    Close,
    Translate,
    Retry,
    Edit(bool),
}

#[derive(Clone, PartialEq)]
struct ResultClickStamp {
    source: ResultBodySource,
    intent: ResultIntent,
    selection: Range<usize>,
    bounds: Bounds<Pixels>,
    editor_scroll: [f32; 2],
    translation_pending: bool,
    retry_available: bool,
    retry_busy: bool,
}

struct ResultClick {
    stamp: ResultClickStamp,
    focus: FocusHandle,
    _blur: Subscription,
    _activation: Subscription,
}

#[derive(Default)]
pub(crate) struct ResultClickState {
    slot: ClickSlot<ResultClick>,
}

fn down_proof(e: &MouseDownEvent) -> DownProof {
    let m = e.modifiers;
    DownProof {
        left: e.button == MouseButton::Left,
        position: [
            f32::from(e.position.x).to_bits(),
            f32::from(e.position.y).to_bits(),
        ],
        modifiers: u8::from(m.control)
            | (u8::from(m.alt) << 1)
            | (u8::from(m.shift) << 2)
            | (u8::from(m.platform) << 3)
            | (u8::from(m.function) << 4),
        click_count: e.click_count,
        first_mouse: e.first_mouse,
    }
}

impl CaptureView {
    fn result_source_identity(&self) -> ResultSourceIdentity {
        ResultSourceIdentity {
            target: self.current_ocr_target(),
            canvas: self.source.bounds,
            revisions: (
                self.editor.document.revision(),
                self.editor.document.edit_revision(),
            ),
            stamp: self.editor.scene_stamp(),
            identity: crate::scene::SourceIdentity::of(&self.source_pixels()),
        }
    }

    pub(crate) fn result_body_source(&self, window: &Window) -> Option<ResultBodySource> {
        if !self.inline_ocr_host()
            || self.recognition_result.is_none()
            || self.ocr_overlay.is_some() && self.result_editor.is_none()
        {
            return None;
        }
        Some(ResultLease {
            frame: self.result_output_frame(),
            body: self.current_result_body_identity(),
            source: self.result_source_identity(),
            text: Arc::from(self.current_result_text()?),
            epoch: Arc::clone(&self.ocr_projection_epoch),
            projection: self.ocr_projection(window)?,
        })
    }

    pub(crate) fn cancel_ocr_result_click(&self) {
        self.result_click.borrow_mut().slot.cancel();
    }

    pub(crate) fn invalidate_ocr_result_body_paint(&self) {
        self.result_body_paint.borrow_mut().take();
        self.result_input_layout.borrow_mut().take();
    }

    pub(crate) fn result_body_layout_current(&self, window: &Window) -> bool {
        self.result_body_paint
            .borrow()
            .as_ref()
            .is_some_and(|paint| {
                paint.source.current_in(self, window)
                    && (self.result_editor.is_none()
                        || paint.editor_scroll == self.result_editor_scroll_offset())
            })
    }

    fn result_editor_scroll_offset(&self) -> [f32; 2] {
        let offset = self.result_editor_scroll.offset();
        [f32::from(offset.x), f32::from(offset.y)]
    }

    pub(crate) fn result_body_event_current(
        &self,
        source: Option<&ResultBodySource>,
        layout_token: &Arc<()>,
        window: &Window,
    ) -> bool {
        let Some(source) = source else { return false };
        window.is_window_active()
            && self.focus.is_focused(window)
            && self.result_selection_pointer_ready()
            && !self.result_editor_composing()
            && source.current_in(self, window)
            && self
                .result_body_paint
                .borrow()
                .as_ref()
                .is_some_and(|paint| {
                    &paint.source == source
                        && Arc::ptr_eq(&paint.layout_token, layout_token)
                        && (self.result_editor.is_none()
                            || paint.editor_scroll == self.result_editor_scroll_offset())
                })
    }

    pub(crate) fn publish_result_body_layout(
        &self,
        source: Option<&ResultBodySource>,
        layout_token: &Arc<()>,
        bounds: Bounds<Pixels>,
        editor_scroll: [f32; 2],
        window: &Window,
    ) -> bool {
        let Some(source) = source else { return false };
        publish_body_paint(
            &self.result_body_paint,
            ResultBodyPaint {
                source: source.clone(),
                layout_token: Arc::clone(layout_token),
                bounds,
                editor_scroll,
                writer: None,
            },
            source.current_in(self, window),
        )
    }

    fn result_writer_access(&self, window: &Window, allow_drag: bool) -> bool {
        let capture = unsafe { GetCapture() };
        window.is_window_active()
            && self.focus.is_focused(window)
            && self.inline_ocr_host()
            && self.inline_ocr_interaction_idle()
            && self.result_editor_ready()
            && (allow_drag || !self.result_selection_dragging(self.result_selection_generation()))
            && self.pointer_down_start.is_none()
            && self.toolbar_drag.is_none()
            && self.input.drag_anchor.is_none()
            && !self.ocr_overlay_host_blocked()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && self.text_copy_completion_is_idle()
            && (capture.0.is_null()
                || allow_drag
                    && capture == crate::gpui_capture::hwnd(window)
                    && crate::pin_view::find_ui::primary_button_down())
    }

    fn result_writer_current(
        &self,
        writer: &Rc<RefCell<ActiveResultWriter>>,
        window: &Window,
        allow_drag: bool,
    ) -> bool {
        let active = writer.borrow();
        self.result_writer_access(window, allow_drag)
            && active.focus.is_focused(window)
            && self.result_body_source(window).is_some_and(|source| {
                self.result_editor.as_ref().is_some_and(|editor| {
                    active
                        .lease
                        .text_current(&source, &editor.input, &active.lease.layout_token)
                })
            })
            && self
                .result_body_paint
                .borrow()
                .as_ref()
                .is_some_and(|paint| {
                    same_writer_registration(&paint.writer, writer)
                        && Arc::ptr_eq(&paint.layout_token, &active.lease.layout_token)
                        && paint.source == active.lease.painted
                        && paint.editor_scroll == self.result_editor_scroll_offset()
                })
            && self
                .result_input_layout
                .borrow()
                .as_ref()
                .is_some_and(|input| {
                    input.generation == active.lease.painted.frame.generation
                        && Some(input.revision) == active.lease.painted.frame.revision
                })
    }

    pub(crate) fn result_writer_key_current(&self, window: &Window, cx: &Context<Self>) -> bool {
        let writer = self
            .result_body_paint
            .borrow()
            .as_ref()
            .and_then(|paint| paint.writer.clone());
        writer.is_some_and(|writer| {
            writer.borrow().owner.entity_id() == cx.entity_id()
                && !self.result_editor_composing()
                && self.result_writer_current(&writer, window, false)
        })
    }

    pub(crate) fn result_writer_geometry_current(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> bool {
        self.result_writer_key_current(window, cx)
            && self
                .result_body_paint
                .borrow()
                .as_ref()
                .and_then(|paint| paint.writer.clone())
                .is_some_and(|writer| writer.borrow().lease.geometry_current())
    }

    pub(crate) fn begin_result_key_command(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<ResultKeyCommand> {
        self.result_writer_key_current(window, cx).then_some(())?;
        let writer = self.result_body_paint.borrow().as_ref()?.writer.clone()?;
        let active = writer.borrow();
        let ticket = ResultKeyCommand {
            writer: writer.clone(),
            before: active.lease.expected.clone(),
            input_before: active.lease.input.clone(),
            pointer: false,
        };
        Some(ticket)
    }

    pub(crate) fn begin_result_pointer_command(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<ResultKeyCommand> {
        let writer = self.result_body_paint.borrow().as_ref()?.writer.clone()?;
        if writer.borrow().owner.entity_id() != cx.entity_id()
            || self.result_editor_composing()
            || !self.result_writer_current(&writer, window, true)
            || !writer.borrow().lease.geometry_current()
        {
            return None;
        }
        let active = writer.borrow();
        Some(ResultKeyCommand {
            writer: writer.clone(),
            before: active.lease.expected.clone(),
            input_before: active.lease.input.clone(),
            pointer: true,
        })
    }

    pub(crate) fn finish_result_key_command(
        &self,
        ticket: ResultKeyCommand,
        window: &Window,
        cx: &Context<Self>,
    ) {
        let domain_ready = ticket.writer.borrow().owner.entity_id() == cx.entity_id()
            && ticket.writer.borrow().focus.is_focused(window)
            && self.result_writer_access(window, ticket.pointer)
            && self
                .result_body_paint
                .borrow()
                .as_ref()
                .is_some_and(|paint| {
                    let active = ticket.writer.borrow();
                    same_writer_registration(&paint.writer, &ticket.writer)
                        && Arc::ptr_eq(&paint.layout_token, &active.lease.layout_token)
                        && paint.source == active.lease.painted
                        && paint.editor_scroll == self.result_editor_scroll_offset()
                });
        let after = domain_ready
            .then(|| {
                Some((
                    self.result_body_source(window)?,
                    self.result_editor.as_ref()?.input.clone(),
                ))
            })
            .flatten();
        let mut active = ticket.writer.borrow_mut();
        if let Some((after, input)) = after {
            active
                .lease
                .advance_owned(&ticket.before, &ticket.input_before, after, input);
        } else {
            active.lease.valid = false;
        }
    }

    fn result_click_stamp(
        &self,
        intent: ResultIntent,
        window: &Window,
    ) -> Option<ResultClickStamp> {
        let source = self.result_body_source(window)?;
        let paint = self.result_body_paint.borrow();
        let paint = paint.as_ref().filter(|paint| paint.source == source)?;
        Some(ResultClickStamp {
            source,
            intent,
            selection: self.result_selection_range(),
            bounds: paint.bounds,
            editor_scroll: paint.editor_scroll,
            translation_pending: self.translation_pending(),
            retry_available: self.result_retry_available(),
            retry_busy: self.result_retry_busy(),
        })
    }

    fn result_click_access(&self, window: &Window, held: bool) -> bool {
        let capture = unsafe { GetCapture() };
        window.is_window_active()
            && self.focus.is_focused(window)
            && self.inline_ocr_host()
            && self.inline_ocr_interaction_idle()
            && self.result_selection_pointer_ready()
            && !self.result_editor_composing()
            && !self.result_selection_dragging(self.result_selection_generation())
            && self.pointer_down_start.is_none()
            && self.toolbar_drag.is_none()
            && self.input.drag_anchor.is_none()
            && !self.ocr_overlay_host_blocked()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && self.text_copy_completion_is_idle()
            && self.painted_result_output.get() == Some(self.result_output_frame())
            && self
                .painted_result_projection
                .borrow()
                .as_ref()
                .is_some_and(|paint| {
                    paint.current(&self.ocr_projection_epoch, self.ocr_projection(window))
                })
            && self.result_body_layout_current(window)
            && (capture.0.is_null()
                || held
                    && capture == crate::gpui_capture::hwnd(window)
                    && crate::pin_view::find_ui::primary_button_down())
    }

    fn result_click_current(&self, stamp: &ResultClickStamp, window: &Window, held: bool) -> bool {
        self.result_click_access(window, held)
            && self.result_click_stamp(stamp.intent, window).as_ref() == Some(stamp)
    }

    pub(crate) fn refresh_ocr_result_click(&self, window: &Window) {
        let invalid = self
            .result_click
            .borrow()
            .slot
            .payload()
            .is_some_and(|click| {
                !click.focus.is_focused(window)
                    || !self.result_click_current(&click.stamp, window, true)
            });
        if invalid {
            self.cancel_ocr_result_click();
        }
    }

    pub(crate) fn bind_result_click(
        &self,
        button: gpui::Stateful<gpui::Div>,
        widget: &'static str,
        intent: ResultIntent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // 只捕获render的稳定来源；首次真实paint之后Down再取得已画布局的bounds。
        let source = self.result_body_source(window);
        let down_source = source.clone();
        button
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    view.cancel_ocr_result_click();
                    cx.stop_propagation();
                    let Some(source) = &down_source else { return };
                    if event.first_mouse
                        || !source.current_in(view, window)
                        || !view.result_click_access(window, true)
                    {
                        return;
                    }
                    let Some(stamp) = view
                        .result_click_stamp(intent, window)
                        .filter(|s| &s.source == source)
                    else {
                        return;
                    };
                    let Some(focus) = window.focused(cx) else {
                        return;
                    };
                    let token = Arc::new(());
                    let weak = Rc::downgrade(&view.result_click);
                    let blur_token = Arc::clone(&token);
                    let blur = cx.on_blur(&focus, window, move |_, _, _| {
                        if let Some(click) = weak.upgrade() {
                            click.borrow_mut().slot.cancel_token(&blur_token);
                        }
                    });
                    let weak = Rc::downgrade(&view.result_click);
                    let activation_token = Arc::clone(&token);
                    let activation = cx.observe_window_activation(window, move |_, window, _| {
                        if !window.is_window_active() {
                            if let Some(click) = weak.upgrade() {
                                click.borrow_mut().slot.cancel_token(&activation_token);
                            }
                        }
                    });
                    view.result_click.borrow_mut().slot.arm(
                        token,
                        widget,
                        down_proof(event),
                        ResultClick {
                            stamp,
                            focus,
                            _blur: blur,
                            _activation: activation,
                        },
                    );
                }),
            )
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                let (down, left_pair) = match event {
                    ClickEvent::Mouse(mouse) => (
                        Some(down_proof(&mouse.down)),
                        mouse.down.button == MouseButton::Left
                            && mouse.up.button == MouseButton::Left,
                    ),
                    ClickEvent::Keyboard(_) => (None, false),
                };
                let claimed =
                    view.result_click
                        .borrow_mut()
                        .slot
                        .claim(widget, down, left_pair, |click| {
                            source.as_ref() == Some(&click.stamp.source)
                                && click.stamp.intent == intent
                                && click.focus.is_focused(window)
                                && view.result_click_current(&click.stamp, window, false)
                                && !crate::pin_view::find_ui::primary_button_down()
                        });
                if let Some(click) = claimed {
                    match click.stamp.intent {
                        ResultIntent::Copy => {
                            view.copy_recognition_result(click.stamp.source.frame, window, cx)
                        }
                        ResultIntent::Close => {
                            view.dismiss_recognition_result();
                            cx.notify();
                        }
                        ResultIntent::Translate => {
                            view.translate_recognition_result(click.stamp.source.frame, cx)
                        }
                        ResultIntent::Retry => view.retry_recognition_result(window, cx),
                        ResultIntent::Edit(editing) => {
                            if view.result_editor.is_some() == editing {
                                view.toggle_result_editor(window, cx);
                            }
                        }
                    }
                }
            }))
    }
}

/// paint capture 先于内部ScrollHandle。滚动先撤销布局资格，再由实际新paint重授予。
pub(crate) fn observe_result_input(view: &CaptureView, window: &mut Window) {
    let weak = Rc::downgrade(&view.result_click);
    window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
        if phase == DispatchPhase::Capture {
            if let Some(click) = weak.upgrade() {
                click.borrow_mut().slot.cancel();
            }
        }
    });
    let click = Rc::downgrade(&view.result_click);
    let body = Rc::downgrade(&view.result_body_paint);
    let input = Rc::downgrade(&view.result_input_layout);
    window.on_mouse_event(move |_: &ScrollWheelEvent, phase, window, _| {
        if phase == DispatchPhase::Capture {
            if let Some(click) = click.upgrade() {
                click.borrow_mut().slot.cancel();
            }
            if let Some(body) = body.upgrade() {
                body.borrow_mut().take();
            }
            if let Some(input) = input.upgrade() {
                input.borrow_mut().take();
            }
            window.refresh();
        }
    });
    let weak = Rc::downgrade(&view.result_click);
    window.on_key_event(move |_: &KeyDownEvent, phase, _, _| {
        if phase == DispatchPhase::Capture {
            if let Some(click) = weak.upgrade() {
                click.borrow_mut().slot.cancel();
            }
        }
    });
}

/// 每次真实paint绑定一份新实体；旧IME callback不借新共享布局复活。
pub(crate) struct ResultInputLeaseHandler {
    inner: Entity<crate::text_input::ResultInputHandler>,
    owner: WeakEntity<CaptureView>,
    writer: Rc<RefCell<ActiveResultWriter>>,
    focus: FocusHandle,
    body_paint: Rc<RefCell<Option<ResultBodyPaint>>>,
    input_layout: Rc<RefCell<Option<crate::text_input::ResultInputLayout>>>,
    click: Rc<RefCell<ResultClickState>>,
    _blur: Subscription,
    _activation: Subscription,
}

impl ResultInputLeaseHandler {
    pub(crate) fn new(
        inner: Entity<crate::text_input::ResultInputHandler>,
        owner: WeakEntity<CaptureView>,
        source: ResultBodySource,
        layout_token: Arc<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = owner.upgrade().expect("真实paint已核结果owner");
        let view = view.read(cx);
        let focus = view.focus.clone();
        let body_paint = Rc::clone(&view.result_body_paint);
        let input_layout = Rc::clone(&view.result_input_layout);
        let click = Rc::clone(&view.result_click);
        let input = view
            .result_editor
            .as_ref()
            .expect("真实输入paint拥有全文编辑器")
            .input
            .clone();
        let writer = Rc::new(RefCell::new(ActiveResultWriter {
            owner: owner.clone(),
            focus: focus.clone(),
            lease: WriterLease::new(source, input, layout_token),
        }));
        if let Some(paint) = body_paint.borrow_mut().as_mut() {
            let active = writer.borrow();
            if paint.source == active.lease.painted
                && Arc::ptr_eq(&paint.layout_token, &active.lease.layout_token)
            {
                paint.writer = Some(writer.clone());
            }
        }
        let blur = cx.on_blur(&focus, window, |handler, _, _| handler.revoke());
        let activation = cx.observe_window_activation(window, |handler, window, _| {
            if !window.is_window_active() {
                handler.revoke();
            }
        });
        Self {
            inner,
            owner,
            writer,
            focus,
            body_paint,
            input_layout,
            click,
            _blur: blur,
            _activation: activation,
        }
    }

    fn revoke(&mut self) {
        let token = {
            let mut active = self.writer.borrow_mut();
            active.lease.valid = false;
            Arc::clone(&active.lease.layout_token)
        };
        let owns = self
            .body_paint
            .borrow()
            .as_ref()
            .is_some_and(|paint| same_writer_registration(&paint.writer, &self.writer));
        if owns && revoke_layout(&self.body_paint, &token) {
            self.input_layout.borrow_mut().take();
            self.click.borrow_mut().slot.cancel();
        }
    }

    fn with_input<R>(
        &self,
        geometry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        call: impl FnOnce(
            &mut crate::text_input::ResultInputHandler,
            &mut Window,
            &mut Context<crate::text_input::ResultInputHandler>,
        ) -> R,
    ) -> Option<R> {
        let ready = self.owner.upgrade().is_some_and(|owner| {
            let view = owner.read(cx);
            self.writer.borrow().owner.entity_id() == owner.entity_id()
                && self.focus.is_focused(window)
                && view.result_writer_current(&self.writer, window, false)
                && (!geometry || self.writer.borrow().lease.geometry_current())
        });
        ready.then(|| self.inner.update(cx, |inner, cx| call(inner, window, cx)))
    }

    fn write_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        call: impl FnOnce(
            &mut crate::text_input::ResultInputHandler,
            &mut Window,
            &mut Context<crate::text_input::ResultInputHandler>,
        ),
    ) {
        let (before, input_before) = {
            let active = self.writer.borrow();
            (active.lease.expected.clone(), active.lease.input.clone())
        };
        if self.with_input(false, window, cx, call).is_none() {
            return;
        }
        let after = self.owner.upgrade().and_then(|owner| {
            let view = owner.read(cx);
            (view.result_writer_access(window, false)
                && self.focus.is_focused(window)
                && view
                    .result_body_paint
                    .borrow()
                    .as_ref()
                    .is_some_and(|paint| {
                        let active = self.writer.borrow();
                        same_writer_registration(&paint.writer, &self.writer)
                            && Arc::ptr_eq(&paint.layout_token, &active.lease.layout_token)
                            && paint.source == active.lease.painted
                            && paint.editor_scroll == view.result_editor_scroll_offset()
                    }))
            .then(|| {
                Some((
                    view.result_body_source(window)?,
                    view.result_editor.as_ref()?.input.clone(),
                ))
            })
            .flatten()
        });
        if let Some((after, input_after)) = after {
            self.writer.borrow_mut().lease.advance_owned(
                &before,
                &input_before,
                after,
                input_after,
            );
        } else {
            self.writer.borrow_mut().lease.valid = false;
        }
    }
}

impl EntityInputHandler for ResultInputLeaseHandler {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.with_input(false, window, cx, |inner, window, cx| {
            inner.text_for_range(range, adjusted, window, cx)
        })
        .flatten()
    }
    fn selected_text_range(
        &mut self,
        disabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.with_input(false, window, cx, |inner, window, cx| {
            inner.selected_text_range(disabled, window, cx)
        })
        .flatten()
    }
    fn marked_text_range(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.with_input(false, window, cx, |inner, window, cx| {
            inner.marked_text_range(window, cx)
        })
        .flatten()
    }
    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.write_input(window, cx, |inner, window, cx| {
            inner.unmark_text(window, cx)
        });
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_input(window, cx, |inner, window, cx| {
            inner.replace_text_in_range(range, text, window, cx)
        });
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_input(window, cx, |inner, window, cx| {
            inner.replace_and_mark_text_in_range(range, text, selected, window, cx)
        });
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.with_input(true, window, cx, |inner, window, cx| {
            inner.bounds_for_range(range, bounds, window, cx)
        })
        .flatten()
    }
    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.with_input(true, window, cx, |inner, window, cx| {
            inner.character_index_for_point(point, window, cx)
        })
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type Lease = ResultLease<Option<u64>, [u32; 3]>;
    fn lease(target: Option<u64>) -> Lease {
        ResultLease {
            frame: ResultFrame {
                generation: 7,
                revision: Some(3),
            },
            body: Arc::new(()),
            source: target,
            text: Arc::from("中文😀\n"),
            epoch: Arc::new(()),
            projection: [125, 800, 600],
        }
    }
    #[test]
    fn pure_control_or_qr_text_source_without_ocr_target_still_matches() {
        let source = lease(None);
        assert!(source == source.clone());
        assert!(source != lease(Some(7)));
    }
    #[test]
    fn equal_body_bytes_and_frame_do_not_resurrect_replaced_body_owner() {
        let source = lease(None);
        let mut other = source.clone();
        other.body = Arc::new(());
        assert!(source != other);
    }
    #[test]
    fn zoom_dpi_or_client_aba_needs_new_epoch_even_when_projection_equal() {
        let source = lease(Some(7));
        let mut other = source.clone();
        other.epoch = Arc::new(());
        assert_eq!(source.projection, other.projection);
        assert!(source != other);
    }
    #[test]
    fn changed_source_selection_document_frame_revision_and_text_reject() {
        let source = lease(Some(7));
        let mut changes = Vec::new();
        let mut other = source.clone();
        other.source = Some(8);
        changes.push(other);
        let mut other = source.clone();
        other.frame.generation += 1;
        changes.push(other);
        let mut other = source.clone();
        other.frame.revision = None;
        changes.push(other);
        let mut other = source.clone();
        other.text = Arc::from("新文字");
        changes.push(other);
        let mut other = source.clone();
        other.projection[0] = 150;
        changes.push(other);
        for other in changes {
            assert!(source != other);
        }
    }
    #[test]
    fn stale_caret_paint_cannot_clear_or_replace_new_layout_receipt() {
        let slot = RefCell::new(Some("new-layout"));
        assert!(!publish_body_paint(&slot, "old-layout", false));
        assert_eq!(*slot.borrow(), Some("new-layout"));
        assert!(publish_body_paint(&slot, "current-layout", true));
        assert_eq!(*slot.borrow(), Some("current-layout"));
    }
    #[test]
    fn old_focus_callback_cannot_revoke_later_layout_token() {
        let old = Arc::new(());
        let new = Arc::new(());
        let slot = RefCell::new(Some(BodyPaintLease {
            source: lease(None),
            layout_token: new.clone(),
            bounds: (),
            editor_scroll: [0., 0.],
            writer: None,
        }));
        assert!(!revoke_layout(&slot, &old));
        assert!(slot.borrow().is_some());
        assert!(revoke_layout(&slot, &new));
        assert!(slot.borrow().is_none());
    }
    #[test]
    fn edited_undo_equal_bytes_keep_new_actual_body_identity() {
        let mut editor = crate::capture_view::text_result::ResultEditor::new("中文😀".into(), 0..0);
        let before = Arc::clone(&editor.body_identity);
        editor.replace(Some(0..0), "a", false, None);
        editor.undo();
        assert_eq!(editor.text, "中文😀");
        assert!(!Arc::ptr_eq(&before, &editor.body_identity));
    }
    #[test]
    fn stable_button_source_does_not_depend_on_per_paint_layout_token() {
        let source = lease(None);
        let old = BodyPaintLease {
            source: source.clone(),
            layout_token: Arc::new(()),
            bounds: [1, 2],
            editor_scroll: [0., 0.],
            writer: None,
        };
        let new = BodyPaintLease {
            source: source.clone(),
            layout_token: Arc::new(()),
            bounds: [1, 2],
            editor_scroll: [0., 0.],
            writer: None,
        };
        assert!(old.source == new.source);
        assert_eq!(old.bounds, new.bounds);
        assert!(!Arc::ptr_eq(&old.layout_token, &new.layout_token));
    }

    fn editor_source(
        editor: &crate::capture_view::text_result::ResultEditor,
        domain: &Lease,
    ) -> Lease {
        let mut source = domain.clone();
        source.frame.revision = Some(editor.revision);
        source.body = Arc::clone(&editor.body_identity);
        source.text = Arc::from(editor.text.as_str());
        source
    }

    #[test]
    fn actual_editor_preedit_update_commit_and_text_queries_continue_without_paint() {
        let mut editor =
            crate::capture_view::text_result::ResultEditor::new("甲old\n乙".into(), 3..6);
        let domain = editor_source(&editor, &lease(None));
        let token = Arc::new(());
        let mut writer = WriterLease::new(domain.clone(), editor.input.clone(), token.clone());
        for (text, marked) in [("中", true), ("中文", true), ("文😀", false)] {
            let before = editor_source(&editor, &domain);
            let input_before = editor.input.clone();
            assert!(writer.text_current(&before, &input_before, &token));
            editor.replace(None, text, marked, None);
            let after = editor_source(&editor, &domain);
            assert!(writer.advance_owned(
                &before,
                &input_before,
                after.clone(),
                editor.input.clone()
            ));
            assert!(writer.text_current(&after, &editor.input, &token));
            assert!(!writer.geometry_current());
        }
        assert_eq!(editor.text, "甲文😀\n乙");
        assert!(editor.input.marked.is_none());
        assert!(writer.painted == domain);
    }

    #[test]
    fn actual_editor_own_unmark_can_finish_same_native_callback_chain() {
        let mut editor = crate::capture_view::text_result::ResultEditor::new("甲".into(), 3..3);
        let domain = editor_source(&editor, &lease(None));
        let token = Arc::new(());
        let mut writer = WriterLease::new(domain.clone(), editor.input.clone(), token.clone());
        let before = editor_source(&editor, &domain);
        let input_before = editor.input.clone();
        editor.replace(None, "😀", true, None);
        assert!(writer.advance_owned(
            &before,
            &input_before,
            editor_source(&editor, &domain),
            editor.input.clone()
        ));
        let before = editor_source(&editor, &domain);
        let input_before = editor.input.clone();
        assert!(writer.text_current(&before, &input_before, &token));
        assert!(editor.unmark());
        let after = editor_source(&editor, &domain);
        assert!(writer.advance_owned(&before, &input_before, after.clone(), editor.input.clone()));
        assert!(writer.text_current(&after, &editor.input, &token));
        assert!(!writer.geometry_current());
        assert_eq!(editor.text, "甲😀");
    }

    #[test]
    fn actual_external_edit_undo_aba_and_another_handler_cannot_adopt_writer_chain() {
        let mut editor = crate::capture_view::text_result::ResultEditor::new("中文😀".into(), 0..0);
        let domain = editor_source(&editor, &lease(None));
        let token = Arc::new(());
        let writer = WriterLease::new(domain.clone(), editor.input.clone(), token.clone());
        editor.replace(None, "a", false, None);
        assert!(!writer.text_current(&editor_source(&editor, &domain), &editor.input, &token));
        editor.undo();
        assert_eq!(editor.text, "中文😀");
        assert!(!writer.text_current(&editor_source(&editor, &domain), &editor.input, &token));
        let mut active = WriterLease::new(
            editor_source(&editor, &domain),
            editor.input.clone(),
            token.clone(),
        );
        let other = WriterLease::new(
            editor_source(&editor, &domain),
            editor.input.clone(),
            token.clone(),
        );
        let before = editor_source(&editor, &domain);
        let input_before = editor.input.clone();
        editor.replace(None, "新", true, None);
        let after = editor_source(&editor, &domain);
        assert!(active.advance_owned(&before, &input_before, after.clone(), editor.input.clone()));
        assert!(!other.text_current(&after, &editor.input, &token));
        assert!(active.text_current(&after, &editor.input, &token));
    }

    #[test]
    fn writer_requires_same_layout_epoch_source_generation_and_expected_input_state() {
        let source = lease(None);
        let token = Arc::new(());
        let writer = WriterLease::new(source.clone(), 1u8, token.clone());
        assert!(!writer.text_current(&source, &2, &token));
        assert!(!writer.text_current(&source, &1, &Arc::new(())));
        let mut changed = source.clone();
        changed.epoch = Arc::new(());
        assert!(!writer.text_current(&changed, &1, &token));
        for kind in 0..3 {
            let mut writer = WriterLease::new(source.clone(), 1u8, token.clone());
            let mut after = source.clone();
            match kind {
                0 => after.source = Some(7),
                1 => after.frame.generation += 1,
                _ => after.projection[0] += 1,
            }
            assert!(!writer.advance_owned(&source, &1, after, 2));
            assert!(!writer.text_current(&source, &1, &token));
        }
    }
}
