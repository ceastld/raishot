//! 长图剪辑只记录原图中的删除区间；不移动像素、标注或桌面原点。
use crate::{capture::Image, stitch::ScrollAxis, Result};
use std::ops::Range;

const MAX_OUTPUT_PIXELS: u64 = 32_000_000;
const MAX_OUTPUT_SIDE: u32 = 65_535;
const MIN_BLANK_LINES: u32 = 6;

/// 一个保留片段，source 与 display_start 都沿长轴计量，单位为原图像素。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClipSegment {
    pub source: Range<u32>,
    pub display_start: u32,
}

/// 接缝是零宽几何边界，指针落在其上时由调用者决定归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeamSide {
    /// 返回接缝之前的原图边界；开头裁剪处返回原图 0。
    Before,
    /// 返回接缝之后的原图边界；结尾裁剪处返回原图长轴末端。
    After,
}

/// 所有区间均为相对原图的半开区间；删除记录按起点排序，互不重叠且不相邻。
///
/// 超出输出上限的源图仍可进入模型并继续裁剪；只有 check_output 拒绝输出超限成品。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LongClip {
    width: u32,
    height: u32,
    axis: ScrollAxis,
    removed: Vec<Range<u32>>,
    cross: Range<u32>,
}

impl LongClip {
    pub(crate) fn new(width: u32, height: u32, axis: ScrollAxis) -> Result<Self> {
        let cross_len = match axis {
            ScrollAxis::Vertical => width,
            ScrollAxis::Horizontal => height,
        };
        let clip = Self {
            width,
            height,
            axis,
            removed: Vec::new(),
            cross: 0..cross_len,
        };
        clip.validate()?;
        Ok(clip)
    }

    pub(crate) fn source_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn axis(&self) -> ScrollAxis {
        self.axis
    }

    pub(crate) fn removed(&self) -> &[Range<u32>] {
        &self.removed
    }

    pub(crate) fn cross_range(&self) -> Range<u32> {
        self.cross.clone()
    }

    fn long_len(&self) -> u32 {
        match self.axis {
            ScrollAxis::Vertical => self.height,
            ScrollAxis::Horizontal => self.width,
        }
    }

    fn cross_len(&self) -> u32 {
        match self.axis {
            ScrollAxis::Vertical => self.width,
            ScrollAxis::Horizontal => self.height,
        }
    }

    fn display_len(&self) -> u32 {
        self.long_len()
            - self
                .removed
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u32>()
    }

    pub(crate) fn output_size(&self) -> (u32, u32) {
        let long = self.display_len();
        let cross = self.cross.end - self.cross.start;
        match self.axis {
            ScrollAxis::Vertical => (cross, long),
            ScrollAxis::Horizontal => (long, cross),
        }
    }

    pub(crate) fn is_identity(&self) -> bool {
        self.removed.is_empty() && self.cross == (0..self.cross_len())
    }

