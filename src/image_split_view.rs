//! 图片分割窗口：左侧参数，右侧源图叠加切线、点选切片；切片、命名和保存均由分割计划负责。
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_split::{
        parse_weights, AxisSplit, SplitItemResult, SplitOptions, SplitOutcome, SplitOutput,
        SplitPiece, SplitPlan, SplitReport,
    },
    image_tool_view::{
        host::{self, HostedTool, ToolEvent, ToolHost},
        single::SingleImage,
    },
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    save_options::{SaveEncoding, SaveFormat},
    settings_widgets::ButtonKind,
    text_field::{TextField, TextFieldEvent},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, AnyElement, App, AppContext, Bounds,
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, Pixels,
    Render, RenderImage, Subscription, Window,
};
use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
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
    Step(i32),
    Format(SaveFormat),
    Directory,
    Save,
    Cancel,
    Reveal,
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
    result_page: usize,
    encoding: SaveEncoding,
    directory: Option<PathBuf>,
    directory_prompt: bool,
    /// 还没选目录就点保存：选好目录后接着保存。
    save_after_prompt: bool,
    name: Entity<TextField>,
    request_id: u64,
    pending_preview: bool,
    job: Option<Job>,
    plan: Option<Arc<SplitPlan>>,
    /// 最近一次成功的计划；新计划算出前继续画切线，避免叠加层闪烁。
    shown_plan: Option<Arc<SplitPlan>>,
    piece: Option<SplitPiece>,
    source_preview: Option<Arc<RenderImage>>,
    /// 上一帧源图实际绘制的区域，点击按它换算源像素。
    image_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    progress: Option<Arc<Mutex<Progress>>>,
    report: Option<SplitReport>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    host: ToolHost,
}

/// 嵌进单图工具窗或主界面弹层；源图与另外两个单图工具共用，处理产物保持未保存调整且不复制整帧。
pub(crate) fn embedded(
    source: SingleImage,
    protected: Arc<Vec<PathBuf>>,
    host: ToolHost,
    window: &mut Window,
    cx: &mut App,
) -> Result<Entity<ImageSplitView>> {
    let source = match source {
        SingleImage::Shared(image) => SplitSource::Image(image),
        SingleImage::Processed(image) => SplitSource::Processed(image),
    };
    validate_input(&source, ToolLimits::default())?;
    if protected.len() > ToolLimits::default().max_items {
        return Err(tr!(
            "原片保护路径数量超过上限",
            "Too many protected source paths"
        )
        .into());
    }
    Ok(cx.new(|cx| ImageSplitView::new(source, protected, host, window, cx)))
}

impl HostedTool for ImageSplitView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.job.is_some()
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImageSplitView::request_close(self, cx)
    }
}

impl EventEmitter<ToolEvent> for ImageSplitView {}

