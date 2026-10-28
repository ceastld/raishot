//! 内部单图工具窗；只接不可变源快照，处理和保存共用一个后台槽。
#[path = "image_tool_barcode_view.rs"]
pub(crate) mod barcode_view;
#[path = "image_batch_view.rs"]
pub(crate) mod batch_view;
#[path = "image_collage_view.rs"]
pub(crate) mod collage_view;
#[path = "image_compare_view.rs"]
pub(crate) mod compare_view;
#[path = "image_compress.rs"]
pub(crate) mod compress;
#[path = "image_tool_geometry.rs"]
pub(crate) mod geometry;
#[path = "image_tool_measure_view.rs"]
pub(crate) mod measure_view;
#[path = "image_pdf_view.rs"]
pub(crate) mod pdf_view;
#[path = "image_tool_qr_view.rs"]
pub(crate) mod qr_view;
#[path = "image_split_view.rs"]
pub(crate) mod split_view;

use crate::{
    capture::Image,
    image_export_metadata::{MetadataPolicy, SourceMetadata},
    image_resize::{resize, ResizeFilter},
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    image_transform::{
        color::ColorAdjustments, sharpen::SharpenOptions, ResizeOptions, TransformOptions,
        TransformPlan,
    },
    image_webp_encode::WebpMode,
    pin_model::Adjustments,
    save_options::{PngCompression, SaveEncoding, SaveFormat},
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
        Arc,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Preview,
    Save,
}

#[derive(Clone)]
struct JobTicket {
    request_id: u64,
    kind: JobKind,
    cancel: Arc<AtomicBool>,
}

/// 取消不会释放正在运行的槽；只有对应后台任务返回时才允许下一项进入。
struct WorkGate {
    request_id: u64,
    pending_preview: bool,
    active: Option<JobTicket>,
    prompt_pending: bool,
}

impl WorkGate {
    fn new() -> Self {
        Self {
            request_id: 1,
            pending_preview: true,
            active: None,
            prompt_pending: false,
        }
    }

    fn saving(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|job| job.kind == JobKind::Save)
    }

    fn change(&mut self) -> bool {
        if self.saving() {
            return false;
        }
        self.request_id = self.request_id.wrapping_add(1);
        self.pending_preview = true;
        if let Some(job) = &self.active {
            job.cancel.store(true, Ordering::Release);
        }
        true
    }

    fn launch(&mut self, kind: JobKind) -> Option<JobTicket> {
        if self.active.is_some()
            || (kind == JobKind::Save && (self.pending_preview || self.prompt_pending))
        {
            return None;
        }
        if kind == JobKind::Preview && !self.pending_preview {
            return None;
        }
        let ticket = JobTicket {
            request_id: self.request_id,
            kind,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if kind == JobKind::Preview {
            self.pending_preview = false;
        }
        self.active = Some(ticket.clone());
        Some(ticket)
    }

    /// None 表示不是当前槽；false 表示槽已归还，但结果已取消或被新请求取代。
    fn finish(&mut self, ticket: &JobTicket) -> Option<bool> {
        if !self
            .active
            .as_ref()
            .is_some_and(|job| Arc::ptr_eq(&job.cancel, &ticket.cancel))
        {
            return None;
        }
        self.active = None;
        Some(
            ticket.request_id == self.request_id
                && !ticket.cancel.load(Ordering::Acquire)
                && !self.pending_preview,
        )
    }

    fn cancel(&mut self) {
        if let Some(job) = &self.active {
            job.cancel.store(true, Ordering::Release);
        }
        if !self.saving() {
            self.request_id = self.request_id.wrapping_add(1);
            self.pending_preview = false;
        }
    }
}

struct PreviewResult {
    processed: Arc<ProcessedImage>,
    encoded: Arc<compress::EncodedImage>,
    image: Arc<RenderImage>,
}

const PREVIEW_WIDTH: u32 = 960;
const PREVIEW_HEIGHT: u32 = 600;
const PREVIEW_RESERVED_BYTES: u64 = PREVIEW_WIDTH as u64 * PREVIEW_HEIGHT as u64 * 4;

struct PreviewRequest {
    source: Arc<Image>,
    options: TransformOptions,
    encoding: SaveEncoding,
    metadata_policy: MetadataPolicy,
    metadata_source: Option<Arc<SourceMetadata>>,
    colors: ColorAdjustments,
    sharpening: SharpenOptions,
    request_id: u64,
    display_reserved_bytes: ToolResult<u64>,
}

impl PreviewRequest {
    fn prepare(self, cancel: &AtomicBool) -> ToolResult<PreviewResult> {
        check_cancel(cancel)?;
        let display_reserved_bytes = self.display_reserved_bytes?;
        let processed = TransformPlan::new(
            &self.source,
            self.options,
            ToolLimits::default(),
            self.request_id,
        )?
        .render_with_effects(&self.source, self.colors, self.sharpening, cancel)?;
        check_cancel(cancel)?;
        let encoded = compress::prepare_with_metadata(
            &processed,
            self.encoding,
            self.metadata_policy,
            self.metadata_source.as_deref(),
            cancel,
        )?;
        let image = if matches!(self.encoding.format, SaveFormat::Jpeg | SaveFormat::Webp)
            || self.metadata_policy != MetadataPolicy::Strip
        {
            let resident = host_resident_bytes(
                &[
                    self.source.bgra.capacity() as u64,
                    processed.image().bgra.capacity() as u64,
                ],
                self.metadata_source.as_deref(),
                display_reserved_bytes,
            )?;
            let decoded = encoded.decode_with_resident(resident, cancel)?;
            preview_image(&decoded, self.encoding, cancel)?
        } else {
            preview_image(processed.image(), self.encoding, cancel)?
        };
        check_cancel(cancel)?;
        Ok(PreviewResult {
            processed: Arc::new(processed),
            encoded: Arc::new(encoded),
            image,
        })
    }
}

fn host_resident_bytes(
    pixels: &[u64],
    metadata: Option<&SourceMetadata>,
    display_reserved_bytes: u64,
) -> ToolResult<u64> {
    pixels
        .iter()
        .copied()
        .chain([metadata.map_or(0, |source| source.retained_bytes() as u64)])
        .try_fold(display_reserved_bytes, |total, bytes| {
            total.checked_add(bytes)
        })
        .ok_or_else(|| {
            ToolError::Invalid(
                tr!(
                    "图片工具驻留预算溢出",
                    "Image-tool resident budget overflow"
                )
                .into(),
            )
        })
}

