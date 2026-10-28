//! 识别条码窗口：左侧码制、选项与结果，右侧预览标出所选条码的扫描线；来源快照只读。
#[path = "image_tool_barcode.rs"]
pub(crate) mod model;

use self::model::{
    BarcodeFormat, BarcodeHit, ScanLimits, ScanOptions, ScanProgress, ScanReport, ScanStatus,
};
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tool_view::geometry::ContentRect,
    image_tool_view::{
        host::{self, HostedTool, ToolEvent, ToolHost},
        single::SingleImage,
    },
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    settings_widgets::{self as widgets, ButtonKind},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    Entity, EventEmitter, FocusHandle, KeyDownEvent, PathBuilder, Render, RenderImage, Window,
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
    source: BarcodeSource,
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
    host: ToolHost,
}

/// 在主界面弹层里打开图库选中的一张图。
/// 只读的扫描来源：工具产物或图库里已解码的共享像素。
#[derive(Clone)]
enum BarcodeSource {
    Processed(Arc<ProcessedImage>),
    Shared(Arc<Image>),
}

impl BarcodeSource {
    fn image(&self) -> &Image {
        match self {
            Self::Processed(image) => image.image(),
            Self::Shared(image) => image,
        }
    }
}

/// 嵌进单图工具窗或主界面弹层；源图与另外两个单图工具共用。
pub(crate) fn embedded(
    source: SingleImage,
    host: ToolHost,
    window: &mut Window,
    cx: &mut App,
) -> Result<Entity<ImageBarcodeView>> {
    let source = match source {
        SingleImage::Shared(image) => BarcodeSource::Shared(image),
        SingleImage::Processed(image) => BarcodeSource::Processed(image),
    };
    validate_input(source.image(), ToolLimits::default())?;
    Ok(cx.new(|cx| ImageBarcodeView::new(source, host, window, cx)))
}

impl HostedTool for ImageBarcodeView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.active.is_some()
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImageBarcodeView::request_close(self, cx)
    }
}

impl EventEmitter<ToolEvent> for ImageBarcodeView {}

