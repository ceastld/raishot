//! 批处理窗口：左侧选截图与参数，右侧逐项结果；来源与参数按次冻结，逐项处理直接交给批处理模型。
use crate::{
    image_batch::{
        watermark::WatermarkOptions, BatchItemResult, BatchLimits, BatchNaming, BatchOptions,
        BatchOutcome, BatchPlan,
    },
    image_resize::ResizeFilter,
    image_tool_view::{
        host::{self, HostedTool, ToolEvent, ToolHost},
        strip::{self, ThumbHost, Thumbs},
    },
    image_tools::{validate_output, ToolError},
    image_transform::{
        color::ColorAdjustments, sharpen::SharpenOptions, ResizeOptions, TransformOptions,
    },
    image_webp_encode::WebpMode,
    pin_model::Adjustments,
    save_options::{PngCompression, SaveEncoding, SaveFormat, DATE_FORMATS},
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
    tool_recent_captures::{RecentCaptures, RecentHost},
    tool_window_chrome::{self as chrome, Chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, AppContext, Context, Div, ElementId, Entity,
    EventEmitter, FocusHandle, FontWeight, KeyDownEvent, PathPromptOptions, Render, SharedString,
    Subscription, Window, WindowHandle,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const PAGE_SIZE: usize = 40;
const MAX_CLIPBOARD_PATH_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SizePreset {
    Original,
    Fixed1024,
    Fixed1920,
    Custom(u32, u32),
}

impl SizePreset {
    fn resize(self) -> Option<ResizeOptions> {
        let (width, height) = match self {
            Self::Original => return None,
            Self::Fixed1024 => (1024, 768),
            Self::Fixed1920 => (1920, 1080),
            Self::Custom(width, height) => (width, height),
        };
        Some(ResizeOptions {
            width,
            height,
            filter: ResizeFilter::Bilinear,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Sources,
    Directory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NamingMode {
    SourceStem,
    Template,
}

#[derive(Clone, Copy)]
enum Control {
    Add,
    Clear,
    Directory,
    Move(usize, usize),
    Remove(usize),
    Page(usize),
    Format(SaveFormat),
    Compression(PngCompression),
    Quality(i16),
    WebpMode(WebpMode),
    WebpQuality(i16),
    Size(SizePreset),
    ApplySize,
    Rotate,
    FlipX,
    FlipY,
    Grayscale,
    Invert,
    Brightness(i16),
    Contrast(i16),
    Saturation(i16),
    Sharpen(u16),
    ResetColors,
    ToggleWatermark,
    WatermarkSize(u16),
    WatermarkOpacity(u8),
    WatermarkColor(u32),
    Naming(NamingMode),
    DateFormat(usize),
    Start,
    Cancel,
    Copy(usize),
    CopyAll,
    Close,
}

/// 固定长度的结果槽只存路径与终态，回调不会持有图片或无限积压消息。
struct ProgressInbox {
    slots: Vec<Option<BatchItemResult>>,
    changed: bool,
}

impl ProgressInbox {
    fn new(count: usize) -> Self {
        Self {
            slots: vec![None; count],
            changed: false,
        }
    }

    fn publish(&mut self, item: &BatchItemResult) {
        if let Some(slot) = self.slots.get_mut(item.index) {
            *slot = Some(item.clone());
            self.changed = true;
        }
    }

    fn drain_into(&mut self, rows: &mut [Option<BatchItemResult>]) -> bool {
        if !self.changed {
            return false;
        }
        for (destination, source) in rows.iter_mut().zip(&mut self.slots) {
            if let Some(item) = source.take() {
                merge_result(destination, item);
            }
        }
        self.changed = false;
        true
    }
}

struct ActiveBatch {
    request_id: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ProgressInbox>>,
}

#[derive(Clone)]
struct FrozenBatch {
    plan: Arc<BatchPlan>,
    colors: ColorAdjustments,
    sharpening: SharpenOptions,
    watermark: Option<WatermarkOptions>,
}

pub(crate) struct ImageBatchView {
    paths: Vec<PathBuf>,
    directory: Option<PathBuf>,
    encoding: SaveEncoding,
    size: SizePreset,
    width: Entity<TextField>,
    height: Entity<TextField>,
    size_dirty: bool,
    adjustments: Adjustments,
    colors: ColorAdjustments,
    sharpening: SharpenOptions,
    watermark_enabled: bool,
    watermark: WatermarkOptions,
    watermark_text: Entity<TextField>,
    suffix: Entity<TextField>,
    template: Entity<TextField>,
    naming_mode: NamingMode,
    date_format: &'static str,
    request_id: u64,
    active: Option<ActiveBatch>,
    frozen: Option<FrozenBatch>,
    prompt: Option<PromptKind>,
    rows: Vec<Option<BatchItemResult>>,
    page: usize,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    recent: RecentCaptures,
    _subscriptions: Vec<Subscription>,
    host: ToolHost,
    thumbs: Thumbs,
}

impl ThumbHost for ImageBatchView {
    fn thumbs(&mut self) -> &mut Thumbs {
        &mut self.thumbs
    }
}

impl RecentHost for ImageBatchView {
    fn recent(&mut self) -> &mut RecentCaptures {
        &mut self.recent
    }
}

/// 可以传空列表，再由窗口中的多选文件对话框添加；本入口不读取文件内容。
pub(crate) fn open(paths: Vec<PathBuf>, cx: &mut App) -> Result<WindowHandle<ImageBatchView>> {
    if paths.len() > BatchLimits::default().image.max_items {
        return Err(tr!("批处理图片数量超过上限", "Too many batch images").into());
    }
    host::open_window(
        "batch",
        brand_title!("批量导出", "Batch export").into(),
        (1120., 800.),
        (680., 480.),
        cx,
        move |host, window, cx| ImageBatchView::new(paths, host, window, cx),
    )
}

/// 在主界面弹层里打开，来源按图库选择顺序。
pub(crate) fn popup(
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> Result<Entity<ImageBatchView>> {
    if paths.len() > BatchLimits::default().image.max_items {
        return Err(tr!("批处理图片数量超过上限", "Too many batch images").into());
    }
    Ok(cx.new(|cx| ImageBatchView::new(paths, ToolHost::Popup, window, cx)))
}

impl HostedTool for ImageBatchView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.locked()
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImageBatchView::request_close(self, cx)
    }
    fn sources(&self) -> Vec<PathBuf> {
        self.paths.clone()
    }
    fn set_sources(&mut self, paths: Vec<PathBuf>, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.paths == paths {
            return;
        }
        let max_items = BatchLimits::default().image.max_items;
        self.paths = paths.into_iter().take(max_items).collect();
        self.page = 0;
        self.invalidate();
        cx.notify();
    }
}

impl EventEmitter<ToolEvent> for ImageBatchView {}

impl ImageBatchView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        paths: Vec<PathBuf>,
        host: ToolHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        RecentCaptures::load(cx);
        let suffix =
            cx.new(|cx| TextField::new("_processed".into(), tr!("输出后缀", "Output suffix"), cx));
        let template = cx.new(|cx| {
            TextField::new(
                crate::save_options::default_template().into(),
                "{date}_{time}_{width}x{height}",
                cx,
            )
        });
        let watermark_text = cx.new(|cx| {
            TextField::new(
                String::new(),
                tr!(
                    "水印文字（最多 4096 字符）",
                    "Watermark text (up to 4096 characters)"
                ),
                cx,
            )
        });
        let width = cx.new(|cx| TextField::new("1024".into(), tr!("最终宽度", "Final width"), cx));
        let height = cx.new(|cx| TextField::new("768".into(), tr!("最终高度", "Final height"), cx));
        let fields = [
            suffix.clone(),
            template.clone(),
            watermark_text.clone(),
            width.clone(),
            height.clone(),
        ];
        let focus_group: Vec<_> = fields.iter().map(|field| field.downgrade()).collect();
        let mut subscriptions = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            field.update(cx, |field, cx| {
                field.configure_focus(focus_group.clone(), window, cx);
                field.set_enabled(index == 0 || index >= 3, cx);
            });
            let size_field = index >= 3;
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                move |view: &mut ImageBatchView, _, event, window, cx| match event {
                    TextFieldEvent::Changed if !view.locked() => {
                        if size_field {
                            view.size_dirty = true;
                        } else {
                            view.invalidate();
                        }
                        cx.notify();
                    }
                    TextFieldEvent::Submit if size_field => view.apply_size(window, cx),
                    TextFieldEvent::Submit => view.start(window, cx),
                    _ => {}
                },
            ));
        }
        let rows = vec![None; paths.len()];
        ImageBatchView {
            paths,
            directory: None,
            encoding: SaveEncoding::default(),
            size: SizePreset::Original,
            width,
            height,
            size_dirty: true,
            adjustments: Adjustments::default(),
            colors: ColorAdjustments::default(),
            sharpening: SharpenOptions::default(),
            watermark_enabled: false,
            watermark: WatermarkOptions::default(),
            watermark_text,
            suffix,
            template,
            naming_mode: NamingMode::SourceStem,
            date_format: DATE_FORMATS[0],
            request_id: 1,
            active: None,
            frozen: None,
            prompt: None,
            rows,
            page: 0,
            notice: String::new(),
            failed: false,
            closing: false,
            focus,
            recent: RecentCaptures::default(),
            _subscriptions: subscriptions,
            host,
            thumbs: Thumbs::default(),
        }
    }
}

