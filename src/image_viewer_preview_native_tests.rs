//! 自有窗口的超限PNG预览、局部交付与framebuffer像素检查；不读取桌面。
use super::*;
use gpui::{AsyncApp, WeakEntity, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle},
    rc::Rc,
    sync::Weak,
    time::{SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HANDLE, HWND, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::WaitForSingleObject,
    UI::WindowsAndMessaging::IsWindow,
};

pub(super) type ImageWindow = WindowHandle<ImageViewer>;
const WAIT: Duration = Duration::from_secs(20);
const LARGE: (u32, u32) = (8000, 4500);
const ENCODED_LIMIT: usize = 4 * 1024 * 1024;
const RGB: [[u8; 3]; 4] = [[24, 48, 72], [84, 40, 28], [32, 76, 44], [68, 36, 88]];
const SMALL: (u32, u32) = (16, 8);
const SMALL_RGB: [u8; 3] = [38, 62, 70];
const PATCH: (u32, u32) = (4100, 2256);
const PATCH_RGB: [u8; 3] = [76, 68, 22];

fn require(condition: bool, message: &str) -> crate::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct LimitedFile {
    file: File,
    bytes: usize,
}

impl Write for LimitedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > ENCODED_LIMIT.saturating_sub(self.bytes) {
            return Err(io::Error::other("PNG fixture exceeds 4 MiB"));
        }
        let count = self.file.write(bytes)?;
        self.bytes += count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

struct Fixtures {
    root: PathBuf,
    temp: PathBuf,
    large: PathBuf,
    small: PathBuf,
}

impl Fixtures {
    fn new(case: NativeCase) -> crate::Result<Self> {
        require(
            u64::from(LARGE.0) * u64::from(LARGE.1) > crate::image_decode::MAX_DECODE_PIXELS,
            "The private source must exceed the ordinary full-image pixel gate",
        )?;
        let temp = std::env::temp_dir();
        crate::image_collection::ensure_local_directory(&temp)?;
        let temp = fs::canonicalize(temp)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let mut created = None;
        for attempt in 0..8 {
            let root = temp.join(format!(
                "rustshot-preview-native-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    created = Some(root);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let root = created.ok_or("Unable to create the private preview fixture directory")?;
        let fixtures = Self {
            large: root.join("large.png"),
            small: root.join("small.png"),
            root,
            temp,
        };
        fs::create_dir(fixtures.root.join("data"))?;
        if matches!(case, NativeCase::Preview) {
            fixtures.write_png(&fixtures.large, LARGE, true)?;
            fixtures.write_png(&fixtures.small, SMALL, false)?;
        }
        Ok(fixtures)
    }

    fn write_png(&self, path: &Path, dimensions: (u32, u32), large: bool) -> crate::Result<()> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut output = LimitedFile { file, bytes: 0 };
        let mut encoder = png::Encoder::new(&mut output, dimensions.0, dimensions.1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        // 仅保留一行32000B原始RGBA；不生成8000×4500的完整数组。
        let mut row = vec![0; dimensions.0 as usize * 4];
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let rgb = if large { RGB[x / 2000] } else { SMALL_RGB };
            pixel.copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        {
            let mut stream = writer.stream_writer()?;
            for y in 0..dimensions.1 {
                if large && (PATCH.1..PATCH.1 + 4).contains(&y) {
                    let color = if y < PATCH.1 + 3 {
                        PATCH_RGB
                    } else {
                        RGB[(PATCH.0 / 2000) as usize]
                    };
                    for x in PATCH.0..PATCH.0 + 3 {
                        let index = x as usize * 4;
                        row[index..index + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
                    }
                }
                stream.write_all(&row)?;
            }
            stream.finish()?;
        }
        writer.finish()?;
        require(
            output.bytes <= ENCODED_LIMIT,
            "Encoded fixture is too large",
        )
    }

    fn cleanup(&self) -> crate::Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        let actual = fs::canonicalize(&self.root)?;
        require(
            actual.parent() == Some(self.temp.as_path())
                && actual.file_name() == self.root.file_name(),
            "Refusing to remove a fixture directory outside its private temporary location",
        )?;
        fs::remove_dir_all(actual)?;
        Ok(())
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("Preview fixture cleanup failed: {error}");
        }
    }
}

struct DataDirectory(Option<OsString>);

impl DataDirectory {
    fn install(path: &Path) -> Self {
        let previous = std::env::var_os("RUSTSHOT_DATA_DIR");
        std::env::set_var("RUSTSHOT_DATA_DIR", path);
        Self(previous)
    }
}

impl Drop for DataDirectory {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("RUSTSHOT_DATA_DIR", value),
            None => std::env::remove_var("RUSTSHOT_DATA_DIR"),
        }
    }
}

