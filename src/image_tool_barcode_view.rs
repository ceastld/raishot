//! 一维条码识别窗口；来源快照只读，扫描线仅在预览中绘制。
#[path = "image_tool_barcode.rs"]
pub(crate) mod model;

use self::model::{
    BarcodeFormat, BarcodeHit, ScanLimits, ScanOptions, ScanProgress, ScanReport, ScanStatus,
};
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tool_view::geometry::ContentRect,
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    ElementId, FocusHandle, KeyDownEvent, PathBuilder, Render, RenderImage, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const PAGE_SIZE: usize = 32;
const SUMMARY_CHARS: usize = 160;
const MAX_COPY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Control {
    Format(BarcodeFormat),
    All,
    Inverted,
    Scan,
    Cancel,
    Select(usize),
    Page(usize),
    CopyCurrent,
    CopyAll,
    Close,
}

struct ActiveScan {
    request_id: u64,
    cancel: Arc<AtomicBool>,
    latest: Arc<Mutex<ScanProgress>>,
}

struct ScanOutput {
    preview: Option<Arc<RenderImage>>,
    report: ToolResult<ScanReport>,
}

pub(crate) struct ImageBarcodeView {
    source: Arc<ProcessedImage>,
    options: ScanOptions,
    request_id: u64,
    active: Option<ActiveScan>,
    preview: Option<Arc<RenderImage>>,
    report: Option<ScanReport>,
    progress: ScanProgress,
    selected: Option<usize>,
    page: usize,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
}

pub(crate) fn open_processed(
    source: Arc<ProcessedImage>,
    cx: &mut App,
) -> Result<WindowHandle<ImageBarcodeView>> {
    validate_input(source.image(), ToolLimits::default())?;
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(1060.), px(860.)), |display| {
            let bounds = display.bounds();
            size(
                px(1060_f32.min((f32::from(bounds.size.width) - 48.).max(440.))),
                px(860_f32.min((f32::from(bounds.size.height) - 72.).max(420.))),
            )
        });
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(brand_title!(
                "一维条码识别（实验）",
                "Barcode recognition (preview)"
            ));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                ImageBarcodeView {
                    source,
                    options: ScanOptions::default(),
                    request_id: 1,
                    active: None,
                    preview: None,
                    report: None,
                    progress: ScanProgress::default(),
                    selected: None,
                    page: 0,
                    notice: String::new(),
                    failed: false,
                    closing: false,
                    focus,
                }
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view.update(cx, |view, cx| view.start(window, cx));
            view
        },
    )?)
}

