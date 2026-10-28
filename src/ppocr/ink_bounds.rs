//! 仅以完整原图中明确的墨迹证据，沿词／行投影法向对称缩小几何。
//! CTC 的 u 带、中心线停点、正文与原外包络均保留；不声称精确字形。
use super::RgbImage;
use crate::{
    annotation::Point,
    geometry::Rect,
    recognition::{geometry::Projection, HorizontalDirection, RecognizedText, TextGeometry},
    Result,
};
use unicode_segmentation::UnicodeSegmentation;

#[path = "ink_alignment.rs"]
mod ink_alignment;

const MAX_WORDS: usize = 4_096;
const MAX_WORD_PIXELS: usize = 65_536;
const MAX_TOTAL_PIXELS: usize = 2_000_000;
const WEAK_DELTA: f32 = 2.;
const STRONG_DELTA: f32 = 40.;
const BINS: usize = 64;

#[derive(Clone, Copy)]
struct Roi {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
}
impl Roi {
    fn new(projection: Projection, rgb: &RgbImage) -> Option<Self> {
        let quad = projection.quad();
        let left = quad
            .iter()
            .map(|p| p.x)
            .fold(f32::INFINITY, f32::min)
            .floor();
        let top = quad
            .iter()
            .map(|p| p.y)
            .fold(f32::INFINITY, f32::min)
            .floor();
        let right = quad
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil();
        let bottom = quad
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            .ceil();
        // 不夹取到图像边缘；完整 halo 及其外侧像素都必须可用。
        if left < 2. || top < 2. || right + 2. > rgb.width as f32 || bottom + 2. > rgb.height as f32
        {
            return None;
        }
        let roi = Self {
            left: left as usize - 1,
            top: top as usize - 1,
            right: right as usize + 1,
            bottom: bottom as usize + 1,
        };
        (roi.area()? <= MAX_WORD_PIXELS).then_some(roi)
    }
    fn area(self) -> Option<usize> {
        self.right
            .checked_sub(self.left)?
            .checked_mul(self.bottom.checked_sub(self.top)?)
    }
}