struct KeepAlive;

impl Render for KeepAlive {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        div()
            .size_full()
            .bg(rgb(crate::theme::palette(cx).background))
    }
}

#[derive(Default)]
pub(super) struct Lifecycle {
    window: Cell<Option<ImageWindow>>,
    native: Cell<isize>,
    entity: RefCell<Option<WeakEntity<ImageViewer>>>,
    worker: RefCell<Option<Weak<(Mutex<Mailbox>, Condvar)>>>,
    worker_native: RefCell<Option<OwnedHandle>>,
    source: RefCell<Option<Weak<crate::image_preview::source::FrozenPreviewSource>>>,
    images: RefCell<Vec<Weak<RenderImage>>>,
}

impl Lifecycle {
    pub(super) fn remember_image(&self, image: &Arc<RenderImage>) {
        let weak = Arc::downgrade(image);
        let mut images = self.images.borrow_mut();
        if !images.iter().any(|previous| previous.ptr_eq(&weak)) {
            images.push(weak);
        }
    }

    pub(super) fn old_pixels_pending(&self) -> bool {
        self.source
            .borrow()
            .as_ref()
            .is_some_and(|source| source.upgrade().is_some())
            || self
                .images
                .borrow()
                .iter()
                .any(|image| image.upgrade().is_some())
    }

    fn closing_pending(&self) -> bool {
        self.old_pixels_pending()
            || self
                .entity
                .borrow()
                .as_ref()
                .is_some_and(|entity| entity.upgrade().is_some())
            || self
                .worker
                .borrow()
                .as_ref()
                .is_some_and(|worker| worker.upgrade().is_some())
            || (self.native.get() != 0
                && unsafe { IsWindow(Some(HWND(self.native.get() as *mut _))) }.as_bool())
    }

    fn worker_finished(&self) -> crate::Result<bool> {
        let worker = self.worker_native.borrow();
        let worker = worker
            .as_ref()
            .ok_or("Missing duplicate of the real viewer worker handle")?;
        let result = unsafe { WaitForSingleObject(HANDLE(worker.as_raw_handle()), 0) };
        if result == WAIT_OBJECT_0 {
            Ok(true)
        } else if result == WAIT_TIMEOUT {
            Ok(false)
        } else if result == WAIT_FAILED {
            Err(io::Error::last_os_error().into())
        } else {
            Err(format!("Unexpected worker wait result: {}", result.0).into())
        }
    }
}

#[derive(Clone, Copy)]
struct PreviewProbe {
    serial: u64,
    area: Rect,
    zoom: f64,
    offset: (f64, f64),
    raster: (u32, u32),
}

pub(super) async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(16))
        .await;
}

pub(super) async fn wait_view<T>(
    window: ImageWindow,
    phase: &str,
    cx: &mut AsyncApp,
    mut check: impl FnMut(
        &mut ImageViewer,
        &mut Window,
        &mut Context<ImageViewer>,
    ) -> crate::Result<Option<T>>,
) -> crate::Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = window.update(cx, |view, window, cx| check(view, window, cx))?? {
            return Ok(value);
        }
        require(
            Instant::now() < deadline,
            &format!("Preview native phase timed out: {phase}"),
        )?;
        tick(cx).await;
    }
}

fn bgra(x: u32) -> [u8; 4] {
    let [r, g, b] = RGB[(x / 2000).min(3) as usize];
    [b, g, r, 255]
}

fn patch_bgra() -> [u8; 4] {
    let [r, g, b] = PATCH_RGB;
    [b, g, r, 255]
}

