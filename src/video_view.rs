//! 只读外部视频查看窗口。画面与声音由 Player 按需解码，窗口只保存当前显示帧。
#[path = "video_controls_details.rs"]
mod video_controls_details;
#[path = "video_gif_view.rs"]
mod video_gif_view;
#[path = "video_open.rs"]
mod video_open;
#[path = "video_view_audio.rs"]
mod video_view_audio;
#[path = "video_view_audio_processing.rs"]
mod video_view_audio_processing;
#[path = "video_view_embedded.rs"]
mod video_view_embedded;
#[path = "video_view_history.rs"]
mod video_view_history;
#[path = "video_view_popup.rs"]
mod video_view_popup;
#[path = "video_view_queue.rs"]
mod video_view_queue;
#[path = "video_view_recent.rs"]
mod video_view_recent;
#[path = "video_view_session.rs"]
mod video_view_session;
#[cfg(test)]
#[path = "video_view_smoke.rs"]
mod video_view_smoke;
#[path = "video_view_subtitle_gate.rs"]
mod video_view_subtitle_gate;
#[path = "video_view_subtitle_timing.rs"]
mod video_view_subtitle_timing;
#[path = "video_view_subtitles.rs"]
mod video_view_subtitles;
#[path = "video_view_visibility.rs"]
mod video_view_visibility;