pub(super) fn tighten(
    mut result: RecognizedText,
    rgb: &RgbImage,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<RecognizedText> {
    checkpoint()?;
    if result.words.is_empty() {
        return Ok(result);
    }
    let Some(count) = rgb.width.checked_mul(rgb.height) else {
        return Ok(result);
    };
    if rgb.width == 0
        || rgb.height == 0
        || count > 32_000_000
        || rgb.pixels.len() != count
        || result.words.len() > MAX_WORDS
        || result.lines.len() > 1_000
        || result.words.len() != result.regions.len()
        || result.text
            != result
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
    {
        return Ok(result);
    }
    // 原 RGB 浮点数组直接读取；坏数据不经 clamp／插值伪装成证据。
    for row in rgb.pixels.chunks_exact(rgb.width) {
        checkpoint()?;
        if row
            .iter()
            .flatten()
            .any(|p| !p.is_finite() || !(0. ..=255.).contains(p))
        {
            return Ok(result);
        }
    }
    let image = Rect::full(rgb.width as u32, rgb.height as u32);
    let mut line_geometry = Vec::with_capacity(result.lines.len());
    let mut boundaries = Vec::with_capacity(result.lines.len());
    for line in &result.lines {
        checkpoint()?;
        if line.text.trim().is_empty()
            || !line
                .rect
                .is_some_and(|r| r.valid(image.width, image.height))
        {
            return Ok(result);
        }
        line_geometry.push(
            line.geometry
                .and_then(|g| g.validated_in(image))
                .filter(|p| horizontal(*p) == line.horizontal_direction),
        );
        boundaries.push(
            line.text
                .grapheme_indices(true)
                .map(|(byte, _)| byte)
                .chain(std::iter::once(line.text.len()))
                .collect::<Vec<_>>(),
        );
    }
    let mut union = vec![None; result.lines.len()];
    let mut cursors = vec![0usize; result.lines.len()];
    let mut plans = Vec::with_capacity(result.words.len());
    let mut total = 0usize;
    for (word, region) in result.words.iter().zip(&result.regions) {
        checkpoint()?;
        if word.line >= result.lines.len()
            || word.rect != *region
            || !region.valid(image.width, image.height)
            || word.text.trim().is_empty()
            || word.horizontal_direction != result.lines[word.line].horizontal_direction
        {
            return Ok(result);
        }
        let content = &result.lines[word.line].text;
        let cursor = cursors[word.line];
        let Some(found) = content.get(cursor..).and_then(|rest| rest.find(&word.text)) else {
            return Ok(result);
        };
        let start = cursor + found;
        let end = start + word.text.len();
        if !content[cursor..start].chars().all(char::is_whitespace)
            || boundaries[word.line].binary_search(&start).is_err()
            || boundaries[word.line].binary_search(&end).is_err()
        {
            return Ok(result);
        }
        cursors[word.line] = end;
        union[word.line] = Some(match union[word.line] {
            None => word.rect,
            Some(old) => union_rect(old, word.rect),
        });
        let plan = eligible(&word.text).and_then(|graphemes| {
            let line = line_geometry[word.line]?;
            let projection = word.geometry?.validated_in(word.rect)?;
            if projection.direction() != line.direction()
                || horizontal(projection) != word.horizontal_direction
                || !projection.quad().iter().all(|&corner| {
                    line.inverse(corner).is_some_and(|local| {
                        local.iter().all(|value| (-0.0001..=1.0001).contains(value))
                    })
                })
            {
                return None;
            }
            Some((projection, Roi::new(projection, rgb)?, graphemes))
        });
        if let Some((_, roi, _)) = plan {
            total += roi.area().unwrap_or(MAX_TOTAL_PIXELS + 1);
            if total > MAX_TOTAL_PIXELS {
                return Ok(result);
            }
        }
        plans.push(plan);
    }
    for ((line, rect), cursor) in result.lines.iter().zip(union).zip(cursors) {
        checkpoint()?;
        if line.rect != rect || !line.text[cursor..].chars().all(char::is_whitespace) {
            return Ok(result);
        }
    }
    // 选字共用整行高度。单独保存行高证据，不改 CTC 来源、原行框或字间停点；
    // 词带左右端常切到笔画，不能以逐词收紧成功作为整行收紧的前提。
    let mut line_plans = Vec::with_capacity(result.lines.len());
    for (line, projection) in result.lines.iter().zip(line_geometry) {
        checkpoint()?;
        let plan = projection.and_then(|projection| {
            (line.graphemes.is_some() && line.text.chars().any(char::is_alphanumeric))
                .then_some(())?;
            Some((projection, Roi::new(projection, rgb)?))
        });
        if let Some((_, roi)) = plan {
            let pixels = roi.area().unwrap_or(MAX_TOTAL_PIXELS + 1);
            if pixels > MAX_TOTAL_PIXELS.saturating_sub(total) {
                // 新行高证据超预算只跳过该行，不撤销原逐词路径的可用计划。
                line_plans.push(None);
                continue;
            }
            total += pixels;
        }
        line_plans.push(plan);
    }
    for (word, plan) in result.words.iter_mut().zip(plans) {
        checkpoint()?;
        if let Some((projection, roi, graphemes)) = plan {
            if let Some(geometry) =
                evidence(projection, roi, rgb, graphemes, false, &mut checkpoint)?
                    .and_then(|g| preserve_stops(projection, g, word.rect, graphemes))
            {
                word.geometry = Some(geometry);
            }
        }
    }
    for (line, plan) in result.lines.iter_mut().zip(line_plans) {
        checkpoint()?;
        if let Some((projection, roi)) = plan {
            // 整行检查所有非背景像素，包含最淡的抗锯齿边；主体阈值只用于
            // 确认存在文字，绝不用于忽略淡笔画或选择要裁掉的像素。
            if let Some(evidence) = line_evidence(projection, roi, rgb, &mut checkpoint)? {
                if let Some(sampling) = &mut line.graphemes {
                    sampling.tighten_selection_geometry(evidence.geometry);
                    let mut reading = evidence.reading;
                    if let Some(aligned) = aligned_evidence(projection, roi, rgb, &mut checkpoint)?
                    {
                        if sampling.rectify_selection_geometry(aligned.geometry) {
                            reading = aligned.reading;
                        }
                    }
                    if !sampling.tighten_selection_reading_range(reading) {
                        // 一端不得越过已有的内部分字边界；另一端仍可独立安全收紧。
                        let [start, end] = reading;
                        let current = sampling.selection_reading_range();
                        sampling.tighten_selection_reading_range([start, current[1]]);
                        let current = sampling.selection_reading_range();
                        sampling.tighten_selection_reading_range([current[0], end]);
                    }
                }
            }
        }
    }
    checkpoint()?;
    Ok(result)
}

fn eligible(text: &str) -> Option<usize> {
    if text.is_empty() || text.len() > 1_024 {
        return None;
    }
    let mut count = 0;
    for grapheme in text.graphemes(true) {
        let mut chars = grapheme.chars();
        let c = chars.next()?;
        if chars.next().is_some()
            || !(c.is_ascii_alphanumeric()
                || matches!(c as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF | 0x2CEB0..=0x2EBEF
            | 0x2EBF0..=0x2EE5F | 0x2F800..=0x2FA1F | 0x30000..=0x3134F | 0x31350..=0x323AF))
        {
            return None;
        }
        count += 1;
    }
    (count <= 256).then_some(count)
}

fn horizontal(p: Projection) -> HorizontalDirection {
    if p.point(1., 0.5).x < p.point(0., 0.5).x {
        HorizontalDirection::RightToLeft
    } else {
        HorizontalDirection::LeftToRight
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn cell(projection: Projection, x: usize, y: usize) -> Option<[f64; 4]> {
    let local = [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)].map(|(x, y)| {
        projection.inverse(Point {
            x: x as f32,
            y: y as f32,
        })
    });
    let local = [local[0]?, local[1]?, local[2]?, local[3]?];
    Some([
        local.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min),
        local.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min),
        local.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max),
        local.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max),
    ])
}

