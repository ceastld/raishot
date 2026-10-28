//! 视频文件的独立 GIF 导出会话；参数与播放控制分离，隐藏面板不会中断导出。
use super::{button, video_shortcuts, FrameCompletion, FrameWork, VideoView};
use crate::gif_export::GifOptions as Options;
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Div, Stateful, Window};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Copy)]
enum Change {
    Whole,
    Start,
    End,
    Fps(u32),
    Side(u32),
    Colors(usize),
}

#[derive(Clone, Copy)]
enum Phase {
    Choosing,
    Preparing,
}

struct GifJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    generation: u64,
    source: PathBuf,
    options: Options,
    phase: Phase,
    report_cancel: bool,
}

impl Drop for GifJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

struct GifResult {
    path: PathBuf,
    source: PathBuf,
    generation: u64,
    options: Options,
}

#[derive(Default)]
pub(super) struct GifSession {
    visible: bool,
    options: Options,
    job: Option<GifJob>,
    result: Option<GifResult>,
    feedback: Option<String>,
}

impl VideoView {
    pub(super) fn gif_visible(&self) -> bool {
        self.gif_export.visible
    }

    pub(super) fn gif_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        button(
            "video-gif-toggle",
            tr!("转 GIF", "Export GIF"),
            self.gif_export.visible,
            cx,
        )
        .on_click(cx.listener(|view, _, _, cx| {
            if view.prompt_pending {
                return;
            }
            view.gif_export.visible = !view.gif_export.visible;
            if view.gif_export.visible {
                view.hide_recent();
                view.hide_queue();
                view.hide_audio();
                view.show_details = false;
            }
            cx.notify();
        }))
    }

    pub(super) fn hide_gif(&mut self) {
        self.gif_export.visible = false;
    }

    pub(super) fn reset_gif_for_source(&mut self) {
        self.cancel_gif_export(false);
        self.hide_gif();
        self.gif_export.options.start = Duration::ZERO;
        self.gif_export.options.end = None;
        self.gif_export.result = None;
        self.gif_export.feedback = None;
    }

    pub(super) fn cancel_gif_export(&mut self, report: bool) {
        if let Some(job) = &mut self.gif_export.job {
            // 后台归还准备对象之前继续占用单任务槽，取消不会堆积解码任务。
            job.cancel.store(true, Ordering::Release);
            job.report_cancel |= report;
            if report {
                self.gif_export.feedback =
                    Some(tr!("已请求取消 GIF 导出", "GIF export cancellation requested").into());
            }
        }
    }

    pub(super) fn gif_export_active(&self) -> bool {
        self.gif_export
            .job
            .as_ref()
            .is_some_and(|job| !job.cancel.load(Ordering::Acquire))
    }

    fn gif_options_locked(&self) -> bool {
        self.prompt_pending || self.gif_export.job.is_some()
    }

    fn accept_gif_result(&mut self, cancel: &Arc<AtomicBool>) -> bool {
        let Some(job) = &self.gif_export.job else {
            return false;
        };
        if !Arc::ptr_eq(&job.cancel, cancel) {
            return false;
        }
        let current = job.generation == self.source_generation
            && self.path.as_ref() == Some(&job.source)
            && job.options == self.gif_export.options;
        if !current || cancel.load(Ordering::Acquire) {
            let job = self.gif_export.job.take().expect("matching GIF job");
            if current && job.report_cancel {
                self.gif_export.feedback =
                    Some(tr!("已取消 GIF 导出", "GIF export canceled").into());
            }
            return false;
        }
        true
    }

    fn watch_gif_progress(
        &self,
        cancel: Arc<AtomicBool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |weak, cx| {
            let mut previous = 0;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let running = weak
                    .update_in(cx, |view, _, cx| {
                        if !view.session_active {
                            return false;
                        }
                        let Some(job) = &view.gif_export.job else {
                            return false;
                        };
                        if !Arc::ptr_eq(&job.cancel, &cancel) {
                            return false;
                        }
                        let progress = job.progress.load(Ordering::Acquire) / 100;
                        if progress != previous {
                            previous = progress;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_gif_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.gif_options_locked() {
            return;
        }
        let source = match self
            .path
            .as_ref()
            .filter(|_| self.submitted_frame.is_some())
        {
            Some(path) => path.clone(),
            None => {
                self.gif_export.feedback = Some(
                    tr!(
                        "请先打开视频并等待画面显示",
                        "Open a video and wait for its picture first"
                    )
                    .into(),
                );
                cx.notify();
                return;
            }
        };
        let options = self.gif_export.options;
        let duration = self
            .player
            .as_ref()
            .map_or(Duration::ZERO, |player| player.position().1);
        if let Err(error) = validate_range(options, duration) {
            self.gif_export.feedback = Some(error);
            cx.notify();
            return;
        }
        let generation = self.source_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        self.gif_export.job = Some(GifJob {
            cancel: cancel.clone(),
            progress: progress.clone(),
            generation,
            source: source.clone(),
            options,
            phase: Phase::Choosing,
            report_cancel: false,
        });
        self.gif_export.result = None;
        self.gif_export.feedback = None;
        self.prompt_pending = true;
        let name = suggested_name(&source, options);
        let directory = source.parent().unwrap_or_else(|| Path::new("."));
        let receiver = cx.prompt_for_new_path(directory, Some(&name));
        cx.spawn_in(window, async move |weak, cx| {
            let target = match receiver.await {
                Ok(Ok(Some(path))) => Some(Ok(path)),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => Some(Err(error.to_string())),
            };
            let continuing = weak
                .update_in(cx, |view, window, cx| {
                    view.prompt_pending = false;
                    if !view.accept_gif_result(&cancel) {
                        cx.notify();
                        return false;
                    }
                    match &target {
                        Some(Ok(_)) => {
                            view.gif_export.job.as_mut().expect("current GIF job").phase =
                                Phase::Preparing;
                            view.watch_gif_progress(cancel.clone(), window, cx);
                            cx.notify();
                            true
                        }
                        Some(Err(error)) => {
                            view.gif_export.job.take();
                            view.gif_export.feedback = Some(trf!(
                                "无法打开 GIF 保存对话框：{error}",
                                "Cannot open the GIF save dialog: {error}"
                            ));
                            cx.notify();
                            false
                        }
                        None => {
                            view.gif_export.job.take();
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
            // 原生选择器等待不持有保活；真实准备到提交/拒绝的整个生命周期持有。
            let _work = Arc::new(FrameWork::begin());
            let preparing_work = _work.clone();
            let preparing_cancel = cancel.clone();
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let result = (|| {
                        if preparing_cancel.load(Ordering::Acquire) {
                            return Err(tr!("已取消 GIF 导出", "GIF export canceled").to_string());
                        }
                        crate::gif_export::prepare_with_options(
                            &source,
                            &target,
                            crate::gif_export::GifOptions {
                                start: options.start,
                                end: options.end,
                                fps: options.fps,
                                max_side: options.max_side,
                                colors: options.colors,
                            },
                            |value| {
                                if preparing_cancel.load(Ordering::Acquire) {
                                    return false;
                                }
                                if value.is_finite() {
                                    progress.store(
                                        (value.clamp(0., 1.) * 10_000.).round() as u32,
                                        Ordering::Release,
                                    );
                                }
                                !preparing_cancel.load(Ordering::Acquire)
                            },
                        )
                        .map_err(|error| error.to_string())
                    })();
                    FrameCompletion {
                        result,
                        _work: preparing_work,
                    }
                })
                .await;
            let _ = weak.update_in(cx, |view, _, cx| {
                if !view.accept_gif_result(&cancel) {
                    cx.notify();
                    return;
                }
                // 任务Arc、来源代次/路径及全部选项都仍匹配，才允许发布成品。
                let job = view.gif_export.job.as_ref().expect("current GIF job");
                let source = job.source.clone();
                let result = prepared.result.and_then(|prepared| {
                    prepared.commit(&cancel).map_err(|error| error.to_string())
                });
                view.gif_export.job.take();
                match result {
                    Ok(path) => {
                        view.gif_export.result = Some(GifResult {
                            path,
                            source,
                            generation,
                            options,
                        });
                        view.gif_export.feedback = Some(
                            tr!(
                                "GIF 已保存，可查看实际成品",
                                "GIF saved; the exported file is ready to view"
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        view.gif_export.feedback =
                            Some(trf!("GIF 导出失败：{error}", "GIF export failed: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn change_gif_options(&mut self, change: Change) {
        if self.gif_options_locked() {
            return;
        }
        let mut options = self.gif_export.options;
        match change {
            Change::Whole => {
                options.start = Duration::ZERO;
                options.end = None;
            }
            Change::Start | Change::End => {
                let Some(frame) = &self.submitted_frame else {
                    self.gif_export.feedback = Some(
                        tr!(
                            "当前没有已提交的画面可标记",
                            "No submitted frame is ready to mark"
                        )
                        .into(),
                    );
                    return;
                };
                match change {
                    Change::Start => options.start = frame.position,
                    Change::End => options.end = Some(frame.position),
                    _ => unreachable!(),
                }
            }
            Change::Fps(fps) => options.fps = fps,
            Change::Side(side) => options.max_side = side,
            Change::Colors(colors) => options.colors = colors,
        }
        let duration = self
            .player
            .as_ref()
            .map_or(Duration::ZERO, |player| player.position().1);
        if let Err(error) = validate_range(options, duration) {
            self.gif_export.feedback = Some(error);
            return;
        }
        if options != self.gif_export.options {
            self.gif_export.options = options;
            self.gif_export.result = None;
            self.gif_export.feedback = None;
        }
    }

    fn gif_option_button(
        &self,
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        selected: bool,
        change: Change,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        button(id, label, selected, cx)
            .opacity(if self.gif_options_locked() { 0.45 } else { 1. })
            .on_click(cx.listener(move |view, _, _, cx| {
                view.change_gif_options(change);
                cx.notify();
            }))
    }

    pub(super) fn gif_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.gif_export.visible {
            return None;
        }
        let p = crate::theme::palette(cx);
        let options = self.gif_export.options;
        let end = options
            .end
            .map(format_pts)
            .unwrap_or_else(|| tr!("片尾", "End").into());
        let mut panel = div()
            .id("video-gif-panel")
            .w_full()
            .min_h(px(0.))
            .max_h(px(248.))
            .overflow_y_scroll()
            .flex_shrink()
            .px(px(16.))
            .py(px(8.))
            .bg(rgb(p.background))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.foreground))
                    .child(trf!(
                        "范围：{} → {end}（结束点不包含在内）",
                        "Range: {} → {end} (end excluded)",
                        format_pts(options.start)
                    )),
            )
            .child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(tr!(
                "GIF时间按0.01秒取整；标记使用当前已提交画面的时间。",
                "GIF time is rounded to 0.01s; marks use the submitted picture time."
            )))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .py(px(5.))
                    .child(self.gif_option_button(
                        "video-gif-start",
                        tr!("画面设为起点", "Mark start"),
                        false,
                        Change::Start,
                        cx,
                    ))
                    .child(self.gif_option_button(
                        "video-gif-end",
                        tr!("画面设为终点", "Mark end"),
                        false,
                        Change::End,
                        cx,
                    ))
                    .child(self.gif_option_button(
                        "video-gif-whole",
                        tr!("整段", "Whole video"),
                        options.start.is_zero() && options.end.is_none(),
                        Change::Whole,
                        cx,
                    )),
            );
        let mut fps = option_row(tr!("帧率", "Frame rate"), cx);
        for (value, id) in [
            (5, "video-gif-fps-5"),
            (10, "video-gif-fps-10"),
            (15, "video-gif-fps-15"),
        ] {
            fps = fps.child(self.gif_option_button(
                id,
                format!("{value} fps"),
                options.fps == value,
                Change::Fps(value),
                cx,
            ));
        }
        let mut size = option_row(tr!("最长边", "Max side"), cx);
        for (value, id) in [
            (320, "video-gif-side-320"),
            (640, "video-gif-side-640"),
            (960, "video-gif-side-960"),
            (1280, "video-gif-side-1280"),
        ] {
            size = size.child(self.gif_option_button(
                id,
                format!("{value}px"),
                options.max_side == value,
                Change::Side(value),
                cx,
            ));
        }
        let mut colors = option_row(tr!("颜色上限", "Color limit"), cx);
        for (value, id) in [
            (64, "video-gif-colors-64"),
            (128, "video-gif-colors-128"),
            (256, "video-gif-colors-256"),
        ] {
            colors = colors.child(self.gif_option_button(
                id,
                value.to_string(),
                options.colors == value,
                Change::Colors(value),
                cx,
            ));
        }
        panel = panel.child(fps).child(size).child(colors);
        let duration = self
            .player
            .as_ref()
            .map_or(Duration::ZERO, |player| player.position().1);
        if range_duration(options, duration).is_some_and(|span| span > Duration::from_secs(60)) {
            panel = panel.child(
                div()
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "所选范围超过60秒，GIF可能较大；可缩短范围或降低帧率、尺寸。",
                        "Over 60 seconds: the GIF may be large. Shorten the range or reduce frame rate and size."
                    )),
            );
        }
        panel = panel.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .pt(px(5.))
                .child(
                    button("video-gif-export", tr!("另存 GIF…", "Save GIF…"), false, cx)
                        .opacity(if self.gif_options_locked() { 0.45 } else { 1. })
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.start_gif_export(window, cx);
                        })),
                )
                .child(
                    button("video-gif-hide", tr!("收起", "Hide"), false, cx).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.hide_gif();
                            cx.notify();
                        },
                    )),
                ),
        );
        Some(panel.into_any_element())
    }

    pub(super) fn gif_status(&self, cx: &mut Context<Self>) -> Div {
        let p = crate::theme::palette(cx);
        let mut status = div().flex().flex_col().gap(px(4.));
        if let Some(job) = &self.gif_export.job {
            let canceling = job.cancel.load(Ordering::Acquire);
            let text = if canceling {
                tr!("正在取消 GIF 导出…", "Canceling GIF export…").to_string()
            } else if matches!(job.phase, Phase::Choosing) {
                tr!("请选择 GIF 保存位置", "Choose a GIF destination").to_string()
            } else {
                let progress = job.progress.load(Ordering::Acquire).min(10_000) / 100;
                trf!("GIF 导出：{progress}%", "GIF export: {progress}%")
            };
            status = status.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(text),
                    )
                    .child(
                        button(
                            "video-gif-cancel",
                            tr!("取消导出", "Cancel export"),
                            false,
                            cx,
                        )
                        .opacity(if canceling { 0.45 } else { 1. })
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.cancel_gif_export(true);
                            cx.notify();
                        })),
                    ),
            );
        }
        if let Some(feedback) = &self.gif_export.feedback {
            status = status.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(feedback.clone()),
            );
        }
        if let Some(result) = self.gif_export.result.as_ref().filter(|result| {
            result.generation == self.source_generation
                && self.path.as_ref() == Some(&result.source)
                && result.options == self.gif_export.options
        }) {
            let path = result.path.clone();
            status = status.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        button(
                            "video-gif-open-result",
                            tr!("查看 GIF", "View GIF"),
                            false,
                            cx,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if view.gif_options_locked() {
                                return;
                            }
                            let current = view.gif_export.result.as_ref().is_some_and(|result| {
                                result.path == path
                                    && result.generation == view.source_generation
                                    && view.path.as_ref() == Some(&result.source)
                                    && result.options == view.gif_export.options
                            });
                            if current {
                                if let Err(error) = crate::file_launch_command::open_image(&path) {
                                    view.gif_export.feedback = Some(trf!(
                                        "无法打开 GIF 成品：{error}",
                                        "Cannot open the exported GIF: {error}"
                                    ));
                                    cx.notify();
                                }
                            }
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(result.path.display().to_string()),
                    ),
            );
        }
        status
    }
}

