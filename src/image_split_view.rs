//! 内部图片分割工作流；切片、命名和保存均由已有分割计划负责。
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_split::{
        parse_weights, AxisSplit, SplitItemResult, SplitOptions, SplitOutcome, SplitOutput,
        SplitPiece, SplitPlan, SplitReport,
    },
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    save_options::{SaveEncoding, SaveFormat},
    text_field::{TextField, TextFieldEvent},
    Result,
};
use gpui::{
    div, img, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context, Entity,
    FocusHandle, KeyDownEvent, ObjectFit, Render, RenderImage, Subscription, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const PAGE_SIZE: usize = 24;

#[derive(Clone)]
enum SplitSource {
    Image(Arc<Image>),
    Processed(Arc<ProcessedImage>),
}

impl std::ops::Deref for SplitSource {
    type Target = Image;
    fn deref(&self) -> &Image {
        match self {
            Self::Image(image) => image,
            Self::Processed(image) => image.image(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Rows,
    Columns,
    Grid,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Measure {
    Parts,
    Pixels,
    Ratio,
    CustomRatio,
}
#[derive(Clone, Copy)]
enum Control {
    Layout(Layout),
    Measure(Measure),
    Ratio(u8),
    ApplyRatios,
    Columns(i32),
    Rows(i32),
    Select(usize),
    Page(i32),
    Format(SaveFormat),
    Directory,
    Save,
    Cancel,
    Refresh,
    ResultPage(i32),
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Preview,
    Save,
}
struct Job {
    kind: JobKind,
    cancel: Arc<AtomicBool>,
}

struct Preview {
    plan: Arc<SplitPlan>,
    selected: SplitPiece,
    tile: Arc<RenderImage>,
    source: Option<Arc<RenderImage>>,
}

#[derive(Clone, Default)]
struct Progress {
    total: usize,
    done: usize,
    saved: usize,
    failed: usize,
    cancelled: usize,
    last: Option<SplitItemResult>,
}
impl Progress {
    fn record(&mut self, item: &SplitItemResult) {
        self.done += 1;
        match &item.outcome {
            SplitOutcome::Saved(_) => self.saved += 1,
            SplitOutcome::Failed(_) => self.failed += 1,
            SplitOutcome::Cancelled => self.cancelled += 1,
        }
        self.last = Some(item.clone());
    }
}

pub(crate) struct ImageSplitView {
    source: SplitSource,
    protected: Arc<Vec<PathBuf>>,
    layout: Layout,
    measure: Measure,
    ratio: u8,
    custom_columns: Vec<u32>,
    custom_rows: Vec<u32>,
    column_ratio: Entity<TextField>,
    row_ratio: Entity<TextField>,
    columns: u32,
    rows: u32,
    selected: usize,
    page: usize,
    result_page: usize,
    encoding: SaveEncoding,
    directory: Option<PathBuf>,
    directory_prompt: bool,
    name: Entity<TextField>,
    request_id: u64,
    pending_preview: bool,
    job: Option<Job>,
    plan: Option<Arc<SplitPlan>>,
    piece: Option<SplitPiece>,
    source_preview: Option<Arc<RenderImage>>,
    tile_preview: Option<Arc<RenderImage>>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    progress: Option<Arc<Mutex<Progress>>>,
    report: Option<SplitReport>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

pub(crate) fn open(
    image: Image,
    protected_paths: Vec<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<ImageSplitView>> {
    open_shared(Arc::new(image), Arc::new(protected_paths), cx)
}

pub(crate) fn open_shared(
    source: Arc<Image>,
    protected: Arc<Vec<PathBuf>>,
    cx: &mut App,
) -> Result<WindowHandle<ImageSplitView>> {
    open_source(SplitSource::Image(source), protected, cx)
}

/// 单图工具可直接移交当前产物的共享所有权，保持未保存调整且不复制整帧。
pub(crate) fn open_processed(
    source: Arc<ProcessedImage>,
    protected: Arc<Vec<PathBuf>>,
    cx: &mut App,
) -> Result<WindowHandle<ImageSplitView>> {
    open_source(SplitSource::Processed(source), protected, cx)
}

fn open_source(
    source: SplitSource,
    protected: Arc<Vec<PathBuf>>,
    cx: &mut App,
) -> Result<WindowHandle<ImageSplitView>> {
    validate_input(&source, ToolLimits::default())?;
    if protected.len() > ToolLimits::default().max_items {
        return Err(tr!(
            "原片保护路径数量超过上限",
            "Too many protected source paths"
        )
        .into());
    }
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(1000.), px(820.)), |display| {
            let size = display.bounds().size;
            gpui::size(
                px(1000_f32.min((f32::from(size.width) - 48.).max(420.))),
                px(820_f32.min((f32::from(size.height) - 72.).max(400.))),
            )
        });
    let initial_name = protected
        .first()
        .and_then(|path| path.file_stem())
        .and_then(|name| name.to_str())
        .map_or_else(|| "Image_split".to_owned(), |name| format!("{name}_split"));
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                let name = cx.new(|cx| {
                    TextField::new(initial_name, tr!("文件名前缀", "File name prefix"), cx)
                });
                let subscription = cx.subscribe_in(
                    &name,
                    window,
                    |view: &mut ImageSplitView, _, event, window, cx| match event {
                        TextFieldEvent::Submit => view.save(window, cx),
                        TextFieldEvent::Changed => {
                            if !view.closing && !view.saving() {
                                view.notice.clear();
                                view.failed = false;
                                cx.notify();
                            }
                        }
                    },
                );
                let custom_columns = if source.width >= 3 {
                    vec![1, 2]
                } else {
                    vec![1]
                };
                let custom_rows = if source.height >= 3 {
                    vec![1, 2]
                } else {
                    vec![1]
                };
                let column_ratio =
                    cx.new(|cx| TextField::new(weight_text(&custom_columns), "1:2:1", cx));
                let row_ratio = cx.new(|cx| TextField::new(weight_text(&custom_rows), "1:2", cx));
                let fields = [&name, &column_ratio, &row_ratio];
                let focus_group: Vec<_> = fields.iter().map(|field| field.downgrade()).collect();
                for field in fields {
                    field.update(cx, |field, cx| {
                        field.configure_focus(focus_group.clone(), window, cx)
                    });
                }
                let mut subscriptions = vec![subscription];
                for field in [&column_ratio, &row_ratio] {
                    subscriptions.push(cx.subscribe_in(
                        field,
                        window,
                        |view: &mut ImageSplitView, _, event, window, cx| match event {
                            TextFieldEvent::Submit => view.apply_ratios(window, cx),
                            TextFieldEvent::Changed if !view.closing && !view.saving() => {
                                cx.notify();
                            }
                            _ => {}
                        },
                    ));
                }
                let columns = source.width.min(2);
                let rows = source.height.min(2);
                ImageSplitView {
                    source,
                    protected,
                    layout: Layout::Grid,
                    measure: Measure::Parts,
                    ratio: 0,
                    custom_columns,
                    custom_rows,
                    column_ratio,
                    row_ratio,
                    columns,
                    rows,
                    selected: 0,
                    page: 0,
                    result_page: 0,
                    encoding: SaveEncoding::default(),
                    directory: None,
                    directory_prompt: false,
                    name,
                    request_id: 1,
                    pending_preview: true,
                    job: None,
                    plan: None,
                    piece: None,
                    source_preview: None,
                    tile_preview: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
                    progress: None,
                    report: None,
                    notice: String::new(),
                    failed: false,
                    closing: false,
                    focus,
                    _subscriptions: subscriptions,
                }
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view.update(cx, |view, cx| {
                view.enable_fields(cx);
                view.start_preview(window, cx);
            });
            view
        },
    )?)
}

impl ImageSplitView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        self.enable_fields(cx);
        self.pending_preview = false;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
            self.notice = tr!(
                "正在取消并关闭，已保存文件保留…",
                "Cancelling and closing; saved files are kept…"
            )
            .into();
            self.failed = false;
            cx.notify();
            false
        } else {
            true
        }
    }

    fn saving(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.kind == JobKind::Save)
    }
    fn ready(&self) -> bool {
        !self.closing
            && self.job.is_none()
            && !self.pending_preview
            && !self.directory_prompt
            && self
                .plan
                .as_ref()
                .is_some_and(|plan| plan.request_id() == self.request_id)
    }
    fn options(&self) -> SplitOptions {
        if self.measure == Measure::CustomRatio {
            return custom_options(self.layout, &self.custom_columns, &self.custom_rows);
        }
        let axis = |value| match self.measure {
            Measure::Parts => AxisSplit::EqualParts(value),
            Measure::Pixels => AxisSplit::Pixels(value),
            Measure::Ratio => AxisSplit::Weights(match self.ratio {
                1 => vec![2, 1],
                2 => vec![1, 2, 1],
                _ => vec![1, 2],
            }),
            Measure::CustomRatio => unreachable!("custom ratios handled above"),
        };
        match self.layout {
            Layout::Rows => SplitOptions::horizontal(axis(self.rows)),
            Layout::Columns => SplitOptions::vertical(axis(self.columns)),
            Layout::Grid => SplitOptions {
                columns: axis(self.columns),
                rows: axis(self.rows),
            },
        }
    }

    fn enable_fields(&self, cx: &mut Context<Self>) {
        let enabled = !self.closing && !self.saving();
        self.name
            .update(cx, |field, cx| field.set_enabled(enabled, cx));
        self.column_ratio.update(cx, |field, cx| {
            field.set_enabled(
                enabled && self.measure == Measure::CustomRatio && self.layout != Layout::Rows,
                cx,
            )
        });
        self.row_ratio.update(cx, |field, cx| {
            field.set_enabled(
                enabled && self.measure == Measure::CustomRatio && self.layout != Layout::Columns,
                cx,
            )
        });
    }

    fn ratios_composing(&self, cx: &App) -> bool {
        self.measure == Measure::CustomRatio
            && ((self.layout != Layout::Rows && self.column_ratio.read(cx).is_composing())
                || (self.layout != Layout::Columns && self.row_ratio.read(cx).is_composing()))
    }

    fn ratios_dirty(&self, cx: &App) -> bool {
        self.measure == Measure::CustomRatio
            && ((self.layout != Layout::Rows
                && self.column_ratio.read(cx).text() != weight_text(&self.custom_columns))
                || (self.layout != Layout::Columns
                    && self.row_ratio.read(cx).text() != weight_text(&self.custom_rows)))
    }

    fn apply_ratios(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.saving() || self.ratios_composing(cx) {
            return;
        }
        let parsed = (|| {
            let columns = if self.layout == Layout::Rows {
                self.custom_columns.clone()
            } else {
                parse_weights(
                    self.column_ratio.read(cx).text(),
                    ToolLimits::default().max_items,
                )?
            };
            let rows = if self.layout == Layout::Columns {
                self.custom_rows.clone()
            } else {
                parse_weights(
                    self.row_ratio.read(cx).text(),
                    ToolLimits::default().max_items,
                )?
            };
            let options = custom_options(self.layout, &columns, &rows);
            // 只应用可以形成完整切片的参数；无效草稿不清掉已经应用的预览。
            SplitPlan::new(
                &self.source,
                &options,
                ToolLimits::default(),
                self.request_id,
            )?;
            Ok::<_, ToolError>((columns, rows))
        })();
        let (columns, rows) = match parsed {
            Ok(weights) => weights,
            Err(error) => {
                self.notice = error.to_string();
                self.failed = true;
                cx.notify();
                return;
            }
        };
        let same = self.measure == Measure::CustomRatio
            && self.custom_columns == columns
            && self.custom_rows == rows;
        self.custom_columns = columns;
        self.custom_rows = rows;
        self.measure = Measure::CustomRatio;
        if self.layout != Layout::Rows {
            self.column_ratio.update(cx, |field, cx| {
                field.set_text(weight_text(&self.custom_columns), cx)
            });
        }
        if self.layout != Layout::Columns {
            self.row_ratio.update(cx, |field, cx| {
                field.set_text(weight_text(&self.custom_rows), cx)
            });
        }
        window.focus(&self.focus);
        self.enable_fields(cx);
        self.notice.clear();
        self.failed = false;
        if same && self.plan.is_some() {
            cx.notify();
        } else {
            self.selected = 0;
            self.changed(window, cx);
        }
    }
    fn clear_preview(&mut self) {
        self.plan = None;
        self.piece = None;
        if let Some(image) = self.tile_preview.take() {
            self.retiring.push(image);
        }
    }
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.saving() {
            return;
        }
        self.request_id = self.request_id.wrapping_add(1);
        self.pending_preview = true;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
        self.clear_preview();
        self.notice.clear();
        self.failed = false;
        self.start_preview(window, cx);
        cx.notify();
    }
    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.job.is_some() || !self.pending_preview {
            return;
        }
        self.pending_preview = false;
        let request_id = self.request_id;
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job {
            kind: JobKind::Preview,
            cancel: cancel.clone(),
        });
        let source = self.source.clone();
        let options = self.options();
        let index = self.selected;
        let encoding = self.encoding;
        let prepare_source = self.source_preview.is_none();
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            check_cancel(&work_cancel)?;
            let plan = SplitPlan::new(&source, &options, ToolLimits::default(), request_id)?;
            let selected = plan
                .piece(index.min(plan.len() - 1))
                .expect("nonempty split plan");
            let tile = plan.render(&source, selected.index, &work_cancel)?;
            let tile = small_preview(tile.image(), encoding, &work_cancel)?;
            let source_preview = if prepare_source {
                Some(small_preview(
                    &source,
                    SaveEncoding::default(),
                    &work_cancel,
                )?)
            } else {
                None
            };
            check_cancel(&work_cancel)?;
            Ok::<_, ToolError>(Preview {
                plan: Arc::new(plan),
                selected,
                tile,
                source: source_preview,
            })
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view
                    .job
                    .as_ref()
                    .is_some_and(|job| Arc::ptr_eq(&job.cancel, &cancel))
                {
                    return;
                }
                view.job = None;
                if view.closing {
                    drop(result);
                    window.remove_window();
                    return;
                }
                if view.request_id != request_id
                    || cancel.load(Ordering::Acquire)
                    || view.pending_preview
                {
                    drop(result);
                    view.start_preview(window, cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(preview) => {
                        view.clear_preview();
                        view.selected = preview.selected.index;
                        view.page = view.selected / PAGE_SIZE;
                        view.piece = Some(preview.selected);
                        view.plan = Some(preview.plan);
                        view.tile_preview = Some(preview.tile);
                        if let Some(image) = preview.source {
                            view.source_preview = Some(image);
                        }
                        view.notice.clear();
                        view.failed = false;
                    }
                    Err(error) => {
                        view.notice = error.to_string();
                        view.failed = error != ToolError::Cancelled;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn choose_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.saving() || self.directory_prompt {
            return;
        }
        self.directory_prompt = true;
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr!("选择分割结果目录", "Choose a split output folder").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = receiver.await;
            let _ = weak.update(cx, |view, cx| {
                view.directory_prompt = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.directory = Some(path);
                            view.notice.clear();
                            view.failed = false;
                        }
                    }
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() || self.name.read(cx).is_composing() || self.ratios_composing(cx) {
            return;
        }
        let Some(directory) = self.directory.clone() else {
            self.notice = tr!("请先选择输出目录", "Choose an output folder first").into();
            cx.notify();
            return;
        };
        let output = match SplitOutput::new(directory, self.name.read(cx).text(), self.encoding) {
            Ok(output) => output,
            Err(error) => {
                self.notice = error.to_string();
                self.failed = true;
                cx.notify();
                return;
            }
        };
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let request_id = self.request_id;
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job {
            kind: JobKind::Save,
            cancel: cancel.clone(),
        });
        window.focus(&self.focus);
        self.enable_fields(cx);
        self.report = None;
        self.result_page = 0;
        self.notice.clear();
        self.failed = false;
        let progress = Arc::new(Mutex::new(Progress {
            total: plan.len(),
            ..Default::default()
        }));
        self.progress = Some(progress.clone());
        let source = self.source.clone();
        let protected = self.protected.clone();
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            for path in protected.iter() {
                check_cancel(&work_cancel)?;
                crate::image_collection::ensure_local_file_or_missing(path)
                    .map_err(ToolError::Invalid)?;
            }
            plan.save_all(
                &source,
                &output,
                request_id,
                &protected,
                &work_cancel,
                |item| {
                    if let Ok(mut progress) = progress.lock() {
                        progress.record(item);
                    }
                },
            )
        });
        let poll_cancel = cancel.clone();
        cx.spawn_in(window, async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let keep = weak
                .update(cx, |view, cx| {
                    let active = view
                        .job
                        .as_ref()
                        .is_some_and(|job| Arc::ptr_eq(&job.cancel, &poll_cancel));
                    if active {
                        cx.notify();
                    }
                    active
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view
                    .job
                    .as_ref()
                    .is_some_and(|job| Arc::ptr_eq(&job.cancel, &cancel))
                {
                    return;
                }
                view.job = None;
                view.enable_fields(cx);
                match result {
                    Ok(report) => {
                        view.report = Some(report);
                        view.failed = false;
                        view.notice = tr!(
                            "分割任务已结束，成功结果已保留",
                            "Split task finished; saved files are kept"
                        )
                        .into();
                    }
                    Err(error) => {
                        view.failed = error != ToolError::Cancelled;
                        view.notice = error.to_string();
                    }
                }
                if view.closing {
                    window.remove_window();
                    return;
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
                return;
            }
            Control::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Release);
                }
                if !self.saving() {
                    self.request_id = self.request_id.wrapping_add(1);
                    self.pending_preview = false;
                    self.clear_preview();
                }
                self.notice = tr!(
                    "已请求取消，已保存文件会保留",
                    "Cancellation requested; saved files are kept"
                )
                .into();
                self.failed = false;
                cx.notify();
                return;
            }
            Control::ResultPage(delta) => {
                let pages = self
                    .report
                    .as_ref()
                    .map_or(1, |report| report.items.len().div_ceil(PAGE_SIZE));
                self.result_page = shifted_page(self.result_page, delta, pages);
                cx.notify();
                return;
            }
            Control::Directory => {
                self.choose_directory(window, cx);
                return;
            }
            Control::Save => {
                self.save(window, cx);
                return;
            }
            Control::ApplyRatios => {
                self.apply_ratios(window, cx);
                return;
            }
            _ if self.closing || self.saving() => return,
            Control::Layout(layout) => {
                window.focus(&self.focus);
                self.layout = layout;
                self.selected = 0;
            }
            Control::Measure(measure) => {
                window.focus(&self.focus);
                if self.measure != measure {
                    self.measure = measure;
                    self.columns = if measure == Measure::Parts {
                        self.source.width.min(2)
                    } else {
                        512
                    };
                    self.rows = if measure == Measure::Parts {
                        self.source.height.min(2)
                    } else {
                        512
                    };
                    self.selected = 0;
                }
            }
            Control::Ratio(ratio) => {
                window.focus(&self.focus);
                self.ratio = ratio;
                self.measure = Measure::Ratio;
                self.selected = 0;
            }
            Control::Columns(delta) => {
                let maximum = if self.measure == Measure::Parts {
                    self.source.width
                } else {
                    65_535
                };
                self.columns =
                    (i64::from(self.columns) + i64::from(delta)).clamp(1, i64::from(maximum)) as u32
            }
            Control::Rows(delta) => {
                let maximum = if self.measure == Measure::Parts {
                    self.source.height
                } else {
                    65_535
                };
                self.rows =
                    (i64::from(self.rows) + i64::from(delta)).clamp(1, i64::from(maximum)) as u32
            }
            Control::Format(format) => self.encoding.format = format,
            Control::Select(index) => self.selected = index,
            Control::Page(delta) => {
                let pages = self
                    .plan
                    .as_ref()
                    .map_or(1, |plan| plan.len().div_ceil(PAGE_SIZE));
                self.page = shifted_page(self.page, delta, pages);
                cx.notify();
                return;
            }
            Control::Refresh => {}
        }
        self.enable_fields(cx);
        self.changed(window, cx);
    }
    fn button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<String>,
        active: bool,
        enabled: bool,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let label: String = label.into();
        div()
            .id(id)
            .px_3()
            .py_2()
            .rounded_md()
            .text_size(px(13.))
            .bg(rgb(if active {
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
                        cx.listener(move |view, _, window, cx| view.control(action, window, cx)),
                    )
            })
            .child(label)
            .into_any_element()
    }
}