fn display_resident_bytes(slots: usize) -> ToolResult<u64> {
    u64::try_from(slots)
        .ok()
        .and_then(|slots| slots.checked_add(1))
        .and_then(|slots| slots.checked_mul(PREVIEW_RESERVED_BYTES))
        .ok_or_else(|| {
            ToolError::Invalid(
                tr!("图片预览驻留预算溢出", "Preview resident budget overflow").into(),
            )
        })
}

#[derive(Clone, Copy)]
enum Control {
    Rotate(i8),
    FlipX,
    FlipY,
    Grayscale,
    Invert,
    Brightness(i16),
    Contrast(i16),
    Saturation(i16),
    Sharpen(u16),
    Scale(u16),
    ApplySize,
    Format(SaveFormat),
    Compression(PngCompression),
    Quality(i16),
    WebpMode(WebpMode),
    WebpQuality(i16),
    Metadata(MetadataPolicy),
    Reset,
    Preview,
    Save,
    Cancel,
    Close,
    Compare,
    CompareFiles,
    Split,
    Batch,
    Collage,
    Pdf,
    PdfCurrent,
    Measure,
    Barcodes,
    Qr,
}

pub(crate) struct ImageToolView {
    source: Arc<Image>,
    protected_paths: Arc<Vec<PathBuf>>,
    metadata_source: Option<Arc<SourceMetadata>>,
    metadata_policy: MetadataPolicy,
    adjustments: Adjustments,
    colors: ColorAdjustments,
    sharpening: SharpenOptions,
    scale: u16,
    custom_size: Option<(u32, u32)>,
    width: Entity<TextField>,
    height: Entity<TextField>,
    size_dirty: bool,
    _size_subscriptions: Vec<Subscription>,
    encoding: SaveEncoding,
    gate: WorkGate,
    result: Option<Arc<ProcessedImage>>,
    encoded: Option<Arc<compress::EncodedImage>>,
    preview: Option<Arc<RenderImage>>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
}

/// 入口由集成人按实验资格接线；本模块不注册菜单、CLI 或读取原图文件。
pub(crate) fn open(
    image: Image,
    protected_paths: Vec<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<ImageToolView>> {
    open_shared(Arc::new(image), protected_paths, cx)
}

/// 来源元数据与像素一起移交，不通过保护路径重新读取原片。
pub(crate) fn open_with_metadata(
    image: Image,
    protected_paths: Vec<PathBuf>,
    metadata_source: Option<Arc<SourceMetadata>>,
    cx: &mut App,
) -> Result<WindowHandle<ImageToolView>> {
    open_shared_with_metadata(Arc::new(image), protected_paths, metadata_source, cx)
}

/// 其它内部工具可移交共享快照，不为再次打开工具复制整帧。
pub(crate) fn open_shared(
    image: Arc<Image>,
    protected_paths: Vec<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<ImageToolView>> {
    open_shared_with_metadata(image, protected_paths, None, cx)
}

pub(crate) fn open_shared_with_metadata(
    image: Arc<Image>,
    protected_paths: Vec<PathBuf>,
    metadata_source: Option<Arc<SourceMetadata>>,
    cx: &mut App,
) -> Result<WindowHandle<ImageToolView>> {
    validate_input(&image, ToolLimits::default())?;
    if protected_paths.len() > ToolLimits::default().max_items {
        return Err(tr!(
            "原片保护路径数量超过上限",
            "Too many protected source paths"
        )
        .into());
    }
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(960.), px(760.)), |display| {
            let bounds = display.bounds();
            size(
                px(960_f32.min((f32::from(bounds.size.width) - 48.).max(420.))),
                px(760_f32.min((f32::from(bounds.size.height) - 72.).max(400.))),
            )
        });
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(brand_title!("图片处理", "Image tools"));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                let width =
                    cx.new(|cx| TextField::new(image.width.to_string(), tr!("宽度", "Width"), cx));
                let height = cx
                    .new(|cx| TextField::new(image.height.to_string(), tr!("高度", "Height"), cx));
                let fields = [width.clone(), height.clone()];
                let focus_group: Vec<_> = fields.iter().map(|field| field.downgrade()).collect();
                let mut size_subscriptions = Vec::new();
                for field in &fields {
                    field.update(cx, |field, cx| {
                        field.configure_focus(focus_group.clone(), window, cx)
                    });
                    size_subscriptions.push(cx.subscribe_in(
                        field,
                        window,
                        |view: &mut ImageToolView, _, event, window, cx| match event {
                            TextFieldEvent::Submit => view.apply_size(window, cx),
                            TextFieldEvent::Changed if !view.closing && !view.gate.saving() => {
                                view.size_dirty = true;
                                cx.notify();
                            }
                            _ => {}
                        },
                    ));
                }
                ImageToolView {
                    source: image,
                    protected_paths: Arc::new(protected_paths),
                    metadata_source,
                    metadata_policy: MetadataPolicy::default(),
                    adjustments: Adjustments::default(),
                    colors: ColorAdjustments::default(),
                    sharpening: SharpenOptions::default(),
                    scale: 100,
                    custom_size: None,
                    width,
                    height,
                    size_dirty: false,
                    _size_subscriptions: size_subscriptions,
                    encoding: SaveEncoding::default(),
                    gate: WorkGate::new(),
                    result: None,
                    encoded: None,
                    preview: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
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
            view.update(cx, |view, cx| view.start_preview(window, cx));
            view
        },
    )?)
}

