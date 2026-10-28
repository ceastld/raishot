//! 大来源采样预览的双精度物理视口；不改变完整图片使用的视口类型。
use crate::{geometry::Rect, Result};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PreviewViewport {
    source_dimensions: (u32, u32),
    zoom: f64,
    offset: (f64, f64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PhysicalRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl PreviewViewport {
    pub(crate) fn new(source_dimensions: (u32, u32)) -> Result<Self> {
        if source_dimensions.0 == 0 || source_dimensions.1 == 0 {
            return Err(invalid());
        }
        Ok(Self {
            source_dimensions,
            zoom: 1.,
            offset: (0., 0.),
        })
    }

    pub(crate) fn source_dimensions(&self) -> (u32, u32) {
        self.source_dimensions
    }

    pub(crate) fn zoom(&self) -> f64 {
        self.zoom
    }

    pub(crate) fn offset(&self) -> (f64, f64) {
        self.offset
    }

    pub(crate) fn fit(&mut self, area: Rect) -> Result<()> {
        checked_edges(area)?;
        let width = f64::from(self.source_dimensions.0);
        let height = f64::from(self.source_dimensions.1);
        let zoom = (f64::from(area.width) / width)
            .min(f64::from(area.height) / height)
            .min(1.);
        let offset = (
            f64::from(area.x) + (f64::from(area.width) - width * zoom) / 2.,
            f64::from(area.y) + (f64::from(area.height) - height * zoom) / 2.,
        );
        self.commit(zoom, offset)
    }

    pub(crate) fn set_zoom(&mut self, zoom: f64, anchor: (f64, f64)) -> Result<()> {
        self.full_bounds()?;
        if !zoom.is_finite() || zoom <= 0. || !finite_point(anchor) {
            return Err(invalid());
        }
        let long = self.source_dimensions.0.max(self.source_dimensions.1);
        let zoom = zoom.clamp(1. / f64::from(long), 8.);
        if zoom == self.zoom {
            return Ok(());
        }
        let ratio = zoom / self.zoom;
        let offset = (
            anchor.0 - (anchor.0 - self.offset.0) * ratio,
            anchor.1 - (anchor.1 - self.offset.1) * ratio,
        );
        self.commit(zoom, offset)
    }

    pub(crate) fn set_offset(&mut self, offset: (f64, f64)) -> Result<()> {
        self.commit(self.zoom, offset)
    }

    /// 不裁剪到来源边界；调用方可用同一变换定位离屏点。不可表示的结果返回None。
    pub(crate) fn window_point(&self, source: (f64, f64)) -> Option<(f64, f64)> {
        if !finite_point(source) {
            return None;
        }
        let point = (
            source.0 * self.zoom + self.offset.0,
            source.1 * self.zoom + self.offset.1,
        );
        finite_point(point).then_some(point)
    }

    pub(crate) fn source_point(&self, window: (f64, f64)) -> Option<(f64, f64)> {
        if !finite_point(window) {
            return None;
        }
        let point = (
            (window.0 - self.offset.0) / self.zoom,
            (window.1 - self.offset.1) / self.zoom,
        );
        finite_point(point).then_some(point)
    }

    /// 归正源域的整数半开范围；先判断真实交集，padding不会把离屏图变成可见图。
    pub(crate) fn visible_region(&self, area: Rect, padding: u32) -> Result<Option<Rect>> {
        let (area_right, area_bottom) = checked_edges(area)?;
        let full = self.full_bounds()?;
        if full.left + full.width <= f64::from(area.x)
            || full.top + full.height <= f64::from(area.y)
            || full.left >= f64::from(area_right)
            || full.top >= f64::from(area_bottom)
        {
            return Ok(None);
        }
        let (left, top) = self
            .source_point((f64::from(area.x), f64::from(area.y)))
            .ok_or_else(invalid)?;
        let (right, bottom) = self
            .source_point((f64::from(area_right), f64::from(area_bottom)))
            .ok_or_else(invalid)?;
        let left = left.max(0.);
        let top = top.max(0.);
        let right = right.min(f64::from(self.source_dimensions.0));
        let bottom = bottom.min(f64::from(self.source_dimensions.1));
        if right <= left || bottom <= top {
            return Ok(None);
        }
        let padding = f64::from(padding);
        // 先夹到有限u32源域，再扩整数边，转换不会依赖饱和cast掩盖溢出。
        let x = (left.floor() - padding).max(0.) as u32;
        let y = (top.floor() - padding).max(0.) as u32;
        let end_x = (right.ceil() + padding).min(f64::from(self.source_dimensions.0)) as u32;
        let end_y = (bottom.ceil() + padding).min(f64::from(self.source_dimensions.1)) as u32;
        Ok(Some(Rect {
            x,
            y,
            width: end_x - x,
            height: end_y - y,
        }))
    }

    /// 连续物理边界；调用方最终绘制时才转换为UI坐标，不在各ROI边缘取整。
    pub(crate) fn projected_bounds(&self, region: Rect) -> Result<PhysicalRect> {
        let (right, bottom) = checked_edges(region)?;
        if right > self.source_dimensions.0
            || bottom > self.source_dimensions.1
            || !self.zoom.is_finite()
            || self.zoom <= 0.
            || !finite_point(self.offset)
        {
            return Err(invalid());
        }
        let bounds = PhysicalRect {
            left: f64::from(region.x) * self.zoom + self.offset.0,
            top: f64::from(region.y) * self.zoom + self.offset.1,
            width: f64::from(region.width) * self.zoom,
            height: f64::from(region.height) * self.zoom,
        };
        let right = bounds.left + bounds.width;
        let bottom = bounds.top + bounds.height;
        if ![
            bounds.left,
            bounds.top,
            bounds.width,
            bounds.height,
            right,
            bottom,
        ]
        .iter()
        .all(|value| value.is_finite())
            || bounds.width <= 0.
            || bounds.height <= 0.
            || right <= bounds.left
            || bottom <= bounds.top
        {
            return Err(invalid());
        }
        Ok(bounds)
    }

    fn full_bounds(&self) -> Result<PhysicalRect> {
        self.projected_bounds(Rect::full(
            self.source_dimensions.0,
            self.source_dimensions.1,
        ))
    }

    fn commit(&mut self, zoom: f64, offset: (f64, f64)) -> Result<()> {
        let candidate = Self {
            zoom,
            offset,
            ..*self
        };
        candidate.full_bounds()?;
        *self = candidate;
        Ok(())
    }
}

fn finite_point((x, y): (f64, f64)) -> bool {
    x.is_finite() && y.is_finite()
}

fn checked_edges(rect: Rect) -> Result<(u32, u32)> {
    if rect.width == 0 || rect.height == 0 {
        return Err(invalid());
    }
    Ok((
        rect.x.checked_add(rect.width).ok_or_else(invalid)?,
        rect.y.checked_add(rect.height).ok_or_else(invalid)?,
    ))
}

fn invalid() -> Box<dyn std::error::Error> {
    crate::tr!("预览视口或范围无效", "Invalid preview viewport or region").into()
}

#[cfg(test)]
#[path = "image_preview_viewport_tests.rs"]
mod tests;