impl Drop for ImageSplitView {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageSplitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        window.set_window_title(brand_title!("图片分割（内部）", "Split image (internal)"));
        let palette = crate::theme::palette(cx);
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let enabled = !self.closing && !self.saving();
        let busy = self.job.is_some() || self.pending_preview;
        let status = if self.saving() {
            tr!("正在逐项保存…", "Saving pieces…")
        } else if busy {
            tr!("正在更新预览…", "Updating preview…")
        } else {
            tr!("请选择切片查看并保存", "Select a piece to inspect and save")
        };
        let mut layout = div().flex().flex_wrap().gap_2();
        for (id, value, label) in [
            ("split-rows", Layout::Rows, tr!("横向条带", "Rows")),
            ("split-columns", Layout::Columns, tr!("竖向条带", "Columns")),
            ("split-grid", Layout::Grid, tr!("网格", "Grid")),
        ] {
            layout = layout.child(self.button(
                id,
                label,
                self.layout == value,
                enabled,
                Control::Layout(value),
                cx,
            ));
        }
        let mut controls = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(self.button(
                "split-parts",
                tr!("均分", "Equal parts"),
                self.measure == Measure::Parts,
                enabled,
                Control::Measure(Measure::Parts),
                cx,
            ))
            .child(self.button(
                "split-pixels",
                tr!("固定像素", "Pixels"),
                self.measure == Measure::Pixels,
                enabled,
                Control::Measure(Measure::Pixels),
                cx,
            ))
            .child(self.button(
                "split-ratio",
                tr!("比例", "Ratio"),
                self.measure == Measure::Ratio,
                enabled,
                Control::Measure(Measure::Ratio),
                cx,
            ))
            .child(self.button(
                "split-custom-ratio",
                tr!("自定比例", "Custom ratios"),
                self.measure == Measure::CustomRatio,
                enabled,
                Control::Measure(Measure::CustomRatio),
                cx,
            ));
        if self.measure == Measure::Ratio {
            for (id, label, ratio) in [
                ("split-ratio-12", "1:2", 0),
                ("split-ratio-21", "2:1", 1),
                ("split-ratio-121", "1:2:1", 2),
            ] {
                controls = controls.child(self.button(
                    id,
                    label,
                    self.ratio == ratio,
                    enabled,
                    Control::Ratio(ratio),
                    cx,
                ));
            }
            if self.layout == Layout::Grid {
                controls = controls.child(div().text_color(rgb(palette.muted)).child(tr!(
                    "行、列均使用所选比例",
                    "The ratio applies to rows and columns"
                )));
            }
        }
        let step = if self.measure == Measure::Parts {
            1
        } else {
            128
        };
        if matches!(self.measure, Measure::Parts | Measure::Pixels) && self.layout != Layout::Rows {
            controls = controls
                .child(div().child(if self.measure == Measure::Parts {
                    trf!("列数：{}", "Columns: {}", self.columns)
                } else {
                    trf!("块宽：{}像素", "Piece width: {} px", self.columns)
                }))
                .child(self.button(
                    "split-cols-minus",
                    "−",
                    false,
                    enabled && self.columns > 1,
                    Control::Columns(-step),
                    cx,
                ))
                .child(self.button(
                    "split-cols-plus",
                    "+",
                    false,
                    enabled,
                    Control::Columns(step),
                    cx,
                ));
        }
        if matches!(self.measure, Measure::Parts | Measure::Pixels)
            && self.layout != Layout::Columns
        {
            controls = controls
                .child(div().child(if self.measure == Measure::Parts {
                    trf!("行数：{}", "Rows: {}", self.rows)
                } else {
                    trf!("块高：{}像素", "Piece height: {} px", self.rows)
                }))
                .child(self.button(
                    "split-rows-minus",
                    "−",
                    false,
                    enabled && self.rows > 1,
                    Control::Rows(-step),
                    cx,
                ))
                .child(self.button(
                    "split-rows-plus",
                    "+",
                    false,
                    enabled,
                    Control::Rows(step),
                    cx,
                ));
        }
        let applied_ratios = match self.layout {
            Layout::Rows => trf!(
                "已应用行比例：{}",
                "Applied rows: {}",
                weight_text(&self.custom_rows)
            ),
            Layout::Columns => trf!(
                "已应用列比例：{}",
                "Applied columns: {}",
                weight_text(&self.custom_columns)
            ),
            Layout::Grid => trf!(
                "已应用列 {}，行 {}",
                "Applied columns {}, rows {}",
                weight_text(&self.custom_columns),
                weight_text(&self.custom_rows)
            ),
        };
        let custom_inputs = div().flex().flex_col().gap_2()
            .when(self.layout != Layout::Rows, |inputs| inputs
                .child(div().child(tr!("列比例（宽）", "Column ratios (width)")))
                .child(self.column_ratio.clone()))
            .when(self.layout != Layout::Columns, |inputs| inputs
                .child(div().child(tr!("行比例（高）", "Row ratios (height)")))
                .child(self.row_ratio.clone()))
            .child(self.button("split-apply-ratios", tr!("应用比例", "Apply ratios"), true, enabled, Control::ApplyRatios, cx))
            .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(applied_ratios))
            .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(if self.ratios_dirty(cx) {
                tr!("输入尚未应用；预览与保存仍使用已应用比例。", "Inputs are not applied; preview and save use the applied ratios.")
            } else {
                tr!("用冒号或逗号分隔正整数；只有应用后才更新预览。行列可设不同的比例。", "Separate positive integers with colons or commas, then Apply. Rows and columns can differ.")
            }));
        let mut pieces = div().flex().flex_wrap().gap_2();
        if let Some(plan) = &self.plan {
            let start = self.page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(plan.len());
            for index in start..end {
                pieces = pieces.child(self.button(
                    ("split-piece", index),
                    (index + 1).to_string(),
                    index == self.selected,
                    enabled,
                    Control::Select(index),
                    cx,
                ));
            }
            pieces = pieces
                .child(self.button(
                    "split-prev-page",
                    tr!("上一页", "Previous"),
                    false,
                    enabled && self.page > 0,
                    Control::Page(-1),
                    cx,
                ))
                .child(self.button(
                    "split-next-page",
                    tr!("下一页", "Next"),
                    false,
                    enabled && end < plan.len(),
                    Control::Page(1),
                    cx,
                ));
        }
        let mut formats = div().flex().flex_wrap().gap_2();
        for (id, format) in [
            ("split-png", SaveFormat::Png),
            ("split-jpeg", SaveFormat::Jpeg),
            ("split-bmp", SaveFormat::Bmp),
            ("split-webp", SaveFormat::Webp),
        ] {
            formats = formats.child(self.button(
                id,
                if format == SaveFormat::Webp {
                    tr!("WebP 无损", "WebP lossless")
                } else {
                    format.label()
                },
                self.encoding.format == format,
                enabled,
                Control::Format(format),
                cx,
            ));
        }
        let height = (f32::from(window.viewport_size().height) * 0.27).clamp(100., 250.);
        let image_panel = |label: String, image: Option<Arc<RenderImage>>| {
            let frame = div()
                .h(px(height))
                .w_full()
                .bg(rgb(palette.track))
                .rounded_md()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center();
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .child(div().child(label))
                .child(match image {
                    Some(image) => {
                        frame.child(img(image).size_full().object_fit(ObjectFit::Contain))
                    }
                    None => frame.child(div().text_color(rgb(palette.muted)).child(status)),
                })
        };
        let selected_label = self.piece.map_or_else(
            || tr!("切片预览", "Piece preview").into(),
            |piece| {
                trf!(
                    "第{}块 · 行{} 列{} · {}×{}",
                    "Piece {} · row {} col {} · {}×{}",
                    piece.index + 1,
                    piece.row + 1,
                    piece.column + 1,
                    piece.width,
                    piece.height
                )
            },
        );
        let preview = div()
            .flex()
            .gap_3()
            .child(image_panel(
                trf!(
                    "源图 {}×{}",
                    "Source {}×{}",
                    self.source.width,
                    self.source.height
                ),
                self.source_preview.clone(),
            ))
            .child(image_panel(selected_label, self.tile_preview.clone()));
        let directory = self.directory.as_ref().map_or_else(
            || tr!("尚未选择输出目录", "No output folder selected").into(),
            |path| path.to_string_lossy().into_owned(),
        );
        let planned = self
            .plan
            .as_ref()
            .zip(self.piece)
            .and_then(|(plan, piece)| {
                self.directory
                    .as_ref()
                    .map(|directory| (plan, piece, directory))
            })
            .map(|(_, piece, directory)| {
                SplitOutput::new(directory.clone(), self.name.read(cx).text(), self.encoding)
                    .and_then(|output| output.path_for(piece))
            })
            .transpose();
        let planned_name = match planned {
            Ok(Some(path)) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Err(error) => error.to_string(),
            _ => String::new(),
        };
        let progress = self
            .progress
            .as_ref()
            .and_then(|progress| progress.lock().ok().map(|progress| progress.clone()))
            .unwrap_or_default();
        let progress_label = trf!(
            "已处理{}/{} · 成功{} · 失败{} · 取消{}",
            "Done {}/{} · saved {} · failed {} · cancelled {}",
            progress.done,
            progress.total,
            progress.saved,
            progress.failed,
            progress.cancelled
        );
        let mut results = div().flex().flex_col().gap_2();
        if let Some(report) = &self.report {
            let start = self.result_page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(report.items.len());
            for item in &report.items[start..end] {
                let text = match &item.outcome {
                    SplitOutcome::Saved(_) => trf!("已保存：{}", "Saved: {}", item.path.display()),
                    SplitOutcome::Failed(error) => trf!(
                        "第{}块失败：{error}",
                        "Piece {} failed: {error}",
                        item.piece.index + 1
                    ),
                    SplitOutcome::Cancelled => {
                        trf!("第{}块已取消", "Piece {} cancelled", item.piece.index + 1)
                    }
                };
                results = results.child(div().text_size(px(12.)).child(text));
            }
            results = results.child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.button(
                        "split-results-prev",
                        tr!("上一页结果", "Previous results"),
                        false,
                        self.result_page > 0,
                        Control::ResultPage(-1),
                        cx,
                    ))
                    .child(self.button(
                        "split-results-next",
                        tr!("下一页结果", "Next results"),
                        false,
                        end < report.items.len(),
                        Control::ResultPage(1),
                        cx,
                    )),
            );
        } else if let Some(item) = &progress.last {
            results = results.child(
                div()
                    .text_size(px(12.))
                    .child(item.path.to_string_lossy().into_owned()),
            );
        }
        let body = div()
            .id("split-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout)
            .child(controls)
            .when(self.measure == Measure::CustomRatio, |body| {
                body.child(custom_inputs)
            })
            .child(preview)
            .child(pieces)
            .child(formats)
            .child(div().child(tr!("文件名前缀", "File name prefix")))
            .child(self.name.clone())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(planned_name),
            )
            .child(div().text_size(px(12.)).child(directory))
            .child(div().child(progress_label))
            .child(results);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(14.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.name.read(cx).is_composing() || view.ratios_composing(cx) {
                    return;
                }
                if event.keystroke.key == "s" && event.keystroke.modifiers.control {
                    if !event.is_held {
                        view.save(window, cx);
                    }
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    if !event.is_held {
                        let action = if view.job.is_some() {
                            Control::Cancel
                        } else {
                            Control::Close
                        };
                        view.control(action, window, cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .text_size(px(20.))
                    .child(tr!("图片分割（内部）", "Split image (internal)")),
            )
            .child(body)
            .child(
                div()
                    .max_h(px(60.))
                    .text_size(px(12.))
                    .text_color(rgb(if self.failed {
                        palette.danger
                    } else {
                        palette.muted
                    }))
                    .child(if self.notice.is_empty() {
                        status.to_owned()
                    } else {
                        self.notice.clone()
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .flex_shrink_0()
                    .gap_2()
                    .child(self.button(
                        "split-folder",
                        tr!("输出目录…", "Output folder…"),
                        false,
                        enabled && !self.directory_prompt,
                        Control::Directory,
                        cx,
                    ))
                    .child(self.button(
                        "split-refresh",
                        tr!("更新预览", "Preview"),
                        false,
                        enabled,
                        Control::Refresh,
                        cx,
                    ))
                    .child(self.button(
                        "split-save",
                        tr!("保存全部切片", "Save all pieces"),
                        true,
                        self.ready() && self.directory.is_some(),
                        Control::Save,
                        cx,
                    ))
                    .child(self.button(
                        "split-cancel",
                        tr!("取消处理", "Cancel task"),
                        false,
                        busy,
                        Control::Cancel,
                        cx,
                    ))
                    .child(self.button(
                        "split-close",
                        tr!("关闭", "Close"),
                        false,
                        true,
                        Control::Close,
                        cx,
                    )),
            )
            .into_any_element()
    }
}

fn custom_options(layout: Layout, columns: &[u32], rows: &[u32]) -> SplitOptions {
    match layout {
        Layout::Rows => SplitOptions::horizontal(AxisSplit::Weights(rows.to_vec())),
        Layout::Columns => SplitOptions::vertical(AxisSplit::Weights(columns.to_vec())),
        Layout::Grid => SplitOptions {
            columns: AxisSplit::Weights(columns.to_vec()),
            rows: AxisSplit::Weights(rows.to_vec()),
        },
    }
}

fn weight_text(weights: &[u32]) -> String {
    weights
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(":")
}

fn shifted_page(page: usize, delta: i32, pages: usize) -> usize {
    (page as i64 + i64::from(delta)).clamp(0, pages.saturating_sub(1) as i64) as usize
}
fn small_preview(
    image: &Image,
    encoding: SaveEncoding,
    cancel: &AtomicBool,
) -> ToolResult<Arc<RenderImage>> {
    let density = (600. / f64::from(image.width))
        .min(400. / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * density).round().max(1.) as u32;
    let height = (f64::from(image.height) * density).round().max(1.) as u32;
    let mut image = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        ToolLimits::default(),
        cancel,
    )?;
    if matches!(encoding.format, SaveFormat::Jpeg | SaveFormat::Bmp) {
        crate::export::flatten_on_white(&mut image.bgra);
    }
    check_cancel(cancel)?;
    Ok(crate::canvas_source::render_image(
        width, height, image.bgra,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_split_view_reports_success_failure_and_cancellation_without_losing_paths() {
        let piece = SplitPiece {
            index: 0,
            column: 0,
            row: 0,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let path = PathBuf::from("result.png");
        let mut progress = Progress::default();
        for outcome in [
            SplitOutcome::Saved(crate::image_tools::SavedImage {
                path: path.clone(),
                width: 1,
                height: 1,
            }),
            SplitOutcome::Failed("exists".into()),
            SplitOutcome::Cancelled,
        ] {
            progress.record(&SplitItemResult {
                piece,
                path: path.clone(),
                outcome,
            });
        }
        assert_eq!(
            (
                progress.done,
                progress.saved,
                progress.failed,
                progress.cancelled
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(progress.last.unwrap().path, path);
        assert_eq!(shifted_page(0, -1, 0), 0);
        assert_eq!(shifted_page(0, 1, 3), 1);
        assert_eq!(shifted_page(2, 1, 3), 2);
    }
}
