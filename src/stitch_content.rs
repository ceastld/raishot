//! 稳定内容框跟踪与固定栏边界检测。
//! 内容框只辅助对齐；未检测到固定栏时回退整帧，不能拒绝滚动或裁掉整帧边带。
use crate::{capture::Image, geometry::Rect, stitch::ScrollAxis};

// 阈值单位为采集图的物理像素，不乘显示器 DPI。
const MIN_CONTENT_SIZE: u32 = 50;
const MARGIN: u32 = 2;
const MIN_FIXED_BORDER_ROWS: u32 = 8;
const MIN_CONTENT_HEIGHT_RATIO: f64 = 0.70;
const ROW_COLUMN_DIFF_THRESHOLD: u64 = 4;
const UNCHANGED_ROW_DIFF_THRESHOLD: u64 = 1;

/// 位于逻辑帧坐标：x 为跨轴，y 为滚动轴；横向捕获以零复制转置视图检测。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FixedBorderInsets {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

#[derive(Clone, Default)]
pub(crate) struct StableContentBorderTracker {
    minimum: Option<FixedBorderInsets>,
    content: Option<Rect>,
}
impl StableContentBorderTracker {
    /// 稳定窗口为整个当前会话：四边独立累计最小值，只有 Reset 清空。
    /// 不永久锚定首帧，不用白色/文字纹理作为额外拒绝条件。
    pub(crate) fn reset(&mut self) {
        self.minimum = None;
        self.content = None;
    }
    pub(crate) fn content_rect(&self) -> Option<Rect> {
        self.content
    }
    /// 对齐区域保留 2px margin；真正拼接的 seam 必须避开原始固定栏。
    pub(crate) fn fixed_borders(&self) -> Option<FixedBorderInsets> {
        self.minimum
    }
    pub(crate) fn observe_frame_pair(
        &mut self,
        previous: &Image,
        current: &Image,
        axis: ScrollAxis,
    ) -> Option<Rect> {
        if let Some(detected) = estimate_fixed_borders(previous, current, axis) {
            let (width, height) = dimensions(current, axis);
            self.merge(detected, width, height);
        }
        // 无检测结果时保留先前稳定估计。
        self.content
    }
    fn merge(&mut self, detected: FixedBorderInsets, width: u32, height: u32) {
        let minimum = self.minimum.map_or(detected, |previous| FixedBorderInsets {
            top: previous.top.min(detected.top),
            bottom: previous.bottom.min(detected.bottom),
            left: previous.left.min(detected.left),
            right: previous.right.min(detected.right),
        });
        self.minimum = Some(minimum);
        self.content = build_content_rect(width, height, minimum);
    }
}

fn dimensions(image: &Image, axis: ScrollAxis) -> (u32, u32) {
    match axis {
        ScrollAxis::Vertical => (image.width, image.height),
        ScrollAxis::Horizontal => (image.height, image.width),
    }
}

pub(crate) fn physical_rect(logical: Rect, axis: ScrollAxis) -> Rect {
    match axis {
        ScrollAxis::Vertical => logical,
        ScrollAxis::Horizontal => Rect {
            x: logical.y,
            y: logical.x,
            width: logical.height,
            height: logical.width,
        },
    }
}

pub(crate) fn build_content_rect(
    width: u32,
    height: u32,
    insets: FixedBorderInsets,
) -> Option<Rect> {
    let top = insets.top.saturating_sub(MARGIN);
    let bottom = insets.bottom.saturating_sub(MARGIN);
    let left = insets.left.saturating_sub(MARGIN);
    let right = insets.right.saturating_sub(MARGIN);
    let w = width.checked_sub(left)?.checked_sub(right)?;
    let h = height.checked_sub(top)?.checked_sub(bottom)?;
    if w < MIN_CONTENT_SIZE
        || h < MIN_CONTENT_SIZE
        || h as f64 / (height as f64) < MIN_CONTENT_HEIGHT_RATIO
    {
        return None;
    }
    Some(Rect {
        x: left,
        y: top,
        width: w,
        height: h,
    })
}