impl ImageBarcodeView {
    fn busy(&self) -> bool {
        self.active.is_some() || self.closing
    }

    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
            self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
            self.failed = false;
            cx.notify();
            false
        } else {
            true
        }
    }

    fn invalidate(&mut self) {
        self.request_id = self.request_id.wrapping_add(1);
        self.report = None;
        self.selected = None;
        self.page = 0;
        self.progress = ScanProgress::default();
        self.notice = tr!("选项已变化，请重新识别", "Options changed; scan again").into();
        self.failed = false;
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if let Err(error) = self.options.validate() {
            self.notice = error.to_string();
            self.failed = true;
            cx.notify();
            return;
        }
        window.focus(&self.focus);
        self.request_id = self.request_id.wrapping_add(1);
        let request_id = self.request_id;
        let source = self.source.clone();
        let options = self.options.clone();
        let prepare = self.preview.is_none();
        let cancel = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(ScanProgress::default()));
        self.active = Some(ActiveScan {
            request_id,
            cancel: cancel.clone(),
            latest: latest.clone(),
        });
        self.report = None;
        self.selected = None;
        self.page = 0;
        self.progress = ScanProgress::default();
        self.notice = if prepare {
            tr!("正在准备预览并识别…", "Preparing preview and scanning…")
        } else {
            tr!("正在识别…", "Scanning…")
        }
        .into();
        self.failed = false;
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            let preview = if prepare {
                Some(prepare_preview(source.image(), &work_cancel)?)
            } else {
                None
            };
            let report = model::scan(
                source.image(),
                &options,
                ScanLimits::default(),
                &work_cancel,
                |progress| {
                    if let Ok(mut slot) = latest.lock() {
                        *slot = progress;
                    }
                },
            );
            // scan 已决定完成、部分取消或预算终态；晚到取消不推翻该报告。
            Ok::<_, ToolError>(ScanOutput { preview, report })
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.active.as_ref().is_some_and(|active| {
                    active.request_id == request_id && Arc::ptr_eq(&active.cancel, &cancel)
                }) {
                    return;
                }
                view.flush_progress();
                view.active = None;
                if view.closing {
                    drop(result);
                    window.remove_window();
                    return;
                }
                if view.request_id != request_id {
                    drop(result);
                    return;
                }
                let report = match result {
                    Ok(output) => {
                        if let Some(preview) = output.preview {
                            view.preview = Some(preview);
                        }
                        output.report
                    }
                    Err(error) => Err(error),
                };
                match report {
                    Ok(report) => {
                        view.progress = report.progress;
                        view.selected = (!report.hits.is_empty()).then_some(0);
                        view.report = Some(report);
                        view.notice.clear();
                        view.failed = false;
                    }
                    Err(error) => {
                        view.failed = error != ToolError::Cancelled;
                        view.notice = if error == ToolError::Cancelled {
                            tr!(
                                "已取消；尚未产生识别报告，可重新识别",
                                "Cancelled before a scan report was ready; scan again"
                            )
                            .into()
                        } else {
                            error.to_string()
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let keep = weak
                .update(cx, |view, cx| {
                    if !view
                        .active
                        .as_ref()
                        .is_some_and(|active| active.request_id == request_id)
                    {
                        return false;
                    }
                    if view.flush_progress() {
                        cx.notify();
                    }
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    fn flush_progress(&mut self) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        let Ok(latest) = active.latest.lock() else {
            return false;
        };
        let changed = progress_values(self.progress) != progress_values(*latest);
        self.progress = *latest;
        changed
    }

    fn control(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        match control {
            Control::Close => {
                if self.request_close(cx) {
                    window.remove_window();
                }
                return;
            }
            Control::Cancel => {
                if let Some(active) = &self.active {
                    active.cancel.store(true, Ordering::Release);
                    self.notice = tr!(
                        "正在取消，已识别的候选会保留…",
                        "Cancelling; recognized candidates will be kept…"
                    )
                    .into();
                    self.failed = false;
                    cx.notify();
                }
                return;
            }
            _ if self.busy() => return,
            Control::Scan => {
                self.start(window, cx);
                return;
            }
            Control::Select(index) => {
                if self
                    .report
                    .as_ref()
                    .is_some_and(|report| index < report.hits.len())
                {
                    self.selected = Some(index);
                    cx.notify();
                }
                return;
            }
            Control::Page(page) => {
                self.page = page.min(page_count(self.hit_count()) - 1);
                cx.notify();
                return;
            }
            Control::CopyCurrent => {
                if let Some(hit) = self
                    .selected
                    .and_then(|index| self.report.as_ref()?.hits.get(index))
                {
                    if let Err(error) = validate_copy_text(&hit.text) {
                        self.notice = error.to_string();
                        self.failed = true;
                        cx.notify();
                        return;
                    }
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(hit.text.clone()));
                    self.notice = tr!(
                        "已复制当前项原始内容",
                        "Copied the current item's original text"
                    )
                    .into();
                    self.failed = false;
                    cx.notify();
                }
                return;
            }
            Control::CopyAll => {
                if let Some(report) = &self.report {
                    match joined_contents(&report.hits) {
                        Ok(text) if !report.hits.is_empty() => {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                            self.notice = trf!(
                                "已复制 {} 项原始内容（逐项换行）",
                                "Copied {} original values, separated by newlines",
                                report.hits.len()
                            );
                            self.failed = false;
                        }
                        Err(error) => {
                            self.notice = error.to_string();
                            self.failed = true;
                        }
                        _ => return,
                    }
                    cx.notify();
                }
                return;
            }
            Control::Format(format) => {
                let index = format.index();
                self.options.formats[index] = !self.options.formats[index];
            }
            Control::All => {
                let enable = !self.options.formats.iter().all(|enabled| *enabled);
                self.options.formats = [enable; 7];
            }
            Control::Inverted => self.options.try_inverted = !self.options.try_inverted,
        }
        self.invalidate();
        cx.notify();
    }

    fn hit_count(&self) -> usize {
        self.report.as_ref().map_or(0, |report| report.hits.len())
    }

    fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<String>,
        selected: bool,
        enabled: bool,
        control: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let label: String = label.into();
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(13.))
            .bg(rgb(if selected {
                palette.selected
            } else {
                palette.track
            }))
            .text_color(rgb(palette.foreground))
            .opacity(if enabled { 1. } else { 0.4 })
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.control(control, window, cx)),
                    )
            })
            .child(label)
            .into_any_element()
    }

    fn preview_panel(&self, height: f32, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut frame = div()
            .id("barcode-preview")
            .relative()
            .w_full()
            .h(px(height))
            .flex_shrink_0()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(palette.track));
        if let Some(preview) = &self.preview {
            let preview = preview.clone();
            let image = self.source.image();
            let dimensions = (image.width, image.height);
            let endpoints = self.selected.and_then(|index| {
                let hit = self.report.as_ref()?.hits.get(index)?;
                Some(((hit.x1, hit.y1), (hit.x2, hit.y2)))
            });
            frame = frame.child(
                canvas(
                    move |bounds, _, _| {
                        content_rect(bounds)
                            .contain(dimensions.0, dimensions.1)
                            .map(|rect| content_rect(paint_bounds(rect)))
                    },
                    move |_, content: Option<ContentRect>, window, _| {
                        let Some(content) = content else {
                            return;
                        };
                        let _ = window.paint_image_exact(
                            paint_bounds(content),
                            px(0.).into(),
                            preview.clone(),
                            0,
                            false,
                        );
                        let Some((first, second)) = endpoints else {
                            return;
                        };
                        let first =
                            content.image_point(dimensions.0, dimensions.1, first.0, first.1);
                        let second =
                            content.image_point(dimensions.0, dimensions.1, second.0, second.1);
                        if let (Some(first), Some(second)) = (first, second) {
                            line(window, first, second, palette.background, 4.);
                            line(window, first, second, palette.accent, 2.);
                            for (x, y) in [first, second] {
                                for (a, b) in
                                    [((x - 5., y), (x + 5., y)), ((x, y - 5.), (x, y + 5.))]
                                {
                                    line(window, a, b, palette.background, 4.);
                                    line(window, a, b, palette.accent, 1.5);
                                }
                            }
                        }
                    },
                )
                .size_full(),
            );
        } else {
            frame = frame.child(div().p_4().text_color(rgb(palette.muted)).child(
                if self.active.is_some() {
                    tr!(
                        "正在准备有界预览与识别报告…",
                        "Preparing a bounded preview and scan report…"
                    )
                } else {
                    tr!("预览未就绪，可重新识别", "Preview is not ready; scan again")
                },
            ));
        }
        frame.into_any_element()
    }

    fn report_status(&self) -> String {
        if let Some(report) = &self.report {
            let count = report.hits.len();
            let status = match report.status {
                ScanStatus::Complete if count == 0 => tr!(
                    "识别完成：未找到所选码制",
                    "Complete: no selected barcode format found"
                )
                .into(),
                ScanStatus::Complete => {
                    trf!("识别完成：{count} 个候选", "Complete: {count} candidates")
                }
                ScanStatus::Cancelled => trf!(
                    "已取消：保留 {count} 个部分候选",
                    "Cancelled: kept {count} partial candidates"
                ),
                ScanStatus::Limited => trf!(
                    "已达扫描预算：保留 {count} 个候选，可能尚有遗漏",
                    "Scan budget reached: kept {count} candidates; others may remain"
                ),
            };
            if report.detail.is_empty() {
                status
            } else {
                format!("{status} · {}", display_summary(&report.detail, 240))
            }
        } else if self.active.is_some() {
            tr!("正在识别，选项已锁定", "Scanning; options are locked").into()
        } else {
            tr!("尚无识别报告", "No scan report").into()
        }
    }
}

