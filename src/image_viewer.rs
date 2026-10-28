//! 图片查看与当前目录浏览；源文件操作须另核加载身份与用户命令。
#[path = "image_viewer_animation.rs"]
mod animation;
#[path = "image_viewer_color.rs"]
mod color;
#[cfg(test)]
#[path = "image_viewer_edge_native_tests.rs"]
mod edge_native_tests;
#[path = "image_viewer_file_actions.rs"]
mod file_actions;
#[path = "image_viewer_file_dialog.rs"]
mod file_dialog;
#[path = "image_viewer_file_drag.rs"]
mod file_drag;
#[path = "image_viewer_file_mutation.rs"]
mod file_mutation;
#[path = "image_viewer_frame.rs"]
mod frame;
#[path = "image_viewer_grid.rs"]
mod grid;
#[cfg(test)]
#[path = "image_viewer_main_backpressure_tests.rs"]
mod main_backpressure_tests;
#[path = "image_viewer_pages.rs"]
mod pages;
#[path = "image_viewer_prefetch.rs"]
mod prefetch;
#[path = "image_viewer_prefetch_control.rs"]
mod prefetch_control;
#[cfg(test)]
#[path = "image_viewer_prefetch_worker_tests.rs"]
mod prefetch_worker_tests;
#[path = "image_viewer_preview.rs"]
mod preview;
#[path = "image_viewer_preview_control.rs"]
mod preview_control;
#[cfg(test)]
#[path = "image_viewer_preview_native_tests.rs"]
mod preview_native_tests;
#[path = "image_viewer_preview_paint.rs"]
mod preview_paint;
#[path = "image_viewer_preview_region.rs"]
mod preview_region;
#[cfg(test)]
#[path = "image_viewer_preview_region_worker_tests.rs"]
mod preview_region_worker_tests;
#[path = "image_viewer_probe.rs"]
mod probe;
#[path = "image_viewer_properties.rs"]
mod properties;
#[path = "image_viewer_recent.rs"]
mod recent;
#[cfg(test)]
#[path = "image_viewer_snapshot_backpressure_tests.rs"]
mod snapshot_backpressure_tests;
#[cfg(test)]
#[path = "image_viewer_snapshot_tests.rs"]
mod snapshot_tests;
#[path = "image_viewer_static_rotation.rs"]
mod static_rotation;
#[path = "image_viewer_svg_dialog.rs"]
mod svg_dialog;
#[cfg(test)]
#[path = "image_viewer_svg_worker_tests.rs"]
mod svg_worker_tests;
#[cfg(test)]
#[path = "image_viewer_thumbnail_budget_tests.rs"]
mod thumbnail_budget_tests;
#[path = "image_viewer_toolbar.rs"]
mod toolbar;
#[path = "image_webp_playback.rs"]
mod webp_playback;
#[path = "image_webp.rs"]
pub(crate) mod webp_source;

use crate::image_animation_playback::{PlaybackSnapshot, PlaybackState};
use crate::{
    annotation::Point,
    canvas_source::CanvasSource,
    geometry::{Rect, Rotation},
    image_collection::ImageCollection,
    image_metadata::ImageMetadata,
    image_viewer_rotation::ImageRotation,
    viewport::Viewport,
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, Context,
    Entity, EventEmitter, FocusHandle, KeyDownEvent, MouseButton, PathPromptOptions, RenderImage,
    ScrollDelta, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
#[cfg(test)]
use std::time::SystemTime;
use std::{
    collections::VecDeque,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const THUMBNAIL_DECODE_BYTES: u64 = 48 * 1024 * 1024;
const THUMBNAIL_WORKING_BYTES: u64 = 2 * 1024 * 1024;
const THUMBNAIL_SIDE: u32 = 256;
const THUMBNAIL_CACHE_BYTES: usize = 8 * 1024 * 1024;
const THUMBNAIL_CACHE_ITEMS: usize = 24;
const MAX_THUMBNAIL_JOBS: usize = 13;
const THUMBNAIL_STRIP_HEIGHT: f32 = 104.;

#[derive(Clone)]
enum Thumbnail {
    Ready(Arc<RenderImage>),
    Failed,
}

impl Thumbnail {
    fn bytes(&self) -> usize {
        match self {
            Self::Ready(image) => image.as_bytes(0).map_or(0, |bytes| bytes.len()),
            Self::Failed => 0,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ThumbnailKey {
    path: PathBuf,
    source_file: Option<crate::file_snapshot::FileSnapshot>,
}

impl ThumbnailKey {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            source_file: source_file_snapshot(path),
        }
    }
}

#[derive(Default)]
struct ThumbnailCache {
    entries: VecDeque<(ThumbnailKey, Thumbnail)>,
    bytes: usize,
}

impl ThumbnailCache {
    fn get(&mut self, key: &ThumbnailKey) -> Option<Thumbnail> {
        key.source_file.as_ref()?;
        let index = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(index)?;
        let thumbnail = entry.1.clone();
        self.entries.push_back(entry);
        Some(thumbnail)
    }

    fn insert(&mut self, key: ThumbnailKey, thumbnail: Thumbnail) {
        // 身份不可用时仍可生成本次缩略图，但不能凭路径复用或封存失败。
        if key.source_file.is_none() {
            return;
        }
        // 同一路径的新版本同时替换旧版本，失败项也参与条目预算。
        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached, _)| cached.path == key.path)
        {
            if let Some((_, previous)) = self.entries.remove(index) {
                self.bytes -= previous.bytes();
            }
        }
        if thumbnail.bytes() > THUMBNAIL_CACHE_BYTES {
            return;
        }
        self.bytes += thumbnail.bytes();
        self.entries.push_back((key, thumbnail));
        while self.entries.len() > THUMBNAIL_CACHE_ITEMS || self.bytes > THUMBNAIL_CACHE_BYTES {
            if let Some((_, previous)) = self.entries.pop_front() {
                self.bytes -= previous.bytes();
            }
        }
    }
}

struct PremultipliedImage<'a>(&'a crate::capture::Image);

impl image::GenericImageView for PremultipliedImage<'_> {
    // 16位有效值放在u32通道中，让超长图缩小的面积累加使用u64，避免溢出。
    type Pixel = image::Rgba<u32>;

    fn dimensions(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        let offset = (y as usize * self.0.width as usize + x as usize) * 4;
        let pixel = &self.0.bgra[offset..offset + 4];
        let alpha = u32::from(pixel[3]);
        let channel = |index: usize| (u32::from(pixel[index]) * alpha * 257 + 127) / 255;
        image::Rgba([channel(0), channel(1), channel(2), alpha * 257])
    }
}

fn thumbnail_dimensions(width: u32, height: u32) -> (u32, u32) {
    let divisor = width.max(height).max(THUMBNAIL_SIDE);
    (
        (u64::from(width) * u64::from(THUMBNAIL_SIDE) / u64::from(divisor)).max(1) as u32,
        (u64::from(height) * u64::from(THUMBNAIL_SIDE) / u64::from(divisor)).max(1) as u32,
    )
}