fn verify_cpu_preview(view: &ImageViewer, lifecycle: &Lifecycle) -> crate::Result<()> {
    let image = view.image.as_ref().ok_or("Missing loaded preview")?;
    let geometry = image
        .preview_geometry
        .ok_or("Large PNG did not use Preview")?;
    require(
        geometry.encoded_dimensions() == LARGE,
        "Preview lost encoded dimensions",
    )?;
    require(
        geometry.normalized_source_dimensions() == LARGE,
        "Preview lost source dimensions",
    )?;
    require(
        !image.has_full_pixels()
            && image.raw_page.is_none()
            && image.raw_frame.is_none()
            && image.metadata_source.is_none(),
        "Sampled pixels acquired full-image or metadata-export qualification",
    )?;
    let source = image
        .preview_source
        .as_ref()
        .ok_or("Missing frozen preview source")?;
    *lifecycle.source.borrow_mut() = Some(Arc::downgrade(source));
    let (width, height) = geometry.raster_dimensions();
    require(
        width <= 2048 && u64::from(width) * u64::from(height) <= 4_000_000,
        "Overview exceeds its raster cap",
    )?;
    require(
        image.tiles.len() == 1,
        "Overview must have one sampled texture",
    )?;
    let texture = &image.tiles[0].1;
    lifecycle.remember_image(texture);
    let bytes = texture.as_bytes(0).ok_or("Overview has no CPU pixels")?;
    require(
        bytes.len() == width as usize * height as usize * 4,
        "Overview byte count mismatch",
    )?;
    for x in [width / 8, width * 3 / 8, width * 5 / 8, width * 7 / 8] {
        // 独立整数中心公式，不用生产采样函数生成自己的期望。
        let source_x =
            ((2 * u64::from(x) + 1) * u64::from(LARGE.0) / (2 * u64::from(width))) as u32;
        let index = (u64::from(height / 2) * u64::from(width) + u64::from(x)) as usize * 4;
        require(
            bytes[index..index + 4] == bgra(source_x),
            "Overview differs from the literal source bands",
        )?;
    }
    // 依据实际交付栅格检查标记周围的总览像素；不假定默认栅格永远为2048宽。
    let marker_x = (u64::from(PATCH.0 + 1) * u64::from(width) / u64::from(LARGE.0)) as u32;
    let marker_y = (u64::from(PATCH.1 + 1) * u64::from(height) / u64::from(LARGE.1)) as u32;
    for y in marker_y.saturating_sub(2)..(marker_y + 3).min(height) {
        for x in marker_x.saturating_sub(2)..(marker_x + 3).min(width) {
            let source_x =
                ((2 * u64::from(x) + 1) * u64::from(LARGE.0) / (2 * u64::from(width))) as u32;
            let source_y =
                ((2 * u64::from(y) + 1) * u64::from(LARGE.1) / (2 * u64::from(height))) as u32;
            require(
                !(PATCH.0..PATCH.0 + 3).contains(&source_x)
                    || !(PATCH.1..PATCH.1 + 3).contains(&source_y),
                "Overview sampling unexpectedly covers the independent detail marker",
            )?;
            let index = (u64::from(y) * u64::from(width) + u64::from(x)) as usize * 4;
            require(
                bytes[index..index + 4] == bgra(PATCH.0),
                "Overview contains the high-resolution-only marker",
            )?;
        }
    }
    Ok(())
}

async fn capture_uniform(
    window: ImageWindow,
    projected: (f64, f64),
    area: Rect,
    expected: [u8; 4],
    phase: &str,
    cx: &mut AsyncApp,
) -> crate::Result<()> {
    require(
        projected.0.is_finite() && projected.1.is_finite(),
        "Nonfinite GPU sample coordinates",
    )?;
    require(
        projected.0 >= 1.
            && projected.1 >= 1.
            && projected.0 < u32::MAX as f64
            && projected.1 < u32::MAX as f64,
        "GPU sample is outside physical coordinates",
    )?;
    let rect = [
        projected.0.floor() as u32 - 1,
        projected.1.floor() as u32 - 1,
        3,
        3,
    ];
    require(
        rect[0] >= area.x
            && rect[1] >= area.y
            && u64::from(rect[0]) + 3 <= u64::from(area.x) + u64::from(area.width)
            && u64::from(rect[1]) + 3 <= u64::from(area.y) + u64::from(area.height),
        "GPU sample falls outside the actual image area",
    )?;
    let bytes = capture_pixels(window, rect, phase, cx).await?;
    let difference = bytes
        .chunks_exact(4)
        .flat_map(|pixel| {
            pixel
                .iter()
                .zip(expected)
                .map(|(actual, expected)| actual.abs_diff(expected))
        })
        .max()
        .unwrap_or(0);
    require(difference <= 1, &format!("{phase} GPU pixels differ: rect={rect:?}, center={projected:?}, area={area:?}, actual={bytes:?}, expected={expected:?}, max_difference={difference}"))?;
    eprintln!("preview native phase={phase} physical_rect={rect:?} actual_bgra={bytes:?} expected_bgra={expected:?} tolerance=1 owned_framebuffer=true");
    Ok(())
}

