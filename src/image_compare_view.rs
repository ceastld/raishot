//! 只读双图对比；同步视口从冻结源像素采样，不修改文件或编辑历史。
#[path = "image_compare_files.rs"]
mod files;
#[path = "image_compare_viewport.rs"]
mod viewport;

use self::viewport::{CompareMapping, CompareRect, CompareViewport};
use super::{compress::EncodedImage, JobKind, WorkGate};
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tools::{
        check_cancel, validate_input, validate_output, ProcessedImage, ToolError, ToolLimits,
        ToolResult,
    },
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, PathPromptOptions,
    Render, RenderImage, ScrollDelta, ScrollWheelEvent, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex, Weak},
};

const PREVIEW_WIDTH: u32 = 960;
const PREVIEW_HEIGHT: u32 = 600;
const DISPLAY_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FROZEN_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompareMode {
    SideBySide,
    Before,
    After,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Before,
    After,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Self::Before => 0,
            Self::After => 1,
        }
    }
}

#[derive(Clone, Copy)]
enum Control {
    Mode(CompareMode),
    Swap,
    Files,
    Fit,
    Actual,
    Zoom(f64),
    Pan(f64, f64),
    Cancel,
    Close,
}

struct ComparePreviews {
    before: Arc<RenderImage>,
    after: Arc<RenderImage>,
}

enum ComparePixels {
    Image(Arc<Image>),
    Processed(Arc<ProcessedImage>),
}

impl ComparePixels {
    fn image(&self) -> &Image {
        match self {
            Self::Image(image) => image,
            Self::Processed(image) => image.image(),
        }
    }
    fn shares_pixels(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Image(a), Self::Image(b)) => Arc::ptr_eq(a, b),
            (Self::Processed(a), Self::Processed(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

struct CompareFrames {
    pixels: [ComparePixels; 2],
}

#[derive(Default)]
struct HostReservation {
    base: u64,
    source: Option<Weak<Image>>,
    source_bytes: u64,
    encoded_bytes: u64,
}

impl HostReservation {
    fn snapshot(base: u64, source: &Arc<Image>, encoded: &EncodedImage) -> Self {
        Self {
            base,
            source: Some(Arc::downgrade(source)),
            source_bytes: source.bgra.capacity() as u64,
            encoded_bytes: encoded.reserved_bytes(),
        }
    }

    fn source_is_in(&self, frames: &CompareFrames) -> bool {
        self.source.as_ref().is_some_and(|source| {
            frames.pixels.iter().any(|pixels| match pixels {
                ComparePixels::Image(image) => source.ptr_eq(&Arc::downgrade(image)),
                ComparePixels::Processed(_) => false,
            })
        })
    }
}

impl CompareFrames {
    fn resident_bytes(&self) -> ToolResult<u64> {
        let first = self.pixels[0].image().bgra.capacity() as u64;
        let second = if self.pixels[0].shares_pixels(&self.pixels[1]) {
            0
        } else {
            self.pixels[1].image().bgra.capacity() as u64
        };
        first
            .checked_add(second)
            .filter(|bytes| *bytes <= MAX_FROZEN_BYTES)
            .ok_or_else(|| {
                ToolError::Invalid(
                    tr!(
                        "对照冻结像素超过 256 MiB 预算",
                        "Comparison pixels exceed the 256 MiB budget"
                    )
                    .into(),
                )
            })
    }
}

#[derive(Clone, Copy, PartialEq)]
struct TileKey {
    generation: u64,
    viewport: CompareViewport,
    panels: [Option<CompareRect>; 2],
    display_scale: f64,
}

struct CompareTiles {
    key: TileKey,
    images: [Option<Arc<RenderImage>>; 2],
    downsampled: [bool; 2],
}

enum CompareRequest {
    Sources(CompareSource, CompareSource),
    Files(Vec<PathBuf>),
    Tiles(Arc<CompareFrames>, TileKey),
}

enum CompareWorkResult {
    Loaded {
        frames: Arc<CompareFrames>,
        previews: ComparePreviews,
        labels: [String; 2],
        note: String,
    },
    Tiles(CompareTiles),
}

enum CompareSource {
    Image(Arc<Image>),
    Processed(Arc<ProcessedImage>),
    Encoded(Arc<EncodedImage>),
}

impl CompareSource {
    fn resolve(
        self,
        external_resident_bytes: u64,
        cancel: &AtomicBool,
    ) -> ToolResult<ComparePixels> {
        check_cancel(cancel)?;
        match self {
            Self::Image(image) => Ok(ComparePixels::Image(image)),
            Self::Processed(image) => Ok(ComparePixels::Processed(image)),
            Self::Encoded(image) => Ok(ComparePixels::Image(Arc::new(
                image.decode_with_resident(external_resident_bytes, cancel)?,
            ))),
        }
    }
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Image(image) => (image.width, image.height),
            Self::Processed(image) => (image.image().width, image.image().height),
            Self::Encoded(image) => (image.width(), image.height()),
        }
    }

    fn validate(&self) -> ToolResult<()> {
        match self {
            Self::Image(image) => validate_input(image, ToolLimits::default()).map(|_| ()),
            Self::Processed(image) => {
                validate_input(image.image(), ToolLimits::default()).map(|_| ())
            }
            Self::Encoded(image) => {
                validate_output(image.width(), image.height(), ToolLimits::default()).map(|_| ())
            }
        }
    }

    fn preview(&self, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
        match self {
            Self::Image(image) => prepare_preview(image, cancel),
            Self::Processed(image) => prepare_preview(image.image(), cancel),
            Self::Encoded(image) => {
                // 只在后台回读封存成品，缩略图完成后释放临时解码帧。
                let decoded = image.decode(cancel)?;
                prepare_preview(&decoded, cancel)
            }
        }
    }

    fn shares_pixels(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Image(left), Self::Image(right)) => Arc::ptr_eq(left, right),
            (Self::Processed(left), Self::Processed(right)) => Arc::ptr_eq(left, right),
            (Self::Encoded(left), Self::Encoded(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

pub(crate) struct ImageCompareView {
    before_size: (u32, u32),
    after_size: (u32, u32),
    previews: Option<ComparePreviews>,
    frames: Option<Arc<CompareFrames>>,
    tiles: Option<CompareTiles>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    labels: [String; 2],
    note: String,
    host_reservation: HostReservation,
    generation: u64,
    viewport: CompareViewport,
    panels: Arc<Mutex<[Option<CompareRect>; 2]>>,
    active_side: Side,
    drag: Option<(Side, (f64, f64))>,
    desired_tile: Option<TileKey>,
    gate: WorkGate,
    pending: Option<CompareRequest>,
    loading: bool,
    prompt_pending: bool,
    cancelled: bool,
    mode: CompareMode,
    swapped: bool,
    closing: bool,
    error: Option<String>,
    focus: FocusHandle,
}

/// 只借用不可变图像快照；关闭后取消准备，准备结束后不再持有大图。
pub(crate) fn open(
    before: Arc<Image>,
    after: Arc<Image>,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    open_sources(
        CompareSource::Image(before),
        CompareSource::Image(after),
        HostReservation::default(),
        cx,
    )
}

/// 工具窗保留处理产物的共享所有权，不为进入对比复制完整结果帧。
pub(crate) fn open_processed(
    before: Arc<Image>,
    after: Arc<ProcessedImage>,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    open_sources(
        CompareSource::Image(before),
        CompareSource::Processed(after),
        HostReservation::default(),
        cx,
    )
}

/// 冻结打开时的编码成品；结果侧包含压缩、透明合成和颜色配置回读后的像素。
pub(crate) fn open_encoded(
    before: Arc<Image>,
    after: Arc<EncodedImage>,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    open_sources(
        CompareSource::Image(before),
        CompareSource::Encoded(after),
        HostReservation::default(),
        cx,
    )
}

/// 宿主R只记录打开时已知的像素、元数据和显示预留；不含共享before及本次编码。
/// 后续换文件仍保留此预留，不跟踪父窗口未来的其它任务或全进程峰值。
pub(crate) fn open_encoded_with_resident(
    before: Arc<Image>,
    after: Arc<EncodedImage>,
    host_resident_bytes: u64,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    let host_reservation = HostReservation::snapshot(host_resident_bytes, &before, &after);
    open_sources(
        CompareSource::Image(before),
        CompareSource::Encoded(after),
        host_reservation,
        cx,
    )
}

fn open_sources(
    before: CompareSource,
    after: CompareSource,
    host_reservation: HostReservation,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    before.validate()?;
    after.validate()?;
    open_request(
        Some(CompareRequest::Sources(before, after)),
        host_reservation,
        true,
        cx,
    )
}

/// 两张外部文件只读对照；空列表在新窗口选择，顺序由左右标签和交换按钮明确。
pub(crate) fn open_files(
    paths: Vec<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    if !paths.is_empty() && paths.len() != 2 {
        return Err(ToolError::Invalid(
            tr!("请选择恰好两张图片", "Choose exactly two images").into(),
        )
        .into());
    }
    let request = (!paths.is_empty()).then_some(CompareRequest::Files(paths));
    open_request(request, HostReservation::default(), true, cx)
}

/// 主界面工具页入口：默认对照最近两张截图（旧左新右）；不足两张时打开空窗口，不弹文件框。
pub(crate) fn open_recent_captures(cx: &mut App) -> Result<WindowHandle<ImageCompareView>> {
    let recent = crate::tool_recent_captures::latest(2);
    match recent.as_slice() {
        [(newer, _), (older, _), ..] => open_files(vec![older.clone(), newer.clone()], cx),
        _ => open_request(None, HostReservation::default(), false, cx),
    }
}

fn open_request(
    request: Option<CompareRequest>,
    host_reservation: HostReservation,
    prompt: bool,
    cx: &mut App,
) -> Result<WindowHandle<ImageCompareView>> {
    let sizes = match &request {
        Some(CompareRequest::Sources(before, after)) => [before.dimensions(), after.dimensions()],
        _ => [(0, 0); 2],
    };
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(1080.), px(760.)), |display| {
            let bounds = display.bounds();
            size(
                px(1080_f32.min((f32::from(bounds.size.width) - 48.).max(420.))),
                px(760_f32.min((f32::from(bounds.size.height) - 72.).max(340.))),
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
                "双图对照",
                "Image comparison"
            ));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                ImageCompareView {
                    before_size: sizes[0],
                    after_size: sizes[1],
                    previews: None,
                    frames: None,
                    tiles: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
                    labels: [
                        tr!("原图", "Original").into(),
                        tr!("处理结果", "Result").into(),
                    ],
                    note: String::new(),
                    host_reservation,
                    generation: 0,
                    viewport: CompareViewport::default(),
                    panels: Arc::new(Mutex::new([None; 2])),
                    active_side: Side::Before,
                    drag: None,
                    desired_tile: None,
                    gate: WorkGate::new(),
                    pending: None,
                    loading: false,
                    prompt_pending: false,
                    cancelled: false,
                    mode: CompareMode::SideBySide,
                    swapped: false,
                    closing: false,
                    error: None,
                    focus,
                }
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view.update(cx, |view, cx| match request {
                Some(request) => view.queue(request, window, cx),
                None if prompt => view.choose_files(window, cx),
                None => {}
            });
            view
        },
    )?)
}