impl ImageSplitView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        source: SplitSource,
        protected: Arc<Vec<PathBuf>>,
        host: ToolHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.defer_in(window, |view, window, cx| {
            view.enable_fields(cx);
            view.start_preview(window, cx);
        });
        let initial_name = protected
            .first()
            .and_then(|path| path.file_stem())
            .and_then(|name| name.to_str())
            .map_or_else(|| "Image_split".to_owned(), |name| format!("{name}_split"));
        let focus = cx.focus_handle();
        window.focus(&focus);
        let name =
            cx.new(|cx| TextField::new(initial_name, tr!("文件名前缀", "File name prefix"), cx));
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
        let column_ratio = cx.new(|cx| TextField::new(weight_text(&custom_columns), "1:2:1", cx));
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
            result_page: 0,
            encoding: SaveEncoding::default(),
            directory: None,
            directory_prompt: false,
            save_after_prompt: false,
            name,
            request_id: 1,
            pending_preview: true,
            job: None,
            plan: None,
            shown_plan: None,
            piece: None,
            source_preview: None,
            image_bounds: Rc::default(),
            progress: None,
            report: None,
            notice: String::new(),
            failed: false,
            closing: false,
            focus,
            _subscriptions: subscriptions,
            host,
        }
    }
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
    }

    /// 切片只是计划里的矩形，换选不必重新计算或渲染。
    fn select_piece(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(piece) = self.plan.as_ref().and_then(|plan| plan.piece(index)) else {
            return;
        };
        self.selected = index;
        self.piece = Some(piece);
        cx.notify();
    }

    /// 把窗口坐标换成源像素，落在哪块切片就选哪块。
    fn pick_piece(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let (Some(bounds), Some(plan)) = (self.image_bounds.get(), self.plan.clone()) else {
            return;
        };
        if !bounds.contains(&event.position) || self.closing || self.saving() {
            return;
        }
        let x = f64::from(f32::from(event.position.x - bounds.origin.x))
            / f64::from(f32::from(bounds.size.width))
            * f64::from(self.source.width);
        let y = f64::from(f32::from(event.position.y - bounds.origin.y))
            / f64::from(f32::from(bounds.size.height))
            * f64::from(self.source.height);
        if let Some(index) = piece_at(&plan, x as u32, y as u32) {
            self.select_piece(index, cx);
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
        let prepare_source = self.source_preview.is_none();
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            check_cancel(&work_cancel)?;
            let plan = SplitPlan::new(&source, &options, ToolLimits::default(), request_id)?;
            let selected = plan
                .piece(index.min(plan.len() - 1))
                .expect("nonempty split plan");
            let source_preview = if prepare_source {
                Some(small_preview(&source, &work_cancel)?)
            } else {
                None
            };
            check_cancel(&work_cancel)?;
            Ok::<_, ToolError>(Preview {
                plan: Arc::new(plan),
                selected,
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
                    host::finish_close(view.host, window, cx);
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
                        view.piece = Some(preview.selected);
                        view.shown_plan = Some(preview.plan.clone());
                        view.plan = Some(preview.plan);
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
            let _ = weak.update_in(cx, |view, window, cx| {
                view.directory_prompt = false;
                let save_after = std::mem::take(&mut view.save_after_prompt);
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.directory = Some(path);
                            view.notice.clear();
                            view.failed = false;
                            if save_after {
                                view.save(window, cx);
                            }
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
            self.save_after_prompt = true;
            self.choose_directory(window, cx);
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
                        view.notice.clear();
                    }
                    Err(error) => {
                        view.failed = error != ToolError::Cancelled;
                        view.notice = error.to_string();
                    }
                }
                if view.closing {
                    host::finish_close(view.host, window, cx);
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
                    host::finish_close(self.host, window, cx);
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
            Control::Reveal => {
                let saved = self.report.as_ref().and_then(|report| {
                    report.items.iter().find_map(|item| match &item.outcome {
                        SplitOutcome::Saved(saved) => Some(saved.path.clone()),
                        _ => None,
                    })
                });
                if let Some(Err(error)) = saved.map(|path| crate::library::reveal(&path)) {
                    self.notice = error.to_string();
                    self.failed = true;
                    cx.notify();
                }
                return;
            }
            Control::Step(delta) => {
                let count = self.plan.as_ref().map_or(0, |plan| plan.len());
                if count > 0 {
                    let index = (self.selected as i64 + i64::from(delta)).rem_euclid(count as i64);
                    self.select_piece(index as usize, cx);
                }
                return;
            }
            Control::Format(format) => {
                // 格式只影响保存时的编码与文件名，不改切片计划。
                if !self.closing && !self.saving() {
                    self.encoding.format = format;
                    cx.notify();
                }
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
        }
        self.enable_fields(cx);
        self.changed(window, cx);
    }
    fn set_layout(&mut self, layout: Layout, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Layout(layout), window, cx);
    }
    fn set_measure(&mut self, measure: Measure, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Measure(measure), window, cx);
    }
    fn set_ratio(&mut self, ratio: u8, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Ratio(ratio), window, cx);
    }
    fn set_format(&mut self, format: SaveFormat, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Format(format), window, cx);
    }

    /// 源图预览：按比例居中，叠加所有切线并高亮当前切片；点哪块选哪块。
    fn stage(&self, c: chrome::Chrome) -> AnyElement {
        let Some(preview) = self.source_preview.clone() else {
            let text = if self.job.is_some() || self.pending_preview {
                tr!("正在准备预览…", "Preparing preview…")
            } else {
                tr!("预览未就绪", "Preview is not ready")
            };
            return chrome::empty_state(Icon::Grid, text, c).into_any_element();
        };
        let dimensions = (self.source.width, self.source.height);
        let plan = self.shown_plan.clone();
        let selected = self.piece;
        let slot = self.image_bounds.clone();
        let palette = c.palette;
        canvas(
            move |bounds, _, _| {
                let scale = (f32::from(bounds.size.width) / dimensions.0 as f32)
                    .min(f32::from(bounds.size.height) / dimensions.1 as f32);
                let fitted = size(
                    px((dimensions.0 as f32 * scale).floor().max(1.)),
                    px((dimensions.1 as f32 * scale).floor().max(1.)),
                );
                let image = Bounds::new(
                    point(
                        bounds.origin.x + (bounds.size.width - fitted.width) / 2.,
                        bounds.origin.y + (bounds.size.height - fitted.height) / 2.,
                    ),
                    fitted,
                );
                slot.set(Some(image));
                image
            },
            move |_, image, window, _| {
                let _ = window.paint_image_exact(image, px(0.).into(), preview, 0, false);
                let Some(plan) = plan else {
                    return;
                };
                let to_x =
                    |x: u32| image.origin.x + image.size.width * (x as f32 / dimensions.0 as f32);
                let to_y =
                    |y: u32| image.origin.y + image.size.height * (y as f32 / dimensions.1 as f32);
                let rect = |piece: SplitPiece| {
                    Bounds::from_corners(
                        point(to_x(piece.x), to_y(piece.y)),
                        point(to_x(piece.x + piece.width), to_y(piece.y + piece.height)),
                    )
                };
                if let Some(piece) = selected {
                    window.paint_quad(gpui::fill(rect(piece), rgba((palette.accent << 8) | 0x40)));
                }
                // 切线画在每块的右边和下边，外框单独画一圈。
                for index in 0..plan.len() {
                    let Some(piece) = plan.piece(index) else {
                        break;
                    };
                    let bounds = rect(piece);
                    if piece.x + piece.width < dimensions.0 {
                        let x = bounds.origin.x + bounds.size.width;
                        line(
                            window,
                            point(x, bounds.origin.y),
                            point(x, bounds.origin.y + bounds.size.height),
                            palette.accent,
                        );
                    }
                    if piece.y + piece.height < dimensions.1 {
                        let y = bounds.origin.y + bounds.size.height;
                        line(
                            window,
                            point(bounds.origin.x, y),
                            point(bounds.origin.x + bounds.size.width, y),
                            palette.accent,
                        );
                    }
                }
                if let Some(piece) = selected {
                    let bounds = rect(piece);
                    window.paint_quad(
                        gpui::outline(bounds, rgb(palette.accent), gpui::BorderStyle::Solid)
                            .border_widths(px(2.)),
                    );
                }
            },
        )
        .size_full()
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
        host::set_title(self.host, window, brand_title!("图片分割", "Split image"));
        let c = chrome::chrome(cx);
        let enabled = !self.closing && !self.saving();
        let busy = self.job.is_some() || self.pending_preview;

        // —— 切法 ——
        let direction = chrome::field(tr!("方向", "Direction"), c).child(chrome::choice(
            "split-layout",
            &[
                (tr!("横条", "Rows"), Layout::Rows),
                (tr!("竖条", "Columns"), Layout::Columns),
                (tr!("网格", "Grid"), Layout::Grid),
            ],
            self.layout,
            enabled,
            c,
            cx,
            Self::set_layout,
        ));
        let mut method = chrome::field(tr!("切法", "Method"), c).child(chrome::choice(
            "split-measure",
            &[
                (tr!("均分", "Equal"), Measure::Parts),
                (tr!("按像素", "Pixels"), Measure::Pixels),
                (tr!("比例", "Ratio"), Measure::Ratio),
                (tr!("自定", "Custom"), Measure::CustomRatio),
            ],
            self.measure,
            enabled,
            c,
            cx,
            Self::set_measure,
        ));
        match self.measure {
            Measure::Parts | Measure::Pixels => {
                let parts = self.measure == Measure::Parts;
                let step = if parts { 1 } else { 128 };
                let value = |count: u32| {
                    if parts {
                        count.to_string()
                    } else {
                        format!("{count} px")
                    }
                };
                if self.layout != Layout::Rows {
                    method = method.child(
                        chrome::option_row(
                            if parts {
                                tr!("列数", "Columns")
                            } else {
                                tr!("块宽", "Width")
                            },
                            c,
                        )
                        .child(chrome::stepper(
                            "split-columns",
                            value(self.columns),
                            enabled && self.columns > 1,
                            enabled,
                            c,
                            cx,
                            move |view, window, cx| {
                                view.control(Control::Columns(-step), window, cx)
                            },
                            move |view, window, cx| {
                                view.control(Control::Columns(step), window, cx)
                            },
                        )),
                    );
                }
                if self.layout != Layout::Columns {
                    method = method.child(
                        chrome::option_row(
                            if parts {
                                tr!("行数", "Rows")
                            } else {
                                tr!("块高", "Height")
                            },
                            c,
                        )
                        .child(chrome::stepper(
                            "split-rows",
                            value(self.rows),
                            enabled && self.rows > 1,
                            enabled,
                            c,
                            cx,
                            move |view, window, cx| view.control(Control::Rows(-step), window, cx),
                            move |view, window, cx| view.control(Control::Rows(step), window, cx),
                        )),
                    );
                }
            }
            Measure::Ratio => {
                method = method.child(chrome::choice(
                    "split-ratio",
                    &[("1 : 2", 0), ("2 : 1", 1), ("1 : 2 : 1", 2)],
                    self.ratio,
                    enabled,
                    c,
                    cx,
                    Self::set_ratio,
                ));
                if self.layout == Layout::Grid {
                    method = method.child(chrome::hint(
                        tr!(
                            "行和列使用同一比例。",
                            "Rows and columns use the same ratio."
                        ),
                        c,
                    ));
                }
            }
            Measure::CustomRatio => {
                if self.layout != Layout::Rows {
                    method = method.child(
                        chrome::option_row(tr!("列宽比", "Columns"), c).child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(self.column_ratio.clone()),
                        ),
                    );
                }
                if self.layout != Layout::Columns {
                    method = method.child(
                        chrome::option_row(tr!("行高比", "Rows"), c)
                            .child(div().flex_1().min_w(px(0.)).child(self.row_ratio.clone())),
                    );
                }
                let dirty = self.ratios_dirty(cx);
                let apply = chrome::action(
                    "split-apply-ratios",
                    tr!("应用比例", "Apply ratios"),
                    if dirty {
                        ButtonKind::Primary
                    } else {
                        ButtonKind::Secondary
                    },
                    enabled,
                    c,
                );
                method = method
                    .child(div().flex().child(if enabled {
                        apply.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::ApplyRatios, window, cx)
                        }))
                    } else {
                        apply
                    }))
                    .child(chrome::hint(
                        if dirty {
                            tr!(
                                "改动尚未应用，按回车或点“应用比例”更新。",
                                "Not applied yet; press Enter or Apply ratios."
                            )
                        } else {
                            tr!(
                                "用冒号分隔正整数，例如 1:2:1。",
                                "Separate positive integers with colons, e.g. 1:2:1."
                            )
                        },
                        c,
                    ));
            }
        }

        // —— 保存 ——
        let planned = self
            .piece
            .zip(self.directory.clone())
            .map(|(piece, directory)| {
                SplitOutput::new(directory, self.name.read(cx).text(), self.encoding)
                    .and_then(|output| output.path_for(piece))
            })
            .transpose();
        let example = match planned {
            Ok(Some(path)) => Some(trf!(
                "例：{}",
                "e.g. {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
            Err(error) => Some(error.to_string()),
            Ok(None) => None,
        };
        let format = chrome::field(tr!("格式", "Format"), c).child(chrome::choice(
            "split-format",
            &[
                ("PNG", SaveFormat::Png),
                ("JPEG", SaveFormat::Jpeg),
                ("BMP", SaveFormat::Bmp),
                ("WebP", SaveFormat::Webp),
            ],
            self.encoding.format,
            enabled,
            c,
            cx,
            Self::set_format,
        ));
        let name = chrome::field(tr!("文件名", "File name"), c)
            .child(self.name.clone())
            .when_some(example, |field, example| {
                field.child(chrome::hint(example, c))
            });
        let change = chrome::action(
            "split-folder",
            if self.directory.is_some() {
                tr!("更改…", "Change…")
            } else {
                tr!("选择…", "Choose…")
            },
            ButtonKind::Secondary,
            enabled && !self.directory_prompt,
            c,
        );
        let folder = chrome::field(tr!("保存到", "Save to"), c).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(c.palette.muted))
                        .child(self.directory.as_ref().map_or_else(
                            || tr!("保存时选择文件夹", "Choose when saving").to_owned(),
                            |path| path.to_string_lossy().into_owned(),
                        )),
                )
                .child(if enabled && !self.directory_prompt {
                    change.on_click(cx.listener(|view, _, window, cx| {
                        view.control(Control::Directory, window, cx)
                    }))
                } else {
                    change
                }),
        );

        let mut sidebar = chrome::sidebar("split-sidebar", c)
            .child(direction)
            .child(method)
            .child(format)
            .child(name)
            .child(folder);

        // —— 上次保存的结果：成功只给计数，失败逐项列出 ——
        if let Some(report) = &self.report {
            let saved = report
                .items
                .iter()
                .filter(|item| matches!(item.outcome, SplitOutcome::Saved(_)))
                .count();
            let problems: Vec<_> = report
                .items
                .iter()
                .filter(|item| !matches!(item.outcome, SplitOutcome::Saved(_)))
                .collect();
            let mut result = chrome::field(tr!("上次保存", "Last save"), c).child(chrome::hint(
                if problems.is_empty() {
                    trf!("已保存全部 {saved} 块。", "Saved all {saved} pieces.")
                } else {
                    trf!(
                        "已保存 {saved} 块，{} 块未保存：",
                        "Saved {saved}; {} not saved:",
                        problems.len()
                    )
                },
                c,
            ));
            let start = self.result_page * PAGE_SIZE;
            for item in problems.iter().skip(start).take(PAGE_SIZE) {
                let text = match &item.outcome {
                    SplitOutcome::Failed(error) => {
                        trf!(
                            "第 {} 块：{error}",
                            "Piece {}: {error}",
                            item.piece.index + 1
                        )
                    }
                    _ => trf!(
                        "第 {} 块：已取消",
                        "Piece {}: cancelled",
                        item.piece.index + 1
                    ),
                };
                result = result.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(c.palette.danger))
                        .child(text),
                );
            }
            if problems.len() > PAGE_SIZE {
                result = result.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(chrome::row_icon(
                            "split-results-prev",
                            Icon::ChevronLeft,
                            self.result_page > 0,
                            c,
                            cx,
                            |view, window, cx| view.control(Control::ResultPage(-1), window, cx),
                        ))
                        .child(chrome::row_icon(
                            "split-results-next",
                            Icon::ChevronRight,
                            start + PAGE_SIZE < problems.len(),
                            c,
                            cx,
                            |view, window, cx| view.control(Control::ResultPage(1), window, cx),
                        )),
                );
            }
            if saved > 0 {
                result = result.child(
                    div().flex().child(
                        chrome::action(
                            "split-reveal",
                            tr!("在文件夹中显示", "Show in folder"),
                            ButtonKind::Secondary,
                            true,
                            c,
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::Reveal, window, cx)
                        })),
                    ),
                );
            }
            sidebar = sidebar.child(result);
        }

        // —— 预览 ——
        let count = self.plan.as_ref().map_or(0, |plan| plan.len());
        let piece_text = self.piece.map(|piece| {
            trf!(
                "第 {} / {count} 块 · {} × {}",
                "Piece {} / {count} · {} × {}",
                piece.index + 1,
                piece.width,
                piece.height
            )
        });
        let navigable = count > 1 && enabled;
        let bar = chrome::stage_bar(c)
            .child(div().flex_1().min_w(px(0.)).truncate().child(trf!(
                "源图 {} × {} · 点选切片查看尺寸",
                "Source {} × {} · click a piece to inspect it",
                self.source.width,
                self.source.height
            )))
            .when_some(piece_text, |bar, text| {
                bar.child(chrome::row_icon(
                    "split-prev",
                    Icon::ChevronLeft,
                    navigable,
                    c,
                    cx,
                    |view, window, cx| view.control(Control::Step(-1), window, cx),
                ))
                .child(
                    div()
                        .text_color(rgb(c.palette.foreground))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(text),
                )
                .child(chrome::row_icon(
                    "split-next",
                    Icon::ChevronRight,
                    navigable,
                    c,
                    cx,
                    |view, window, cx| view.control(Control::Step(1), window, cx),
                ))
            });
        let stage = chrome::stage(c)
            .id("split-stage")
            .flex_1()
            .min_h(px(0.))
            .p_4()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _, cx| view.pick_piece(event, cx)),
            )
            .child(self.stage(c));
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
            .child(stage);

        // —— 底栏 ——
        let progress = self
            .progress
            .as_ref()
            .and_then(|progress| progress.lock().ok().map(|progress| progress.clone()))
            .unwrap_or_default();
        let (status, tone): (String, Tone) = if self.saving() {
            (
                trf!(
                    "正在保存 {}/{}…",
                    "Saving {}/{}…",
                    progress.done,
                    progress.total
                ),
                Tone::Busy,
            )
        } else if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Idle
                },
            )
        } else if busy {
            (tr!("正在更新预览…", "Updating preview…").into(), Tone::Busy)
        } else if let Some(report) = &self.report {
            let saved = report
                .items
                .iter()
                .filter(|item| matches!(item.outcome, SplitOutcome::Saved(_)))
                .count();
            (
                trf!(
                    "已保存 {saved}/{} 块",
                    "Saved {saved}/{} pieces",
                    report.items.len()
                ),
                if saved == report.items.len() {
                    Tone::Done
                } else {
                    Tone::Failed
                },
            )
        } else {
            (trf!("将切成 {count} 块", "{count} pieces"), Tone::Idle)
        };
        let mut actions = Vec::new();
        if busy {
            actions.push(
                chrome::action(
                    "split-cancel",
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
        let can_save = self.ready();
        let save = chrome::action(
            "split-save",
            trf!("保存 {count} 块…", "Save {count} pieces…"),
            ButtonKind::Primary,
            can_save,
            c,
        );
        actions.push(
            if can_save {
                save.on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Save, window, cx)),
                )
            } else {
                save
            }
            .into_any_element(),
        );

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.name.read(cx).is_composing() || view.ratios_composing(cx) {
                    return;
                }
                let plain = !event.keystroke.modifiers.modified();
                match event.keystroke.key.as_str() {
                    "s" if event.keystroke.modifiers.control => {
                        if !event.is_held {
                            view.save(window, cx);
                        }
                    }
                    "escape" if plain => {
                        if !event.is_held {
                            let action = if view.job.is_some() {
                                Control::Cancel
                            } else {
                                Control::Close
                            };
                            view.control(action, window, cx);
                        }
                    }
                    // 焦点在输入框里时方向键归输入框，这里只在窗口焦点上换片。
                    "left" | "up" if plain && view.focus.is_focused(window) => {
                        view.control(Control::Step(-1), window, cx)
                    }
                    "right" | "down" if plain && view.focus.is_focused(window) => {
                        view.control(Control::Step(1), window, cx)
                    }
                    _ => return,
                }
                cx.stop_propagation();
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

