//! 二维码生成窗口；只生成图片或按明确操作复制，另存复用图片工具的保存事务。
#[path = "image_tool_qr.rs"]
pub(crate) mod core;

use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tools::{check_cancel, ToolError, ToolLimits, ToolResult},
    settings_widgets::ButtonKind,
    text_field::{TextField, TextFieldEvent},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    div, img, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    Focusable, KeyDownEvent, ObjectFit, Render, RenderImage, Subscription, Task, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

/// 输入停顿多久后自动生成；生成很快，停顿只为少做中间结果。
const AUTO_GENERATE_DELAY: Duration = Duration::from_millis(250);
/// 预览中二维码卡片的边长（逻辑像素）。
const PREVIEW_CARD: f32 = 280.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Generate,
    Copy,
}

#[derive(Clone)]
struct Job {
    kind: JobKind,
    request_id: u64,
    cancel: Arc<AtomicBool>,
}

struct QrResult {
    request_id: u64,
    image: Arc<Image>,
}

struct GeneratedPreview {
    image: Arc<Image>,
    preview: Arc<RenderImage>,
}

#[derive(Clone, Copy)]
enum Control {
    Scale(u32),
    Copy,
    Tools,
    Cancel,
    Close,
}

/// 每模块像素的四档；界面按成品大小称呼，确切像素显示在状态栏。
fn scales() -> [(&'static str, u32, &'static str); 4] {
    [
        ("qr-scale-2", 2, tr!("小", "S")),
        ("qr-scale-4", 4, tr!("中", "M")),
        ("qr-scale-8", 8, tr!("大", "L")),
        ("qr-scale-16", 16, tr!("特大", "XL")),
    ]
}

pub(crate) struct ImageQrView {
    text: Entity<TextField>,
    scale: u32,
    request_id: u64,
    job: Option<Job>,
    result: Option<QrResult>,
    preview: Option<Arc<RenderImage>>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    auto: Option<Task<()>>,
    _subscription: Subscription,
}

pub(crate) fn open(cx: &mut App) -> Result<WindowHandle<ImageQrView>> {
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(520.), px(640.)), |display| {
            let bounds = display.bounds();
            size(
                px(520_f32.min((f32::from(bounds.size.width) - 48.).max(400.))),
                px(640_f32.min((f32::from(bounds.size.height) - 72.).max(400.))),
            )
        });
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(brand_title!("生成二维码", "Create QR code"));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                let text = cx.new(|cx| {
                    TextField::new(
                        String::new(),
                        tr!("输入文字或链接", "Enter text or a link"),
                        cx,
                    )
                });
                // 打开即可输入；Esc 等按键仍冒泡到窗口根处理。
                window.focus(&text.focus_handle(cx));
                let focus_group = vec![text.downgrade()];
                text.update(cx, |field, cx| {
                    field.configure_focus(focus_group, window, cx)
                });
                let subscription = cx.subscribe_in(
                    &text,
                    window,
                    |view: &mut ImageQrView, _, event, window, cx| match event {
                        TextFieldEvent::Changed => {
                            if !view.closing {
                                view.invalidate(cx);
                                view.schedule_generate(window, cx);
                            }
                        }
                        TextFieldEvent::Submit => {
                            view.auto = None;
                            view.generate(window, cx);
                        }
                    },
                );
                ImageQrView {
                    text,
                    scale: 4,
                    request_id: 1,
                    job: None,
                    result: None,
                    preview: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
                    notice: String::new(),
                    failed: false,
                    closing: false,
                    focus,
                    auto: None,
                    _subscription: subscription,
                }
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view
        },
    )?)
}

