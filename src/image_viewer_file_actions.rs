//! 源文件拖动只携带路径；捕获和请求拥有者在窗口线程释放。
use super::*;
use crate::pin_drag_request::NativeDragRequest;
use windows::Win32::{
    Foundation::HWND,
    UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
};

pub(super) struct Gesture {
    origin: gpui::Point<gpui::Pixels>,
    hwnd: HWND,
    transferred: bool,
}

impl Drop for Gesture {
    fn drop(&mut self) {
        if !self.transferred && unsafe { GetCapture() } == self.hwnd {
            let _ = unsafe { ReleaseCapture() };
        }
    }
}

impl ImageViewer {
    pub(super) fn arm_file_drag(
        &mut self,
        origin: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading
            || self.modal_is_open()
            || self.image.is_none()
            || self.source_file.is_none()
            || self.file_drag_request.is_some()
        {
            return;
        }
        self.file_drag_gesture = None;
        self.invalidate_prefetch();
        let hwnd = crate::gpui_capture::hwnd(window);
        unsafe {
            SetCapture(hwnd);
        }
        if unsafe { GetCapture() } != hwnd {
            return;
        }
        self.pan = None;
        self.preview_pan = None;
        self.file_drag_gesture = Some(Gesture {
            origin,
            hwnd,
            transferred: false,
        });
        self.notice = Some(
            tr!(
                "拖动以复制源文件，Esc 取消",
                "Drag to copy the source file; Esc cancels"
            )
            .into(),
        );
        cx.notify();
    }

    pub(super) fn cancel_file_drag(&mut self) {
        self.file_drag_gesture = None;
        self.file_drag_request = None;
    }

    pub(super) fn move_file_drag(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(gesture) = self.file_drag_gesture.as_ref() else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left)
            || unsafe { GetCapture() } != gesture.hwnd
        {
            self.file_drag_gesture = None;
            return true;
        }
        let dx = f32::from(event.position.x - gesture.origin.x);
        let dy = f32::from(event.position.y - gesture.origin.y);
        if dx.abs().max(dy.abs()) < 4. {
            return true;
        }
        let (Some(path), Some(expected)) = (self.path.clone(), self.source_file.clone()) else {
            self.file_drag_gesture = None;
            return true;
        };
        let request = match NativeDragRequest::begin(gesture.hwnd) {
            Ok(request) => request,
            Err(error) => {
                self.file_drag_gesture = None;
                self.error = Some(error.to_string());
                cx.notify();
                return true;
            }
        };
        // 捕获的清理权交给NativeDragRequest；不能在交接后由手势Drop释放。
        self.file_drag_gesture.as_mut().unwrap().transferred = true;
        self.file_drag_gesture = None;
        let ticket = request.ticket();
        let callback_ticket = ticket.clone();
        let serial = self.serial;
        self.file_drag_request = Some(request);
        self.notice = Some(
            tr!(
                "正在拖出源文件 · 松开放置",
                "Dragging source file · release to drop"
            )
            .into(),
        );
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { file_drag::drag_out(path, expected, ticket) });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                if view.serial != serial
                    || !view
                        .file_drag_request
                        .as_ref()
                        .is_some_and(|request| request.ticket().same_request(&callback_ticket))
                {
                    return;
                }
                view.file_drag_request = None;
                match result {
                    Ok(file_drag::DragOutcome::Copied) => {
                        view.notice = Some(tr!("已复制源文件", "Source file copied").into())
                    }
                    Ok(file_drag::DragOutcome::Cancelled) => view.notice = None,
                    Err(error) => {
                        view.notice = None;
                        view.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        true
    }
}