use crate::{
    preview_audio::AudioStreamSelection,
    recording_preview::{Player, RATES},
    video_controls::{button, ratio_at, slider, SliderBounds},
    video_file_queue::VideoFileQueue,
    video_frame_export::FrameSnapshot,
    video_shortcuts::{self, VideoShortcut},
    Result,
};
use gpui::{
    canvas, div, prelude::*, px, rgb, size, App, Application, Bounds, Context, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, PathPromptOptions,
    Render, RenderImage, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub(crate) use video_view_history::HistoryVideoPreview;
pub(crate) use video_view_popup::PopupCloseRequested;
use video_view_session::Presentation;

#[derive(Clone, Copy)]
enum Scrub {
    Seek,
    Volume,
}

#[derive(Clone, Copy)]
enum FrameAction {
    Copy,
    Save,
    Edit,
    Pin,
}

struct FrameJob {
    action: FrameAction,
    cancel: Arc<AtomicBool>,
}

static ACTIVE_FRAME_WORK: AtomicUsize = AtomicUsize::new(0);

/// 后台输出的暂存/HGLOBAL清理完成前保留进程，不保留窗口或播放器。
struct FrameWork;

impl FrameWork {
    fn begin() -> Self {
        ACTIVE_FRAME_WORK.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for FrameWork {
    fn drop(&mut self) {
        ACTIVE_FRAME_WORK.fetch_sub(1, Ordering::AcqRel);
    }
}

struct FrameCompletion<T> {
    // 字段按声明顺序析构：前台任务取消也先清理准备对象，再释放进程保活。
    result: T,
    _work: Arc<FrameWork>,
}

impl Drop for FrameJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

struct SubtitleLoad(Arc<AtomicBool>);
impl Drop for SubtitleLoad {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
struct SceneFrame {
    serial: u64,
    position: Duration,
    image: Arc<RenderImage>,
}

pub(crate) struct VideoView {
    presentation: Presentation,
    session_active: bool,
    popup_surface_size: Option<gpui::Size<gpui::Pixels>>,
    queue: VideoFileQueue,
    source_generation: u64,
    opener: video_open::VideoOpener,
    opening: Option<video_open::VideoOpenTicket>,
    open_error: Option<String>,
    player: Option<Player>,
    path: Option<PathBuf>,
    focus: FocusHandle,
    image: Option<SceneFrame>,
    submitted_frame: Option<SceneFrame>,
    #[cfg(test)]
    screen_roi_bounds: Cell<Option<Bounds<gpui::Pixels>>>,
    frame_job: Option<FrameJob>,
    gif_export: video_gif_view::GifSession,
    subtitles: Option<video_view_subtitles::SubtitleTrack>,
    subtitle_path: Option<PathBuf>,
    subtitle_job: Option<SubtitleLoad>,
    subtitle_offset: video_view_subtitle_timing::SubtitleOffset,
    subtitle_feedback: Option<String>,
    recent: video_view_recent::RecentState,
    recent_recorded: bool,
    show_details: bool,
    show_queue: bool,
    show_audio: bool,
    audio_keyboard_index: Option<usize>,
    audio_keyboard_scroll: Cell<bool>,
    audio_scroll: gpui::ScrollHandle,
    // 让上一场景引用的纹理完整度过下一次绘制，再归还 atlas。
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    seek_bounds: SliderBounds,
    volume_bounds: SliderBounds,
    scrub: Option<Scrub>,
    scrub_capture: Option<windows::Win32::Foundation::HWND>,
    seek_resume: bool,
    seek_drag_ratio: Option<f32>,
    seek_last_sent: Option<(Instant, f32)>,
    volume: f32,
    muted: bool,
    repeat: bool,
    minimize: video_view_visibility::MinimizeState,
    prompt_pending: bool,
    notice: Option<String>,
    frame_draw_error: Option<String>,
    frame_feedback: Option<String>,
    last_position: (Duration, Duration),
    last_playing: bool,
    last_seeking: bool,
    last_error: Option<String>,
    last_audio_error: Option<String>,
    last_audio_info: Option<Vec<(u32, usize)>>,
    last_audio_selection: Option<AudioStreamSelection>,
    last_audio_switching: bool,
    repeat_armed: bool,
}

impl VideoView {
    /// 原生验证可从窗口实体取得统计；报告不含文件路径或视频正文。
    pub(crate) fn sync_report(&self) -> Option<serde_json::Value> {
        self.player.as_ref().map(Player::sync_report)
    }

    pub(crate) fn open(path: Option<PathBuf>, cx: &mut App) -> Result<WindowHandle<Self>> {
        Self::open_paths(path.into_iter().collect(), cx)
    }

    pub(crate) fn open_paths(paths: Vec<PathBuf>, cx: &mut App) -> Result<WindowHandle<Self>> {
        crate::theme::install(cx);
        let queue = VideoFileQueue::from_paths(paths)?;
        let opener = video_open::VideoOpener::new()?;
        let display = cx.primary_display().map(|display| display.bounds().size);
        let width = display.map_or(960., |size| f32::from(size.width).min(1100.) * 0.85);
        let height = display.map_or(640., |size| f32::from(size.height).min(800.) * 0.85);
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let choose_file = queue.is_empty();
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(520.), px(360.))),
                titlebar: Some(TitlebarOptions {
                    appears_transparent: false,
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            move |window, cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                let view =
                    cx.new(|_| Self::new_session(queue, opener, focus, Presentation::Window));
                Self::watch_session(&view, window, cx);
                view.update(cx, |view, cx| {
                    if choose_file {
                        view.open_prompt(cx);
                    } else {
                        view.start_current();
                        cx.notify();
                    }
                });
                view
            },
        )?)
    }

    fn replace_file(&mut self, path: PathBuf) {
        self.finish_scrub(None, false);
        self.cancel_frame_output(false);
        self.clear_frame();
        let player = if self.presentation == Presentation::Embedded {
            // 从暂停状态创建也发布真实首帧，不依赖UI调度及时发送暂停。
            Player::open_file_paused_with_audio_settings(path.clone(), self.volume, self.muted)
        } else {
            Player::open_file_with_audio_settings(path.clone(), self.volume, self.muted)
        };
        self.player = Some(player);
        self.path = Some(path);
        self.open_error = None;
        self.notice = None;
        self.frame_draw_error = None;
        self.frame_feedback = None;
        self.scrub = None;
        self.last_position = (Duration::ZERO, Duration::ZERO);
        self.last_seeking = false;
        self.last_error = None;
        self.last_audio_error = None;
        self.last_audio_info = None;
        self.last_audio_selection = None;
        self.last_audio_switching = false;
        self.repeat_armed = true;
    }

    fn start_current(&mut self) {
        if !self.session_active {
            return;
        }
        self.source_generation = self.source_generation.wrapping_add(1);
        self.minimize.cancel_resume();
        self.reset_gif_for_source();
        self.finish_scrub(None, false);
        self.hide_recent();
        self.hide_queue();
        self.hide_audio();
        self.recent_recorded = false;
        self.opener.cancel();
        self.opening = None;
        self.open_error = None;
        self.cancel_frame_output(false);
        self.clear_subtitles();
        self.player = None;
        self.clear_frame();
        self.scrub = None;
        self.notice = None;
        self.frame_draw_error = None;
        self.frame_feedback = None;
        self.last_position = (Duration::ZERO, Duration::ZERO);
        self.last_playing = false;
        self.last_seeking = false;
        self.last_error = None;
        self.last_audio_error = None;
        self.last_audio_info = None;
        self.last_audio_selection = None;
        self.last_audio_switching = false;
        self.repeat_armed = true;
        self.path = self.queue.current().map(|item| item.path().to_path_buf());
        let Some(index) = self.queue.index() else {
            return;
        };
        let Some(item) = self.queue.current() else {
            return;
        };
        let Some(path) = item.open_path().map(PathBuf::from) else {
            self.open_error = item.error().map(str::to_string);
            return;
        };
        match self.opener.request(index, path) {
            Ok(ticket) => self.opening = Some(ticket),
            Err(error) => {
                self.queue.record_result(index, Err(error.clone()));
                self.open_error = Some(error);
            }
        }
    }

    fn replace_queue(&mut self, queue: VideoFileQueue) {
        if !self.session_active || queue.is_empty() {
            return;
        }
        self.queue = queue;
        self.start_current();
    }

    fn navigate_files(&mut self, delta: isize, cx: &mut Context<Self>) {
        if !self.prompt_pending && self.queue.navigate(delta) {
            self.start_current();
            cx.notify();
        }
    }

    fn poll_open(&mut self) -> bool {
        let Some(completion) = self.opener.poll() else {
            return false;
        };
        if !self
            .opening
            .as_ref()
            .is_some_and(|ticket| ticket.same_request(&completion.ticket))
            || self.queue.index() != Some(completion.index)
            || self.queue.current().and_then(|item| item.open_path())
                != Some(completion.path.as_path())
        {
            return false;
        }
        self.opening = None;
        self.queue
            .record_result(completion.index, completion.result.clone());
        match completion.result {
            Ok(()) => self.replace_file(completion.path),
            Err(error) => self.open_error = Some(error),
        }
        true
    }

    fn open_prompt(&mut self, cx: &mut Context<Self>) {
        if !self.session_active || self.prompt_pending {
            return;
        }
        self.cancel_frame_output(false);
        self.prompt_pending = true;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr!("选择视频文件，可多选", "Choose video files").into()),
        });
        cx.spawn(async move |weak, cx| {
            let result = receiver
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = weak.update(cx, |view, cx| {
                view.prompt_pending = false;
                if !view.session_active {
                    return;
                }
                match result {
                    Ok(Some(paths)) => match VideoFileQueue::from_paths(paths) {
                        Ok(queue) => view.replace_queue(queue),
                        Err(error) => view.notice = Some(error),
                    },
                    Ok(None) => {}
                    Err(error) => {
                        view.notice = Some(trf!(
                            "无法打开文件选择器：{error}",
                            "Cannot open the file picker: {error}"
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn pull(&mut self, minimized: bool) -> bool {
        if !self.session_active {
            return false;
        }
        let gesture_changed = self.update_scrub();
        let opened = self.poll_open();
        if minimized && !self.minimize.is_minimized() {
            // 先结束可能被最小化打断的拖动，再保存最终播放意图。
            self.finish_scrub(None, true);
        }
        let visibility_action = self.minimize.update(
            minimized,
            self.source_generation,
            self.player.as_ref().is_some_and(Player::playing),
        );
        let visibility_changed = visibility_action != video_view_visibility::Action::None;
        if let Some(player) = self.player.as_mut() {
            match visibility_action {
                video_view_visibility::Action::None => {}
                video_view_visibility::Action::Pause => player.set_playing(false),
                video_view_visibility::Action::Resume => {
                    if player.error().is_none() {
                        player.set_playing(true);
                    }
                }
            }
        }
        let Some(player) = self.player.as_mut() else {
            return opened || gesture_changed || visibility_changed;
        };
        let mut changed = opened || gesture_changed || visibility_changed;
        if let Some((serial, position, pixels)) = player.take_frame_with_time() {
            let (width, height) = player.size();
            if let Some(pixels) = image::RgbaImage::from_raw(width, height, pixels) {
                let next = Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]));
                if let Some(previous) = self.image.replace(SceneFrame {
                    serial,
                    position,
                    image: next,
                }) {
                    retire_scene_frame(&mut self.retiring, self.submitted_frame.as_ref(), previous);
                }
                changed = true;
            }
        }
        let position = player.position();
        let playing = player.playing();
        let seeking = player.seeking();
        let error = player.error();
        if error.is_some() && error != self.last_error {
            if let Some(index) = self.queue.index() {
                self.queue
                    .record_result(index, Err(error.clone().expect("video error")));
            }
        }
        let audio_error = player.audio_error();
        let audio_info = player.audio_info();
        let audio_selection = player.audio_selection();
        let audio_switching = player.audio_switching();
        let audio_changed = audio_error != self.last_audio_error
            || audio_info != self.last_audio_info
            || audio_selection != self.last_audio_selection
            || audio_switching != self.last_audio_switching;
        if audio_changed && self.show_audio && self.audio_keyboard_index.is_some() {
            // 异步状态行改变高度时，重新按本轮布局保持键盘候选可见。
            self.audio_keyboard_scroll.set(true);
        }
        // 用户暂停会解除武装；正常播放结束则沿同一个循环意图重播。
        if playing && !self.last_playing {
            self.repeat_armed = true;
        }
        let ended = !playing
            && !seeking
            && !position.1.is_zero()
            && position.0 >= position.1
            && error.is_none();
        if self.repeat
            && self.repeat_armed
            && ended
            && !minimized
            && !matches!(self.scrub, Some(Scrub::Seek))
        {
            player.toggle();
            // Play同步发布playing；不依赖轮询碰巧看到极短视频的播放区间。
            self.repeat_armed = true;
        }
        // 时间标签只需 0.1 秒精度，避免暂停时不断重绘。
        changed |= position.0.as_millis() / 100 != self.last_position.0.as_millis() / 100
            || position.1 != self.last_position.1
            || playing != self.last_playing
            || seeking != self.last_seeking
            || error != self.last_error
            || audio_changed;
        self.last_position = position;
        self.last_playing = playing;
        self.last_seeking = seeking;
        self.last_error = error;
        self.last_audio_error = audio_error;
        self.last_audio_info = audio_info;
        self.last_audio_selection = audio_selection;
        self.last_audio_switching = audio_switching;
        changed
    }

    fn set_volume(&mut self, value: f32) {
        self.volume = value.clamp(0., 1.);
        // 调节音量恢复声音，静音按钮则保留原音量方便再次打开。
        self.muted = false;
        if let Some(player) = &self.player {
            player.set_volume(self.volume);
            player.set_muted(false);
        }
    }

    fn clear_frame(&mut self) {
        #[cfg(test)]
        self.screen_roi_bounds.set(None);
        if let Some(frame) = self.image.take() {
            retire_scene_frame(&mut self.retiring, self.submitted_frame.as_ref(), frame);
        }
        self.submitted_frame = None;
    }

    fn toggle_muted(&mut self) {
        self.muted = !self.muted;
        if let Some(player) = &self.player {
            player.set_muted(self.muted);
        }
    }

    fn step_frame(&mut self) {
        if self.prompt_pending || self.image.is_none() {
            return;
        }
        self.minimize.cancel_resume();
        self.finish_scrub(None, true);
        self.repeat_armed = false;
        if let Some(player) = &mut self.player {
            player.step_forward();
        }
    }

    fn toggle_playback(&mut self) {
        self.minimize.cancel_resume();
        self.finish_scrub(None, true);
        if let Some(player) = &mut self.player {
            if player.seeking() {
                // 挂起定位时旧位置可能仍在片尾，只改意图，不能隐式重播覆盖目标。
                player.set_playing(!player.playing());
            } else {
                player.toggle();
            }
            // 用户暂停即解除片尾重播；再次播放时重新武装。
            self.repeat_armed = player.playing();
        }
    }

    fn begin_scrub(&mut self, kind: Scrub, x: gpui::Pixels, window: &Window) {
        if !self.session_active {
            return;
        }
        self.finish_scrub(None, true);
        if matches!(kind, Scrub::Seek) {
            let Some(player) = &mut self.player else {
                return;
            };
            if player.position().1.is_zero() {
                return;
            }
            self.minimize.cancel_resume();
            self.seek_resume = player.playing();
            player.set_playing(false);
            self.repeat_armed = false;
        }
        self.scrub = Some(kind);
        let owner = crate::gpui_capture::hwnd(window);
        unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(owner);
            if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == owner {
                self.scrub_capture = Some(owner);
            }
        }
        self.scrub_at(x, true);
        if self.scrub_capture.is_none() {
            self.finish_scrub(None, true);
        }
    }

    fn update_scrub(&mut self) -> bool {
        if self.scrub.is_none() {
            return false;
        }
        let captured = self.scrub_capture.is_some_and(|owner| unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == owner
        });
        if !captured {
            self.finish_scrub(None, true);
            return true;
        }
        if matches!(self.scrub, Some(Scrub::Seek)) {
            if let (Some(target), Some(player)) = (self.seek_drag_ratio, &self.player) {
                if self.seek_last_sent.is_none_or(|(sent, previous)| {
                    previous != target && sent.elapsed() >= Duration::from_millis(30)
                }) {
                    player.seek_ratio(target);
                    self.seek_last_sent = Some((Instant::now(), target));
                    return true;
                }
            }
        }
        false
    }

    fn scrub_at(&mut self, x: gpui::Pixels, force: bool) {
        match self.scrub {
            Some(Scrub::Seek) => {
                if let (Some(player), Some(ratio)) = (&self.player, ratio_at(&self.seek_bounds, x))
                {
                    if !player.position().1.is_zero() {
                        self.seek_drag_ratio = Some(ratio);
                        if self.seek_last_sent.is_none_or(|(sent, previous)| {
                            previous != ratio
                                && (force || sent.elapsed() >= Duration::from_millis(30))
                        }) {
                            player.seek_ratio(ratio);
                            self.seek_last_sent = Some((Instant::now(), ratio));
                        }
                    }
                }
            }
            Some(Scrub::Volume) => {
                if let Some(ratio) = ratio_at(&self.volume_bounds, x) {
                    self.set_volume(ratio);
                }
            }
            None => {}
        }
    }

    fn finish_scrub(&mut self, x: Option<gpui::Pixels>, restore_playback: bool) {
        if restore_playback {
            if let Some(x) = x {
                self.scrub_at(x, true);
            }
            if matches!(self.scrub, Some(Scrub::Seek)) {
                if let Some(player) = &mut self.player {
                    if let Some(target) = self.seek_drag_ratio {
                        if self.seek_last_sent.is_none_or(|(_, sent)| sent != target) {
                            player.seek_ratio(target);
                        }
                    }
                    player.set_playing(self.seek_resume);
                    self.repeat_armed = self.seek_resume;
                }
            }
        }
        if let Some(owner) = self.scrub_capture.take() {
            unsafe {
                if windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() == owner {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                }
            }
        }
        self.scrub = None;
        self.seek_resume = false;
        self.seek_drag_ratio = None;
        self.seek_last_sent = None;
    }

    fn frame_snapshot(&self) -> Result<FrameSnapshot> {
        if self.frame_draw_error.is_some() {
            return Err(tr!(
                "当前画面绘制失败，恢复后才能输出视频帧",
                "Wait for video drawing to recover before exporting a frame"
            )
            .into());
        }
        let frame = self.submitted_frame.as_ref().ok_or(tr!(
            "当前没有可输出的视频帧",
            "No video frame is ready to export"
        ))?;
        let source = self
            .path
            .clone()
            .ok_or(tr!("请先打开视频文件", "Open a video file first"))?;
        FrameSnapshot::new(frame.image.clone(), source, frame.position)
    }

    fn cancel_subtitle_load(&mut self) {
        if let Some(job) = &self.subtitle_job {
            job.0.store(true, Ordering::Release);
        }
    }

    fn clear_subtitles(&mut self) {
        self.cancel_subtitle_load();
        self.subtitles = None;
        self.subtitle_offset.reset();
        self.subtitle_path = None;
        self.subtitle_feedback = None;
    }

    fn adjust_subtitles(&mut self, delta_millis: i32) {
        if self.subtitles.is_some() {
            self.subtitle_offset.adjust(delta_millis);
        }
    }

    fn choose_subtitles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_pending || self.subtitle_job.is_some() {
            return;
        }
        if self.path.is_none() {
            self.notice = Some(
                tr!(
                    "请先打开视频，再载入本地字幕",
                    "Open a video before loading subtitles"
                )
                .into(),
            );
            cx.notify();
            return;
        }
        self.prompt_pending = true;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                tr!(
                    "选择本地字幕（SRT / WebVTT）",
                    "Choose local subtitles (SRT / WebVTT)"
                )
                .into(),
            ),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                view.prompt_pending = false;
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.load_subtitles(path, window, cx);
                        }
                    }
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        view.subtitle_feedback = Some(trf!(
                            "字幕选择失败：{error}",
                            "Subtitle picker failed: {error}"
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_subtitles(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_none() || self.subtitle_job.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.subtitle_job = Some(SubtitleLoad(cancel.clone()));
        self.subtitle_feedback = None;
        let preparing_cancel = cancel.clone();
        let loading_path = path.clone();
        let task = cx.background_executor().spawn(async move {
            video_view_subtitles::SubtitleTrack::load_explicit(&loading_path, &preparing_cancel)
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, _, cx| {
                if !view
                    .subtitle_job
                    .as_ref()
                    .is_some_and(|job| Arc::ptr_eq(&job.0, &cancel))
                {
                    return;
                }
                if !cancel.load(Ordering::Acquire) {
                    match result {
                        Ok(track) => {
                            view.subtitles = Some(track);
                            view.subtitle_offset.reset();
                            view.subtitle_path = Some(path);
                            view.subtitle_feedback = None;
                        }
                        Err(error) => view.subtitle_feedback = Some(error.to_string()),
                    }
                }
                view.subtitle_job.take();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn subtitle_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .pt(px(5.));
        if let Some(job) = &self.subtitle_job {
            let canceled = job.0.load(Ordering::Acquire);
            return row
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(if canceled {
                            tr!("正在取消字幕载入…", "Canceling subtitle loading…")
                        } else {
                            tr!("正在载入本地字幕…", "Loading local subtitles…")
                        }),
                )
                .child(
                    button(
                        "video-cancel-subtitles",
                        tr!("取消字幕载入", "Cancel subtitles"),
                        false,
                        cx,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.cancel_subtitle_load();
                        cx.notify();
                    })),
                );
        }
        row = row.child(
            button(
                "video-load-subtitles",
                tr!("本地字幕", "Local subtitles"),
                self.subtitles.is_some(),
                cx,
            )
            .on_click(cx.listener(|view, _, window, cx| view.choose_subtitles(window, cx))),
        );
        if let Some(path) = &self.subtitle_path {
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                        ),
                )
                .child(
                    button(
                        "video-clear-subtitles",
                        tr!("关闭字幕", "Clear subtitles"),
                        false,
                        cx,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.clear_subtitles();
                        cx.notify();
                    })),
                )
                .child(self.subtitle_timing_controls(cx));
        }
        if let Some(message) = &self.subtitle_feedback {
            row = row.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.danger))
                    .child(message.clone()),
            );
        }
        row
    }

    fn subtitle_timing_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        let offset = f64::from(self.subtitle_offset.millis()) / 1000.;
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .flex_1()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(trf!(
                        "字幕延迟：{offset:+.1} 秒",
                        "Subtitle delay: {offset:+.1}s"
                    )),
            )
            .child(
                button(
                    "video-subtitles-earlier",
                    tr!("提前 (G)", "Earlier (G)"),
                    false,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.adjust_subtitles(-500);
                    cx.notify();
                })),
            )
            .child(
                button(
                    "video-subtitles-later",
                    tr!("延后 (H)", "Later (H)"),
                    false,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.adjust_subtitles(500);
                    cx.notify();
                })),
            )
            .child(
                button("video-subtitles-reset", tr!("归零", "Reset"), false, cx)
                    .opacity(if self.subtitle_offset.millis() == 0 {
                        0.45
                    } else {
                        1.
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.subtitle_offset.reset();
                        cx.notify();
                    })),
            )
    }

    fn begin_frame_job(&mut self, action: FrameAction) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.frame_job = Some(FrameJob {
            action,
            cancel: cancel.clone(),
        });
        self.notice = None;
        self.frame_feedback = None;
        cancel
    }

    fn accept_frame_result(&mut self, cancel: &Arc<AtomicBool>, cx: &mut Context<Self>) -> bool {
        if !self
            .frame_job
            .as_ref()
            .is_some_and(|job| Arc::ptr_eq(&job.cancel, cancel))
        {
            return false;
        }
        if cancel.load(Ordering::Acquire) {
            self.frame_job.take();
            cx.notify();
            return false;
        }
        true
    }

    fn cancel_frame_output(&mut self, notify: bool) {
        if let Some(job) = &self.frame_job {
            // 后台返回前仍占有任务槽，连续取消不会堆积多份大帧/剪贴板准备任务。
            job.cancel.store(true, Ordering::Release);
            if notify {
                self.frame_feedback = Some(tr!("已取消当前帧输出", "Frame export canceled").into());
            }
        }
    }

    fn save_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_pending || self.frame_job.is_some() {
            return;
        }
        let snapshot = match self.frame_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.notice = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let cancel = self.begin_frame_job(FrameAction::Save);
        let receiver =
            cx.prompt_for_new_path(snapshot.directory(), Some(&snapshot.suggested_name()));
        self.prompt_pending = true;
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let target = match chosen {
                Ok(Ok(Some(path))) => Some(Ok(path)),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => Some(Err(error.to_string())),
            };
            let continuing = weak
                .update_in(cx, |view, _, cx| {
                    view.prompt_pending = false;
                    if !view.accept_frame_result(&cancel, cx) {
                        return false;
                    }
                    match &target {
                        Some(Ok(_)) => {
                            cx.notify();
                            true
                        }
                        Some(Err(error)) => {
                            view.frame_job.take();
                            view.notice = Some(trf!(
                                "无法打开保存对话框：{error}",
                                "Cannot open the save dialog: {error}"
                            ));
                            cx.notify();
                            false
                        }
                        None => {
                            view.frame_job.take();
                            cx.notify();
                            false
                        }
                    }
                })
                .unwrap_or(false);
            if !continuing {
                return;
            }
            let Some(Ok(target)) = target else {
                return;
            };
            let _work = Arc::new(FrameWork::begin());
            let preparing_work = _work.clone();
            let preparing_cancel = cancel.clone();
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let result = snapshot
                        .prepare_new_png(&target, &preparing_cancel)
                        .map_err(|error| error.to_string());
                    FrameCompletion {
                        result,
                        _work: preparing_work,
                    }
                })
                .await;
            let _ = weak.update_in(cx, |view, _, cx| {
                if !view.accept_frame_result(&cancel, cx) {
                    return;
                }
                let result = prepared.result.and_then(|prepared| {
                    prepared.commit(&cancel).map_err(|error| error.to_string())
                });
                view.frame_job.take();
                match result {
                    Ok(path) => {
                        view.frame_feedback =
                            Some(trf!("当前帧已保存：{}", "Frame saved: {}", path.display()))
                    }
                    Err(error) => {
                        view.notice = Some(trf!(
                            "当前帧保存失败：{error}",
                            "Frame save failed: {error}"
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn convert_frame(&mut self, action: FrameAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_pending
            || self.frame_job.is_some()
            || !matches!(action, FrameAction::Edit | FrameAction::Pin)
        {
            return;
        }
        let snapshot = match self.frame_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.notice = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let cancel = self.begin_frame_job(action);
        let preparing_cancel = cancel.clone();
        let work = Arc::new(FrameWork::begin());
        let preparing_work = work.clone();
        let task = cx.background_executor().spawn(async move {
            let result = snapshot
                .prepare_pixels(&preparing_cancel)
                .map_err(|error| error.to_string());
            FrameCompletion {
                result,
                _work: preparing_work,
            }
        });
        cx.spawn_in(window, async move |weak, cx| {
            let _work = work;
            let completion = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.accept_frame_result(&cancel, cx) {
                    return;
                }
                let result = completion.result.and_then(|image| {
                    match action {
                        FrameAction::Edit => crate::image_editor::open(image, None, cx).map(|_| ()),
                        FrameAction::Pin => {
                            crate::pin_view::PinView::open(image, pin_origin(window), cx)
                        }
                        _ => {
                            return Err(
                                tr!("当前帧输出操作无效", "Invalid frame action").to_string()
                            )
                        }
                    }
                    .map_err(|error| error.to_string())
                });
                view.frame_job.take();
                match result {
                    Ok(()) if matches!(action, FrameAction::Edit) => {
                        view.frame_feedback = Some(
                            tr!(
                                "当前帧已在图片编辑器打开",
                                "Frame opened in the image editor"
                            )
                            .to_string(),
                        )
                    }
                    Ok(()) => {
                        view.frame_feedback = Some(tr!("已贴出当前帧", "Frame pinned").to_string())
                    }
                    Err(error) => {
                        view.notice = Some(trf!(
                            "当前帧输出失败：{error}",
                            "Frame export failed: {error}"
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_pending || self.frame_job.is_some() {
            return;
        }
        let sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        if sequence == 0 {
            self.notice = Some(
                tr!(
                    "无法读取剪贴板状态，请重试复制当前帧",
                    "Cannot read the clipboard state; try copying the frame again"
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let snapshot = match self.frame_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.notice = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let retained_bytes = crate::video_frame_export::retained_render_bytes(
            self.image
                .iter()
                .chain(self.submitted_frame.iter())
                .map(|frame| &frame.image)
                .chain(self.retiring.iter())
                .chain(self.retired.iter()),
        );
        let cancel = self.begin_frame_job(FrameAction::Copy);
        let preparing_cancel = cancel.clone();
        let work = Arc::new(FrameWork::begin());
        let preparing_work = work.clone();
        let task = cx.background_executor().spawn(async move {
            let result = snapshot
                .prepare_clipboard(&preparing_cancel, retained_bytes)
                .map_err(|error| error.to_string());
            FrameCompletion {
                result,
                _work: preparing_work,
            }
        });
        cx.spawn_in(window, async move |weak, cx| {
            let _work = work;
            let prepared = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.accept_frame_result(&cancel, cx) {
                    return;
                }
                let result = prepared.result.and_then(|prepared| {
                    prepared
                        .publish_if_unchanged(crate::gpui_capture::hwnd(window), sequence)
                        .map_err(|error| error.to_string())
                });
                view.frame_job.take();
                match result {
                    Ok(()) => {
                        view.frame_feedback = Some(tr!("已复制当前帧", "Frame copied").to_string())
                    }
                    Err(error) => {
                        view.notice = Some(trf!(
                            "当前帧复制失败：{error}",
                            "Frame copy failed: {error}"
                        ))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn frame_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .pt(px(5.));
        if let Some(job) = &self.frame_job {
            let canceled = job.cancel.load(Ordering::Acquire);
            let label = if canceled {
                tr!("正在取消当前帧输出…", "Canceling frame export…")
            } else {
                match job.action {
                    FrameAction::Copy => tr!("正在准备复制当前帧…", "Preparing frame copy…"),
                    FrameAction::Save if self.prompt_pending => {
                        tr!("选择 PNG 保存位置…", "Choose where to save the PNG…")
                    }
                    FrameAction::Save => tr!("正在保存当前帧…", "Saving frame…"),
                    FrameAction::Edit => tr!("正在准备图片编辑…", "Preparing the image editor…"),
                    FrameAction::Pin => tr!("正在准备贴图…", "Preparing the pin…"),
                }
            };
            row = row
                .child(
                    div()
                        .flex_1()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(label),
                )
                .child(
                    button("video-cancel-frame", tr!("取消", "Cancel"), false, cx).on_click(
                        cx.listener(|view, _, _, cx| {
                            view.cancel_frame_output(true);
                            cx.notify();
                        }),
                    ),
                );
        } else if self.submitted_frame.is_some() && self.frame_draw_error.is_none() {
            for (id, action, label) in [
                (
                    "video-copy-frame",
                    FrameAction::Copy,
                    tr!("复制当前帧", "Copy frame"),
                ),
                (
                    "video-save-frame",
                    FrameAction::Save,
                    tr!("保存 PNG", "Save PNG"),
                ),
                (
                    "video-edit-frame",
                    FrameAction::Edit,
                    tr!("图片编辑", "Edit image"),
                ),
                (
                    "video-pin-frame",
                    FrameAction::Pin,
                    tr!("贴出当前帧", "Pin frame"),
                ),
            ] {
                row = row.child(button(id, label, false, cx).on_click(cx.listener(
                    move |view, _, window, cx| match action {
                        FrameAction::Copy => view.copy_frame(window, cx),
                        FrameAction::Save => view.save_frame(window, cx),
                        FrameAction::Edit | FrameAction::Pin => {
                            view.convert_frame(action, window, cx)
                        }
                    },
                )));
            }
        }
        row
    }

    fn shortcut(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session_active || self.prompt_pending {
            if self.presentation == Presentation::Popup {
                cx.stop_propagation();
            }
            return;
        }
        if self.presentation == Presentation::Popup {
            match video_shortcuts::resolve(&event.keystroke) {
                Some(VideoShortcut::Close | VideoShortcut::ExitFullscreen) => {
                    if !event.is_held {
                        self.request_popup_close(cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                Some(VideoShortcut::ToggleFullscreen) => {
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if self.audio_shortcut(event, cx) {
            return;
        }
        let Some(command) = video_shortcuts::resolve(&event.keystroke) else {
            if self.presentation == Presentation::Popup {
                cx.stop_propagation();
            }
            return;
        };
        if event.is_held
            && !matches!(
                command,
                VideoShortcut::SeekSeconds(_) | VideoShortcut::AdjustVolume(_)
            )
        {
            cx.stop_propagation();
            return;
        }
        if self.scrub.is_some() {
            self.finish_scrub(None, true);
            if command == VideoShortcut::ExitFullscreen {
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        match command {
            VideoShortcut::OpenFile => self.open_prompt(cx),
            VideoShortcut::PreviousFile => self.navigate_files(-1, cx),
            VideoShortcut::NextFile => self.navigate_files(1, cx),
            VideoShortcut::Close => {
                if self.presentation == Presentation::Window {
                    window.remove_window();
                }
            }
            VideoShortcut::CopyFrame => self.copy_frame(window, cx),
            VideoShortcut::SaveFrame => self.save_frame(window, cx),
            VideoShortcut::EditFrame => self.convert_frame(FrameAction::Edit, window, cx),
            VideoShortcut::PinFrame => self.convert_frame(FrameAction::Pin, window, cx),
            VideoShortcut::LoadSubtitles => self.choose_subtitles(window, cx),
            VideoShortcut::SubtitleOffsetMillis(delta) => self.adjust_subtitles(delta),
            VideoShortcut::TogglePlayback => {
                self.toggle_playback();
            }
            VideoShortcut::StepForward => self.step_frame(),
            VideoShortcut::SeekSeconds(seconds) => {
                if let Some(player) = &self.player {
                    let (position, duration) = player.position();
                    if let Some(ratio) = video_shortcuts::seek_ratio(position, duration, seconds) {
                        self.repeat_armed = player.playing();
                        player.seek_ratio(ratio);
                    }
                }
            }
            VideoShortcut::ToStart | VideoShortcut::ToEnd => {
                if let Some(player) = &self.player {
                    self.repeat_armed = player.playing();
                    player.seek_ratio(if command == VideoShortcut::ToStart {
                        0.
                    } else {
                        1.
                    });
                }
            }
            VideoShortcut::AdjustVolume(amount) => {
                self.set_volume(self.volume + amount as f32 / 100.)
            }
            VideoShortcut::ToggleMuted => self.toggle_muted(),
            VideoShortcut::ToggleFullscreen => {
                if self.presentation == Presentation::Window {
                    window.toggle_fullscreen();
                }
            }
            VideoShortcut::ExitFullscreen => {
                if self.recent_visible()
                    || self.show_queue
                    || self.show_audio
                    || self.show_details
                    || self.gif_visible()
                {
                    // 一次Esc只收起当前面板，保留后台任务与全屏状态。
                    self.hide_recent();
                    self.hide_queue();
                    self.hide_audio();
                    self.hide_gif();
                    self.show_details = false;
                } else if self.gif_export_active() {
                    self.cancel_gif_export(true);
                } else if self
                    .frame_job
                    .as_ref()
                    .is_some_and(|job| !job.cancel.load(Ordering::Acquire))
                {
                    self.cancel_frame_output(true);
                } else if self
                    .subtitle_job
                    .as_ref()
                    .is_some_and(|job| !job.0.load(Ordering::Acquire))
                {
                    self.cancel_subtitle_load();
                } else if self.presentation == Presentation::Window && window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for VideoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.presentation == Presentation::Window {
            crate::theme::sync_window(window, cx);
        }
        let title = brand_title!("视频查看", "Video viewer");
        let name = self
            .path
            .as_deref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        if self.presentation == Presentation::Window {
            window.set_window_title(
                &name
                    .as_ref()
                    .map_or_else(|| title.to_string(), |name| format!("{name} - {title}")),
            );
        }
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let p = crate::theme::palette(cx);
        let narrow = if self.presentation == Presentation::Popup {
            self.popup_surface_size
                .map_or(true, |size| f32::from(size.width) < 720.)
        } else {
            f32::from(window.viewport_size().width) < 720.
        };
        let (position, duration) = self
            .player
            .as_ref()
            .map_or((Duration::ZERO, Duration::ZERO), Player::position);
        let playing = self.player.as_ref().is_some_and(Player::playing);
        let seeking = self.player.as_ref().is_some_and(Player::seeking);
        let error = self
            .open_error
            .clone()
            .or_else(|| self.player.as_ref().and_then(Player::error));
        let ended = !duration.is_zero() && position >= duration && !playing && !seeking;
        let media_ratio = if duration.is_zero() {
            0.
        } else {
            (position.as_secs_f64() / duration.as_secs_f64()) as f32
        };
        let ratio = self.seek_drag_ratio.unwrap_or(media_ratio);
        let mut picture = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x000000));
        let frame_painted = Rc::new(Cell::new(false));
        if let Some(frame) = &self.image {
            let submitting = frame.clone();
            let frame_receipt = frame_painted.clone();
            let weak = cx.entity().downgrade();
            picture = picture.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        frame_receipt.set(false);
                        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                            return;
                        }
                        let image_size = submitting.image.size(0);
                        if u32::from(image_size.width) == 0 || u32::from(image_size.height) == 0 {
                            return;
                        }
                        let fitted = ObjectFit::Contain.get_bounds(bounds, image_size);
                        let visible = fitted.intersect(&window.content_mask().bounds);
                        if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
                            return;
                        }
                        if let Err(error) = window.paint_image(
                            fitted,
                            px(0.).into(),
                            submitting.image.clone(),
                            0,
                            false,
                        ) {
                            let message = trf!(
                                "无法绘制视频画面：{error}",
                                "Cannot draw the video frame: {error}"
                            );
                            let _ = weak.update(cx, |view, cx| {
                                if view.image.as_ref().is_some_and(|current| {
                                    Arc::ptr_eq(&current.image, &submitting.image)
                                }) && view.frame_draw_error.as_deref() != Some(message.as_str())
                                {
                                    view.frame_draw_error = Some(message);
                                    defer_video_notify(cx);
                                }
                            });
                            return;
                        }
                        frame_receipt.set(true);
                        let _ = weak.update(cx, |view, cx| {
                            if !view.image.as_ref().is_some_and(|current| {
                                current.serial == submitting.serial
                                    && Arc::ptr_eq(&current.image, &submitting.image)
                            }) {
                                return;
                            }
                            let recovered = view.frame_draw_error.take().is_some();
                            #[cfg(test)]
                            view.screen_roi_bounds.set(Some(fitted));
                            if view.submitted_frame.as_ref().is_some_and(|previous| {
                                previous.serial == submitting.serial
                                    && Arc::ptr_eq(&previous.image, &submitting.image)
                            }) {
                                if recovered {
                                    defer_video_notify(cx);
                                }
                                return;
                            }
                            if let Some(player) = &view.player {
                                player.observe_submitted(submitting.serial);
                            }
                            let first_frame = view.submitted_frame.is_none();
                            // 只确认本次场景已绘制的帧；不把Submitted当作显示器Presented。
                            view.submitted_frame = Some(submitting.clone());
                            if view.presentation != Presentation::Embedded && !view.recent_recorded
                            {
                                view.recent_recorded = true;
                                view.record_recent_current(cx);
                            }
                            if first_frame || recovered {
                                defer_video_notify(cx);
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        if let (Some(frame), Some(track)) = (&self.image, &self.subtitles) {
            if let Some(text) = self
                .subtitle_offset
                .query_position(frame.position)
                .and_then(|position| subtitle_text(track, position))
            {
                picture = picture.child(video_view_subtitle_gate::gated_subtitle(
                    div()
                        .absolute()
                        .left_0()
                        .bottom(px(16.))
                        .w_full()
                        .px(px(18.))
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .max_w(px(900.))
                                .max_h(px(140.))
                                .overflow_hidden()
                                .px(px(12.))
                                .py(px(7.))
                                .rounded_md()
                                .bg(gpui::rgba(0x000000cc))
                                .text_color(rgb(0xffffff))
                                .text_size(px(18.))
                                .line_clamp(4)
                                .text_ellipsis()
                                .text_center()
                                .child(text),
                        )
                        .into_any_element(),
                    frame_painted.clone(),
                ));
            }
        }
        let message = error
            .as_ref()
            .map(|error| trf!("无法播放此视频：{error}", "Cannot play this video: {error}"))
            .or_else(|| {
                self.image.is_none().then(|| {
                    if self.player.is_some() || self.opening.is_some() {
                        tr!("正在加载视频…", "Loading video…").to_string()
                    } else {
                        tr!("打开一个视频文件开始播放", "Open a video to start playing").to_string()
                    }
                })
            });
        if let Some(message) = message {
            picture = picture.child(
                div()
                    .p(px(24.))
                    .max_w(px(640.))
                    .text_size(px(14.))
                    .text_color(rgb(0xeeeeee))
                    .text_center()
                    .child(message),
            );
        }
        let picture = picture.on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, _, window, cx| {
                window.focus(&view.focus);
                view.toggle_playback();
                cx.stop_propagation();
                cx.notify();
            }),
        );
        let seek = slider("video-progress", ratio, self.seek_bounds.clone(), cx).on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, window, cx| {
                window.focus(&view.focus);
                view.begin_scrub(Scrub::Seek, event.position.x, window);
                cx.stop_propagation();
                cx.notify();
            }),
        );
        if self.presentation == Presentation::Embedded {
            return self.render_embedded(picture, seek, position, duration, playing, ended, cx);
        }
        let volume = slider(
            "video-volume",
            if self.muted { 0. } else { self.volume },
            self.volume_bounds.clone(),
            cx,
        )
        .w(px(if narrow { 64. } else { 100. }))
        .flex_shrink_0()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, window, cx| {
                view.begin_scrub(Scrub::Volume, event.position.x, window);
                cx.stop_propagation();
                cx.notify();
            }),
        );
        let play_label = if playing {
            tr!("暂停", "Pause")
        } else if ended {
            tr!("重播", "Replay")
        } else {
            tr!("播放", "Play")
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .w_full()
            .child(
                button("video-play", play_label, false, cx).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.toggle_playback();
                        cx.notify();
                    },
                )),
            )
            .child(
                button(
                    "video-step-forward",
                    tr!("下一帧 (N)", "Next frame (N)"),
                    false,
                    cx,
                )
                .opacity(if self.image.is_some() { 1. } else { 0.45 })
                .on_click(cx.listener(|view, _, _, cx| {
                    view.step_frame();
                    cx.notify();
                })),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .whitespace_nowrap()
                    .flex_shrink_0()
                    .child(format!(
                        "{} / {}",
                        video_shortcuts::format_time(position),
                        video_shortcuts::format_time(duration)
                    )),
            )
            .when(seeking, |row| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(tr!("正在定位…", "Seeking…")),
                )
            })
            .child(div().flex_1())
            .child(
                button(
                    "video-mute",
                    if self.muted || self.volume == 0. {
                        tr!("静音", "Muted")
                    } else {
                        tr!("音量", "Volume")
                    },
                    self.muted,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.toggle_muted();
                    cx.notify();
                })),
            )
            .child(volume)
            .child(self.audio_button(cx));
        if !narrow {
            row = row.child(
                div()
                    .w(px(36.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(format!("{:.0}%", self.volume * 100.)),
            );
        }
        let rate = self.player.as_ref().map_or(1., |player| player.rate);
        row = row
            .child(
                button("video-rate", format!("{rate}×"), false, cx).on_click(cx.listener(
                    |view, _, _, cx| {
                        if let Some(player) = &mut view.player {
                            let next = RATES
                                .iter()
                                .position(|rate| *rate == player.rate)
                                .map_or(1, |index| (index + 1) % RATES.len());
                            player.set_rate(RATES[next]);
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                button("video-repeat", tr!("循环", "Loop"), self.repeat, cx).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.repeat = !view.repeat;
                        view.repeat_armed = view.player.as_ref().is_some_and(Player::playing);
                        cx.notify();
                    },
                )),
            )
            .when(self.presentation == Presentation::Window, |row| {
                row.child(
                    button(
                        "video-fullscreen",
                        if window.is_fullscreen() {
                            tr!("退出全屏", "Exit full screen")
                        } else {
                            tr!("全屏", "Full screen")
                        },
                        false,
                        cx,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        if view.presentation == Presentation::Window {
                            window.toggle_fullscreen();
                        }
                        cx.notify();
                    })),
                )
            });
        let mut controls = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .px(px(16.))
            .pb(px(10.))
            .border_t_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.background))
            .child(seek)
            .child(row)
            .child(self.frame_controls(cx).child(self.gif_button(cx)))
            .child(self.subtitle_controls(cx))
            .child(self.gif_status(cx));
        let warning = self
            .notice
            .clone()
            .or_else(|| self.frame_draw_error.clone())
            .or_else(|| self.player.as_ref().and_then(Player::audio_error));
        if let Some(warning) = warning {
            controls = controls.child(
                div()
                    .pt(px(5.))
                    .text_size(px(11.))
                    .text_color(rgb(p.danger))
                    .child(warning),
            );
        }
        if let Some(feedback) = &self.frame_feedback {
            controls = controls.child(
                div()
                    .pt(px(5.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(feedback.clone()),
            );
        }
        div()
            .id("video-view")
            .relative()
            .track_focus(&self.focus)
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(p.background))
            .text_color(rgb(p.foreground))
            .on_key_down(cx.listener(Self::shortcut))
            .on_drop(
                cx.listener(|view, paths: &gpui::ExternalPaths, window, cx| {
                    if view.prompt_pending {
                        return;
                    }
                    if let [path] = paths.paths() {
                        if view.path.is_some()
                            && path.extension().is_some_and(|extension| {
                                extension.eq_ignore_ascii_case("srt")
                                    || extension.eq_ignore_ascii_case("vtt")
                            })
                        {
                            view.load_subtitles(path.clone(), window, cx);
                            cx.stop_propagation();
                            return;
                        }
                    }
                    match video_open::queue_from_drop(paths) {
                        Ok(Some(queue)) => view.replace_queue(queue),
                        Ok(None) => {}
                        Err(error) => view.notice = Some(error),
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .when(self.presentation == Presentation::Popup, |root| {
                root.child(self.popup_surface_probe(cx))
            })
            .child(Self::scrub_gestures(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(8.))
                    .flex_shrink_0()
                    .child(
                        button("video-open", tr!("打开视频", "Open video"), false, cx)
                            .on_click(cx.listener(|view, _, _, cx| view.open_prompt(cx))),
                    )
                    .child(self.recent_button(cx))
                    .child(
                        button(
                            "video-details-toggle",
                            tr!("信息", "Info"),
                            self.show_details,
                            cx,
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.show_details = !view.show_details;
                            if view.show_details {
                                view.hide_recent();
                                view.hide_queue();
                                view.hide_audio();
                                view.hide_gif();
                            }
                            cx.notify();
                        })),
                    )
                    .when(self.queue.len() > 1, |header| {
                        let (position, count) = self.queue.position().unwrap_or((0, 0));
                        header
                            .child(
                                button("video-previous-file", "‹", false, cx)
                                    .opacity(if position > 1 { 1. } else { 0.4 })
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.navigate_files(-1, cx)),
                                    ),
                            )
                            .child(self.queue_button(cx))
                            .child(
                                button("video-next-file", "›", false, cx)
                                    .opacity(if position < count { 1. } else { 0.4 })
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.navigate_files(1, cx)),
                                    ),
                            )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(name.unwrap_or_else(|| {
                                tr!("只读查看", "Read-only viewer").to_string()
                            })),
                    )
                    .when(!narrow, |header| {
                        header.child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(tr!(
                            "拖入视频 · PgUp/PgDn 切换",
                            "Drop videos · PgUp/PgDn browse"
                        )))
                    }),
            )
            .children(self.recent_menu(cx))
            .children(self.queue_menu(cx))
            .when(self.show_details, |root| {
                root.child(video_controls_details::card(
                    self.path.as_deref(),
                    self.player.as_ref(),
                    cx,
                ))
            })
            .child(picture)
            .children(self.audio_menu(cx))
            .children(self.gif_panel(cx))
            .child(controls)
            .into_any_element()
    }
}

fn defer_video_notify(cx: &mut Context<VideoView>) {
    let notify = cx.entity().downgrade();
    cx.defer(move |cx| {
        let _ = notify.update(cx, |_, cx| cx.notify());
    });
}

fn retire_scene_frame(
    retiring: &mut Vec<Arc<RenderImage>>,
    submitted: Option<&SceneFrame>,
    previous: SceneFrame,
) {
    // 未绘制的帧不占atlas，直接释放；停绘期间不能把每个解码帧都积到队列。
    if submitted.is_some_and(|shown| Arc::ptr_eq(&shown.image, &previous.image))
        && !retiring
            .iter()
            .any(|image| Arc::ptr_eq(image, &previous.image))
    {
        retiring.push(previous.image);
    }
}

fn subtitle_text(track: &video_view_subtitles::SubtitleTrack, pts: Duration) -> Option<String> {
    let mut visible = track.visible_at(pts);
    let mut text = String::new();
    let mut remaining = 1000;
    let mut lines = 1;
    let mut truncated = false;
    'cues: for cue in visible.by_ref().take(8) {
        if !text.is_empty() {
            if lines == 4 {
                truncated = true;
                break;
            }
            text.push('\n');
            lines += 1;
        }
        for character in cue.text.chars() {
            if remaining == 0 || (character == '\n' && lines == 4) {
                truncated = true;
                break 'cues;
            }
            if character == '\n' {
                lines += 1;
            }
            text.push(character);
            remaining -= 1;
        }
    }
    if truncated || visible.next().is_some() {
        text.push('…');
    }
    (!text.is_empty()).then_some(text)
}

fn pin_origin(window: &Window) -> (i32, i32) {
    let mut bounds = windows::Win32::Foundation::RECT::default();
    if unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
            crate::gpui_capture::hwnd(window),
            &mut bounds,
        )
    }
    .is_ok()
    {
        (
            bounds.left.saturating_add(32),
            bounds.top.saturating_add(32),
        )
    } else {
        (0, 0)
    }
}

/// 独立查看进程入口；外部文件仅交给解码器读取，不参与录屏临时文件清理。
pub fn run(path: Option<PathBuf>) -> Result<()> {
    run_paths(path.into_iter().collect())
}

/// 显式多文件共用一个活动播放器；顺序保留，坏项由窗口显示，首尾不自动循环。
pub(crate) fn run_paths(paths: Vec<PathBuf>) -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let errors = Rc::new(RefCell::new(None::<String>));
    let final_errors = errors.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            let waiting_for_work = Rc::new(Cell::new(false));
            cx.on_window_closed(move |cx| {
                if !can_exit(cx) {
                    return;
                }
                if !has_pending_work() {
                    cx.quit();
                    return;
                }
                if waiting_for_work.replace(true) {
                    return;
                }
                let waiting = waiting_for_work.clone();
                // 窗口和播放器正常释放；配置落盘与输出取消清理在后台完成后退出。
                cx.spawn(async move |cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                    let finished = cx
                        .update(|cx| {
                            if !can_exit(cx) {
                                return true;
                            }
                            if !has_pending_work() {
                                cx.quit();
                                return true;
                            }
                            false
                        })
                        .unwrap_or(true);
                    if finished {
                        waiting.set(false);
                        break;
                    }
                })
                .detach();
            })
            .detach();
            if let Err(error) = VideoView::open_paths(paths, cx) {
                *errors.borrow_mut() = Some(error.to_string());
                cx.quit();
            }
            cx.activate(true);
        });
    let error = final_errors.borrow_mut().take();
    error.map_or(Ok(()), |error| Err(error.into()))
}

fn can_exit(cx: &App) -> bool {
    cx.windows().is_empty()
        && !crate::pin_workspace_runtime::is_restoring(cx)
        && !crate::pin_live_view::has_open_windows()
}

fn has_pending_work() -> bool {
    video_view_recent::has_pending_work() || ACTIVE_FRAME_WORK.load(Ordering::Acquire) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpainted_frames_do_not_accumulate_while_window_stops_drawing() {
        fn frame(serial: u64) -> SceneFrame {
            SceneFrame {
                serial,
                position: Duration::from_millis(serial),
                image: Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).unwrap(),
                )])),
            }
        }
        let submitted = frame(1);
        let mut current = submitted.clone();
        let mut retiring = Vec::new();
        let mut unpainted = Vec::new();
        // 模拟多次轮询都没有后续render/paint；只保留真实旧场景的纹理候选。
        for serial in 2..128 {
            let previous = std::mem::replace(&mut current, frame(serial));
            if serial > 2 {
                unpainted.push(Arc::downgrade(&previous.image));
            }
            retire_scene_frame(&mut retiring, Some(&submitted), previous);
        }
        assert_eq!(retiring.len(), 1);
        assert!(Arc::ptr_eq(&retiring[0], &submitted.image));
        assert!(unpainted.iter().all(|frame| frame.upgrade().is_none()));
        let last_unpainted = Arc::downgrade(&current.image);
        retire_scene_frame(&mut retiring, Some(&submitted), current);
        assert!(last_unpainted.upgrade().is_none());
        retire_scene_frame(&mut retiring, Some(&submitted), submitted.clone());
        assert_eq!(retiring.len(), 1);
        let shown = Arc::downgrade(&submitted.image);
        drop(submitted);
        assert!(shown.upgrade().is_some());
        drop(retiring);
        assert!(shown.upgrade().is_none());
    }

    #[test]
    fn abandoned_frame_completion_cleans_up_before_releasing_process_work() {
        struct Cleanup {
            work: std::sync::Weak<FrameWork>,
            cleaned: Arc<AtomicBool>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                assert!(self.work.upgrade().is_some());
                self.cleaned.store(true, Ordering::Release);
            }
        }
        let work = Arc::new(FrameWork::begin());
        let observing = Arc::downgrade(&work);
        let cleaned = Arc::new(AtomicBool::new(false));
        let completion = FrameCompletion {
            result: Cleanup {
                work: observing.clone(),
                cleaned: cleaned.clone(),
            },
            _work: work.clone(),
        };
        // 前台等待者已经消失，后台结果仍需完成暂存/剪贴板资源清理。
        drop(work);
        assert!(observing.upgrade().is_some());
        drop(completion);
        assert!(cleaned.load(Ordering::Acquire));
        assert!(observing.upgrade().is_none());
    }
}