impl Drop for ImageBarcodeView {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageBarcodeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let palette = crate::theme::palette(cx);
        let enabled = !self.busy();
        let mut options = div().flex().flex_wrap().gap_2().items_center();
        for format in BarcodeFormat::ALL {
            let selected = self.options.formats[format.index()];
            options = options.child(self.button(
                ("barcode-format", format.index()),
                format!("{} {}", if selected { "☑" } else { "☐" }, format.label()),
                selected,
                enabled,
                Control::Format(format),
                cx,
            ));
        }
        let all = self.options.formats.iter().all(|enabled| *enabled);
        options = options
            .child(self.button(
                "barcode-all",
                tr!("全部码制", "All formats"),
                all,
                enabled,
                Control::All,
                cx,
            ))
            .child(self.button(
                "barcode-inverted",
                tr!("尝试反色", "Try inverted"),
                self.options.try_inverted,
                enabled,
                Control::Inverted,
                cx,
            ));
        let mut hits = div()
            .id("barcode-hits")
            .flex()
            .flex_col()
            .gap_2()
            .min_h(px(80.));
        if let Some(report) = &self.report {
            for (index, hit) in report
                .hits
                .iter()
                .enumerate()
                .skip(self.page * PAGE_SIZE)
                .take(PAGE_SIZE)
            {
                let selected = self.selected == Some(index);
                hits = hits.child(
                    div()
                        .id(("barcode-hit", index))
                        .p_2()
                        .rounded_md()
                        .overflow_hidden()
                        .bg(rgb(if selected {
                            palette.selected
                        } else {
                            palette.track
                        }))
                        .when(enabled, |row| {
                            row.cursor_pointer().on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.control(Control::Select(index), window, cx)
                                },
                            ))
                        })
                        .child(div().text_size(px(13.)).child(format!(
                            "{} · {}",
                            index + 1,
                            hit.format.label()
                        )))
                        .child(
                            div()
                                .text_size(px(13.))
                                .child(display_summary(&hit.text, SUMMARY_CHARS)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(palette.muted))
                                .child(trf!(
                                    "扫描线：({:.1}, {:.1}) → ({:.1}, {:.1}) · {} 字节",
                                    "Scan line: ({:.1}, {:.1}) → ({:.1}, {:.1}) · {} bytes",
                                    hit.x1,
                                    hit.y1,
                                    hit.x2,
                                    hit.y2,
                                    hit.text.len()
                                )),
                        ),
                );
            }
        }
        let image = self.source.image();
        let page_count = page_count(self.hit_count());
        let preview_height = (f32::from(window.viewport_size().height) * 0.34).clamp(100., 300.);
        div().size_full().p_4().flex().flex_col().gap_3().bg(rgb(palette.background))
            .text_color(rgb(palette.foreground)).font_family("Microsoft YaHei UI").text_size(px(14.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held {
                        view.control(if view.active.is_some() { Control::Cancel } else { Control::Close }, window, cx);
                    }
                }
            }))
            .child(div().text_size(px(20.)).child(tr!("一维条码识别（实验）", "Barcode recognition (preview)")))
            .child(div().id("barcode-body").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().gap_3()
                .child(options)
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(trf!(
                    "来源：{} × {} 像素 · 白底缩略仅供预览，识别使用源像素",
                    "Source: {} × {} pixels · white preview background; scanning uses source pixels",
                    image.width, image.height
                )))
                .child(self.preview_panel(preview_height, cx))
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                    "选择候选查看扫描线和端点；端点不代表完整包围框。同内容的不同位置分别列出。",
                    "Select a candidate to show its scan line and endpoints, not a bounding box. Different positions are listed separately."
                )))
                .child(div().text_color(rgb(palette.muted)).child(self.report_status()))
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(trf!(
                    "累计采样 {} 像素 · 解码尝试 {} 次 · 候选 {} 项",
                    "{} sampled pixels · {} decode attempts · {} candidates",
                    self.progress.sampled_pixels, self.progress.decode_attempts, self.progress.results
                )))
                .child(hits))
            .child(div().flex().flex_wrap().items_center().gap_2().flex_shrink_0()
                .child(self.button("barcode-prev", tr!("上一页", "Previous"), false, enabled && self.page > 0, Control::Page(self.page.saturating_sub(1)), cx))
                .child(div().text_color(rgb(palette.muted)).child(trf!("第 {}/{} 页", "Page {}/{}", self.page + 1, page_count)))
                .child(self.button("barcode-next", tr!("下一页", "Next"), false, enabled && self.page + 1 < page_count, Control::Page(self.page + 1), cx)))
            .child(div().id("barcode-notice").max_h(px(64.)).overflow_y_scroll().flex_shrink_0().text_size(px(12.))
                .text_color(rgb(if self.failed { palette.danger } else { palette.muted })).child(self.notice.clone()))
            .child(div().flex().flex_wrap().gap_2().flex_shrink_0()
                .child(self.button("barcode-scan", tr!("识别", "Scan"), true, enabled && self.options.formats.iter().any(|selected| *selected), Control::Scan, cx))
                .child(self.button("barcode-cancel", tr!("取消", "Cancel"), false, self.active.is_some() && !self.closing, Control::Cancel, cx))
                .child(self.button("barcode-copy", tr!("复制当前项", "Copy current"), false, enabled && self.selected.is_some(), Control::CopyCurrent, cx))
                .child(self.button("barcode-copy-all", tr!("复制全部内容", "Copy all contents"), false, enabled && self.hit_count() > 0, Control::CopyAll, cx))
                .child(self.button("barcode-close", tr!("关闭", "Close"), false, !self.closing, Control::Close, cx)))
            .into_any_element()
    }
}

