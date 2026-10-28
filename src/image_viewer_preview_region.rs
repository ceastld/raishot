//! 局部预览的请求身份、合并和串行生产桥；工作线程与呈现账本由宿主负责。
use crate::{
    geometry::Rect,
    image_preview::{
        png::{region, PreviewColorStatus, PreviewLimits, PreviewSampling},
        region_geometry::RegionGeometry,
        source::FrozenPreviewSource,
    },
};
use gpui::RenderImage;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const DEBOUNCE: Duration = Duration::from_millis(120);
const SURFACE_CONTROL: u64 = 64 * 1024;

pub(super) struct Request {
    pub(super) serial: u64,
    pub(super) epoch: u64,
    pub(super) source: Arc<FrozenPreviewSource>,
    pub(super) region: Rect,
    /// 排除source整个retained_bytes，含其它来源、旧像素、完成槽与退役纹理。
    pub(super) external_other_resident: u64,
    pub(super) cancel: Arc<AtomicBool>,
}

pub(super) struct Completion {
    pub(super) serial: u64,
    pub(super) epoch: u64,
    pub(super) source: Arc<FrozenPreviewSource>,
    pub(super) region: Rect,
    pub(super) result: Result<Surface, String>,
}

#[derive(Clone)]
pub(super) struct Surface {
    pub(super) geometry: RegionGeometry,
    /// 直alpha的BGRA，不混合棋盘，不授予完整源图资格。
    pub(super) image: Arc<RenderImage>,
}

/// 无线程和窗口副作用；所有大像素由region producer计费，RenderImage接管同一Vec。
pub(super) fn complete(request: Request) -> Completion {
    let Request {
        serial,
        epoch,
        source,
        region,
        external_other_resident,
        cancel,
    } = request;
    let result = (|| {
        check_cancel(&cancel)?;
        let plan = region::inspect_region(
            &source,
            region,
            PreviewLimits::default(),
            external_other_resident,
            &cancel,
        )
        .map_err(|error| error.to_string())?;
        let expected_geometry = plan.geometry();
        let decoded = region::decode_region(plan, &cancel).map_err(|error| error.to_string())?;
        check_cancel(&cancel)?;
        let dimensions = decoded.geometry.raster_dimensions();
        let expected_bytes = u64::from(dimensions.0)
            .checked_mul(u64::from(dimensions.1))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(invalid)?;
        if decoded.geometry != expected_geometry
            || decoded.geometry.normalized_source_region() != region
            || decoded.source != source.source()
            || decoded.sampling != PreviewSampling::PixelCenterNearest
            || decoded.color != PreviewColorStatus::NotColorManaged
            || (decoded.image.width, decoded.image.height) != dimensions
            || decoded.image.bgra.len() as u64 != expected_bytes
            || decoded.image.bgra.capacity() as u64 > expected_bytes
        {
            return Err(invalid());
        }
        // 后端已释放，只再保留来源、唯一输出Vec和小型Surface/RenderImage控制结构。
        let final_bytes = external_other_resident
            .checked_add(source.retained_bytes())
            .and_then(|bytes| bytes.checked_add(decoded.image.bgra.capacity() as u64))
            .and_then(|bytes| bytes.checked_add(SURFACE_CONTROL))
            .ok_or_else(budget)?;
        if final_bytes > crate::image_decode::MAX_DECODE_BYTES {
            return Err(budget());
        }
        check_cancel(&cancel)?;
        let image = crate::canvas_source::render_image(
            decoded.image.width,
            decoded.image.height,
            decoded.image.bgra,
        );
        check_cancel(&cancel)?;
        Ok(Surface {
            geometry: decoded.geometry,
            image,
        })
    })();
    let result = match check_cancel(&cancel) {
        Err(error) => Err(error),
        Ok(()) => result,
    };
    Completion {
        serial,
        epoch,
        source,
        region,
        result,
    }
}

struct Target {
    serial: u64,
    source: Arc<FrozenPreviewSource>,
    region: Rect,
}

#[derive(Default)]
pub(super) struct State {
    epoch: u64,
    latest_target: Option<Target>,
    due: Option<Instant>,
    inflight: bool,
    current_surface: Option<Surface>,
    error: Option<String>,
}