fn option_row(label: &'static str, cx: &Context<VideoView>) -> Div {
    let p = crate::theme::palette(cx);
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(6.))
        .py(px(3.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(p.muted))
                .child(label),
        )
}

fn format_pts(time: Duration) -> String {
    format!(
        "{}.{:02}",
        video_shortcuts::format_time(time),
        time.as_millis() / 10 % 100
    )
}

fn suggested_name(source: &Path, options: Options) -> String {
    let suffix = if options.start.is_zero() && options.end.is_none() {
        ".gif".to_string()
    } else {
        let end = options
            .end
            .map(|end| end.as_millis().to_string())
            .unwrap_or_else(|| "end".into());
        format!("_{}-{end}.gif", options.start.as_millis())
    };
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let mut budget = 255_usize.saturating_sub(suffix.encode_utf16().count());
    let stem: String = stem
        .chars()
        .take_while(|character| {
            let units = character.len_utf16();
            if units > budget {
                return false;
            }
            budget -= units;
            true
        })
        .collect();
    let stem = if stem.is_empty() { "Video" } else { &stem };
    format!("{stem}{suffix}")
}

fn range_duration(options: Options, duration: Duration) -> Option<Duration> {
    options
        .end
        .or_else(|| (!duration.is_zero()).then_some(duration))?
        .checked_sub(options.start)
        .filter(|span| !span.is_zero())
}