fn thumbnail_pixels(
    image: &crate::capture::Image,
) -> std::result::Result<crate::capture::Image, String> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if image.width == 0 || image.height == 0 || expected != Some(image.bgra.len()) {
        return Err(tr!("缩略图像素无效", "Invalid thumbnail pixels").into());
    }
    let (width, height) = thumbnail_dimensions(image.width, image.height);
    // 通道保持BGRA；借用整图做预乘采样，仅对≤256×256的输出反预乘。
    let pixels = image::imageops::thumbnail(&PremultipliedImage(image), width, height);
    let sampling_bytes = (pixels.as_raw().capacity() as u64)
        .checked_mul(std::mem::size_of::<u32>() as u64)
        .ok_or_else(|| tr!("缩略图容量溢出", "Thumbnail capacity overflow").to_string())?;
    let output_bytes = width as usize * height as usize * 4;
    let within_budget = |capacity: usize| {
        sampling_bytes
            .checked_add(capacity as u64)
            .is_some_and(|bytes| bytes <= THUMBNAIL_WORKING_BYTES)
    };
    if !within_budget(output_bytes) {
        return Err(tr!(
            "缩略图暂存超过预算",
            "Thumbnail workspace exceeds its budget"
        )
        .into());
    }
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(output_bytes)
        .map_err(|_| tr!("缩略图分配失败", "Thumbnail allocation failed").to_string())?;
    if !within_budget(bgra.capacity()) {
        return Err(tr!(
            "缩略图暂存超过预算",
            "Thumbnail workspace exceeds its budget"
        )
        .into());
    }
    for pixel in pixels.pixels() {
        let alpha = pixel.0[3];
        for channel in &pixel.0[..3] {
            bgra.push(if alpha == 0 {
                0
            } else {
                ((channel * 255 + alpha / 2) / alpha).min(255) as u8
            });
        }
        bgra.push(((alpha + 128) / 257).min(255) as u8);
    }
    Ok(crate::capture::Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

fn thumbnail(image: &crate::capture::Image) -> std::result::Result<Arc<RenderImage>, String> {
    let (_, previews) =
        CanvasSource::image(thumbnail_pixels(image)?).map_err(|error| error.to_string())?;
    previews
        .into_iter()
        .next()
        .ok_or_else(|| tr!("缩略图为空", "Empty thumbnail").into())
}

struct LoadedImage {
    metadata: ImageMetadata,
    /// 仅有采样栅格；没有完整源像素快照或编辑/导出资格。
    preview_geometry: Option<crate::image_preview::PreviewGeometry>,
    preview_source: Option<Arc<crate::image_preview::source::FrozenPreviewSource>>,
    tiles: Vec<(Rect, Arc<RenderImage>)>,
    thumbnail: Arc<RenderImage>,
    raw_frame: Option<frame::FrameRef>,
    raw_page: Option<Arc<crate::capture::Image>>,
    warning: Option<String>,
    metadata_source: Option<Arc<crate::image_export_metadata::SourceMetadata>>,
    rotation: Rotation,
}

impl LoadedImage {
    fn has_full_pixels(&self) -> bool {
        self.preview_geometry.is_none()
    }

    fn logical_dimensions(&self) -> (u32, u32) {
        self.preview_geometry
            .map_or(self.metadata.display_dimensions, |geometry| {
                geometry.normalized_source_dimensions()
            })
    }

    fn logical_display_dimensions(&self) -> (u32, u32) {
        let (width, height) = self.logical_dimensions();
        self.rotation.size(width, height)
    }

    fn projection(&self) -> Option<ImageRotation> {
        self.has_full_pixels()
            .then(|| ImageRotation::new(self.metadata.display_dimensions, self.rotation).ok())
            .flatten()
    }
}

/// 透明图按最坏情况计入棋盘副本；长图还需整批原始图块，先核算再分配。
fn check_presentation_budget(
    (width, height): (u32, u32),
    source_reserved_bytes: u64,
    metadata_bytes: u64,
    keep_raw: bool,
) -> std::result::Result<(), String> {
    check_presentation_budget_with_resident(
        (width, height),
        source_reserved_bytes,
        metadata_bytes,
        keep_raw,
        0,
    )
}

/// external_resident包含宿主仍持有的像素与缩略缓存；构造本图的预算另行追加。
fn check_presentation_budget_with_resident(
    (width, height): (u32, u32),
    source_reserved_bytes: u64,
    metadata_bytes: u64,
    keep_raw: bool,
    external_resident: u64,
) -> std::result::Result<(), String> {
    let invalid = || {
        tr!(
            "图片显示超过总驻留预算",
            "Image presentation exceeds the resident budget"
        )
        .to_string()
    };
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > crate::export::MAX_DECODE_SIDE
        || height > crate::export::MAX_DECODE_SIDE
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
    {
        return Err(invalid());
    }
    let pixel_bytes = pixels.checked_mul(4).ok_or_else(invalid)?;
    if source_reserved_bytes < pixel_bytes {
        return Err(invalid());
    }
    let extra_copies = if width > 16_384 || height > 16_384 {
        2u64
    } else {
        1u64
    } + u64::from(keep_raw);
    let retained = source_reserved_bytes
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(external_resident.max(THUMBNAIL_CACHE_BYTES as u64)))
        .ok_or_else(invalid)?;
    let (thumb_width, thumb_height) = thumbnail_dimensions(width, height);
    let thumbnail_bytes = u64::from(thumb_width) * u64::from(thumb_height) * 4;
    // 缩略采样先于整图呈现，取两个阶段的峰值；呈现期间仍保留最终缩略Arc。
    let sampling = retained
        .checked_add(THUMBNAIL_WORKING_BYTES)
        .ok_or_else(invalid)?;
    let visible = pixel_bytes
        .checked_mul(extra_copies)
        .and_then(|bytes| bytes.checked_add(retained))
        .and_then(|bytes| bytes.checked_add(thumbnail_bytes))
        .ok_or_else(invalid)?;
    if visible.max(sampling) > crate::image_decode::MAX_DECODE_BYTES {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
fn load(
    path: &Path,
    max_bytes: u64,
    cancel: &AtomicBool,
) -> std::result::Result<LoadedImage, String> {
    let decoded = crate::image_decode::decode_path_with_budget_cancellable(path, max_bytes, cancel)
        .map_err(|e| e.to_string())?;
    // 静态图与选中页都冻结同一份原色像素；后续操作不靠路径时间戳重建来源。
    pages::loaded(decoded, cancel)
}

fn load_with_resident(
    path: &Path,
    max_bytes: u64,
    cancel: &AtomicBool,
    resident_bytes: u64,
) -> std::result::Result<LoadedImage, String> {
    let decoded = crate::image_decode::viewer::decode_path(path, max_bytes, resident_bytes, cancel)
        .map_err(|error| error.to_string())?;
    match decoded {
        crate::image_decode::viewer::ViewerDecoded::Full(decoded) => {
            pages::loaded_with_resident(decoded, cancel, resident_bytes)
        }
        crate::image_decode::viewer::ViewerDecoded::Preview {
            decoded,
            encoded_bytes,
            source,
        } => preview::loaded_with_source(
            decoded,
            encoded_bytes,
            Some(source),
            cancel,
            resident_bytes,
        ),
    }
}

struct Request {
    serial: u64,
    path: PathBuf,
    scan: bool,
    page: Option<usize>,
    raster: Option<RasterRequest>,
    resident_bytes: u64,
}

struct RasterRequest {
    dimensions: (u32, u32),
    expected: crate::file_snapshot::FileSnapshot,
}

struct Completion {
    serial: u64,
    collection: Option<ImageCollection>,
    result: std::result::Result<LoadedImage, String>,
    animation: Option<PlaybackSnapshot>,
    pages: Option<pages::Info>,
    source_kind: Option<pages::Kind>,
    warning: Option<String>,
    source_file: Option<crate::file_snapshot::FileSnapshot>,
}

#[derive(Clone, Copy)]
enum SnapshotTarget {
    Editor,
    Crop,
    Ocr,
    Pin { origin: (i32, i32) },
    Tools,
    Clipboard { owner: isize, sequence: u32 },
}

struct SnapshotRequest {
    serial: u64,
    path: PathBuf,
    frame: Option<frame::FrameRef>,
    raw_page: Option<Arc<crate::capture::Image>>,
    metadata_source: Option<Arc<crate::image_export_metadata::SourceMetadata>>,
    resident_bytes: u64,
    target: SnapshotTarget,
}

struct SnapshotCompletion {
    serial: u64,
    path: PathBuf,
    target: SnapshotTarget,
    metadata_source: Option<Arc<crate::image_export_metadata::SourceMetadata>>,
    result: std::result::Result<SnapshotOutput, String>,
}

enum SnapshotOutput {
    Image(crate::capture::Image),
    Clipboard(crate::export::PreparedImageClipboard),
}

struct RotationRequest {
    serial: u64,
    epoch: u64,
    projection: ImageRotation,
    tiles: Vec<(Rect, Arc<RenderImage>)>,
    resident_bytes: u64,
    cancel: Arc<AtomicBool>,
}

struct RotationCompletion {
    serial: u64,
    epoch: u64,
    projection: ImageRotation,
    result: std::result::Result<Vec<(Rect, Arc<RenderImage>)>, String>,
}

struct AnimationUpdate {
    serial: u64,
    epoch: u64,
    tick: animation::Tick,
}

struct ThumbnailRequest {
    serial: u64,
    epoch: u64,
    resident_bytes: u64,
    cancel: Arc<AtomicBool>,
    paths: VecDeque<(usize, PathBuf)>,
}

struct ThumbnailCompletion {
    serial: u64,
    epoch: u64,
    index: usize,
    thumbnail: Thumbnail,
}

impl Mailbox {
    fn publish_thumbnail(&mut self, completion: ThumbnailCompletion) {
        if !self.stopped
            && self.serial == completion.serial
            && self.thumbnail_epoch == completion.epoch
            && self.thumbnails.len() < MAX_THUMBNAIL_JOBS
        {
            self.thumbnails.push(completion);
        }
    }
}

#[derive(Default)]
struct Mailbox {
    serial: u64,
    pending: Option<Request>,
    completed: Option<Completion>,
    thumbnail_request: Option<ThumbnailRequest>,
    thumbnail_epoch: u64,
    thumbnail_cancel: Option<Arc<AtomicBool>>,
    thumbnails: Vec<ThumbnailCompletion>,
    animation_epoch: u64,
    animation_presented: Option<(u64, u64)>,
    animation_controls: VecDeque<animation::Action>,
    animation_update: Option<AnimationUpdate>,
    active_cancel: Option<Arc<AtomicBool>>,
    snapshot_request: Option<SnapshotRequest>,
    snapshot_completed: Option<SnapshotCompletion>,
    rotation_request: Option<RotationRequest>,
    rotation_completed: Option<RotationCompletion>,
    rotation_cancel: Option<Arc<AtomicBool>>,
    preview_region_request: Option<preview_region::Request>,
    preview_region_completed: Option<preview_region::Completion>,
    preview_region_cancel: Option<Arc<AtomicBool>>,
    prefetch_request: Option<prefetch::Grant>,
    prefetch_cancel: Option<Arc<AtomicBool>>,
    prefetch_epoch: u64,
    generation_cancel: Arc<AtomicBool>,
    stopped: bool,
    #[cfg(test)]
    snapshot_backpressure_waits: u64,
    #[cfg(test)]
    main_backpressure_waits: u64,
    #[cfg(test)]
    preview_region_backpressure_waits: u64,
    #[cfg(test)]
    thumbnail_backpressure_waits: u64,
    #[cfg(test)]
    prefetch_commits: u64,
    #[cfg(test)]
    prefetch_hits: u64,
}

/// 请求与完成均为单槽，取消只使当前解码失效，不再排队整批解码。
struct Loader {
    shared: Arc<(Mutex<Mailbox>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Loader {
    fn new() -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rustshot-image-viewer".into())
            .spawn(move || decode_loop(worker))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn request(&self, request: Request) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::stop_animation(&mut mailbox);
        mailbox.serial = request.serial;
        mailbox.pending = Some(request);
        mailbox.completed = None;
        mailbox.thumbnail_request = None;
        mailbox.thumbnails.clear();
        self.shared.1.notify_one();
    }

    fn cancel(&self, serial: u64) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::stop_animation(&mut mailbox);
        mailbox.serial = serial;
        mailbox.pending = None;
        mailbox.completed = None;
        mailbox.thumbnail_request = None;
        mailbox.thumbnails.clear();
        self.shared.1.notify_one();
    }

    fn take(&self) -> Option<Completion> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let result = mailbox.completed.take();
        if result.is_some() {
            Self::revoke_thumbnails_locked(&mut mailbox);
            self.shared.1.notify_one();
        }
        result
    }

    fn stop_animation(mailbox: &mut Mailbox) {
        Self::revoke_prefetch_locked(mailbox);
        Self::revoke_thumbnails_locked(mailbox);
        Self::revoke_preview_region_locked(mailbox);
        mailbox.generation_cancel.store(true, Ordering::Relaxed);
        mailbox.generation_cancel = Arc::new(AtomicBool::new(false));
        if let Some(cancel) = mailbox.active_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.animation_controls.clear();
        mailbox.animation_update = None;
        mailbox.animation_epoch = 0;
        mailbox.animation_presented = None;
        mailbox.snapshot_request = None;
        mailbox.snapshot_completed = None;
        if let Some(cancel) = mailbox.rotation_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.rotation_request = None;
        mailbox.rotation_completed = None;
    }

    fn animation_control(&self, serial: u64, action: animation::Action) -> Option<u64> {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != serial {
            return None;
        }
        Self::revoke_prefetch_locked(&mut mailbox);
        Self::revoke_thumbnails_locked(&mut mailbox);
        if matches!(&action, animation::Action::Restart) {
            if let Some(cancel) = &mailbox.active_cancel {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        mailbox.animation_epoch = mailbox.animation_epoch.wrapping_add(1);
        mailbox.animation_update = None;
        if mailbox.animation_controls.len() == 32 {
            mailbox.animation_controls.pop_front();
        }
        mailbox.animation_controls.push_back(action);
        self.shared.1.notify_one();
        Some(mailbox.animation_epoch)
    }

    fn take_animation(&self) -> Option<AnimationUpdate> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let result = mailbox.animation_update.take();
        if result.is_some() {
            // 新帧在宿主应用回执与退役旧图之间不能沿旧账本继续解码。
            Self::revoke_thumbnails_locked(&mut mailbox);
            self.shared.1.notify_one();
        }
        result
    }

    fn presented(&self, serial: u64, epoch: u64, generation: u64) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.serial == serial && mailbox.animation_presented != Some((epoch, generation)) {
            mailbox.animation_presented = Some((epoch, generation));
            self.shared.1.notify_one();
        }
    }

    fn snapshot(&self, request: SnapshotRequest) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != request.serial {
            return;
        }
        Self::revoke_prefetch_locked(&mut mailbox);
        Self::revoke_thumbnails_locked(&mut mailbox);
        mailbox.snapshot_request = Some(request);
        self.shared.1.notify_one();
    }

    fn take_snapshot(&self) -> Option<SnapshotCompletion> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let completion = mailbox.snapshot_completed.take();
        if completion.is_some() {
            Self::revoke_thumbnails_locked(&mut mailbox);
            self.shared.1.notify_one();
        }
        completion
    }

    fn rotate(&self, request: RotationRequest) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != request.serial {
            return;
        }
        Self::revoke_prefetch_locked(&mut mailbox);
        Self::revoke_thumbnails_locked(&mut mailbox);
        if let Some(cancel) = mailbox.rotation_cancel.replace(request.cancel.clone()) {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.rotation_request = Some(request);
        mailbox.rotation_completed = None;
        self.shared.1.notify_one();
    }

    fn take_rotation(&self) -> Option<RotationCompletion> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let result = mailbox.rotation_completed.take();
        if result.is_some() {
            Self::revoke_thumbnails_locked(&mut mailbox);
            self.shared.1.notify_one();
        }
        result
    }

    fn request_thumbnails(&self, mut request: ThumbnailRequest) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != request.serial {
            return;
        }
        request.paths.truncate(MAX_THUMBNAIL_JOBS);
        Self::revoke_prefetch_locked(&mut mailbox);
        Self::revoke_thumbnails_locked(&mut mailbox);
        mailbox.thumbnail_epoch = request.epoch;
        mailbox.thumbnail_cancel = Some(request.cancel.clone());
        mailbox.thumbnail_request = Some(request);
        mailbox.thumbnails.clear();
        self.shared.1.notify_one();
    }

    fn revoke_thumbnails_locked(mailbox: &mut Mailbox) {
        if let Some(cancel) = mailbox.thumbnail_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.thumbnail_request = None;
        mailbox.thumbnails.clear();
    }

    fn revoke_thumbnails(&self) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::revoke_thumbnails_locked(&mut mailbox);
        self.shared.1.notify_one();
    }

    fn take_thumbnails(&self) -> Vec<ThumbnailCompletion> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let result = std::mem::take(&mut mailbox.thumbnails);
        if !result.is_empty() {
            self.shared.1.notify_one();
        }
        result
    }

    fn shutdown(&self) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::stop_animation(&mut mailbox);
        mailbox.stopped = true;
        mailbox.pending = None;
        mailbox.completed = None;
        mailbox.thumbnail_request = None;
        mailbox.thumbnails.clear();
        self.shared.1.notify_one();
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.shutdown();
        // 同步解码不能在中间安全终止；关闭时不阻塞 UI，工作线程完成后释放像素并退出。
        // 不持有窗口或 GPU 设备，且最多只有当前一次受预算约束的解码。
        self.thread.take();
    }
}

fn current(shared: &(Mutex<Mailbox>, Condvar), serial: u64) -> bool {
    let mailbox = shared.0.lock().unwrap();
    !mailbox.stopped && mailbox.serial == serial
}