impl ImageBarcodeView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        source: BarcodeSource,
        host: ToolHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.defer_in(window, |view, window, cx| {
            view.start(window, cx);
        });
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
            host,
        }
    }
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
        self.notice = tr!(
            "选项已变化，点“识别”重新扫描",
            "Options changed; press Scan"
        )
        .into();
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
        self.notice.clear();
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
                    host::finish_close(view.host, window, cx);
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
                    host::finish_close(self.host, window, cx);
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

    fn preview_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let c = chrome::chrome(cx);
        let palette = c.palette;
        let Some(preview) = self.preview.clone() else {
            return chrome::empty_state(
                Icon::Ocr,
                if self.active.is_some() {
                    tr!("正在准备预览…", "Preparing preview…")
                } else {
                    tr!("预览未就绪，可重新识别", "Preview is not ready; scan again")
                },
                c,
            )
            .into_any_element();
        };
        let image = self.source.image();
        let dimensions = (image.width, image.height);
        let endpoints = self.selected.and_then(|index| {
            let hit = self.report.as_ref()?.hits.get(index)?;
            Some(((hit.x1, hit.y1), (hit.x2, hit.y2)))
        });
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
                let first = content.image_point(dimensions.0, dimensions.1, first.0, first.1);
                let second = content.image_point(dimensions.0, dimensions.1, second.0, second.1);
                if let (Some(first), Some(second)) = (first, second) {
                    line(window, first, second, palette.background, 4.);
                    line(window, first, second, palette.accent, 2.);
                    for (x, y) in [first, second] {
                        for (a, b) in [((x - 5., y), (x + 5., y)), ((x, y - 5.), (x, y + 5.))] {
                            line(window, a, b, palette.background, 4.);
                            line(window, a, b, palette.accent, 1.5);
                        }
                    }
                }
            },
        )
        .size_full()
        .into_any_element()
    }

    fn report_status(&self) -> String {
        if let Some(report) = &self.report {
            let count = report.hits.len();
            match report.status {
                ScanStatus::Complete if count == 0 => tr!(
                    "没有找到所选码制的条码",
                    "No barcodes of the selected formats found"
                )
                .into(),
                ScanStatus::Complete => trf!("找到 {count} 个条码", "Found {count} barcodes"),
                ScanStatus::Cancelled => trf!(
                    "已取消，保留 {count} 个结果",
                    "Cancelled; kept {count} results"
                ),
                ScanStatus::Limited => trf!(
                    "只扫描了一部分（{}），找到 {count} 个",
                    "Scanned partially ({}); found {count}",
                    report.detail
                ),
            }
        } else if self.active.is_some() {
            trf!(
                "正在识别… 已尝试 {} 次",
                "Scanning… {} attempts",
                self.progress.decode_attempts
            )
        } else {
            tr!("点“识别”开始扫描", "Press Scan to start").into()
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
        let c = chrome::chrome(cx);
        let enabled = !self.busy();

        // —— 码制与选项 ——
        let mut chips = div().flex().flex_wrap().gap_2();
        for format in BarcodeFormat::ALL {
            let chip = widgets::chip(
                ("barcode-format", format.index()),
                format.label(),
                self.options.formats[format.index()],
                c.palette,
                c.surface,
            );
            chips = chips.child(if enabled {
                chip.on_click(cx.listener(move |view, _, window, cx| {
                    view.control(Control::Format(format), window, cx)
                }))
            } else {
                chip.opacity(0.45)
            });
        }
        let all = self.options.formats.iter().all(|enabled| *enabled);
        let toggle_all = widgets::chip(
            "barcode-all",
            if all {
                tr!("全不选", "None")
            } else {
                tr!("全选", "All")
            },
            false,
            c.palette,
            c.surface,
        );
        chips = chips.child(if enabled {
            toggle_all
                .on_click(cx.listener(|view, _, window, cx| view.control(Control::All, window, cx)))
        } else {
            toggle_all.opacity(0.45)
        });
        let formats = chrome::field(tr!("码制", "Formats"), c).child(chips);
        let inverted = chrome::field(tr!("选项", "Options"), c).child(
            div()
                .id("barcode-inverted")
                .flex()
                .items_center()
                .gap_3()
                .min_h(px(32.))
                .when(enabled, |row| {
                    row.cursor_pointer()
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::Inverted, window, cx)
                        }))
                })
                .child(
                    div()
                        .flex_1()
                        .text_size(px(13.))
                        .child(tr!("也识别反色条码", "Also try inverted")),
                )
                .child(widgets::switch(
                    self.options.try_inverted,
                    c.palette,
                    c.surface,
                )),
        );

        // —— 结果 ——
        let count = self.hit_count();
        let mut results = chrome::field(trf!("结果 · {count}", "Results · {count}"), c);
        match &self.report {
            Some(report) if !report.hits.is_empty() => {
                let mut list = div().flex().flex_col();
                for (index, hit) in report
                    .hits
                    .iter()
                    .enumerate()
                    .skip(self.page * PAGE_SIZE)
                    .take(PAGE_SIZE)
                {
                    let row = chrome::list_row(
                        ("barcode-hit", index),
                        index + 1,
                        display_summary(&hit.text, SUMMARY_CHARS),
                        Some(hit.format.label().into()),
                        c,
                    )
                    .when(self.selected == Some(index), |row| {
                        row.bg(rgb(c.surface.nav_selected))
                    })
                    .when(enabled, |row| {
                        row.cursor_pointer()
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.control(Control::Select(index), window, cx)
                            }))
                    })
                    .child(chrome::row_icon(
                        ("barcode-copy", index),
                        Icon::Copy,
                        enabled,
                        c,
                        cx,
                        move |view, window, cx| {
                            view.control(Control::Select(index), window, cx);
                            view.control(Control::CopyCurrent, window, cx);
                        },
                    ));
                    list = list.child(row);
                }
                results = results.child(list);
                let pages = page_count(count);
                if pages > 1 {
                    results = results.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(12.))
                            .text_color(rgb(c.palette.muted))
                            .child(chrome::row_icon(
                                "barcode-prev",
                                Icon::ChevronLeft,
                                enabled && self.page > 0,
                                c,
                                cx,
                                |view, window, cx| {
                                    view.control(
                                        Control::Page(view.page.saturating_sub(1)),
                                        window,
                                        cx,
                                    )
                                },
                            ))
                            .child(format!("{} / {pages}", self.page + 1))
                            .child(chrome::row_icon(
                                "barcode-next",
                                Icon::ChevronRight,
                                enabled && self.page + 1 < pages,
                                c,
                                cx,
                                |view, window, cx| {
                                    view.control(Control::Page(view.page + 1), window, cx)
                                },
                            )),
                    );
                }
            }
            _ => {
                results = results.child(chrome::hint(self.report_status(), c));
            }
        }

        let sidebar = chrome::sidebar("barcode-sidebar", c)
            .child(formats)
            .child(inverted)
            .child(results);

        // —— 预览 ——
        let image = self.source.image();
        let bar = chrome::stage_bar(c).child(div().flex_1().min_w(px(0.)).truncate().child(
            if self.selected.is_some() {
                trf!(
                    "{} × {} 像素 · 线条标出所选条码的扫描位置",
                    "{} × {} px · the line marks the selected barcode",
                    image.width,
                    image.height
                )
            } else {
                trf!("{} × {} 像素", "{} × {} px", image.width, image.height)
            },
        ));
        let main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .px_5()
            .pb_5()
            .pt_2()
            .child(bar)
            .child(
                chrome::stage(c)
                    .flex_1()
                    .min_h(px(0.))
                    .p_4()
                    .child(self.preview_panel(cx)),
            );

        // —— 底栏 ——
        let (status, tone) = if !self.notice.is_empty() && self.active.is_none() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else {
            (
                self.report_status(),
                if self.active.is_some() {
                    Tone::Busy
                } else if count > 0 {
                    Tone::Done
                } else {
                    Tone::Idle
                },
            )
        };
        let mut actions = Vec::new();
        if self.active.is_some() && !self.closing {
            actions.push(
                chrome::action(
                    "barcode-cancel",
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
        if count > 0 {
            let copy_all = chrome::action(
                "barcode-copy-all",
                tr!("复制全部", "Copy all"),
                ButtonKind::Secondary,
                enabled,
                c,
            );
            actions.push(
                if enabled {
                    copy_all.on_click(
                        cx.listener(|view, _, window, cx| {
                            view.control(Control::CopyAll, window, cx)
                        }),
                    )
                } else {
                    copy_all
                }
                .into_any_element(),
            );
        }
        let can_scan = enabled && self.options.formats.iter().any(|selected| *selected);
        let scan = chrome::action(
            "barcode-scan",
            if self.report.is_some() {
                tr!("重新识别", "Scan again")
            } else {
                tr!("识别", "Scan")
            },
            ButtonKind::Primary,
            can_scan,
            c,
        );
        actions.push(
            if can_scan {
                scan.on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Scan, window, cx)),
                )
            } else {
                scan
            }
            .into_any_element(),
        );

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held {
                        view.control(
                            if view.active.is_some() {
                                Control::Cancel
                            } else {
                                Control::Close
                            },
                            window,
                            cx,
                        );
                    }
                } else if event.keystroke.key == "c" && event.keystroke.modifiers.control {
                    cx.stop_propagation();
                    view.control(Control::CopyCurrent, window, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(sidebar)
                    .child(main),
            )
            .child(chrome::footer(status, tone, actions, c))
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