#[derive(Clone, Copy)]
struct Evidence {
    /// 全阅读范围的独立显示行高，不能用裁短后的框重新分配字间停点。
    geometry: TextGeometry,
    reading: [f64; 2],
}

impl Evidence {
    fn quad(self) -> Option<TextGeometry> {
        let projection = Projection::new(self.geometry)?;
        Some(TextGeometry {
            quad: [
                projection.point(self.reading[0], 0.),
                projection.point(self.reading[1], 0.),
                projection.point(self.reading[1], 1.),
                projection.point(self.reading[0], 1.),
            ],
        })
    }
}

fn evidence(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    graphemes: usize,
    include_faint: bool,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<TextGeometry>> {
    Ok(
        scan_evidence(projection, roi, rgb, graphemes, include_faint, checkpoint)?
            .map(|evidence| evidence.geometry),
    )
}

fn line_evidence(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Evidence>> {
    scan_evidence(projection, roi, rgb, 1, true, checkpoint)
}

fn aligned_evidence(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Evidence>> {
    let Some(geometry) = ink_alignment::horizontal_geometry(projection, roi, rgb, checkpoint)?
    else {
        return Ok(None);
    };
    let Some(projection) = Projection::new(geometry) else {
        return Ok(None);
    };
    let reading = if let Some(roi) = Roi::new(projection, rgb) {
        aligned_reading(projection, roi, rgb, checkpoint)?
    } else {
        None
    };
    // 坐标系拉平后重新计算端部证据；不能把旧倾斜坐标中的 u 极值直接套入新框。
    Ok(Some(Evidence {
        geometry,
        reading: reading.unwrap_or([0., 1.]),
    }))
}

/// 水平性、背景和文字资格已经通过；此处只在新映射中测两端并复核完整足迹。
fn aligned_reading(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<[f64; 2]>> {
    let background = rgb.pixels[roi.top * rgb.width + roi.left];
    let (mut first, mut last) = (1f64, 0f64);
    let (mut top, mut bottom) = (1f64, 0f64);
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            if rgb.pixels[y * rgb.width + x] == background {
                continue;
            }
            let Some([u0, v0, u1, v1]) = cell(projection, x, y) else {
                return Ok(None);
            };
            if u0 < 0. || v0 < 0. || u1 > 1. || v1 > 1. {
                return Ok(None);
            }
            first = first.min(u0);
            last = last.max(u1);
            top = top.min(v0);
            bottom = bottom.max(v1);
        }
    }
    if first >= last {
        return Ok(None);
    }
    let padding = selection_padding((bottom - top) * normal_height(projection));
    let reading = reading_trim(projection, first, last, padding);
    let Some(cropped) = (Evidence {
        geometry: TextGeometry {
            quad: projection.quad(),
        },
        reading,
    })
    .quad()
    .and_then(Projection::new) else {
        return Ok(None);
    };
    let edges = padding_edges(cropped.quad());
    let moved = [false, reading[1] < 1., false, reading[0] > 0.];
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            if rgb.pixels[y * rgb.width + x] == background {
                continue;
            }
            let Some([u0, v0, u1, v1]) = cell(cropped, x, y) else {
                return Ok(None);
            };
            if u0 < 0. || v0 < 0. || u1 > 1. || v1 > 1. {
                return Ok(None);
            }
            if !pixel_has_padding(&edges, moved, x, y, padding) {
                return Ok(None);
            }
        }
    }
    Ok(Some(reading))
}