impl ImageCompareView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        self.pending = None;
        self.gate.cancel();
        if self.gate.active.is_some() {
            cx.notify();
            false
        } else {
            true
        }
    }

    fn queue(&mut self, request: CompareRequest, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        if !matches!(request, CompareRequest::Tiles(..)) {
            self.loading = true;
            self.drag = None;
            self.desired_tile = None;
            self.error = None;
        }
        self.gate.change();
        self.cancelled = false;
        self.pending = Some(request);
        self.start_work(window, cx);
        cx.notify();
    }

    fn external_resident_bytes(&self, request: &CompareRequest) -> ToolResult<u64> {
        request_resident_bytes(&self.host_reservation, request, self.frames.as_deref())
    }

    fn start_work(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.gate.active.is_some() {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        let external = match self.external_resident_bytes(&request) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.loading = false;
                self.gate.cancel();
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let Some(ticket) = self.gate.launch(JobKind::Preview) else {
            self.pending = Some(request);
            return;
        };
        let cancel = ticket.cancel.clone();
        let task = cx
            .background_executor()
            .spawn(async move { prepare_request(request, external, &cancel) });
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
                    drop(result);
                    if view.pending.is_none() {
                        view.loading = false;
                    }
                    view.start_work(window, cx);
                    cx.notify();
                    return;
                }
                view.loading = false;
                match result {
                    Ok(CompareWorkResult::Loaded {
                        frames,
                        previews,
                        labels,
                        note,
                    }) => {
                        view.retire_previews();
                        view.retire_tiles();
                        view.before_size = (
                            frames.pixels[0].image().width,
                            frames.pixels[0].image().height,
                        );
                        view.after_size = (
                            frames.pixels[1].image().width,
                            frames.pixels[1].image().height,
                        );
                        view.frames = Some(frames);
                        view.previews = Some(previews);
                        view.labels = labels;
                        view.note = note;
                        view.generation = view.generation.wrapping_add(1);
                        view.viewport = CompareViewport::default();
                        view.desired_tile = None;
                        view.error = None;
                    }
                    Ok(CompareWorkResult::Tiles(tiles)) => {
                        if view.current_key(window) == Some(tiles.key) {
                            view.retire_tiles();
                            view.tiles = Some(tiles);
                        } else {
                            view.desired_tile = None;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                view.refresh_tiles(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn retire_previews(&mut self) {
        if let Some(previews) = self.previews.take() {
            self.retiring.extend([previews.before, previews.after]);
        }
    }

    fn retire_tiles(&mut self) {
        if let Some(tiles) = self.tiles.take() {
            self.retiring.extend(tiles.images.into_iter().flatten());
        }
    }

    fn current_key(&self, window: &Window) -> Option<TileKey> {
        self.frames.as_ref()?;
        let mut panels = *self.panels.lock().ok()?;
        match self.mode {
            CompareMode::Before => panels[1] = None,
            CompareMode::After => panels[0] = None,
            CompareMode::SideBySide => {}
        }
        if panels.iter().all(Option::is_none) {
            return None;
        }
        Some(TileKey {
            generation: self.generation,
            viewport: self.viewport,
            panels,
            display_scale: f64::from(window.scale_factor()),
        })
    }

    fn refresh_tiles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.loading || self.prompt_pending {
            return;
        }
        let Some(key) = self.current_key(window) else {
            return;
        };
        if self.desired_tile == Some(key)
            || self.tiles.as_ref().is_some_and(|tiles| tiles.key == key)
        {
            return;
        }
        self.desired_tile = Some(key);
        if let Some(frames) = self.frames.clone() {
            self.queue(CompareRequest::Tiles(frames, key), window, cx);
        }
    }

    fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing || self.loading || self.prompt_pending {
            return;
        }
        self.prompt_pending = true;
        self.drag = None;
        window.focus(&self.focus);
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr!("选择两张只读对照图片", "Choose two comparison images").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if view.closing || !view.prompt_pending {
                    return;
                }
                view.prompt_pending = false;
                match chosen {
                    Ok(Ok(Some(paths))) if paths.len() == 2 => {
                        view.queue(CompareRequest::Files(paths), window, cx)
                    }
                    Ok(Ok(Some(_))) => {
                        view.error = Some(
                            tr!(
                                "请选择恰好两张图片，原对照保持不变",
                                "Choose exactly two images; the current comparison is unchanged"
                            )
                            .into(),
                        )
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => view.error = Some(error.to_string()),
                    Err(error) => view.error = Some(error.to_string()),
                }
                view.refresh_tiles(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing && !matches!(action, Control::Close) {
            return;
        }
        match action {
            Control::Mode(mode) => {
                self.mode = mode;
                self.drag = None;
            }
            Control::Swap if self.mode == CompareMode::SideBySide => self.swapped = !self.swapped,
            Control::Swap => return,
            Control::Files => {
                self.choose_files(window, cx);
                return;
            }
            Control::Fit => {
                self.viewport = CompareViewport::default();
                self.desired_tile = None;
                self.drag = None;
            }
            Control::Actual => {
                let side = self.visible_active_side();
                let scale = f64::from(window.scale_factor());
                self.edit_viewport(side, window, cx, |view, dimensions, panel| {
                    view.actual(dimensions, panel, scale)
                });
                return;
            }
            Control::Zoom(factor) => {
                let side = self.visible_active_side();
                self.edit_viewport(side, window, cx, |view, dimensions, panel| {
                    view.zoom_at(
                        dimensions,
                        panel,
                        factor,
                        (panel.x + panel.width / 2., panel.y + panel.height / 2.),
                    )
                });
                return;
            }
            Control::Pan(x, y) => {
                let side = self.visible_active_side();
                self.edit_viewport(side, window, cx, |view, dimensions, panel| {
                    view.pan(dimensions, panel, x, y)
                });
                return;
            }
            Control::Cancel => {
                self.gate.cancel();
                self.pending = None;
                self.loading = false;
                self.drag = None;
                self.desired_tile = self.current_key(window);
                self.error = None;
                self.cancelled = true;
            }
            Control::Close => {
                if self.request_close(cx) {
                    window.remove_window();
                }
                return;
            }
        }
        if !matches!(action, Control::Cancel) {
            self.refresh_tiles(window, cx);
        }
        cx.notify();
    }

    fn visible_active_side(&self) -> Side {
        match self.mode {
            CompareMode::Before => Side::Before,
            CompareMode::After => Side::After,
            CompareMode::SideBySide => self.active_side,
        }
    }

    fn edit_viewport(
        &mut self,
        side: Side,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut CompareViewport, (u32, u32), CompareRect) -> bool,
    ) {
        if self.closing || self.loading || self.prompt_pending || self.frames.is_none() {
            return;
        }
        let panel = self
            .panels
            .lock()
            .ok()
            .and_then(|panels| panels[side.index()]);
        let Some(panel) = panel else {
            return;
        };
        let dimensions = if side == Side::Before {
            self.before_size
        } else {
            self.after_size
        };
        self.active_side = side;
        if edit(&mut self.viewport, dimensions, panel) {
            self.refresh_tiles(window, cx);
        }
        cx.notify();
    }

    fn picked(
        &mut self,
        side: Side,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing || self.loading || self.prompt_pending || self.frames.is_none() {
            return;
        }
        let position = (
            f64::from(f32::from(event.position.x)),
            f64::from(f32::from(event.position.y)),
        );
        let panel = self
            .panels
            .lock()
            .ok()
            .and_then(|panels| panels[side.index()]);
        if !panel.is_some_and(|rect| inside(rect, position)) {
            return;
        }
        self.active_side = side;
        self.drag = Some((side, position));
        window.focus(&self.focus);
        cx.stop_propagation();
        cx.notify();
    }

    fn moved(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((side, previous)) = self.drag else {
            return;
        };
        if !matches!(
            event.pressed_button,
            Some(MouseButton::Left | MouseButton::Middle)
        ) {
            self.drag = None;
            return;
        }
        let position = (
            f64::from(f32::from(event.position.x)),
            f64::from(f32::from(event.position.y)),
        );
        self.drag = Some((side, position));
        self.edit_viewport(side, window, cx, |view, dimensions, panel| {
            view.pan(
                dimensions,
                panel,
                position.0 - previous.0,
                position.1 - previous.1,
            )
        });
    }

    fn wheeled(
        &mut self,
        side: Side,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = (
            f64::from(f32::from(event.position.x)),
            f64::from(f32::from(event.position.y)),
        );
        let panel = self
            .panels
            .lock()
            .ok()
            .and_then(|panels| panels[side.index()]);
        if !panel.is_some_and(|rect| inside(rect, position)) {
            return;
        }
        let delta = match event.delta {
            ScrollDelta::Lines(p) => f64::from(p.y) * 48.,
            ScrollDelta::Pixels(p) => f64::from(f32::from(p.y)),
        };
        let factor = (delta.clamp(-1000., 1000.) / 400.).exp();
        self.edit_viewport(side, window, cx, |view, dimensions, panel| {
            view.zoom_at(dimensions, panel, factor, position)
        });
        cx.stop_propagation();
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        selected: bool,
        enabled: bool,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        div()
            .id(id)
            .px_3()
            .py_2()
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
                        cx.listener(move |view, _, window, cx| view.control(action, window, cx)),
                    )
            })
            .child(label)
            .into_any_element()
    }

    fn panel(&self, side: Side, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let index = side.index();
        let label = display_label(&self.labels[index]);
        let dimensions = if side == Side::Before {
            self.before_size
        } else {
            self.after_size
        };
        let preview = self.previews.as_ref().map(|previews| {
            if side == Side::Before {
                previews.before.clone()
            } else {
                previews.after.clone()
            }
        });
        let tile = self.tiles.as_ref().map(|tiles| {
            (
                tiles.key,
                tiles.images[index].clone(),
                tiles.downsampled[index],
            )
        });
        let viewport = self.viewport;
        let generation = self.generation;
        let panels = self.panels.clone();
        let weak = cx.entity().downgrade();
        let preview = div()
            .id(if side == Side::Before {
                "compare-image-before"
            } else {
                "compare-image-after"
            })
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(if self.active_side == side {
                palette.accent
            } else {
                palette.border
            }))
            .bg(rgb(palette.track))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event, window, cx| view.picked(side, event, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |view, event, window, cx| view.picked(side, event, window, cx)),
            )
            .on_scroll_wheel(
                cx.listener(move |view, event, window, cx| view.wheeled(side, event, window, cx)),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let full = compare_rect(bounds);
                        let panel = CompareRect {
                            height: (full.height - 22.).max(0.),
                            ..full
                        };
                        let valid = (panel.width > 0. && panel.height > 0.).then_some(panel);
                        let changed = if let Ok(mut slots) = panels.lock() {
                            let changed = slots[index] != valid;
                            slots[index] = valid;
                            changed
                        } else {
                            false
                        };
                        if changed {
                            let weak = weak.clone();
                            window.defer(cx, move |window, cx| {
                                let _ = weak.update(cx, |view, cx| view.refresh_tiles(window, cx));
                            });
                        }
                        (
                            full,
                            panel,
                            valid.and_then(|panel| viewport.mapping(dimensions, panel)),
                        )
                    },
                    move |_,
                          (full, panel, mapping): (
                        CompareRect,
                        CompareRect,
                        Option<CompareMapping>,
                    ),
                          window,
                          cx| {
                        let display_scale = f64::from(window.scale_factor());
                        let current = tile.as_ref().filter(|(key, image, _)| {
                            key.generation == generation
                                && key.viewport == viewport
                                && key.panels[index] == Some(panel)
                                && key.display_scale == display_scale
                                && image.is_some()
                        });
                        if let Some((_, Some(image), _)) = current {
                            let _ = window.paint_image_exact(
                                paint_bounds(panel),
                                px(0.).into(),
                                image.clone(),
                                0,
                                false,
                            );
                        } else if let (Some(image), Some(mapping)) = (&preview, mapping) {
                            let _ = window.with_content_mask(
                                Some(gpui::ContentMask {
                                    bounds: paint_bounds(panel),
                                }),
                                |window| {
                                    window.paint_image_exact(
                                        paint_bounds(mapping.image),
                                        px(0.).into(),
                                        image.clone(),
                                        0,
                                        false,
                                    )
                                },
                            );
                        }
                        let ratio = mapping
                            .map(|map| {
                                map.image.width / f64::from(dimensions.0) * display_scale * 100.
                            })
                            .unwrap_or(0.);
                        let status = match current {
                            Some((_, Some(image), true)) => trf!(
                                "降采样预览 {}×{} · 当前源像素比例 {ratio:.1}%",
                                "Reduced preview {}×{} · source scale {ratio:.1}%",
                                u32::from(image.size(0).width),
                                u32::from(image.size(0).height)
                            ),
                            Some(_) => trf!(
                                "源像素采样 · 当前源像素比例 {ratio:.1}%",
                                "Source-pixel sampling · source scale {ratio:.1}%"
                            ),
                            None if preview.is_some() => trf!(
                                "过渡预览（非精细像素） · {ratio:.1}%",
                                "Preview only, not fine pixels · {ratio:.1}%"
                            ),
                            None => tr!(
                                "选择两张图片，或等待冻结像素读入",
                                "Choose two images, or wait for pixels to load"
                            )
                            .into(),
                        };
                        paint_status(
                            window,
                            cx,
                            CompareRect {
                                x: full.x,
                                y: full.y + panel.height,
                                width: full.width,
                                height: (full.height - panel.height).max(0.),
                            },
                            status,
                            palette.muted,
                            palette.background,
                        );
                    },
                )
                .size_full(),
            );
        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(palette.foreground))
                    .child(if dimensions.0 == 0 || dimensions.1 == 0 {
                        trf!("{label} · 未选择", "{label} · no image")
                    } else {
                        trf!(
                            "{label} · {} × {} 源像素",
                            "{label} · {} × {} source pixels",
                            dimensions.0,
                            dimensions.1
                        )
                    }),
            )
            .child(preview)
            .into_any_element()
    }
}