impl ImageBatchView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
            self.closing = true;
            self.notice = tr!(
                "正在取消并关闭，已成功的文件保留…",
                "Cancelling and closing; saved files will be kept…"
            )
            .into();
            self.failed = false;
            cx.notify();
            false
        } else {
            true
        }
    }

    fn locked(&self) -> bool {
        self.active.is_some() || self.prompt.is_some() || self.closing
    }

    fn invalidate(&mut self) {
        self.request_id = self.request_id.wrapping_add(1);
        self.frozen = None;
        self.rows = vec![None; self.paths.len()];
        self.page = self.page.min(page_count(self.paths.len()) - 1);
        self.notice.clear();
        self.failed = false;
    }

    fn enable_parameter_fields(&self, cx: &mut Context<Self>) {
        let unlocked = !self.locked();
        self.suffix.update(cx, |field, cx| {
            field.set_enabled(unlocked && self.naming_mode == NamingMode::SourceStem, cx)
        });
        self.template.update(cx, |field, cx| {
            field.set_enabled(unlocked && self.naming_mode == NamingMode::Template, cx)
        });
        self.watermark_text.update(cx, |field, cx| {
            field.set_enabled(unlocked && self.watermark_enabled, cx)
        });
        for field in [&self.width, &self.height] {
            field.update(cx, |field, cx| field.set_enabled(unlocked, cx));
        }
    }

    fn parameters_composing(&self, cx: &App) -> bool {
        let naming = match self.naming_mode {
            NamingMode::SourceStem => self.suffix.read(cx).is_composing(),
            NamingMode::Template => self.template.read(cx).is_composing(),
        };
        naming
            || (self.watermark_enabled && self.watermark_text.read(cx).is_composing())
            || self.width.read(cx).is_composing()
            || self.height.read(cx).is_composing()
    }

    fn sync_size_fields(&mut self, cx: &mut Context<Self>) {
        if let Some(size) = self.size.resize() {
            self.width
                .update(cx, |field, cx| field.set_text(size.width.to_string(), cx));
            self.height
                .update(cx, |field, cx| field.set_text(size.height.to_string(), cx));
            self.size_dirty = false;
        } else {
            // 各来源尺寸不同；原尺寸模式不为填充草稿读取任一文件。
            self.size_dirty = true;
        }
    }

    fn apply_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.parameters_composing(cx) {
            return;
        }
        let candidate = parse_dimensions(self.width.read(cx).text(), self.height.read(cx).text());
        let (width, height) = match candidate {
            Ok(size) => size,
            Err(error) => {
                self.notice = trf!(
                    "{error}；已应用参数和已有结果保持不变",
                    "{error}; applied parameters and existing results are kept"
                );
                self.failed = true;
                cx.notify();
                return;
            }
        };
        window.focus(&self.focus);
        let changed = self.size.resize() != SizePreset::Custom(width, height).resize();
        self.size = SizePreset::Custom(width, height);
        self.sync_size_fields(cx);
        if changed {
            self.invalidate();
        } else {
            self.notice.clear();
            self.failed = false;
        }
        cx.notify();
    }

    fn prompt(&mut self, kind: PromptKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        self.prompt = Some(kind);
        window.focus(&self.focus);
        self.enable_parameter_fields(cx);
        let request_id = self.request_id;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: kind == PromptKind::Sources,
            directories: kind == PromptKind::Directory,
            multiple: kind == PromptKind::Sources,
            prompt: Some(if kind == PromptKind::Sources {
                tr!("添加待处理图片", "Add batch images").into()
            } else {
                tr!("选择输出目录", "Choose an output folder").into()
            }),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, _, cx| {
                if view.prompt != Some(kind) {
                    return;
                }
                view.prompt = None;
                view.enable_parameter_fields(cx);
                if view.active.is_some() || request_id != view.request_id {
                    return;
                }
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        let changed = match kind {
                            PromptKind::Sources => {
                                let max_items = BatchLimits::default().image.max_items;
                                if paths.len() > max_items.saturating_sub(view.paths.len()) {
                                    view.notice = trf!(
                                        "最多处理 {max_items} 张图片，请减少本次选择",
                                        "Choose fewer files; the limit is {max_items} images"
                                    );
                                    view.failed = true;
                                    cx.notify();
                                    return;
                                }
                                let changed = !paths.is_empty();
                                view.paths.extend(paths);
                                changed
                            }
                            PromptKind::Directory => {
                                if let Some(path) = paths.into_iter().next() {
                                    let changed = view.directory.as_ref() != Some(&path);
                                    view.directory = Some(path);
                                    changed
                                } else {
                                    false
                                }
                            }
                        };
                        if changed {
                            view.invalidate();
                        }
                    }
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                    Ok(Ok(None)) | Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.paths.is_empty() || self.parameters_composing(cx) {
            return;
        }
        let frozen = self.frozen.clone();
        let indices = retry_indices(&self.rows);
        if indices.is_empty() {
            self.notice = tr!(
                "本批所有文件均已成功保存",
                "All files in this batch are already saved"
            )
            .into();
            self.failed = false;
            cx.notify();
            return;
        }
        let Some(directory) = self.directory.clone() else {
            self.notice = tr!("请先选择输出目录", "Choose an output folder first").into();
            self.failed = true;
            cx.notify();
            return;
        };
        window.focus(&self.focus);
        self.request_id = self.request_id.wrapping_add(1);
        let request_id = self.request_id;
        let paths = self.paths.clone();
        let options = BatchOptions {
            directory,
            encoding: self.encoding,
            transform: TransformOptions {
                adjustments: self.adjustments,
                resize: self.size.resize(),
            },
            naming: match self.naming_mode {
                NamingMode::SourceStem => BatchNaming::SourceStem {
                    suffix: self.suffix.read(cx).text().to_owned(),
                },
                NamingMode::Template => BatchNaming::Template {
                    template: self.template.read(cx).text().to_owned(),
                    date_format: self.date_format.into(),
                },
            },
        };
        let colors = self.colors;
        let sharpening = self.sharpening;
        let watermark = self.watermark_enabled.then(|| WatermarkOptions {
            text: self.watermark_text.read(cx).text().to_owned(),
            ..self.watermark.clone()
        });
        // 保留已提交的行；相同计划重试时只清掉这次参与处理的旧终态。
        for &index in &indices {
            self.rows[index] = None;
        }
        self.notice.clear();
        self.failed = false;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(ProgressInbox::new(paths.len())));
        self.active = Some(ActiveBatch {
            request_id,
            cancel: cancel.clone(),
            progress: progress.clone(),
        });
        self.enable_parameter_fields(cx);
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            let frozen = match frozen {
                Some(frozen) => frozen,
                None => FrozenBatch {
                    plan: Arc::new(BatchPlan::new(
                        &paths,
                        options,
                        BatchLimits::default(),
                        request_id,
                    )?),
                    colors,
                    sharpening,
                    watermark,
                },
            };
            let report = frozen.plan.run_selected_with_sharpening(
                frozen.plan.request_id(),
                &indices,
                frozen.colors,
                frozen.sharpening,
                frozen.watermark.clone(),
                &work_cancel,
                |item| {
                    if let Ok(mut inbox) = progress.lock() {
                        inbox.publish(item);
                    }
                },
            )?;
            Ok::<_, ToolError>((frozen, report))
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
                view.enable_parameter_fields(cx);
                match result {
                    Ok((frozen, report)) => {
                        // 取消只影响后续项，真实已提交的成功结果仍显示为成功。
                        view.frozen = Some(frozen);
                        merge_results(&mut view.rows, report.items);
                        let (saved, failed, cancelled) = result_counts(&view.rows);
                        view.notice = trf!(
                            "完成：成功 {saved}，失败 {failed}，取消 {cancelled}",
                            "Finished: {saved} saved, {failed} failed, {cancelled} cancelled"
                        );
                        view.failed = failed > 0;
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
        cx.spawn_in(window, async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(75))
                .await;
            let keep_polling = weak
                .update_in(cx, |view, _, cx| {
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
            if !keep_polling {
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
        let progress = active.progress.clone();
        let Ok(mut inbox) = progress.lock() else {
            return false;
        };
        inbox.drain_into(&mut self.rows)
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
                        "正在取消，已成功的文件保留…",
                        "Cancelling; saved files will be kept…"
                    )
                    .into();
                    self.failed = false;
                    cx.notify();
                }
                return;
            }
            Control::Page(page) => {
                self.page = page.min(page_count(self.paths.len()) - 1);
                cx.notify();
                return;
            }
            Control::Copy(index) => {
                if let Some(Some(BatchItemResult {
                    outcome: BatchOutcome::Saved(saved),
                    ..
                })) = self.rows.get(index)
                {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        saved.path.to_string_lossy().into_owned(),
                    ));
                }
                return;
            }
            Control::CopyAll => {
                self.flush_progress();
                match saved_path_list(&self.rows, MAX_CLIPBOARD_PATH_BYTES) {
                    Ok(paths) if !paths.is_empty() => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(paths));
                        let count = result_counts(&self.rows).0;
                        self.notice = trf!(
                            "已复制 {count} 个成功文件的路径",
                            "Copied {count} saved file paths"
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
                return;
            }
            _ if self.locked() => return,
            Control::Add => {
                self.prompt(PromptKind::Sources, window, cx);
                return;
            }
            Control::Directory => {
                self.prompt(PromptKind::Directory, window, cx);
                return;
            }
            Control::Clear => {
                self.paths.clear();
                self.invalidate();
                self.notice = tr!(
                    "已清空来源列表，已保存的文件保留",
                    "Source list cleared; saved files are kept"
                )
                .into();
                cx.notify();
                return;
            }
            Control::Start => {
                self.start(window, cx);
                return;
            }
            Control::Move(from, before) => {
                let original = self.paths.clone();
                if !strip::move_before(&mut self.paths, from, before) || self.paths == original {
                    self.paths = original;
                    return;
                }
                let to = if before > from { before - 1 } else { before };
                self.page = to / PAGE_SIZE;
            }
            Control::Remove(index) => {
                if index >= self.paths.len() {
                    return;
                }
                self.paths.remove(index);
            }
            Control::Format(format) => {
                if self.encoding.format == format {
                    return;
                }
                self.encoding.format = format;
            }
            Control::Compression(compression) => {
                if self.encoding.format != SaveFormat::Png
                    || self.encoding.png_compression == compression
                {
                    return;
                }
                self.encoding.png_compression = compression;
            }
            Control::Quality(delta) => {
                if self.encoding.format != SaveFormat::Jpeg {
                    return;
                }
                let quality = (i16::from(self.encoding.jpeg_quality) + delta).clamp(1, 100) as u8;
                if self.encoding.jpeg_quality == quality {
                    return;
                }
                self.encoding.jpeg_quality = quality;
            }
            Control::WebpMode(mode) => {
                if self.encoding.format != SaveFormat::Webp || self.encoding.webp.mode == mode {
                    return;
                }
                self.encoding.webp.mode = mode;
            }
            Control::WebpQuality(delta) => {
                if self.encoding.format != SaveFormat::Webp
                    || self.encoding.webp.mode != WebpMode::Lossy
                {
                    return;
                }
                let quality = (i16::from(self.encoding.webp.quality) + delta).clamp(1, 100) as u8;
                if self.encoding.webp.quality == quality {
                    return;
                }
                self.encoding.webp.quality = quality;
            }
            Control::Size(size) => {
                window.focus(&self.focus);
                let changed = self.size.resize() != size.resize();
                self.size = size;
                self.sync_size_fields(cx);
                if !changed {
                    cx.notify();
                    return;
                }
            }
            Control::ApplySize => {
                self.apply_size(window, cx);
                return;
            }
            Control::Rotate => self.adjustments = self.adjustments.rotated(1),
            Control::FlipX => self.adjustments.flip_x = !self.adjustments.flip_x,
            Control::FlipY => self.adjustments.flip_y = !self.adjustments.flip_y,
            Control::Grayscale => self.adjustments.grayscale = !self.adjustments.grayscale,
            Control::Invert => self.adjustments.invert = !self.adjustments.invert,
            Control::Brightness(delta) => {
                let brightness = (self.adjustments.brightness + delta).clamp(-100, 100);
                if self.adjustments.brightness == brightness {
                    return;
                }
                self.adjustments.brightness = brightness;
            }
            Control::Contrast(delta) => {
                let contrast = (self.colors.contrast + delta).clamp(-100, 100);
                if self.colors.contrast == contrast {
                    return;
                }
                self.colors.contrast = contrast;
            }
            Control::Saturation(delta) => {
                let saturation = (self.colors.saturation + delta).clamp(-100, 100);
                if self.colors.saturation == saturation {
                    return;
                }
                self.colors.saturation = saturation;
            }
            Control::Sharpen(amount) => {
                if self.sharpening.amount_percent == amount {
                    return;
                }
                self.sharpening.amount_percent = amount;
            }
            Control::ResetColors => {
                if self.colors == ColorAdjustments::default() {
                    return;
                }
                self.colors = ColorAdjustments::default();
            }
            Control::ToggleWatermark => {
                window.focus(&self.focus);
                self.watermark_enabled = !self.watermark_enabled;
                self.enable_parameter_fields(cx);
            }
            Control::WatermarkSize(size) => {
                let size = f32::from(size);
                if self.watermark.font_size == size {
                    return;
                }
                self.watermark.font_size = size;
            }
            Control::WatermarkOpacity(opacity) => {
                let opacity = f32::from(opacity) / 100.;
                if self.watermark.opacity == opacity {
                    return;
                }
                self.watermark.opacity = opacity;
            }
            Control::WatermarkColor(color) => {
                if self.watermark.color == color {
                    return;
                }
                self.watermark.color = color;
            }
            Control::Naming(mode) => {
                if self.naming_mode == mode {
                    return;
                }
                window.focus(&self.focus);
                self.naming_mode = mode;
                self.enable_parameter_fields(cx);
            }
            Control::DateFormat(index) => {
                let Some(format) = DATE_FORMATS.get(index) else {
                    return;
                };
                if self.date_format == *format {
                    return;
                }
                window.focus(&self.focus);
                self.date_format = *format;
            }
        }
        self.invalidate();
        cx.notify();
    }

    /// 最近截图点一下加到末尾，再点移出。
    fn toggle_capture(&mut self, path: PathBuf, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        if let Some(index) = self.paths.iter().position(|added| *added == path) {
            self.paths.remove(index);
        } else if self.paths.len() < BatchLimits::default().image.max_items {
            self.paths.push(path);
            self.page = page_count(self.paths.len()) - 1;
        } else {
            return;
        }
        self.invalidate();
        cx.notify();
    }

    /// 分段组里的一项；参数锁定时置灰。
    fn segment(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        active: bool,
        control: Control,
        c: Chrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let unlocked = !self.locked();
        let item = chrome::segment(id, label, active, unlocked, c);
        if unlocked {
            item.on_click(cx.listener(move |view, _, window, cx| view.control(control, window, cx)))
                .into_any_element()
        } else {
            item.into_any_element()
        }
    }

    /// 可多选的开关标签（镜像、灰度等）。
    fn toggle(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        active: bool,
        control: Control,
        c: Chrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let unlocked = !self.locked();
        let chip = widgets::chip(id, label, active, c.palette, c.surface);
        if unlocked {
            chip.on_click(cx.listener(move |view, _, window, cx| view.control(control, window, cx)))
                .into_any_element()
        } else {
            chip.opacity(0.45).into_any_element()
        }
    }

    /// 次要按钮；不可用时不挂接点击。
    fn action(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        kind: ButtonKind,
        enabled: bool,
        control: Control,
        c: Chrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = chrome::action(id, label, kind, enabled, c);
        if enabled {
            button
                .on_click(cx.listener(move |view, _, window, cx| view.control(control, window, cx)))
                .into_any_element()
        } else {
            button.into_any_element()
        }
    }

    /// “名称 − 数值 +” 的步进行，每步 10。
    #[allow(clippy::too_many_arguments)]
    fn stepper(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        down: Option<Control>,
        up: Option<Control>,
        c: Chrome,
        cx: &mut Context<Self>,
    ) -> Div {
        let unlocked = !self.locked();
        let mut row = chrome::option_row(label, c);
        for (suffix, glyph, control) in [("down", "−", down), ("up", "+", up)] {
            let enabled = unlocked && control.is_some();
            let button =
                div()
                    .id(SharedString::from(format!("{id}-{suffix}")))
                    .size(px(28.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(c.surface.line))
                    .bg(rgb(c.surface.raised))
                    .text_size(px(14.))
                    .when(!enabled, |button| button.opacity(0.4))
                    .when_some(control.filter(|_| enabled), |button, control| {
                        button
                            .cursor_pointer()
                            .hover(move |style| style.bg(rgb(c.palette.hover)))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.control(control, window, cx)
                            }))
                    })
                    .child(glyph);
            if suffix == "up" {
                row = row.child(
                    div()
                        .w(px(40.))
                        .text_center()
                        .text_size(px(13.))
                        .child(value.clone()),
                );
            }
            row = row.child(button);
        }
        row
    }

    fn strip_move(
        &mut self,
        from: usize,
        before: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.control(Control::Move(from, before), window, cx);
    }

    /// 弹层回图库挑图，独立窗口选文件。
    fn add_sources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.locked() && !host::request_pick(self.host, cx) {
            self.control(Control::Add, window, cx);
        }
    }

    fn row(&self, index: usize, c: Chrome, cx: &mut Context<Self>) -> AnyElement {
        let unlocked = !self.locked();
        let (name, source) = crate::tool_recent_captures::describe(&self.paths[index]);
        let (badge, color, detail, saved) = match self.rows.get(index).and_then(Option::as_ref) {
            Some(item) => match &item.outcome {
                BatchOutcome::Saved(saved) => (
                    tr!("已保存", "Saved"),
                    c.palette.accent,
                    saved.path.display().to_string(),
                    true,
                ),
                BatchOutcome::Failed(error) => (
                    tr!("失败", "Failed"),
                    c.palette.danger,
                    error.to_string(),
                    false,
                ),
                BatchOutcome::Cancelled => (
                    tr!("已取消", "Cancelled"),
                    c.palette.muted,
                    source.clone().unwrap_or_default(),
                    false,
                ),
            },
            None => (
                if self.active.is_some() {
                    tr!("等待", "Waiting")
                } else {
                    ""
                },
                c.palette.muted,
                source.clone().unwrap_or_default(),
                false,
            ),
        };
        let row = chrome::list_row_with(
            ("batch-source", index),
            index + 1,
            Some(strip::row_thumb(self.thumbs.get(&self.paths[index]), c)),
            name.clone(),
            Some(detail.into()),
            c,
        );
        strip::draggable(
            row,
            index,
            format!("{}. {name}", index + 1),
            unlocked,
            c,
            cx,
            Self::strip_move,
        )
        .when(!badge.is_empty(), |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(rgb(color))
                    .child(badge),
            )
        })
        .when(saved, |row| {
            row.child(chrome::row_icon(
                ("batch-copy", index),
                Icon::Copy,
                true,
                c,
                cx,
                move |view, window, cx| view.control(Control::Copy(index), window, cx),
            ))
        })
        .child(chrome::row_icon(
            ("batch-remove", index),
            Icon::Close,
            unlocked,
            c,
            cx,
            move |view, window, cx| view.control(Control::Remove(index), window, cx),
        ))
        .into_any_element()
    }
}