/// 源像素坐标落在哪块切片；计划的行列边界单调递增。
fn piece_at(plan: &SplitPlan, x: u32, y: u32) -> Option<usize> {
    (0..plan.len()).find(|&index| {
        plan.piece(index).is_some_and(|piece| {
            (piece.x..piece.x + piece.width).contains(&x)
                && (piece.y..piece.y + piece.height).contains(&y)
        })
    })
}

fn line(window: &mut Window, from: gpui::Point<Pixels>, to: gpui::Point<Pixels>, color: u32) {
    let mut path = gpui::PathBuilder::stroke(px(1.));
    path.move_to(from);
    path.line_to(to);
    if let Ok(path) = path.build() {
        window.paint_path(path, rgb(color));
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
fn small_preview(image: &Image, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
    let density = (1200. / f64::from(image.width))
        .min(900. / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * density).round().max(1.) as u32;
    let height = (f64::from(image.height) * density).round().max(1.) as u32;
    let image = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        ToolLimits::default(),
        cancel,
    )?;
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

    #[test]
    fn image_split_view_click_maps_source_pixels_to_pieces() {
        let source = Image {
            width: 10,
            height: 7,
            bgra: vec![0; 10 * 7 * 4],
            readback_ms: 0.,
        };
        let options = SplitOptions {
            columns: AxisSplit::EqualParts(3),
            rows: AxisSplit::EqualParts(2),
        };
        let plan = SplitPlan::new(&source, &options, ToolLimits::default(), 1).unwrap();
        assert_eq!(piece_at(&plan, 0, 0), Some(0));
        assert_eq!(piece_at(&plan, 9, 0), Some(2));
        assert_eq!(piece_at(&plan, 0, 6), Some(3));
        assert_eq!(piece_at(&plan, 9, 6), Some(5));
        assert_eq!(piece_at(&plan, 10, 0), None);
    }
}