    /// 验证文档及撤销记录中的几何，不把输出上限混入编辑有效性。
    pub(crate) fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err("长图原始尺寸不能为空".into());
        }
        valid_range(&self.cross, self.cross_len(), false)?;
        let mut previous_end = None;
        let mut deleted = 0_u64;
        for range in &self.removed {
            valid_range(range, self.long_len(), false)?;
            if previous_end.is_some_and(|end| range.start <= end) {
                return Err("长图删除区间必须排序并合并相邻或重叠部分".into());
            }
            previous_end = Some(range.end);
            deleted += u64::from(range.end - range.start);
        }
        if deleted >= u64::from(self.long_len()) {
            return Err("剪辑后必须至少保留一个像素".into());
        }
        Ok(())
    }

    pub(crate) fn exceeds_output_limit(&self) -> bool {
        let (width, height) = self.output_size();
        width > MAX_OUTPUT_SIDE
            || height > MAX_OUTPUT_SIDE
            || u64::from(width) * u64::from(height) > MAX_OUTPUT_PIXELS
    }

    pub(crate) fn check_output(&self) -> Result<()> {
        self.validate()?;
        if self.exceeds_output_limit() {
            return Err("成品超过 65535 单边或 3200 万像素，请先裁剪".into());
        }
        Ok(())
    }

    pub(crate) fn segments(&self) -> Vec<ClipSegment> {
        let mut result = Vec::with_capacity(self.removed.len() + 1);
        let mut source_start = 0;
        let mut display_start = 0;
        for range in &self.removed {
            if source_start < range.start {
                result.push(ClipSegment {
                    source: source_start..range.start,
                    display_start,
                });
                display_start += range.start - source_start;
            }
            source_start = range.end;
        }
        if source_start < self.long_len() {
            result.push(ClipSegment {
                source: source_start..self.long_len(),
                display_start,
            });
        }
        result
    }

    /// 删除原图区间；空区间和重复删除不提交。任何错误均保持当前模型不变。
    pub(crate) fn delete(&mut self, range: Range<u32>) -> Result<bool> {
        valid_range(&range, self.long_len(), true)?;
        if range.is_empty() {
            return Ok(false);
        }
        let mut removed = self.removed.clone();
        removed.push(range);
        self.replace_geometry(removed, self.cross.clone())
    }

    pub(crate) fn restore(&mut self, index: usize) -> bool {
        if index >= self.removed.len() {
            return false;
        }
        self.removed.remove(index);
        true
    }

    /// 调整归并后的第 index 条记录；把两端拖到同一位置等同恢复该条记录。
    pub(crate) fn adjust(&mut self, index: usize, range: Range<u32>) -> Result<bool> {
        if index >= self.removed.len() {
            return Err("找不到要调整的删除区间".into());
        }
        valid_range(&range, self.long_len(), true)?;
        let mut removed = self.removed.clone();
        if range.is_empty() {
            removed.remove(index);
        } else {
            removed[index] = range;
        }
        self.replace_geometry(removed, self.cross.clone())
    }

    pub(crate) fn set_cross(&mut self, range: Range<u32>) -> Result<bool> {
        valid_range(&range, self.cross_len(), false)?;
        self.replace_geometry(self.removed.clone(), range)
    }

    /// 替换首尾裁剪及横向范围，保留严格位于原图内部的删除记录。
    ///
    /// 首尾与相连中段归并后是一条记录，向外裁回时按这条记录整体调整，不恢复其来源历史。
    pub(crate) fn trim(&mut self, long_keep: Range<u32>, cross_keep: Range<u32>) -> Result<bool> {
        valid_range(&long_keep, self.long_len(), false)?;
        valid_range(&cross_keep, self.cross_len(), false)?;
        let mut removed: Vec<_> = self
            .removed
            .iter()
            .filter(|range| range.start > 0 && range.end < self.long_len())
            .cloned()
            .collect();
        if long_keep.start > 0 {
            removed.push(0..long_keep.start);
        }
        if long_keep.end < self.long_len() {
            removed.push(long_keep.end..self.long_len());
        }
        self.replace_geometry(removed, cross_keep)
    }

    /// 恢复全部原图范围；历史事务由文档调用者负责。
    pub(crate) fn clear(&mut self) -> bool {
        if self.is_identity() {
            return false;
        }
        self.removed.clear();
        self.cross = 0..self.cross_len();
        true
    }

    fn replace_geometry(&mut self, removed: Vec<Range<u32>>, cross: Range<u32>) -> Result<bool> {
        let mut next = self.clone();
        next.removed = normalize(removed);
        next.cross = cross;
        next.validate()?;
        if *self == next {
            return Ok(false);
        }
        *self = next;
        Ok(true)
    }

    /// 把保留的原图像素位置映射到成品；删除区间 [start,end) 内返回 None。
    ///
    /// 允许几何端点 source_long 映射到成品末端，即便末尾被裁掉；其它删除起点不特殊处理。
    pub(crate) fn source_to_display(&self, position: f32) -> Option<f32> {
        if !position.is_finite() || position < 0. || position > self.long_len() as f32 {
            return None;
        }
        if position == self.long_len() as f32 {
            return Some(self.display_len() as f32);
        }
        let mut deleted = 0;
        for range in &self.removed {
            if position < range.start as f32 {
                break;
            }
            if position < range.end as f32 {
                return None;
            }
            deleted += range.end - range.start;
        }
        Some(position - deleted as f32)
    }

    /// 把成品坐标映射回原图；非接缝处与 side 无关。
    ///
    /// 接缝含开头与结尾裁剪的零宽边界：Before 返回删除起点，After 返回删除终点。
    /// 允许 0 与成品长轴末端；越界和非有限值返回 None。此方法不分配保留片段。
    pub(crate) fn display_to_source(&self, position: f32, side: SeamSide) -> Option<f32> {
        if !position.is_finite() || position < 0. || position > self.display_len() as f32 {
            return None;
        }
        let mut deleted = 0;
        for range in &self.removed {
            let seam = (range.start - deleted) as f32;
            if position < seam {
                break;
            }
            if position == seam {
                return Some(match side {
                    SeamSide::Before => range.start as f32,
                    SeamSide::After => range.end as f32,
                });
            }
            deleted += range.end - range.start;
        }
        Some(position + deleted as f32)
    }
}

