//! 可重用 DXGI 采集器；仅把指定全局区域读回 CPU，不创建 GPUI 预览副本。
use crate::{
    capture::{
        cancellation::{CaptureCancelled, CaptureOutcome},
        Image, LiveScreen, Snapshot,
    },
    geometry::{Rect, ScreenRect},
    Result,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC};
pub struct LiveCapture {
    screens: Vec<LiveScreen>,
    pub bounds: ScreenRect,
    region: Option<ScreenRect>,
}
impl LiveCapture {
    /// 只量测持有的 Snapshot 纹理结构；不包含 DXGI duplication、临时 staging 或驱动缓存。
    pub(crate) fn storage_probe(&self) -> (u64, u64, usize) {
        let mut owned_pixels = 0_u64;
        let mut output_pixels = 0_u64;
        for screen in &self.screens {
            let snapshot = &screen.snapshot;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { snapshot.texture().GetDesc(&mut desc) };
            let storage = snapshot.texture_storage_rect();
            debug_assert_eq!((desc.Width, desc.Height), (storage.width, storage.height));
            owned_pixels =
                owned_pixels.saturating_add(u64::from(desc.Width) * u64::from(desc.Height));
            output_pixels = output_pixels
                .saturating_add(u64::from(snapshot.raw_width) * u64::from(snapshot.raw_height));
        }
        (owned_pixels, output_pixels, self.screens.len())
    }

    pub fn new() -> Result<Self> {
        let snapshots = Snapshot::capture_all()?;
        Self::from_snapshots(snapshots, None, &|| false)
    }

    pub(crate) fn new_for_region_cancellable(
        region: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let snapshots = Snapshot::capture_region_cancellable(region, cancelled)?;
        Self::from_snapshots(snapshots, Some(region), cancelled)
    }

