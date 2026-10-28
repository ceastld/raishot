//! 显式刷新桌面的宿主事务：准备失败不修改旧源、选区或文档。
use super::{Action, CaptureView};
use crate::{
    capture_refresh::{self, RefreshTopology},
    gpui_capture,
};
use gpui::{Context, RenderImage, Window};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

impl CaptureView {
    pub(crate) fn can_refresh_capture(&self) -> bool {
        !self.pin_mode
            && self.image_canvas.is_none()
            && self.capture_lock.is_some()
            && self.control_selection_ready
            && self.refresh_request.is_none()
            && !self.saving
            && !self.color_picker_pending
            && !self.pin_handoff_pending
            && self.toolbar_drag.is_none()
            && self.pointer_down_start.is_none()
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && self.editor.text_draft.is_none()
            && !self.editor.watermark.has_preview()
            && self.editor.local_fill_candidate().is_none()
            && self.editor.lasso_draft.is_none()
            && self.editor.document.crop_mask.is_none()
            && !self.text_composing()
            && !self.result_editor_composing()
            && self.result_editor.is_none()
            && !self.ocr_overlay_dragging()
            && !self.result_selection_dragging(self.result_selection_generation())
            && self.input.drag_anchor.is_none()
            && !self.quick_ocr_pending()
            && self.quick_save_cancel.is_none()
            && !self.quick_link_pending
            && !self.image_translation.busy()
            && !self.image_translation.after_ocr
            && !self.qr.is_opening()
            && (self.toolbar_panel == crate::toolbar_layout::ToolbarPanel::None
                || self.toolbar_panel.more())
    }

    pub(crate) fn refresh_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_refresh_capture() {
            self.status = tr!(
                "请先结束当前手势或关闭参数面板，再刷新截图",
                "Finish the gesture or close the settings panel before refreshing"
            )
            .into();
            cx.notify();
            return;
        }
        let topology = RefreshTopology::of(&self.source);
        let session = self
            .capture_lock
            .as_ref()
            .expect("refresh capture session")
            .native_cancel_token();
        let cancel = Arc::new(AtomicBool::new(false));
        self.refresh_request = Some(cancel.clone());
        self.toolbar_panel = crate::toolbar_layout::ToolbarPanel::None;
        self.flyout.close();
        self.screenshot_toolbar_menu.dismiss();
        self.release_capture();
        self.status = tr!("正在刷新截图…", "Refreshing capture…").into();
        let escape_was_down = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 as i32,
            )
        } < 0;
        // 参数输入和绘制已通过资格门；所有普通截图控件都随本宿主隐藏。
        unsafe {
            let _ = ShowWindow(gpui_capture::hwnd(window), SW_HIDE);
        }
        let expected = topology.clone();
        let request = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            capture_refresh::prepare(expected, session, request, escape_was_down)
                .map_err(|error| error.to_string())
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if !view
                    .refresh_request
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &cancel))
                {
                    return;
                }
                view.refresh_request = None;
                match result {
                    Ok(prepared)
                        if !cancel.load(Ordering::Acquire) && topology.matches(&view.source) =>
                    {
                        view.cancel_capture_ocr_tasks("刷新底图，取消旧来源请求");
                        view.dismiss_recognition_result();
                        view.invalidate_translation_source();
                        view.invalidate_capture_controls();
                        view.qr.invalidate_source();
                        view.clear_idle_preview_cursor();
                        view.clear_magnifier_sample();
                        view.set_manual_preview(None);
                        view.invalidate_click_sequence();
                        view.source = prepared.source;
                        view.retired_raster_previews
                            .extend(std::mem::replace(&mut view.previews, prepared.previews));
                        if let Some(old) = view.cursor_preview.take() {
                            view.retired_raster_previews.push(old);
                        }
                        view.cursor_preview = prepared.cursor.as_ref().and_then(|cursor| {
                            image::RgbaImage::from_raw(
                                cursor.image.width,
                                cursor.image.height,
                                cursor.image.bgra.clone(),
                            )
                            .map(|pixels| {
                                Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))
                            })
                        });
                        view.cursor = prepared.cursor;
                        view.window_targets = prepared.window_targets;
                        view.hover_window = None;
                        view.scene = Arc::new(crate::scene::Scene::default());
                        view.scene_stamp = None;
                        if let Some(old) = view.finish_preview.take() {
                            view.retired_raster_previews
                                .extend(old.parts.into_iter().map(|part| part.preview));
                        }
                        view.painted_preview.set(None);
                        view.painted_selection.set(None);
                        view.ui_positions.borrow_mut().clear();
                        view.sync_ocr_task_targets();
                        view.status = tr!("截图已刷新", "Capture refreshed").into();
                    }
                    Ok(_) => {
                        view.status = tr!(
                            "刷新已取消，当前截图已保留",
                            "Refresh cancelled; capture kept"
                        )
                        .into()
                    }
                    Err(_) if cancel.load(Ordering::Acquire) => {
                        view.status = tr!(
                            "刷新已取消，当前截图已保留",
                            "Refresh cancelled; capture kept"
                        )
                        .into();
                    }
                    Err(error) => {
                        view.status = trf!(
                            "刷新失败，当前截图已保留：{error}",
                            "Refresh failed; capture kept: {error}"
                        )
                    }
                }
                gpui_capture::foreground_auxiliary(window);
                window.focus(&view.focus);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn cancel_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cancel) = self.refresh_request.take() {
            cancel.store(true, Ordering::Release);
            self.status = tr!(
                "刷新已取消，当前截图已保留",
                "Refresh cancelled; capture kept"
            )
            .into();
            gpui_capture::foreground_auxiliary(window);
            window.focus(&self.focus);
            cx.notify();
        }
    }

    pub(crate) fn handle_refresh_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.refresh_request.is_some() {
            if matches!(action, Action::Cancel) {
                self.cancel_refresh(window, cx);
            }
            return true;
        }
        if matches!(action, Action::Refresh) {
            self.refresh_capture(window, cx);
            return true;
        }
        false
    }
}