fn page_count(count: usize) -> usize {
    count.div_ceil(PAGE_SIZE).max(1)
}

fn progress_values(progress: ScanProgress) -> (u64, usize, usize) {
    (
        progress.sampled_pixels,
        progress.decode_attempts,
        progress.results,
    )
}

fn content_rect(bounds: Bounds<gpui::Pixels>) -> ContentRect {
    ContentRect {
        x: f64::from(f32::from(bounds.origin.x)),
        y: f64::from(f32::from(bounds.origin.y)),
        width: f64::from(f32::from(bounds.size.width)),
        height: f64::from(f32::from(bounds.size.height)),
    }
}

fn paint_bounds(rect: ContentRect) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(px(rect.x as f32), px(rect.y as f32)),
        size(px(rect.width as f32), px(rect.height as f32)),
    )
}

fn line(window: &mut Window, first: (f64, f64), second: (f64, f64), color: u32, width: f32) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(point(px(first.0 as f32), px(first.1 as f32)));
    path.line_to(point(px(second.0 as f32), px(second.1 as f32)));
    if let Ok(path) = path.build() {
        window.paint_path(path, rgb(color));
    }
}

fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (960. / f64::from(width))
        .min(520. / f64::from(height))
        .min(1.);
    (
        (f64::from(width) * scale).round().max(1.) as u32,
        (f64::from(height) * scale).round().max(1.) as u32,
    )
}