fn decode_loop(shared: Arc<(Mutex<Mailbox>, Condvar)>) {
    let clock = Instant::now();
    let mut active: Option<(u64, animation::AnyAnimation)> = None;
    let mut thumbnails = ThumbnailCache::default();
    let mut cache: Option<(prefetch::Entry, Arc<AtomicBool>)> = None;
    loop {
        enum Work {
            Main(Request),
            Control(u64, u64, VecDeque<animation::Action>),
            Animation(u64, u64),
            Begin(u64, u64, u64),
            Snapshot(SnapshotRequest),
            Rotation(RotationRequest),
            PreviewRegion(preview_region::Request),
            Prefetch(prefetch::Grant),
            Thumbnail {
                serial: u64,
                epoch: u64,
                index: usize,
                path: PathBuf,
                resident_bytes: u64,
                cancel: Arc<AtomicBool>,
            },
        }
        let work = {
            let mut mailbox = shared.0.lock().unwrap();
            loop {
                if mailbox.stopped {
                    return;
                }
                if let Some(request) = mailbox.pending.take() {
                    break Work::Main(request);
                }
                if cache
                    .as_ref()
                    .is_some_and(|(_, cancel)| cancel.load(Ordering::Relaxed))
                {
                    cache = None;
                }
                if mailbox.completed.is_some() {
                    // 宿主可能仍保留旧冻结图；新主图完成槽取走前不再生成其它像素。
                    #[cfg(test)]
                    {
                        mailbox.main_backpressure_waits += 1;
                        shared.1.notify_all();
                    }
                    mailbox = shared.1.wait(mailbox).unwrap();
                    continue;
                }
                if mailbox.snapshot_completed.is_some() {
                    // 完成槽可能持有整图或剪贴板准备缓冲；先由宿主取走，才能开始下一次像素分配。
                    // 主请求、取消和关闭会清空过期槽并唤醒；不能丢弃当前有效结果换取继续执行。
                    #[cfg(test)]
                    {
                        mailbox.snapshot_backpressure_waits += 1;
                        shared.1.notify_all();
                    }
                    mailbox = shared.1.wait(mailbox).unwrap();
                    continue;
                }
                if mailbox.preview_region_completed.is_some() {
                    // ROI完成槽仍持有直alpha栅格；宿主消费或撤销前不开始其它像素分配。
                    #[cfg(test)]
                    {
                        mailbox.preview_region_backpressure_waits += 1;
                        shared.1.notify_all();
                    }
                    mailbox = shared.1.wait(mailbox).unwrap();
                    continue;
                }
                if active
                    .as_ref()
                    .is_some_and(|(serial, _)| *serial != mailbox.serial)
                {
                    active = None;
                }
                let pending_presentation = active
                    .as_ref()
                    .and_then(|(_, animation)| animation.presentation_pending());
                if let Some(generation) = pending_presentation {
                    // 控件epoch变化不能否认本代首帧已经显示；否则紧接初显的Enter会在暂停核心里丢失。
                    if mailbox
                        .animation_presented
                        .is_some_and(|(_, presented)| presented == generation)
                    {
                        break Work::Begin(mailbox.serial, mailbox.animation_epoch, generation);
                    }
                }
                if !mailbox.animation_controls.is_empty() {
                    let commands = std::mem::take(&mut mailbox.animation_controls);
                    break Work::Control(mailbox.serial, mailbox.animation_epoch, commands);
                }
                if let Some(request) = mailbox.snapshot_request.take() {
                    break Work::Snapshot(request);
                }
                if let Some(request) = mailbox.rotation_request.take() {
                    break Work::Rotation(request);
                }
                if let Some(request) = mailbox.preview_region_request.take() {
                    break Work::PreviewRegion(request);
                }
                let deadline = active
                    .as_ref()
                    .and_then(|(_, animation)| animation.deadline());
                if deadline.is_some_and(|deadline| deadline <= clock.elapsed()) {
                    break Work::Animation(mailbox.serial, mailbox.animation_epoch);
                }
                // 连续播放期间不让低优先级同步解码阻塞下一帧；暂停后继续同一缩略计划。
                #[cfg(test)]
                if mailbox.thumbnail_request.is_some()
                    && (mailbox.rotation_completed.is_some() || mailbox.animation_update.is_some())
                {
                    mailbox.thumbnail_backpressure_waits += 1;
                    shared.1.notify_all();
                }
                if deadline.is_none()
                    && pending_presentation.is_none()
                    && mailbox.rotation_completed.is_none()
                    && mailbox.animation_update.is_none()
                {
                    if let Some(request) = mailbox.thumbnail_request.as_mut() {
                        if let Some((index, path)) = request.paths.pop_front() {
                            break Work::Thumbnail {
                                serial: request.serial,
                                epoch: request.epoch,
                                index,
                                path,
                                resident_bytes: request.resident_bytes,
                                cancel: request.cancel.clone(),
                            };
                        }
                        mailbox.thumbnail_request = None;
                    }
                }
                if active.is_none()
                    && mailbox.completed.is_none()
                    && mailbox.snapshot_completed.is_none()
                    && mailbox.rotation_completed.is_none()
                    && mailbox.animation_update.is_none()
                    && mailbox.thumbnails.is_empty()
                {
                    if let Some(grant) = mailbox.prefetch_request.take() {
                        break Work::Prefetch(grant);
                    }
                }
                mailbox = if let Some(deadline) = deadline {
                    shared
                        .1
                        .wait_timeout(mailbox, deadline.saturating_sub(clock.elapsed()))
                        .unwrap()
                        .0
                } else {
                    shared.1.wait(mailbox).unwrap()
                };
            }
        };
        let request = match work {
            Work::Main(request) => request,
            Work::PreviewRegion(request) => {
                cache = None;
                let serial = request.serial;
                let cancel = request.cancel.clone();
                {
                    let mailbox = shared.0.lock().unwrap();
                    if mailbox.stopped || mailbox.serial != serial || cancel.load(Ordering::Acquire)
                    {
                        continue;
                    }
                }
                let completion = preview_region::complete(request);
                let mut mailbox = shared.0.lock().unwrap();
                if !mailbox.stopped && mailbox.serial == serial && !cancel.load(Ordering::Acquire) {
                    mailbox.preview_region_completed = Some(completion);
                    shared.1.notify_all();
                }
                continue;
            }
            Work::Prefetch(grant) => {
                cache = None;
                let serial = grant.serial;
                let epoch = grant.epoch;
                let cancel = grant.cancel.clone();
                // 动画与集合来源仍由主请求建立目录/帧时钟，不能用首幅raw绕过它们。
                if !matches!(probe::detect(&grant.path, &cancel), Ok(None)) {
                    continue;
                }
                let entry = prefetch::load(grant);
                #[allow(unused_mut)]
                let mut mailbox = shared.0.lock().unwrap();
                if !mailbox.stopped
                    && mailbox.serial == serial
                    && mailbox.prefetch_epoch == epoch
                    && !cancel.load(Ordering::Relaxed)
                {
                    if let Ok(entry) = entry {
                        cache = Some((entry, cancel));
                        #[cfg(test)]
                        {
                            mailbox.prefetch_commits += 1;
                            shared.1.notify_all();
                        }
                    }
                }
                continue;
            }
            Work::Rotation(request) => {
                cache = None;
                let result =
                    static_rotation::rotate(&request, || request.cancel.load(Ordering::Relaxed));
                let mut mailbox = shared.0.lock().unwrap();
                if !mailbox.stopped
                    && mailbox.serial == request.serial
                    && !request.cancel.load(Ordering::Relaxed)
                {
                    mailbox.rotation_completed = Some(RotationCompletion {
                        serial: request.serial,
                        epoch: request.epoch,
                        projection: request.projection,
                        result,
                    });
                }
                continue;
            }
            Work::Begin(serial, epoch, generation) => {
                cache = None;
                if let Some((_, animation)) = active.as_mut() {
                    let tick = animation.begin(clock.elapsed(), generation);
                    publish_animation(&shared, serial, epoch, tick, animation.cancellation());
                }
                continue;
            }
            Work::Snapshot(request) => {
                cache = None;
                let cancel = {
                    let mailbox = shared.0.lock().unwrap();
                    if mailbox.stopped || mailbox.serial != request.serial {
                        continue;
                    }
                    mailbox.generation_cancel.clone()
                };
                let result = source_snapshot_cancellable(&request, &cancel).and_then(|image| {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(tr!("图片操作已取消", "Image operation canceled").into());
                    }
                    match request.target {
                        SnapshotTarget::Clipboard { .. } => {
                            crate::export::prepare_image_clipboard_with_budget_cancellable(
                                &image,
                                &cancel,
                                request
                                    .resident_bytes
                                    .saturating_add(image.bgra.capacity() as u64),
                                crate::image_decode::MAX_DECODE_BYTES,
                            )
                            .map(SnapshotOutput::Clipboard)
                            .map_err(|error| error.to_string())
                        }
                        _ => Ok(SnapshotOutput::Image(image)),
                    }
                });
                let mut mailbox = shared.0.lock().unwrap();
                if !mailbox.stopped && mailbox.serial == request.serial {
                    mailbox.snapshot_completed = Some(SnapshotCompletion {
                        serial: request.serial,
                        path: request.path,
                        target: request.target,
                        metadata_source: request.metadata_source,
                        result,
                    });
                }
                continue;
            }
            Work::Control(serial, epoch, commands) => {
                cache = None;
                if let Some((active_serial, animation)) = active.as_mut() {
                    if *active_serial == serial {
                        let mut tick = None;
                        for command in commands {
                            // 丢弃同批尚未交付的旧显示副本，再分配下一次控制的呈现。
                            drop(tick.take());
                            tick = Some(animation.control(command, clock.elapsed()));
                        }
                        if let Some(tick) = tick {
                            publish_animation(
                                &shared,
                                serial,
                                epoch,
                                tick,
                                animation.cancellation(),
                            );
                        }
                    }
                }
                continue;
            }
            Work::Animation(serial, epoch) => {
                cache = None;
                if let Some((active_serial, animation)) = active.as_mut() {
                    if *active_serial == serial {
                        let tick = animation.tick(clock.elapsed(), false);
                        publish_animation(&shared, serial, epoch, tick, animation.cancellation());
                    }
                }
                continue;
            }
            Work::Thumbnail {
                serial,
                epoch,
                index,
                path,
                resident_bytes,
                cancel,
            } => {
                cache = None;
                {
                    let mailbox = shared.0.lock().unwrap();
                    if mailbox.stopped
                        || mailbox.serial != serial
                        || mailbox.thumbnail_epoch != epoch
                        || cancel.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                }
                let resident_bytes = resident_bytes.saturating_add(
                    active
                        .as_ref()
                        .map_or(0, |(_, animation)| animation.retained_bytes()),
                );
                let Some(decode_resident_bytes) = resident_bytes
                    .checked_add(THUMBNAIL_WORKING_BYTES)
                    .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
                else {
                    let mut mailbox = shared.0.lock().unwrap();
                    if !cancel.load(Ordering::Relaxed) {
                        mailbox.publish_thumbnail(ThumbnailCompletion {
                            serial,
                            epoch,
                            index,
                            thumbnail: Thumbnail::Failed,
                        });
                    }
                    continue;
                };
                let key = ThumbnailKey::new(&path);
                let thumbnail = thumbnails.get(&key).unwrap_or_else(|| {
                    // 缩略请求只生成缩略图；集合来源也不能借主图入口分配整批显示副本。
                    let result = crate::image_decode::decode_path_for_thumbnail_cancellable(
                        &path,
                        THUMBNAIL_DECODE_BYTES,
                        decode_resident_bytes,
                        &cancel,
                    )
                    .map_err(|error| error.to_string())
                    .and_then(|decoded| {
                        let raw = decoded.image;
                        // 缩略采样前释放来源sidecar，额外量只留下raw和有界采样暂存。
                        drop(decoded.metadata);
                        drop(decoded.metadata_source);
                        drop(decoded.warning);
                        thumbnail(&raw)
                    })
                    .ok()
                    .filter(|_| ThumbnailKey::new(&path) == key)
                    .map_or(Thumbnail::Failed, Thumbnail::Ready);
                    // 取消/切图不是解码失败；不能把健康邻图按同一版本缓存为失败。
                    if ThumbnailKey::new(&path) != key {
                        return Thumbnail::Failed;
                    }
                    let mailbox = shared.0.lock().unwrap();
                    if mailbox.stopped
                        || mailbox.serial != serial
                        || mailbox.thumbnail_epoch != epoch
                        || cancel.load(Ordering::Relaxed)
                    {
                        return Thumbnail::Failed;
                    }
                    // 与请求换代共用锁，检查与写缓存之间也不能插入一次取消。
                    // 宿主预算不足或取消不是来源损坏；失败不能写入跨请求负缓存。
                    if matches!(&result, Thumbnail::Ready(_)) {
                        thumbnails.insert(key, result.clone());
                    }
                    drop(mailbox);
                    result
                });
                let mut mailbox = shared.0.lock().unwrap();
                if cancel.load(Ordering::Relaxed) {
                    continue;
                }
                mailbox.publish_thumbnail(ThumbnailCompletion {
                    serial,
                    epoch,
                    index,
                    thumbnail,
                });
                continue;
            }
        };
        active = None;
        if request.scan
            || request.page.is_some()
            || request.raster.is_some()
            || cache
                .as_ref()
                .is_some_and(|(entry, _)| entry.path != request.path)
        {
            cache = None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut mailbox = shared.0.lock().unwrap();
            if mailbox.stopped || mailbox.serial != request.serial {
                continue;
            }
            // 枚举目录前就交出令牌，换图/关闭不用等旧目录逐项完成元数据检查。
            // 与快照操作使用的generation_cancel独立，重播换令牌不取消后续快照。
            mailbox.active_cancel = Some(cancel.clone());
        }
        let collection = request
            .scan
            .then(|| ImageCollection::scan_cancellable(&request.path, &cancel).ok())
            .flatten();
        if cancel.load(Ordering::Relaxed) || !current(&shared, request.serial) {
            continue;
        }
        let key = ThumbnailKey::new(&request.path);
        let source_file = key.source_file.clone();
        if cache.as_ref().is_some_and(|(entry, _)| {
            source_file.as_ref() != Some(&entry.source_file)
                || request
                    .resident_bytes
                    .checked_add(entry.retained_bytes)
                    .and_then(|bytes| bytes.checked_add(prefetch::QUALIFICATION_BYTES))
                    .is_none_or(|bytes| bytes > crate::image_decode::MAX_DECODE_BYTES)
        }) {
            cache = None;
        }
        let mut animation_warning = None;
        let mut pages = None;
        let mut source_kind = None;
        let local_header = if request.raster.is_some() {
            Ok(())
        } else {
            crate::image_collection::ensure_local_file(&request.path)
                .and_then(|()| pages::format_hint(&request.path, &cancel))
                .map(|kind| source_kind = kind)
        };
        if source_kind.is_some() || local_header.is_err() {
            cache = None;
        }
        let mut result = if let Some(raster) = &request.raster {
            if source_file.as_ref() != Some(&raster.expected) {
                Err(tr!("SVG来源已变化，请重新加载", "SVG source changed; reload it").into())
            } else {
                crate::image_decode::decode_svg_path_with_resident_budget_cancellable(
                    &request.path,
                    raster.dimensions,
                    crate::image_decode::MAX_DECODE_BYTES,
                    request.resident_bytes,
                    &cancel,
                )
                .map_err(|error| error.to_string())
                .and_then(|decoded| {
                    pages::loaded_with_resident(decoded, &cancel, request.resident_bytes)
                })
            }
        } else {
            match local_header {
                Err(error) => Err(error),
                Ok(()) => match probe::detect(&request.path, &cancel) {
                    Err(error) => Err(error.to_string()),
                    Ok(Some(kind)) => {
                        cache = None;
                        match animation::AnyAnimation::open(
                            kind,
                            &request.path,
                            request.resident_bytes,
                            cancel.clone(),
                            clock.elapsed(),
                        ) {
                            Ok((animation, image)) => {
                                active = Some((request.serial, animation));
                                Ok(image)
                            }
                            Err(error)
                                if !cancel.load(Ordering::Relaxed)
                                    && current(&shared, request.serial) =>
                            {
                                load_with_resident(
                                    &request.path,
                                    crate::image_decode::MAX_DECODE_BYTES,
                                    &cancel,
                                    request.resident_bytes,
                                )
                                .map(|image| {
                                    animation_warning = Some(trf!(
                                        "动画无法播放，显示静态预览：{error}",
                                        "Animation unavailable; showing static preview: {error}"
                                    ));
                                    image
                                })
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Ok(None)
                        if cancel.load(Ordering::Relaxed) || !current(&shared, request.serial) =>
                    {
                        Err(tr!("图片读取已取消", "Image reading canceled").into())
                    }
                    Ok(None) => {
                        // 资格失败时先释放raw，再允许集合解码分配；文件可能在探测后换格式。
                        let cached = cache.take().and_then(|(entry, _)| {
                            matches!(entry.matches(&request.path, &cancel), Ok(true))
                                .then_some(entry)
                        });
                        if let Some(entry) = cached {
                            let loaded = pages::loaded_with_resident(
                                entry.into_decoded(),
                                &cancel,
                                request.resident_bytes,
                            );
                            #[cfg(test)]
                            if loaded.is_ok() {
                                let mut mailbox = shared.0.lock().unwrap();
                                mailbox.prefetch_hits += 1;
                                shared.1.notify_all();
                            }
                            loaded
                        } else {
                            match pages::open_if_collection_with_resident(
                                &request.path,
                                request.page,
                                request.resident_bytes,
                                &cancel,
                            ) {
                                Ok(Some(opened)) => {
                                    pages = Some(opened.info);
                                    opened.result
                                }
                                Err(error) => Err(error),
                                Ok(None)
                                    if cancel.load(Ordering::Relaxed)
                                        || !current(&shared, request.serial) =>
                                {
                                    Err(tr!("图片读取已取消", "Image reading canceled").into())
                                }
                                Ok(None) => load_with_resident(
                                    &request.path,
                                    crate::image_decode::MAX_DECODE_BYTES,
                                    &cancel,
                                    request.resident_bytes,
                                ),
                            }
                        }
                    }
                },
            }
        };
        let source_file = source_file.filter(|expected| {
            (result.is_ok()
                || request
                    .raster
                    .as_ref()
                    .is_some_and(|raster| &raster.expected == expected))
                && source_file_snapshot(&request.path).as_ref() == Some(expected)
        });
        if request.raster.is_some() && result.is_ok() && source_file.is_none() {
            result = Err(tr!("SVG来源已变化，请重新加载", "SVG source changed; reload it").into());
        }
        let unchanged = key.source_file.is_some() && ThumbnailKey::new(&request.path) == key;
        let mut mailbox = shared.0.lock().unwrap();
        if mailbox.stopped {
            break;
        }
        if mailbox.serial != request.serial {
            continue;
        }
        if let Some(warning) = result
            .as_ref()
            .ok()
            .and_then(|image| image.warning.as_deref())
        {
            animation_warning = Some(match animation_warning {
                Some(previous) => format!("{previous} · {warning}"),
                None => warning.to_string(),
            });
        }
        let small = result.as_ref().map_or(Thumbnail::Failed, |image| {
            Thumbnail::Ready(image.thumbnail.clone())
        });
        if unchanged && request.raster.is_none() && matches!(&small, Thumbnail::Ready(_)) {
            thumbnails.insert(key.clone(), small);
        }
        mailbox.completed = Some(Completion {
            serial: request.serial,
            collection,
            result,
            animation: active.as_ref().map(|(_, animation)| animation.snapshot()),
            pages,
            source_kind,
            warning: animation_warning,
            source_file,
        });
        drop(mailbox);
    }
}

fn source_file_snapshot(path: &Path) -> Option<crate::file_snapshot::FileSnapshot> {
    crate::image_collection::ensure_local_file(path).ok()?;
    crate::file_snapshot::inspect(path).ok().flatten()
}

/// 子编辑器在后台关闭时只释放状态，不把前台从其它应用或窗口抢回来。
fn editor_return_has_foreground(child: isize, parent: isize) -> bool {
    let foreground =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;
    foreground != 0 && (foreground == child || foreground == parent)
}

#[cfg(test)]
fn source_snapshot(
    request: &SnapshotRequest,
) -> std::result::Result<crate::capture::Image, String> {
    source_snapshot_cancellable(request, &AtomicBool::new(false))
}

fn source_snapshot_cancellable(
    request: &SnapshotRequest,
    cancel: &AtomicBool,
) -> std::result::Result<crate::capture::Image, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(tr!("图片操作已取消", "Image operation canceled").into());
    }
    let raw = request
        .frame
        .as_ref()
        .map(|frame| frame.image())
        .or(request.raw_page.as_deref())
        .ok_or_else(|| {
            tr!(
                "没有已交付的原色快照，请重新加载图片",
                "No delivered source-pixel snapshot; reload the image"
            )
            .to_string()
        })?;
    let invalid = || {
        tr!(
            "图片快照超过总驻留预算",
            "Image snapshot exceeds the resident budget"
        )
        .to_string()
    };
    let expected = u64::from(raw.width)
        .checked_mul(u64::from(raw.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(invalid)?;
    let minimum_resident = (raw.bgra.capacity() as u64).saturating_add(
        request
            .metadata_source
            .as_ref()
            .map_or(0, |source| source.retained_bytes() as u64),
    );
    if raw.width == 0
        || raw.height == 0
        || raw.width > crate::export::MAX_DECODE_SIDE
        || raw.height > crate::export::MAX_DECODE_SIDE
        || expected / 4 > crate::image_decode::MAX_DECODE_PIXELS
        || expected != raw.bgra.len() as u64
        || request.resident_bytes < minimum_resident
        || request
            .resident_bytes
            .checked_add(expected)
            .is_none_or(|bytes| bytes > crate::image_decode::MAX_DECODE_BYTES)
    {
        return Err(invalid());
    }
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(raw.bgra.len())
        .map_err(|_| tr!("图片快照分配失败", "Image snapshot allocation failed").to_string())?;
    if request
        .resident_bytes
        .checked_add(bgra.capacity() as u64)
        .is_none_or(|bytes| bytes > crate::image_decode::MAX_DECODE_BYTES)
    {
        return Err(invalid());
    }
    for chunk in raw.bgra.chunks(64 * 1024) {
        if cancel.load(Ordering::Relaxed) {
            return Err(tr!("图片操作已取消", "Image operation canceled").into());
        }
        bgra.extend_from_slice(chunk);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(tr!("图片操作已取消", "Image operation canceled").into());
    }
    Ok(crate::capture::Image {
        width: raw.width,
        height: raw.height,
        bgra,
        readback_ms: raw.readback_ms,
    })
}

/// 同一渲染Arc或帧租约可能同时在当前、原始和退役队列中，只计一次可见缓冲。
fn presentation_resident_bytes<'a>(
    image: &'a LoadedImage,
    extra_tiles: impl Iterator<Item = &'a Arc<RenderImage>>,
    extra_frames: impl Iterator<Item = &'a frame::FrameRef>,
) -> u64 {
    presentation_resident_bytes_for(Some(image), extra_tiles, extra_frames)
}

fn presentation_resident_bytes_for<'a>(
    image: Option<&'a LoadedImage>,
    extra_tiles: impl Iterator<Item = &'a Arc<RenderImage>>,
    extra_frames: impl Iterator<Item = &'a frame::FrameRef>,
) -> u64 {
    presentation_resident_bytes_for_excluding_preview_source(image, extra_tiles, extra_frames, None)
}

