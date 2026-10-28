//! 主界面内的完整播放器；宿主负责遮罩及关闭，不取得原生窗口控制权。
use super::*;
use gpui::{Entity, EventEmitter};

#[derive(Clone, Copy, Debug)]
pub(crate) struct PopupCloseRequested;

impl EventEmitter<PopupCloseRequested> for VideoView {}

impl VideoView {
    pub(crate) fn popup(path: PathBuf, window: &mut Window, cx: &mut App) -> Result<Entity<Self>> {
        let queue = VideoFileQueue::from_paths(vec![path])?;
        let opener = video_open::VideoOpener::new()?;
        let view =
            cx.new(|cx| Self::new_session(queue, opener, cx.focus_handle(), Presentation::Popup));
        Self::watch_session(&view, window, cx);
        view.update(cx, |view, cx| {
            view.popup_focus(window);
            view.start_current();
            cx.notify();
        });
        Ok(view)
    }

    pub(crate) fn popup_focus(&self, window: &mut Window) {
        if self.presentation == Presentation::Popup && self.session_active {
            window.focus(&self.focus);
        }
    }

    pub(super) fn popup_surface_probe(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let weak = cx.entity().downgrade();
        canvas(
            move |bounds, _, cx| {
                let _ = weak.update(cx, |view, cx| {
                    if view.session_active && view.popup_surface_size != Some(bounds.size) {
                        view.popup_surface_size = Some(bounds.size);
                        defer_video_notify(cx);
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    /// 先停会话，再由宿主移除实体；迟到选图不能重建已关闭的播放器。
    pub(crate) fn shutdown_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.presentation != Presentation::Popup || !self.session_active {
            return;
        }
        self.session_active = false;
        self.source_generation = self.source_generation.wrapping_add(1);
        self.minimize.cancel_resume();
        self.opener.cancel();
        self.opening = None;
        self.finish_scrub(None, false);
        self.cancel_frame_output(false);
        self.reset_gif_for_source();
        self.clear_subtitles();
        self.player = None;
        self.path = None;
        video_view_session::retire_session_images(self.take_session_images(), window);
        cx.notify();
    }

    pub(super) fn request_popup_close(&mut self, cx: &mut Context<Self>) {
        if self.presentation == Presentation::Popup && self.session_active {
            cx.emit(PopupCloseRequested);
        }
    }
}
