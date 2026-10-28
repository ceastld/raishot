//! 窗口与嵌入预览共用同一查看会话；定时任务只持有实体弱引用。
use super::*;
use gpui::Entity;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Presentation {
    Window,
    Embedded,
    Popup,
}

impl VideoView {
    pub(super) fn new_session(
        queue: VideoFileQueue,
        opener: video_open::VideoOpener,
        focus: FocusHandle,
        presentation: Presentation,
    ) -> Self {
        Self {
            session_active: true,
            popup_surface_size: None,
            queue,
            source_generation: 0,
            opener,
            opening: None,
            open_error: None,
            player: None,
            path: None,
            focus,
            presentation,
            image: None,
            submitted_frame: None,
            #[cfg(test)]
            screen_roi_bounds: Cell::new(None),
            frame_job: None,
            gif_export: video_gif_view::GifSession::default(),
            subtitles: None,
            subtitle_path: None,
            subtitle_job: None,
            subtitle_offset: video_view_subtitle_timing::SubtitleOffset::default(),
            subtitle_feedback: None,
            recent: video_view_recent::RecentState::default(),
            recent_recorded: false,
            show_details: false,
            show_queue: false,
            show_audio: false,
            audio_keyboard_index: None,
            audio_keyboard_scroll: Cell::new(false),
            audio_scroll: gpui::ScrollHandle::new(),
            retiring: Vec::new(),
            retired: Vec::new(),
            seek_bounds: Rc::new(Cell::new(None)),
            volume_bounds: Rc::new(Cell::new(None)),
            scrub: None,
            scrub_capture: None,
            seek_resume: false,
            seek_drag_ratio: None,
            seek_last_sent: None,
            volume: 1.,
            muted: presentation == Presentation::Embedded,
            repeat: false,
            minimize: video_view_visibility::MinimizeState::default(),
            prompt_pending: false,
            notice: None,
            frame_draw_error: None,
            frame_feedback: None,
            last_position: (Duration::ZERO, Duration::ZERO),
            last_playing: false,
            last_seeking: false,
            last_error: None,
            last_audio_error: None,
            last_audio_info: None,
            last_audio_selection: None,
            last_audio_switching: false,
            repeat_armed: true,
        }
    }

    /// 每个新实体注册一次；不延长实体、播放器或宿主窗口的生命周期。
    pub(super) fn watch_session(view: &Entity<Self>, window: &Window, cx: &mut App) {
        view.update(cx, |_, cx| {
            cx.on_release_in(window, |view, window, _| {
                view.session_active = false;
                view.finish_scrub(None, false);
                view.minimize.cancel_resume();
                view.opener.cancel();
                view.opening = None;
                view.cancel_frame_output(false);
                view.cancel_gif_export(false);
                view.cancel_subtitle_load();
                view.player.take();

                retire_session_images(view.take_session_images(), window);
            })
            .detach();
        });

        let weak = view.downgrade();
        let native = crate::gpui_capture::hwnd(window).0 as usize;
        cx.spawn(async move |cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if !weak
                .update(cx, |view, cx| {
                    if !view.session_active {
                        return false;
                    }
                    let minimized = unsafe {
                        windows::Win32::UI::WindowsAndMessaging::IsIconic(
                            windows::Win32::Foundation::HWND(native as *mut _),
                        )
                    }
                    .as_bool();
                    if view.pull(minimized) {
                        cx.notify();
                    }
                    true
                })
                .unwrap_or(false)
            {
                break;
            }
        })
        .detach();
    }

    pub(super) fn take_session_images(&mut self) -> Vec<Arc<RenderImage>> {
        let mut images = Vec::new();
        for frame in self
            .image
            .take()
            .into_iter()
            .chain(self.submitted_frame.take())
        {
            retain_unique_image(&mut images, frame.image);
        }
        for image in self.retiring.drain(..).chain(self.retired.drain(..)) {
            retain_unique_image(&mut images, image);
        }
        images
    }
}

pub(super) fn retire_session_images(images: Vec<Arc<RenderImage>>, window: &mut Window) {
    if images.is_empty() {
        return;
    }
    // 保留跨过一次完整重绘，让移除播放器后的场景先替换旧场景。
    // 宿主先关闭时，回调及其图像跟随窗口释放。
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, _| {
            for image in images {
                let _ = window.drop_image(image);
            }
            window.refresh();
        });
        window.refresh();
    });
    window.refresh();
}

fn retain_unique_image(images: &mut Vec<Arc<RenderImage>>, image: Arc<RenderImage>) {
    if !images.iter().any(|previous| Arc::ptr_eq(previous, &image)) {
        images.push(image);
    }
}