impl Drop for ImageBatchView {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageBatchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let c = chrome::chrome(cx);
        let palette = c.palette;
        let unlocked = !self.locked();
        let max_items = BatchLimits::default().image.max_items;

        // —— 来源 ——
        let can_add = unlocked && self.paths.len() < max_items;
        let sources = (self.host == ToolHost::Window).then(|| {
            chrome::field(tr!("最近截图", "Recent captures"), c)
                .child(
                    self.recent
                        .render(&self.paths, unlocked, c, cx, Self::toggle_capture),
                )
                .child(self.action(
                    "batch-add",
                    tr!("添加图片文件…", "Add image files…"),
                    ButtonKind::Secondary,
                    can_add,
                    Control::Add,
                    c,
                    cx,
                ))
        });

        // —— 格式 ——
        let mut formats = widgets::segmented(c.surface);
        for (id, format) in [
            ("batch-png", SaveFormat::Png),
            ("batch-jpeg", SaveFormat::Jpeg),
            ("batch-bmp", SaveFormat::Bmp),
            ("batch-webp", SaveFormat::Webp),
        ] {
            formats = formats.child(self.segment(
                id,
                format.label(),
                self.encoding.format == format,
                Control::Format(format),
                c,
                cx,
            ));
        }
        let mut format = chrome::field(tr!("输出格式", "Format"), c).child(formats);
        match self.encoding.format {
            SaveFormat::Png => {
                let mut compression = widgets::segmented(c.surface);
                for (id, label, value) in [
                    ("batch-png-fast", tr!("快速", "Fast"), PngCompression::Fast),
                    (
                        "batch-png-balanced",
                        tr!("均衡", "Balanced"),
                        PngCompression::Balanced,
                    ),
                    (
                        "batch-png-small",
                        tr!("最小", "Smallest"),
                        PngCompression::Small,
                    ),
                ] {
                    compression = compression.child(self.segment(
                        id,
                        label,
                        self.encoding.png_compression == value,
                        Control::Compression(value),
                        c,
                        cx,
                    ));
                }
                format =
                    format.child(chrome::option_row(tr!("压缩", "Compress"), c).child(compression));
            }
            SaveFormat::Jpeg => {
                let quality = self.encoding.jpeg_quality;
                format = format.child(self.stepper(
                    "batch-quality",
                    tr!("质量", "Quality"),
                    quality.to_string(),
                    (quality > 1).then_some(Control::Quality(-10)),
                    (quality < 100).then_some(Control::Quality(10)),
                    c,
                    cx,
                ));
            }
            SaveFormat::Webp => {
                let mut mode = widgets::segmented(c.surface);
                for (id, label, value) in [
                    (
                        "batch-webp-lossless",
                        tr!("无损", "Lossless"),
                        WebpMode::Lossless,
                    ),
                    ("batch-webp-lossy", tr!("有损", "Lossy"), WebpMode::Lossy),
                ] {
                    mode = mode.child(self.segment(
                        id,
                        label,
                        self.encoding.webp.mode == value,
                        Control::WebpMode(value),
                        c,
                        cx,
                    ));
                }
                format = format.child(chrome::option_row(tr!("模式", "Mode"), c).child(mode));
                if self.encoding.webp.mode == WebpMode::Lossy {
                    let quality = self.encoding.webp.quality;
                    format = format.child(self.stepper(
                        "batch-webp-quality",
                        tr!("质量", "Quality"),
                        quality.to_string(),
                        (quality > 1).then_some(Control::WebpQuality(-10)),
                        (quality < 100).then_some(Control::WebpQuality(10)),
                        c,
                        cx,
                    ));
                }
            }
            _ => {}
        }
        format = format.child(chrome::hint(
            tr!(
                "PNG 保留透明，JPEG、BMP 的透明处为白色；动图只处理首帧。",
                "PNG keeps transparency; JPEG and BMP use white. Animated images use the first frame."
            ),
            c,
        ));