impl ImageToolView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        self.enable_size_fields(cx);
        self.gate.cancel();
        if self.gate.active.is_none() {
            return true;
        }
        self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
        self.failed = false;
        cx.notify();
        false
    }

    fn transform_options(&self) -> TransformOptions {
        let (width, height) = self
            .adjustments
            .dimensions(self.source.width, self.source.height);
        TransformOptions {
            adjustments: self.adjustments,
            resize: self
                .custom_size
                .or_else(|| {
                    (self.scale != 100).then(|| {
                        (
                            scaled_side(width, self.scale),
                            scaled_side(height, self.scale),
                        )
                    })
                })
                .map(|(width, height)| ResizeOptions {
                    width,
                    height,
                    filter: ResizeFilter::Bilinear,
                }),
        }
    }

    fn size_fields_composing(&self, cx: &App) -> bool {
        self.width.read(cx).is_composing() || self.height.read(cx).is_composing()
    }

    fn enable_size_fields(&self, cx: &mut Context<Self>) {
        let enabled = !self.closing && !self.gate.saving();
        for field in [&self.width, &self.height] {
            field.update(cx, |field, cx| field.set_enabled(enabled, cx));
        }
    }

    fn sync_size_inputs(&mut self, cx: &mut Context<Self>) {
        let options = self.transform_options();
        let (width, height) = options.resize.map_or_else(
            || {
                self.adjustments
                    .dimensions(self.source.width, self.source.height)
            },
            |resize| (resize.width, resize.height),
        );
        self.width
            .update(cx, |field, cx| field.set_text(width.to_string(), cx));
        self.height
            .update(cx, |field, cx| field.set_text(height.to_string(), cx));
        self.size_dirty = false;
    }

    fn apply_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.gate.saving() || self.size_fields_composing(cx) {
            return;
        }
        let width = self.width.read(cx).text().trim().parse::<u32>();
        let height = self.height.read(cx).text().trim().parse::<u32>();
        let candidate = match (width, height) {
            (Ok(width), Ok(height)) if width != 0 && height != 0 => (width, height),
            _ => {
                self.notice = tr!(
                    "宽度和高度须为大于 0 的整数；已应用结果保持不变",
                    "Width and height must be positive integers; the applied result is kept"
                )
                .into();
                self.failed = true;
                cx.notify();
                return;
            }
        };
        let options = TransformOptions {
            adjustments: self.adjustments,
            resize: Some(ResizeOptions {
                width: candidate.0,
                height: candidate.1,
                filter: ResizeFilter::Bilinear,
            }),
        };
        if let Err(error) = TransformPlan::new(
            &self.source,
            options,
            ToolLimits::default(),
            self.gate.request_id,
        ) {
            self.notice = error.to_string();
            self.failed = true;
            cx.notify();
            return;
        }
        window.focus(&self.focus);
        self.custom_size = Some(candidate);
        self.sync_size_inputs(cx);
        self.changed(window, cx);
    }

    fn clear_result(&mut self) {
        self.result = None;
        self.encoded = None;
        if let Some(image) = self.preview.take() {
            self.retiring.push(image);
        }
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || !self.gate.change() {
            return;
        }
        self.clear_result();
        self.notice.clear();
        self.failed = false;
        self.start_preview(window, cx);
        cx.notify();
    }

    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let Some(ticket) = self.gate.launch(JobKind::Preview) else {
            return;
        };
        let request = PreviewRequest {
            source: self.source.clone(),
            options: self.transform_options(),
            encoding: self.encoding,
            metadata_policy: self.metadata_policy,
            metadata_source: self.metadata_source.clone(),
            colors: self.colors,
            sharpening: self.sharpening,
            request_id: ticket.request_id,
            display_reserved_bytes: self.display_reservation(),
        };
        let work = ticket.clone();
        let task = cx
            .background_executor()
            .spawn(async move { request.prepare(&work.cancel) });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                let Some(current) = view.gate.finish(&ticket) else {
                    return;
                };
                if view.closing {
                    drop(result);
                    window.remove_window();
                    return;
                }
                if !current {
                    // 先丢弃旧的大帧，再启动合并后的最新请求。
                    drop(result);
                    view.start_preview(window, cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(result) => {
                        view.clear_result();
                        view.result = Some(result.processed);
                        view.encoded = Some(result.encoded);
                        view.preview = Some(result.image);
                        view.notice.clear();
                        view.failed = false;
                    }
                    Err(error) => {
                        view.clear_result();
                        view.failed = error != ToolError::Cancelled;
                        view.notice = error.to_string();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn display_reservation(&self) -> ToolResult<u64> {
        // RenderImage不公开底层Vec容量；按既有预览尺寸上限逐槽保守预留，并计入下一张预览。
        let slots = self
            .retiring
            .len()
            .checked_add(self.retired.len())
            .and_then(|slots| slots.checked_add(usize::from(self.preview.is_some())))
            .ok_or_else(|| {
                ToolError::Invalid(tr!("图片预览数量溢出", "Preview image count overflow").into())
            })?;
        display_resident_bytes(slots)
    }

    fn compare_host_reservation(&self) -> ToolResult<u64> {
        // 只保守记录打开时已知的父工具驻留；共享before及本次编码由对照解码另计。
        // 不跟踪父工具未来启动的其它任务，也不承诺全进程或codec私有堆峰值闭合。
        host_resident_bytes(
            &[self
                .result
                .as_ref()
                .map_or(0, |image| image.image().bgra.capacity() as u64)],
            self.metadata_source.as_deref(),
            self.display_reservation()?,
        )
    }

    fn save_ready(&self) -> bool {
        !self.closing
            && self.gate.active.is_none()
            && !self.gate.pending_preview
            && !self.gate.prompt_pending
            && self
                .result
                .as_ref()
                .is_some_and(|image| image.request_id() == self.gate.request_id)
            && self.encoded.as_ref().is_some_and(|image| {
                image.request_id() == self.gate.request_id && image.encoding() == self.encoding
            })
    }

    fn prompt_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_ready() {
            return;
        }
        let request_id = self.gate.request_id;
        let encoding = self.encoding;
        let directory = self
            .protected_paths
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let stem = self
            .protected_paths
            .first()
            .and_then(|path| path.file_stem())
            .and_then(|stem| stem.to_str())
            .unwrap_or("Image");
        let name = crate::save_options::file_name(&format!("{stem}_processed"), encoding.format)
            .unwrap_or_else(|_| format!("Image_processed.{}", encoding.format.extension()));
        window.focus(&self.focus);
        self.gate.prompt_pending = true;
        self.notice.clear();
        self.failed = false;
        let receiver = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                view.gate.prompt_pending = false;
                if view.gate.request_id != request_id
                    || view.encoding != encoding
                    || !view.save_ready()
                {
                    view.notice = tr!(
                        "参数已变化，请等待当前预览后重新保存",
                        "Options changed; wait for the current preview and save again"
                    )
                    .into();
                    view.failed = false;
                    cx.notify();
                    return;
                }
                match chosen {
                    Ok(Ok(Some(mut path))) => {
                        if path.extension().is_none() {
                            path.set_extension(encoding.format.extension());
                        }
                        view.start_save(path, window, cx);
                    }
                    Ok(Ok(None)) | Err(_) => {
                        view.notice = tr!("已取消保存", "Save cancelled").into();
                        cx.notify();
                    }
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn start_save(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_ready() {
            return;
        }
        let Some(image) = self.encoded.clone() else {
            return;
        };
        let Some(ticket) = self.gate.launch(JobKind::Save) else {
            return;
        };
        window.focus(&self.focus);
        self.enable_size_fields(cx);
        let protected = self.protected_paths.clone();
        let work = ticket.clone();
        let task = cx
            .background_executor()
            .spawn(async move { image.save(work.request_id, &path, &protected, &work.cancel) });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if view.gate.finish(&ticket).is_none() {
                    return;
                }
                view.enable_size_fields(cx);
                // 提交成功是终态；迟到的取消不能把已写出的文件改报为取消。
                match result {
                    Ok(saved) => {
                        view.notice = trf!("已保存：{}", "Saved: {}", saved.path.display());
                        view.failed = false;
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
        self.notice.clear();
        self.failed = false;
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
                let saving = self.gate.saving();
                self.gate.cancel();
                if !saving {
                    self.clear_result();
                }
                self.notice = tr!("已请求取消", "Cancellation requested").into();
                self.failed = false;
                cx.notify();
                return;
            }
            Control::Save => {
                self.prompt_save(window, cx);
                return;
            }
            Control::ApplySize => {
                self.apply_size(window, cx);
                return;
            }
            Control::Compare
            | Control::CompareFiles
            | Control::Split
            | Control::Batch
            | Control::Collage
            | Control::Pdf
            | Control::PdfCurrent
            | Control::Measure
            | Control::Barcodes
            | Control::Qr => {
                let ready = match action {
                    Control::CompareFiles
                    | Control::Batch
                    | Control::Collage
                    | Control::Pdf
                    | Control::Qr => !self.closing && !self.gate.saving(),
                    _ => self.save_ready(),
                };
                if !ready {
                    return;
                }
                let result = match action {
                    Control::CompareFiles => compare_view::open_files(Vec::new(), cx).map(|_| ()),
                    Control::Compare => (|| -> Result<()> {
                        let resident = self.compare_host_reservation()?;
                        compare_view::open_encoded_with_resident(
                            self.source.clone(),
                            self.encoded.as_ref().unwrap().clone(),
                            resident,
                            cx,
                        )?;
                        Ok(())
                    })(),
                    Control::Split => split_view::open_processed(
                        self.result.as_ref().unwrap().clone(),
                        self.protected_paths.clone(),
                        cx,
                    )
                    .map(|_| ()),
                    Control::Batch => {
                        batch_view::open(self.protected_paths.as_ref().clone(), cx).map(|_| ())
                    }
                    Control::Collage => {
                        collage_view::open(self.protected_paths.as_ref().clone(), cx).map(|_| ())
                    }
                    Control::Pdf => {
                        pdf_view::open(self.protected_paths.as_ref().clone(), cx).map(|_| ())
                    }
                    Control::PdfCurrent => pdf_view::open_processed(
                        self.result.as_ref().unwrap().clone(),
                        self.protected_paths.clone(),
                        cx,
                    )
                    .map(|_| ()),
                    Control::Measure => {
                        measure_view::open_processed(self.result.as_ref().unwrap().clone(), cx)
                            .map(|_| ())
                    }
                    Control::Barcodes => {
                        barcode_view::open_processed(self.result.as_ref().unwrap().clone(), cx)
                            .map(|_| ())
                    }
                    Control::Qr => qr_view::open(cx).map(|_| ()),
                    _ => unreachable!(),
                };
                if let Err(error) = result {
                    self.notice = error.to_string();
                    self.failed = true;
                    cx.notify();
                }
                return;
            }
            _ if self.closing || self.gate.saving() => return,
            Control::Rotate(delta) => {
                self.adjustments = self.adjustments.rotated(delta);
                if !self.size_dirty {
                    self.sync_size_inputs(cx);
                }
            }
            Control::FlipX => self.adjustments.flip_x = !self.adjustments.flip_x,
            Control::FlipY => self.adjustments.flip_y = !self.adjustments.flip_y,
            Control::Grayscale => self.adjustments.grayscale = !self.adjustments.grayscale,
            Control::Invert => self.adjustments.invert = !self.adjustments.invert,
            Control::Brightness(delta) => {
                self.adjustments.brightness = (self.adjustments.brightness + delta).clamp(-100, 100)
            }
            Control::Contrast(delta) => {
                self.colors.contrast = (self.colors.contrast + delta).clamp(-100, 100)
            }
            Control::Saturation(delta) => {
                self.colors.saturation = (self.colors.saturation + delta).clamp(-100, 100)
            }
            Control::Sharpen(amount) => self.sharpening.amount_percent = amount,
            Control::Scale(scale) => {
                window.focus(&self.focus);
                self.scale = scale;
                self.custom_size = None;
                self.sync_size_inputs(cx);
            }
            Control::Format(format) => self.encoding.format = format,
            Control::Compression(compression) => self.encoding.png_compression = compression,
            Control::Quality(delta) => {
                if self.encoding.format != SaveFormat::Jpeg {
                    return;
                }
                self.encoding.jpeg_quality =
                    (i16::from(self.encoding.jpeg_quality) + delta).clamp(1, 100) as u8;
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
            Control::Metadata(policy) => {
                if self.metadata_policy == policy {
                    return;
                }
                if policy != MetadataPolicy::Strip
                    && (self.metadata_source.is_none() || self.encoding.format == SaveFormat::Bmp)
                {
                    return;
                }
                if policy == MetadataPolicy::ColorAndSupportedExif
                    && !self
                        .metadata_source
                        .as_ref()
                        .is_some_and(|source| source.exif_summary().is_some())
                {
                    return;
                }
                self.metadata_policy = policy;
            }
            Control::Reset => {
                window.focus(&self.focus);
                self.adjustments = Adjustments::default();
                self.colors = ColorAdjustments::default();
                self.sharpening = SharpenOptions::default();
                self.scale = 100;
                self.custom_size = None;
                self.sync_size_inputs(cx);
            }
            Control::Preview => {}
        }
        self.changed(window, cx);
    }

    fn button(
        &self,
        id: &'static str,
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

    fn status(&self) -> &'static str {
        if self.gate.saving() {
            tr!("正在保存，处理参数暂时锁定…", "Saving; options are locked…")
        } else if self.gate.prompt_pending {
            tr!("正在选择保存位置…", "Choosing a save location…")
        } else if self.gate.active.is_some() || self.gate.pending_preview {
            tr!("正在更新预览…", "Updating preview…")
        } else if self.result.is_some() {
            tr!("预览已就绪", "Preview ready")
        } else {
            tr!("暂无可保存结果", "No result to save")
        }
    }
}

impl Drop for ImageToolView {
    fn drop(&mut self) {
        if let Some(job) = &self.gate.active {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageToolView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        window.set_window_title(brand_title!("图片处理", "Image tools"));
        let palette = crate::theme::palette(cx);
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let enabled = !self.closing && !self.gate.saving();
        let preview_height = (f32::from(window.viewport_size().height) * 0.43).clamp(100., 320.);
        let mut preview = div()
            .w_full()
            .h(px(preview_height))
            .flex_shrink_0()
            .overflow_hidden()
            .rounded_md()
            .bg(rgb(palette.track))
            .flex()
            .items_center()
            .justify_center();
        preview = match &self.preview {
            Some(image) => preview.child(
                img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => preview.child(div().text_color(rgb(palette.muted)).child(self.status())),
        };
        let geometry = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(self.button(
                "tool-rotate-left",
                tr!("左转90°", "Rotate left"),
                false,
                enabled,
                Control::Rotate(-1),
                cx,
            ))
            .child(self.button(
                "tool-rotate-right",
                tr!("右转90°", "Rotate right"),
                false,
                enabled,
                Control::Rotate(1),
                cx,
            ))
            .child(self.button(
                "tool-flip-x",
                tr!("水平镜像", "Flip X"),
                self.adjustments.flip_x,
                enabled,
                Control::FlipX,
                cx,
            ))
            .child(self.button(
                "tool-flip-y",
                tr!("垂直镜像", "Flip Y"),
                self.adjustments.flip_y,
                enabled,
                Control::FlipY,
                cx,
            ))
            .child(self.button(
                "tool-gray",
                tr!("灰度", "Grayscale"),
                self.adjustments.grayscale,
                enabled,
                Control::Grayscale,
                cx,
            ))
            .child(self.button(
                "tool-invert",
                tr!("反色", "Invert"),
                self.adjustments.invert,
                enabled,
                Control::Invert,
                cx,
            ));
        let tone = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(div().text_color(rgb(palette.muted)).child(trf!(
                "亮度：{:+}",
                "Brightness: {:+}",
                self.adjustments.brightness
            )))
            .child(self.button(
                "tool-darker",
                "−10",
                false,
                enabled && self.adjustments.brightness > -100,
                Control::Brightness(-10),
                cx,
            ))
            .child(self.button(
                "tool-brighter",
                "+10",
                false,
                enabled && self.adjustments.brightness < 100,
                Control::Brightness(10),
                cx,
            ))
            .child(self.button(
                "tool-reset",
                tr!("还原处理", "Reset"),
                false,
                enabled,
                Control::Reset,
                cx,
            ));
        let color_tone = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(div().text_color(rgb(palette.muted)).child(trf!(
                "对比度：{:+}",
                "Contrast: {:+}",
                self.colors.contrast
            )))
            .child(self.button(
                "tool-contrast-down",
                "−10",
                false,
                enabled && self.colors.contrast > -100,
                Control::Contrast(-10),
                cx,
            ))
            .child(self.button(
                "tool-contrast-up",
                "+10",
                false,
                enabled && self.colors.contrast < 100,
                Control::Contrast(10),
                cx,
            ))
            .child(div().text_color(rgb(palette.muted)).child(trf!(
                "饱和度：{:+}",
                "Saturation: {:+}",
                self.colors.saturation
            )))
            .child(self.button(
                "tool-saturation-down",
                "−10",
                false,
                enabled && self.colors.saturation > -100,
                Control::Saturation(-10),
                cx,
            ))
            .child(self.button(
                "tool-saturation-up",
                "+10",
                false,
                enabled && self.colors.saturation < 100,
                Control::Saturation(10),
                cx,
            ));
        let mut sharpening = div().flex().flex_wrap().gap_2().items_center().child(
            div()
                .text_color(rgb(palette.muted))
                .child(tr!("锐化", "Sharpen")),
        );
        for (id, amount) in [
            ("tool-sharpen-off", 0_u16),
            ("tool-sharpen-25", 25),
            ("tool-sharpen-50", 50),
            ("tool-sharpen-100", 100),
        ] {
            let label = if amount == 0 {
                tr!("关闭", "Off").to_owned()
            } else {
                format!("{amount}%")
            };
            sharpening = sharpening.child(self.button(
                id,
                label,
                self.sharpening.amount_percent == amount,
                enabled,
                Control::Sharpen(amount),
                cx,
            ));
        }
        let mut scaling = div().flex().flex_wrap().gap_2().items_center().child(
            div()
                .text_color(rgb(palette.muted))
                .child(tr!("尺寸", "Size")),
        );
        for (id, percent) in [
            ("tool-scale-50", 50u16),
            ("tool-scale-100", 100),
            ("tool-scale-200", 200),
        ] {
            scaling = scaling.child(self.button(
                id,
                format!("{percent}%"),
                self.custom_size.is_none() && self.scale == percent,
                enabled,
                Control::Scale(percent),
                cx,
            ));
        }
        let custom_inputs = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_color(rgb(palette.muted))
                    .child(tr!("宽", "Width")),
            )
            .child(div().w(px(96.)).child(self.width.clone()))
            .child(
                div()
                    .text_color(rgb(palette.muted))
                    .child(tr!("高", "Height")),
            )
            .child(div().w(px(96.)).child(self.height.clone()))
            .child(self.button(
                "tool-apply-size",
                tr!("应用宽高", "Apply dimensions"),
                self.custom_size.is_some(),
                enabled,
                Control::ApplySize,
                cx,
            ));
        let applied_size = match self.custom_size {
            Some((width, height)) => trf!(
                "已应用参数：{width} × {height} 像素",
                "Applied dimensions: {width} × {height} pixels"
            ),
            None => trf!(
                "已应用尺寸预设：{}%（保持比例）",
                "Applied size preset: {}% (keep aspect ratio)",
                self.scale
            ),
        };
        let mut formats = div().flex().flex_wrap().gap_2().items_center();
        for (id, format) in [
            ("tool-png", SaveFormat::Png),
            ("tool-jpeg", SaveFormat::Jpeg),
            ("tool-bmp", SaveFormat::Bmp),
            ("tool-webp", SaveFormat::Webp),
        ] {
            formats = formats.child(self.button(
                id,
                format.label(),
                self.encoding.format == format,
                enabled,
                Control::Format(format),
                cx,
            ));
        }
        if self.encoding.format == SaveFormat::Jpeg {
            formats = formats
                .child(div().text_color(rgb(palette.muted)).child(trf!(
                    "JPEG 质量：{}",
                    "JPEG quality: {}",
                    self.encoding.jpeg_quality
                )))
                .child(self.button(
                    "tool-quality-down",
                    "−10",
                    false,
                    enabled && self.encoding.jpeg_quality > 1,
                    Control::Quality(-10),
                    cx,
                ))
                .child(self.button(
                    "tool-quality-up",
                    "+10",
                    false,
                    enabled && self.encoding.jpeg_quality < 100,
                    Control::Quality(10),
                    cx,
                ));
        }
        if self.encoding.format == SaveFormat::Webp {
            for (id, label, mode) in [
                (
                    "tool-webp-lossless",
                    tr!("无损", "Lossless"),
                    WebpMode::Lossless,
                ),
                ("tool-webp-lossy", tr!("有损", "Lossy"), WebpMode::Lossy),
            ] {
                formats = formats.child(self.button(
                    id,
                    label,
                    self.encoding.webp.mode == mode,
                    enabled,
                    Control::WebpMode(mode),
                    cx,
                ));
            }
            if self.encoding.webp.mode == WebpMode::Lossy {
                formats = formats
                    .child(div().text_color(rgb(palette.muted)).child(trf!(
                        "WebP 质量：{}",
                        "WebP quality: {}",
                        self.encoding.webp.quality
                    )))
                    .child(self.button(
                        "tool-webp-quality-down",
                        "−10",
                        false,
                        enabled && self.encoding.webp.quality > 1,
                        Control::WebpQuality(-10),
                        cx,
                    ))
                    .child(self.button(
                        "tool-webp-quality-up",
                        "+10",
                        false,
                        enabled && self.encoding.webp.quality < 100,
                        Control::WebpQuality(10),
                        cx,
                    ));
            }
        }
        if self.encoding.format == SaveFormat::Png {
            for (id, label, compression) in [
                (
                    "tool-png-fast",
                    tr!("PNG 快速", "PNG fast"),
                    PngCompression::Fast,
                ),
                (
                    "tool-png-balanced",
                    tr!("PNG 均衡", "PNG balanced"),
                    PngCompression::Balanced,
                ),
                (
                    "tool-png-small",
                    tr!("PNG 最小", "PNG small"),
                    PngCompression::Small,
                ),
            ] {
                formats = formats.child(self.button(
                    id,
                    label,
                    self.encoding.png_compression == compression,
                    enabled,
                    Control::Compression(compression),
                    cx,
                ));
            }
        }
        let exif_summary = self
            .metadata_source
            .as_ref()
            .and_then(|source| source.exif_summary());
        let metadata = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(self.button(
                "tool-metadata-strip",
                tr!("去除元数据", "Strip metadata"),
                self.metadata_policy == MetadataPolicy::Strip,
                enabled,
                Control::Metadata(MetadataPolicy::Strip),
                cx,
            ))
            .child(self.button(
                "tool-metadata-color",
                tr!("保留颜色配置（不含 EXIF）", "Keep color profile (no EXIF)"),
                self.metadata_policy == MetadataPolicy::ColorOnly,
                enabled
                    && self.metadata_source.is_some()
                    && self.encoding.format != SaveFormat::Bmp,
                Control::Metadata(MetadataPolicy::ColorOnly),
                cx,
            ))
            .child(self.button(
                "tool-metadata-exif",
                tr!("保留颜色及已支持 EXIF", "Keep color and supported EXIF"),
                self.metadata_policy == MetadataPolicy::ColorAndSupportedExif,
                enabled && exif_summary.is_some() && self.encoding.format != SaveFormat::Bmp,
                Control::Metadata(MetadataPolicy::ColorAndSupportedExif),
                cx,
            ));
        let metadata_hint = if self.metadata_source.is_none() {
            tr!(
                "当前来源没有可验证的颜色配置，仅可去除元数据。",
                "This source has no verified color metadata; stripping is available."
            )
        } else if self.encoding.format == SaveFormat::Bmp {
            tr!(
                "BMP 不支持保留颜色配置，请选择去除元数据。",
                "BMP cannot keep a color profile; select Strip metadata."
            )
        } else {
            match self.metadata_policy {
                MetadataPolicy::Strip => tr!(
                    "去除颜色配置及 EXIF；保存当前处理像素。",
                    "Removes color profiles and EXIF; saves the processed pixels."
                ),
                MetadataPolicy::ColorOnly => tr!(
                    "仅保留适用于当前像素的颜色配置，不含 EXIF；预览与保存使用同一编码结果。",
                    "Keeps only a profile matching the pixels, without EXIF. Preview and save use the same encoding."
                ),
                MetadataPolicy::ColorAndSupportedExif => tr!(
                    "仅保留已支持字段；方向和尺寸按当前结果重建。GPS 标签、缩略图、厂商及未知字段不保留。",
                    "Keeps supported fields; rebuilds orientation and dimensions for the result. GPS tags, thumbnails, private and unknown fields are excluded."
                ),
            }
        };
        let exif_note = self.metadata_source.as_ref().map_or_else(String::new, |source| {
            if let Some(summary) = exif_summary {
                trf!(
                    "源 EXIF 已支持 {} 个字段；{} 个已扫描标签不保留。GPS 标签、缩略图、厂商及未知字段始终不保留。",
                    "Source EXIF: {} supported fields; {} scanned tags excluded. GPS tags, thumbnails, private and unknown fields are always excluded.",
                    summary.retained_fields,
                    summary.dropped_fields
                )
            } else if let Some(reason) = source.exif_error() {
                trf!("EXIF 保留不可用：{reason}", "EXIF preservation unavailable: {reason}")
            } else {
                tr!("此来源没有可保留的已支持 EXIF", "This source has no supported EXIF to keep").into()
            }
        });
        let dimensions = self
            .result
            .as_ref()
            .map(|result| {
                trf!(
                    "结果：{} × {} 像素 · 编码后 {} 字节",
                    "Result: {} × {} pixels · {} encoded bytes",
                    result.image().width,
                    result.image().height,
                    self.encoded
                        .as_ref()
                        .map_or(0, |encoded| encoded.byte_len()),
                )
            })
            .unwrap_or_else(|| self.status().into());
        let body = div()
            .id("image-tool-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .child(preview)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(geometry)
                    .child(tone)
                    .child(color_tone)
                    .child(sharpening)
                    .child(scaling)
                    .child(custom_inputs)
                    .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(applied_size))
                    .when(self.size_dirty, |body| body.child(div().text_size(px(12.)).text_color(rgb(palette.accent)).child(tr!(
                        "宽高输入尚未应用；预览和保存仍使用已应用参数。",
                        "Dimension edits are not applied; preview and save still use the applied options."
                    ))))
                    .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                        "指定最终宽高可能改变比例；旋转不交换已指定的宽高。比例预设保持原比例。",
                        "Exact dimensions may change the aspect ratio and stay fixed when rotating. Presets keep the aspect ratio."
                    )))
                    .child(formats)
                    .child(metadata)
                    .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(metadata_hint))
                    .when(!exif_note.is_empty(), |body| body.child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(exif_note)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(self.button(
                                "tool-compare-files",
                                tr!("双文件对照…", "Compare two files…"),
                                false,
                                enabled,
                                Control::CompareFiles,
                                cx,
                            ))
                            .child(self.button(
                                "tool-compare",
                                tr!("变换前后对比", "Compare transforms"),
                                false,
                                self.save_ready(),
                                Control::Compare,
                                cx,
                            ))
                            .child(self.button(
                                "tool-split",
                                tr!("分割当前结果…", "Split result…"),
                                false,
                                self.save_ready(),
                                Control::Split,
                                cx,
                            ))
                            .child(self.button(
                                "tool-batch",
                                tr!("批量文件…", "Batch files…"),
                                false,
                                enabled,
                                Control::Batch,
                                cx,
                            ))
                            .child(self.button(
                                "tool-collage",
                                tr!("拼接文件…", "Collage files…"),
                                false,
                                enabled,
                                Control::Collage,
                                cx,
                            ))
                            .child(self.button(
                                "tool-pdf-current",
                                tr!("当前结果转 PDF…", "Result to PDF…"),
                                false,
                                self.save_ready(),
                                Control::PdfCurrent,
                                cx,
                            ))
                            .child(self.button(
                                "tool-pdf",
                                tr!("图片文件转 PDF…", "Files to PDF…"),
                                false,
                                enabled,
                                Control::Pdf,
                                cx,
                            ))
                            .child(self.button(
                                "tool-measure",
                                tr!("像素取色与测量…", "Pixel color and measure…"),
                                false,
                                self.save_ready(),
                                Control::Measure,
                                cx,
                            ))
                            .child(self.button(
                                "tool-barcodes",
                                tr!("一维条码识别…", "Read barcodes…"),
                                false,
                                self.save_ready(),
                                Control::Barcodes,
                                cx,
                            ))
                            .child(self.button(
                                "tool-qr",
                                tr!("生成二维码…", "Generate QR…"),
                                false,
                                enabled,
                                Control::Qr,
                                cx,
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(if !matches!(self.encoding.format, SaveFormat::Jpeg | SaveFormat::Bmp) {
                        tr!(
                            "保留透明；尺寸预设以源图为准。保存复用显示体积的编码结果。",
                            "Transparency is preserved. Sizes use the source image. Saved bytes match the displayed size."
                        )
                    } else {
                        tr!(
                            "JPEG 预览来自本次实际编码；JPEG/BMP 透明部分合成白色。保存复用显示体积的编码结果。",
                            "JPEG previews use the actual encoding. JPEG/BMP use white for transparency. Saved bytes match the displayed size."
                        )
                    }),
            );
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
                if view.size_fields_composing(cx) {
                    return;
                }
                if event.is_held {
                    return;
                }
                if event.keystroke.key == "s" && event.keystroke.modifiers.control {
                    view.control(Control::Save, window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    let action = if view.gate.active.is_some() {
                        Control::Cancel
                    } else {
                        Control::Close
                    };
                    view.control(action, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .text_size(px(20.))
                    .child(tr!("图片处理", "Image tools")),
            )
            .child(div().text_color(rgb(palette.muted)).child(trf!(
                "源图：{} × {} 像素 · {dimensions}",
                "Source: {} × {} pixels · {dimensions}",
                self.source.width,
                self.source.height
            )))
            .child(body)
            .child(
                div()
                    .id("image-tool-notice")
                    .max_h(px(64.))
                    .overflow_y_scroll()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(rgb(if self.failed {
                        palette.danger
                    } else {
                        palette.muted
                    }))
                    .child(if self.notice.is_empty() {
                        self.status().to_owned()
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
                        "tool-preview",
                        tr!("更新预览", "Preview"),
                        false,
                        enabled,
                        Control::Preview,
                        cx,
                    ))
                    .child(self.button(
                        "tool-cancel",
                        tr!("取消处理", "Cancel task"),
                        false,
                        self.gate.active.is_some() || self.gate.pending_preview,
                        Control::Cancel,
                        cx,
                    ))
                    .child(self.button(
                        "tool-save",
                        tr!("另存为…", "Save as…"),
                        true,
                        self.save_ready(),
                        Control::Save,
                        cx,
                    ))
                    .child(self.button(
                        "tool-close",
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

fn scaled_side(side: u32, percent: u16) -> u32 {
    ((u64::from(side) * u64::from(percent) + 50) / 100).max(1) as u32
}

fn preview_image(
    image: &Image,
    encoding: SaveEncoding,
    cancel: &AtomicBool,
) -> ToolResult<Arc<RenderImage>> {
    let density = (f64::from(PREVIEW_WIDTH) / f64::from(image.width))
        .min(f64::from(PREVIEW_HEIGHT) / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * density).round().max(1.) as u32;
    let height = (f64::from(image.height) * density).round().max(1.) as u32;
    let mut preview = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        ToolLimits::default(),
        cancel,
    )?;
    if matches!(encoding.format, SaveFormat::Jpeg | SaveFormat::Bmp) {
        crate::export::flatten_on_white(&mut preview.bgra);
    }
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
    fn image_tool_view_host_preview_resident_reaches_decoder_and_precancel() {
        let source = Arc::new(Image {
            width: 2,
            height: 1,
            bgra: vec![10, 20, 30, 0, 40, 50, 60, 128],
            readback_ms: 0.,
        });
        let original = source.bgra.clone();
        let processed = ProcessedImage::new(
            Image {
                width: source.width,
                height: source.height,
                bgra: original.clone(),
                readback_ms: 0.,
            },
            109,
        );
        let cancel = AtomicBool::new(false);
        let encoded = crate::export::encode_image_cancellable(
            processed.image(),
            SaveEncoding::default(),
            &cancel,
        )
        .unwrap();
        let inspected = crate::image_container_metadata::inspect(&encoded).unwrap();
        let mut reason = String::with_capacity(1024);
        reason.push_str("unsupported EXIF fixture");
        let metadata = Arc::new(
            SourceMetadata::new(
                crate::image_color_management::ManagedMetadata {
                    source: inspected.summary,
                    profile: None,
                    color_conversion: crate::image_color_management::ColorConversion::AssumedSrgb,
                },
                None,
            )
            .unwrap()
            .with_exif(Err(reason)),
        );
        let request = |display_reserved_bytes| PreviewRequest {
            source: source.clone(),
            options: TransformOptions::default(),
            encoding: SaveEncoding::default(),
            metadata_policy: MetadataPolicy::ColorOnly,
            metadata_source: Some(metadata.clone()),
            colors: ColorAdjustments::default(),
            sharpening: SharpenOptions::default(),
            request_id: 109,
            display_reserved_bytes,
        };
        let display = display_resident_bytes(2).unwrap();
        let ready = request(Ok(display)).prepare(&cancel).unwrap();
        assert_eq!(ready.processed.image().bgra, original);
        assert_eq!(ready.image.as_bytes(0).unwrap(), original);
        assert_eq!(
            host_resident_bytes(
                &[
                    source.bgra.capacity() as u64,
                    ready.processed.image().bgra.capacity() as u64,
                ],
                Some(&metadata),
                display,
            )
            .unwrap(),
            source.bgra.capacity() as u64
                + ready.processed.image().bgra.capacity() as u64
                + metadata.retained_bytes() as u64
                + 3 * PREVIEW_RESERVED_BYTES,
        );
        // 进入真实后台预览函数；若回读恢复为decode(R=0)，这一项会错误成功。
        assert!(matches!(
            request(Ok(crate::image_decode::MAX_DECODE_BYTES)).prepare(&cancel),
            Err(ToolError::Failed(_))
        ));
        assert!(matches!(
            request(Ok(u64::MAX)).prepare(&cancel),
            Err(ToolError::Invalid(_))
        ));
        assert!(matches!(
            request(Ok(u64::MAX)).prepare(&AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert!(display_resident_bytes(usize::MAX).is_err());
        assert_eq!(source.bgra, original);
        assert_eq!(processed.image().bgra, original);
    }

    #[test]
    fn image_tool_view_coalesces_latest_request_until_old_task_returns() {
        let mut gate = WorkGate::new();
        let old = gate.launch(JobKind::Preview).unwrap();
        assert!(gate.change());
        assert!(gate.change());
        assert!(old.cancel.load(Ordering::Acquire));
        assert!(gate.launch(JobKind::Preview).is_none());
        assert_eq!(gate.finish(&old), Some(false));
        let latest = gate.launch(JobKind::Preview).unwrap();
        assert_eq!(latest.request_id, gate.request_id);
        assert!(!latest.cancel.load(Ordering::Acquire));
        assert_eq!(gate.finish(&old), None);
        assert_eq!(gate.finish(&latest), Some(true));
    }

    #[test]
    fn image_tool_view_save_locks_parameters_until_cancelled_job_returns() {
        let mut gate = WorkGate::new();
        let preview = gate.launch(JobKind::Preview).unwrap();
        assert_eq!(gate.finish(&preview), Some(true));
        gate.prompt_pending = true;
        assert!(gate.launch(JobKind::Save).is_none());
        gate.prompt_pending = false;
        let save = gate.launch(JobKind::Save).unwrap();
        assert!(!gate.change());
        gate.cancel();
        assert!(gate.saving());
        assert!(!gate.change());
        assert!(gate.launch(JobKind::Preview).is_none());
        assert_eq!(gate.finish(&save), Some(false));
        assert!(gate.change());
    }

    #[test]
    fn image_tool_view_cancelled_preview_cannot_install_or_start_without_new_request() {
        let mut gate = WorkGate::new();
        let preview = gate.launch(JobKind::Preview).unwrap();
        gate.cancel();
        assert_eq!(gate.finish(&preview), Some(false));
        assert!(gate.launch(JobKind::Preview).is_none());
        assert!(gate.change());
        assert!(gate.launch(JobKind::Preview).is_some());
        assert_eq!(scaled_side(1, 50), 1);
        assert_eq!(scaled_side(7, 50), 4);
        assert_eq!(scaled_side(7, 200), 14);
    }
}