fn presentation_resident_bytes_for_excluding_preview_source<'a>(
    image: Option<&'a LoadedImage>,
    extra_tiles: impl Iterator<Item = &'a Arc<RenderImage>>,
    extra_frames: impl Iterator<Item = &'a frame::FrameRef>,
    exclude_source: Option<&Arc<crate::image_preview::source::FrozenPreviewSource>>,
) -> u64 {
    let mut resident = THUMBNAIL_CACHE_BYTES as u64;
    if let Some(source) = image.and_then(|image| image.preview_source.as_ref()) {
        resident = resident.saturating_add(
            crate::image_preview::source::source_resident_bytes(&[source], exclude_source)
                .unwrap_or(u64::MAX),
        );
    }
    let mut raw_seen = std::collections::HashSet::new();
    let mut metadata_seen = std::collections::HashSet::new();
    let mut render_seen = std::collections::HashSet::new();
    if let Some(raw) = image.and_then(|image| image.raw_page.as_ref()) {
        raw_seen.insert(Arc::as_ptr(raw) as usize);
        resident = resident.saturating_add(raw.bgra.capacity() as u64);
    }
    if let Some(source) = image.and_then(|image| image.metadata_source.as_ref()) {
        metadata_seen.insert(Arc::as_ptr(source) as usize);
        resident = resident.saturating_add(source.retained_bytes() as u64);
    }
    if let Some(warning) = image.and_then(|image| image.warning.as_ref()) {
        resident = resident.saturating_add(warning.capacity() as u64);
    }
    for frame in image
        .into_iter()
        .flat_map(|image| image.raw_frame.iter())
        .chain(extra_frames)
    {
        let raw = frame.image();
        if raw_seen.insert(raw as *const crate::capture::Image as usize) {
            resident = resident.saturating_add(raw.bgra.capacity() as u64);
        }
        if let Some(source) = frame.metadata_source() {
            if metadata_seen.insert(Arc::as_ptr(source) as usize) {
                resident = resident.saturating_add(source.retained_bytes() as u64);
            }
        }
    }
    for tile in image
        .into_iter()
        .flat_map(|image| image.tiles.iter().map(|(_, tile)| tile))
        .chain(image.into_iter().map(|image| &image.thumbnail))
        .chain(extra_tiles)
    {
        if render_seen.insert(Arc::as_ptr(tile) as usize) {
            resident =
                resident.saturating_add(tile.as_bytes(0).map_or(0, |bytes| bytes.len() as u64));
        }
    }
    resident
}

fn publish_animation(
    shared: &(Mutex<Mailbox>, Condvar),
    serial: u64,
    epoch: u64,
    tick: animation::Tick,
    cancel: Arc<AtomicBool>,
) {
    let mut mailbox = shared.0.lock().unwrap();
    if mailbox.stopped || mailbox.serial != serial {
        cancel.store(true, Ordering::Relaxed);
        return;
    }
    mailbox.active_cancel = Some(cancel);
    if mailbox.animation_epoch == epoch {
        mailbox.animation_update = Some(AnimationUpdate {
            serial,
            epoch,
            tick,
        });
    }
}