fn normal_height(projection: Projection) -> f64 {
    let q = projection.quad();
    let edges = padding_edges(q);
    [(q[0], 2), (q[1], 2), (q[2], 0), (q[3], 0)]
        .into_iter()
        .map(|(p, edge)| {
            edges[edge][0] * f64::from(p.x) + edges[edge][1] * f64::from(p.y) + edges[edge][2]
        })
        .fold(f64::INFINITY, f64::min)
}

fn padding_edges(quad: [Point; 4]) -> [[f64; 3]; 4] {
    std::array::from_fn(|i| {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let dx = f64::from(b.x) - f64::from(a.x);
        let dy = f64::from(b.y) - f64::from(a.y);
        let length = dx.hypot(dy);
        [
            -dy / length,
            dx / length,
            (dy * f64::from(a.x) - dx * f64::from(a.y)) / length,
        ]
    })
}

fn pixel_has_padding(
    edges: &[[f64; 3]; 4],
    moved: [bool; 4],
    x: usize,
    y: usize,
    padding: f64,
) -> bool {
    edges.iter().zip(moved).all(|(&[a, b, c], moved)| {
        // 线性距离在像素方块四角取最小值，不只检查中心点。
        !moved || a * x as f64 + b * y as f64 + c + a.min(0.) + b.min(0.) >= padding - 0.001
    })
}

/// 交互选区需要可见留白；完整包含墨迹不等于边缘已经足够舒适。
fn selection_padding(ink_height: f64) -> f64 {
    (ink_height * 0.12).max(2.)
}

fn reading_trim(projection: Projection, first: f64, last: f64, padding: f64) -> [f64; 2] {
    let q = projection.quad();
    let edges = padding_edges(q);
    let width = [(q[0], 1), (q[3], 1), (q[1], 3), (q[2], 3)]
        .into_iter()
        .map(|(p, edge)| {
            edges[edge][0] * f64::from(p.x) + edges[edge][1] * f64::from(p.y) + edges[edge][2]
        })
        .fold(f64::INFINITY, f64::min);
    let mut reading = [0., 1.];
    if width.is_finite() && width > 0. {
        let guard = padding / width;
        let start = (first - (first * 0.2).max(guard)).max(0.);
        let end = (last + ((1. - last) * 0.2).max(guard)).min(1.);
        if start * width >= 0.5 {
            reading[0] = start
        }
        if (1. - end) * width >= 0.5 {
            reading[1] = end
        }
    }
    reading
}