pub(super) struct TargetChange {
    pub(super) changed: bool,
    pub(super) retired: Option<Arc<RenderImage>>,
}

impl State {
    /// changed时宿主撤销旧worker旗标；同源换区域保留旧详情，等待新结果覆盖。
    pub(super) fn set_target(
        &mut self,
        serial: u64,
        source: &Arc<FrozenPreviewSource>,
        region: Rect,
        now: Instant,
    ) -> Result<TargetChange, String> {
        let facts = source.geometry();
        RegionGeometry::new(
            facts.encoded_dimensions(),
            region,
            (1, 1),
            facts.source_orientation(),
        )
        .map_err(|error| error.to_string())?;
        if self.latest_target.as_ref().is_some_and(|target| {
            target.serial == serial
                && Arc::ptr_eq(&target.source, source)
                && target.region == region
        }) {
            return Ok(TargetChange {
                changed: false,
                retired: None,
            });
        }
        let due = now.checked_add(DEBOUNCE).ok_or_else(invalid)?;
        let changed_source = self.latest_target.as_ref().map_or(true, |target| {
            target.serial != serial || !Arc::ptr_eq(&target.source, source)
        });
        let retired = if changed_source {
            self.current_surface.take().map(|surface| surface.image)
        } else {
            None
        };
        self.epoch = self.epoch.wrapping_add(1);
        self.latest_target = Some(Target {
            serial,
            source: Arc::clone(source),
            region,
        });
        self.due = Some(due);
        self.error = None;
        self.inflight = false;
        Ok(TargetChange {
            changed: true,
            retired,
        })
    }

    /// 只清控制状态；宿主同时撤销Loader中的请求/活动取消旗标。
    pub(super) fn clear(&mut self) -> Option<Arc<RenderImage>> {
        self.epoch = self.epoch.wrapping_add(1);
        self.latest_target = None;
        self.due = None;
        self.inflight = false;
        self.error = None;
        self.current_surface.take().map(|surface| surface.image)
    }

    pub(super) fn due_request(
        &mut self,
        now: Instant,
        external_other_resident: u64,
    ) -> Option<Request> {
        if self.inflight || self.due.is_none_or(|due| now < due) {
            return None;
        }
        let target = self.latest_target.as_ref()?;
        let request = Request {
            serial: target.serial,
            epoch: self.epoch,
            source: Arc::clone(&target.source),
            region: target.region,
            external_other_resident,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        self.due = None;
        self.inflight = true;
        Some(request)
    }

    /// bool表示回执获接受（含错误）；错误不移除旧详情，stale回执不改变任何状态。
    pub(super) fn accept(
        &mut self,
        completion: Completion,
        current_serial: u64,
        current_source: &Arc<FrozenPreviewSource>,
    ) -> (bool, Option<Arc<RenderImage>>) {
        let Some(target) = self.latest_target.as_ref() else {
            return (false, None);
        };
        if !self.inflight
            || completion.epoch != self.epoch
            || completion.serial != current_serial
            || target.serial != current_serial
            || !Arc::ptr_eq(&completion.source, current_source)
            || !Arc::ptr_eq(&target.source, current_source)
            || completion.region != target.region
        {
            return (false, None);
        }
        self.inflight = false;
        self.due = None;
        match completion.result {
            Ok(surface) => {
                self.error = None;
                let retired = self
                    .current_surface
                    .replace(surface)
                    .map(|surface| surface.image);
                (true, retired)
            }
            Err(error) => {
                self.error = Some(error);
                (true, None)
            }
        }
    }

    pub(super) fn surface(&self) -> Option<&Surface> {
        self.current_surface.as_ref()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn pending(&self) -> bool {
        self.due.is_some() || self.inflight
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(crate::tr!("PNG预览已取消", "PNG preview canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> String {
    crate::tr!("局部预览事实不一致", "Region preview facts do not match").into()
}

fn budget() -> String {
    crate::tr!(
        "局部预览呈现超过256MiB预算",
        "Region presentation exceeds the 256 MiB budget"
    )
    .into()
}

#[cfg(test)]
#[path = "image_viewer_preview_region_tests.rs"]
mod tests;