impl Drop for ImageCompareView {
    fn drop(&mut self) {
        self.gate.cancel();
    }
}

impl Render for ImageCompareView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        self.refresh_tiles(window, cx);
        let palette = crate::theme::palette(cx);
        let viewport_enabled =
            self.frames.is_some() && !self.closing && !self.loading && !self.prompt_pending;
        let mut images = div().flex_1().min_h(px(0.)).flex().gap_3();
        images = match self.mode {
            CompareMode::Before => images.child(self.panel(Side::Before, cx)),
            CompareMode::After => images.child(self.panel(Side::After, cx)),
            CompareMode::SideBySide => {
                let sides = if self.swapped {
                    [Side::After, Side::Before]
                } else {
                    [Side::Before, Side::After]
                };
                images
                    .child(self.panel(sides[0], cx))
                    .child(self.panel(sides[1], cx))
            }
        };
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
            .on_mouse_move(cx.listener(Self::moved))
            .on_mouse_up(MouseButton::Left, cx.listener(|view, _, _, _| view.drag = None))
            .on_mouse_up(MouseButton::Middle, cx.listener(|view, _, _, _| view.drag = None))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|view, _, _, _| view.drag = None))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(|view, _, _, _| view.drag = None))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.is_held || event.keystroke.modifiers.modified() {
                    return;
                }
                let action = match event.keystroke.key.as_str() {
                    "1" => Control::Mode(CompareMode::SideBySide),
                    "2" => Control::Mode(CompareMode::Before),
                    "3" => Control::Mode(CompareMode::After),
                    "x" => Control::Swap,
                    "f" => Control::Fit,
                    "0" => Control::Actual,
                    "+" | "=" => Control::Zoom(2.),
                    "-" => Control::Zoom(0.5),
                    "left" => Control::Pan(40., 0.),
                    "right" => Control::Pan(-40., 0.),
                    "up" => Control::Pan(0., 40.),
                    "down" => Control::Pan(0., -40.),
                    "escape" => Control::Close,
                    _ => return,
                };
                view.control(action, window, cx);
                cx.stop_propagation();
            }))
            .child(div().text_size(px(20.)).child(tr!("双图对照", "Image comparison")))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .flex_shrink_0()
                    .gap_2()
                    .child(self.button("compare-files", tr!("选择两张图片…", "Choose two images…"), false, !self.closing && !self.loading && !self.prompt_pending, Control::Files, cx))
                    .child(self.button("compare-pair", tr!("并排", "Side by side"), self.mode == CompareMode::SideBySide, !self.closing, Control::Mode(CompareMode::SideBySide), cx))
                    .child(self.button("compare-before", tr!("只看图一", "First only"), self.mode == CompareMode::Before, !self.closing, Control::Mode(CompareMode::Before), cx))
                    .child(self.button("compare-after", tr!("只看图二", "Second only"), self.mode == CompareMode::After, !self.closing, Control::Mode(CompareMode::After), cx))
                    .child(self.button("compare-swap", tr!("交换左右", "Swap sides"), false, self.mode == CompareMode::SideBySide, Control::Swap, cx))
                    .child(self.button("compare-fit", tr!("适应", "Fit"), false, viewport_enabled, Control::Fit, cx))
                    .child(self.button("compare-actual", tr!("当前侧 100%", "Active side 100%"), false, viewport_enabled, Control::Actual, cx))
                    .child(self.button("compare-zoom-out", "−", false, viewport_enabled, Control::Zoom(0.5), cx))
                    .child(self.button("compare-zoom-in", "+", false, viewport_enabled, Control::Zoom(2.), cx))
                    .child(self.button("compare-cancel", tr!("取消任务", "Cancel task"), false, !self.closing && self.gate.active.is_some(), Control::Cancel, cx))
                    .child(self.button("compare-close", tr!("关闭", "Close"), false, true, Control::Close, cx)),
            )
            .child(images)
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(rgb(if self.error.is_some() { palette.danger } else { palette.muted }))
                    .child(self.error.clone().unwrap_or_else(|| {
                        let state = if self.loading { tr!("正在读取新对照，完成前保留旧对照。", "Loading a new pair; the opened pair is kept until ready.") } else if self.cancelled { tr!("已请求取消，已打开的对照保持不变。", "Cancellation requested; the opened comparison is kept.") } else { "" };
                        format!("{state} {} {}", self.note, tr!("滚轮缩放，拖动平移，两侧同步归一化中心和适应倍数。F 适应，0 当前侧100%，1/2/3 切换，X 交换；另一侧比例见各自状态条。", "Wheel zooms; drag pans. Both sides share a normalized center and fit multiplier. F fits, 0 sets active side to 100%, 1/2/3 switches views, X swaps; each side shows its own scale."))
                    })),
            )
            .into_any_element()
    }
}