pub(super) async fn capture_pixels(
    window: ImageWindow,
    rect: [u32; 4],
    phase: &str,
    cx: &mut AsyncApp,
) -> crate::Result<Vec<u8>> {
    require(
        rect[2] == 3 && rect[3] == 3,
        "Owned framebuffer check is limited to 3x3 pixels",
    )?;
    window.update(cx, |_, window, _| window.request_frame_capture(rect))??;
    let deadline = Instant::now() + WAIT;
    let bytes = loop {
        tick(cx).await;
        if let Some(result) = window.update(cx, |_, window, _| window.take_frame_capture())? {
            break result.map_err(|error| error.to_string())?;
        }
        require(
            Instant::now() < deadline,
            &format!("Owned framebuffer readback timed out: {phase}"),
        )?;
    };
    require(
        bytes.len() == 36,
        "Owned framebuffer must return 36 BGRA bytes",
    )?;
    Ok(bytes)
}

async fn exercise(
    window: ImageWindow,
    small: PathBuf,
    lifecycle: &Lifecycle,
    cx: &mut AsyncApp,
) -> crate::Result<()> {
    let fit = wait_view(window, "Main Preview", cx, |view, window, cx| {
        if view.loading {
            return Ok(None);
        }
        if view.image.is_none() {
            return Err(format!("Large PNG load failed: {:?}", view.error).into());
        }
        verify_cpu_preview(view, lifecycle)?;
        view.auto_fit = true;
        view.fit(window);
        let area = image_area(window);
        let viewport = view
            .preview_viewport
            .ok_or("Preview has no source viewport")?;
        let expected_zoom = (f64::from(area.width) / f64::from(LARGE.0))
            .min(f64::from(area.height) / f64::from(LARGE.1))
            .min(1.);
        let expected_offset = (
            f64::from(area.x) + (f64::from(area.width) - f64::from(LARGE.0) * expected_zoom) / 2.,
            f64::from(area.y) + (f64::from(area.height) - f64::from(LARGE.1) * expected_zoom) / 2.,
        );
        require(
            (viewport.zoom() - expected_zoom).abs() < 1e-9
                && (viewport.offset().0 - expected_offset.0).abs() < 1e-6
                && (viewport.offset().1 - expected_offset.1).abs() < 1e-6,
            "Fit differs from independent physical geometry",
        )?;
        let raster = view
            .image
            .as_ref()
            .unwrap()
            .preview_geometry
            .unwrap()
            .raster_dimensions();
        cx.notify();
        window.refresh();
        Ok(Some(PreviewProbe {
            serial: view.serial,
            area,
            zoom: expected_zoom,
            offset: expected_offset,
            raster,
        }))
    })
    .await?;
    for x in [1000, 3000, 5000, 7000] {
        // 扩一枚overview texel和2源像素，独立验证3×3过滤支持仍在同一实心色带内。
        let margin = 2. + f64::from(LARGE.0) / f64::from(fit.raster.0) + 3. / fit.zoom;
        require(
            margin < 1000. && margin < 2250.,
            "Fit has no stable source-band footprint",
        )?;
        capture_uniform(
            window,
            (
                fit.offset.0 + (f64::from(x) + 0.5) * fit.zoom,
                fit.offset.1 + 2250.5 * fit.zoom,
            ),
            fit.area,
            bgra(x),
            "fit",
            cx,
        )
        .await?;
    }
    window.update(cx, |view, window, cx| -> crate::Result<()> {
        view.zoom(1., area_center(window));
        let viewport = view
            .preview_viewport
            .as_mut()
            .ok_or("Original-size Preview lost its viewport")?;
        let offset = viewport.offset();
        // 原尺寸动作之后把物理pan落在整数边界；标记的3×3采样完全位于三个源像素内。
        viewport.set_offset((offset.0.round(), offset.1.round()))?;
        cx.notify();
        window.refresh();
        Ok(())
    })??;
    let roi = wait_view(window, "original-size ROI", cx, |view, window, _| {
        require(
            view.serial == fit.serial && !view.loading,
            "Main changed during ROI delivery",
        )?;
        require(view.preview_paint_error.is_none(), "Preview paint failed")?;
        if let Some(error) = view.preview_region.error() {
            return Err(error.to_owned().into());
        }
        if view.preview_region.pending() {
            return Ok(None);
        }
        let Some(surface) = view.preview_region.surface() else {
            return Ok(None);
        };
        let viewport = view.preview_viewport.ok_or("ROI has no source viewport")?;
        let source = view
            .image
            .as_ref()
            .and_then(|image| image.preview_source.as_ref())
            .ok_or("ROI lost its frozen source")?;
        let original = lifecycle
            .source
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or("Original frozen source was released before ROI")?;
        require(
            Arc::ptr_eq(source, &original),
            "ROI replaced the original frozen source",
        )?;
        require(
            viewport.zoom() == 1.,
            "Original-size action did not preserve physical 1:1 zoom",
        )?;
        let region = surface.geometry.normalized_source_region();
        require(
            surface.geometry.raster_dimensions() == (region.width, region.height),
            "This actual window exceeds the 1:1 ROI test raster cap",
        )?;
        require(
            viewport.visible_region(image_area(window), 2)? == Some(region),
            "ROI is not the actual visible source region",
        )?;
        lifecycle.remember_image(&surface.image);
        let bytes = surface.image.as_bytes(0).ok_or("ROI has no CPU pixels")?;
        require(
            PATCH.0 >= region.x
                && PATCH.1 >= region.y
                && PATCH.0 + 3 <= region.x + region.width
                && PATCH.1 + 3 <= region.y + region.height,
            "The detail marker is not fully visible in this actual window",
        )?;
        for y in PATCH.1..PATCH.1 + 3 {
            for x in PATCH.0..PATCH.0 + 3 {
                let index = (u64::from(y - region.y) * u64::from(region.width)
                    + u64::from(x - region.x)) as usize
                    * 4;
                require(
                    bytes.get(index..index + 4) == Some(patch_bgra().as_slice()),
                    "ROI did not recover the high-resolution source marker",
                )?;
            }
        }
        for x in [region.x + region.width / 4, region.x + region.width * 3 / 4] {
            let y = region.y + region.height / 2;
            let index = (u64::from(y - region.y) * u64::from(region.width)
                + u64::from(x - region.x)) as usize
                * 4;
            require(
                bytes.get(index..index + 4) == Some(bgra(x).as_slice()),
                "ROI differs from literal source bands",
            )?;
        }
        Ok(Some((
            region,
            PreviewProbe {
                serial: view.serial,
                area: image_area(window),
                zoom: 1.,
                offset: viewport.offset(),
                raster: surface.geometry.raster_dimensions(),
            },
        )))
    })
    .await?;
    for x in [roi.0.x + roi.0.width / 4, roi.0.x + roi.0.width * 3 / 4] {
        let y = roi.0.y + roi.0.height / 2;
        require(
            x % 2000 > 8 && x % 2000 < 1992,
            "ROI GPU anchor is too close to a color boundary",
        )?;
        capture_uniform(
            window,
            (
                roi.1.offset.0 + f64::from(x) + 0.5,
                roi.1.offset.1 + f64::from(y) + 0.5,
            ),
            roi.1.area,
            bgra(x),
            "ROI",
            cx,
        )
        .await?;
    }
    require(
        roi.1.offset.0.fract() == 0. && roi.1.offset.1.fract() == 0.,
        "Marker capture requires integer physical offsets",
    )?;
    capture_uniform(
        window,
        (
            roi.1.offset.0 + f64::from(PATCH.0) + 1.5,
            roi.1.offset.1 + f64::from(PATCH.1) + 1.5,
        ),
        roi.1.area,
        patch_bgra(),
        "ROI-only marker",
        cx,
    )
    .await?;
    window.update(cx, |view, _, cx| view.open_path(small.clone(), true, cx))?;
    let full = wait_view(window, "replacement Full", cx, |view, window, cx| {
        if view.loading {
            return Ok(None);
        }
        let image = view.image.as_ref().ok_or("Replacement image failed")?;
        require(
            view.serial != fit.serial
                && image.has_full_pixels()
                && image.preview_source.is_none()
                && view.preview_region.surface().is_none(),
            "Replacement retained the old Preview/ROI identity",
        )?;
        let raw = image
            .raw_page
            .as_ref()
            .ok_or("Replacement Full has no raw pixels")?;
        require(
            (raw.width, raw.height) == SMALL
                && raw.bgra
                    == [SMALL_RGB[2], SMALL_RGB[1], SMALL_RGB[0], 255]
                        .repeat(SMALL.0 as usize * SMALL.1 as usize),
            "Replacement CPU pixels are not the private small PNG",
        )?;
        view.auto_fit = true;
        view.fit(window);
        view.zoom(8., area_center(window));
        let viewport = view.viewport;
        cx.notify();
        window.refresh();
        Ok(Some((viewport, image_area(window))))
    })
    .await?;
    let point = full.0.window_point(Point {
        x: SMALL.0 as f32 / 2.,
        y: SMALL.1 as f32 / 2.,
    });
    require(
        full.0.zoom >= 4.,
        "Replacement is too small for a stable 3x3 sample",
    )?;
    // 放大完整图仍可用线性过滤；独立核三枚物理像素中心的支持远离纹理边缘。
    let zoom = f64::from(full.0.zoom);
    let left = (f64::from(point.x).floor() - 0.5 - f64::from(full.0.offset.x)) / zoom;
    let top = (f64::from(point.y).floor() - 0.5 - f64::from(full.0.offset.y)) / zoom;
    let right = (f64::from(point.x).floor() + 1.5 - f64::from(full.0.offset.x)) / zoom;
    let bottom = (f64::from(point.y).floor() + 1.5 - f64::from(full.0.offset.y)) / zoom;
    require(
        left >= 1.5
            && top >= 1.5
            && right <= f64::from(SMALL.0) - 1.5
            && bottom <= f64::from(SMALL.1) - 1.5,
        "Replacement 3x3 capture must stay inside the linear-filter texture interior",
    )?;
    capture_uniform(
        window,
        (f64::from(point.x), f64::from(point.y)),
        full.1,
        [SMALL_RGB[2], SMALL_RGB[1], SMALL_RGB[0], 255],
        "replacement",
        cx,
    )
    .await?;
    let deadline = Instant::now() + WAIT;
    while lifecycle.old_pixels_pending() {
        require(
            Instant::now() < deadline,
            "Old overview/detail/source references survived replacement",
        )?;
        window.update(cx, |_, window, _| window.refresh())?;
        tick(cx).await;
    }
    window.update(cx, |view, _, _| {
        if let Some(image) = &view.image {
            for (_, texture) in &image.tiles {
                lifecycle.remember_image(texture);
            }
        }
    })?;
    Ok(())
}