#[cfg(test)]
fn file_stamp(path: &Path) -> Option<(u64, SystemTime)> {
    crate::image_collection::ensure_local_file(path).ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

fn thumbnail_range(total: usize, current: usize, width: f32) -> Range<usize> {
    if total == 0 {
        return 0..0;
    }
    let slots = (((width - 18.).max(1.) / 94.).floor() as usize).clamp(1, 11);
    let slots = if slots > 1 && slots % 2 == 0 {
        slots - 1
    } else {
        slots
    };
    let count = slots.min(total);
    let start = current
        .min(total - 1)
        .saturating_sub(count / 2)
        .min(total - count);
    start..start + count
}

struct ThumbnailSlot {
    index: usize,
    thumbnail: Option<Thumbnail>,
}

pub(crate) struct PopupCloseRequested;

pub(crate) struct ImageViewer {
    focus: FocusHandle,
    popup: bool,
    surface_bounds: Option<Bounds<gpui::Pixels>>,
    owner: gpui::AnyWindowHandle,
    owner_hwnd: isize,
    closing: bool,
    editor_return_epoch: u64,
    loader: Loader,
    path: Option<PathBuf>,
    collection: Option<ImageCollection>,
    serial: u64,
    loading: bool,
    raster_pending: bool,
    error: Option<String>,
    notice: Option<String>,
    image: Option<LoadedImage>,
    animation: Option<PlaybackSnapshot>,
    pages: Option<pages::Info>,
    source_kind: Option<pages::Kind>,
    source_file: Option<crate::file_snapshot::FileSnapshot>,
    file_drag_gesture: Option<file_actions::Gesture>,
    file_drag_request: Option<crate::pin_drag_request::NativeDragRequest>,
    file_mutations: file_dialog::State,
    raster_dialog: svg_dialog::State,
    prefetch_plan: Option<prefetch_control::Plan>,
    prefetch_epoch: u64,
    animation_epoch: u64,
    snapshot_pending: bool,
    show_properties: bool,
    projection: Option<ImageRotation>,
    desired_projection: Option<ImageRotation>,
    original_tiles: Vec<(Rect, Arc<RenderImage>)>,
    rotation_epoch: u64,
    recent: recent::RecentImages,
    show_grid: bool,
    picking: bool,
    /// 顶栏“更多”菜单是否展开。
    more_open: bool,
    hover_point: Option<Point>,
    preview_color: Option<color::PreviewColor>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    retiring_frames: Vec<frame::FrameRef>,
    retired_frames: Vec<frame::FrameRef>,
    viewport: Viewport,
    preview_viewport: Option<crate::image_preview::viewport::PreviewViewport>,
    preview_region: preview_region::State,
    preview_pan: Option<(Point, (f64, f64))>,
    preview_paint_error: Option<String>,
    auto_fit: bool,
    host: (u32, u32),
    pan: Option<(Point, Point)>,
    choosing: bool,
    thumbnail_range: Range<usize>,
    thumbnail_plan: Option<(u64, Range<usize>, u64)>,
    thumbnail_epoch: u64,
    thumbnails: Vec<ThumbnailSlot>,
    thumbnails_paused: bool,
}

impl EventEmitter<PopupCloseRequested> for ImageViewer {}

impl ImageViewer {
    fn new_entity(
        loader: Loader,
        path: Option<PathBuf>,
        popup: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let view = cx.new(|cx| {
            let mut view = ImageViewer {
                focus,
                popup,
                surface_bounds: None,
                owner: window.window_handle(),
                owner_hwnd: crate::gpui_capture::hwnd(window).0 as isize,
                closing: false,
                editor_return_epoch: 0,
                loader,
                path: None,
                collection: None,
                serial: 0,
                loading: false,
                raster_pending: false,
                error: None,
                notice: None,
                image: None,
                animation: None,
                pages: None,
                source_kind: None,
                source_file: None,
                file_drag_gesture: None,
                file_drag_request: None,
                file_mutations: file_dialog::State::default(),
                raster_dialog: svg_dialog::State::default(),
                prefetch_plan: None,
                prefetch_epoch: 0,
                animation_epoch: 0,
                snapshot_pending: false,
                show_properties: false,
                projection: None,
                desired_projection: None,
                original_tiles: Vec::new(),
                rotation_epoch: 0,
                recent: recent::RecentImages::default(),
                show_grid: false,
                picking: false,
                more_open: false,
                hover_point: None,
                preview_color: None,
                retiring: Vec::new(),
                retired: Vec::new(),
                retiring_frames: Vec::new(),
                retired_frames: Vec::new(),
                viewport: Viewport::default(),
                preview_viewport: None,
                preview_region: preview_region::State::default(),
                preview_pan: None,
                preview_paint_error: None,
                auto_fit: true,
                host: (0, 0),
                pan: None,
                choosing: false,
                thumbnail_range: 0..0,
                thumbnail_plan: None,
                thumbnail_epoch: 0,
                thumbnails: Vec::new(),
                thumbnails_paused: false,
            };
            if let Some(path) = path {
                view.open_path(path, true, cx);
            } else {
                view.choose(cx);
            }
            view
        });
        if !popup {
            let weak_close = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                let _ = weak_close.update(cx, |view, _| view.closing = true);
                true
            });
        }
        let weak = view.downgrade();
        cx.spawn(async move |cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(30))
                .await;
            let active = weak.update(cx, |view, cx| {
                if view.closing {
                    return false;
                }
                view.poll(cx);
                true
            });
            if !matches!(active, Ok(true)) {
                break;
            }
        })
        .detach();
        view
    }

    /// 复用宿主窗口的原生弹层；只拥有图像会话，不拥有窗口。
    pub(crate) fn popup(path: PathBuf, window: &mut Window, cx: &mut App) -> Result<Entity<Self>> {
        let loader = Loader::new()?;
        Ok(Self::new_entity(loader, Some(path), true, window, cx))
    }

    pub(crate) fn shutdown_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.editor_return_epoch = self.editor_return_epoch.wrapping_add(1);
        self.cancel_file_drag();
        self.file_mutations.close();
        self.raster_dialog.close();
        self.serial = self.serial.wrapping_add(1);
        self.loader.shutdown();
        self.loading = false;
        self.snapshot_pending = false;
        self.animation = None;
        self.pages = None;
        self.collection = None;
        self.clear_preview_region();
        self.retire_image();
        self.retiring
            .extend(self.original_tiles.drain(..).map(|(_, image)| image));
        for slot in self.thumbnails.drain(..) {
            if let Some(Thumbnail::Ready(image)) = slot.thumbnail {
                self.retiring.push(image);
            }
        }
        let mut images = Vec::new();
        for image in self.retired.drain(..).chain(self.retiring.drain(..)) {
            if !images.iter().any(|previous| Arc::ptr_eq(previous, &image)) {
                images.push(image);
            }
        }
        let mut frames = std::mem::take(&mut self.retired_frames);
        frames.append(&mut self.retiring_frames);
        if !images.is_empty() || !frames.is_empty() {
            // 宿主先用不含弹层的新场景替换旧场景，再释放旧场景仍可能使用的纹理与帧租约。
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |window, _| {
                    for image in images {
                        let _ = window.drop_image(image);
                    }
                    drop(frames);
                    window.refresh();
                });
                window.refresh();
            });
            window.refresh();
        }
        cx.notify();
    }

    fn surface_size(&self, window: &Window) -> gpui::Size<gpui::Pixels> {
        self.surface_bounds
            .map_or_else(|| window.viewport_size(), |bounds| bounds.size)
    }

    fn image_area(&self, window: &Window) -> Rect {
        self.surface_bounds.map_or_else(
            || image_area(window),
            |bounds| image_area_in(bounds, window.scale_factor()),
        )
    }

    fn area_center(&self, window: &Window) -> Point {
        rect_center(self.image_area(window))
    }

    fn set_surface_bounds(
        &mut self,
        bounds: Bounds<gpui::Pixels>,
        dpi: f32,
        cx: &mut Context<Self>,
    ) {
        if !self.popup || self.surface_bounds == Some(bounds) {
            return;
        }
        if let Some(previous) = self.surface_bounds {
            let delta = bounds.origin - previous.origin;
            let dx = f32::from(delta.x) * dpi;
            let dy = f32::from(delta.y) * dpi;
            self.viewport.offset.x += dx;
            self.viewport.offset.y += dy;
            if let Some(viewport) = &mut self.preview_viewport {
                let (x, y) = viewport.offset();
                let _ = viewport.set_offset((x + f64::from(dx), y + f64::from(dy)));
            }
        }
        self.surface_bounds = Some(bounds);
        cx.notify();
    }

    fn modal_is_open(&self) -> bool {
        self.file_mutations.is_open() || self.raster_dialog.is_open()
    }

    fn rasterize_svg(
        &mut self,
        path: PathBuf,
        expected: crate::file_snapshot::FileSnapshot,
        dimensions: (u32, u32),
        cx: &mut Context<Self>,
    ) {
        if self.loading
            || self.snapshot_pending
            || self.path.as_ref() != Some(&path)
            || self.source_file.as_ref() != Some(&expected)
            || !self.image.as_ref().is_some_and(|image| {
                image.metadata.format == crate::image_metadata::SourceFormat::Svg
            })
        {
            return;
        }
        self.invalidate_prefetch();
        self.raster_dialog.close();
        self.cancel_file_drag();
        self.pan = None;
        self.preview_pan = None;
        self.serial = self.serial.wrapping_add(1);
        self.loading = true;
        self.raster_pending = true;
        self.error = None;
        self.notice = None;
        // 保留原色、视口及显示图到成功回执；失败或取消仍能继续查看原来的画布。
        self.loader.request(Request {
            serial: self.serial,
            path,
            scan: false,
            page: None,
            raster: Some(RasterRequest {
                dimensions,
                expected,
            }),
            resident_bytes: self.foreground_resident_bytes(),
        });
        cx.notify();
    }

    fn open_path(&mut self, path: PathBuf, scan: bool, cx: &mut Context<Self>) {
        self.open_path_page(path, scan, None, cx);
    }

    fn open_path_page(
        &mut self,
        path: PathBuf,
        scan: bool,
        page: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.closing || self.file_mutations.busy() {
            return;
        }
        self.file_mutations.close();
        self.raster_dialog.close();
        self.raster_pending = false;
        self.prefetch_plan = None;
        let path = std::path::absolute(&path).unwrap_or(path);
        if page.is_none() || self.path.as_ref() != Some(&path) {
            self.pages = None;
        } else if let (Some(pages), Some(index)) = (&mut self.pages, page) {
            pages.selected = index;
        }
        self.serial = self.serial.wrapping_add(1);
        self.loading = true;
        self.error = None;
        self.notice = None;
        self.preview_color = None;
        self.recent.hide();
        self.pan = None;
        self.preview_pan = None;
        self.preview_viewport = None;
        self.preview_paint_error = None;
        self.clear_preview_region();
        self.auto_fit = true;
        self.animation = None;
        self.source_kind = None;
        self.source_file = None;
        self.cancel_file_drag();
        self.animation_epoch = 0;
        self.snapshot_pending = false;
        self.projection = None;
        self.desired_projection = None;
        self.rotation_epoch = 0;
        self.thumbnail_plan = None;
        self.thumbnails_paused = false;
        self.retire_image();
        self.retiring
            .extend(self.original_tiles.drain(..).map(|(_, image)| image));
        if scan {
            self.collection = None;
            for slot in self.thumbnails.drain(..) {
                if let Some(Thumbnail::Ready(image)) = slot.thumbnail {
                    self.retiring.push(image);
                }
            }
            self.thumbnail_range = 0..0;
        }
        self.loader.request(Request {
            serial: self.serial,
            path: path.clone(),
            scan,
            page,
            raster: None,
            resident_bytes: self.foreground_resident_bytes(),
        });
        self.path = Some(path);
        cx.notify();
    }

    fn retire_image(&mut self) {
        if let Some(image) = self.image.take() {
            if let Some(frame) = image.raw_frame {
                self.retiring_frames.push(frame);
            }
            self.retiring
                .extend(image.tiles.into_iter().map(|(_, image)| image));
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let mut changed = false;
        if let Some(completion) = self.loader.take().filter(|c| c.serial == self.serial) {
            self.thumbnail_plan = None;
            let raster = std::mem::take(&mut self.raster_pending);
            self.loading = false;
            self.animation = completion.animation;
            self.source_kind = completion.source_kind;
            self.source_file = completion.source_file;
            // 同类集合的坏图项仍可沿目录恢复；头部变成别种格式时不保留旧目录。
            let same_page_kind = self
                .pages
                .as_ref()
                .is_some_and(|pages| Some(pages.kind) == completion.source_kind);
            if completion.pages.is_some() || completion.result.is_ok() || !same_page_kind {
                self.pages = completion.pages;
            }
            self.error = completion.warning;
            if let Some(collection) = completion.collection {
                self.collection = Some(collection);
            }
            match completion.result {
                Ok(image) => {
                    self.preview_viewport = image.preview_geometry.and_then(|geometry| {
                        crate::image_preview::viewport::PreviewViewport::new(
                            geometry.normalized_source_dimensions(),
                        )
                        .ok()
                    });
                    if raster {
                        self.auto_fit = true;
                    }
                    if let Some(path) = &self.path {
                        self.recent.record(path);
                    }
                    if image.raw_frame.is_none() {
                        self.original_tiles = image.tiles.clone();
                    }
                    self.desired_projection = image.projection();
                    if !image.has_full_pixels() {
                        self.projection = None;
                        self.picking = false;
                        self.show_grid = false;
                    }
                    if !raster {
                        if let Some(index) = self.collection.as_ref().map(|c| c.position().0 - 1) {
                            self.replace_thumbnail(
                                index,
                                Thumbnail::Ready(image.thumbnail.clone()),
                            );
                        }
                    }
                    if let Some(previous) = self.image.replace(image) {
                        if let Some(frame) = previous.raw_frame {
                            self.retiring_frames.push(frame);
                        }
                        self.retiring
                            .extend(previous.tiles.into_iter().map(|(_, image)| image));
                    }
                }
                Err(error) => {
                    if !raster {
                        if let Some(index) = self.collection.as_ref().map(|c| c.position().0 - 1) {
                            self.replace_thumbnail(index, Thumbnail::Failed);
                        }
                    }
                    self.error = Some(error);
                }
            }
            changed = true;
        }
        if let Some(update) = self
            .loader
            .take_animation()
            .filter(|update| update.serial == self.serial && update.epoch == self.animation_epoch)
        {
            self.thumbnail_plan = None;
            if let Some(image) = update.tick.image {
                self.desired_projection =
                    ImageRotation::new(image.metadata.display_dimensions, image.rotation).ok();
                if let Some(warning) = image.warning.as_ref() {
                    self.error = Some(warning.clone());
                }
                self.retire_image();
                self.image = Some(image);
            }
            self.animation = Some(update.tick.snapshot);
            if let Some(error) = update.tick.error {
                self.error = Some(error);
                self.desired_projection = self.image.as_ref().and_then(|image| {
                    ImageRotation::new(image.metadata.display_dimensions, image.rotation).ok()
                });
            }
            // 背压状态也触发一轮渲染，让旧RenderImage及其帧租约按相同步骤退役。
            changed = true;
        }
        for completion in self.loader.take_thumbnails() {
            if completion.serial == self.serial && completion.epoch == self.thumbnail_epoch {
                self.replace_thumbnail(completion.index, completion.thumbnail);
                changed = true;
            }
        }
        if let Some(completion) = self
            .loader
            .take_snapshot()
            .filter(|c| c.serial == self.serial)
        {
            self.thumbnail_plan = None;
            self.snapshot_pending = false;
            let result = completion
                .result
                .and_then(|output| match (completion.target, output) {
                    (
                        target @ (SnapshotTarget::Editor
                        | SnapshotTarget::Crop
                        | SnapshotTarget::Ocr),
                        SnapshotOutput::Image(image),
                    ) => crate::image_editor::open(image, Some(completion.path.clone()), cx)
                        .and_then(|handle| {
                            // 只在子编辑器实际释放后返回；关闭确认取消不会触发。
                            let weak = cx.weak_entity();
                            let popup = self.popup;
                            let owner = self.owner;
                            let parent_hwnd = self.owner_hwnd;
                            let serial = self.serial;
                            let path = completion.path.clone();
                            let expected = self.source_file.clone();
                            self.editor_return_epoch = self.editor_return_epoch.wrapping_add(1);
                            let epoch = self.editor_return_epoch;
                            handle.update(cx, move |_, child_window, child_cx| {
                                let child_hwnd = crate::gpui_capture::hwnd(child_window).0 as isize;
                                child_cx
                                    .on_release(move |_, app| {
                                        let may_return =
                                            editor_return_has_foreground(child_hwnd, parent_hwnd);
                                        if !may_return {
                                            return;
                                        }
                                        app.defer(move |app| {
                                            if !editor_return_has_foreground(
                                                child_hwnd,
                                                parent_hwnd,
                                            ) {
                                                return;
                                            }
                                            let _ = owner.update(app, |root, window, app| {
                                                if !popup && root.entity_id() != weak.entity_id() {
                                                    return;
                                                }
                                                let _ = weak.update(app, |view, cx| {
                                                    if view.editor_return_epoch != epoch
                                                        || view.serial != serial
                                                        || view.path.as_ref() != Some(&path)
                                                        || view.source_file.as_ref()
                                                            != expected.as_ref()
                                                        || view.closing
                                                        || view.loading
                                                        || view.snapshot_pending
                                                        || view.modal_is_open()
                                                        || view.choosing
                                                        || crate::gpui_capture::hwnd(window).0
                                                            as isize
                                                            != parent_hwnd
                                                        || !editor_return_has_foreground(
                                                            child_hwnd,
                                                            parent_hwnd,
                                                        )
                                                    {
                                                        return;
                                                    }
                                                    window.activate_window();
                                                    window.focus(&view.focus);
                                                    cx.notify();
                                                });
                                            });
                                        });
                                    })
                                    .detach();
                            })?;
                            let action = match target {
                                SnapshotTarget::Crop => Some(crate::capture_view::Action::Tool(
                                    crate::annotation::Tool::Crop,
                                )),
                                SnapshotTarget::Ocr => Some(crate::capture_view::Action::Ocr),
                                _ => None,
                            };
                            if let Some(action) = action {
                                handle.update(cx, |view, window, cx| {
                                    view.perform(action, window, cx)
                                })?;
                            }
                            Ok(())
                        })
                        .map_err(|error| error.to_string()),
                    (SnapshotTarget::Pin { origin }, SnapshotOutput::Image(image)) => {
                        crate::pin_view::PinView::open(image, origin, cx)
                            .map_err(|error| error.to_string())
                    }
                    (SnapshotTarget::Tools, SnapshotOutput::Image(image)) => {
                        crate::image_tool_view::open_with_metadata(
                            image,
                            vec![completion.path],
                            completion.metadata_source,
                            cx,
                        )
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                    }
                    (
                        SnapshotTarget::Clipboard { owner, sequence },
                        SnapshotOutput::Clipboard(prepared),
                    ) => prepared
                        .publish_if_unchanged(
                            windows::Win32::Foundation::HWND(owner as *mut _),
                            sequence,
                        )
                        .map(|_| {
                            self.notice = Some(tr!("已复制源图像素", "Source pixels copied").into())
                        })
                        .map_err(|error| error.to_string()),
                    _ => Err(tr!("图片操作回执不匹配", "Image operation result mismatch").into()),
                });
            if let Err(error) = result {
                self.error = Some(error);
            }
            changed = true;
        }
        if let Some(completion) = self
            .loader
            .take_rotation()
            .filter(|c| c.serial == self.serial && c.epoch == self.rotation_epoch)
        {
            self.thumbnail_plan = None;
            match completion.result {
                Ok(tiles) => {
                    if let Some(image) = self.image.as_mut() {
                        self.retiring.extend(
                            std::mem::replace(&mut image.tiles, tiles)
                                .into_iter()
                                .map(|(_, image)| image),
                        );
                        image.rotation = completion.projection.rotation();
                    }
                }
                Err(error) => {
                    self.error = Some(error);
                    self.desired_projection = self.projection;
                }
            }
            changed = true;
        }
        if changed {
            cx.notify();
        }
        self.poll_preview_region(cx);
    }

    fn replace_thumbnail(&mut self, index: usize, thumbnail: Thumbnail) {
        if let Some(slot) = self.thumbnails.iter_mut().find(|slot| slot.index == index) {
            if matches!((&slot.thumbnail, &thumbnail),
                (Some(Thumbnail::Ready(previous)), Thumbnail::Ready(next))
                    if Arc::ptr_eq(previous, next))
            {
                return;
            }
            if let Some(Thumbnail::Ready(previous)) = slot.thumbnail.replace(thumbnail) {
                self.retiring.push(previous);
            }
        }
    }

    fn animation_control(&mut self, action: animation::Action, cx: &mut Context<Self>) {
        if self.animation.is_none() {
            return;
        }
        if let Some(epoch) = self.loader.animation_control(self.serial, action) {
            self.thumbnail_plan = None;
            self.animation_epoch = epoch;
            self.error = None;
            cx.notify();
        }
    }

    fn prepare_thumbnails(&mut self, width: f32) {
        // 新主图接管前不排旧宿主账本之外的解码；退役图释放后再重建计划。
        if self.loading
            || self.raster_pending
            || self.snapshot_pending
            || self.preview_region.pending()
            || self.thumbnails_paused
            || self
                .desired_projection
                .zip(self.projection)
                .is_some_and(|(desired, presented)| {
                    desired.rotation() as u8 != presented.rotation() as u8
                })
            || !self.retiring.is_empty()
            || !self.retired.is_empty()
            || !self.retiring_frames.is_empty()
            || !self.retired_frames.is_empty()
        {
            self.thumbnail_plan = None;
            self.loader.revoke_thumbnails();
            return;
        }
        let Some(collection) = &self.collection else {
            return;
        };
        let (position, total) = collection.position();
        let current = position - 1;
        let visible = thumbnail_range(total, current, width);
        let buffered = visible.start.saturating_sub(1)..(visible.end + 1).min(total);
        self.thumbnail_range = visible;
        let resident_bytes = self.foreground_resident_bytes();
        if self
            .thumbnail_plan
            .as_ref()
            .is_some_and(|(serial, range, resident)| {
                *serial == self.serial && *range == buffered && *resident == resident_bytes
            })
        {
            return;
        }
        let mut paths: Vec<_> = buffered
            .clone()
            .filter_map(|index| {
                collection
                    .path_at(index)
                    .map(|path| (index, path.to_path_buf()))
            })
            .collect();
        paths.sort_by_key(|(index, _)| index.abs_diff(current));
        self.thumbnail_plan = Some((self.serial, buffered.clone(), resident_bytes));
        self.thumbnail_epoch = self.thumbnail_epoch.wrapping_add(1);
        let mut previous = std::mem::take(&mut self.thumbnails);
        for index in buffered {
            let thumbnail = previous
                .iter_mut()
                .find(|slot| slot.index == index)
                .and_then(|slot| slot.thumbnail.take())
                .or_else(|| {
                    (index == current)
                        .then(|| {
                            self.image
                                .as_ref()
                                .map(|image| Thumbnail::Ready(image.thumbnail.clone()))
                        })
                        .flatten()
                });
            self.thumbnails.push(ThumbnailSlot { index, thumbnail });
        }
        for slot in previous {
            if let Some(Thumbnail::Ready(image)) = slot.thumbnail {
                self.retiring.push(image);
            }
        }
        paths.retain(|(index, _)| {
            !self.thumbnails.iter().any(|slot| {
                slot.index == *index && matches!(&slot.thumbnail, Some(Thumbnail::Ready(_)))
            })
        });
        if !self.thumbnails_paused {
            self.loader.request_thumbnails(ThumbnailRequest {
                serial: self.serial,
                epoch: self.thumbnail_epoch,
                resident_bytes,
                cancel: Arc::new(AtomicBool::new(false)),
                paths: paths.into(),
            });
        }
    }

    fn select_thumbnail(
        &mut self,
        index: usize,
        serial: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_mutations.busy() {
            return;
        }
        window.focus(&self.focus);
        if serial != self.serial {
            return;
        }
        let Some(collection) = self.collection.as_mut() else {
            return;
        };
        if collection.select(index) {
            let path = collection.current().to_path_buf();
            self.open_path(path, false, cx);
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.file_mutations.busy() {
            return;
        }
        self.file_mutations.close();
        self.raster_dialog.close();
        self.raster_pending = false;
        self.cancel_file_drag();
        self.serial = self.serial.wrapping_add(1);
        self.loader.cancel(self.serial);
        self.loading = false;
        self.thumbnails_paused = true;
        self.thumbnail_epoch = self.thumbnail_epoch.wrapping_add(1);
        self.thumbnail_plan = None;
        self.error = Some(tr!("已取消加载", "Loading canceled").into());
        cx.notify();
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        if self.choosing {
            return;
        }
        self.choosing = true;
        self.pan = None;
        self.preview_pan = None;
        self.clear_preview_region();
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("选择图片", "Choose an image").into()),
        });
        cx.spawn(async move |view, cx| {
            let selected = receiver.await;
            let _ = view.update(cx, |view, cx| {
                if view.closing {
                    return;
                }
                view.choosing = false;
                match selected {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.open_path(path, true, cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => view.error = Some(error.to_string()),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn navigate(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.file_mutations.busy() {
            return;
        }
        let Some(collection) = self.collection.as_mut() else {
            return;
        };
        if collection.navigate(delta) {
            let path = collection.current().to_path_buf();
            self.open_path(path, false, cx);
        }
    }

    fn edge(&mut self, last: bool, cx: &mut Context<Self>) {
        if self.file_mutations.busy() {
            return;
        }
        let Some(collection) = self.collection.as_mut() else {
            return;
        };
        let changed = if last {
            collection.last()
        } else {
            collection.first()
        };
        if changed {
            let path = collection.current().to_path_buf();
            self.open_path(path, false, cx);
        }
    }

    fn icon_navigate(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (Some(pages), Some(path)) = (&self.pages, &self.path) else {
            return;
        };
        if let Some(index) = pages
            .selected
            .checked_add_signed(delta)
            .filter(|index| *index < pages.count())
        {
            self.open_path_page(path.clone(), false, Some(index), cx);
        }
    }

    fn fit(&mut self, window: &Window) {
        let area = self.image_area(window);
        if let Some(image) = &self.image {
            if let Some(viewport) = &mut self.preview_viewport {
                if let Err(error) = viewport.fit(area) {
                    self.notice = Some(error.to_string());
                }
            } else {
                let (width, height) = image.logical_display_dimensions();
                self.viewport.fit(Rect::full(width, height), area);
            }
        }
    }

    fn displayed_zoom(&self) -> f64 {
        self.preview_viewport
            .map_or(f64::from(self.viewport.zoom), |viewport| viewport.zoom())
    }

    fn begin_pan(&mut self, position: Point) {
        self.pan = None;
        self.preview_pan = None;
        if let Some(viewport) = self.preview_viewport {
            self.preview_pan = Some((position, viewport.offset()));
        } else {
            self.pan = Some((position, self.viewport.offset));
        }
        self.auto_fit = false;
    }

    fn zoom(&mut self, zoom: f64, anchor: Point) {
        if self.image.is_some() {
            self.auto_fit = false;
            if let Some(viewport) = &mut self.preview_viewport {
                if let Err(error) =
                    viewport.set_zoom(zoom, (f64::from(anchor.x), f64::from(anchor.y)))
                {
                    self.notice = Some(error.to_string());
                }
            } else {
                self.viewport.set_zoom(zoom as f32, anchor);
            }
        }
    }

    fn snapshot(&mut self, target: SnapshotTarget, cx: &mut Context<Self>) {
        if self.loading || self.snapshot_pending {
            return;
        }
        if self
            .image
            .as_ref()
            .is_some_and(|image| !image.has_full_pixels())
        {
            self.notice = Some(
                tr!(
                    "当前只有采样预览，无法提供完整图像像素",
                    "Only a sampled preview is available; full image pixels are unavailable"
                )
                .into(),
            );
            cx.notify();
            return;
        }
        self.invalidate_prefetch();
        let (Some(image), Some(path)) = (&self.image, &self.path) else {
            return;
        };
        let mut resident_bytes = self.resident_bytes(image);
        // 工作线程可能正在生成尚未交付的一次旋转；旧显示副本会保留到下一次绘制。
        if self.animation.is_none()
            && self
                .desired_projection
                .is_some_and(|desired| desired.rotation() as u8 != image.rotation as u8)
        {
            resident_bytes = resident_bytes.saturating_add(
                u64::from(image.metadata.display_dimensions.0)
                    .saturating_mul(u64::from(image.metadata.display_dimensions.1))
                    .saturating_mul(4),
            );
        }
        let request = SnapshotRequest {
            serial: self.serial,
            path: path.clone(),
            frame: image.raw_frame.clone(),
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes,
            target,
        };
        if self.animation.is_some() {
            self.animation_control(animation::Action::Pause, cx);
        }
        self.snapshot_pending = true;
        self.loader.snapshot(request);
        cx.notify();
    }

    fn edit(&mut self, cx: &mut Context<Self>) {
        self.snapshot(SnapshotTarget::Editor, cx);
    }

    fn copy_image(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.snapshot(
            SnapshotTarget::Clipboard {
                owner: crate::gpui_capture::hwnd(window).0 as isize,
                sequence: unsafe {
                    windows::Win32::System::DataExchange::GetClipboardSequenceNumber()
                },
            },
            cx,
        );
    }

    fn pin(&mut self, cx: &mut Context<Self>) {
        let mut cursor = windows::Win32::Foundation::POINT::default();
        if let Err(error) =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) }
        {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        self.snapshot(
            SnapshotTarget::Pin {
                origin: (cursor.x, cursor.y),
            },
            cx,
        );
    }

    fn copy_path(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = &self.path {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                path.to_string_lossy().into_owned(),
            ));
            self.notice = Some(tr!("已复制路径", "Path copied").into());
            cx.notify();
        }
    }

    fn update_color(&mut self, window: &Window) {
        self.preview_color = if self.picking {
            self.image
                .as_ref()
                .filter(|image| image.has_full_pixels())
                .zip(self.hover_point)
                .and_then(|(image, point)| {
                    let projection =
                        ImageRotation::new(image.metadata.display_dimensions, image.rotation)
                            .ok()?;
                    color::sample(
                        &image.tiles,
                        self.viewport,
                        projection,
                        self.image_area(window),
                        point,
                    )
                })
        } else {
            None
        };
    }

    fn copy_color(&mut self, cx: &mut Context<Self>) {
        if let Some(color) = self.preview_color.filter(|_| self.picking) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(color.hex()));
            self.notice = Some(
                tr!(
                    "已复制预览合成色（含透明背景）",
                    "Preview color copied (includes transparency background)"
                )
                .into(),
            );
            cx.notify();
        }
    }

    fn file_action(&mut self, copy: bool, window: &Window, cx: &mut Context<Self>) {
        let Some(path) = &self.path else {
            return;
        };
        let result =
            crate::image_collection::ensure_local_file(path).and_then(|()| {
                if copy {
                    if self.source_file.as_ref().is_none_or(|expected| {
                        source_file_snapshot(path).as_ref() != Some(expected)
                    }) {
                        return Err(tr!(
                        "源文件已变化或身份不可用，请重新加载后复制文件",
                        "Source changed or cannot be identified; reload before copying the file"
                    )
                        .into());
                    }
                    crate::recording_view::copy_file_to_clipboard(
                        crate::gpui_capture::hwnd(window),
                        path,
                    )
                } else {
                    crate::library::reveal(path)
                }
                .map_err(|error| error.to_string())
            });
        match result {
            Ok(()) => {
                self.notice = Some(
                    if copy {
                        tr!("已复制文件引用", "File reference copied")
                    } else {
                        tr!("已在资源管理器中定位", "Shown in Explorer")
                    }
                    .into(),
                )
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn resident_bytes(&self, image: &LoadedImage) -> u64 {
        presentation_resident_bytes(
            image,
            self.original_tiles
                .iter()
                .map(|(_, image)| image)
                .chain(self.retiring.iter())
                .chain(self.retired.iter())
                .chain(
                    self.preview_region
                        .surface()
                        .into_iter()
                        .map(|surface| &surface.image),
                )
                .chain(
                    self.thumbnails
                        .iter()
                        .filter_map(|slot| match &slot.thumbnail {
                            Some(Thumbnail::Ready(image)) => Some(image),
                            _ => None,
                        }),
                ),
            self.retiring_frames
                .iter()
                .chain(self.retired_frames.iter()),
        )
        .saturating_add(self.pages.as_ref().map_or(0, pages::Info::retained_bytes))
    }

    fn rotate(&mut self, clockwise: bool, cx: &mut Context<Self>) {
        if self.loading
            || self
                .image
                .as_ref()
                .is_some_and(|image| !image.has_full_pixels())
        {
            return;
        }
        self.invalidate_prefetch();
        self.thumbnail_plan = None;
        if self.animation.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.state,
                PlaybackState::Failed | PlaybackState::Cancelled
            )
        }) {
            return;
        }
        let Some(projection) = self.desired_projection.or(self.projection) else {
            return;
        };
        let target = projection.turned(clockwise);
        self.desired_projection = Some(target);
        self.error = None;
        if self.animation.is_some() {
            self.animation_control(animation::Action::Rotate(target.rotation()), cx);
            return;
        }
        let resident_bytes = self
            .image
            .as_ref()
            .map_or(THUMBNAIL_CACHE_BYTES as u64, |image| {
                self.resident_bytes(image)
            });
        self.rotation_epoch = self.rotation_epoch.wrapping_add(1);
        self.loader.rotate(RotationRequest {
            serial: self.serial,
            epoch: self.rotation_epoch,
            projection: target,
            tiles: self.original_tiles.clone(),
            resident_bytes,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        cx.notify();
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.svg_dialog_key(event, window, cx) || self.file_dialog_key(event, window, cx) {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        let plain =
            !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function;
        let control = modifiers.control && !modifiers.alt && !modifiers.platform;
        let key = event.keystroke.key.as_str();
        if plain
            && key == "escape"
            && (self.file_drag_gesture.is_some() || self.file_drag_request.is_some())
        {
            self.cancel_file_drag();
            self.notice = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if event.is_held
            && matches!(
                key,
                "escape" | "f11" | "e" | "o" | "space" | "r" | "i" | "c" | "x" | "p" | "g" | "k"
            )
        {
            return;
        }
        if plain && self.recent.is_open() {
            let handled = match key {
                "up" => {
                    self.recent.move_selection(-1);
                    true
                }
                "down" => {
                    self.recent.move_selection(1);
                    true
                }
                "home" => {
                    self.recent.move_selection(isize::MIN);
                    true
                }
                "end" => {
                    self.recent.move_selection(isize::MAX);
                    true
                }
                "enter" => {
                    if let Some(path) = self.recent.selected_path() {
                        self.open_path(path, true, cx);
                    }
                    true
                }
                "escape" => {
                    self.recent.hide();
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        match key {
            "r" if control => self.recent.toggle(),
            "i" if plain => self.show_properties = !self.show_properties,
            "g" if plain
                && self
                    .image
                    .as_ref()
                    .is_some_and(LoadedImage::has_full_pixels) =>
            {
                self.show_grid = !self.show_grid
            }
            "k" if plain
                && self
                    .image
                    .as_ref()
                    .is_some_and(LoadedImage::has_full_pixels) =>
            {
                self.picking = !self.picking
            }
            "c" if plain && self.picking => self.copy_color(cx),
            "x" if plain => self.snapshot(SnapshotTarget::Crop, cx),
            "o" if plain => self.snapshot(SnapshotTarget::Ocr, cx),
            "p" if plain => self.pin(cx),
            "c" if control && modifiers.shift => self.copy_path(cx),
            "c" if control && self.image.is_some() => self.copy_image(window, cx),
            "[" if plain => self.rotate(false, cx),
            "]" if plain => self.rotate(true, cx),
            "space" if plain && self.animation.is_some() => {
                self.animation_control(animation::Action::Toggle, cx)
            }
            "." if plain && self.animation.is_some() => {
                self.animation_control(animation::Action::Step, cx)
            }
            "r" if plain && self.animation.is_some() => {
                self.animation_control(animation::Action::Restart, cx)
            }
            "enter" if plain && self.animation.is_some() => {
                self.animation_control(animation::Action::Input, cx)
            }
            "pageup" if control && self.pages.is_some() => self.icon_navigate(-1, cx),
            "pagedown" if control && self.pages.is_some() => self.icon_navigate(1, cx),
            "left" | "pageup" if plain => self.navigate(-1, cx),
            "right" | "pagedown" if plain => self.navigate(1, cx),
            "home" if plain => self.edge(false, cx),
            "end" if plain => self.edge(true, cx),
            "0" if plain || control => {
                self.auto_fit = true;
                self.fit(window);
            }
            "1" if plain || control => self.zoom(1., self.area_center(window)),
            "+" | "=" if plain || control => {
                self.zoom(self.displayed_zoom() * 1.2, self.area_center(window))
            }
            "-" if plain || control => {
                self.zoom(self.displayed_zoom() / 1.2, self.area_center(window))
            }
            "f11" if plain && !self.popup => window.toggle_fullscreen(),
            "escape" if plain && !self.popup && window.is_fullscreen() => {
                window.toggle_fullscreen()
            }
            "escape" if plain && self.more_open => self.more_open = false,
            "escape" if plain && self.loading && !self.popup => self.cancel(cx),
            "escape" if plain && self.recent.is_open() => self.recent.hide(),
            "escape" if plain && self.show_properties => self.show_properties = false,
            "escape" if plain => {
                if self.popup {
                    cx.emit(PopupCloseRequested);
                } else {
                    self.closing = true;
                    window.remove_window();
                }
            }
            "o" if control => self.choose(cx),
            "e" if plain && self.image.is_some() => self.edit(cx),
            "f5" if plain => {
                if let Some(path) = self.path.clone() {
                    self.open_path(path, true, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn thumbnail_strip(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let palette = crate::theme::palette(cx);
        let count = self.thumbnail_range.len();
        let width = f32::from(self.surface_size(window).width);
        let cell_width = ((width - 24. - 6. * count.saturating_sub(1) as f32)
            / count.max(1) as f32)
            .clamp(1., 88.);
        let current = self
            .collection
            .as_ref()
            .map(|collection| collection.position().0 - 1);
        let mut strip = div()
            .absolute()
            .bottom(px(32.))
            .left_0()
            .w_full()
            .h(px(THUMBNAIL_STRIP_HEIGHT))
            .px(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .bg(rgb(palette.background))
            .border_t_1()
            .border_color(rgb(palette.border))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        for index in self.thumbnail_range.clone() {
            let Some(path) = self
                .collection
                .as_ref()
                .and_then(|collection| collection.path_at(index))
            else {
                continue;
            };
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let thumbnail = self
                .thumbnails
                .iter()
                .find(|slot| slot.index == index)
                .and_then(|slot| slot.thumbnail.as_ref());
            let serial = self.serial;
            let mut cell = div()
                .id(("image-thumbnail", index))
                .w(px(cell_width))
                .h(px(88.))
                .p(px(3.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .rounded(px(5.))
                .border_2()
                .border_color(rgb(if current == Some(index) {
                    palette.accent
                } else {
                    palette.border
                }))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(palette.hover)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        view.select_thumbnail(index, serial, window, cx);
                    }),
                );
            if let Some(Thumbnail::Ready(image)) = thumbnail {
                let image = image.clone();
                cell = cell.child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            let dimensions = image.size(0);
                            let (width, height) =
                                (dimensions.width.0 as f32, dimensions.height.0 as f32);
                            let zoom = (f32::from(bounds.size.width) / width)
                                .min(f32::from(bounds.size.height) / height);
                            let target = size(px(width * zoom), px(height * zoom));
                            let origin = bounds.origin
                                + point(
                                    (bounds.size.width - target.width) / 2.,
                                    (bounds.size.height - target.height) / 2.,
                                );
                            let _ = window.paint_image(
                                Bounds::new(origin, target),
                                px(0.).into(),
                                image.clone(),
                                0,
                                false,
                            );
                        },
                    )
                    .w_full()
                    .h(px(58.)),
                );
            } else {
                cell = cell.child(
                    div()
                        .w_full()
                        .h(px(58.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(palette.muted))
                        .child(if matches!(thumbnail, Some(Thumbnail::Failed)) {
                            "×"
                        } else {
                            "…"
                        }),
                );
            }
            strip =
                strip.child(cell.child(div().w_full().truncate().text_size(px(10.)).child(name)));
        }
        strip.into_any_element()
    }
}

fn client(point: gpui::Point<gpui::Pixels>, window: &Window) -> Point {
    Point {
        x: f32::from(point.x) * window.scale_factor(),
        y: f32::from(point.y) * window.scale_factor(),
    }
}

fn image_area(window: &Window) -> Rect {
    image_area_in(
        Bounds::new(point(px(0.), px(0.)), window.viewport_size()),
        window.scale_factor(),
    )
}

fn image_area_in(bounds: Bounds<gpui::Pixels>, dpi: f32) -> Rect {
    let x = (f32::from(bounds.origin.x) * dpi).round().max(0.) as u32;
    let y = (f32::from(bounds.origin.y) * dpi).round().max(0.) as u32;
    let width = (f32::from(bounds.size.width) * dpi).round().max(1.) as u32;
    let height = (f32::from(bounds.size.height) * dpi).round().max(1.) as u32;
    let margin = (12. * dpi).round() as u32;
    let top = ((toolbar::TOOLBAR_HEIGHT + 12.) * dpi).round() as u32;
    let bottom = ((44. + THUMBNAIL_STRIP_HEIGHT) * dpi).round() as u32;
    Rect {
        x: x.saturating_add(margin.min(width / 4)),
        y: y.saturating_add(top.min(height / 3)),
        width: width.saturating_sub(margin * 2).max(1),
        height: height.saturating_sub(top + bottom).max(1),
    }
}

#[cfg(test)]
fn area_center(window: &Window) -> Point {
    rect_center(image_area(window))
}

fn rect_center(area: Rect) -> Point {
    Point {
        x: area.x as f32 + area.width as f32 / 2.,
        y: area.y as f32 + area.height as f32 / 2.,
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.popup {
            crate::theme::sync_window(window, cx);
        }
        for image in self.retired.drain(..) {
            // LRU命中或窗口变窄后仍在使用的缩略图不能随旧条带一起失效。
            let visible = self.thumbnails.iter().any(|slot| {
                matches!(&slot.thumbnail,
                Some(Thumbnail::Ready(current)) if Arc::ptr_eq(current, &image))
            });
            let visible = visible
                || self
                    .preview_region
                    .surface()
                    .is_some_and(|surface| Arc::ptr_eq(&surface.image, &image))
                || self.image.as_ref().is_some_and(|current| {
                    current
                        .tiles
                        .iter()
                        .any(|(_, current)| Arc::ptr_eq(current, &image))
                });
            if !visible {
                let _ = window.drop_image(image);
            }
        }
        self.retired.append(&mut self.retiring);
        self.retired_frames.clear();
        self.retired_frames.append(&mut self.retiring_frames);
        self.prepare_thumbnails(f32::from(self.surface_size(window).width));
        // 静态、暂停和EOF没有后续解码交付；只为尚待退役的一代再画一次，清空后停止。
        if !self.retired.is_empty()
            || !self.retired_frames.is_empty()
            || !self.retiring.is_empty()
            || !self.retiring_frames.is_empty()
        {
            cx.notify();
        }
        let image_area = self.image_area(window);
        if let Some(image) = &self.image {
            if let Some(next) = image.projection() {
                if let Some(previous) = self.projection {
                    if previous.rotation() as u8 != next.rotation() as u8 {
                        match previous.reorient_viewport(
                            next.rotation(),
                            &mut self.viewport,
                            image_area,
                            self.auto_fit,
                        ) {
                            Ok(projection) => self.projection = Some(projection),
                            Err(error) => self.error = Some(error),
                        }
                    }
                } else {
                    self.projection = Some(next);
                }
            }
        }
        let viewport_size = self.surface_size(window);
        let host = (
            f32::from(viewport_size.width).round() as u32,
            f32::from(viewport_size.height).round() as u32,
        );
        if self.auto_fit || self.host != host {
            if self.auto_fit {
                self.fit(window);
            }
            self.host = host;
        }
        self.update_color(window);
        let title = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| {
                format!(
                    "{} - {}",
                    name.to_string_lossy(),
                    brand_title!("图片查看", "Image viewer")
                )
            })
            .unwrap_or_else(|| brand_title!("图片查看", "Image viewer").into());
        if !self.popup {
            window.set_window_title(&title);
        }
        let palette = crate::theme::palette(cx);
        let mut root = div()
            .id("image-viewer")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .text_size(px(12.))
            .font_family("Microsoft YaHei UI")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, e: &gpui::MouseDownEvent, w, cx| {
                    w.focus(&view.focus);
                    let position = client(e.position, w);
                    if view.image.is_some()
                        && view
                            .image_area(w)
                            .contains((position.x.floor() as i32, position.y.floor() as i32))
                    {
                        view.begin_pan(position);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|view, e: &gpui::MouseDownEvent, w, _| {
                    let position = client(e.position, w);
                    if view.image.is_some()
                        && view
                            .image_area(w)
                            .contains((position.x.floor() as i32, position.y.floor() as i32))
                    {
                        view.begin_pan(position);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|view, e: &gpui::MouseMoveEvent, w, cx| {
                if view.modal_is_open() {
                    return;
                }
                if view.move_file_drag(e, w, cx) {
                    return;
                }
                view.hover_point = Some(client(e.position, w));
                if view.picking {
                    let previous = view.preview_color;
                    view.update_color(w);
                    if previous != view.preview_color {
                        cx.notify();
                    }
                }
                if !matches!(
                    e.pressed_button,
                    Some(MouseButton::Left | MouseButton::Middle)
                ) {
                    view.pan = None;
                    view.preview_pan = None;
                    return;
                }
                if let Some((start, offset)) = view.preview_pan {
                    let now = client(e.position, w);
                    if let Some(viewport) = &mut view.preview_viewport {
                        if let Err(error) = viewport.set_offset((
                            offset.0 + f64::from(now.x) - f64::from(start.x),
                            offset.1 + f64::from(now.y) - f64::from(start.y),
                        )) {
                            view.notice = Some(error.to_string());
                        }
                    }
                    cx.notify();
                } else if let Some((start, offset)) = view.pan {
                    let now = client(e.position, w);
                    view.viewport.offset = Point {
                        x: offset.x + now.x - start.x,
                        y: offset.y + now.y - start.y,
                    };
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, _| {
                    view.pan = None;
                    view.preview_pan = None;
                    view.file_drag_gesture = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, _| {
                    view.pan = None;
                    view.preview_pan = None;
                    view.file_drag_gesture = None;
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|view, _, _, _| {
                    view.pan = None;
                    view.preview_pan = None;
                }),
            )
            .on_scroll_wheel(cx.listener(|view, e: &gpui::ScrollWheelEvent, w, cx| {
                let delta = match e.delta {
                    ScrollDelta::Lines(p) => p.y * 48.,
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                };
                view.zoom(
                    view.displayed_zoom() * (f64::from(delta) / 400.).exp(),
                    client(e.position, w),
                );
                cx.notify();
            }));
        if self.popup {
            let weak = cx.weak_entity();
            root = root.child(
                canvas(
                    move |bounds, window, cx| {
                        let dpi = window.scale_factor();
                        let _ =
                            weak.update(cx, |view, cx| view.set_surface_bounds(bounds, dpi, cx));
                    },
                    |_, _, _, _| (),
                )
                .absolute()
                .size_full(),
            );
        }
        if let Some(image) = &self.image {
            if let Some(frame) = &image.raw_frame {
                self.loader
                    .presented(self.serial, self.animation_epoch, frame.generation());
            }
            let viewport = self.viewport;
            let dpi = window.scale_factor();
            let tiles = image.tiles.clone();
            let show_grid = self.show_grid && image.has_full_pixels();
            let dimensions = image.logical_display_dimensions();
            let raster_dimensions = image.rotation.size(
                image.metadata.display_dimensions.0,
                image.metadata.display_dimensions.1,
            );
            let scale_x = dimensions.0 as f32 / raster_dimensions.0 as f32;
            let scale_y = dimensions.1 as f32 / raster_dimensions.1 as f32;
            let preview =
                image
                    .preview_geometry
                    .zip(self.preview_viewport)
                    .map(|(geometry, viewport)| {
                        (geometry, viewport, self.preview_region.surface().cloned())
                    });
            let checker_colors = if crate::theme::is_light(cx) {
                (0xe0e0e0, 0xc0c0c0)
            } else {
                (palette.background, palette.hover)
            };
            let preview_paint_error = self.preview_paint_error.clone();
            let weak_preview = cx.weak_entity();
            let preview_serial = self.serial;
            let image_area = self.image_area(window);
            root = root.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        if let Some((geometry, viewport, detail)) = &preview {
                            if let Some((_, overview)) = tiles.first() {
                                let error = preview_paint::paint(
                                    overview,
                                    *geometry,
                                    *viewport,
                                    detail.as_ref(),
                                    image_area,
                                    dpi,
                                    checker_colors,
                                    window,
                                )
                                .err();
                                if error != preview_paint_error {
                                    let weak = weak_preview.clone();
                                    let expected_viewport = *viewport;
                                    window.defer(cx, move |_, cx| {
                                        let _ = weak.update(cx, |view, cx| {
                                            if view.serial == preview_serial
                                                && view.preview_viewport == Some(expected_viewport)
                                                && view.preview_paint_error != error
                                            {
                                                view.preview_paint_error = error;
                                                cx.notify();
                                            }
                                        });
                                    });
                                }
                            }
                            return;
                        }
                        for (rect, image) in &tiles {
                            let p = viewport.window_point(Point {
                                x: rect.x as f32 * scale_x,
                                y: rect.y as f32 * scale_y,
                            });
                            let bounds = Bounds::new(
                                point(px(p.x / dpi), px(p.y / dpi)),
                                size(
                                    px(rect.width as f32 * scale_x * viewport.zoom / dpi),
                                    px(rect.height as f32 * scale_y * viewport.zoom / dpi),
                                ),
                            );
                            if (viewport.zoom * scale_x - 1.).abs() < f32::EPSILON
                                && (viewport.zoom * scale_y - 1.).abs() < f32::EPSILON
                            {
                                let _ = window.paint_image_nearest_exact(
                                    bounds,
                                    px(0.).into(),
                                    image.clone(),
                                    0,
                                    false,
                                );
                            } else {
                                // 与网格/取色共用精确物理边界，分块不能各自floor原点或ceil尺寸。
                                let _ = window.paint_image_exact(
                                    bounds,
                                    px(0.).into(),
                                    image.clone(),
                                    0,
                                    false,
                                );
                            }
                        }
                        if show_grid {
                            grid::paint(viewport, dimensions, image_area, dpi, window);
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        } else {
            let message = if self.loading {
                if self.popup {
                    tr!("正在加载…", "Loading…").to_string()
                } else {
                    tr!("正在加载…（Esc 取消）", "Loading… (Esc to cancel)").to_string()
                }
            } else {
                self.error
                    .clone()
                    .unwrap_or_else(|| tr!("打开一张图片开始查看", "Open an image to begin").into())
            };
            root = root.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(32.))
                    .child(message),
            );
        }
        let position = self.collection.as_ref().map(|c| c.position()).unwrap_or((
            usize::from(self.path.is_some()),
            usize::from(self.path.is_some()),
        ));
        let mut status = if let Some(image) = &self.image {
            let (width, height) = image.logical_display_dimensions();
            let format = if self
                .pages
                .as_ref()
                .is_some_and(|pages| pages.kind == pages::Kind::Cursor)
            {
                "CUR"
            } else {
                image.metadata.format_name()
            };
            format!(
                "{} / {} · {} × {} · {} · {:.0}%",
                position.0,
                position.1,
                width,
                height,
                format,
                self.displayed_zoom() * 100.
            )
        } else {
            format!("{} / {}", position.0, position.1)
        };
        if let Some(geometry) = self.image.as_ref().and_then(|image| image.preview_geometry) {
            let (width, height) = geometry.raster_dimensions();
            status.push_str(&trf!(
                " · 采样预览 {width}×{height} · 100%按原图坐标",
                " · Sampled preview {width}×{height} · 100% uses source coordinates"
            ));
            if self.preview_region.pending() {
                status.push_str(tr!(
                    " · 细节更新中（可能仍显示旧区域）",
                    " · Updating detail (previous region may remain)"
                ));
            } else if let Some(surface) = self.preview_region.surface() {
                let region = surface.geometry.normalized_source_region();
                let raster = surface.geometry.raster_dimensions();
                status.push_str(if raster == (region.width, region.height) {
                    tr!(" · 区域1:1采样", " · Region sampled 1:1")
                } else {
                    tr!(" · 区域仍经采样", " · Region downsampled")
                });
            }
        }
        if let Some(color) = self.preview_color {
            let hex = color.hex();
            let (x, y) = color.source;
            status.push_str(&trf!(
                " · 预览合成 {hex} · 归一源({x},{y}) · C复制",
                " · Preview RGB {hex} · Source({x},{y}) · C to copy"
            ));
        } else if self.picking {
            status.push_str(tr!(
                " · 预览合成RGB（含透明背景）· 悬停取色",
                " · Preview RGB (includes background) · Hover to sample"
            ));
        }
        let warning = self
            .preview_paint_error
            .as_deref()
            .or(self.preview_region.error())
            .or(self.error.as_deref().filter(|_| self.image.is_some()))
            .or(self.notice.as_deref())
            .or_else(|| self.collection.as_ref().and_then(|c| c.scan_warning()))
            .unwrap_or("");
        if self.show_properties {
            let mut rows = properties::rows(
                self.path.as_deref(),
                self.image.as_ref().map(|image| image.metadata),
                self.animation.as_ref(),
                self.image
                    .as_ref()
                    .and_then(|image| image.raw_frame.as_deref()),
                self.pages
                    .as_ref()
                    .map(|pages| pages.kind)
                    .or(self.source_kind)
                    .map(|kind| kind.format_name()),
                self.image
                    .as_ref()
                    .and_then(|image| image.metadata_source.as_deref()),
            );
            if let Some(pages) = &self.pages {
                rows.extend(properties::collection_rows(pages));
            }
            if let Some(image) = &self.image {
                if let Some(geometry) = image.preview_geometry {
                    preview::property_rows(&mut rows, geometry);
                }
                let (width, height) = image.logical_display_dimensions();
                rows.insert(
                    4,
                    (
                        tr!("当前显示尺寸", "Current display dimensions"),
                        format!("{width} × {height} px"),
                    ),
                );
                rows.insert(
                    5,
                    (
                        tr!("查看旋转", "Viewing rotation"),
                        format!(
                            "{}°",
                            match image.rotation {
                                Rotation::Identity => 0,
                                Rotation::R90 => 90,
                                Rotation::R180 => 180,
                                Rotation::R270 => 270,
                            }
                        ),
                    ),
                );
            }
            let mut panel = div()
                .id("image-viewer-properties")
                .absolute()
                .right(px(12.))
                .top(px(toolbar::TOOLBAR_HEIGHT + 4.))
                .w(px(320.))
                .max_h(px(440.))
                .overflow_y_scroll()
                .p(px(12.))
                .rounded(px(6.))
                .bg(rgb(palette.background))
                .border_1()
                .border_color(rgb(palette.border))
                .flex()
                .flex_col()
                .gap(px(7.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
            for (label, value) in rows {
                panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_color(rgb(palette.muted)).child(label))
                        .child(div().text_size(px(11.)).child(value)),
                );
            }
            root = root.child(panel);
        }
        let mut root = root
            .child(self.recent.render(cx))
            .child(self.controls(cx))
            .child(self.thumbnail_strip(window, cx))
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .bg(rgb(palette.background))
                    .text_color(rgb(palette.muted))
                    .child(status)
                    .child(warning.to_owned()),
            );
        if let Some(menu) = self.more_menu(cx) {
            root = root.child(menu);
        }
        if let Some(dialog) = self.file_dialog(window, cx) {
            root = root.child(dialog);
        }
        if let Some(dialog) = self.svg_dialog(window, cx) {
            root = root.child(dialog);
        }
        self.refresh_prefetch();
        self.prepare_preview_region(window);
        root
    }
}

fn open_viewer(
    loader: Loader,
    path: Option<PathBuf>,
    options: WindowOptions,
    cx: &mut App,
) -> Result<gpui::WindowHandle<ImageViewer>> {
    Ok(cx.open_window(options, move |window, cx| {
        ImageViewer::new_entity(loader, path, false, window, cx)
    })?)
}

pub fn run(path: Option<PathBuf>) -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let loader = Loader::new()?;
    let errors = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let final_errors = errors.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty()
                    && !crate::pin_workspace_runtime::is_restoring(cx)
                    && !crate::pin_live_view::has_open_windows()
                {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1000.), px(720.)), cx);
            let result = open_viewer(
                loader,
                path,
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions::default()),
                    focus: true,
                    ..Default::default()
                },
                cx,
            );
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

#[cfg(test)]
mod thumbnail_tests {
    use super::*;

    fn image(width: u32, height: u32, bgra: Vec<u8>) -> crate::capture::Image {
        crate::capture::Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        }
    }

    fn key(index: usize) -> ThumbnailKey {
        ThumbnailKey {
            path: format!("{index}.png").into(),
            source_file: Some(crate::file_snapshot::FileSnapshot {
                volume: 1,
                id: [0; 16],
                created: 0,
                written: 0,
                bytes: 4,
                attributes: 0,
            }),
        }
    }

    #[test]
    fn image_viewer_thumbnail_window_contains_selection_and_stays_bounded() {
        assert_eq!(thumbnail_range(200, 100, 1000.), 96..105);
        assert_eq!(thumbnail_range(200, 0, 1000.), 0..9);
        assert_eq!(thumbnail_range(200, 199, 1000.), 191..200);
        assert_eq!(thumbnail_range(0, 0, 1000.), 0..0);
        for total in [1, 3, 17, 20_000] {
            for selected in [0, total / 2, total - 1, usize::MAX] {
                for width in [1., 280., 500., 1000., 2400.] {
                    let range = thumbnail_range(total, selected, width);
                    assert!(range.contains(&selected.min(total - 1)));
                    assert!(range.len() <= 11 && range.end <= total);
                    let buffered = range.start.saturating_sub(1)..(range.end + 1).min(total);
                    assert!(buffered.len() <= MAX_THUMBNAIL_JOBS);
                }
            }
        }
    }

    #[test]
    fn image_viewer_thumbnail_alpha_keeps_opaque_color_and_source_pixels() {
        let pixels = [255, 255, 255, 255, 0, 0, 255, 0].repeat(256);
        let source = image(512, 1, pixels.clone());
        let small = thumbnail_pixels(&source).unwrap();
        assert_eq!((small.width, small.height), (256, 1));
        for pixel in small.bgra.chunks_exact(4) {
            assert_eq!(&pixel[..3], &[255, 255, 255]);
            assert!((127..=128).contains(&pixel[3]));
        }
        assert_eq!(source.bgra, pixels);
        let tiny = thumbnail_pixels(&image(1, 1, vec![17, 29, 53, 255])).unwrap();
        assert_eq!((tiny.width, tiny.height), (1, 1));
        assert_eq!(tiny.bgra, [17, 29, 53, 255]);
    }

    #[test]
    fn image_viewer_thumbnail_area_accumulator_handles_wide_source_coverage() {
        let source = image(300, 300, vec![255; 300 * 300 * 4]);
        // 一个输出格覆盖的有效16位通道总和超过u32；预乘视图用u64累加。
        let small = image::imageops::thumbnail(&PremultipliedImage(&source), 1, 1);
        assert_eq!(small.get_pixel(0, 0).0, [65535; 4]);
    }

    #[test]
    fn image_viewer_thumbnail_lru_and_negative_cache_share_item_budget() {
        let mut cache = ThumbnailCache::default();
        for index in 0..THUMBNAIL_CACHE_ITEMS {
            cache.insert(key(index), Thumbnail::Failed);
        }
        assert!(matches!(cache.get(&key(0)), Some(Thumbnail::Failed)));
        cache.insert(key(THUMBNAIL_CACHE_ITEMS), Thumbnail::Failed);
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(0)).is_some());
        assert_eq!(cache.entries.len(), THUMBNAIL_CACHE_ITEMS);
        let mut changed = key(0);
        changed.source_file.as_mut().unwrap().bytes = 8;
        assert!(cache.get(&changed).is_none());
        cache.insert(changed, Thumbnail::Failed);
        assert!(cache.get(&key(0)).is_none());
        assert_eq!(cache.entries.len(), THUMBNAIL_CACHE_ITEMS);
    }

    #[test]
    fn image_viewer_thumbnail_replaced_file_with_same_stamp_misses_cached_pixels() {
        use std::fs;
        let temporary = snapshot_tests::TempDir::new();
        let path = temporary.path.join("source.png");
        fs::write(&path, snapshot_tests::static_png((1, 1), &[255, 0, 0, 255])).unwrap();
        let stamp = file_stamp(&path).unwrap();
        let original = ThumbnailKey::new(&path);
        assert!(original.source_file.is_some());
        let mut cache = ThumbnailCache::default();
        cache.insert(original.clone(), Thumbnail::Failed);
        // 保留旧对象以免文件系统复用ID；新图恢复相同长度和写入时间。
        fs::rename(&path, temporary.path.join("previous.png")).unwrap();
        fs::write(&path, snapshot_tests::static_png((1, 1), &[0, 255, 0, 255])).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stamp.1))
            .unwrap();
        assert_eq!(file_stamp(&path), Some(stamp));
        let replaced = ThumbnailKey::new(&path);
        assert!(replaced.source_file.is_some());
        assert!(original != replaced);
        assert!(cache.get(&replaced).is_none());
        cache.insert(replaced.clone(), Thumbnail::Failed);
        assert!(cache.get(&original).is_none());
        assert!(cache.get(&replaced).is_some());
        let unavailable = ThumbnailKey {
            source_file: None,
            ..replaced
        };
        cache.insert(unavailable.clone(), Thumbnail::Failed);
        assert!(cache.get(&unavailable).is_none());
    }

    #[test]
    fn image_viewer_thumbnail_lru_enforces_pixel_byte_budget() {
        let mut cache = ThumbnailCache::default();
        let pixels = crate::canvas_source::render_image(1024, 1024, vec![0; 1024 * 1024 * 4]);
        for index in 0..3 {
            cache.insert(key(index), Thumbnail::Ready(pixels.clone()));
        }
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bytes, THUMBNAIL_CACHE_BYTES);
        assert!(cache.get(&key(0)).is_none());
    }

    #[test]
    fn image_viewer_thumbnail_mailbox_rejects_stale_epochs_and_cancelled_requests() {
        let loader = Loader {
            shared: Arc::new((
                Mutex::new(Mailbox {
                    serial: 7,
                    thumbnail_epoch: 3,
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            thread: None,
        };
        {
            let mut mailbox = loader.shared.0.lock().unwrap();
            for (serial, epoch) in [(6, 3), (7, 2), (7, 3)] {
                mailbox.publish_thumbnail(ThumbnailCompletion {
                    serial,
                    epoch,
                    index: 0,
                    thumbnail: Thumbnail::Failed,
                });
            }
            assert_eq!(mailbox.thumbnails.len(), 1);
        }
        loader.cancel(8);
        assert!(loader.take_thumbnails().is_empty());
        loader.request_thumbnails(ThumbnailRequest {
            serial: 7,
            epoch: 4,
            resident_bytes: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            paths: VecDeque::new(),
        });
        assert!(loader.shared.0.lock().unwrap().thumbnail_request.is_none());
        loader.request_thumbnails(ThumbnailRequest {
            serial: 8,
            epoch: 5,
            resident_bytes: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            paths: (0..100).map(|index| (index, key(index).path)).collect(),
        });
        assert_eq!(
            loader
                .shared
                .0
                .lock()
                .unwrap()
                .thumbnail_request
                .as_ref()
                .unwrap()
                .paths
                .len(),
            MAX_THUMBNAIL_JOBS
        );
    }
}
