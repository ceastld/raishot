//! OCR 临时反馈的投影和显示时钟；不保存像素、任务拥有者或编辑状态。
use crate::geometry::Rect;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Initial,
    Retry,
    Foreground,
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TaskKey {
    pub(crate) kind: TaskKind,
    pub(crate) id: u64,
}

#[derive(Default)]
pub(crate) struct ProgressClock {
    active: Option<(TaskKey, Instant)>,
}

impl ProgressClock {
    /// 只观察已核定的实际任务；None 同时清除阶段，不持有任何工作或取消权。
    pub(crate) fn observe(&mut self, key: Option<TaskKey>, now: Instant) -> Option<Duration> {
        let Some(key) = key else {
            self.active = None;
            return None;
        };
        if self.active.is_none_or(|(old, _)| old != key) {
            self.active = Some((key, now));
        }
        Some(now.saturating_duration_since(self.active?.1))
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
    pub(crate) fn project(roi: Rect, canvas: Rect, scale: f32, offset: (f32, f32)) -> Option<Self> {
        if !roi.valid(canvas.width, canvas.height)
            || !scale.is_finite()
            || scale <= 0.
            || !offset.0.is_finite()
            || !offset.1.is_finite()
        {
            return None;
        }
        let result = Self {
            x: offset.0 + roi.x as f32 / scale,
            y: offset.1 + roi.y as f32 / scale,
            width: roi.width as f32 / scale,
            height: roi.height as f32 / scale,
        };
        [result.x, result.y, result.width, result.height]
            .into_iter()
            .all(f32::is_finite)
            .then_some(result)
            .filter(|r| r.width > 0. && r.height > 0.)
    }

    /// 文字按真实排版宽度居中；窄/矮选区只保留扫描带，文字不会挤出选区。
    pub(crate) fn badge(self, text_width: f32) -> Option<Self> {
        let width = text_width + 28.;
        let height = 26.;
        if !text_width.is_finite()
            || text_width <= 0.
            || width + 16. > self.width
            || height + 16. > self.height
        {
            return None;
        }
        Some(Self {
            x: self.x + (self.width - width) / 2.,
            y: self.y + (self.height - height) / 2.,
            width,
            height,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanFrame {
    pub(crate) center_y: f32,
    pub(crate) band_height: f32,
    pub(crate) edge_alpha: u8,
}

impl ScanFrame {
    pub(crate) fn new(roi: DisplayRect, elapsed: Duration) -> Self {
        // 往返扫描仅表达“仍在工作”，不把动画位置当作识别百分比。
        let phase = (elapsed.as_nanos() % 1_600_000_000) as f32 / 1_600_000_000.;
        let position = if phase <= 0.5 {
            phase * 2.
        } else {
            (1. - phase) * 2.
        };
        let position = position * position * (3. - 2. * position);
        Self {
            center_y: roi.y + roi.height * position,
            band_height: (roi.height * 0.12).clamp(6., 38.).min(roi.height),
            edge_alpha: 72 + (position * 36.) as u8,
        }
    }
}

/// 单行 ROI 相对固定 OCR 输入；只能映射回该原选区之内。
pub(crate) fn local_roi(selected: Rect, relative: Rect, canvas: Rect) -> Option<Rect> {
    if !selected.valid(canvas.width, canvas.height)
        || !relative.valid(selected.width, selected.height)
    {
        return None;
    }
    let result = Rect {
        x: selected.x.checked_add(relative.x)?,
        y: selected.y.checked_add(relative.y)?,
        width: relative.width,
        height: relative.height,
    };
    result.valid(canvas.width, canvas.height).then_some(result)
}

#[cfg(test)]
#[path = "capture_ocr_progress_model_tests.rs"]
mod tests;