        // —— 尺寸 ——
        let mut presets = widgets::segmented(c.surface);
        for (id, label, preset) in [
            (
                "batch-size-original",
                tr!("原尺寸", "Original"),
                SizePreset::Original,
            ),
            ("batch-size-1024", "1024×768", SizePreset::Fixed1024),
            ("batch-size-1920", "1920×1080", SizePreset::Fixed1920),
        ] {
            presets = presets.child(self.segment(
                id,
                label,
                self.size == preset,
                Control::Size(preset),
                c,
                cx,
            ));
        }
        let custom = matches!(self.size, SizePreset::Custom(..));
        let mut size = chrome::field(tr!("尺寸", "Size"), c).child(presets).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(84.)).child(self.width.clone()))
                .child(div().text_color(rgb(palette.muted)).child("×"))
                .child(div().w(px(84.)).child(self.height.clone()))
                .child(self.action(
                    "batch-apply-size",
                    if custom {
                        tr!("已应用", "Applied")
                    } else {
                        tr!("应用", "Apply")
                    },
                    if custom {
                        ButtonKind::Primary
                    } else {
                        ButtonKind::Secondary
                    },
                    unlocked,
                    Control::ApplySize,
                    c,
                    cx,
                )),
        );
        if self.size_dirty && custom {
            size = size.child(chrome::hint(
                tr!(
                    "改了宽高后按 Enter 或点“应用”才生效。",
                    "Press Enter or Apply to use the new dimensions."
                ),
                c,
            ));
        } else if self.size != SizePreset::Original {
            size = size.child(chrome::hint(
                tr!(
                    "按指定宽高缩放，可能改变比例。",
                    "Resizes to the exact dimensions; aspect ratio may change."
                ),
                c,
            ));
        }

        // —— 变换与调色 ——
        let transform = chrome::field(tr!("变换", "Transform"), c).child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(self.toggle(
                    "batch-rotate",
                    trf!(
                        "旋转 {}°",
                        "Rotate {}°",
                        u16::from(self.adjustments.turns) * 90
                    ),
                    self.adjustments.turns != 0,
                    Control::Rotate,
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "batch-flip",
                    tr!("水平镜像", "Flip H"),
                    self.adjustments.flip_x,
                    Control::FlipX,
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "batch-flip-y",
                    tr!("垂直镜像", "Flip V"),
                    self.adjustments.flip_y,
                    Control::FlipY,
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "batch-gray",
                    tr!("灰度", "Grayscale"),
                    self.adjustments.grayscale,
                    Control::Grayscale,
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "batch-invert",
                    tr!("反色", "Invert"),
                    self.adjustments.invert,
                    Control::Invert,
                    c,
                    cx,
                )),
        );
        let mut color = chrome::field(tr!("调色", "Color"), c);
        for (id, label, value, down, up) in [
            (
                "batch-brightness",
                tr!("亮度", "Brightness"),
                self.adjustments.brightness,
                Control::Brightness(-10),
                Control::Brightness(10),
            ),
            (
                "batch-contrast",
                tr!("对比度", "Contrast"),
                self.colors.contrast,
                Control::Contrast(-10),
                Control::Contrast(10),
            ),
            (
                "batch-saturation",
                tr!("饱和度", "Saturation"),
                self.colors.saturation,
                Control::Saturation(-10),
                Control::Saturation(10),
            ),
        ] {
            color = color.child(self.stepper(
                id,
                label,
                value.to_string(),
                (value > -100).then_some(down),
                (value < 100).then_some(up),
                c,
                cx,
            ));
        }
        let mut sharpen = widgets::segmented(c.surface);
        for (id, amount) in [
            ("batch-sharpen-off", 0_u16),
            ("batch-sharpen-25", 25),
            ("batch-sharpen-50", 50),
            ("batch-sharpen-100", 100),
        ] {
            sharpen = sharpen.child(self.segment(
                id,
                if amount == 0 {
                    SharedString::from(tr!("关", "Off"))
                } else {
                    format!("{amount}%").into()
                },
                self.sharpening.amount_percent == amount,
                Control::Sharpen(amount),
                c,
                cx,
            ));
        }
        color = color
            .child(chrome::option_row(tr!("锐化", "Sharpen"), c).child(sharpen))
            .when(self.colors != ColorAdjustments::default(), |color| {
                color.child(div().flex().child(self.action(
                    "batch-reset-colors",
                    tr!("复位对比度与饱和度", "Reset contrast and saturation"),
                    ButtonKind::Secondary,
                    unlocked,
                    Control::ResetColors,
                    c,
                    cx,
                )))
            });

        // —— 水印 ——
        let toggle_watermark = div()
            .id("batch-watermark-toggle")
            .flex()
            .items_center()
            .justify_between()
            .child(chrome::field_label(tr!("文字水印", "Text watermark"), c))
            .child(widgets::switch(self.watermark_enabled, palette, c.surface))
            .when(unlocked, |row| {
                row.cursor_pointer()
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.control(Control::ToggleWatermark, window, cx)
                    }))
            });
        let mut watermark = div().flex().flex_col().gap_2().child(toggle_watermark);
        if self.watermark_enabled {
            let mut sizes = widgets::segmented(c.surface);
            for font_size in [16_u16, 28, 48] {
                sizes = sizes.child(self.segment(
                    ("batch-watermark-size", usize::from(font_size)),
                    format!("{font_size}"),
                    self.watermark.font_size == f32::from(font_size),
                    Control::WatermarkSize(font_size),
                    c,
                    cx,
                ));
            }
            let mut opacities = widgets::segmented(c.surface);
            for opacity in [15_u8, 30, 50] {
                opacities = opacities.child(self.segment(
                    ("batch-watermark-opacity", usize::from(opacity)),
                    format!("{opacity}%"),
                    self.watermark.opacity == f32::from(opacity) / 100.,
                    Control::WatermarkOpacity(opacity),
                    c,
                    cx,
                ));
            }
            let mut colors = widgets::segmented(c.surface);
            for (index, (value, label)) in [
                (0xffffff, tr!("白", "White")),
                (0, tr!("黑", "Black")),
                (0x808080, tr!("灰", "Gray")),
            ]
            .into_iter()
            .enumerate()
            {
                colors = colors.child(self.segment(
                    ("batch-watermark-color", index),
                    label,
                    self.watermark.color == value,
                    Control::WatermarkColor(value),
                    c,
                    cx,
                ));
            }
            watermark = watermark
                .child(self.watermark_text.clone())
                .child(chrome::option_row(tr!("字号", "Size"), c).child(sizes))
                .child(chrome::option_row(tr!("不透明度", "Opacity"), c).child(opacities))
                .child(chrome::option_row(tr!("颜色", "Color"), c).child(colors))
                .child(chrome::hint(
                    tr!(
                        "单行文字按 30° 斜向平铺在处理后的图片上。",
                        "Single-line text tiles at 30° over the processed image."
                    ),
                    c,
                ));
        }

        // —— 输出 ——
        let mut naming = widgets::segmented(c.surface);
        for (id, label, mode) in [
            (
                "batch-name-source",
                tr!("原名 + 后缀", "Name + suffix"),
                NamingMode::SourceStem,
            ),
            (
                "batch-name-template",
                tr!("模板", "Template"),
                NamingMode::Template,
            ),
        ] {
            naming = naming.child(self.segment(
                id,
                label,
                self.naming_mode == mode,
                Control::Naming(mode),
                c,
                cx,
            ));
        }
        let directory = self
            .directory
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| tr!("尚未选择输出文件夹", "No output folder yet").into());
        let mut output = chrome::field(tr!("输出", "Output"), c)
            .child(
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
                            .text_color(rgb(if self.directory.is_some() {
                                palette.foreground
                            } else {
                                palette.muted
                            }))
                            .child(directory),
                    )
                    .child(self.action(
                        "batch-directory",
                        tr!("选择…", "Choose…"),
                        ButtonKind::Secondary,
                        unlocked,
                        Control::Directory,
                        c,
                        cx,
                    )),
            )
            .child(chrome::option_row(tr!("命名", "Naming"), c).child(naming));
        match self.naming_mode {
            NamingMode::SourceStem => {
                output = output.child(
                    chrome::option_row(tr!("后缀", "Suffix"), c)
                        .child(div().flex_1().min_w(px(0.)).child(self.suffix.clone())),
                );
            }
            NamingMode::Template => {
                let mut dates = widgets::segmented(c.surface);
                for (index, format) in DATE_FORMATS.iter().take(2).enumerate() {
                    dates = dates.child(self.segment(
                        ("batch-date", index),
                        *format,
                        self.date_format == *format,
                        Control::DateFormat(index),
                        c,
                        cx,
                    ));
                }
                output = output
                    .child(self.template.clone())
                    .child(chrome::option_row(tr!("日期", "Date"), c).child(dates))
                    .child(chrome::hint(
                        tr!(
                            "可用 {date} {time} {width} {height}，不用写扩展名。",
                            "Use {date} {time} {width} {height}; omit the extension."
                        ),
                        c,
                    ));
            }
        }
        output = output.child(chrome::hint(
            tr!(
                "按顺序追加编号（0001…），同名文件不会被覆盖。",
                "Numbers (0001…) follow list order; existing files are never overwritten."
            ),
            c,
        ));

        let sidebar = chrome::sidebar("batch-sidebar", c)
            .w(px(340.))
            .children(sources)
            .child(format)
            .child(size)
            .child(transform)
            .child(color)
            .child(watermark)
            .child(output);

        // —— 列表 ——
        let (saved, failed, cancelled) = result_counts(&self.rows);
        let completed = saved + failed + cancelled;
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(trf!(
                        "图片 · {} / {max_items}",
                        "Images · {} / {max_items}",
                        self.paths.len()
                    )),
            )
            .when(self.host == ToolHost::Popup, |header| {
                let add = chrome::action(
                    "batch-gallery-add",
                    tr!("从图库添加", "Add from gallery"),
                    ButtonKind::Secondary,
                    can_add,
                    c,
                );
                header.child(if can_add {
                    add.on_click(cx.listener(|view, _, window, cx| view.add_sources(window, cx)))
                } else {
                    add
                })
            })
            .child(self.action(
                "batch-clear",
                tr!("清空", "Clear"),
                ButtonKind::Secondary,
                unlocked && !self.paths.is_empty(),
                Control::Clear,
                c,
                cx,
            ));
        let list = if self.paths.is_empty() {
            chrome::stage(c)
                .flex_1()
                .min_h(px(0.))
                .child(chrome::empty_state(
                    Icon::Images,
                    if self.host == ToolHost::Popup {
                        tr!(
                            "点“从图库添加”挑选截图，统一转换格式、尺寸或加水印",
                            "Use Add from gallery to pick captures to convert, resize or watermark"
                        )
                    } else {
                        tr!(
                            "从左侧选择截图，统一转换格式、尺寸或加水印",
                            "Pick captures on the left to convert, resize or watermark them"
                        )
                    },
                    c,
                ))
                .into_any_element()
        } else {
            let start = self.page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(self.paths.len());
            let mut rows = div()
                .id("batch-items")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(c.surface.line))
                .bg(rgb(c.surface.card));
            let visible: Vec<PathBuf> = self.paths[start..end].to_vec();
            Thumbs::request(self, &visible, cx);
            for index in start..end {
                rows = rows.child(self.row(index, c, cx));
            }
            rows.child(strip::drop_end(
                div()
                    .id("batch-drop-end")
                    .h(px(16.))
                    .rounded_sm()
                    .border_1()
                    .border_color(gpui::transparent_black()),
                self.paths.len(),
                unlocked,
                c,
                cx,
                Self::strip_move,
            ))
            .into_any_element()
        };
        let pages = page_count(self.paths.len());
        let mut main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(header)
            .child(list);
        if self.frozen.is_some() {
            main = main.child(chrome::hint(
                tr!(
                    "重试只处理未成功的项，保留已保存的文件和编号；改动来源或参数会开始新一批。",
                    "Retry only reruns unsaved items and keeps their numbers; changing sources or options starts a new batch."
                ),
                c,
            ));
        }
        if pages > 1 {
            main = main.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .flex_shrink_0()
                    .child(chrome::row_icon(
                        "batch-page-prev",
                        Icon::ChevronUp,
                        self.page > 0,
                        c,
                        cx,
                        |view, window, cx| {
                            view.control(Control::Page(view.page.saturating_sub(1)), window, cx)
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(palette.muted))
                            .child(format!("{} / {pages}", self.page + 1)),
                    )
                    .child(chrome::row_icon(
                        "batch-page-next",
                        Icon::Chevron,
                        self.page + 1 < pages,
                        c,
                        cx,
                        |view, window, cx| view.control(Control::Page(view.page + 1), window, cx),
                    )),
            );
        }

        // —— 操作栏 ——
        let (status, tone): (String, Tone) = if self.active.is_some() {
            (
                trf!(
                    "正在处理 {completed}/{} · 成功 {saved} · 失败 {failed}",
                    "Processing {completed}/{} · saved {saved} · failed {failed}",
                    self.paths.len()
                ),
                Tone::Busy,
            )
        } else if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else if completed > 0 {
            (
                trf!(
                    "成功 {saved} · 失败 {failed} · 取消 {cancelled}",
                    "Saved {saved} · failed {failed} · cancelled {cancelled}"
                ),
                if failed > 0 { Tone::Failed } else { Tone::Done },
            )
        } else if self.paths.is_empty() {
            (
                tr!("选择截图或添加图片文件", "Pick captures or add image files").into(),
                Tone::Idle,
            )
        } else if self.directory.is_none() {
            (
                trf!(
                    "{} 张待处理，选择输出文件夹后开始",
                    "{} images; choose an output folder to start",
                    self.paths.len()
                ),
                Tone::Idle,
            )
        } else {
            (
                trf!("{} 张待处理", "{} images ready", self.paths.len()),
                Tone::Idle,
            )
        };
        let mut actions = Vec::new();
        if saved > 0 {
            actions.push(self.action(
                "batch-copy-all",
                tr!("复制成功路径", "Copy saved paths"),
                ButtonKind::Secondary,
                true,
                Control::CopyAll,
                c,
                cx,
            ));
        }
        if self.active.is_some() {
            actions.push(self.action(
                "batch-cancel",
                tr!("取消剩余项", "Cancel remaining"),
                ButtonKind::Secondary,
                true,
                Control::Cancel,
                c,
                cx,
            ));
        }
        // 还没有输出文件夹时，主按钮先引导选择文件夹。
        actions.push(if self.directory.is_none() {
            self.action(
                "batch-start",
                tr!("选择输出文件夹…", "Choose output folder…"),
                ButtonKind::Primary,
                unlocked && !self.paths.is_empty(),
                Control::Directory,
                c,
                cx,
            )
        } else {
            self.action(
                "batch-start",
                if self.frozen.is_some() {
                    tr!("重试未成功项", "Retry unsaved")
                } else {
                    tr!("开始处理", "Start")
                },
                ButtonKind::Primary,
                unlocked && !self.paths.is_empty() && saved < self.paths.len(),
                Control::Start,
                c,
                cx,
            )
        });

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.parameters_composing(cx) {
                    return;
                }
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held && view.prompt.is_none() {
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

fn parse_dimensions(width: &str, height: &str) -> std::result::Result<(u32, u32), ToolError> {
    let dimensions = match (width.trim().parse::<u32>(), height.trim().parse::<u32>()) {
        (Ok(width), Ok(height)) if width != 0 && height != 0 => (width, height),
        _ => {
            return Err(ToolError::Invalid(
                tr!(
                    "宽度和高度须为大于 0 的整数",
                    "Width and height must be positive integers"
                )
                .into(),
            ));
        }
    };
    validate_output(dimensions.0, dimensions.1, BatchLimits::default().image)?;
    Ok(dimensions)
}

fn page_count(count: usize) -> usize {
    count.div_ceil(PAGE_SIZE).max(1)
}

fn result_counts(rows: &[Option<BatchItemResult>]) -> (usize, usize, usize) {
    rows.iter()
        .flatten()
        .fold((0, 0, 0), |(saved, failed, cancelled), item| {
            match item.outcome {
                BatchOutcome::Saved(_) => (saved + 1, failed, cancelled),
                BatchOutcome::Failed(_) => (saved, failed + 1, cancelled),
                BatchOutcome::Cancelled => (saved, failed, cancelled + 1),
            }
        })
}

fn retry_indices(rows: &[Option<BatchItemResult>]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (!item
                .as_ref()
                .is_some_and(|item| matches!(item.outcome, BatchOutcome::Saved(_))))
            .then_some(index)
        })
        .collect()
}