fn validate_range(options: Options, duration: Duration) -> Result<(), String> {
    if options.end.is_some_and(|end| end <= options.start) {
        return Err(tr!(
            "GIF终点必须晚于起点；标记的终点画面不包含在输出内",
            "GIF end must follow start; the marked end frame is excluded"
        )
        .into());
    }
    if !duration.is_zero()
        && (options.start >= duration || options.end.is_some_and(|end| end > duration))
    {
        return Err(tr!(
            "GIF范围超出当前视频时长",
            "The GIF range exceeds the video duration"
        )
        .into());
    }
    if range_duration(options, duration).is_some_and(|span| span < Duration::from_millis(10)) {
        return Err(tr!(
            "GIF范围至少需要0.01秒，请重新标记起点或终点",
            "The GIF range must be at least 0.01s; mark a new start or end"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_rejects_reversed_short_and_out_of_source_marks() {
        let options = Options {
            start: Duration::from_millis(120),
            end: Some(Duration::from_millis(130)),
            ..Options::default()
        };
        let duration = Duration::from_secs(1);
        assert!(validate_range(options, duration).is_ok());
        assert_eq!(
            range_duration(options, duration),
            Some(Duration::from_millis(10))
        );
        for end in [119, 120, 129, 1001] {
            assert!(validate_range(
                Options {
                    end: Some(Duration::from_millis(end)),
                    ..options
                },
                duration
            )
            .is_err());
        }
        assert_eq!(range_duration(Options::default(), duration), Some(duration));
        assert!(validate_range(Options::default(), Duration::ZERO).is_ok());
    }

    #[test]
    fn suggested_name_keeps_unicode_within_the_filename_budget() {
        let source = PathBuf::from(format!("{}.mp4", "片段🙂".repeat(100)));
        let name = suggested_name(
            &source,
            Options {
                start: Duration::from_millis(1234),
                end: Some(Duration::from_millis(5678)),
                ..Options::default()
            },
        );
        assert!(name.encode_utf16().count() <= 255);
        assert!(name.ends_with("_1234-5678.gif"));
        assert!(name.starts_with("片段🙂"));
    }

    #[test]
    fn dropping_session_cancels_its_outstanding_job() {
        let cancel = Arc::new(AtomicBool::new(false));
        let session = GifSession {
            job: Some(GifJob {
                cancel: cancel.clone(),
                progress: Arc::new(AtomicU32::new(0)),
                generation: 1,
                source: PathBuf::from("video.mp4"),
                options: Options::default(),
                phase: Phase::Preparing,
                report_cancel: false,
            }),
            ..GifSession::default()
        };
        drop(session);
        assert!(cancel.load(Ordering::Acquire));
    }
}