fn add_resident_bytes(first: u64, second: u64) -> ToolResult<u64> {
    first.checked_add(second).ok_or_else(|| {
        ToolError::Invalid(tr!("对照驻留预算溢出", "Comparison resident budget overflow").into())
    })
}

fn request_resident_bytes(
    host: &HostReservation,
    request: &CompareRequest,
    frames: Option<&CompareFrames>,
) -> ToolResult<u64> {
    match request {
        CompareRequest::Sources(..) => Ok(host.base),
        CompareRequest::Files(_) => {
            // Sources由before和编码输入各计一次；换文件后原宿主仍可能保留二者。
            let resident = add_resident_bytes(
                add_resident_bytes(host.base, host.source_bytes)?,
                host.encoded_bytes,
            )?;
            match frames {
                Some(frames) => {
                    let old_frames = frames.resident_bytes()?;
                    let shared_source = if host.source_is_in(frames) {
                        host.source_bytes
                    } else {
                        0
                    };
                    let old_frames = old_frames.checked_sub(shared_source).ok_or_else(|| {
                        ToolError::Invalid(
                            tr!(
                                "对照共享驻留预算不一致",
                                "Inconsistent shared comparison reservation"
                            )
                            .into(),
                        )
                    })?;
                    add_resident_bytes(
                        add_resident_bytes(resident, old_frames)?,
                        DISPLAY_RESERVE_BYTES,
                    )
                }
                None => Ok(resident),
            }
        }
        CompareRequest::Tiles(..) => Ok(0),
    }
}