fn scan_evidence(
    projection: Projection,
    roi: Roi,
    rgb: &RgbImage,
    graphemes: usize,
    include_faint: bool,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Evidence>> {
    let background = rgb.pixels[roi.top * rgb.width + roi.left];
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            if y == roi.top || y + 1 == roi.bottom || x == roi.left || x + 1 == roi.right {
                let pixel = rgb.pixels[y * rgb.width + x];
                if (0..3).any(|c| (pixel[c] - background[c]).abs() > 1.) {
                    return Ok(None);
                }
            }
        }
    }
    let (mut first, mut last) = (1f64, 0f64);
    let (mut first_u, mut last_u) = (1f64, 0f64);
    let mut polarity = 0i8;
    let mut strong = vec![0usize; graphemes];
    let mut occupied = [false; BINS];
    let mut grid = [[false; 32]; 32];
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            let pixel = rgb.pixels[y * rgb.width + x];
            let delta = std::array::from_fn::<_, 3, _>(|c| pixel[c] - background[c]);
            if delta.iter().all(|d| *d == 0.) {
                continue;
            }
            if !include_faint && delta.iter().all(|d| d.abs() < WEAK_DELTA) {
                return Ok(None);
            }
            let positive = delta.iter().any(|d| {
                if include_faint {
                    *d > 0.
                } else {
                    *d >= WEAK_DELTA
                }
            });
            let negative = delta.iter().any(|d| {
                if include_faint {
                    *d < 0.
                } else {
                    *d <= -WEAK_DELTA
                }
            });
            if positive == negative {
                return Ok(None);
            }
            let sign = if positive { 1 } else { -1 };
            if polarity != 0 && polarity != sign {
                return Ok(None);
            }
            polarity = sign;
            let Some([u0, v0, u1, v1]) = cell(projection, x, y) else {
                return Ok(None);
            };
            // 全像素单元须在词带内部；附近文字或弱 AA 越界不能忽略。
            if u0 <= 0. || v0 <= 0. || u1 >= 1. || v1 >= 1. {
                return Ok(None);
            }
            first = first.min(v0);
            last = last.max(v1);
            first_u = first_u.min(u0);
            last_u = last_u.max(u1);
            let bins = (v0 * BINS as f64).floor() as usize..=(v1 * BINS as f64).floor() as usize;
            occupied[bins].fill(true);
            if (delta.iter().sum::<f32>() / 3.).abs() >= STRONG_DELTA {
                // 不能让一个深字为同词的相邻淡字提供全部主体证据。
                let start = (u0 * graphemes as f64).floor() as usize;
                let end = (u1 * graphemes as f64).floor() as usize;
                if start == end {
                    strong[start] += 1;
                }
                let rows = (v0 * 32.).floor() as usize..=(v1 * 32.).floor() as usize;
                let columns = (u0 * 32.).floor() as usize..=(u1 * 32.).floor() as usize;
                for row in &mut grid[rows] {
                    row[columns.clone()].fill(true);
                }
            }
        }
    }
    if strong.iter().any(|count| *count < 4) || !single_band(&occupied) || frame_like(&grid) {
        return Ok(None);
    }
    let height = normal_height(projection);
    let padding = selection_padding((last - first) * height);
    let blank = first.min(1. - last);
    // 行选区每侧至少保留两个像素／墨迹高的12%，且行高不少于原来的60%。
    // 不足以留下该余量时保持原高；旧词框路径保持原来的保守规则。
    let a = if include_faint {
        (blank * 0.8).min(blank - padding / height).clamp(0., 0.2)
    } else {
        (blank * 0.8).min(0.35)
    };
    if !a.is_finite() || (!include_faint && a < 0.025) {
        return Ok(None);
    }
    let a = if a >= 0.025 { a } else { 0. };
    let geometry = TextGeometry {
        quad: [
            projection.point(0., a),
            projection.point(1., a),
            projection.point(1., 1. - a),
            projection.point(0., 1. - a),
        ],
    };
    // 不按文字个数均分，也不改阅读映射。两端沿用同一字号留白底线；
    // 不足半像素的移动直接保留，避免浮点噪声抖动。
    let reading = if include_faint {
        reading_trim(projection, first_u, last_u, padding)
    } else {
        [0., 1.]
    };
    // 留白不足以收紧时，整行仍可提供拉平所需的像素资格；不要连方向修复一起跳过。
    if !include_faint && a == 0. && reading == [0., 1.] {
        return Ok(None);
    }
    let candidate = Evidence { geometry, reading };
    let Some(tightened) = candidate.quad().and_then(Projection::new) else {
        return Ok(None);
    };
    if include_faint && normal_height(tightened) + 0.001 < height * 0.6 {
        return Ok(None);
    }
    let edges = padding_edges(tightened.quad());
    let moved = [a > 0., reading[1] < 1., a > 0., reading[0] > 0.];
    // f32 四角重建后再逐单元复核，不能只凭理论局部余白断言 AA 仍在框内。
    for y in roi.top..roi.bottom {
        checkpoint()?;
        for x in roi.left..roi.right {
            let pixel = rgb.pixels[y * rgb.width + x];
            if pixel == background {
                continue;
            }
            if !include_faint && (0..3).all(|c| (pixel[c] - background[c]).abs() < WEAK_DELTA) {
                return Ok(None);
            }
            let Some([u0, v0, u1, v1]) = cell(tightened, x, y) else {
                return Ok(None);
            };
            if u0 < 0. || v0 < 0. || u1 > 1. || v1 > 1. {
                return Ok(None);
            }
            if include_faint && !pixel_has_padding(&edges, moved, x, y, padding) {
                return Ok(None);
            }
        }
    }
    Ok(Some(candidate))
}

