//! 贴图 OCR 的临时反馈几何与相位；不保存像素、拥有者或任务取消权。
use crate::geometry::Rect;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Ordinary,
    Local,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskKey {
    pub(crate) kind: TaskKind,
    pub(crate) identity: Arc<()>,
    pub(crate) id: u64,
}

impl PartialEq for TaskKey {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.id == other.id
            && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for TaskKey {}

#[derive(Clone)]
pub(crate) struct RenderLease {
    key: TaskKey,
    epoch: Arc<()>,
}

#[derive(Default)]
pub(crate) struct ProgressClock {
    active: Option<(TaskKey, Instant)>,
    render_epoch: Option<Arc<()>>,
}

impl ProgressClock {
    /// 输入必须是宿主已核定的实际任务；None 同时清除相位和绘制租约。
    pub(crate) fn observe(&mut self, key: Option<TaskKey>, now: Instant) -> Option<Duration> {
        let Some(key) = key else {
            self.active = None;
            self.render_epoch = None;
            return None;
        };
        if self.active.as_ref().is_none_or(|(old, _)| old != &key) {
            self.active = Some((key, now));
            self.render_epoch = None;
        }
        Some(now.saturating_duration_since(self.active.as_ref()?.1))
    }

    /// 每次 render 都废弃旧闭包的绘制资格，同一任务仍保留动画相位。
    pub(crate) fn begin_render(
        &mut self,
        key: Option<TaskKey>,
        now: Instant,
    ) -> Option<(Duration, RenderLease)> {
        let elapsed = self.observe(key.clone(), now)?;
        let key = key?;
        let epoch = Arc::new(());
        self.render_epoch = Some(epoch.clone());
        Some((elapsed, RenderLease { key, epoch }))
    }

    /// 宿主还须另核 ROI、当前输入、窗口、视口和焦点；租约只核模型身份。
    pub(crate) fn lease_is_current(&self, lease: &RenderLease, current: Option<&TaskKey>) -> bool {
        current.is_some_and(|key| key == &lease.key)
            && self
                .active
                .as_ref()
                .is_some_and(|(key, _)| key == &lease.key)
            && self
                .render_epoch
                .as_ref()
                .is_some_and(|epoch| Arc::ptr_eq(epoch, &lease.epoch))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl DisplayRect {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.width > 0.0
            && self.height > 0.0
            && (self.x + self.width).is_finite()
            && (self.y + self.height).is_finite()
            && self.x + self.width > self.x
            && self.y + self.height > self.y
    }

    /// 输入 ROI 为原图物理像素，offset 为贴图窗口内的物理平移。
    pub(crate) fn project(
        roi: Rect,
        image_size: (u32, u32),
        zoom: f32,
        dpi: f32,
        offset: (f32, f32),
    ) -> Option<Self> {
        if !roi.valid(image_size.0, image_size.1)
            || !zoom.is_finite()
            || zoom <= 0.0
            || !dpi.is_finite()
            || dpi <= 0.0
            || !offset.0.is_finite()
            || !offset.1.is_finite()
        {
            return None;
        }
        let scale = f64::from(zoom) / f64::from(dpi);
        let result = Self {
            x: (f64::from(offset.0) / f64::from(dpi) + f64::from(roi.x) * scale) as f32,
            y: (f64::from(offset.1) / f64::from(dpi) + f64::from(roi.y) * scale) as f32,
            width: (f64::from(roi.width) * scale) as f32,
            height: (f64::from(roi.height) * scale) as f32,
        };
        result.valid().then_some(result)
    }

    pub(crate) fn intersection(self, other: Self) -> Option<Self> {
        if !self.valid() || !other.valid() {
            return None;
        }
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        let result = Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        };
        result.valid().then_some(result)
    }

    /// 宿主用真实文字宽度；调用前可与窗口求交，让徽标也落在可见区。
    pub(crate) fn badge(self, text_width: f32) -> Option<Self> {
        let width = text_width + 28.0;
        let height = 26.0;
        if !self.valid()
            || !text_width.is_finite()
            || text_width <= 0.0
            || width + 16.0 > self.width
            || height + 16.0 > self.height
        {
            return None;
        }
        let result = Self {
            x: self.x + (self.width - width) / 2.0,
            y: self.y + (self.height - height) / 2.0,
            width,
            height,
        };
        result.valid().then_some(result)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanFrame {
    pub(crate) band: DisplayRect,
    pub(crate) edge_alpha: u8,
}

impl ScanFrame {
    pub(crate) fn new(
        roi: DisplayRect,
        visible_window: DisplayRect,
        elapsed: Duration,
    ) -> Option<Self> {
        let visible = roi.intersection(visible_window)?;
        // 往返扫描只表示任务仍在运行；在可见 ROI 内扫描，不伪装百分比。
        let phase = (elapsed.as_nanos() % 1_600_000_000) as f64 / 1_600_000_000.0;
        let position = if phase <= 0.5 {
            phase * 2.0
        } else {
            (1.0 - phase) * 2.0
        };
        let position = (position * position * (3.0 - 2.0 * position)) as f32;
        let height = (visible.height * 0.12).clamp(6.0, 38.0).min(visible.height);
        let band = DisplayRect {
            x: visible.x,
            y: visible.y + (visible.height - height) * position,
            width: visible.width,
            height,
        }
        .intersection(visible)?;
        Some(Self {
            band,
            edge_alpha: 72 + (position * 36.0) as u8,
        })
    }
}

#[cfg(test)]
#[path = "pin_ocr_progress_model_tests.rs"]
mod tests;