struct Frame<'a> {
    image: &'a Image,
    axis: ScrollAxis,
    width: u32,
    height: u32,
}
impl<'a> Frame<'a> {
    fn new(image: &'a Image, axis: ScrollAxis) -> Option<Self> {
        let bytes = (image.width as usize)
            .checked_mul(image.height as usize)?
            .checked_mul(4)?;
        if image.width == 0 || image.height == 0 || image.bgra.len() != bytes {
            return None;
        }
        let (width, height) = dimensions(image, axis);
        Some(Self {
            image,
            axis,
            width,
            height,
        })
    }
    fn offset(&self, x: u32, y: u32) -> usize {
        let (x, y) = match self.axis {
            ScrollAxis::Vertical => (x, y),
            ScrollAxis::Horizontal => (y, x),
        };
        (y as usize * self.image.width as usize + x as usize) * 4
    }
    fn row_step(&self) -> usize {
        match self.axis {
            ScrollAxis::Vertical => 4,
            ScrollAxis::Horizontal => self.image.width as usize * 4,
        }
    }
    fn column_step(&self) -> usize {
        match self.axis {
            ScrollAxis::Vertical => self.image.width as usize * 4,
            ScrollAxis::Horizontal => 4,
        }
    }
}

/// LineExceedsDifference 原定义：只算 RGB；在非负差值超过整行阈值后立刻返回。
fn line_exceeds_difference(
    previous: &Frame<'_>,
    current: &Frame<'_>,
    offset: usize,
    count: u32,
    step: usize,
    limit: u64,
) -> bool {
    let a = &previous.image.bgra;
    let b = &current.image.bgra;
    let mut offset = offset;
    let mut sum = 0u64;
    for _ in 0..count {
        sum += u64::from(a[offset].abs_diff(b[offset]))
            + u64::from(a[offset + 1].abs_diff(b[offset + 1]))
            + u64::from(a[offset + 2].abs_diff(b[offset + 2]));
        if sum > limit {
            return true;
        }
        offset += step;
    }
    false
}

pub(crate) fn estimate_fixed_borders(
    previous: &Image,
    current: &Image,
    axis: ScrollAxis,
) -> Option<FixedBorderInsets> {
    if previous.width != current.width || previous.height != current.height {
        return None;
    }
    let previous = Frame::new(previous, axis)?;
    let current = Frame::new(current, axis)?;
    let (width, height) = (previous.width, previous.height);
    let row_limit = ROW_COLUMN_DIFF_THRESHOLD * width as u64 * 3;
    let changed_row = |y| {
        line_exceeds_difference(
            &previous,
            &current,
            previous.offset(0, y),
            width,
            previous.row_step(),
            row_limit,
        )
    };
    let mut top = 0;
    while top < height && !changed_row(top) {
        top += 1;
    }
    if top == height {
        return None;
    }
    let mut last_row = height - 1;
    while last_row > top && !changed_row(last_row) {
        last_row -= 1;
    }
    let bottom = height - last_row - 1;
    if top < MIN_FIXED_BORDER_ROWS && bottom < MIN_FIXED_BORDER_ROWS {
        return None;
    }

    let changed_rows = last_row - top + 1;
    let column_limit = ROW_COLUMN_DIFF_THRESHOLD * changed_rows as u64 * 3;
    let changed_column = |x| {
        line_exceeds_difference(
            &previous,
            &current,
            previous.offset(x, top),
            changed_rows,
            previous.column_step(),
            column_limit,
        )
    };
    let mut left = 0;
    while left < width && !changed_column(left) {
        left += 1;
    }
    if left == width {
        return None;
    }
    let mut last_column = width - 1;
    while last_column > left && !changed_column(last_column) {
        last_column -= 1;
    }
    let right = width - last_column - 1;
    if last_column - left + 1 < MIN_CONTENT_SIZE || changed_rows < MIN_CONTENT_SIZE {
        return None;
    }
    let insets = FixedBorderInsets {
        top,
        bottom,
        left,
        right,
    };
    // BuildContentRect 的 margin/比例与 raw detector 的 candidate 相同。
    build_content_rect(width, height, insets)?;
    let unchanged_limit = UNCHANGED_ROW_DIFF_THRESHOLD * width as u64 * 3;
    let unchanged_row = |y| {
        !line_exceeds_difference(
            &previous,
            &current,
            previous.offset(0, y),
            width,
            previous.row_step(),
            unchanged_limit,
        )
    };
    if top >= MIN_FIXED_BORDER_ROWS && !(0..top).all(unchanged_row) {
        return None;
    }
    if bottom >= MIN_FIXED_BORDER_ROWS && !(height - bottom..height).all(unchanged_row) {
        return None;
    }
    Some(insets)
}

#[cfg(test)]
#[path = "stitch_content_tests.rs"]
mod tests;
