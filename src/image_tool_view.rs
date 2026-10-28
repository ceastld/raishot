//! 图片处理窗口：左侧变换、颜色、尺寸与保存选项，右侧预览；只接不可变源快照，处理和保存共用一个后台槽。
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
#[path = "tool_host.rs"]
pub(crate) mod host;
#[path = "tool_source_strip.rs"]
pub(crate) mod strip;
use self::host::{HostedTool, ToolEvent, ToolHost};
#[path = "image_tool_measure_view.rs"]
pub(crate) mod measure_view;
#[path = "image_pdf_view.rs"]
pub(crate) mod pdf_view;
#[path = "image_tool_qr_view.rs"]
pub(crate) mod qr_view;
#[path = "tool_single_window.rs"]
pub(crate) mod single;
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
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    div, img, prelude::*, px, rgb, AnyElement, App, AppContext, Context, Entity, EventEmitter,
    FocusHandle, KeyDownEvent, ObjectFit, Render, RenderImage, Subscription, Window, WindowHandle,
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

#[derive(Clone, Copy, PartialEq)]
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
    Save,
    Cancel,
    Close,
    Compare,
    Split,
    PdfCurrent,
    Measure,
    Barcodes,
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
    host: ToolHost,
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
    host::open_window(
        "tools",
        brand_title!("图片处理", "Image tools").into(),
        (960., 760.),
        (420., 400.),
        cx,
        move |host, window, cx| {
            ImageToolView::new(image, protected_paths, metadata_source, host, window, cx)
        },
    )
}

impl HostedTool for ImageToolView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.gate.active.is_some() || self.gate.prompt_pending
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImageToolView::request_close(self, cx)
    }
}

impl EventEmitter<ToolEvent> for ImageToolView {}