fn single_band(occupied: &[bool; BINS]) -> bool {
    let Some(first) = occupied.iter().position(|v| *v) else {
        return false;
    };
    let last = occupied.iter().rposition(|v| *v).unwrap_or(first);
    occupied[first..=last].iter().all(|v| *v)
}

fn frame_like(grid: &[[bool; 32]; 32]) -> bool {
    let (mut left, mut top, mut right, mut bottom) = (32usize, 32usize, 0usize, 0usize);
    for (y, row) in grid.iter().enumerate() {
        for (x, value) in row.iter().enumerate() {
            if *value {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
            }
        }
    }
    if right < left + 4 || bottom < top + 4 {
        return false;
    }
    let width = right - left + 1;
    let height = bottom - top + 1;
    let horizontal = [top, bottom]
        .iter()
        .all(|&y| grid[y][left..=right].iter().filter(|v| **v).count() * 4 >= width * 3);
    let vertical = [left, right]
        .iter()
        .all(|&x| (top..=bottom).filter(|&y| grid[y][x]).count() * 4 >= height * 3);
    let inner = (top + 1..bottom)
        .map(|y| grid[y][left + 1..right].iter().filter(|v| **v).count())
        .sum::<usize>();
    horizontal && vertical && inner * 4 < (width - 2) * (height - 2)
}

fn preserve_stops(
    original: Projection,
    geometry: TextGeometry,
    rect: Rect,
    graphemes: usize,
) -> Option<TextGeometry> {
    let tightened = geometry.validated_in(rect)?;
    if tightened.direction() != original.direction()
        || horizontal(tightened) != horizontal(original)
    {
        return None;
    }
    for index in 0..=graphemes {
        let u = index as f64 / graphemes as f64;
        let before = original.point(u, 0.5);
        let after = tightened.point(u, 0.5);
        if (f64::from(before.x) - f64::from(after.x))
            .hypot(f64::from(before.y) - f64::from(after.y))
            > 0.001
        {
            return None;
        }
    }
    Some(geometry)
}

#[cfg(test)]
#[path = "ink_bounds_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ink_bounds_actual_qa.rs"]
mod actual_qa;

#[cfg(test)]
#[path = "ink_endpoints_tests.rs"]
mod endpoints_tests;

#[cfg(test)]
#[path = "ink_alignment_font_qa.rs"]
mod alignment_font_qa;
