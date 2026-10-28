//! 长截图选区手势：只处理物理坐标，预览期间不改变采集会话或已提交结果。
use crate::{geometry::ScreenRect, stitch::ScrollAxis, Result};

const MAX_SIDE: i64 = 16_384;
const MAX_PIXELS: i64 = 32_000_000;
const MIN_CROSS: i64 = 32;
const MIN_ALONG: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionHandle {
    Move,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RegionGesture {
    id: u64,
    original: ScreenRect,
    start: (i32, i32),
    handle: RegionHandle,
    axis_constraint: Option<ScrollAxis>,
    bounds: Edges,
    min_width: i64,
    min_height: i64,
}

#[derive(Clone, Copy, Debug)]
struct Edges {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}
impl Edges {
    fn checked(rect: ScreenRect) -> Result<Self> {
        let left = i64::from(rect.left);
        let top = i64::from(rect.top);
        let right = left + i64::from(rect.width);
        let bottom = top + i64::from(rect.height);
        if rect.width == 0
            || rect.height == 0
            || right > i64::from(i32::MAX)
            || bottom > i64::from(i32::MAX)
        {
            return Err("长截图物理选区为空或端点溢出".into());
        }
        Ok(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    fn contains(self, other: Self) -> bool {
        other.left >= self.left
            && other.top >= self.top
            && other.right <= self.right
            && other.bottom <= self.bottom
    }
}

impl RegionGesture {
    /// min_along 包含固定首尾边带；它与移动约束是两个独立的合同。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_for_axis(
        id: u64,
        original: ScreenRect,
        start: (i32, i32),
        handle: RegionHandle,
        axis_constraint: Option<ScrollAxis>,
        bounds: ScreenRect,
        stitch_axis: ScrollAxis,
        min_along: u32,
    ) -> Result<Self> {
        if id == 0 {
            return Err("长截图选区手势 ID 无效".into());
        }
        if handle != RegionHandle::Move && axis_constraint.is_some() {
            return Err("运行中的长截图只允许沿采集轴移动选区".into());
        }
        if axis_constraint.is_some_and(|axis| axis != stitch_axis) {
            return Err("长截图移动约束与采集方向不一致".into());
        }
        let min_along = i64::from(min_along.max(MIN_ALONG));
        if min_along > MAX_SIDE {
            return Err("固定边带与可滚动内容超过选区边长上限".into());
        }
        let (min_width, min_height) = match stitch_axis {
            ScrollAxis::Vertical => (MIN_CROSS, min_along),
            ScrollAxis::Horizontal => (min_along, MIN_CROSS),
        };
        let bounds = Edges::checked(bounds)?;
        let original_edges = Edges::checked(original)?;
        if !bounds.contains(original_edges)
            || i64::from(original.width) < min_width
            || i64::from(original.height) < min_height
            || i64::from(original.width) > MAX_SIDE
            || i64::from(original.height) > MAX_SIDE
            || i64::from(original.width) * i64::from(original.height) > MAX_PIXELS
        {
            return Err("长截图选区不满足桌面范围、最小尺寸或像素预算".into());
        }
        Ok(Self {
            id,
            original,
            start,
            handle,
            axis_constraint,
            bounds,
            min_width,
            min_height,
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn original(&self) -> ScreenRect {
        self.original
    }

    pub(crate) fn update(&self, point: (i32, i32)) -> Result<ScreenRect> {
        // 指针可跨负坐标屏幕；不能先在 i32 中求差再转成 i64。
        let dx = i64::from(point.0) - i64::from(self.start.0);
        let dy = i64::from(point.1) - i64::from(self.start.1);
        let original = Edges::checked(self.original)?;
        if self.handle == RegionHandle::Move {
            let left = if self.axis_constraint == Some(ScrollAxis::Vertical) {
                original.left
            } else {
                (original.left + dx).clamp(
                    self.bounds.left,
                    self.bounds.right - i64::from(self.original.width),
                )
            };
            let top = if self.axis_constraint == Some(ScrollAxis::Horizontal) {
                original.top
            } else {
                (original.top + dy).clamp(
                    self.bounds.top,
                    self.bounds.bottom - i64::from(self.original.height),
                )
            };
            return self.result(
                left,
                top,
                i64::from(self.original.width),
                i64::from(self.original.height),
            );
        }

        let west = matches!(
            self.handle,
            RegionHandle::West | RegionHandle::NorthWest | RegionHandle::SouthWest
        );
        let east = matches!(
            self.handle,
            RegionHandle::East | RegionHandle::NorthEast | RegionHandle::SouthEast
        );
        let north = matches!(
            self.handle,
            RegionHandle::North | RegionHandle::NorthEast | RegionHandle::NorthWest
        );
        let south = matches!(
            self.handle,
            RegionHandle::South | RegionHandle::SouthEast | RegionHandle::SouthWest
        );
        let resize_x = west || east;
        let resize_y = north || south;
        let mut width = if west {
            (original.right - (original.left + dx)).clamp(
                self.min_width,
                MAX_SIDE.min(original.right - self.bounds.left),
            )
        } else if east {
            (original.right + dx - original.left).clamp(
                self.min_width,
                MAX_SIDE.min(self.bounds.right - original.left),
            )
        } else {
            i64::from(self.original.width)
        };
        let mut height = if north {
            (original.bottom - (original.top + dy)).clamp(
                self.min_height,
                MAX_SIDE.min(original.bottom - self.bounds.top),
            )
        } else if south {
            (original.bottom + dy - original.top).clamp(
                self.min_height,
                MAX_SIDE.min(self.bounds.bottom - original.top),
            )
        } else {
            i64::from(self.original.height)
        };

        if width * height > MAX_PIXELS {
            if resize_x && resize_y {
                // 双轴超预算按比例收缩，仍固定对角点。固定边带较大时保留最小沿轴尺寸。
                let scale = (MAX_PIXELS as f64 / (width * height) as f64).sqrt();
                width = ((width as f64 * scale).floor() as i64).max(self.min_width);
                height = ((height as f64 * scale).floor() as i64).max(self.min_height);
            }
            if width * height > MAX_PIXELS {
                if resize_x && MAX_PIXELS / height >= self.min_width {
                    width = MAX_PIXELS / height;
                } else if resize_y && MAX_PIXELS / width >= self.min_height {
                    height = MAX_PIXELS / width;
                } else {
                    return Err("长截图最小选区超过像素预算".into());
                }
            }
        }
        let left = if west {
            original.right - width
        } else {
            original.left
        };
        let top = if north {
            original.bottom - height
        } else {
            original.top
        };
        self.result(left, top, width, height)
    }

    fn result(&self, left: i64, top: i64, width: i64, height: i64) -> Result<ScreenRect> {
        if width < self.min_width
            || height < self.min_height
            || width > MAX_SIDE
            || height > MAX_SIDE
            || width * height > MAX_PIXELS
        {
            return Err("长截图选区尺寸超出限制".into());
        }
        let result = ScreenRect {
            left: i32::try_from(left)?,
            top: i32::try_from(top)?,
            width: u32::try_from(width)?,
            height: u32::try_from(height)?,
        };
        if !self.bounds.contains(Edges::checked(result)?) {
            return Err("长截图选区超出虚拟桌面".into());
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "long_region_tests.rs"]
mod tests;