impl ImageToolView {
    #[allow(clippy::too_many_arguments)]
    fn new(
        image: Arc<Image>,
        protected_paths: Vec<PathBuf>,
        metadata_source: Option<Arc<SourceMetadata>>,
        host: ToolHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.defer_in(window, |view, window, cx| {
            view.start_preview(window, cx);
        });
        let focus = cx.focus_handle();
        window.focus(&focus);
        let width = cx.new(|cx| TextField::new(image.width.to_string(), tr!("宽度", "Width"), cx));
        let height =
            cx.new(|cx| TextField::new(image.height.to_string(), tr!("高度", "Height"), cx));
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
            host,
        }
    }
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
        // 旧预览留到新预览就绪，避免整块闪成空白；结果已失效，保存随之禁用。
        self.result = None;
        self.encoded = None;
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
                    host::finish_close(view.host, window, cx);
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
                    host::finish_close(view.host, window, cx);
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
                    host::finish_close(self.host, window, cx);
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
            | Control::Split
            | Control::PdfCurrent
            | Control::Measure
            | Control::Barcodes => {
                if !self.save_ready() {
                    return;
                }
                let result = match action {
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
                    Control::PdfCurrent => pdf_view::open_processed(
                        self.result.as_ref().unwrap().clone(),
                        self.protected_paths.clone(),
                        cx,
                    )
                    .map(|_| ()),
                    // 三个单图工具开在同一个窗口里，顶栏可互切，共用当前产物。
                    Control::Split | Control::Measure | Control::Barcodes => {
                        let tool = match action {
                            Control::Split => crate::tool_registry::ToolId::Split,
                            Control::Measure => crate::tool_registry::ToolId::Measure,
                            _ => crate::tool_registry::ToolId::Barcode,
                        };
                        single::open(
                            tool,
                            single::SingleImage::Processed(self.result.as_ref().unwrap().clone()),
                            self.protected_paths.clone(),
                            cx,
                        )
                        .map(|_| ())
                    }
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
        }
        self.changed(window, cx);
    }

    fn pick(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        self.control(action, window, cx);
    }

    /// 带图标的开关按钮：旋转、镜像、灰度这类一次点击的变换。
    fn toggle(
        &self,
        id: &'static str,
        icon: Option<Icon>,
        label: &'static str,
        active: bool,
        enabled: bool,
        action: Control,
        c: chrome::Chrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = if active {
            c.palette.accent
        } else {
            c.palette.foreground
        };
        div()
            .id(id)
            .h(px(32.))
            .px_3()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(if active {
                c.palette.accent
            } else {
                c.surface.line
            }))
            .bg(rgb(if active {
                c.palette.selected
            } else {
                c.surface.raised
            }))
            .text_size(px(13.))
            .text_color(rgb(color))
            .when(!enabled, |button| button.opacity(0.45))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(c.palette.hover)))
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.control(action, window, cx)),
                    )
            })
            .when_some(icon, |button, icon| {
                button.child(crate::toolbar_icons::action(icon, color).size(px(16.)))
            })
            .child(label)
            .into_any_element()
    }

    /// −10 / +10 步进的一行调整：标签、当前值、按钮。
    fn adjust_row(
        &self,
        id: &'static str,
        label: &'static str,
        value: i16,
        enabled: bool,
        make: fn(i16) -> Control,
        c: chrome::Chrome,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        chrome::option_row(label, c).child(chrome::stepper(
            id,
            format!("{value:+}"),
            enabled && value > -100,
            enabled && value < 100,
            c,
            cx,
            move |view, window, cx| view.control(make(-10), window, cx),
            move |view, window, cx| view.control(make(10), window, cx),
        ))
    }

    fn status(&self) -> &'static str {
        if self.gate.saving() {
            tr!("正在保存…", "Saving…")
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
        host::set_title(self.host, window, brand_title!("图片处理", "Image tools"));
        let c = chrome::chrome(cx);
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let enabled = !self.closing && !self.gate.saving();
        let busy = self.gate.active.is_some() || self.gate.pending_preview;

        // —— 变换 ——
        let transform = chrome::field(tr!("变换", "Transform"), c).child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(self.toggle(
                    "tool-rotate-left",
                    Some(Icon::RotateLeft),
                    tr!("左转", "Left"),
                    false,
                    enabled,
                    Control::Rotate(-1),
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "tool-rotate-right",
                    Some(Icon::RotateRight),
                    tr!("右转", "Right"),
                    false,
                    enabled,
                    Control::Rotate(1),
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "tool-flip-x",
                    None,
                    tr!("水平翻转", "Flip H"),
                    self.adjustments.flip_x,
                    enabled,
                    Control::FlipX,
                    c,
                    cx,
                ))
                .child(self.toggle(
                    "tool-flip-y",
                    None,
                    tr!("垂直翻转", "Flip V"),
                    self.adjustments.flip_y,
                    enabled,
                    Control::FlipY,
                    c,
                    cx,
                )),
        );

        // —— 颜色 ——
        let colors = chrome::field(tr!("颜色", "Color"), c)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.toggle(
                        "tool-gray",
                        None,
                        tr!("灰度", "Grayscale"),
                        self.adjustments.grayscale,
                        enabled,
                        Control::Grayscale,
                        c,
                        cx,
                    ))
                    .child(self.toggle(
                        "tool-invert",
                        None,
                        tr!("反色", "Invert"),
                        self.adjustments.invert,
                        enabled,
                        Control::Invert,
                        c,
                        cx,
                    )),
            )
            .child(self.adjust_row(
                "tool-brightness",
                tr!("亮度", "Brightness"),
                self.adjustments.brightness,
                enabled,
                Control::Brightness,
                c,
                cx,
            ))
            .child(self.adjust_row(
                "tool-contrast",
                tr!("对比度", "Contrast"),
                self.colors.contrast,
                enabled,
                Control::Contrast,
                c,
                cx,
            ))
            .child(self.adjust_row(
                "tool-saturation",
                tr!("饱和度", "Saturation"),
                self.colors.saturation,
                enabled,
                Control::Saturation,
                c,
                cx,
            ))
            .child(
                chrome::option_row(tr!("锐化", "Sharpen"), c).child(chrome::choice(
                    "tool-sharpen",
                    &[
                        (tr!("关", "Off"), Control::Sharpen(0)),
                        ("25%", Control::Sharpen(25)),
                        ("50%", Control::Sharpen(50)),
                        ("100%", Control::Sharpen(100)),
                    ],
                    Control::Sharpen(self.sharpening.amount_percent),
                    enabled,
                    c,
                    cx,
                    Self::pick,
                )),
            );

        // —— 尺寸 ——
        let current_scale = if self.custom_size.is_some() {
            Control::ApplySize
        } else {
            Control::Scale(self.scale)
        };
        let mut sizing = chrome::field(tr!("尺寸", "Size"), c).child(chrome::choice(
            "tool-scale",
            &[
                ("50%", Control::Scale(50)),
                ("100%", Control::Scale(100)),
                ("200%", Control::Scale(200)),
            ],
            current_scale,
            enabled,
            c,
            cx,
            Self::pick,
        ));
        let apply = chrome::action(
            "tool-apply-size",
            tr!("应用", "Apply"),
            if self.size_dirty {
                ButtonKind::Primary
            } else {
                ButtonKind::Secondary
            },
            enabled,
            c,
        );
        sizing = sizing
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(84.)).child(self.width.clone()))
                    .child(div().text_color(rgb(c.palette.muted)).child("×"))
                    .child(div().w(px(84.)).child(self.height.clone()))
                    .child(if enabled {
                        apply.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::ApplySize, window, cx)
                        }))
                    } else {
                        apply
                    }),
            )
            .child(chrome::hint(
                if self.size_dirty {
                    tr!(
                        "按回车或点“应用”改用这个宽高。",
                        "Press Enter or Apply to use these dimensions."
                    )
                } else if self.custom_size.is_some() {
                    tr!(
                        "使用指定宽高，可能改变比例。",
                        "Using exact dimensions; the ratio may change."
                    )
                } else {
                    tr!(
                        "百分比保持原比例；也可输入宽高。",
                        "Percentages keep the ratio; or type exact dimensions."
                    )
                },
                c,
            ));

        // —— 保存格式 ——
        let mut format = chrome::field(tr!("保存格式", "Format"), c).child(chrome::choice(
            "tool-format",
            &[
                ("PNG", Control::Format(SaveFormat::Png)),
                ("JPEG", Control::Format(SaveFormat::Jpeg)),
                ("BMP", Control::Format(SaveFormat::Bmp)),
                ("WebP", Control::Format(SaveFormat::Webp)),
            ],
            Control::Format(self.encoding.format),
            enabled,
            c,
            cx,
            Self::pick,
        ));
        match self.encoding.format {
            SaveFormat::Png => {
                format = format.child(chrome::option_row(tr!("压缩", "Compression"), c).child(
                    chrome::choice(
                        "tool-png",
                        &[
                            (
                                tr!("快速", "Fast"),
                                Control::Compression(PngCompression::Fast),
                            ),
                            (
                                tr!("均衡", "Balanced"),
                                Control::Compression(PngCompression::Balanced),
                            ),
                            (
                                tr!("最小", "Smallest"),
                                Control::Compression(PngCompression::Small),
                            ),
                        ],
                        Control::Compression(self.encoding.png_compression),
                        enabled,
                        c,
                        cx,
                        Self::pick,
                    ),
                ));
            }
            SaveFormat::Jpeg => {
                let quality = i16::from(self.encoding.jpeg_quality);
                format = format.child(chrome::option_row(tr!("质量", "Quality"), c).child(
                    chrome::stepper(
                        "tool-quality",
                        quality.to_string(),
                        enabled && quality > 1,
                        enabled && quality < 100,
                        c,
                        cx,
                        |view, window, cx| view.control(Control::Quality(-10), window, cx),
                        |view, window, cx| view.control(Control::Quality(10), window, cx),
                    ),
                ));
            }
            SaveFormat::Webp => {
                format = format.child(chrome::option_row(tr!("方式", "Mode"), c).child(
                    chrome::choice(
                        "tool-webp",
                        &[
                            (
                                tr!("无损", "Lossless"),
                                Control::WebpMode(WebpMode::Lossless),
                            ),
                            (tr!("有损", "Lossy"), Control::WebpMode(WebpMode::Lossy)),
                        ],
                        Control::WebpMode(self.encoding.webp.mode),
                        enabled,
                        c,
                        cx,
                        Self::pick,
                    ),
                ));
                if self.encoding.webp.mode == WebpMode::Lossy {
                    let quality = i16::from(self.encoding.webp.quality);
                    format = format.child(chrome::option_row(tr!("质量", "Quality"), c).child(
                        chrome::stepper(
                            "tool-webp-quality",
                            quality.to_string(),
                            enabled && quality > 1,
                            enabled && quality < 100,
                            c,
                            cx,
                            |view, window, cx| view.control(Control::WebpQuality(-10), window, cx),
                            |view, window, cx| view.control(Control::WebpQuality(10), window, cx),
                        ),
                    ));
                }
            }
            SaveFormat::Bmp => {}
        }
        if matches!(self.encoding.format, SaveFormat::Jpeg | SaveFormat::Bmp) {
            format = format.child(chrome::hint(
                tr!("透明部分会变成白色。", "Transparent areas become white."),
                c,
            ));
        }

        // —— 元数据 ——
        let exif_available = self
            .metadata_source
            .as_ref()
            .is_some_and(|source| source.exif_summary().is_some());
        let keep_allowed =
            self.metadata_source.is_some() && self.encoding.format != SaveFormat::Bmp;
        let mut options = vec![(
            tr!("去除", "Strip"),
            Control::Metadata(MetadataPolicy::Strip),
        )];
        if keep_allowed {
            options.push((
                tr!("保留颜色", "Keep color"),
                Control::Metadata(MetadataPolicy::ColorOnly),
            ));
            if exif_available {
                options.push((
                    tr!("颜色和 EXIF", "Color + EXIF"),
                    Control::Metadata(MetadataPolicy::ColorAndSupportedExif),
                ));
            }
        }
        let metadata = chrome::field(tr!("元数据", "Metadata"), c)
            .child(chrome::choice(
                "tool-metadata",
                &options,
                Control::Metadata(self.metadata_policy),
                enabled,
                c,
                cx,
                Self::pick,
            ))
            .child(chrome::hint(
                match self.metadata_policy {
                    MetadataPolicy::Strip => tr!(
                        "不带颜色配置和拍摄信息，体积最小。",
                        "No color profile or EXIF; smallest file."
                    ),
                    MetadataPolicy::ColorOnly => tr!(
                        "保留颜色配置，不含拍摄信息。",
                        "Keeps the color profile without EXIF."
                    ),
                    MetadataPolicy::ColorAndSupportedExif => tr!(
                        "保留颜色配置和常用拍摄信息；位置、缩略图不保留。",
                        "Keeps color and common EXIF; location and thumbnails are removed."
                    ),
                },
                c,
            ));

        // —— 用当前结果继续 ——
        let ready = self.save_ready();
        let mut next = div().flex().flex_wrap().gap_2();
        for (id, label, action) in [
            (
                "tool-compare",
                tr!("前后对比", "Before/after"),
                Control::Compare,
            ),
            ("tool-split", tr!("分割", "Split"), Control::Split),
            (
                "tool-measure",
                tr!("取色与测量", "Color & measure"),
                Control::Measure,
            ),
            (
                "tool-barcodes",
                tr!("识别条码", "Barcodes"),
                Control::Barcodes,
            ),
            (
                "tool-pdf-current",
                tr!("转 PDF", "To PDF"),
                Control::PdfCurrent,
            ),
        ] {
            let chip = widgets::chip(id, label, false, c.palette, c.surface);
            next = next.child(if ready {
                chip.on_click(
                    cx.listener(move |view, _, window, cx| view.control(action, window, cx)),
                )
            } else {
                chip.opacity(0.45)
            });
        }
        let continue_with = chrome::field(tr!("用结果继续", "Continue with result"), c).child(next);

        let sidebar = chrome::sidebar("image-tool-sidebar", c)
            .child(transform)
            .child(colors)
            .child(sizing)
            .child(format)
            .child(metadata)
            .child(continue_with);

        // —— 预览 ——
        let result_text = self.result.as_ref().map(|result| {
            let bytes = self
                .encoded
                .as_ref()
                .map_or(0, |encoded| encoded.byte_len());
            trf!(
                "结果 {} × {} · {}",
                "Result {} × {} · {}",
                result.image().width,
                result.image().height,
                file_size(bytes as u64)
            )
        });
        let reset = chrome::action(
            "tool-reset",
            tr!("全部还原", "Reset all"),
            ButtonKind::Secondary,
            enabled,
            c,
        );
        let bar = chrome::stage_bar(c)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .gap_3()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(trf!(
                        "源图 {} × {}",
                        "Source {} × {}",
                        self.source.width,
                        self.source.height
                    ))
                    .when_some(result_text, |text, result| {
                        text.child(div().text_color(rgb(c.palette.foreground)).child(result))
                    }),
            )
            .child(if enabled {
                reset.on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Reset, window, cx)),
                )
            } else {
                reset
            });
        let stage = chrome::stage(c)
            .flex_1()
            .min_h(px(0.))
            .p_4()
            .child(match &self.preview {
                Some(image) => img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                None => chrome::empty_state(Icon::Image, self.status(), c).into_any_element(),
            })
            .when(busy && self.preview.is_some(), |stage| {
                stage.child(
                    div()
                        .absolute()
                        .top(px(12.))
                        .right(px(12.))
                        .child(widgets::badge(self.status(), c.palette, c.surface)),
                )
            });
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
        let (status, tone) = if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else if busy || self.gate.saving() {
            (self.status().to_owned(), Tone::Busy)
        } else {
            (self.status().to_owned(), Tone::Idle)
        };
        let mut actions = Vec::new();
        if busy || self.gate.saving() {
            actions.push(
                chrome::action(
                    "tool-cancel",
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
        let save = chrome::action(
            "tool-save",
            tr!("另存为…", "Save as…"),
            ButtonKind::Primary,
            ready,
            c,
        );
        actions.push(
            if ready {
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

fn file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.)
    } else {
        format!("{bytes} B")
    }
}

/// 分割、测量、条码要整张源像素：在调用方线程按前景预算解码一张本地图片。
pub(crate) fn decode_tool_source(
    path: &std::path::Path,
    cancel: &AtomicBool,
) -> std::result::Result<crate::image_decode::DecodedImage, String> {
    crate::image_collection::ensure_local_file(path)?;
    let limits = ToolLimits::default();
    let decoded = crate::image_decode::decode_path_with_foreground_budget_cancellable(
        path,
        limits.max_input_pixels.saturating_mul(4),
        0,
        cancel,
    )
    .map_err(|error| error.to_string())?;
    validate_input(&decoded.image, limits).map_err(|error| error.to_string())?;
    Ok(decoded)
}

/// 单图工具拿到已解码像素后打开；三个单图工具共用一个可互切的窗口。
pub(crate) fn open_single(
    tool: crate::tool_registry::ToolId,
    image: Image,
    path: PathBuf,
    cx: &mut App,
) -> Result<()> {
    single::open(
        tool,
        single::SingleImage::Shared(Arc::new(image)),
        Arc::new(vec![path]),
        cx,
    )
    .map(|_| ())
}

/// 外部文件入口：不经主界面直接打开工具，最后一个窗口关闭即退出。
pub(crate) fn run_files(tool: crate::tool_registry::ToolId, paths: Vec<PathBuf>) -> Result<()> {
    use crate::tool_registry::ToolId;
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let errors = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let final_errors = errors.clone();
    gpui::Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let result = match tool {
                ToolId::Compare => compare_view::open_files(paths, cx).map(|_| ()),
                ToolId::Collage => collage_view::open(paths, cx).map(|_| ()),
                ToolId::Pdf => pdf_view::open(paths, cx).map(|_| ()),
                ToolId::Batch => batch_view::open(paths, cx).map(|_| ()),
                _ => match paths.as_slice() {
                    [path] => decode_tool_source(path, &AtomicBool::new(false))
                        .map_err(Into::into)
                        .and_then(|decoded| open_single(tool, decoded.image, path.clone(), cx)),
                    _ => Err(trf!(
                        "“{}”只接受 1 张图片",
                        "{} takes exactly 1 image",
                        tool.title()
                    )
                    .into()),
                },
            };
            if let Err(error) = result {
                *errors.borrow_mut() = Some(error.to_string());
                cx.quit();
            }
            cx.activate(true);
        });
    let error = final_errors.borrow_mut().take();
    match error {
        Some(error) => Err(error.into()),
        None => Ok(()),
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