fn prepare_request(
    request: CompareRequest,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> ToolResult<CompareWorkResult> {
    check_cancel(cancel)?;
    match request {
        CompareRequest::Sources(before, after) => {
            let encoded = matches!(&after, CompareSource::Encoded(_));
            let before = before.resolve(external_resident_bytes, cancel)?;
            let resident = add_resident_bytes(
                external_resident_bytes,
                before.image().bgra.capacity() as u64,
            )?;
            let after = after.resolve(resident, cancel)?;
            let frames = CompareFrames {
                pixels: [before, after],
            };
            let labels = [
                tr!("原图快照", "Original snapshot").into(),
                if encoded {
                    tr!("成品（编码回读）", "Encoded result")
                } else {
                    tr!("处理像素", "Processed pixels")
                }
                .into(),
            ];
            let note = if encoded {
                tr!(
                    "原图为冻结快照，成品为打开时封存字节的回读；不重新读取来源路径。",
                    "Frozen source and the opened encoded result; source paths are not reread."
                )
            } else {
                tr!(
                    "两侧均为冻结像素快照，不重新读取来源路径。",
                    "Both sides are frozen pixel snapshots; source paths are not reread."
                )
            }
            .into();
            prepare_loaded(frames, labels, note, cancel)
        }
        CompareRequest::Files(paths) => {
            let files::FileComparePair {
                images: [before, after],
                labels,
                warnings,
            } = files::load_pair(&paths, external_resident_bytes, cancel)?;
            let frames = CompareFrames {
                pixels: [ComparePixels::Image(before), ComparePixels::Image(after)],
            };
            let mut note = tr!(
                "文件使用统一解码后的首帧/首页像素，原文件保持只读。",
                "Files use decoded first-frame/page pixels and remain read-only."
            )
            .to_string();
            for (index, warning) in warnings.into_iter().enumerate() {
                if let Some(warning) = warning {
                    note.push_str(&format!(" {}: {warning}", display_label(&labels[index])));
                }
            }
            prepare_loaded(frames, labels, note, cancel)
        }
        CompareRequest::Tiles(frames, key) => {
            let mut images = [None, None];
            let mut downsampled = [false; 2];
            for index in 0..2 {
                let Some(panel) = key.panels[index] else {
                    continue;
                };
                check_cancel(cancel)?;
                let image = frames.pixels[index].image();
                let mapping = key
                    .viewport
                    .mapping((image.width, image.height), panel)
                    .ok_or_else(|| {
                        ToolError::Invalid(
                            tr!("对照视口无效", "Invalid comparison viewport").into(),
                        )
                    })?;
                let (width, height, reduced) =
                    viewport::tile_dimensions(panel.width, panel.height, key.display_scale)
                        .ok_or_else(|| {
                            ToolError::Invalid(
                                tr!("对照采样尺寸无效", "Invalid comparison sampling size").into(),
                            )
                        })?;
                let sampled = viewport::render_view(image, mapping, (width, height), cancel)?;
                check_cancel(cancel)?;
                images[index] = Some(crate::canvas_source::render_image(
                    width,
                    height,
                    sampled.bgra,
                ));
                downsampled[index] = reduced;
            }
            check_cancel(cancel)?;
            Ok(CompareWorkResult::Tiles(CompareTiles {
                key,
                images,
                downsampled,
            }))
        }
    }
}

fn prepare_loaded(
    frames: CompareFrames,
    labels: [String; 2],
    note: String,
    cancel: &AtomicBool,
) -> ToolResult<CompareWorkResult> {
    for pixels in &frames.pixels {
        validate_input(pixels.image(), ToolLimits::default())?;
    }
    frames.resident_bytes()?;
    let before = prepare_preview(frames.pixels[0].image(), cancel)?;
    let after = if frames.pixels[0].shares_pixels(&frames.pixels[1]) {
        before.clone()
    } else {
        prepare_preview(frames.pixels[1].image(), cancel)?
    };
    check_cancel(cancel)?;
    Ok(CompareWorkResult::Loaded {
        frames: Arc::new(frames),
        previews: ComparePreviews { before, after },
        labels,
        note,
    })
}

fn compare_rect(bounds: Bounds<gpui::Pixels>) -> CompareRect {
    CompareRect {
        x: f64::from(f32::from(bounds.origin.x)),
        y: f64::from(f32::from(bounds.origin.y)),
        width: f64::from(f32::from(bounds.size.width)),
        height: f64::from(f32::from(bounds.size.height)),
    }
}

fn inside(rect: CompareRect, point: (f64, f64)) -> bool {
    point.0.is_finite()
        && point.1.is_finite()
        && point.0 >= rect.x
        && point.1 >= rect.y
        && point.0 < rect.x + rect.width
        && point.1 < rect.y + rect.height
}

fn display_label(label: &str) -> String {
    let chars: Vec<_> = label.chars().collect();
    if chars.len() <= 110 {
        return label.to_string();
    }
    format!(
        "{}…{}",
        chars[..40].iter().collect::<String>(),
        chars[chars.len() - 68..].iter().collect::<String>()
    )
}

fn paint_bounds(rect: CompareRect) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(px(rect.x as f32), px(rect.y as f32)),
        size(px(rect.width as f32), px(rect.height as f32)),
    )
}