fn valid_range(range: &Range<u32>, length: u32, allow_empty: bool) -> Result<()> {
    if range.start > range.end || range.end > length || (!allow_empty && range.is_empty()) {
        return Err("剪辑范围必须在原图内且不能反向或为空".into());
    }
    Ok(())
}

fn normalize(mut ranges: Vec<Range<u32>>) -> Vec<Range<u32>> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut result: Vec<Range<u32>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = result.last_mut().filter(|last| range.start <= last.end) {
            previous.end = previous.end.max(range.end);
        } else {
            result.push(range);
        }
    }
    result
}

/// 原图打开时生成一次的空白间隙吸附表；每段连续同色均匀行只保存中线。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlankSnaps {
    positions: Vec<u32>,
}

impl BlankSnaps {
    pub(crate) fn from_image(image: &Image, axis: ScrollAxis) -> Result<Self> {
        if image.width == 0 || image.height == 0 {
            return Err("空白吸附图片尺寸不能为空".into());
        }
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("空白吸附图片尺寸溢出")?;
        if image.bgra.len() != expected {
            return Err("空白吸附图片像素长度与尺寸不符".into());
        }
        let (long, cross) = match axis {
            ScrollAxis::Vertical => (image.height, image.width),
            ScrollAxis::Horizontal => (image.width, image.height),
        };
        let mut positions = Vec::new();
        let mut run: Option<([u8; 4], u32)> = None;
        for position in 0..long {
            let color = uniform_line_color(image, axis, position, cross);
            if run
                .as_ref()
                .is_some_and(|(previous, _)| Some(*previous) == color)
            {
                continue;
            }
            if let Some((_, start)) = run {
                push_blank(&mut positions, start, position);
            }
            run = color.map(|color| (color, position));
        }
        if let Some((_, start)) = run {
            push_blank(&mut positions, start, long);
        }
        Ok(Self { positions })
    }

    pub(crate) fn positions(&self) -> &[u32] {
        &self.positions
    }

    /// 容差使用原图像素，边界包含在内；等距时选靠前的中线。
    pub(crate) fn nearest(&self, position: f32, tolerance: f32) -> Option<u32> {
        if !position.is_finite() || !tolerance.is_finite() || tolerance < 0. {
            return None;
        }
        let position = f64::from(position);
        let tolerance = f64::from(tolerance);
        let index = self
            .positions
            .partition_point(|&candidate| f64::from(candidate) < position);
        let before = index.checked_sub(1).and_then(|i| self.positions.get(i));
        let after = self.positions.get(index);
        before
            .into_iter()
            .chain(after)
            .copied()
            .filter(|&candidate| (f64::from(candidate) - position).abs() <= tolerance)
            .min_by(|a, b| {
                (f64::from(*a) - position)
                    .abs()
                    .total_cmp(&(f64::from(*b) - position).abs())
                    .then_with(|| a.cmp(b))
            })
    }
}

fn uniform_line_color(
    image: &Image,
    axis: ScrollAxis,
    position: u32,
    cross: u32,
) -> Option<[u8; 4]> {
    let pixel = |along_cross: u32| {
        let (x, y) = match axis {
            ScrollAxis::Vertical => (along_cross, position),
            ScrollAxis::Horizontal => (position, along_cross),
        };
        let offset = (y as usize * image.width as usize + x as usize) * 4;
        let bytes = &image.bgra[offset..offset + 4];
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    };
    let color = pixel(0);
    (1..cross).all(|at| pixel(at) == color).then_some(color)
}