async fn cleanup(lifecycle: &Lifecycle, cx: &mut AsyncApp) -> crate::Result<()> {
    if let Some(window) = lifecycle.window.take() {
        let _ = window.update(cx, |view, window, _| {
            view.closing = true;
            window.remove_window();
        });
    }
    let deadline = Instant::now() + WAIT;
    while lifecycle.closing_pending() || !lifecycle.worker_finished()? {
        require(
            Instant::now() < deadline,
            "Viewer HWND/entity/shared owner did not release during cleanup",
        )?;
        tick(cx).await;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum NativeCase {
    Preview,
    Edges,
}

fn run_native_window(case: NativeCase) -> crate::Result<()> {
    let fixtures = Fixtures::new(case)?;
    let edges = match case {
        NativeCase::Preview => None,
        NativeCase::Edges => Some(super::edge_native_tests::prepare(&fixtures.root)?),
    };
    let data_directory = DataDirectory::install(&fixtures.root.join("data"));
    let initial = edges.as_ref().map_or_else(
        || fixtures.large.clone(),
        |fixtures| fixtures.initial.clone(),
    );
    let small = fixtures.small.clone();
    let outcome = Rc::new(RefCell::new(Some(
        "Preview native check did not complete".to_owned(),
    )));
    let finished = outcome.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move |cx: &mut App| {
        crate::theme::install(cx);
        let result = (|| -> crate::Result<_> {
            cx.open_window(WindowOptions { show: false, focus: false, ..Default::default() }, |_, cx| cx.new(|_| KeepAlive))?;
            let bounds = Bounds::centered(None, size(px(640.), px(560.)), cx);
            open_viewer(Loader::new()?, Some(initial), WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), titlebar: Some(TitlebarOptions::default()), focus: false, ..Default::default() }, cx)
        })();
        let window = match result {
            Ok(window) => window,
            Err(error) => {
                *outcome.borrow_mut() = Some(error.to_string());
                for window in cx.windows() { let _ = cx.update_window(window, |_, window, _| window.remove_window()); }
                cx.quit();
                return;
            }
        };
        let lifecycle = Rc::new(Lifecycle::default());
        lifecycle.window.set(Some(window));
        let observation = window.update(cx, |view, window, cx| -> crate::Result<()> {
            lifecycle.native.set(crate::gpui_capture::hwnd(window).0 as isize);
            *lifecycle.entity.borrow_mut() = Some(cx.weak_entity());
            *lifecycle.worker.borrow_mut() = Some(Arc::downgrade(&view.loader.shared));
            let thread = view.loader.thread.as_ref().ok_or("Missing real viewer worker")?;
            let borrowed = unsafe { BorrowedHandle::borrow_raw(thread.as_raw_handle()) };
            *lifecycle.worker_native.borrow_mut() = Some(borrowed.try_clone_to_owned()?);
            Ok(())
        });
        let observation_error = match observation {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        let outcome = outcome.clone();
        cx.spawn(async move |cx| {
            let result = match observation_error {
                Some(error) => Err(error.into()),
                None => match edges.as_ref() {
                    None => exercise(window, small, &lifecycle, cx).await,
                    Some(fixtures) => super::edge_native_tests::exercise(window, fixtures, &lifecycle, cx).await,
                },
            };
            let cleaned = cleanup(&lifecycle, cx).await;
            *outcome.borrow_mut() = match (result, cleaned) {
                (Ok(()), Ok(())) => {
                    match case {
                        NativeCase::Preview => eprintln!("preview native: CPU Preview/ROI and owned_framebuffer_pixels_verified=true; ROI_only_marker_verified=true; old_source_and_textures_released=true; HWND_and_entity_released=true; worker_shared_owner_released=true; real_worker_handle_signaled=true; desktop_pixels_read=false; DWM_presentation_verified=false; GPU_RSS_verified=false"),
                        NativeCase::Edges => eprintln!("edge native: owned_framebuffer_pixels_verified=true; old_textures_released=true; HWND_and_entity_released=true; worker_shared_owner_released=true; real_worker_handle_signaled=true; desktop_pixels_read=false; DWM_presentation_verified=false; GPU_RSS_verified=false"),
                    }
                    None
                }
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Some(error.to_string()),
                (Err(error), Err(cleanup)) => Some(format!("{error}; cleanup failed: {cleanup}")),
            };
            let _ = cx.update(|cx| {
                for window in cx.windows() { let _ = cx.update_window(window, |_, window, _| window.remove_window()); }
                cx.quit();
            });
        }).detach();
    });
    drop(data_directory);
    let result = finished
        .borrow_mut()
        .take()
        .map_or(Ok(()), |error| Err(error.into()));
    let cleanup = fixtures.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(format!("{error}; fixture cleanup failed: {cleanup}").into())
        }
    }
}

pub(super) fn run_edge_window() -> crate::Result<()> {
    run_native_window(NativeCase::Edges)
}

#[test]
#[ignore = "显式单独运行超32MP PNG真实窗口/ROI/framebuffer检查；自有文件，无物理输入或桌面采集"]
fn image_viewer_preview_native_large_png_roi_and_lifecycle() {
    run_native_window(NativeCase::Preview).expect("PNG Preview native window check failed");
}