fn merge_result(destination: &mut Option<BatchItemResult>, item: BatchItemResult) {
    // 成功行代表真实已提交文件；进度和完成回调都不能把它降为失败或取消。
    if !destination
        .as_ref()
        .is_some_and(|item| matches!(item.outcome, BatchOutcome::Saved(_)))
    {
        *destination = Some(item);
    }
}

fn merge_results(rows: &mut [Option<BatchItemResult>], items: Vec<BatchItemResult>) {
    for item in items {
        if let Some(destination) = rows.get_mut(item.index) {
            merge_result(destination, item);
        }
    }
}

fn saved_path_list(
    rows: &[Option<BatchItemResult>],
    max_bytes: usize,
) -> std::result::Result<String, ToolError> {
    let mut paths = String::new();
    for item in rows.iter().flatten() {
        if let BatchOutcome::Saved(saved) = &item.outcome {
            let path = saved.path.to_string_lossy();
            let separator = usize::from(!paths.is_empty());
            if paths
                .len()
                .checked_add(separator)
                .and_then(|length| length.checked_add(path.len()))
                .is_none_or(|length| length > max_bytes)
            {
                return Err(ToolError::Invalid(
                    tr!(
                        "成功路径过长，请逐项复制或分批处理",
                        "Saved paths are too long; copy individually or use smaller batches"
                    )
                    .into(),
                ));
            }
            if separator != 0 {
                paths.push('\n');
            }
            paths.push_str(&path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_tools::SavedImage;

    fn result(index: usize, outcome: BatchOutcome) -> BatchItemResult {
        BatchItemResult {
            index,
            source: PathBuf::from("source.png"),
            output: None,
            outcome,
        }
    }

    #[test]
    fn image_batch_view_dimensions_enforce_nonzero_integer_side_and_pixel_limits() {
        assert_eq!(parse_dimensions(" 1024 ", "768").unwrap(), (1024, 768));
        assert_eq!(parse_dimensions("8000", "4000").unwrap(), (8000, 4000));
        assert_eq!(parse_dimensions("65535", "1").unwrap(), (65535, 1));
        for (width, height) in [
            ("", "768"),
            ("1024", " "),
            ("0", "1"),
            ("1", "0"),
            ("-1", "1"),
            ("1.5", "1"),
            ("4294967296", "1"),
            ("1", "4294967296"),
            ("65536", "1"),
            ("8000", "4001"),
        ] {
            assert!(
                parse_dimensions(width, height).is_err(),
                "{width} × {height}"
            );
        }
    }

    #[test]
    fn image_batch_view_progress_is_bounded_and_drains_to_original_indices() {
        let mut inbox = ProgressInbox::new(3);
        inbox.publish(&result(2, BatchOutcome::Cancelled));
        inbox.publish(&result(0, BatchOutcome::Failed("bad image".into())));
        inbox.publish(&result(8, BatchOutcome::Cancelled));
        let mut rows = vec![None; 3];
        assert!(inbox.drain_into(&mut rows));
        assert_eq!(rows[0].as_ref().unwrap().index, 0);
        assert!(rows[1].is_none());
        assert_eq!(rows[2].as_ref().unwrap().index, 2);
        assert!(inbox.slots.iter().all(Option::is_none));
        assert!(!inbox.drain_into(&mut rows));
    }

    #[test]
    fn image_batch_view_summary_keeps_successes_with_failed_and_cancelled_items() {
        let rows = vec![
            Some(result(
                0,
                BatchOutcome::Saved(SavedImage {
                    path: PathBuf::from("output.png"),
                    width: 1,
                    height: 1,
                }),
            )),
            Some(result(1, BatchOutcome::Failed("bad image".into()))),
            Some(result(2, BatchOutcome::Cancelled)),
            None,
        ];
        assert_eq!(result_counts(&rows), (1, 1, 1));
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(PAGE_SIZE), 1);
        assert_eq!(page_count(PAGE_SIZE + 1), 2);
    }

    #[test]
    fn image_batch_view_copy_paths_uses_only_committed_outputs_with_a_byte_limit() {
        let mut saved = result(
            0,
            BatchOutcome::Saved(SavedImage {
                path: PathBuf::from("已保存.png"),
                width: 1,
                height: 1,
            }),
        );
        saved.output = Some(PathBuf::from("planned.png"));
        let mut failed = result(1, BatchOutcome::Failed("bad image".into()));
        failed.output = Some(PathBuf::from("never-saved.png"));
        let rows = vec![
            Some(saved),
            Some(failed),
            Some(result(2, BatchOutcome::Cancelled)),
        ];
        let expected = "已保存.png";
        assert_eq!(saved_path_list(&rows, expected.len()).unwrap(), expected);
        assert!(saved_path_list(&rows, expected.len() - 1).is_err());
    }

    #[test]
    fn image_batch_view_retry_merges_original_slots_without_losing_saved_paths() {
        let saved = |index, name: &str| {
            result(
                index,
                BatchOutcome::Saved(SavedImage {
                    path: PathBuf::from(name),
                    width: 1,
                    height: 1,
                }),
            )
        };
        let mut rows = vec![
            Some(saved(0, "a_0001.png")),
            Some(result(1, BatchOutcome::Failed("missing".into()))),
            Some(result(2, BatchOutcome::Cancelled)),
            None,
        ];
        let indices = retry_indices(&rows);
        assert_eq!(indices, [1, 2, 3]);
        for index in indices {
            rows[index] = None;
        }
        assert_eq!(saved_path_list(&rows, 1024).unwrap(), "a_0001.png");

        let mut progress = ProgressInbox::new(rows.len());
        progress.publish(&result(0, BatchOutcome::Failed("late failure".into())));
        progress.publish(&saved(2, "c_0003.png"));
        progress.publish(&result(3, BatchOutcome::Cancelled));
        assert!(progress.drain_into(&mut rows));
        merge_results(
            &mut rows,
            vec![
                saved(1, "b_0002.png"),
                result(2, BatchOutcome::Cancelled),
                result(3, BatchOutcome::Failed("still missing".into())),
                result(99, BatchOutcome::Cancelled),
            ],
        );
        assert_eq!(result_counts(&rows), (3, 1, 0));
        assert_eq!(retry_indices(&rows), [3]);
        assert_eq!(
            saved_path_list(&rows, 1024).unwrap(),
            "a_0001.png\nb_0002.png\nc_0003.png"
        );
    }
}
