//! 完整来源字素的实际发射采样带；不把 CTC 空白步或采样范围称作字形墨迹。
//! 证据随原行搬移，字节范围始终局部于行；图像版本资格由宿主任务门核定。
use super::{
    geometry::Projection, HorizontalDirection, RecognizedLine, RecognizedWord, TextGeometry,
};
use crate::geometry::Rect;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_SOURCE_BYTES: usize = 1_048_576;
pub const MAX_LINE_BYTES: usize = 65_536;
pub const MAX_LINE_GRAPHEMES: usize = 4_096;
pub const MAX_EMISSION_RUNS: usize = 32_768;
pub const MAX_SAMPLING_STEPS: usize = 65_536;
pub const MAX_TOTAL_GRAPHEMES: usize = 32_768;
pub const MAX_TOTAL_RUNS: usize = 32_768;
const MAX_WORDS: usize = 32_768;
const MAX_COORDINATE: u32 = 1_000_000;

/// 一个实际非空白 CTC 发射连续段，尚未改成包含 blank 的 Voronoi 范围。
/// source_range 是完整拼接行原文中的 UTF-8 byte 范围，steps 为半开区间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamplingRun {
    pub source_range: Range<usize>,
    pub steps: Range<usize>,
}

/// 完整 Unicode 字素对应的实际发射段外包络；start/end 沿原裁行阅读 u。
/// 多发射段之间的 blank 仅在组合字素外包络内出现，不另造字形边界。
#[derive(Clone, Debug, PartialEq)]
pub struct GraphemeBand {
    pub source_range: Range<usize>,
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceWord {
    text: String,
    source_range: Range<usize>,
    rect: Rect,
    direction: HorizontalDirection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Group {
    source_range: Range<usize>,
    run_range: Range<usize>,
}

/// 可选、只读的字素来源证据；私有快照避免消费者将旧范围绑定到新文字。
/// 不保存 word.geometry；选区外边界可依据墨迹独立内缩，不改动原始采样。
#[derive(Clone, Debug, PartialEq)]
pub struct GraphemeSampling {
    source_text: String,
    source_rect: Rect,
    source_direction: HorizontalDirection,
    source_geometry: TextGeometry,
    selection_geometry: Option<TextGeometry>,
    selection_rectified: bool,
    selection_reading_range: [f64; 2],
    source_words: Vec<SourceWord>,
    runs: Vec<SamplingRun>,
    groups: Vec<Group>,
    steps: usize,
    content_width: usize,
    tensor_width: usize,
}

impl GraphemeSampling {
    /// 从未扩张的实际发射段构造；新增证据不适用时返回 None，不改变 OCR 正文。
    /// line_index 仅检查当前词归属，不进入快照，以支持完整行去重和恢复重排。
    pub fn from_ctc(
        line_index: usize,
        line: &RecognizedLine,
        words: &[&RecognizedWord],
        runs: &[SamplingRun],
        steps: usize,
        content_width: usize,
        tensor_width: usize,
    ) -> Option<Self> {
        dimensions(steps, content_width, tensor_width)?;
        let (rect, geometry, source_words) = source(line_index, line, words)?;
        let groups = groups(&line.text, runs)?;
        bands(&groups, runs, steps, content_width, tensor_width)?;
        Some(Self {
            source_text: line.text.clone(),
            source_rect: rect,
            source_direction: line.horizontal_direction,
            source_geometry: geometry,
            selection_geometry: None,
            selection_rectified: false,
            selection_reading_range: [0., 1.],
            source_words,
            runs: runs.to_vec(),
            groups,
            steps,
            content_width,
            tensor_width,
        })
    }

    /// 先核当前完整行、词快照和有界原始发射，再返回实际采样带。
    /// 不核图像身份；宿主须先通过既有请求／源版本门才调用生产选字消费。
    pub fn validated_bands(
        &self,
        line_index: usize,
        line: &RecognizedLine,
        words: &[&RecognizedWord],
    ) -> Option<Vec<GraphemeBand>> {
        if self.source_text.len() > MAX_LINE_BYTES
            || self.runs.len() > MAX_EMISSION_RUNS
            || self.groups.len() > MAX_LINE_GRAPHEMES
            || self.source_words.len() > MAX_WORDS
            || self.source_text != line.text
            || Some(self.source_rect) != line.rect
            || self.source_direction != line.horizontal_direction
            || Some(self.source_geometry) != line.geometry
        {
            return None;
        }
        dimensions(self.steps, self.content_width, self.tensor_width)?;
        let (_, _, words) = source(line_index, line, words)?;
        if words != self.source_words || groups(&line.text, &self.runs)? != self.groups {
            return None;
        }
        bands(
            &self.groups,
            &self.runs,
            self.steps,
            self.content_width,
            self.tensor_width,
        )
    }

    /// 常量时间预算入口；须在 clone／验证前累计整个识别结果的预算。
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }
    pub fn grapheme_count(&self) -> usize {
        self.groups.len()
    }
    pub fn source_geometry(&self) -> TextGeometry {
        self.source_geometry
    }

    /// 选区可使用独立的统一行高；实际 CTC 发射仍绑定未收紧的原裁行。
    pub(crate) fn selection_geometry(&self) -> TextGeometry {
        self.selection_geometry
            .unwrap_or_else(|| self.source_geometry())
    }

    /// 只裁整行首尾，不把新的外边界用作缩放字素位置的坐标系。
    pub(crate) fn selection_reading_range(&self) -> [f64; 2] {
        self.selection_reading_range
    }

    #[cfg(test)]
    pub(crate) fn selection_rectified(&self) -> bool {
        self.selection_rectified
    }

    #[cfg(test)]
    pub(crate) fn clear_selection_adjustments(&mut self) {
        self.selection_geometry = None;
        self.selection_reading_range = [0., 1.];
        self.selection_rectified = false;
    }

    /// 像素证据由调用方提供；这里只允许近水平裁行拉平，并保持原阅读轴的物理 x。
    pub(crate) fn rectify_selection_geometry(&mut self, geometry: TextGeometry) -> bool {
        let Some(original) = Projection::new(self.source_geometry) else {
            return false;
        };
        let Some(rectified) = Projection::new(geometry) else {
            return false;
        };
        if !matches!(
            original.direction(),
            super::geometry::ReadingDirection::LeftToRight
                | super::geometry::ReadingDirection::RightToLeft
        ) || original.direction() != rectified.direction()
        {
            return false;
        }
        let [start, end] = [original.point(0., 0.5), original.point(1., 0.5)];
        let angle = f64::from(end.y - start.y)
            .abs()
            .atan2(f64::from(end.x - start.x).abs())
            .to_degrees();
        let quad = geometry.quad;
        if angle > 3.
            || quad[0].y != quad[1].y
            || quad[2].y != quad[3].y
            || quad[0].x != quad[3].x
            || quad[1].x != quad[2].x
        {
            return false;
        }
        let Some(centers) = self.selection_centers() else {
            return false;
        };
        let positions = [0., 1.]
            .into_iter()
            .chain(centers.iter().copied())
            .chain(centers.windows(2).map(|pair| (pair[0] + pair[1]) / 2.));
        for u in positions {
            if (f64::from(original.point(u, 0.5).x) - f64::from(rectified.point(u, 0.5).x)).abs()
                > 0.001
            {
                return false;
            }
        }
        self.selection_geometry = Some(geometry);
        self.selection_rectified = true;
        true
    }

    /// 原始字素中心决定的内部分界必须完整保留；空白字素不创建选字单元。
    pub(crate) fn tighten_selection_reading_range(&mut self, range: [f64; 2]) -> bool {
        let [start, end] = range;
        let [previous_start, previous_end] = self.selection_reading_range;
        if !start.is_finite()
            || !end.is_finite()
            || !(0. ..=1.).contains(&start)
            || !(0. ..=1.).contains(&end)
            || start >= end
            || start < previous_start
            || end > previous_end
        {
            return false;
        }
        let Some(centers) = self.selection_centers() else {
            return false;
        };
        if centers.len() > 1
            && (start >= (centers[0] + centers[1]) / 2.
                || end <= (centers[centers.len() - 2] + centers[centers.len() - 1]) / 2.)
        {
            return false;
        }
        self.selection_reading_range = range;
        true
    }

    fn selection_centers(&self) -> Option<Vec<f64>> {
        let bands = bands(
            &self.groups,
            &self.runs,
            self.steps,
            self.content_width,
            self.tensor_width,
        )?;
        let mut centers = Vec::with_capacity(bands.len());
        for band in bands {
            let cluster = self.source_text.get(band.source_range)?;
            if !cluster.chars().all(char::is_whitespace) {
                centers.push(((band.start + band.end) / 2.).clamp(0., 1.));
            }
        }
        if centers.is_empty() {
            return None;
        }
        // 与连续选区的已有回退一致，不能对同一份证据使用两套内部分界。
        if centers.windows(2).any(|pair| pair[1] <= pair[0]) {
            let count = centers.len() as f64;
            for (index, center) in centers.iter_mut().enumerate() {
                *center = (index as f64 + 0.5) / count;
            }
        }
        Some(centers)
    }

    /// 只接受原裁行法向的对称内缩，保留阅读轴坐标与字素中线停点。
    pub(crate) fn tighten_selection_geometry(&mut self, geometry: TextGeometry) -> bool {
        // 拉平已采用独立像素证据，不能由之后的原裁行法向内缩意外覆盖。
        if self.selection_rectified {
            return false;
        }
        let Some(original) = Projection::new(self.source_geometry) else {
            return false;
        };
        let Some(tightened) = Projection::new(geometry) else {
            return false;
        };
        let Some([_, inset]) = original.inverse(geometry.quad[0]) else {
            return false;
        };
        if !(0. ..0.5).contains(&inset) || original.direction() != tightened.direction() {
            return false;
        }
        let expected = [[0., inset], [1., inset], [1., 1. - inset], [0., 1. - inset]];
        for (corner, expected) in geometry.quad.into_iter().zip(expected) {
            let Some(local) = original.inverse(corner) else {
                return false;
            };
            if local
                .into_iter()
                .zip(expected)
                .any(|(value, expected)| (value - expected).abs() > 0.0001)
            {
                return false;
            }
        }
        if let Some(previous) = self.selection_geometry {
            let Some([_, previous_inset]) = original.inverse(previous.quad[0]) else {
                return false;
            };
            if inset + 0.0001 < previous_inset {
                return false;
            }
        }
        let Some(bands) = bands(
            &self.groups,
            &self.runs,
            self.steps,
            self.content_width,
            self.tensor_width,
        ) else {
            return false;
        };
        // 复核实际发射中心及相邻中心的停点，不用按字数等分的点代替 CTC。
        let centers: Vec<_> = bands
            .iter()
            .map(|band| (band.start + band.end) / 2.)
            .collect();
        let positions = [0., 1.]
            .into_iter()
            .chain(centers.iter().copied())
            .chain(centers.windows(2).map(|pair| (pair[0] + pair[1]) / 2.));
        for u in positions {
            let before = original.point(u, 0.5);
            let after = tightened.point(u, 0.5);
            if (f64::from(before.x) - f64::from(after.x))
                .hypot(f64::from(before.y) - f64::from(after.y))
                > 0.001
            {
                return false;
            }
        }
        self.selection_geometry = Some(geometry);
        self.selection_rectified = false;
        true
    }
}

fn dimensions(steps: usize, content: usize, tensor: usize) -> Option<()> {
    (steps > 0
        && steps <= MAX_SAMPLING_STEPS
        && content > 0
        && content <= tensor
        && tensor <= MAX_SAMPLING_STEPS)
        .then_some(())
}

fn source(
    line_index: usize,
    line: &RecognizedLine,
    words: &[&RecognizedWord],
) -> Option<(Rect, TextGeometry, Vec<SourceWord>)> {
    if line.text.is_empty()
        || line.text.len() > MAX_LINE_BYTES
        || line.text.chars().any(char::is_control)
        || words.is_empty()
        || words.len() > MAX_WORDS
    {
        return None;
    }
    let rect = line
        .rect
        .filter(|rect| rect.valid(MAX_COORDINATE, MAX_COORDINATE))?;
    let geometry = line.geometry?;
    if geometry
        .quad
        .iter()
        .any(|point| point.x < 0. || point.y < 0.)
    {
        return None;
    }
    let projection = Projection::new(geometry)?;
    // 原 full crop 可以超出 word-union rect；不能把它裁成轴对齐框。
    let direction = if projection.point(1., 0.5).x < projection.point(0., 0.5).x {
        HorizontalDirection::RightToLeft
    } else {
        HorizontalDirection::LeftToRight
    };
    if direction != line.horizontal_direction {
        return None;
    }
    let mut cursor = 0usize;
    let mut union = None;
    let mut source = Vec::with_capacity(words.len());
    for word in words {
        if word.line != line_index
            || word.text.trim().is_empty()
            || word.text.len() > line.text.len()
            || !word.rect.valid(MAX_COORDINATE, MAX_COORDINATE)
            || word.horizontal_direction != line.horizontal_direction
        {
            return None;
        }
        let found = line.text.get(cursor..)?.find(&word.text)?;
        let start = cursor.checked_add(found)?;
        let end = start.checked_add(word.text.len())?;
        if !line
            .text
            .get(cursor..start)?
            .chars()
            .all(char::is_whitespace)
        {
            return None;
        }
        source.push(SourceWord {
            text: word.text.clone(),
            source_range: start..end,
            rect: word.rect,
            direction: word.horizontal_direction,
        });
        union = Some(match union {
            None => word.rect,
            Some(old) => union_rect(old, word.rect),
        });
        cursor = end;
    }
    if union != Some(rect) || !line.text.get(cursor..)?.chars().all(char::is_whitespace) {
        return None;
    }
    Some((rect, geometry, source))
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn groups(text: &str, runs: &[SamplingRun]) -> Option<Vec<Group>> {
    if text.is_empty()
        || text.len() > MAX_LINE_BYTES
        || runs.is_empty()
        || runs.len() > MAX_EMISSION_RUNS
    {
        return None;
    }
    let mut result: Vec<Group> = text
        .grapheme_indices(true)
        .take(MAX_LINE_GRAPHEMES + 1)
        .map(|(byte, cluster)| Group {
            source_range: byte..byte + cluster.len(),
            run_range: 0..0,
        })
        .collect();
    if result.is_empty() || result.len() > MAX_LINE_GRAPHEMES {
        return None;
    }
    let (mut byte, mut previous_step, mut group_index) = (0usize, 0usize, 0usize);
    for (index, run) in runs.iter().enumerate() {
        if run.source_range.start != byte
            || run.source_range.start >= run.source_range.end
            || text.get(run.source_range.clone()).is_none()
            || run.steps.start < previous_step
            || run.steps.start >= run.steps.end
        {
            return None;
        }
        while result.get(group_index)?.source_range.end <= run.source_range.start {
            group_index += 1;
        }
        let group = result.get_mut(group_index)?;
        // 一个字典 token 跨两个完整字素时没有内部发射证据，本行整体回退。
        if run.source_range.end > group.source_range.end {
            return None;
        }
        if group.run_range.is_empty() {
            group.run_range.start = index;
        }
        group.run_range.end = index + 1;
        byte = run.source_range.end;
        previous_step = run.steps.end;
    }
    if byte != text.len()
        || result.iter().any(|group| {
            group.run_range.is_empty()
                || runs[group.run_range.start].source_range.start != group.source_range.start
                || runs[group.run_range.end - 1].source_range.end != group.source_range.end
        })
    {
        return None;
    }
    Some(result)
}

fn bands(
    groups: &[Group],
    runs: &[SamplingRun],
    steps: usize,
    content_width: usize,
    tensor_width: usize,
) -> Option<Vec<GraphemeBand>> {
    let denominator = steps as f64 * content_width as f64;
    let mut result = Vec::with_capacity(groups.len());
    for group in groups {
        let first = runs.get(group.run_range.start)?;
        let last = runs.get(group.run_range.end.checked_sub(1)?)?;
        if last.steps.end > steps {
            return None;
        }
        let start = (first.steps.start as f64 * tensor_width as f64) / denominator;
        let end = (last.steps.end as f64 * tensor_width as f64) / denominator;
        // 不 clamp padding 的发射到图像边沿或零宽，也不补齐前后 blank。
        if !start.is_finite()
            || !end.is_finite()
            || !(0. ..=1.).contains(&start)
            || !(0. ..=1.).contains(&end)
            || start >= end
        {
            return None;
        }
        result.push(GraphemeBand {
            source_range: group.source_range.clone(),
            start,
            end,
        });
    }
    Some(result)
}

#[cfg(test)]
#[path = "recognition_graphemes_tests.rs"]
mod tests;
