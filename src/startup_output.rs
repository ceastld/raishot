//! 早期输出只保存事件当时的范围；后续编辑边界使请求不可逆失效。
use crate::{editor::Editor, geometry::Rect, startup_preview::PresentedWindowPreview};
use windows::Win32::{
    System::SystemServices::{MK_LBUTTON, MK_RBUTTON},
    UI::WindowsAndMessaging::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupOutputKind {
    Pin,
    Confirm,
    ControlText,
    Ocr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupOutputRange {
    Confirmed(Rect),
    NativeConfirmed {
        receipt: PresentedWindowPreview,
        point: (i32, i32),
    },
    NativePreview {
        receipt: PresentedWindowPreview,
        point: (i32, i32),
    },
    NativeManualPreview {
        receipt: PresentedWindowPreview,
        point: (i32, i32),
    },
    NativePreviewConfirm(PresentedWindowPreview),
    Text {
        rect: Rect,
        original_crop: Option<Rect>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartupOutput {
    pub(crate) kind: StartupOutputKind,
    pub(crate) range: StartupOutputRange,
}

impl StartupOutput {
    /// 固定识别种类与按键时范围；接收端另核原 crop 和空闲状态。
    pub(crate) fn text(&self) -> Option<(StartupOutputKind, Rect, Option<Rect>)> {
        match (self.kind, self.range) {
            (
                kind @ (StartupOutputKind::ControlText | StartupOutputKind::Ocr),
                StartupOutputRange::Text {
                    rect,
                    original_crop,
                },
            ) => Some((kind, rect, original_crop)),
            _ => None,
        }
    }
    /// 只有已确认的原范围能输出；Preview/Text 不能借最终范围相等走此门。
    pub(crate) fn accepts(self, crop: Option<Rect>, ready: bool) -> bool {
        ready
            && match self.range {
                StartupOutputRange::Confirmed(saved) => crop == Some(saved),
                StartupOutputRange::NativeConfirmed { receipt, .. } => {
                    self.kind == StartupOutputKind::Pin
                        && receipt.serial != 0
                        && crop == Some(receipt.rect)
                }
                _ => false,
            }
    }
    /// 已确认范围在原生 Middle 入队时保存，不借交接后出现的工具栏重新命中。
    pub(crate) fn native_confirmed(self) -> Option<(Rect, (i32, i32))> {
        match self.range {
            StartupOutputRange::NativeConfirmed { receipt, point }
                if self.kind == StartupOutputKind::Pin && receipt.serial != 0 =>
            {
                Some((receipt.rect, point))
            }
            _ => None,
        }
    }
    /// 只返回事件当时保存的预览；接收者还需核宿主、编辑空闲和范围资格。
    pub(crate) fn preview(self) -> Option<(Rect, (i32, i32))> {
        match self.range {
            StartupOutputRange::NativePreview { receipt, point }
                if self.kind == StartupOutputKind::Pin && receipt.serial != 0 =>
            {
                Some((receipt.rect, point))
            }
            _ => None,
        }
    }
    /// 固定预览无需指针包含，但不能借自动预览或已确认 crop 的接收门。
    pub(crate) fn manual_preview(self) -> Option<(Rect, (i32, i32))> {
        match self.range {
            StartupOutputRange::NativeManualPreview { receipt, point }
                if self.kind == StartupOutputKind::Pin && receipt.serial != 0 =>
            {
                Some((receipt.rect, point))
            }
            _ => None,
        }
    }
    pub(crate) fn preview_confirm(self) -> Option<Rect> {
        match self.range {
            StartupOutputRange::NativePreviewConfirm(receipt)
                if self.kind == StartupOutputKind::Confirm && receipt.serial != 0 =>
            {
                Some(receipt.rect)
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct StartupOutputQueue {
    pending: Option<StartupOutput>,
    deferred_preview: DeferredPreviewFence,
    deferred_confirmed: DeferredPreviewFence,
    deferred_confirmed_owner: Option<Rect>,
    preview_confirm_release_pending: bool,
    text_cycles: u8,
    deferred_text: DeferredPreviewFence,
    deferred_text_crop: Option<Rect>,
}

/// 只从第一次成功 handoff 的真实状态建立资格；后续编辑失效不可重开。
#[derive(Default)]
struct DeferredPreviewFence {
    established: bool,
    live: bool,
}

impl DeferredPreviewFence {
    fn establish(&mut self, eligible: bool) {
        if !self.established {
            self.established = true;
            self.live = eligible;
        }
    }
    fn invalidate(&mut self) {
        self.established = true;
        self.live = false;
    }
}

impl StartupOutputQueue {
    /// 拒绝事件时的范围也消费原按键周期，不在交接后用晚到画面补输出。
    pub(crate) fn consume_preview_confirm_cycle(&mut self) {
        self.preview_confirm_release_pending = true;
    }
    pub(crate) fn consume_text_cycle(
        &mut self,
        shortcut: crate::startup_surface_input::TextShortcut,
    ) {
        self.text_cycles |= match shortcut {
            crate::startup_surface_input::TextShortcut::Region => 1,
            crate::startup_surface_input::TextShortcut::Control => 2,
        };
    }
    /// 已接受周期即使被编辑撤销，也消费真实 held/Up；新的物理 Down 先过期旧周期。
    pub(crate) fn take_text_cycle(&mut self, message: u32, key: usize, value: isize) -> bool {
        let bit = match key {
            0x52 => 1,
            0x43 => 2,
            _ => return false,
        };
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
            if value & (1 << 30) == 0 {
                self.text_cycles &= !bit;
                false
            } else {
                self.text_cycles & bit != 0
            }
        } else if matches!(message, WM_KEYUP | WM_SYSKEYUP) && self.text_cycles & bit != 0 {
            self.text_cycles &= !bit;
            true
        } else {
            false
        }
    }
    pub(crate) fn invalidate_text(&mut self) {
        if self.deferred_text.established {
            self.deferred_text.invalidate();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|output| output.text().is_some())
        {
            self.pending = None;
        }
    }
    pub(crate) fn observe_text_down(&mut self) {
        self.pending = None;
        self.preview_confirm_release_pending = false;
    }
    pub(crate) fn offer_text(
        &mut self,
        editor: &Editor,
        canvas: Rect,
        shortcut: crate::startup_surface_input::TextShortcut,
        receipt: Option<PresentedWindowPreview>,
        busy: bool,
    ) -> bool {
        if !text_editor_ready(editor, canvas, busy) {
            return false;
        }
        self.accept_text(canvas, editor.crop, shortcut, receipt)
    }
    pub(crate) fn offer_deferred_text(
        &mut self,
        canvas: Rect,
        shortcut: crate::startup_surface_input::TextShortcut,
        receipt: Option<PresentedWindowPreview>,
        busy: bool,
    ) -> bool {
        if !self.deferred_text.live || busy {
            return false;
        }
        self.accept_text(canvas, self.deferred_text_crop, shortcut, receipt)
    }
    fn accept_text(
        &mut self,
        canvas: Rect,
        original_crop: Option<Rect>,
        shortcut: crate::startup_surface_input::TextShortcut,
        receipt: Option<PresentedWindowPreview>,
    ) -> bool {
        let Some(rect) = original_crop
            .or_else(|| receipt.filter(|r| r.serial != 0).map(|r| r.rect))
            .filter(|rect| rect.valid(canvas.width, canvas.height))
        else {
            return false;
        };
        let kind = match (shortcut, original_crop) {
            (crate::startup_surface_input::TextShortcut::Region, Some(_)) => StartupOutputKind::Ocr,
            _ => StartupOutputKind::ControlText,
        };
        self.pending = Some(StartupOutput {
            kind,
            range: StartupOutputRange::Text {
                rect,
                original_crop,
            },
        });
        self.text_cycles |= match shortcut {
            crate::startup_surface_input::TextShortcut::Region => 1,
            crate::startup_surface_input::TextShortcut::Control => 2,
        };
        true
    }
    /// 新 Enter 替换旧输出，但不把仍有效的初始 fence 撤销。
    pub(crate) fn observe_preview_confirm_down(&mut self) {
        self.pending = None;
        self.preview_confirm_release_pending = false;
    }
    /// 只消费已接受的一个真实按键周期；不将其抬起作为待执行编辑键。
    pub(crate) fn take_preview_confirm_release(&mut self, message: u32, key: usize) -> bool {
        if self.preview_confirm_release_pending
            && matches!(message, WM_KEYUP | WM_SYSKEYUP)
            && key == 0x0d
        {
            self.preview_confirm_release_pending = false;
            true
        } else {
            false
        }
    }
    pub(crate) fn offer_preview_confirm(
        &mut self,
        editor: &Editor,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        busy: bool,
    ) -> bool {
        let Some(receipt) = confirmed_preview_receipt(editor, canvas, receipt, busy) else {
            return false;
        };
        self.accept_preview_confirm(receipt);
        true
    }
    pub(crate) fn offer_deferred_preview_confirm(
        &mut self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        busy: bool,
    ) -> bool {
        let Some(receipt) = self.deferred_preview_receipt(canvas, receipt, busy) else {
            return false;
        };
        self.accept_preview_confirm(receipt);
        true
    }
    /// 延迟预览只借首次资格：资格仍有效、不忙，且原事件回执有效。
    fn deferred_preview_receipt(
        &self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        busy: bool,
    ) -> Option<PresentedWindowPreview> {
        if !self.deferred_preview.live || busy {
            return None;
        }
        receipt.filter(|receipt| {
            receipt.serial != 0 && receipt.rect.valid(canvas.width, canvas.height)
        })
    }
    fn accept_preview_confirm(&mut self, receipt: PresentedWindowPreview) {
        self.pending = Some(StartupOutput {
            kind: StartupOutputKind::Confirm,
            range: StartupOutputRange::NativePreviewConfirm(receipt),
        });
        self.preview_confirm_release_pending = true;
    }
    /// 仅在第一次 HWND/坐标核验成功、targetSome 之前读取尚权威的 Editor。
    pub(crate) fn begin_handoff_preview(&mut self, editor: &Editor, canvas: Rect, busy: bool) {
        if !self.deferred_text.established {
            self.deferred_text_crop = editor.crop;
            self.deferred_text
                .establish(text_editor_ready(editor, canvas, busy));
        }
        self.deferred_preview.establish(
            canvas.valid(canvas.width, canvas.height)
                && editor.canvas == canvas
                && crate::image_editor::screenshot_preview_pin_idle(editor, busy, false),
        );
        if !self.deferred_confirmed.established {
            let eligible = editor.canvas == canvas
                && canvas.valid(canvas.width, canvas.height)
                && crate::image_editor::screenshot_middle_pin_ready(editor, canvas, busy, false);
            self.deferred_confirmed_owner = editor.crop.filter(|_| eligible);
            self.deferred_confirmed.establish(eligible);
        }
    }

    /// 原确认范围的输入归属不随输出 fence 撤销；取消后不能改按晚到 crop 再输出。
    /// 仅首次合格 handoff 建立，真实 receipt 缺失或范围不同仍沿原 FIFO 路由。
    pub(crate) fn owns_deferred_confirmed_middle(
        &self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
    ) -> bool {
        receipt.is_some_and(|receipt| {
            receipt.serial != 0
                && receipt.rect.valid(canvas.width, canvas.height)
                && Some(receipt.rect) == self.deferred_confirmed_owner
        })
    }

    /// targetSome 的旧 Editor 不再权威；只借首次资格和原事件真实 selection 回执。
    pub(crate) fn offer_deferred_confirmed(
        &mut self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        actual_point: (i32, i32),
        busy: bool,
    ) -> bool {
        if !self.deferred_confirmed.live || busy {
            return false;
        }
        let Some(receipt) =
            receipt.filter(|receipt| self.owns_deferred_confirmed_middle(canvas, Some(*receipt)))
        else {
            return false;
        };
        self.pending = Some(StartupOutput {
            kind: StartupOutputKind::Pin,
            range: StartupOutputRange::NativeConfirmed {
                receipt,
                point: actual_point,
            },
        });
        true
    }

    /// targetSome 只借最初资格和事件 receipt；不能再读取旧 Editor 或晚来的 GPUI hover。
    pub(crate) fn offer_deferred_preview(
        &mut self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        actual_point: (i32, i32),
        busy: bool,
    ) -> bool {
        let Some(receipt) = self.deferred_preview_receipt(canvas, receipt, busy) else {
            return false;
        };
        // 原生调用方已按事件顺序 observe_input；此次新中键替换旧请求。
        self.pending = Some(StartupOutput {
            kind: StartupOutputKind::Pin,
            range: StartupOutputRange::NativePreview {
                receipt,
                point: actual_point,
            },
        });
        true
    }

    /// 仍使用最初空闲 fence 与原事件回执；固定范围单独标记，保留真实事件点。
    pub(crate) fn offer_deferred_manual_preview(
        &mut self,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        actual_point: (i32, i32),
        busy: bool,
    ) -> bool {
        let Some(receipt) = self.deferred_preview_receipt(canvas, receipt, busy) else {
            return false;
        };
        self.pending = Some(StartupOutput {
            kind: StartupOutputKind::Pin,
            range: StartupOutputRange::NativeManualPreview {
                receipt,
                point: actual_point,
            },
        });
        true
    }

    /// 两层 Move 合并保留了左/右键曾按住的事实，必须在 defer 前调用。
    pub(crate) fn invalidate_deferred_preview(&mut self) {
        self.deferred_preview.invalidate();
        self.deferred_confirmed.invalidate();
        self.deferred_text.invalidate();
        self.pending = None;
        self.preview_confirm_release_pending = false;
    }

    pub(crate) fn offer(
        &mut self,
        kind: StartupOutputKind,
        editor: &Editor,
        canvas: Rect,
        busy: bool,
    ) -> bool {
        if !crate::image_editor::screenshot_middle_pin_ready(editor, canvas, busy, false) {
            return false;
        }
        if self.pending.is_none() {
            self.pending = editor.crop.map(|crop| StartupOutput {
                kind,
                range: StartupOutputRange::Confirmed(crop),
            });
        }
        true
    }
    /// 只用于 targetNone；receipt 在 Middle 入队时固定，不能由后来 Paint 回填。
    pub(crate) fn offer_preview(
        &mut self,
        editor: &Editor,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        actual_point: (i32, i32),
        busy: bool,
    ) -> bool {
        let Some(receipt) = receipt.filter(|receipt| receipt.serial != 0) else {
            return false;
        };
        if crate::preview_middle_pin::candidate(
            editor,
            canvas,
            actual_point,
            Some(receipt.rect),
            busy,
            false,
        ) != Some(receipt.rect)
        {
            return false;
        }
        if self.pending.is_none() {
            self.pending = Some(StartupOutput {
                kind: StartupOutputKind::Pin,
                range: StartupOutputRange::NativePreview {
                    receipt,
                    point: actual_point,
                },
            });
        }
        true
    }
    /// 调用方另外核验 receipt.rect 与事件时的固定范围一致。
    pub(crate) fn offer_manual_preview(
        &mut self,
        editor: &Editor,
        canvas: Rect,
        receipt: Option<PresentedWindowPreview>,
        actual_point: (i32, i32),
        busy: bool,
    ) -> bool {
        let Some(receipt) = confirmed_preview_receipt(editor, canvas, receipt, busy) else {
            return false;
        };
        if self.pending.is_none() {
            self.pending = Some(StartupOutput {
                kind: StartupOutputKind::Pin,
                range: StartupOutputRange::NativeManualPreview {
                    receipt,
                    point: actual_point,
                },
            });
        }
        true
    }

    pub(crate) fn observe_input(&mut self, message: u32, key: usize, deferred: bool) {
        let editing = matches!(
            message,
            WM_LBUTTONDOWN
                | WM_LBUTTONDBLCLK
                | WM_RBUTTONDOWN
                | WM_RBUTTONDBLCLK
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
                | WM_CLOSE
        ) || matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
            && !crate::startup_surface_input::is_modifier(key as u16)
            || deferred && matches!(message, WM_CHAR | WM_SYSCHAR)
            || deferred
                && matches!(
                    message,
                    WM_MOUSEMOVE
                        | WM_MBUTTONDOWN
                        | WM_MBUTTONDBLCLK
                        | WM_MBUTTONUP
                        | WM_LBUTTONUP
                        | WM_RBUTTONUP
                )
                && key & (MK_LBUTTON.0 | MK_RBUTTON.0) as usize != 0;
        if editing {
            self.pending = None;
            self.preview_confirm_release_pending = false;
            if deferred {
                self.deferred_preview.invalidate();
                self.deferred_confirmed.invalidate();
                self.deferred_text.invalidate();
            }
        } else if deferred && matches!(message, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK) {
            // 新中键是动作边界，但不能把仍有效的初始空闲资格一并撤销。
            self.pending = None;
            self.preview_confirm_release_pending = false;
        }
    }
    pub(crate) fn observe_crop(&mut self, before: Option<Rect>, after: Option<Rect>) {
        if before != after {
            self.clear();
        }
    }
    pub(crate) fn clear(&mut self) {
        self.pending = None;
        self.preview_confirm_release_pending = false;
        // 原生重新框选可发生在 handoff 之前，尚未建立的资格由实际 handoff 状态决定。
        if self.deferred_preview.established {
            self.deferred_preview.invalidate();
        }
        if self.deferred_confirmed.established {
            self.deferred_confirmed.invalidate();
        }
        if self.deferred_text.established {
            self.deferred_text.invalidate();
        }
    }
    /// 成功交接最多消费一次；取消、失败和丢弃都只清理，不执行输出。
    pub(crate) fn finish(&mut self, successful: bool) -> Option<StartupOutput> {
        self.deferred_preview.invalidate();
        self.deferred_confirmed.invalidate();
        self.deferred_text.invalidate();
        self.preview_confirm_release_pending = false;
        self.pending.take().filter(|_| successful)
    }
}

/// 只核原生能够证明的空闲状态；不提交草稿，也不读取 provider/像素。
pub(crate) fn text_editor_ready(editor: &Editor, canvas: Rect, busy: bool) -> bool {
    !busy
        && canvas == Rect::full(canvas.width, canvas.height)
        && editor.canvas == canvas
        && canvas.valid(canvas.width, canvas.height)
        && crate::image_editor::screenshot_middle_pin_idle(editor, false, false)
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
        && editor
            .crop
            .is_none_or(|crop| crop.valid(canvas.width, canvas.height))
}

/// 立即预览：回执范围须与当前 Editor 的确认候选一致。
fn confirmed_preview_receipt(
    editor: &Editor,
    canvas: Rect,
    receipt: Option<PresentedWindowPreview>,
    busy: bool,
) -> Option<PresentedWindowPreview> {
    let receipt = receipt.filter(|receipt| receipt.serial != 0)?;
    (crate::preview_middle_pin::confirm_candidate(editor, canvas, Some(receipt.rect), busy, false)
        == Some(receipt.rect))
    .then_some(receipt)
}

#[cfg(test)]
mod startup_output_tests {
    use super::*;
    use crate::annotation::Tool;

    fn confirmed_middle_event() -> crate::startup_surface_input::CapturedInput {
        crate::startup_surface_input::CapturedInput::for_test(
            (WM_MBUTTONDOWN, 0x10, 480 | (250 << 16)),
            crate::geometry::ScreenRect {
                left: -1920,
                top: -240,
                width: 640,
                height: 480,
            },
            (3, 3),
            gpui::Modifiers::default(),
        )
    }

    #[test]
    fn deferred_confirmed_middle_keeps_receipt_skips_late_chrome_and_consumes_once() {
        let editor = editor();
        let original = editor.crop.unwrap();
        let canvas = editor.canvas;
        let revisions = (editor.document.revision(), editor.document.edit_revision());
        let receipt = PresentedWindowPreview {
            rect: original,
            serial: 31,
        };
        let mut event = confirmed_middle_event();
        event.set_presented_confirmed_selection(Some(receipt));
        let point = crate::startup_preview::client_point(event.raw.2);
        assert!(!original.contains(point));
        let mut queue = StartupOutputQueue::default();
        let mut stopped_native = self::editor();
        queue.begin_handoff_preview(&stopped_native, canvas, false);
        stopped_native.set_crop(Some(Rect::full(640, 480)));
        assert!(queue.offer_deferred_confirmed(
            canvas,
            event.presented_confirmed_selection(),
            point,
            false
        ));
        event.consume_native_preview_middle();
        let crate::startup_surface_input::ReplayInput::Event(replay) =
            event.clone().into_replay(1.25).unwrap()
        else {
            panic!("应保留原始 Middle 输入");
        };
        // Handoff 先核此位、再 dispatch_event；晚出现的 Chrome 没有解释这个 raw Down 的机会。
        assert!(replay.native_preview_middle_consumed());
        let gpui::PlatformInput::MouseDown(down) = replay.event else {
            panic!("不是原始 MiddleDown");
        };
        assert_eq!(down.button, gpui::MouseButton::Middle);
        assert_eq!(f32::from(down.position.x) * 1.25, point.0 as f32);
        assert_eq!(f32::from(down.position.y) * 1.25, point.1 as f32);
        let output = queue.finish(true).unwrap();
        assert_eq!(output.native_confirmed(), Some((original, point)));
        assert!(output.accepts(editor.crop, true));
        assert!(!output.accepts(stopped_native.crop, true));
        assert!(!output.accepts(editor.crop, false));
        assert!(!output.accepts(Some(Rect::full(640, 480)), true));
        assert_eq!(output.preview(), None);
        assert_eq!(output.manual_preview(), None);
        assert_eq!(output.preview_confirm(), None);
        assert!(queue.finish(true).is_none());
        assert_eq!(editor.crop, Some(original));
        assert_eq!(
            (editor.document.revision(), editor.document.edit_revision()),
            revisions
        );
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn deferred_confirmed_middle_edit_crop_aba_and_failed_transfer_never_restore_raw_output() {
        let mut editor = editor();
        let original = editor.crop.unwrap();
        let canvas = editor.canvas;
        let receipt = PresentedWindowPreview {
            rect: original,
            serial: 31,
        };
        for (message, key) in [
            (WM_LBUTTONDOWN, 1),
            (WM_RBUTTONDOWN, 2),
            (WM_MOUSEWHEEL, 0),
            (WM_KEYDOWN, 0x41),
            (WM_KEYUP, 0x41),
            (WM_MOUSEMOVE, MK_LBUTTON.0 as usize),
            (WM_CLOSE, 0),
        ] {
            for after_middle in [false, true] {
                let mut queue = StartupOutputQueue::default();
                queue.begin_handoff_preview(&editor, canvas, false);
                let mut event = confirmed_middle_event();
                event.set_presented_confirmed_selection(Some(receipt));
                if after_middle {
                    assert!(queue.offer_deferred_confirmed(
                        canvas,
                        event.presented_confirmed_selection(),
                        (480, 250),
                        false
                    ));
                    event.consume_native_preview_middle();
                }
                queue.observe_input(message, key, true);
                queue.begin_handoff_preview(&editor, canvas, false);
                assert!(queue.owns_deferred_confirmed_middle(canvas, Some(receipt)));
                assert!(!queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
                assert!(queue.finish(true).is_none());
                let crate::startup_surface_input::ReplayInput::Event(replay) =
                    event.into_replay(1.).unwrap()
                else {
                    panic!("不是 Middle");
                };
                assert_eq!(replay.native_preview_middle_consumed(), after_middle);
            }
        }
        let mut queue = StartupOutputQueue::default();
        queue.begin_handoff_preview(&editor, canvas, false);
        let mut event = confirmed_middle_event();
        event.set_presented_confirmed_selection(Some(receipt));
        assert!(queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
        event.consume_native_preview_middle();
        let other = Rect::full(640, 480);
        editor.set_crop(Some(other));
        queue.observe_crop(Some(original), editor.crop);
        editor.set_crop(Some(original));
        queue.observe_crop(Some(other), editor.crop);
        queue.begin_handoff_preview(&editor, canvas, false);
        assert!(queue.owns_deferred_confirmed_middle(canvas, Some(receipt)));
        assert!(!queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
        assert!(queue.finish(true).is_none());
        let crate::startup_surface_input::ReplayInput::Event(replay) =
            event.into_replay(1.).unwrap()
        else {
            panic!("不是 Middle");
        };
        assert!(replay.native_preview_middle_consumed());
        for cancelled in [false, true] {
            let mut queue = StartupOutputQueue::default();
            queue.begin_handoff_preview(&editor, canvas, false);
            assert!(queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
            if cancelled {
                queue.clear();
            }
            assert!(queue.finish(false).is_none());
            queue.begin_handoff_preview(&editor, canvas, false);
            assert!(queue.owns_deferred_confirmed_middle(canvas, Some(receipt)));
            assert!(!queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
            assert!(queue.finish(true).is_none());
        }
    }

    #[test]
    fn deferred_confirmed_middle_rejects_missing_frames_busy_and_ineligible_initial_state() {
        let editor = editor();
        let canvas = editor.canvas;
        let receipt = PresentedWindowPreview {
            rect: editor.crop.unwrap(),
            serial: 31,
        };
        for (frame, busy) in [
            (None, false),
            (
                Some(PresentedWindowPreview {
                    serial: 0,
                    ..receipt
                }),
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect::full(640, 480),
                    ..receipt
                }),
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect {
                        x: 639,
                        y: 0,
                        width: 2,
                        height: 1,
                    },
                    ..receipt
                }),
                false,
            ),
            (Some(receipt), true),
        ] {
            let mut queue = StartupOutputQueue::default();
            queue.begin_handoff_preview(&editor, canvas, false);
            assert_eq!(queue.owns_deferred_confirmed_middle(canvas, frame), busy);
            assert!(!queue.offer_deferred_confirmed(canvas, frame, (480, 250), busy));
            assert!(queue.finish(true).is_none());
        }
        for initial in 0..6 {
            let mut unavailable = self::editor();
            let initial_busy = initial == 0;
            let initial_canvas = if initial == 5 {
                Rect::full(320, 240)
            } else {
                canvas
            };
            match initial {
                1 => unavailable.set_crop(None),
                2 => unavailable.pointer_down((40, 50), 6.),
                3 => {
                    unavailable.select_tool(Tool::Polyline);
                    unavailable.pointer_down((40, 50), 6.);
                    unavailable.pointer_up((60, 70));
                }
                4 => {
                    unavailable.select_tool(Tool::Text);
                    unavailable.begin_text(None, crate::annotation::Point { x: 40., y: 50. });
                }
                _ => {}
            }
            let mut queue = StartupOutputQueue::default();
            queue.begin_handoff_preview(&unavailable, initial_canvas, initial_busy);
            queue.begin_handoff_preview(&editor, canvas, false);
            assert!(!queue.owns_deferred_confirmed_middle(canvas, Some(receipt)));
            assert!(!queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
            assert!(queue.finish(true).is_none());
        }
    }

    #[test]
    fn deferred_confirmed_middle_replaces_new_events_once_and_excludes_preview_routes() {
        let editor = editor();
        let canvas = editor.canvas;
        let receipt = PresentedWindowPreview {
            rect: editor.crop.unwrap(),
            serial: 31,
        };
        let mut queue = StartupOutputQueue::default();
        queue.begin_handoff_preview(&editor, canvas, false);
        assert!(!queue.offer_deferred_preview(canvas, Some(receipt), (40, 50), false));
        assert!(!queue.offer_deferred_manual_preview(canvas, Some(receipt), (480, 250), false));
        assert!(!queue.offer_deferred_preview_confirm(canvas, Some(receipt), false));
        assert!(queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
        for message in [WM_MBUTTONDOWN, WM_MBUTTONDBLCLK] {
            queue.observe_input(message, 0x10, true);
            assert!(queue.pending.is_none());
            assert!(queue.offer_deferred_confirmed(
                canvas,
                Some(PresentedWindowPreview {
                    serial: 32,
                    ..receipt
                }),
                (500, 260),
                false
            ));
        }
        let output = queue.finish(true).unwrap();
        assert_eq!(output.native_confirmed(), Some((receipt.rect, (500, 260))));
        assert_eq!(
            output.range,
            StartupOutputRange::NativeConfirmed {
                receipt: PresentedWindowPreview {
                    serial: 32,
                    ..receipt
                },
                point: (500, 260),
            }
        );
        assert!(queue.finish(true).is_none());
        assert!(!queue.offer_deferred_confirmed(canvas, Some(receipt), (480, 250), false));
        for manual in [false, true] {
            let mut preview = StartupOutputQueue::default();
            preview.begin_handoff_preview(&preview_editor(), canvas, false);
            assert!(!preview.owns_deferred_confirmed_middle(canvas, Some(receipt)));
            assert!(!preview.offer_deferred_confirmed(canvas, Some(receipt), (40, 50), false));
            let accepted = if manual {
                preview.offer_deferred_manual_preview(canvas, Some(receipt), (480, 250), false)
            } else {
                preview.offer_deferred_preview(canvas, Some(receipt), (40, 50), false)
            };
            assert!(accepted);
            let output = preview.finish(true).unwrap();
            assert_eq!(output.native_confirmed(), None);
            assert!(!output.accepts(Some(receipt.rect), true));
        }
    }

    #[test]
    fn confirmed_middle_event_metadata_stays_separate_from_preview_and_later_frames() {
        let receipt = PresentedWindowPreview {
            rect: editor().crop.unwrap(),
            serial: 31,
        };
        let mut event = confirmed_middle_event();
        event.set_presented_window_preview(Some(preview_receipt()));
        assert_eq!(event.presented_confirmed_selection(), None);
        event.set_presented_confirmed_selection(Some(receipt));
        let saved = event.clone();
        event.set_presented_confirmed_selection(Some(PresentedWindowPreview {
            rect: Rect::full(640, 480),
            serial: 32,
        }));
        event.set_presented_window_preview(None);
        assert_eq!(saved.presented_confirmed_selection(), Some(receipt));
        assert_eq!(saved.presented_window_preview(), Some(preview_receipt()));
        assert_eq!(saved.raw, event.raw);
        assert_eq!(saved.received_at, event.received_at);
        let canvas = Rect::full(640, 480);
        let mut preview = StartupOutputQueue::default();
        preview.begin_handoff_preview(&preview_editor(), canvas, false);
        let mut confirmed_only = confirmed_middle_event();
        confirmed_only.set_presented_confirmed_selection(Some(receipt));
        assert!(!preview.offer_deferred_preview(
            canvas,
            confirmed_only.presented_window_preview(),
            (40, 50),
            false
        ));
        assert!(!preview.offer_deferred_confirmed(
            canvas,
            confirmed_only.presented_confirmed_selection(),
            (40, 50),
            false
        ));
        assert!(preview.finish(true).is_none());
        for message in [WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_MOUSEMOVE, WM_KEYDOWN] {
            let mut other = confirmed_middle_event();
            other.raw.0 = message;
            other.set_presented_confirmed_selection(Some(receipt));
            assert_eq!(other.presented_confirmed_selection(), None);
        }
    }

    #[test]
    fn rejected_manual_preview_consumes_key_cycle_without_creating_an_output() {
        let mut queue = StartupOutputQueue::default();
        queue.consume_preview_confirm_cycle();
        assert!(!queue.take_preview_confirm_release(WM_KEYUP, 0x41));
        assert!(queue.take_preview_confirm_release(WM_KEYUP, 0x0d));
        assert!(!queue.take_preview_confirm_release(WM_KEYUP, 0x0d));
        assert!(queue.finish(true).is_none());
        for (shortcut, key) in [
            (crate::startup_surface_input::TextShortcut::Region, 0x52),
            (crate::startup_surface_input::TextShortcut::Control, 0x43),
        ] {
            queue.consume_text_cycle(shortcut);
            assert!(queue.take_text_cycle(WM_KEYDOWN, key, 1 << 30));
            assert!(queue.take_text_cycle(WM_KEYUP, key, 0));
            assert!(!queue.take_text_cycle(WM_KEYUP, key, 0));
            assert!(queue.finish(true).is_none());
        }
    }

    #[test]
    fn manual_preview_output_keeps_outside_event_point_and_distinct_range_kind() {
        let editor = preview_editor();
        let receipt = preview_receipt();
        let point = (0, 0);
        assert!(!receipt.rect.contains(point));
        let mut automatic = StartupOutputQueue::default();
        assert!(automatic.offer_preview(&editor, editor.canvas, Some(receipt), point, false));
        assert_eq!(
            automatic.finish(true).unwrap().preview(),
            Some((receipt.rect, point))
        );
        let mut queue = StartupOutputQueue::default();
        assert!(queue.offer_manual_preview(&editor, editor.canvas, Some(receipt), point, false));
        let output = queue.finish(true).unwrap();
        assert_eq!(output.manual_preview(), Some((receipt.rect, point)));
        assert_eq!(output.preview(), None);
        assert_eq!(output.preview_confirm(), None);
        assert!(!output.accepts(Some(receipt.rect), true));
        assert_eq!(
            StartupOutput {
                kind: StartupOutputKind::Confirm,
                ..output
            }
            .manual_preview(),
            None
        );
        assert!(queue.finish(true).is_none());
        assert!(!queue.offer_manual_preview(&editor, editor.canvas, Some(receipt), point, true));
        assert!(!queue.offer_manual_preview(
            &editor,
            editor.canvas,
            Some(PresentedWindowPreview {
                serial: 0,
                ..receipt
            }),
            point,
            false
        ));
        let mut confirmed = preview_editor();
        confirmed.set_crop(Some(receipt.rect));
        assert!(!queue.offer_manual_preview(
            &confirmed,
            confirmed.canvas,
            Some(receipt),
            point,
            false
        ));
    }

    #[test]
    fn deferred_manual_preview_cannot_reopen_after_snap_reset_or_editing() {
        let editor = preview_editor();
        for (message, key) in [
            (WM_KEYDOWN, 0x41),
            (WM_RBUTTONDOWN, 2),
            (WM_LBUTTONDOWN, 1),
            (WM_MOUSEWHEEL, 0),
        ] {
            let mut queue = StartupOutputQueue::default();
            queue.begin_handoff_preview(&editor, editor.canvas, false);
            assert!(queue.offer_deferred_manual_preview(
                editor.canvas,
                Some(preview_receipt()),
                (0, 0),
                false
            ));
            queue.observe_input(message, key, true);
            queue.begin_handoff_preview(&editor, editor.canvas, false);
            assert!(!queue.offer_deferred_manual_preview(
                editor.canvas,
                Some(preview_receipt()),
                (0, 0),
                false
            ));
            assert!(queue.finish(true).is_none());
        }
        let mut queue = StartupOutputQueue::default();
        assert!(!queue.offer_deferred_manual_preview(
            editor.canvas,
            Some(preview_receipt()),
            (0, 0),
            false
        ));
        queue.begin_handoff_preview(&editor, editor.canvas, false);
        assert!(!queue.offer_deferred_manual_preview(
            editor.canvas,
            Some(preview_receipt()),
            (0, 0),
            true
        ));
        assert!(!queue.offer_deferred_manual_preview(
            editor.canvas,
            Some(PresentedWindowPreview {
                serial: 0,
                ..preview_receipt()
            }),
            (0, 0),
            false
        ));
        assert!(queue.offer_deferred_manual_preview(
            editor.canvas,
            Some(preview_receipt()),
            (0, 0),
            false
        ));
        let output = queue.finish(true).unwrap();
        assert_eq!(
            output.manual_preview(),
            Some((preview_receipt().rect, (0, 0)))
        );
        assert!(queue.finish(true).is_none());
    }

    #[test]
    fn preview_confirm_is_distinct_from_pin_and_consumes_one_release_and_output() {
        let editor = preview_editor();
        let mut queue = StartupOutputQueue::default();
        queue.observe_preview_confirm_down();
        assert!(queue.offer_preview_confirm(
            &editor,
            editor.canvas,
            Some(preview_receipt()),
            false
        ));
        assert!(!queue.take_preview_confirm_release(WM_KEYUP, 0x41));
        assert!(queue.take_preview_confirm_release(WM_KEYUP, 0x0d));
        assert!(!queue.take_preview_confirm_release(WM_KEYUP, 0x0d));
        let output = queue.finish(true).unwrap();
        assert_eq!(output.preview_confirm(), Some(preview_receipt().rect));
        assert_eq!(output.preview(), None);
        assert!(!output.accepts(None, true));
        assert!(!output.accepts(Some(preview_receipt().rect), true));
        assert_eq!(
            StartupOutput {
                kind: StartupOutputKind::Pin,
                ..output
            }
            .preview_confirm(),
            None
        );
        assert!(queue.finish(true).is_none());
        assert!(queue.offer_preview_confirm(
            &editor,
            editor.canvas,
            Some(preview_receipt()),
            false
        ));
        // Down 后 Alt 状态变化产生 SysKeyUp，仍是该已接受周期的一次释放。
        assert!(queue.take_preview_confirm_release(WM_SYSKEYUP, 0x0d));
        assert!(!queue.take_preview_confirm_release(WM_SYSKEYUP, 0x0d));
        assert!(queue.finish(false).is_none());
    }
    fn editor() -> Editor {
        Editor::new(
            Rect::full(640, 480),
            Some(Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 80,
            }),
        )
    }
    fn offer(queue: &mut StartupOutputQueue, editor: &Editor) -> bool {
        queue.offer(StartupOutputKind::Pin, editor, Rect::full(640, 480), false)
    }

    #[test]
    fn early_output_preserves_original_crop_and_consumes_once() {
        let editor = editor();
        let mut queue = StartupOutputQueue::default();
        assert!(offer(&mut queue, &editor));
        assert!(offer(&mut queue, &editor));
        let output = queue.finish(true).unwrap();
        assert_eq!(output.kind, StartupOutputKind::Pin);
        assert_eq!(
            output.range,
            StartupOutputRange::Confirmed(editor.crop.unwrap())
        );
        assert!(queue.finish(true).is_none());
        assert!(output.accepts(editor.crop, true));
        assert!(!output.accepts(None, true));
        assert!(!output.accepts(editor.crop, false));
    }
    #[test]
    fn early_output_rejects_absent_invalid_busy_and_unfinished_content() {
        let mut editor = editor();
        let mut queue = StartupOutputQueue::default();
        assert!(!queue.offer(StartupOutputKind::Pin, &editor, Rect::full(640, 480), true));
        editor.set_crop(None);
        assert!(!offer(&mut queue, &editor));
        editor.set_crop(Some(Rect {
            x: 639,
            y: 0,
            width: 2,
            height: 1,
        }));
        assert!(!offer(&mut queue, &editor));
        editor.set_crop(Some(Rect::full(640, 480)));
        editor.pointer_down((100, 100), 6.);
        assert!(!offer(&mut queue, &editor));
        editor.cancel_gesture();
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((100, 100), 6.);
        editor.pointer_up((100, 100));
        assert!(!offer(&mut queue, &editor));
        assert!(queue.finish(true).is_none());
    }
    #[test]
    fn early_output_native_crop_aba_does_not_restore_old_request() {
        let mut editor = editor();
        let original = editor.crop;
        let mut queue = StartupOutputQueue::default();
        assert!(offer(&mut queue, &editor));
        editor.set_crop(Some(Rect::full(640, 480)));
        queue.observe_crop(original, editor.crop);
        let other = editor.crop;
        editor.set_crop(original);
        queue.observe_crop(other, editor.crop);
        assert!(queue.finish(true).is_none());
    }
    #[test]
    fn early_output_deferred_selection_aba_is_invalidated_before_replay() {
        let mut editor = editor();
        let original = editor.crop;
        let mut queue = StartupOutputQueue::default();
        assert!(offer(&mut queue, &editor));
        queue.observe_input(WM_LBUTTONDOWN, 1, true);
        editor.set_crop(Some(Rect::full(640, 480)));
        queue.observe_input(WM_MOUSEMOVE, 1, true);
        editor.set_crop(original);
        queue.observe_input(WM_MOUSEMOVE, 1, true);
        queue.observe_input(WM_LBUTTONUP, 0, true);
        assert_eq!(editor.crop, original);
        assert!(queue.finish(true).is_none());
    }
    #[test]
    fn early_output_editing_and_cancel_boundaries_do_not_restore_requests() {
        for deferred in [false, true] {
            for (message, key) in [
                (WM_RBUTTONDOWN, 2),
                (WM_RBUTTONDBLCLK, 2),
                (WM_LBUTTONDBLCLK, 1),
                (WM_MOUSEWHEEL, 0),
                (WM_MOUSEHWHEEL, 0),
                (WM_KEYDOWN, 0x1b),
                (WM_SYSKEYDOWN, 0x73),
                (WM_KEYUP, 0x41),
                (WM_CLOSE, 0),
            ] {
                let mut queue = StartupOutputQueue::default();
                assert!(offer(&mut queue, &editor()));
                queue.observe_input(message, key, deferred);
                assert!(
                    queue.finish(true).is_none(),
                    "message={message} deferred={deferred}"
                );
            }
        }
    }
    #[test]
    fn early_output_hover_modifiers_and_idle_capture_loss_keep_the_request() {
        let mut queue = StartupOutputQueue::default();
        assert!(offer(&mut queue, &editor()));
        for (message, key) in [
            (WM_MOUSEMOVE, 0),
            (WM_KEYDOWN, 0x11),
            (WM_KEYUP, 0x11),
            (WM_SYSKEYDOWN, 0x12),
            (WM_SYSKEYUP, 0x12),
            (WM_CAPTURECHANGED, 0),
        ] {
            queue.observe_input(message, key, true);
        }
        assert!(queue.finish(true).is_some());
    }
    #[test]
    fn early_output_new_deferred_middle_replaces_old_request_in_normal_input_order() {
        for message in [WM_MBUTTONDOWN, WM_MBUTTONDBLCLK] {
            let mut queue = StartupOutputQueue::default();
            assert!(offer(&mut queue, &editor()));
            queue.observe_input(message, 0x10, false);
            assert!(queue.pending.is_some());
            queue.observe_input(message, 0x10, true);
            assert!(queue.finish(true).is_none());
        }
    }
    #[test]
    fn early_output_cancelled_or_failed_completion_never_executes() {
        let mut queue = StartupOutputQueue::default();
        assert!(offer(&mut queue, &editor()));
        assert!(queue.finish(false).is_none());
        assert!(queue.finish(true).is_none());
        assert!(queue.offer(
            StartupOutputKind::Confirm,
            &editor(),
            Rect::full(640, 480),
            false
        ));
        assert_eq!(queue.finish(true).unwrap().kind, StartupOutputKind::Confirm);
        assert!(queue.finish(true).is_none());
    }

    fn preview_receipt() -> PresentedWindowPreview {
        PresentedWindowPreview {
            rect: Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 80,
            },
            serial: 42,
        }
    }
    fn preview_editor() -> Editor {
        Editor::new(Rect::full(640, 480), None)
    }
    fn offer_preview(queue: &mut StartupOutputQueue) -> bool {
        queue.offer_preview(
            &preview_editor(),
            Rect::full(640, 480),
            Some(preview_receipt()),
            (40, 50),
            false,
        )
    }

    #[test]
    fn native_preview_preserves_the_middle_receipt_through_hover_and_consumes_once() {
        let mut queue = StartupOutputQueue::default();
        assert!(offer_preview(&mut queue));
        assert!(offer_preview(&mut queue));
        for (message, key) in [
            (WM_MOUSEMOVE, 0),
            (WM_KEYDOWN, 0x11),
            (WM_KEYUP, 0x11),
            (WM_CAPTURECHANGED, 0),
        ] {
            queue.observe_input(message, key, true);
        }
        let output = queue.finish(true).unwrap();
        assert_eq!(
            output.range,
            StartupOutputRange::NativePreview {
                receipt: preview_receipt(),
                point: (40, 50)
            }
        );
        assert_eq!(output.preview(), Some((preview_receipt().rect, (40, 50))));
        assert!(!output.accepts(None, true));
        assert!(!output.accepts(Some(preview_receipt().rect), true));
        assert!(queue.finish(true).is_none());
    }

    #[test]
    fn native_preview_requires_valid_receipt_and_idle_editor_and_keeps_outside_event_point() {
        let editor = preview_editor();
        let canvas = editor.canvas;
        for (receipt, point, busy) in [
            (None, (40, 50), false),
            (
                Some(PresentedWindowPreview {
                    serial: 0,
                    ..preview_receipt()
                }),
                (40, 50),
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect {
                        width: 0,
                        ..preview_receipt().rect
                    },
                    ..preview_receipt()
                }),
                (40, 50),
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect {
                        x: u32::MAX,
                        ..preview_receipt().rect
                    },
                    ..preview_receipt()
                }),
                (40, 50),
                false,
            ),
            (Some(preview_receipt()), (40, 50), true),
        ] {
            let mut queue = StartupOutputQueue::default();
            assert!(!queue.offer_preview(&editor, canvas, receipt, point, busy));
            assert!(queue.finish(true).is_none());
        }
        // 原点命中拒绝改为接受实际已呈现范围；回执/范围/空闲拒绝条件保持。
        for point in [(-1, 50), (120, 50)] {
            let mut queue = StartupOutputQueue::default();
            assert!(queue.offer_preview(&editor, canvas, Some(preview_receipt()), point, false));
            assert_eq!(
                queue.finish(true).unwrap().preview(),
                Some((preview_receipt().rect, point))
            );
            assert!(queue.finish(true).is_none());
        }
        let selected = Editor::new(canvas, Some(preview_receipt().rect));
        let mut queue = StartupOutputQueue::default();
        assert!(!queue.offer_preview(&selected, canvas, Some(preview_receipt()), (40, 50), false));
    }

    #[test]
    fn native_preview_edit_crop_aba_and_new_deferred_middle_never_restore_old_output() {
        let mut crop_aba = StartupOutputQueue::default();
        assert!(offer_preview(&mut crop_aba));
        crop_aba.observe_crop(None, Some(preview_receipt().rect));
        crop_aba.observe_crop(Some(preview_receipt().rect), None);
        assert!(crop_aba.finish(true).is_none());
        for deferred in [false, true] {
            for (message, key) in [
                (WM_LBUTTONDOWN, 1),
                (WM_LBUTTONDBLCLK, 1),
                (WM_RBUTTONDOWN, 2),
                (WM_MOUSEWHEEL, 0),
                (WM_MOUSEHWHEEL, 0),
                (WM_KEYDOWN, 0x41),
                (WM_KEYUP, 0x41),
                (WM_CLOSE, 0),
            ] {
                let mut queue = StartupOutputQueue::default();
                assert!(offer_preview(&mut queue));
                queue.observe_input(message, key, deferred);
                queue.observe_crop(None, Some(preview_receipt().rect));
                queue.observe_crop(Some(preview_receipt().rect), None);
                assert!(queue.finish(true).is_none());
            }
        }
        let mut queue = StartupOutputQueue::default();
        assert!(offer_preview(&mut queue));
        queue.observe_input(WM_MBUTTONDOWN, 0x10, true);
        assert!(queue.finish(true).is_none());
    }

    #[test]
    fn native_preview_cancelled_failed_and_confirm_kind_do_not_execute() {
        let mut queue = StartupOutputQueue::default();
        assert!(offer_preview(&mut queue));
        assert!(queue.finish(false).is_none());
        assert!(queue.finish(true).is_none());
        let wrong_kind = StartupOutput {
            kind: StartupOutputKind::Confirm,
            range: StartupOutputRange::NativePreview {
                receipt: preview_receipt(),
                point: (40, 50),
            },
        };
        assert!(wrong_kind.preview().is_none());
        assert!(!wrong_kind.accepts(None, true));
        assert!(!wrong_kind.accepts(Some(preview_receipt().rect), true));
        let confirmed = StartupOutput {
            kind: StartupOutputKind::Pin,
            range: StartupOutputRange::Confirmed(preview_receipt().rect),
        };
        assert!(confirmed.preview().is_none());
        assert!(confirmed.accepts(Some(preview_receipt().rect), true));
    }

    fn deferred_queue() -> StartupOutputQueue {
        let editor = preview_editor();
        let mut queue = StartupOutputQueue::default();
        queue.begin_handoff_preview(&editor, editor.canvas, false);
        queue
    }
    fn offer_deferred(queue: &mut StartupOutputQueue) -> bool {
        queue.observe_input(WM_MBUTTONDOWN, 0x10, true);
        queue.offer_deferred_preview(
            Rect::full(640, 480),
            Some(preview_receipt()),
            (40, 50),
            false,
        )
    }

    #[test]
    fn deferred_preview_initial_ineligible_or_busy_handoff_cannot_be_rearmed() {
        for (initial, busy) in [(editor(), false), (preview_editor(), true)] {
            let mut queue = StartupOutputQueue::default();
            queue.begin_handoff_preview(&initial, initial.canvas, busy);
            let idle = preview_editor();
            queue.begin_handoff_preview(&idle, idle.canvas, false);
            assert!(!offer_deferred(&mut queue));
            assert!(queue.finish(true).is_none());
        }
        // handoff 前已完成的原生重选，由实际初始空闲状态建立资格。
        let mut queue = StartupOutputQueue::default();
        queue.clear();
        queue.begin_handoff_preview(&preview_editor(), Rect::full(640, 480), false);
        assert!(offer_deferred(&mut queue));
    }

    #[test]
    fn deferred_preview_new_middle_replaces_only_output_and_keeps_live_fence() {
        let mut queue = deferred_queue();
        assert!(offer_deferred(&mut queue));
        let later = PresentedWindowPreview {
            rect: Rect {
                x: 200,
                y: 100,
                width: 80,
                height: 60,
            },
            serial: 43,
        };
        queue.observe_input(WM_MBUTTONDBLCLK, 0x10, true);
        assert!(queue.offer_deferred_preview(Rect::full(640, 480), Some(later), (210, 110), false));
        for (message, key) in [
            (WM_MOUSEMOVE, 0),
            (WM_MOUSEMOVE, 0x10),
            (WM_KEYDOWN, 0x11),
            (WM_KEYUP, 0x11),
            (WM_LBUTTONUP, 0),
            (WM_CAPTURECHANGED, 0),
        ] {
            queue.observe_input(message, key, true);
        }
        assert_eq!(
            queue.finish(true).unwrap().preview(),
            Some((later.rect, (210, 110)))
        );
        assert!(queue.finish(true).is_none());
        queue.begin_handoff_preview(&preview_editor(), Rect::full(640, 480), false);
        assert!(!offer_deferred(&mut queue));
    }

    #[test]
    fn deferred_preview_edit_before_or_after_middle_stays_invalid_through_aba_and_repeat_handoff() {
        for after_middle in [false, true] {
            let mut queue = deferred_queue();
            if after_middle {
                assert!(offer_deferred(&mut queue));
            }
            queue.observe_input(WM_LBUTTONDOWN, 1, true);
            queue.observe_crop(None, Some(preview_receipt().rect));
            queue.observe_crop(Some(preview_receipt().rect), None);
            queue.observe_input(WM_LBUTTONUP, 0, true);
            queue.begin_handoff_preview(&preview_editor(), Rect::full(640, 480), false);
            assert!(!offer_deferred(&mut queue));
            assert!(queue.finish(true).is_none());
        }
    }

    #[test]
    fn deferred_preview_unpaired_edit_inputs_and_coalesced_button_history_invalidate_fence() {
        for (message, key) in [
            (WM_MOUSEMOVE, MK_LBUTTON.0 as usize),
            (WM_MOUSEMOVE, MK_RBUTTON.0 as usize),
            (WM_MBUTTONDOWN, (MK_LBUTTON.0 | 0x10) as usize),
            (WM_LBUTTONUP, MK_RBUTTON.0 as usize),
            (WM_CHAR, 0x41),
            (WM_SYSCHAR, 0x41),
            (WM_KEYUP, 0x41),
            (WM_SYSKEYUP, 0x41),
            (WM_MOUSEWHEEL, 0),
            (WM_MOUSEHWHEEL, 0),
            (WM_CLOSE, 0),
        ] {
            let mut queue = deferred_queue();
            assert!(offer_deferred(&mut queue));
            queue.observe_input(message, key, true);
            assert!(!offer_deferred(&mut queue), "message={message}");
            assert!(queue.finish(true).is_none());
        }
        let mut queue = deferred_queue();
        assert!(offer_deferred(&mut queue));
        queue.invalidate_deferred_preview();
        queue.observe_input(WM_MOUSEMOVE, 0, true);
        assert!(!offer_deferred(&mut queue));
        assert!(queue.finish(true).is_none());
    }

    #[test]
    fn deferred_preview_rejected_receipt_or_busy_cannot_reuse_old_middle_and_point_is_metadata() {
        for (receipt, point, busy) in [
            (None, (40, 50), false),
            (
                Some(PresentedWindowPreview {
                    serial: 0,
                    ..preview_receipt()
                }),
                (40, 50),
                false,
            ),
            (Some(preview_receipt()), (40, 50), true),
        ] {
            let mut queue = deferred_queue();
            assert!(offer_deferred(&mut queue));
            queue.observe_input(WM_MBUTTONDOWN, 0x10, true);
            assert!(!queue.offer_deferred_preview(Rect::full(640, 480), receipt, point, busy));
            assert!(queue.finish(true).is_none());
        }
        for point in [(-1, 50), (120, 50)] {
            let mut queue = deferred_queue();
            assert!(offer_deferred(&mut queue));
            queue.observe_input(WM_MBUTTONDOWN, 0x10, true);
            assert!(queue.offer_deferred_preview(
                Rect::full(640, 480),
                Some(preview_receipt()),
                point,
                false
            ));
            assert_eq!(
                queue.finish(true).unwrap().preview(),
                Some((preview_receipt().rect, point))
            );
            assert!(queue.finish(true).is_none());
        }
    }

    #[test]
    fn deferred_preview_crop_change_cancel_and_failed_finish_cannot_execute_or_reopen() {
        let mut crop_aba = deferred_queue();
        assert!(offer_deferred(&mut crop_aba));
        crop_aba.observe_crop(None, Some(preview_receipt().rect));
        crop_aba.observe_crop(Some(preview_receipt().rect), None);
        assert!(!offer_deferred(&mut crop_aba));
        for cancelled in [false, true] {
            let mut queue = deferred_queue();
            assert!(offer_deferred(&mut queue));
            if cancelled {
                queue.clear();
            }
            assert!(queue.finish(false).is_none());
            queue.begin_handoff_preview(&preview_editor(), Rect::full(640, 480), false);
            assert!(!offer_deferred(&mut queue));
            assert!(queue.finish(true).is_none());
        }
    }
}

#[cfg(test)]
mod startup_text_roi_tests {
    use super::*;
    use crate::startup_surface_input::TextShortcut;

    fn crop() -> Rect {
        Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        }
    }
    fn receipt() -> PresentedWindowPreview {
        PresentedWindowPreview {
            rect: crop(),
            serial: 7,
        }
    }

    #[test]
    fn region_and_control_text_keep_event_range_and_distinct_pixel_routes() {
        let canvas = Rect::full(640, 480);
        let mut editor = Editor::new(canvas, Some(crop()));
        let mut queue = StartupOutputQueue::default();
        assert!(queue.offer_text(&editor, canvas, TextShortcut::Region, None, false));
        assert_eq!(
            queue.finish(true).unwrap().text(),
            Some((StartupOutputKind::Ocr, crop(), Some(crop())))
        );
        assert!(queue.offer_text(&editor, canvas, TextShortcut::Control, None, false));
        assert_eq!(
            queue.finish(true).unwrap().text(),
            Some((StartupOutputKind::ControlText, crop(), Some(crop())))
        );
        editor.set_crop(None);
        assert!(queue.offer_text(
            &editor,
            canvas,
            TextShortcut::Region,
            Some(receipt()),
            false
        ));
        // 后来 hover/绘制 B 不会改已经保存的 A；纯 hover 不属于编辑边界。
        queue.observe_input(WM_MOUSEMOVE, 0, false);
        let output = queue.finish(true).unwrap();
        assert_eq!(
            output.text(),
            Some((StartupOutputKind::ControlText, crop(), None))
        );
        assert!(!output.accepts(Some(crop()), true));
        assert!(queue.finish(true).is_none());
        assert!(!queue.offer_text(&editor, canvas, TextShortcut::Region, None, false));
        assert!(!queue.offer_text(
            &editor,
            canvas,
            TextShortcut::Control,
            Some(receipt()),
            true
        ));
    }

    #[test]
    fn deferred_text_crop_aba_cancel_and_new_physical_cycle_never_restore_request() {
        let canvas = Rect::full(640, 480);
        let mut editor = Editor::new(canvas, Some(crop()));
        let mut queue = StartupOutputQueue::default();
        queue.begin_handoff_preview(&editor, canvas, false);
        assert!(queue.offer_deferred_text(canvas, TextShortcut::Region, None, false));
        queue.observe_input(WM_LBUTTONDOWN, 1, true);
        editor.set_crop(Some(canvas));
        editor.set_crop(Some(crop()));
        // 重复 handoff / 最终相同 crop 都不能重开旧 fence。
        queue.begin_handoff_preview(&editor, canvas, false);
        assert!(!queue.offer_deferred_text(canvas, TextShortcut::Region, None, false));
        assert!(queue.take_text_cycle(WM_KEYDOWN, 0x52, 1 << 30));
        assert!(queue.take_text_cycle(WM_SYSKEYUP, 0x52, 0));
        assert!(!queue.take_text_cycle(WM_KEYUP, 0x52, 0));
        assert!(queue.finish(true).is_none());
        let mut queue = StartupOutputQueue::default();
        assert!(queue.offer_text(&editor, canvas, TextShortcut::Control, None, false));
        queue.clear();
        assert!(queue.take_text_cycle(WM_KEYDOWN, 0x43, 1 << 30));
        // 新的系统 Down 不是旧已接受周期；拒绝后也不能领其 Up。
        assert!(!queue.take_text_cycle(WM_SYSKEYDOWN, 0x43, 0));
        assert!(!queue.take_text_cycle(WM_SYSKEYUP, 0x43, 0));
        assert!(queue.finish(false).is_none());
        let mut queue = StartupOutputQueue::default();
        queue.begin_handoff_preview(&editor, canvas, false);
        assert!(queue.offer_deferred_text(canvas, TextShortcut::Region, None, false));
        queue.invalidate_text();
        assert!(queue.finish(true).is_none());
        assert!(queue.take_text_cycle(WM_KEYUP, 0x52, 0));
    }
}