    pub(crate) fn new_for_recording(
        region: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CaptureOutcome<Self>> {
        CaptureOutcome::from_result((|| {
            let snapshots = Snapshot::capture_recording_region(region, cancelled)?;
            Self::from_snapshots(snapshots, Some(region), cancelled)
        })())
    }

    pub(crate) fn recording_device(&self) -> &ID3D11Device {
        &self.screens[0].snapshot.device
    }

    /// 单屏未旋转时把自有冻结纹理交给编码器；跨屏/旋转沿已有 CPU 合成合同。
    /// refresh 已等 GPU 拷贝完成并归还每个 Acquire 帧，不把桌面复制租约传给 MF。
    pub(crate) fn recording_texture(
        &self,
        global: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CaptureOutcome<Option<(&ID3D11Texture2D, Rect)>>> {
        CaptureOutcome::from_result(self.recording_texture_inner(global, cancelled))
    }

    fn recording_texture_inner(
        &self,
        global: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<(&ID3D11Texture2D, Rect)>> {
        let [screen] = self.screens.as_slice() else {
            return Ok(None);
        };
        let snapshot = &screen.snapshot;
        if !matches!(snapshot.rotation, crate::geometry::Rotation::Identity) {
            return Ok(None);
        }
        let region = global.local(self.bounds);
        screen.refresh_region_cancellable(region, cancelled)?;
        let storage = snapshot.texture_storage_rect();
        let crop = Rect {
            x: region.x.checked_sub(storage.x).ok_or("录屏纹理原点无效")?,
            y: region.y.checked_sub(storage.y).ok_or("录屏纹理原点无效")?,
            width: region.width,
            height: region.height,
        };
        if !crop.valid(storage.width, storage.height) {
            return Err("录屏纹理范围无效".into());
        }
        Ok(Some((snapshot.texture(), crop)))
    }

    fn from_snapshots(
        snapshots: Vec<Snapshot>,
        region: Option<ScreenRect>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let areas: Vec<_> = snapshots
            .iter()
            .map(|s| ScreenRect {
                left: s.desktop.left,
                top: s.desktop.top,
                width: s.width,
                height: s.height,
            })
            .collect();
        let bounds =
            ScreenRect::union(&areas).ok_or(tr!("未找到活动屏幕", "No active display found"))?;
        if let Some(region) = region {
            if !region.local(bounds).valid(bounds.width, bounds.height) {
                return Err(tr!(
                    "采集范围超出活动屏幕布局",
                    "Capture area is outside the active displays"
                )
                .into());
            }
        }
        let mut screens = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            if cancelled() {
                return Err(CaptureCancelled.into());
            }
            screens.push(LiveScreen::new(snapshot)?);
        }
        if cancelled() {
            return Err(CaptureCancelled.into());
        }
        Ok(Self {
            screens,
            bounds,
            region,
        })
    }
    pub fn read(&self, global: ScreenRect) -> Result<Image> {
        self.read_inner(global, None)
    }

    pub(crate) fn read_cancellable(
        &self,
        global: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Image> {
        self.read_inner(global, Some(cancelled))
    }

    pub(crate) fn read_recording(
        &self,
        global: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CaptureOutcome<Image>> {
        CaptureOutcome::from_result(self.read_cancellable(global, cancelled))
    }

    fn read_inner(
        &self,
        global: ScreenRect,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<Image> {
        let started = std::time::Instant::now();
        let never_cancel = || false;
        let cancel = cancelled.unwrap_or(&never_cancel);
        if cancel() {
            return Err(CaptureCancelled.into());
        }
        if let Some(clip) = self.region {
            if global.left < clip.left
                || global.top < clip.top
                || global.left as i64 + global.width as i64 > clip.left as i64 + clip.width as i64
                || global.top as i64 + global.height as i64 > clip.top as i64 + clip.height as i64
            {
                return Err(tr!(
                    "采集范围超出初始化选区，请重新建立采集器",
                    "Capture area exceeds the initial selection; restart capture"
                )
                .into());
            }
        }
        let region = global.local(self.bounds);
        if !region.valid(self.bounds.width, self.bounds.height) {
            return Err(tr!(
                "采集范围超出当前屏幕布局",
                "Capture area is outside the current displays"
            )
            .into());
        }
        let bytes = (region.width as usize)
            .checked_mul(region.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(tr!("采集范围过大", "Capture area is too large"))?;
        // 单个显示器的并集原点就是该屏原点，Snapshot 已完成旋转、行 padding 和 alpha 处理。
        // 直接移交选区读回，避免常见长截图路径额外分配并复制同尺寸的合成帧。
        if let [screen] = self.screens.as_slice() {
            let mut image = if self.region.is_some() || cancelled.is_some() {
                screen.refresh_region_cancellable(region, cancel)?;
                screen.snapshot.read_region_cancellable(region, cancel)?
            } else {
                screen.refresh()?;
                screen.snapshot.read_region(region)?
            };
            if cancel() {
                return Err(CaptureCancelled.into());
            }
            image.readback_ms = started.elapsed().as_secs_f64() * 1000.;
            return Ok(image);
        }
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(bytes)
            .map_err(|_| tr!("采集选区内存不足", "Not enough memory for the capture area"))?;
        bgra.resize(bytes, 0);
        let mut image = Image {
            width: region.width,
            height: region.height,
            bgra,
            readback_ms: 0.,
        };
        for p in image.bgra.chunks_exact_mut(4) {
            p[3] = 255;
        }
        for screen in &self.screens {
            if cancel() {
                return Err(CaptureCancelled.into());
            }
            let snapshot = &screen.snapshot;
            let area = ScreenRect {
                left: snapshot.desktop.left,
                top: snapshot.desktop.top,
                width: snapshot.width,
                height: snapshot.height,
            }
            .local(self.bounds);
            if let Some(part) = area.intersection(region) {
                let local = Rect {
                    x: part.x - area.x,
                    y: part.y - area.y,
                    width: part.width,
                    height: part.height,
                };
                let tile = if self.region.is_some() || cancelled.is_some() {
                    screen.refresh_region_cancellable(local, cancel)?;
                    snapshot.read_region_cancellable(local, cancel)?
                } else {
                    screen.refresh()?;
                    snapshot.read_region(local)?
                };
                for y in 0..part.height {
                    if y.is_multiple_of(64) && cancel() {
                        return Err(CaptureCancelled.into());
                    }
                    let src = (y * part.width * 4) as usize;
                    let dst =
                        (((part.y - region.y + y) * region.width + part.x - region.x) * 4) as usize;
                    image.bgra[dst..dst + part.width as usize * 4]
                        .copy_from_slice(&tile.bgra[src..src + part.width as usize * 4]);
                }
            }
        }
        if cancel() {
            return Err(CaptureCancelled.into());
        }
        image.readback_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(image)
    }
}