impl ImageQrView {
    fn busy(&self) -> bool {
        self.closing || self.job.is_some()
    }

    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
            self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
            self.failed = false;
            cx.notify();
            false
        } else {
            true
        }
    }

    fn clear_result(&mut self) {
        self.result = None;
        if let Some(preview) = self.preview.take() {
            self.retiring.push(preview);
        }
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.request_id = self.request_id.wrapping_add(1);
        self.clear_result();
        self.notice.clear();
        self.failed = false;
        cx.notify();
    }

    fn ready(&self, cx: &App) -> bool {
        !self.busy()
            && !self.text.read(cx).is_composing()
            && self
                .result
                .as_ref()
                .is_some_and(|result| result.request_id == self.request_id)
    }

    fn begin(&mut self, kind: JobKind, cx: &mut Context<Self>) -> Option<Job> {
        if self.busy() {
            return None;
        }
        let job = Job {
            kind,
            request_id: self.request_id,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        self.job = Some(job.clone());
        // 生成时保持可输入：新内容让本次结果过期，完成后按最新内容重来。
        if kind == JobKind::Copy {
            self.text.update(cx, |text, cx| text.set_enabled(false, cx));
        }
        self.notice.clear();
        self.failed = false;
        Some(job)
    }

    fn finish(&mut self, job: &Job, cx: &mut Context<Self>) -> bool {
        if !self
            .job
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.cancel, &job.cancel))
        {
            return false;
        }
        self.job = None;
        self.text.update(cx, |text, cx| text.set_enabled(true, cx));
        true
    }

    fn has_text(&self, cx: &App) -> bool {
        let length = self.text.read(cx).text().len();
        length > 0 && length <= core::MAX_TEXT_BYTES
    }

    fn schedule_generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_text(cx) {
            self.auto = None;
            return;
        }
        self.auto = Some(cx.spawn_in(window, async move |weak, cx| {
            cx.background_executor().timer(AUTO_GENERATE_DELAY).await;
            let _ = weak.update_in(cx, |view, window, cx| {
                view.auto = None;
                view.generate(window, cx);
            });
        }));
    }

    fn generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.text.read(cx).is_composing() {
            return;
        }
        let text = self.text.read(cx).text();
        if text.is_empty() || text.len() > core::MAX_TEXT_BYTES {
            self.notice = trf!(
                "请输入不超过 {} UTF-8 字节的内容",
                "Enter at most {} UTF-8 bytes",
                core::MAX_TEXT_BYTES
            );
            self.failed = true;
            cx.notify();
            return;
        }
        let text = text.to_owned();
        let scale = self.scale;
        self.clear_result();
        let Some(job) = self.begin(JobKind::Generate, cx) else {
            return;
        };
        let work = job.clone();
        let task = cx.background_executor().spawn(async move {
            let image = core::generate(&text, scale, ToolLimits::default(), &work.cancel)?;
            let preview = preview_image(&image, &work.cancel)?;
            check_cancel(&work.cancel)?;
            Ok::<_, ToolError>(GeneratedPreview {
                image: Arc::new(image),
                preview,
            })
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.finish(&job, cx) {
                    return;
                }
                if view.closing {
                    drop(result);
                    window.remove_window();
                    return;
                }
                if job.cancel.load(Ordering::Acquire) {
                    drop(result);
                    view.notice = tr!("已取消生成", "Generation cancelled").into();
                } else if job.request_id != view.request_id {
                    // 生成期间内容或尺寸变了：丢弃旧结果，按最新内容重来。
                    drop(result);
                    if view.has_text(cx) && view.auto.is_none() {
                        view.generate(window, cx);
                    }
                } else {
                    match result {
                        Ok(result) => {
                            view.result = Some(QrResult {
                                request_id: job.request_id,
                                image: result.image,
                            });
                            view.preview = Some(result.preview);
                        }
                        Err(error) => {
                            view.notice = error.to_string();
                            view.failed = error != ToolError::Cancelled;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        if sequence == 0 {
            self.notice = tr!(
                "无法读取剪贴板状态，请重试",
                "Cannot read clipboard state; try again"
            )
            .into();
            self.failed = true;
            cx.notify();
            return;
        }
        let image = self.result.as_ref().unwrap().image.clone();
        let Some(job) = self.begin(JobKind::Copy, cx) else {
            return;
        };
        let work = job.clone();
        let task = cx.background_executor().spawn(async move {
            check_cancel(&work.cancel)?;
            let prepared = crate::export::prepare_image_clipboard_cancellable(&image, &work.cancel)
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            check_cancel(&work.cancel)?;
            Ok::<_, ToolError>(prepared)
        });
        cx.spawn_in(window, async move |weak, cx| {
            let prepared = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.finish(&job, cx) {
                    return;
                }
                if view.closing {
                    drop(prepared);
                    window.remove_window();
                    return;
                }
                if job.cancel.load(Ordering::Acquire) || job.request_id != view.request_id {
                    drop(prepared);
                    view.notice = tr!("已取消复制", "Copy cancelled").into();
                    view.failed = false;
                } else {
                    match prepared.and_then(|prepared| {
                        prepared
                            .publish_if_unchanged(crate::gpui_capture::hwnd(window), sequence)
                            .map_err(|error| ToolError::Failed(error.to_string()))
                    }) {
                        Ok(()) => {
                            view.notice = tr!("已复制二维码图片", "QR image copied").into();
                            view.failed = false;
                        }
                        Err(error) => {
                            view.notice = error.to_string();
                            view.failed = true;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Control::Close => {
                if self.request_close(cx) {
                    window.remove_window();
                }
            }
            Control::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Release);
                }
                self.notice = tr!(
                    "已请求取消，等待当前任务返回",
                    "Cancellation requested; waiting for the current task"
                )
                .into();
                self.failed = false;
                cx.notify();
            }
            _ if self.busy() => {}
            Control::Copy => self.copy(window, cx),
            Control::Scale(scale) => {
                if self.scale != scale {
                    self.scale = scale;
                    self.invalidate(cx);
                    if self.has_text(cx) {
                        self.auto = None;
                        self.generate(window, cx);
                    }
                }
            }
            Control::Tools => {
                if !self.ready(cx) {
                    return;
                }
                let image = self.result.as_ref().unwrap().image.clone();
                if let Err(error) = crate::image_tool_view::open_shared(image, Vec::new(), cx) {
                    self.notice = error.to_string();
                    self.failed = true;
                    cx.notify();
                }
            }
        }
    }

    fn status(&self) -> &'static str {
        if self.closing {
            tr!("正在取消并关闭…", "Cancelling and closing…")
        } else if let Some(job) = &self.job {
            match job.kind {
                JobKind::Generate => tr!("正在生成二维码…", "Generating QR code…"),
                JobKind::Copy => tr!("正在准备复制图片…", "Preparing image copy…"),
            }
        } else {
            tr!(
                "输入文字或链接后自动生成",
                "Type text or a link to generate"
            )
        }
    }
}

impl Drop for ImageQrView {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageQrView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let c = chrome::chrome(cx);
        let palette = c.palette;
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let enabled = !self.busy();
        let ready = self.ready(cx);
        let length = self.text.read(cx).text().len();
        let too_long = length > core::MAX_TEXT_BYTES;

        let mut sizes = crate::settings_widgets::segmented(c.surface);
        for (id, scale, label) in scales() {
            let segment = chrome::segment(id, label, self.scale == scale, enabled, c);
            sizes = sizes.child(if enabled {
                segment.on_click(cx.listener(move |view, _, window, cx| {
                    view.control(Control::Scale(scale), window, cx)
                }))
            } else {
                segment
            });
        }

        let stage = chrome::stage(c)
            .flex_1()
            .min_h(px(200.))
            .child(match &self.preview {
                Some(image) => div()
                    .size(px(PREVIEW_CARD))
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(0xffffff))
                    .shadow_md()
                    .child(
                        img(image.clone())
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element(),
                None => chrome::empty_state(
                    Icon::Qr,
                    if self.job.is_some() {
                        self.status()
                    } else {
                        tr!(
                            "在上方输入文字或链接，二维码会自动生成",
                            "Type text or a link above; the QR code appears here"
                        )
                    },
                    c,
                )
                .into_any_element(),
            });

        let (status, tone): (String, Tone) = if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else if too_long {
            (
                trf!(
                    "内容超过 {} 字节，请缩短",
                    "Content exceeds {} bytes; shorten it",
                    core::MAX_TEXT_BYTES
                ),
                Tone::Failed,
            )
        } else if let (Some(result), true) = (&self.result, ready) {
            (
                trf!(
                    "已生成 · {} × {} 像素",
                    "Ready · {} × {} px",
                    result.image.width,
                    result.image.height
                ),
                Tone::Done,
            )
        } else if self.busy() {
            (self.status().to_owned(), Tone::Busy)
        } else {
            (self.status().to_owned(), Tone::Idle)
        };

        let mut actions = Vec::new();
        if self.job.is_some() {
            actions.push(
                chrome::action(
                    "qr-cancel",
                    tr!("取消", "Cancel"),
                    ButtonKind::Secondary,
                    true,
                    c,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Cancel, window, cx)),
                )
                .into_any_element(),
            );
        }
        for (id, label, kind, action) in [
            (
                "qr-tools",
                tr!("编辑或另存…", "Edit or save…"),
                ButtonKind::Secondary,
                Control::Tools,
            ),
            (
                "qr-copy",
                tr!("复制图片", "Copy image"),
                ButtonKind::Primary,
                Control::Copy,
            ),
        ] {
            let button = chrome::action(id, label, kind, ready, c);
            actions.push(
                if ready {
                    button.on_click(
                        cx.listener(move |view, _, window, cx| view.control(action, window, cx)),
                    )
                } else {
                    button
                }
                .into_any_element(),
            );
        }

        let body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(chrome::field_label(tr!("内容", "Content"), c))
                    .child(self.text.clone())
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .text_size(px(11.))
                            .text_color(rgb(if too_long {
                                palette.danger
                            } else {
                                palette.muted
                            }))
                            .child(format!("{length} / {}", core::MAX_TEXT_BYTES)),
                    ),
            )
            .child(stage)
            .child(chrome::option_row(tr!("尺寸", "Size"), c).child(sizes));

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.is_held || view.text.read(cx).is_composing() {
                    return;
                }
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    view.control(
                        if view.job.is_some() {
                            Control::Cancel
                        } else {
                            Control::Close
                        },
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
            }))
            .child(body)
            .child(chrome::footer(status, tone, actions, c))
            .into_any_element()
    }
}

fn preview_image(image: &Image, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
    let scale = (512. / f64::from(image.width))
        .min(512. / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * scale).round().max(1.) as u32;
    let height = (f64::from(image.height) * scale).round().max(1.) as u32;
    let preview = resize(
        image,
        width,
        height,
        ResizeFilter::Nearest,
        ToolLimits::default(),
        cancel,
    )?;
    check_cancel(cancel)?;
    Ok(crate::canvas_source::render_image(
        width,
        height,
        preview.bgra,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_qr_view_preview_is_bounded_and_keeps_generated_pixels_unmodified() {
        let cancel = AtomicBool::new(false);
        let text = "bounded preview ".repeat(8);
        let image = core::generate(&text, 16, ToolLimits::default(), &cancel).unwrap();
        assert!(image.width > 512);
        let before = image.bgra.clone();
        let preview = preview_image(&image, &cancel).unwrap();
        assert!(u32::from(preview.size(0).width) <= 512);
        assert!(u32::from(preview.size(0).height) <= 512);
        assert_eq!(image.bgra, before);
        assert!(preview
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .all(|pixel| pixel == [0, 0, 0, 255] || pixel == [255; 4]));
        assert!(matches!(
            preview_image(&image, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }
}