fn paint_status(
    window: &mut Window,
    cx: &mut App,
    rect: CompareRect,
    text: String,
    foreground: u32,
    background: u32,
) {
    window.paint_quad(gpui::fill(paint_bounds(rect), rgb(background)));
    let len = text.len();
    let line = window.text_system().shape_line(
        text.into(),
        px(11.),
        &[gpui::TextRun {
            len,
            font: window.text_style().font(),
            color: rgb(foreground).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let _ = window.with_content_mask(
        Some(gpui::ContentMask {
            bounds: paint_bounds(rect),
        }),
        |window| {
            line.paint(
                point(px((rect.x + 5.) as f32), px((rect.y + 2.) as f32)),
                px(18.),
                window,
                cx,
            )
        },
    );
}

fn prepare_previews(
    before: &CompareSource,
    after: &CompareSource,
    cancel: &AtomicBool,
) -> ToolResult<ComparePreviews> {
    let before_preview = before.preview(cancel)?;
    let after_preview = if before.shares_pixels(after) {
        before_preview.clone()
    } else {
        after.preview(cancel)?
    };
    check_cancel(cancel)?;
    Ok(ComparePreviews {
        before: before_preview,
        after: after_preview,
    })
}

fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (f64::from(PREVIEW_WIDTH) / f64::from(width))
        .min(f64::from(PREVIEW_HEIGHT) / f64::from(height))
        .min(1.);
    (
        (f64::from(width) * scale).round().max(1.) as u32,
        (f64::from(height) * scale).round().max(1.) as u32,
    )
}

fn prepare_preview(image: &Image, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
    check_cancel(cancel)?;
    let (width, height) = preview_size(image.width, image.height);
    let preview = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
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
    use crate::{
        image_tool_view::compress,
        save_options::{SaveEncoding, SaveFormat},
    };

    #[test]
    fn image_compare_view_host_sources_reaches_decoder_and_precancel() {
        let source = Arc::new(Image {
            width: 2,
            height: 1,
            bgra: vec![10, 20, 30, 0, 40, 50, 60, 128],
            readback_ms: 0.,
        });
        let original = source.bgra.clone();
        let processed = ProcessedImage::new(
            Image {
                width: 2,
                height: 1,
                bgra: original.clone(),
                readback_ms: 0.,
            },
            111,
        );
        let cancel = AtomicBool::new(false);
        let encoded =
            Arc::new(compress::prepare(&processed, SaveEncoding::default(), &cancel).unwrap());
        let request = || {
            CompareRequest::Sources(
                CompareSource::Image(source.clone()),
                CompareSource::Encoded(encoded.clone()),
            )
        };
        let compatible = request();
        let resident =
            request_resident_bytes(&HostReservation::default(), &compatible, None).unwrap();
        let loaded = prepare_request(compatible, resident, &cancel).unwrap();
        let CompareWorkResult::Loaded { frames, .. } = loaded else {
            panic!("expected frozen sources");
        };
        assert_eq!(frames.pixels[1].image().bgra, original);
        let hosted = request();
        let host =
            HostReservation::snapshot(crate::image_decode::MAX_DECODE_BYTES, &source, &encoded);
        let resident = request_resident_bytes(&host, &hosted, None).unwrap();
        // Sources不能丢弃宿主R或仅以before覆盖它；同一真实回读应在分配前失败。
        assert!(matches!(
            prepare_request(hosted, resident, &cancel),
            Err(ToolError::Failed(_))
        ));
        assert!(matches!(
            prepare_request(request(), u64::MAX, &cancel),
            Err(ToolError::Invalid(_))
        ));
        assert!(matches!(
            prepare_request(request(), u64::MAX, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(source.bgra, original);
        assert_eq!(processed.image().bgra, original);
        assert_eq!(frames.pixels[1].image().bgra, original);
    }

    #[test]
    fn image_compare_view_host_reservation_survives_sources_to_files() {
        struct TestFile(PathBuf);
        impl Drop for TestFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("source.png"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let source = Arc::new(Image {
            width: 1,
            height: 1,
            bgra: vec![20, 40, 60, 128],
            readback_ms: 0.,
        });
        let cancel = AtomicBool::new(false);
        let processed = ProcessedImage::new(
            Image {
                width: source.width,
                height: source.height,
                bgra: source.bgra.clone(),
                readback_ms: 0.,
            },
            112,
        );
        let encoded =
            Arc::new(compress::prepare(&processed, SaveEncoding::default(), &cancel).unwrap());
        let host = HostReservation::snapshot(512, &source, &encoded);
        let source_request = CompareRequest::Sources(
            CompareSource::Image(source.clone()),
            CompareSource::Encoded(encoded.clone()),
        );
        let source_resident = request_resident_bytes(&host, &source_request, None).unwrap();
        assert_eq!(source_resident, host.base);
        let loaded = prepare_request(source_request, source_resident, &cancel).unwrap();
        let CompareWorkResult::Loaded { frames, .. } = loaded else {
            panic!("expected frozen sources");
        };
        let directory = TestFile(std::env::temp_dir().join(format!(
            "rabshot-compare-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )));
        std::fs::create_dir(&directory.0).unwrap();
        let path = directory.0.join("source.png");
        let bytes =
            crate::export::encode_image_cancellable(&source, SaveEncoding::default(), &cancel)
                .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let request = || CompareRequest::Files(vec![path.clone(), path.clone()]);
        let baseline =
            request_resident_bytes(&HostReservation::default(), &request(), Some(&frames)).unwrap();
        assert_eq!(
            baseline,
            frames.resident_bytes().unwrap() + DISPLAY_RESERVE_BYTES
        );
        let first_external = request_resident_bytes(&host, &request(), Some(&frames)).unwrap();
        assert_eq!(first_external, host.base + host.encoded_bytes + baseline);
        let first_files = prepare_request(request(), first_external, &cancel).unwrap();
        let CompareWorkResult::Loaded {
            frames: file_frames,
            ..
        } = first_files
        else {
            panic!("expected first file pair");
        };
        assert!(!host.source_is_in(&file_frames));
        let second_external =
            request_resident_bytes(&host, &request(), Some(&file_frames)).unwrap();
        assert_eq!(
            second_external,
            host.base
                + host.source_bytes
                + host.encoded_bytes
                + file_frames.resident_bytes().unwrap()
                + DISPLAY_RESERVE_BYTES,
        );
        assert!(prepare_request(request(), second_external, &cancel).is_ok());
        let high_host =
            HostReservation::snapshot(crate::image_decode::MAX_DECODE_BYTES, &source, &encoded);
        let hosted = request_resident_bytes(&high_host, &request(), Some(&file_frames)).unwrap();
        assert!(prepare_request(request(), hosted, &cancel).is_err());
        let overflow_host = HostReservation::snapshot(u64::MAX, &source, &encoded);
        assert!(request_resident_bytes(&overflow_host, &request(), Some(&file_frames)).is_err());
        assert!(matches!(
            prepare_request(request(), hosted, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        // 两次失败与取消都不消费旧图对或改变文件；下一次请求仍用原宿主预留。
        assert_eq!(frames.pixels[0].image().bgra, source.bgra);
        assert_eq!(frames.pixels[1].image().bgra, source.bgra);
        assert_eq!(
            request_resident_bytes(&host, &request(), Some(&frames)).unwrap(),
            first_external
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn image_compare_view_frozen_encoded_pair_feeds_real_viewport_pixels_and_cancel() {
        let source = Arc::new(Image {
            width: 2,
            height: 2,
            bgra: vec![
                20, 40, 80, 0, 60, 100, 180, 128, 200, 120, 70, 255, 80, 160, 220, 255,
            ],
            readback_ms: 0.,
        });
        let original = source.bgra.clone();
        let processed = ProcessedImage::new(
            Image {
                width: 2,
                height: 2,
                bgra: original.clone(),
                readback_ms: 0.,
            },
            91,
        );
        let cancel = AtomicBool::new(false);
        let encoded = Arc::new(
            compress::prepare(
                &processed,
                SaveEncoding {
                    format: SaveFormat::Jpeg,
                    jpeg_quality: 10,
                    ..Default::default()
                },
                &cancel,
            )
            .unwrap(),
        );
        let actual = encoded.decode(&cancel).unwrap();
        assert_ne!(actual.bgra, original);
        let loaded = prepare_request(
            CompareRequest::Sources(
                CompareSource::Image(source.clone()),
                CompareSource::Encoded(encoded.clone()),
            ),
            0,
            &cancel,
        )
        .unwrap();
        let CompareWorkResult::Loaded {
            frames, previews, ..
        } = loaded
        else {
            panic!("expected frozen sources");
        };
        drop(encoded);
        assert_eq!(frames.pixels[0].image().bgra, original);
        assert_eq!(frames.pixels[1].image().bgra, actual.bgra);
        assert_eq!(previews.after.as_bytes(0).unwrap(), actual.bgra);
        let key = TileKey {
            generation: 3,
            viewport: CompareViewport::default(),
            panels: [
                Some(CompareRect {
                    x: 0.,
                    y: 0.,
                    width: 4.,
                    height: 4.,
                }),
                Some(CompareRect {
                    x: 10.,
                    y: 0.,
                    width: 4.,
                    height: 4.,
                }),
            ],
            display_scale: 1.,
        };
        let rendered =
            prepare_request(CompareRequest::Tiles(frames.clone(), key), 0, &cancel).unwrap();
        let CompareWorkResult::Tiles(tiles) = rendered else {
            panic!("expected viewport images");
        };
        for side in 0..2 {
            let pixels = frames.pixels[side].image();
            let mut expected = Vec::new();
            for y in 0..4 {
                for x in 0..4 {
                    let start = ((y / 2) * 2 + x / 2) * 4;
                    expected.extend_from_slice(&pixels.bgra[start..start + 4]);
                }
            }
            assert_eq!(
                tiles.images[side].as_ref().unwrap().as_bytes(0).unwrap(),
                expected
            );
            assert!(!tiles.downsampled[side]);
        }
        assert!(matches!(
            prepare_request(
                CompareRequest::Tiles(frames.clone(), key),
                0,
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(source.bgra, original);
        assert_eq!(processed.image().bgra, original);
        assert_eq!(frames.pixels[1].image().bgra, actual.bgra);
    }

    #[test]
    fn image_compare_view_encoded_result_matches_png_jpeg_and_bmp_pixels() {
        let mut bgra = Vec::new();
        for y in 0..24_u32 {
            for x in 0..32_u32 {
                bgra.extend_from_slice(&[
                    (x * 73 + y * 19) as u8,
                    (x * 11 + y * 97) as u8,
                    (x * 53 + y * 41) as u8,
                    if x < 8 { 0 } else { 128 + (y % 2) as u8 * 127 },
                ]);
            }
        }
        let source = Arc::new(Image {
            width: 32,
            height: 24,
            bgra,
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
            73,
        );
        let cancel = AtomicBool::new(false);
        let mut jpeg_previews = Vec::new();
        for (format, jpeg_quality) in [
            (SaveFormat::Png, 90),
            (SaveFormat::Jpeg, 10),
            (SaveFormat::Jpeg, 95),
            (SaveFormat::Bmp, 90),
        ] {
            let encoded = Arc::new(
                compress::prepare(
                    &processed,
                    SaveEncoding {
                        format,
                        jpeg_quality,
                        ..Default::default()
                    },
                    &cancel,
                )
                .unwrap(),
            );
            let decoded = encoded.decode(&cancel).unwrap();
            let before = CompareSource::Image(source.clone());
            let after = CompareSource::Encoded(encoded.clone());
            before.validate().unwrap();
            after.validate().unwrap();
            let previews = prepare_previews(&before, &after, &cancel).unwrap();
            assert_eq!(before.dimensions(), (32, 24));
            assert_eq!(after.dimensions(), (32, 24));
            assert_eq!(previews.before.as_bytes(0).unwrap(), original);
            assert_eq!(previews.after.as_bytes(0).unwrap(), decoded.bgra);
            if matches!(format, SaveFormat::Jpeg | SaveFormat::Bmp) {
                assert_ne!(decoded.bgra, original);
                assert!(decoded.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
                if format == SaveFormat::Bmp {
                    assert_eq!(&decoded.bgra[..4], &[255, 255, 255, 255]);
                } else {
                    jpeg_previews.push(decoded.bgra);
                }
            } else {
                assert_eq!(decoded.bgra, original);
            }
            assert_eq!(encoded.request_id(), 73);
        }
        assert_ne!(jpeg_previews[0], jpeg_previews[1]);
        assert_eq!(source.bgra, original);
        assert_eq!(processed.image().bgra, original);
    }

    #[test]
    fn image_compare_view_encoded_color_only_uses_icc_and_preserves_source_on_cancel() {
        use crate::{
            image_color_management::{ColorConversion, ManagedMetadata, UnappliedReason},
            image_container_metadata::{FieldSummary, MetadataStatus},
            image_export_metadata::{MetadataPolicy, SourceMetadata},
        };
        const LINEAR: &[u8] = include_bytes!("../tests/data/image_color/linear-rgb.icc");
        let source = Arc::new(Image {
            width: 2,
            height: 1,
            bgra: vec![64, 96, 128, 255, 32, 128, 192, 128],
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
            74,
        );
        let encoding = SaveEncoding::default();
        let plain = crate::export::encode_image(&source, encoding).unwrap();
        let mut summary = crate::image_container_metadata::inspect(&plain)
            .unwrap()
            .summary;
        summary.icc = FieldSummary {
            status: MetadataStatus::Present,
            encoded_bytes: LINEAR.len() as u64,
            decoded_bytes: Some(LINEAR.len() as u64),
        };
        let metadata = SourceMetadata::new(
            ManagedMetadata {
                source: summary,
                profile: Some(crate::image_color_profile::inspect(LINEAR).unwrap()),
                color_conversion: ColorConversion::EmbeddedUnapplied(
                    UnappliedReason::TransformFailed,
                ),
            },
            Some(LINEAR),
        )
        .unwrap();
        let facts = metadata.facts();
        let retained = metadata.retained_bytes();
        let cancel = AtomicBool::new(false);
        let encoded = Arc::new(
            compress::prepare_with_metadata(
                &processed,
                encoding,
                MetadataPolicy::ColorOnly,
                Some(&metadata),
                &cancel,
            )
            .unwrap(),
        );
        let decoded = encoded.decode(&cancel).unwrap();
        assert_ne!(decoded.bgra, original);
        let before = CompareSource::Image(source.clone());
        let after = CompareSource::Encoded(encoded.clone());
        let previews = prepare_previews(&before, &after, &cancel).unwrap();
        assert_eq!(previews.before.as_bytes(0).unwrap(), original);
        assert_eq!(previews.after.as_bytes(0).unwrap(), decoded.bgra);
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            prepare_previews(&before, &after, &cancelled),
            Err(ToolError::Cancelled)
        ));
        assert!(matches!(
            after.preview(&cancelled),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(source.bgra, original);
        assert_eq!(processed.image().bgra, original);
        assert_eq!(metadata.facts(), facts);
        assert_eq!(metadata.retained_bytes(), retained);
        assert_eq!(encoded.request_id(), 74);
    }

    #[test]
    fn image_compare_view_preview_size_bounds_long_and_wide_images_without_upscaling() {
        assert_eq!(preview_size(1920, 1080), (960, 540));
        assert_eq!(preview_size(1000, 2000), (300, 600));
        assert_eq!(preview_size(1, 65_535), (1, 600));
        assert_eq!(preview_size(65_535, 1), (960, 1));
        assert_eq!(preview_size(10, 20), (10, 20));
    }

    #[test]
    fn image_compare_view_small_preview_preserves_alpha_and_hidden_rgb() {
        let image = Image {
            width: 2,
            height: 1,
            bgra: vec![200, 10, 20, 0, 30, 40, 50, 128],
            readback_ms: 0.,
        };
        let preview = prepare_preview(&image, &AtomicBool::new(false)).unwrap();
        assert_eq!(u32::from(preview.size(0).width), 2);
        assert_eq!(u32::from(preview.size(0).height), 1);
        assert_eq!(preview.as_bytes(0).unwrap(), image.bgra.as_slice());
        assert!(matches!(
            prepare_preview(&image, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }
}
