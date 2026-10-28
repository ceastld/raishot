//! 采样预览的纯绘制分区：统一棋盘底，ROI内不再叠加总览纹理。
use crate::{geometry::Rect, image_preview::viewport::PhysicalRect, Result};

const MAX_CELLS: u64 = 4096;
// 保持整数索引和相邻f64格线可分辨；极小DPI也只会进一步放大格子。
const MAX_GRID_INDEX: f64 = (1u64 << 50) as f64;

/// 上、下、左、右四块只覆盖detail以外的完整源域；零面积块省略。
pub(crate) fn overview_regions(
    source_dimensions: (u32, u32),
    detail: Option<Rect>,
) -> Result<[Option<Rect>; 4]> {
    let (width, height) = source_dimensions;
    if width == 0 || height == 0 {
        return Err(invalid());
    }
    let Some(detail) = detail else {
        return Ok([Some(Rect::full(width, height)), None, None, None]);
    };
    let right = detail
        .x
        .checked_add(detail.width)
        .filter(|right| *right <= width)
        .ok_or_else(invalid)?;
    let bottom = detail
        .y
        .checked_add(detail.height)
        .filter(|bottom| *bottom <= height)
        .ok_or_else(invalid)?;
    if detail.width == 0 || detail.height == 0 {
        return Err(invalid());
    }
    Ok([
        nonempty(0, 0, width, detail.y),
        nonempty(0, bottom, width, height - bottom),
        nonempty(0, detail.y, detail.x, detail.height),
        nonempty(right, detail.y, width - right, detail.height),
    ])
}

fn nonempty(x: u32, y: u32, width: u32, height: u32) -> Option<Rect> {
    (width != 0 && height != 0).then_some(Rect {
        x,
        y,
        width,
        height,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CheckerCell {
    pub rect: PhysicalRect,
    pub alternate: bool,
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

/// 无堆分配的逐格迭代器；不根据原图像素尺寸创建棋盘或格子目录。
#[derive(Clone, Debug)]
pub(crate) struct CheckerGrid {
    visible: Option<Bounds>,
    side: f64,
    first_x: i64,
    end_x: i64,
    end_y: i64,
    x: i64,
    y: i64,
}

impl CheckerGrid {
    pub(crate) fn new(image: PhysicalRect, area: Rect, dpi: f64) -> Result<Self> {
        let image = image_bounds(image)?;
        if area.width == 0 || area.height == 0 {
            return Err(invalid());
        }
        let area_right = area.x.checked_add(area.width).ok_or_else(invalid)?;
        let area_bottom = area.y.checked_add(area.height).ok_or_else(invalid)?;
        let mut side = 16. * dpi;
        if !dpi.is_finite() || dpi <= 0. || !side.is_finite() || side <= 0. {
            return Err(invalid());
        }
        let visible = Bounds {
            left: image.left.max(f64::from(area.x)),
            top: image.top.max(f64::from(area.y)),
            right: image.right.min(f64::from(area_right)),
            bottom: image.bottom.min(f64::from(area_bottom)),
        };
        if visible.right <= visible.left || visible.bottom <= visible.top {
            return Ok(Self {
                visible: None,
                side,
                first_x: 0,
                end_x: 0,
                end_y: 0,
                x: 0,
                y: 0,
            });
        }
        loop {
            if let (Some((first_x, end_x)), Some((first_y, end_y))) = (
                axis_range(visible.left, visible.right, side),
                axis_range(visible.top, visible.bottom, side),
            ) {
                let columns = (end_x - first_x) as u64;
                let rows = (end_y - first_y) as u64;
                if columns
                    .checked_mul(rows)
                    .is_some_and(|cells| cells <= MAX_CELLS)
                {
                    return Ok(Self {
                        visible: Some(visible),
                        side,
                        first_x,
                        end_x,
                        end_y,
                        x: first_x,
                        y: first_y,
                    });
                }
            }
            // 交集被有限的u32窗口限定，倍增至单格之前不会需要无限side。
            side *= 2.;
            if !side.is_finite() {
                return Err(invalid());
            }
        }
    }
}

impl Iterator for CheckerGrid {
    type Item = CheckerCell;

    fn next(&mut self) -> Option<Self::Item> {
        let visible = self.visible?;
        while self.y < self.end_y {
            let (x, y) = (self.x, self.y);
            self.x += 1;
            if self.x == self.end_x {
                self.x = self.first_x;
                self.y += 1;
            }
            let left = (x as f64 * self.side).max(visible.left);
            let top = (y as f64 * self.side).max(visible.top);
            let right = ((x + 1) as f64 * self.side).min(visible.right);
            let bottom = ((y + 1) as f64 * self.side).min(visible.bottom);
            if right > left && bottom > top {
                return Some(CheckerCell {
                    rect: PhysicalRect {
                        left,
                        top,
                        width: right - left,
                        height: bottom - top,
                    },
                    alternate: (x ^ y) & 1 != 0,
                });
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.visible.is_none() || self.y >= self.end_y {
            return (0, Some(0));
        }
        let rows_after_current = (self.end_y - self.y - 1) as usize;
        let columns = (self.end_x - self.first_x) as usize;
        let current = (self.end_x - self.x) as usize;
        (0, Some(rows_after_current * columns + current))
    }
}

impl std::iter::FusedIterator for CheckerGrid {}

fn image_bounds(image: PhysicalRect) -> Result<Bounds> {
    if !image.left.is_finite()
        || !image.top.is_finite()
        || !image.width.is_finite()
        || !image.height.is_finite()
        || image.width <= 0.
        || image.height <= 0.
    {
        return Err(invalid());
    }
    let right = image.left + image.width;
    let bottom = image.top + image.height;
    if !right.is_finite() || !bottom.is_finite() || right <= image.left || bottom <= image.top {
        return Err(invalid());
    }
    Ok(Bounds {
        left: image.left,
        top: image.top,
        right,
        bottom,
    })
}

fn axis_range(left: f64, right: f64, side: f64) -> Option<(i64, i64)> {
    let first = (left / side).floor();
    let end = (right / side).ceil();
    if !first.is_finite()
        || !end.is_finite()
        || first < 0.
        || end < 0.
        || first > MAX_GRID_INDEX
        || end > MAX_GRID_INDEX
    {
        return None;
    }
    let mut first = first as i64;
    let mut end = end as i64;
    // 除法在格线处的最后一位舍入不能遗漏交集首尾；多余零面积格由next省略。
    if first > 0 && first as f64 * side > left {
        first -= 1;
    }
    end = end.max(first + 1);
    if end as f64 * side < right {
        end += 1;
    }
    (end as f64 <= MAX_GRID_INDEX).then_some((first, end))
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "预览绘制范围或DPI无效",
        "Invalid preview paint bounds or DPI"
    )
    .into()
}

#[cfg(test)]
#[path = "image_preview_paint_tests.rs"]
mod tests;