fn prepare_preview(image: &Image, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
    check_cancel(cancel)?;
    let (width, height) = preview_size(image.width, image.height);
    let mut preview = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        ToolLimits::default(),
        cancel,
    )?;
    crate::export::flatten_on_white(&mut preview.bgra);
    check_cancel(cancel)?;
    Ok(crate::canvas_source::render_image(
        width,
        height,
        preview.bgra,
    ))
}

fn display_summary(text: &str, max_chars: usize) -> String {
    if text.is_empty() {
        return tr!("（空内容）", "(empty content)").into();
    }
    let mut summary = String::new();
    let mut chars = text.chars();
    for ch in chars.by_ref().take(max_chars) {
        if ch.is_control()
            || matches!(ch, '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        {
            summary.extend(ch.escape_default());
        } else {
            summary.push(ch);
        }
    }
    if chars.next().is_some() {
        summary.push('…');
    }
    summary
}

fn validate_copy_text(text: &str) -> ToolResult<()> {
    if text.contains('\0') {
        return Err(ToolError::Invalid(
            tr!(
                "内容含 NUL，系统文本剪贴板无法完整复制",
                "Contains NUL; the system text clipboard cannot copy it in full"
            )
            .into(),
        ));
    }
    Ok(())
}

fn joined_contents(hits: &[BarcodeHit]) -> ToolResult<String> {
    for hit in hits {
        validate_copy_text(&hit.text)?;
    }
    let bytes = hits
        .iter()
        .try_fold(hits.len().saturating_sub(1), |total, hit| {
            total.checked_add(hit.text.len())
        })
        .filter(|bytes| *bytes <= MAX_COPY_BYTES)
        .ok_or_else(|| {
            ToolError::Invalid(tr!("复制内容超过预算", "Copy contents exceed the limit").into())
        })?;
    let mut text = String::new();
    text.try_reserve_exact(bytes).map_err(|_| {
        ToolError::Failed(tr!("无法分配复制内容", "Unable to allocate clipboard contents").into())
    })?;
    for (index, hit) in hits.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        text.push_str(&hit.text);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_barcode_view_display_escapes_controls_and_only_truncates_the_summary() {
        let text = "原值\n\0\u{202e}abcdef";
        assert_eq!(display_summary(text, 4), "原值\\n\\u{0}…");
        assert_eq!(
            display_summary("A\r\n\tB\u{202e}", 100),
            "A\\r\\n\\tB\\u{202e}"
        );
        assert_eq!(text, "原值\n\0\u{202e}abcdef");
    }

    #[test]
    fn image_tool_barcode_view_copy_preserves_values_and_same_text_at_different_positions() {
        let hits = [
            BarcodeHit {
                format: BarcodeFormat::Code128,
                text: "a\tb\n中".into(),
                x1: 1.,
                y1: 2.,
                x2: 3.,
                y2: 2.,
            },
            BarcodeHit {
                format: BarcodeFormat::Code128,
                text: "a\tb\n中".into(),
                x1: 10.,
                y1: 2.,
                x2: 12.,
                y2: 2.,
            },
        ];
        assert!(validate_copy_text("a\tb\n中\r\u{1b}😀").is_ok());
        assert_eq!(joined_contents(&hits).unwrap(), "a\tb\n中\na\tb\n中");
        assert_eq!(joined_contents(&[]).unwrap(), "");
        for text in ["\0", "\0abc", "ab\0cd", "abc\0"] {
            assert!(validate_copy_text(text).is_err());
            let mut invalid = hits[1].clone();
            invalid.text = text.into();
            assert!(joined_contents(&[hits[0].clone(), invalid]).is_err());
        }
        assert_eq!(hits[0].text, "a\tb\n中");
    }

    #[test]
    fn image_tool_barcode_view_preview_is_bounded_without_upscaling() {
        assert_eq!(preview_size(1920, 1040), (960, 520));
        assert_eq!(preview_size(1, 65_535), (1, 520));
        assert_eq!(preview_size(65_535, 1), (960, 1));
        assert_eq!(preview_size(2, 1), (2, 1));
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(PAGE_SIZE + 1), 2);
    }
}