fn push_blank(positions: &mut Vec<u32>, start: u32, end: u32) {
    if end - start >= MIN_BLANK_LINES {
        positions.push(start + (end - start) / 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 4]) -> Image {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&pixel(x, y));
            }
        }
        Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        }
    }

    #[test]
    fn long_clip_merges_adjacent_ranges_and_restores_merged_records() {
        let mut clip = LongClip::new(100, 1000, ScrollAxis::Vertical).unwrap();
        assert!(clip.delete(200..300).unwrap());
        assert!(clip.delete(300..350).unwrap());
        assert!(!clip.delete(250..325).unwrap());
        assert!(clip.delete(100..225).unwrap());
        assert!(clip.delete(600..700).unwrap());
        assert_eq!(clip.removed(), &[100..350, 600..700]);
        assert!(clip.adjust(0, 150..650).unwrap());
        assert_eq!(clip.removed(), &[150..700]);
        assert_eq!(clip.output_size(), (100, 450));
        assert!(clip.restore(0));
        assert!(!clip.restore(0));
        assert!(clip.is_identity());
        assert!(!clip.clear());
    }

    #[test]
    fn long_clip_trim_preserves_interior_cuts_and_can_expand_the_edges() {
        let mut clip = LongClip::new(100, 100, ScrollAxis::Vertical).unwrap();
        clip.delete(0..10).unwrap();
        clip.delete(30..40).unwrap();
        clip.delete(90..100).unwrap();
        assert!(clip.trim(20..80, 5..95).unwrap());
        assert_eq!(clip.removed(), &[0..20, 30..40, 80..100]);
        assert_eq!(clip.cross_range(), 5..95);
        assert_eq!(clip.output_size(), (90, 50));
        assert!(!clip.trim(20..80, 5..95).unwrap());
        clip.trim(0..100, 0..100).unwrap();
        assert_eq!(clip.removed(), &[30..40]);
        assert_eq!(clip.output_size(), (100, 90));
        let before = clip.clone();
        assert!(clip.trim(32..38, 0..100).is_err());
        assert_eq!(clip, before);
        assert!(clip.adjust(0, 35..35).unwrap());
        assert!(clip.is_identity());
    }

    #[test]
    fn long_clip_invalid_or_empty_operations_are_atomic() {
        assert!(LongClip::new(0, 10, ScrollAxis::Vertical).is_err());
        assert!(LongClip::new(10, 0, ScrollAxis::Horizontal).is_err());
        let mut clip = LongClip::new(20, 100, ScrollAxis::Vertical).unwrap();
        assert!(!clip.delete(40..40).unwrap());
        clip.delete(0..50).unwrap();
        let before = clip.clone();
        for range in [50..100, 0..100, 9..8, 100..101] {
            assert!(clip.delete(range).is_err());
            assert_eq!(clip, before);
        }
        assert!(clip.adjust(0, 0..100).is_err());
        assert!(clip.adjust(1, 1..2).is_err());
        assert!(clip.set_cross(3..3).is_err());
        assert!(clip.set_cross(0..21).is_err());
        assert_eq!(clip, before);
        assert!(clip.clear());
        assert!(clip.is_identity());
    }

    #[test]
    fn long_clip_retained_segments_follow_both_axes() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let (width, height) = match axis {
                ScrollAxis::Vertical => (80, 120),
                ScrollAxis::Horizontal => (120, 80),
            };
            let mut clip = LongClip::new(width, height, axis).unwrap();
            clip.delete(20..40).unwrap();
            clip.set_cross(5..75).unwrap();
            assert_eq!(clip.source_size(), (width, height));
            assert_eq!(clip.axis(), axis);
            assert_eq!(
                clip.segments(),
                vec![
                    ClipSegment {
                        source: 0..20,
                        display_start: 0,
                    },
                    ClipSegment {
                        source: 40..120,
                        display_start: 20,
                    },
                ]
            );
            assert_eq!(
                clip.output_size(),
                match axis {
                    ScrollAxis::Vertical => (70, 100),
                    ScrollAxis::Horizontal => (100, 70),
                }
            );
        }
    }

    #[test]
    fn long_clip_coordinate_boundaries_define_seam_ownership() {
        let mut clip = LongClip::new(10, 100, ScrollAxis::Vertical).unwrap();
        for range in [0..10, 20..30, 40..50, 80..100] {
            clip.delete(range).unwrap();
        }
        assert_eq!(clip.output_size(), (10, 50));
        assert_eq!(clip.source_to_display(10.), Some(0.));
        assert_eq!(clip.source_to_display(19.5), Some(9.5));
        assert_eq!(clip.source_to_display(20.), None);
        assert_eq!(clip.source_to_display(30.), Some(10.));
        assert_eq!(clip.source_to_display(80.), None);
        assert_eq!(clip.source_to_display(100.), Some(50.));
        for (display, before, after) in [
            (0., 0., 10.),
            (10., 20., 30.),
            (20., 40., 50.),
            (50., 80., 100.),
        ] {
            assert_eq!(
                clip.display_to_source(display, SeamSide::Before),
                Some(before)
            );
            assert_eq!(
                clip.display_to_source(display, SeamSide::After),
                Some(after)
            );
        }
        for source in [10.25, 19.75, 30.5, 39.5, 50.25, 79.75] {
            let display = clip.source_to_display(source).unwrap();
            assert_eq!(
                clip.display_to_source(display, SeamSide::Before),
                Some(source)
            );
            assert_eq!(
                clip.display_to_source(display, SeamSide::After),
                Some(source)
            );
        }
        for position in [-1., 101., f32::NAN, f32::INFINITY] {
            assert_eq!(clip.source_to_display(position), None);
        }
        for position in [-1., 51., f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(clip.display_to_source(position, SeamSide::After), None);
        }
    }

    #[test]
    fn long_clip_output_limits_allow_editing_and_restoring_large_sources() {
        let mut pixels = LongClip::new(8001, 4000, ScrollAxis::Horizontal).unwrap();
        assert!(pixels.exceeds_output_limit());
        assert!(pixels.check_output().is_err());
        pixels.delete(0..1).unwrap();
        assert_eq!(pixels.output_size(), (8000, 4000));
        assert!(pixels.check_output().is_ok());
        assert!(pixels.restore(0));
        assert!(pixels.exceeds_output_limit());
        let mut side = LongClip::new(1, 70_000, ScrollAxis::Vertical).unwrap();
        assert!(side.exceeds_output_limit());
        side.delete(65_535..70_000).unwrap();
        assert!(side.check_output().is_ok());
        let largest = LongClip::new(u32::MAX, u32::MAX, ScrollAxis::Vertical).unwrap();
        assert!(largest.exceeds_output_limit());
    }

    #[test]
    fn long_clip_blank_snaps_require_six_identical_uniform_lines() {
        let source = image(4, 30, |x, y| match y {
            2..=7 => [255, 255, 255, 255],
            10..=15 => [20, 20, 20, 255],
            18..=22 => [90, 90, 90, 255],
            24..=29 => [y as u8, y as u8, y as u8, 255],
            _ => [x as u8, (x + y) as u8, 0, 255],
        });
        let snaps = BlankSnaps::from_image(&source, ScrollAxis::Vertical).unwrap();
        assert_eq!(snaps.positions(), &[5, 13]);
        assert_eq!(snaps.nearest(9., 4.), Some(5));
        assert_eq!(snaps.nearest(9., 3.9), None);
        assert_eq!(snaps.nearest(5.5, 0.5), Some(5));
        assert_eq!(snaps.nearest(13., 0.), Some(13));
        assert_eq!(snaps.nearest(f32::NAN, 5.), None);
        assert_eq!(snaps.nearest(5., -1.), None);
    }

    #[test]
    fn long_clip_blank_snaps_handle_horizontal_lines_and_validate_pixels() {
        let mut source = image(20, 3, |x, y| match x {
            5..=10 => [240, 240, 240, 255],
            12..=17 => [30, 30, 30, 255],
            _ => [y as u8, x as u8, 0, 255],
        });
        let snaps = BlankSnaps::from_image(&source, ScrollAxis::Horizontal).unwrap();
        assert_eq!(snaps.positions(), &[8, 15]);
        assert!(BlankSnaps::from_image(&source, ScrollAxis::Vertical)
            .unwrap()
            .positions()
            .is_empty());
        source.bgra.push(0);
        assert!(BlankSnaps::from_image(&source, ScrollAxis::Horizontal).is_err());
        assert!(BlankSnaps::from_image(&image(0, 3, |_, _| [0; 4]), ScrollAxis::Vertical).is_err());
    }
}
