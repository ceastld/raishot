//! 识别文字的持久标记；仅生成既有文档对象，不依赖 OCR 提供者、窗口或选区。
//! 四角沿裁行阅读坐标 u0v0/u1v0/u1v1/u0v1；仅为估计文字单元，全画布物理坐标。
use crate::{
    annotation::{Annotation, HighlighterBlend, Point, Style, Tool},
    recognition::{geometry::Projection, TextGeometry},
    Result,
};

const MAX_SPANS: usize = 4096;
const MAX_POINTS: usize = 65_536;
const MAX_COORDINATE: f32 = 1_000_000.;
const MAX_EXTENT: f64 = 65_535.;
const MAX_LEDGER_RECORDS: usize = 8192;
const MAX_LEDGER_POINTS: usize = 262_144;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMark {
    Highlight,
    Underline,
    Wave,
    Strike,
}
impl TextMark {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Highlight => tr!("高亮", "Highlight"),
            Self::Underline => tr!("下划线", "Underline"),
            Self::Wave => tr!("波浪线", "Wave"),
            Self::Strike => tr!("删除线", "Strike"),
        }
    }
}

struct Frame {
    origin: Point,
    width: f32,
    height: f32,
    right: Point,
    down: Point,
    projection: Option<Projection>,
}
impl Frame {
    fn from_quad(quad: &[Point; 4]) -> Result<Self> {
        if quad.iter().any(|point| {
            !point.x.is_finite()
                || !point.y.is_finite()
                || point.x.abs() > MAX_COORDINATE
                || point.y.abs() > MAX_COORDINATE
        }) {
            return Err("文字标记坐标无效或超出范围".into());
        }
        let (ux, uy) = (
            f64::from(quad[1].x) - f64::from(quad[0].x),
            f64::from(quad[1].y) - f64::from(quad[0].y),
        );
        let (vx, vy) = (
            f64::from(quad[3].x) - f64::from(quad[0].x),
            f64::from(quad[3].y) - f64::from(quad[0].y),
        );
        let (width, height) = (ux.hypot(uy), vx.hypot(vy));
        // 保留既有旋转矩形对象；其余有效凸四边形沿裁行投影，不能退回 AABB。
        let closure = (f64::from(quad[2].x) - f64::from(quad[0].x) - ux - vx)
            .hypot(f64::from(quad[2].y) - f64::from(quad[0].y) - uy - vy);
        if !(0.25..=MAX_EXTENT).contains(&width)
            || !(0.25..=MAX_EXTENT).contains(&height)
            || ux * vy - uy * vx <= 0.
        {
            return Err("文字标记需要有效的非退化裁行四边形".into());
        }
        let projection =
            Projection::new(TextGeometry { quad: *quad }).ok_or("文字标记投影无效或不可逆")?;
        for pair in [(quad[1], quad[2]), (quad[3], quad[2])] {
            if !(0.25..=MAX_EXTENT).contains(&f64::from(pair.0.distance(pair.1))) {
                return Err("文字标记裁行边长无效或超出范围".into());
            }
        }
        let coordinate = quad
            .iter()
            .map(|p| p.x.abs().max(p.y.abs()))
            .fold(1., f32::max);
        let numerical = 0.0001 + f64::from(coordinate) * f64::from(f32::EPSILON) * 2.;
        let rectangular = (ux * vx + uy * vy).abs() <= width * height * 0.001
            && closure <= 0.25 + width.max(height) * 0.00001
            && (ux * vx + uy * vy).abs() <= (width + height) * numerical * 2.
            && closure <= numerical * 4.;
        Ok(Self {
            origin: quad[0],
            width: width as f32,
            height: height as f32,
            right: Point {
                x: (ux / width) as f32,
                y: (uy / width) as f32,
            },
            down: Point {
                x: (vx / height) as f32,
                y: (vy / height) as f32,
            },
            projection: (!rectangular).then_some(projection),
        })
    }
    fn map(&self, x: f32, y: f32) -> Point {
        if let Some(projection) = &self.projection {
            return projection.point(
                f64::from(x) / f64::from(self.width),
                f64::from(y) / f64::from(self.height),
            );
        }
        Point {
            x: self.origin.x + x * self.right.x + y * self.down.x,
            y: self.origin.y + x * self.right.y + y * self.down.y,
        }
    }
    fn local(&self, point: Point) -> Option<[f64; 2]> {
        if let Some(projection) = &self.projection {
            let [u, v] = projection.inverse(point)?;
            return Some([u * f64::from(self.width), v * f64::from(self.height)]);
        }
        let dx = f64::from(point.x) - f64::from(self.origin.x);
        let dy = f64::from(point.y) - f64::from(self.origin.y);
        let determinant = f64::from(self.right.x) * f64::from(self.down.y)
            - f64::from(self.right.y) * f64::from(self.down.x);
        let x = (dx * f64::from(self.down.y) - dy * f64::from(self.down.x)) / determinant;
        let y = (dy * f64::from(self.right.x) - dx * f64::from(self.right.y)) / determinant;
        (x.is_finite() && y.is_finite()).then_some([x, y])
    }
    /// 截取原有世界线段，不重新生成未选中的波形。投影的 u 在线段上非线性变化。
    fn clip_fraction(&self, a: Point, b: Point, ax: f64, bx: f64, x: f64) -> Result<f64> {
        if self.projection.is_none() {
            return Ok(((x - ax) / (bx - ax)).clamp(0., 1.));
        }
        let c = self.map(x as f32, 0.);
        let d = self.map(x as f32, self.height);
        let (sx, sy) = (
            f64::from(b.x) - f64::from(a.x),
            f64::from(b.y) - f64::from(a.y),
        );
        let (lx, ly) = (
            f64::from(d.x) - f64::from(c.x),
            f64::from(d.y) - f64::from(c.y),
        );
        let denominator = sx * ly - sy * lx;
        let numerator =
            (f64::from(c.x) - f64::from(a.x)) * ly - (f64::from(c.y) - f64::from(a.y)) * lx;
        let t = numerator / denominator;
        if !t.is_finite() || !(-0.00001..=1.00001).contains(&t) {
            return Err("文字波浪线投影裁切不可逆".into());
        }
        Ok(t.clamp(0., 1.))
    }
    fn wave_steps(&self) -> usize {
        // 每周期八段；周期随字高变化，短字也保持完整的起止波形。
        let wavelength = (self.height * 0.45).clamp(5., 12.);
        (self.width / wavelength).round().max(1.) as usize * 8
    }
    fn wave_amplitude(&self) -> f32 {
        (self.height * 0.065).min(1.8).min(self.width * 0.12)
    }
    fn validate_wave_envelope(&self) -> Result<()> {
        if self.projection.is_some() {
            let amplitude = self.wave_amplitude();
            let quad = [
                self.map(0., self.height - amplitude),
                self.map(self.width, self.height - amplitude),
                self.map(self.width, self.height + amplitude),
                self.map(0., self.height + amplitude),
            ];
            // 扩展 v 域仍须保持正分母的严格凸映射；跨过透视极点的有限端点也拒绝。
            Projection::new(TextGeometry { quad }).ok_or("文字波浪线幅度超出有效透视域")?;
        }
        Ok(())
    }
}

/// 只采用颜色、线宽和不透明度；不继承其它绘图工具的虚线、平滑或箭头设置。
/// 所有输入与点预算先验证，失败时不返回半批对象；调用者应一次 `insert_many` 提交。
pub(crate) fn annotations(
    mark: TextMark,
    quads: &[[Point; 4]],
    style: Style,
) -> Result<Vec<Annotation>> {
    let frames = frames(mark, quads, style)?;
    let annotations: Vec<_> = frames
        .into_iter()
        .map(|frame| match mark {
            TextMark::Highlight => highlight(&frame, style),
            _ => stroke(mark, &frame, style),
        })
        .collect();
    if annotations.iter().any(|annotation| {
        annotation.points.iter().any(|p| {
            !p.x.is_finite()
                || !p.y.is_finite()
                || p.x.abs() > MAX_COORDINATE
                || p.y.abs() > MAX_COORDINATE
        }) || (annotation.tool == Tool::Polyline && !annotation.valid())
    }) {
        return Err("文字标记投影路径无效或超出范围".into());
    }
    Ok(annotations)
}

fn frames(mark: TextMark, quads: &[[Point; 4]], style: Style) -> Result<Vec<Frame>> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    if quads.len() > MAX_SPANS {
        return Err("一次文字标记最多4096个区域".into());
    }
    if !style.width.is_finite()
        || style.width <= 0.
        || !style.opacity.is_finite()
        || !(0. ..=1.).contains(&style.opacity)
    {
        return Err("文字标记线宽或不透明度无效".into());
    }
    let frames = quads
        .iter()
        .map(Frame::from_quad)
        .collect::<Result<Vec<_>>>()?;
    let mut total_points = 0usize;
    for frame in &frames {
        if mark == TextMark::Wave {
            frame.validate_wave_envelope()?;
        }
        let count = if mark == TextMark::Wave {
            frame.wave_steps() + 1
        } else if mark == TextMark::Highlight && frame.projection.is_some() {
            4
        } else {
            2
        };
        total_points = total_points
            .checked_add(count)
            .filter(|total| *total <= MAX_POINTS)
            .ok_or("文字标记超出点数预算")?;
    }
    Ok(frames)
}

fn highlight(frame: &Frame, style: Style) -> Annotation {
    if frame.projection.is_some() {
        let quad = frame.quad(Interval {
            start: 0.,
            end: frame.width,
        });
        let mut annotation = Annotation::new(
            Tool::Polyline,
            quad[0],
            Style {
                color: style.color,
                // 既有 Polyline 网格保留 0.01px 最小描边，不把投影扩成矩形。
                width: 0.01,
                opacity: style.opacity.min(0.35),
                filled: true,
                polyline_smooth: false,
                dashed: false,
                ..Style::default()
            },
        );
        annotation.points = quad.to_vec();
        return annotation;
    }
    let center = frame.map(frame.width / 2., frame.height / 2.);
    // 既有矩形对象要求至少 2px；极窄字形保持中心，只补足可编辑最小尺寸。
    let (width, height) = (frame.width.max(2.), frame.height.max(2.));
    let mut annotation = Annotation::new(
        Tool::HighlighterRect,
        Point {
            x: center.x - width / 2.,
            y: center.y - height / 2.,
        },
        Style {
            color: style.color,
            width: 1.,
            opacity: style.opacity.min(0.35),
            filled: true,
            radius: 0.,
            highlighter_blend: HighlighterBlend::Alpha,
            ..Style::default()
        },
    );
    annotation.points.push(Point {
        x: center.x + width / 2.,
        y: center.y + height / 2.,
    });
    annotation.rotation = frame.right.y.atan2(frame.right.x).to_degrees();
    annotation
}

fn stroke(mark: TextMark, frame: &Frame, style: Style) -> Annotation {
    let width = style
        .width
        .clamp(0.75, (frame.height * 0.12).clamp(0.75, 4.));
    let y = if mark == TextMark::Strike {
        frame.height * 0.55
    } else {
        frame.height
    };
    let mut annotation = Annotation::new(
        Tool::Pen,
        frame.map(0., y),
        Style {
            color: style.color,
            width,
            opacity: style.opacity,
            pencil_smooth: false,
            dashed: false,
            ..Style::default()
        },
    );
    if mark == TextMark::Wave {
        let steps = frame.wave_steps();
        let cycles = steps / 8;
        let amplitude = frame.wave_amplitude();
        for step in 1..steps {
            let t = step as f32 / steps as f32;
            let phase = t * cycles as f32 * std::f32::consts::TAU;
            annotation
                .points
                .push(frame.map(t * frame.width, y + phase.sin() * amplitude));
        }
    }
    // 显式固定端点，避免累计步进或 sin(2πn) 的浮点残差。
    annotation.points.push(frame.map(frame.width, y));
    annotation
}

#[derive(Clone)]
struct PendingMark {
    kind: TextMark,
    quad: [Point; 4],
    annotation: Annotation,
}
struct OwnedMark {
    id: crate::document::ObjectId,
    mark: PendingMark,
}
struct Underlay {
    objects: Vec<crate::document::Object>,
    crop_mask: Option<crate::crop_mask::Polygon>,
}

/// 只管理本次识别会话亲自提交的对象，历史记录让 undo/redo 后仍可认出原 ID。
/// 用户改变几何或样式后，当前对象不再与记录匹配，不能被选字菜单自动删除。
#[derive(Default)]
pub(crate) struct MarkLedger {
    records: Vec<OwnedMark>,
    /// 本会话提交的非文字标记对象（排除行的局部填充），只用于下层核验。
    owned: Vec<(crate::document::ObjectId, Annotation)>,
    underlay: Option<Underlay>,
    generation: u64,
}

/// 已验输入的纯计划；Scene 成功准备后再提交，避免错误产生半批标记。
pub(crate) struct MarkEdit {
    revision: u64,
    edit_revision: u64,
    generation: u64,
    insertions: Vec<PendingMark>,
    replacements: Vec<(crate::document::ObjectId, PendingMark)>,
    removals: Vec<crate::document::ObjectId>,
    points: usize,
    underlay: Option<Underlay>,
}
impl MarkEdit {
    pub(crate) fn annotations(&self, document: &crate::document::Document) -> Vec<Annotation> {
        let removals: std::collections::HashSet<_> = self.removals.iter().copied().collect();
        let replacements: std::collections::HashMap<_, _> = self
            .replacements
            .iter()
            .map(|(id, mark)| (*id, &mark.annotation))
            .collect();
        document
            .items()
            .iter()
            .filter(|object| !removals.contains(&object.id))
            .map(|object| {
                replacements
                    .get(&object.id)
                    .map_or(&object.annotation, |annotation| *annotation)
                    .clone()
            })
            .chain(self.insertions.iter().map(|mark| mark.annotation.clone()))
            .collect()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.insertions.is_empty() && self.replacements.is_empty() && self.removals.is_empty()
    }
    fn reserve_mark(&mut self, mark: &PendingMark) -> Result<()> {
        self.points = self
            .points
            .checked_add(mark.annotation.points.len())
            .filter(|points| *points <= MAX_POINTS)
            .ok_or("文字标记超出点数预算")?;
        if self.insertions.len() + self.replacements.len() >= MAX_SPANS {
            return Err("一次文字标记最多4096个区域".into());
        }
        Ok(())
    }
    fn insert(&mut self, mark: PendingMark) -> Result<()> {
        self.reserve_mark(&mark)?;
        self.insertions.push(mark);
        Ok(())
    }
    fn replace(&mut self, id: crate::document::ObjectId, mark: PendingMark) -> Result<()> {
        self.reserve_mark(&mark)?;
        self.replacements.push((id, mark));
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Interval {
    start: f32,
    end: f32,
}
struct Row {
    frame: Frame,
    selected: Vec<Interval>,
}
struct SelectionCoverage<'a> {
    rows: Vec<Row>,
    per_row: Vec<Vec<(&'a OwnedMark, Interval)>>,
    coverage: Vec<Vec<Interval>>,
    toggle_off: bool,
}
impl Frame {
    fn interval(&self, other: &Frame) -> Option<Interval> {
        if self.projection.is_some() || other.projection.is_some() {
            let quad = other.quad(Interval {
                start: 0.,
                end: other.width,
            });
            let [a, b, c, d] = quad.map(|point| self.local(point));
            let (a, b, c, d) = (a?, b?, c?, d?);
            // 同一阅读投影带的子区间才可合并；平行边或 AABB 重叠不构成证明。
            let tolerance = 0.02 + f64::from(self.width.max(self.height)) * 0.000001;
            if (a[1]).abs() > tolerance
                || (b[1]).abs() > tolerance
                || (c[1] - f64::from(self.height)).abs() > tolerance
                || (d[1] - f64::from(self.height)).abs() > tolerance
                || (a[0] - d[0]).abs() > tolerance
                || (b[0] - c[0]).abs() > tolerance
                || b[0] <= a[0]
            {
                return None;
            }
            let (start, end) = (a[0], b[0]);
            // 检查内部对应点，拒绝坐标系相同边界但投影参数不同的区域。
            for (u, v) in [(0.25, 0.), (0.5, 0.5), (0.75, 1.)] {
                let expected = self.map((start + (end - start) * u) as f32, self.height * v as f32);
                let actual = other.map(other.width * u as f32, other.height * v as f32);
                let distance = f64::from(expected.distance(actual));
                if !distance.is_finite() || distance > tolerance {
                    return None;
                }
            }
            let interval = Interval {
                start: start as f32,
                end: end as f32,
            };
            return (interval.start.is_finite()
                && interval.end.is_finite()
                && interval.end > interval.start)
                .then_some(interval);
        }
        let dot = |a: Point, b: Point| a.x * b.x + a.y * b.y;
        if dot(self.right, other.right) < 0.99999
            || dot(self.down, other.down) < 0.99999
            || (self.height - other.height).abs() > 0.25
        {
            return None;
        }
        let delta = Point {
            x: other.origin.x - self.origin.x,
            y: other.origin.y - self.origin.y,
        };
        if dot(delta, self.down).abs() > 0.25 {
            return None;
        }
        let start = dot(delta, self.right);
        Some(Interval {
            start,
            end: start + other.width,
        })
    }
    fn quad(&self, interval: Interval) -> [Point; 4] {
        [
            self.map(interval.start, 0.),
            self.map(interval.end, 0.),
            self.map(interval.end, self.height),
            self.map(interval.start, self.height),
        ]
    }
}
fn merged(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut result: Vec<Interval> = Vec::new();
    for interval in intervals {
        if let Some(last) = result
            .last_mut()
            .filter(|last| interval.start <= last.end + 0.02)
        {
            last.end = last.end.max(interval.end);
        } else {
            result.push(interval);
        }
    }
    result
}
fn subtract(interval: Interval, cuts: &[Interval]) -> Vec<Interval> {
    let mut result = Vec::new();
    let mut start = interval.start;
    for cut in cuts {
        if cut.end <= start || cut.start >= interval.end {
            continue;
        }
        if cut.start - start >= 0.25 {
            result.push(Interval {
                start,
                end: cut.start.min(interval.end),
            });
        }
        start = start.max(cut.end);
        if start >= interval.end {
            return result;
        }
    }
    if interval.end - start >= 0.25 {
        result.push(Interval {
            start,
            end: interval.end,
        });
    }
    result
}
fn intersects(a: Interval, b: Interval) -> bool {
    a.start < b.end - 0.02 && a.end > b.start + 0.02
}
fn pending(kind: TextMark, frame: &Frame, interval: Interval, style: Style) -> Result<PendingMark> {
    let quad = frame.quad(interval);
    let annotation = annotations(kind, &[quad], style)?.remove(0);
    Ok(PendingMark {
        kind,
        quad,
        annotation,
    })
}

fn surviving_mark(mark: &PendingMark, frame: &Frame, interval: Interval) -> Result<PendingMark> {
    if mark.kind != TextMark::Wave {
        return pending(mark.kind, frame, interval, mark.annotation.style);
    }
    let quad = frame.quad(interval);
    Frame::from_quad(&quad)?;
    let local_x = |point: Point| -> Result<f64> {
        frame
            .local(point)
            .map(|local| local[0])
            .ok_or_else(|| "文字波浪线原始路径投影无效".into())
    };
    let interpolate = |a: Point, b: Point, t: f64| {
        if t <= 0. {
            a
        } else if t >= 1. {
            b
        } else {
            Point {
                x: (f64::from(a.x) + (f64::from(b.x) - f64::from(a.x)) * t) as f32,
                y: (f64::from(a.y) + (f64::from(b.y) - f64::from(a.y)) * t) as f32,
            }
        }
    };
    let mut points = Vec::new();
    let mut append = |point| {
        if points.last() != Some(&point) {
            points.push(point);
        }
    };
    let start = f64::from(interval.start);
    let end = f64::from(interval.end);
    for segment in mark.annotation.points.windows(2) {
        let (a, b) = (local_x(segment[0])?, local_x(segment[1])?);
        if b < start - 0.001 || a > end + 0.001 {
            continue;
        }
        if b < a - 0.001 {
            return Err("文字波浪线的路径顺序无效".into());
        }
        if (b - a).abs() <= f64::EPSILON {
            append(segment[0]);
            append(segment[1]);
            continue;
        }
        let enter = if a >= start - 0.001 {
            0.
        } else {
            frame.clip_fraction(segment[0], segment[1], a, b, start)?
        };
        let exit = if b <= end + 0.001 {
            1.
        } else {
            frame.clip_fraction(segment[0], segment[1], a, b, end)?
        };
        if enter <= exit {
            append(interpolate(segment[0], segment[1], enter));
            append(interpolate(segment[0], segment[1], exit));
        }
    }
    if points.len() < 2 {
        return Err("文字波浪线剩余范围没有有效路径".into());
    }
    let mut annotation = Annotation::new(Tool::Pen, points[0], mark.annotation.style);
    annotation.points = points;
    Ok(PendingMark {
        kind: mark.kind,
        quad,
        annotation,
    })
}
impl MarkLedger {
    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.owned.clear();
        self.underlay = None;
        self.generation = self.generation.wrapping_add(1);
    }
    /// 仅在用户撤销/重做后核验：识别前的对象必须完整保持，其它对象只能是
    /// 本会话记录过且当前实际几何/样式仍精确相同的标记，不能忽略手工改动。
    pub(crate) fn matches_underlay(&self, document: &crate::document::Document) -> bool {
        let Some(underlay) = &self.underlay else {
            return false;
        };
        if document.crop_mask != underlay.crop_mask {
            return false;
        }
        let mut owned: std::collections::HashMap<_, Vec<&Annotation>> =
            std::collections::HashMap::new();
        for record in &self.records {
            owned
                .entry(record.id)
                .or_default()
                .push(&record.mark.annotation);
        }
        for (id, annotation) in &self.owned {
            owned.entry(*id).or_default().push(annotation);
        }
        document
            .items()
            .iter()
            .filter(|object| {
                !owned.get(&object.id).is_some_and(|annotations| {
                    annotations
                        .iter()
                        .any(|annotation| **annotation == object.annotation)
                })
            })
            .eq(underlay.objects.iter())
    }
    /// 与prepare共用真实当前对象和完整同色覆盖；不生成标注、不提交历史。
    pub(crate) fn selection_fully_marked(
        &self,
        document: &crate::document::Document,
        mark: TextMark,
        quads: &[[Point; 4]],
        style: Style,
    ) -> Result<bool> {
        Ok(self
            .selection_coverage(document, mark, quads, style)?
            .toggle_off)
    }
    fn selection_coverage<'a>(
        &'a self,
        document: &crate::document::Document,
        mark: TextMark,
        quads: &[[Point; 4]],
        style: Style,
    ) -> Result<SelectionCoverage<'a>> {
        let mut rows: Vec<Row> = Vec::new();
        for frame in frames(mark, quads, style)? {
            if let Some((row, interval)) = rows
                .iter_mut()
                .find_map(|row| row.frame.interval(&frame).map(|interval| (row, interval)))
            {
                row.selected.push(interval);
            } else {
                let width = frame.width;
                rows.push(Row {
                    frame,
                    selected: vec![Interval {
                        start: 0.,
                        end: width,
                    }],
                });
            }
        }
        for row in &mut rows {
            row.selected = merged(std::mem::take(&mut row.selected));
        }
        let current: std::collections::HashMap<_, _> = document
            .items()
            .iter()
            .map(|object| (object.id, &object.annotation))
            .collect();
        let mut seen = std::collections::HashSet::new();
        let owned: Vec<_> = self
            .records
            .iter()
            .rev()
            .filter(|record| {
                record.mark.kind == mark
                    && current.get(&record.id).is_some_and(|annotation| {
                        **annotation == record.mark.annotation && seen.insert(record.id)
                    })
            })
            .collect();
        let mut per_row = Vec::with_capacity(rows.len());
        let mut assigned = std::collections::HashSet::new();
        for row in &rows {
            let mut items = Vec::new();
            for record in &owned {
                let frame = Frame::from_quad(&record.mark.quad)?;
                if let Some(interval) = row.frame.interval(&frame).filter(|interval| {
                    row.selected
                        .iter()
                        .any(|selected| intersects(*interval, *selected))
                }) {
                    if assigned.insert(record.id) {
                        items.push((*record, interval));
                    }
                }
            }
            per_row.push(items);
        }
        let coverage: Vec<_> = per_row
            .iter()
            .map(|items| {
                merged(
                    items
                        .iter()
                        .filter_map(|(record, interval)| {
                            (record.mark.annotation.style.color == style.color).then_some(*interval)
                        })
                        .collect(),
                )
            })
            .collect();
        let toggle_off = !rows.is_empty()
            && rows.iter().zip(&coverage).all(|(row, covered)| {
                row.selected
                    .iter()
                    .all(|selected| subtract(*selected, covered).is_empty())
            });
        Ok(SelectionCoverage {
            rows,
            per_row,
            coverage,
            toggle_off,
        })
    }
    pub(crate) fn prepare(
        &self,
        document: &crate::document::Document,
        mark: TextMark,
        quads: &[[Point; 4]],
        style: Style,
    ) -> Result<MarkEdit> {
        let mut edit = MarkEdit {
            revision: document.revision(),
            edit_revision: document.edit_revision(),
            generation: self.generation,
            insertions: Vec::new(),
            replacements: Vec::new(),
            removals: Vec::new(),
            points: 0,
            underlay: self.underlay.is_none().then(|| Underlay {
                objects: document.items().to_vec(),
                crop_mask: document.crop_mask.clone(),
            }),
        };
        let SelectionCoverage {
            rows,
            per_row,
            coverage,
            toggle_off,
        } = self.selection_coverage(document, mark, quads, style)?;
        for ((row, items), covered) in rows.iter().zip(per_row).zip(coverage) {
            for (record, interval) in items {
                let same_color = record.mark.annotation.style.color == style.color;
                if same_color != toggle_off {
                    continue;
                }
                let survivors = subtract(interval, &row.selected);
                if survivors.is_empty() {
                    edit.removals.push(record.id);
                    if edit.removals.len() > MAX_SPANS {
                        return Err("一次文字标记最多4096个区域".into());
                    }
                } else {
                    let old_frame = Frame::from_quad(&record.mark.quad)?;
                    let relative = |survivor: Interval| {
                        let scale =
                            if row.frame.projection.is_some() || old_frame.projection.is_some() {
                                old_frame.width / (interval.end - interval.start)
                            } else {
                                1.
                            };
                        Interval {
                            start: (survivor.start - interval.start) * scale,
                            end: (survivor.end - interval.start) * scale,
                        }
                    };
                    let mut survivors = survivors.into_iter();
                    let first = surviving_mark(
                        &record.mark,
                        &old_frame,
                        relative(survivors.next().unwrap()),
                    )?;
                    if first.annotation != record.mark.annotation {
                        edit.replace(record.id, first)?;
                    }
                    for survivor in survivors {
                        edit.insert(surviving_mark(
                            &record.mark,
                            &old_frame,
                            relative(survivor),
                        )?)?;
                    }
                }
            }
            if !toggle_off {
                for interval in row
                    .selected
                    .iter()
                    .flat_map(|selected| subtract(*selected, &covered))
                {
                    edit.insert(pending(mark, &row.frame, interval, style)?)?;
                }
            }
        }
        let new_records = edit.insertions.len() + edit.replacements.len();
        let new_points: usize = edit
            .insertions
            .iter()
            .chain(edit.replacements.iter().map(|(_, mark)| mark))
            .map(|mark| mark.annotation.points.len())
            .sum();
        let current_points: usize = self
            .records
            .iter()
            .map(|record| record.mark.annotation.points.len())
            .sum();
        if new_records > MAX_SPANS
            || edit.removals.len() > MAX_SPANS
            || self.records.len() + new_records > MAX_LEDGER_RECORDS
            || new_points > MAX_POINTS
            || current_points + new_points > MAX_LEDGER_POINTS
        {
            return Err("文字标记历史或点数超出预算，请重新开始识别".into());
        }
        Ok(edit)
    }
    /// 过期计划不写文档、不消费 redo，也不取得其它对象的归属。
    pub(crate) fn commit(
        &mut self,
        document: &mut crate::document::Document,
        edit: MarkEdit,
    ) -> bool {
        if edit.revision != document.revision()
            || edit.edit_revision != document.edit_revision()
            || edit.generation != self.generation
            || edit.is_empty()
        {
            return false;
        }
        let ids = document.insert_replace_remove_many(
            edit.insertions
                .iter()
                .map(|mark| mark.annotation.clone())
                .collect(),
            &edit
                .replacements
                .iter()
                .map(|(id, mark)| (*id, mark.annotation.clone()))
                .collect::<Vec<_>>(),
            &edit.removals,
        );
        if self.underlay.is_none() {
            self.underlay = edit.underlay;
        }
        self.records.extend(
            edit.replacements
                .into_iter()
                .map(|(id, mark)| OwnedMark { id, mark }),
        );
        self.records.extend(
            ids.into_iter()
                .zip(edit.insertions)
                .map(|(id, mark)| OwnedMark { id, mark }),
        );
        self.generation = self.generation.wrapping_add(1);
        true
    }
    /// 不参与选字覆盖的本会话对象；作为一条文档命令提交，撤销/重做后仍算本会话所有。
    pub(crate) fn commit_owned(
        &mut self,
        document: &mut crate::document::Document,
        insertions: Vec<Annotation>,
        replacements: &[(crate::document::ObjectId, Annotation)],
        removals: &[crate::document::ObjectId],
    ) -> Vec<crate::document::ObjectId> {
        if self.underlay.is_none() {
            self.underlay = Some(Underlay {
                objects: document.items().to_vec(),
                crop_mask: document.crop_mask.clone(),
            });
        }
        let ids = document.insert_replace_remove_many(insertions.clone(), replacements, removals);
        self.owned.extend(replacements.iter().cloned());
        self.owned.extend(ids.iter().copied().zip(insertions));
        self.generation = self.generation.wrapping_add(1);
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{annotation::Bounds, transform};

    fn quad(x: f32, y: f32, width: f32, height: f32) -> [Point; 4] {
        transform::corners(Bounds {
            x,
            y,
            width,
            height,
        })
    }
    fn close(actual: Point, expected: Point) {
        assert!(
            actual.distance(expected) < 0.002,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn rotated_highlight_retains_full_canvas_location_and_readable_alpha() {
        let original = quad(-120., 210., 90., 18.);
        let rotation = transform::Rotation::new(Point { x: -75., y: 219. }, 37.);
        let input = original.map(|point| rotation.map(point));
        let items = annotations(TextMark::Highlight, &[input], Style::default()).unwrap();
        let annotation = &items[0];
        assert!(annotation.valid());
        assert_eq!(annotation.tool, Tool::HighlighterRect);
        assert_eq!(annotation.style.highlighter_blend, HighlighterBlend::Alpha);
        assert!(annotation.style.opacity > 0. && annotation.style.opacity <= 0.35);
        for (actual, expected) in transform::corners(annotation.local_bounds())
            .map(|point| annotation.to_world(point))
            .into_iter()
            .zip(input)
        {
            close(actual, expected);
        }
    }

    #[test]
    fn separate_lines_are_independent_objects_with_one_undo_batch() {
        let mut document = crate::document::Document::default();
        let items = annotations(
            TextMark::Underline,
            &[quad(80., 30., 75., 16.), quad(90., 65., 35., 16.)],
            Style::default(),
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        close(items[0].points[0], Point { x: 80., y: 46. });
        close(items[0].points[1], Point { x: 155., y: 46. });
        close(items[1].points[0], Point { x: 90., y: 81. });
        assert!(items.iter().all(|item| item.points.len() == 2));
        document.insert_many(items);
        document.undo();
        assert!(document.items().is_empty());
        assert!(!document.can_undo());
        document.redo();
        assert_eq!(document.items().len(), 2);
    }

    #[test]
    fn rotated_wave_has_exact_ends_and_bounded_local_displacement() {
        let input = [
            Point { x: 120., y: 50. },
            Point { x: 120., y: 190. },
            Point { x: 100., y: 190. },
            Point { x: 100., y: 50. },
        ];
        let annotation = annotations(TextMark::Wave, &[input], Style::default())
            .unwrap()
            .remove(0);
        close(annotation.points[0], Point { x: 100., y: 50. });
        close(
            *annotation.points.last().unwrap(),
            Point { x: 100., y: 190. },
        );
        assert!(annotation.points.iter().all(|point| {
            (98.7..=101.3).contains(&point.x) && (50. ..=190.).contains(&point.y)
        }));
        assert!(annotation.points.iter().any(|point| point.x > 101.));
        assert!(annotation.points.iter().any(|point| point.x < 99.));
        assert!(!annotation.style.pencil_smooth);
        assert!(!annotation.style.dashed);
        assert!(annotation.points.len() < 200);
        assert_eq!(annotation.rotation, 0.);
    }

    #[test]
    fn strike_follows_rotated_text_center_instead_of_world_horizontal() {
        let input = quad(150., 90., 100., 20.)
            .map(|point| transform::Rotation::new(Point { x: 200., y: 100. }, 180.).map(point));
        let annotation = annotations(TextMark::Strike, &[input], Style::default())
            .unwrap()
            .remove(0);
        close(annotation.points[0], Point { x: 250., y: 99. });
        close(annotation.points[1], Point { x: 150., y: 99. });
        assert!(annotation.valid());
    }

    #[test]
    fn invalid_batch_is_rejected_without_partial_annotations() {
        let valid = quad(10., 10., 70., 20.);
        let mut nonfinite = valid;
        nonfinite[2].x = f32::NAN;
        let mut concave = valid;
        concave[2] = Point { x: 15., y: 15. };
        let mut reversed = valid;
        reversed.swap(1, 3);
        for invalid in [nonfinite, concave, reversed, quad(10., 10., 0., 20.)] {
            assert!(annotations(TextMark::Highlight, &[valid, invalid], Style::default()).is_err());
        }
    }

    #[test]
    fn empty_input_is_noop_but_oversized_input_and_wave_budget_are_rejected() {
        assert!(annotations(TextMark::Wave, &[], Style::default())
            .unwrap()
            .is_empty());
        let too_many = vec![quad(1., 1., 20., 12.); MAX_SPANS + 1];
        assert!(annotations(TextMark::Highlight, &too_many, Style::default()).is_err());
        assert!(annotations(
            TextMark::Wave,
            &[quad(1., 1., 65_535., 1.)],
            Style::default()
        )
        .is_err());
        let two_large = [quad(1., 1., 25_000., 2.); 2];
        assert!(annotations(TextMark::Wave, &two_large, Style::default()).is_err());
    }

    #[test]
    fn tiny_highlight_keeps_center_and_bad_style_never_enters_document() {
        let items = annotations(
            TextMark::Highlight,
            &[quad(4., 8., 0.5, 1.)],
            Style::default(),
        )
        .unwrap();
        assert!(items[0].valid());
        close(
            transform::center(items[0].local_bounds()),
            Point { x: 4.25, y: 8.5 },
        );
        for style in [
            Style {
                width: f32::NAN,
                ..Style::default()
            },
            Style {
                width: -1.,
                ..Style::default()
            },
            Style {
                opacity: 1.1,
                ..Style::default()
            },
        ] {
            assert!(annotations(TextMark::Underline, &[quad(4., 8., 12., 16.)], style).is_err());
        }
    }

    #[test]
    fn highlight_uses_existing_scene_and_crop_keeps_glyph_contrast() {
        use crate::{
            geometry::Rect,
            scene::{Layer, PixelPlane, Scene, SourcePixels},
        };
        let canvas = Rect::full(64, 40);
        let mut bytes = [255, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
        let glyph_offset = ((20 * canvas.width + 25) * 4) as usize;
        bytes[glyph_offset..glyph_offset + 3].fill(0);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
            overlay: None,
        };
        let annotations = annotations(
            TextMark::Highlight,
            &[quad(8., 12., 40., 16.)],
            Style {
                color: 0xffff00,
                ..Style::default()
            },
        )
        .unwrap();
        let scene = Scene::prepare(&annotations, &source);
        assert!(scene.errors.is_empty(), "{:?}", scene.errors);
        assert!(matches!(scene.layers[0], Layer::Vector { .. }));
        let mut full = source.read(canvas);
        scene.apply(&mut full, canvas);
        let white_offset = ((20 * canvas.width + 30) * 4) as usize;
        for channel in 0..3 {
            assert!(full.bgra[white_offset + channel] - full.bgra[glyph_offset + channel] >= 160);
        }
        assert_eq!(&full.bgra[..4], &[255, 255, 255, 255]);
        let crop = Rect {
            x: 15,
            y: 15,
            width: 32,
            height: 20,
        };
        let mut cropped = source.read(crop);
        scene.apply(&mut cropped, crop);
        for y in 0..crop.height {
            let start = (((crop.y + y) * canvas.width + crop.x) * 4) as usize;
            let destination = (y * crop.width * 4) as usize;
            assert_eq!(
                &cropped.bgra[destination..destination + (crop.width * 4) as usize],
                &full.bgra[start..start + (crop.width * 4) as usize]
            );
        }
    }

    fn apply(
        ledger: &mut MarkLedger,
        document: &mut crate::document::Document,
        kind: TextMark,
        quads: &[[Point; 4]],
        style: Style,
    ) {
        let edit = ledger.prepare(document, kind, quads, style).unwrap();
        let expected = edit.annotations(document);
        assert!(ledger.commit(document, edit));
        assert_eq!(
            document
                .items()
                .iter()
                .map(|object| object.annotation.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn selection_activity_matches_real_toggle_for_mixed_partial_canceled_and_history_states() {
        for kind in [
            TextMark::Highlight,
            TextMark::Underline,
            TextMark::Wave,
            TextMark::Strike,
        ] {
            let red = Style {
                color: 0xff1122,
                ..Style::default()
            };
            let blue = Style {
                color: 0x1155ff,
                ..red
            };
            let whole = [quad(20., 30., 40., 18.)];
            let first = [quad(20., 30., 20., 18.)];
            let last = [quad(40., 30., 20., 18.)];
            let other = if kind == TextMark::Underline {
                TextMark::Strike
            } else {
                TextMark::Underline
            };
            let mut document = crate::document::Document::default();
            let manual = annotations(kind, &whole, red).unwrap().remove(0);
            let manual_id = document.insert(manual.clone());
            let mut ledger = MarkLedger::default();
            // 同形同色手工对象不取得归属；没有选择不能亮。
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, kind, &[], red)
                .unwrap());
            apply(&mut ledger, &mut document, kind, &first, red);
            assert!(ledger
                .selection_fully_marked(&document, kind, &first, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, other, &first, red)
                .unwrap());
            // 取消真实补全计划不能填补覆盖、改变历史或消费既有Redo。
            document.undo();
            let saved = document.items().to_vec();
            let saved_revision = (
                document.revision(),
                document.edit_revision(),
                ledger.generation,
            );
            assert!(document.can_redo());
            let canceled = ledger.prepare(&document, kind, &whole, red).unwrap();
            drop(canceled);
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert_eq!(document.items(), saved);
            assert_eq!(
                (
                    document.revision(),
                    document.edit_revision(),
                    ledger.generation
                ),
                saved_revision
            );
            assert!(document.can_redo());
            document.redo();
            assert!(ledger
                .selection_fully_marked(&document, kind, &first, red)
                .unwrap());
            // 不同种类和不同颜色的部分覆盖，均不能冒充整个选择的同类同色。
            apply(&mut ledger, &mut document, other, &last, red);
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, other, &whole, red)
                .unwrap());
            apply(&mut ledger, &mut document, kind, &last, blue);
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, blue)
                .unwrap());
            assert!(ledger
                .selection_fully_marked(&document, kind, &last, blue)
                .unwrap());
            let mixed = document.items().to_vec();
            apply(&mut ledger, &mut document, kind, &whole, red);
            assert!(ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, blue)
                .unwrap());
            document.undo();
            assert_eq!(document.items(), mixed);
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            document.redo();
            assert!(ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            // 同类同色再次执行真实计划即取消；Undo/Redo的活动状态只读实际对象。
            apply(&mut ledger, &mut document, kind, &whole, red);
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(document
                .items()
                .iter()
                .any(|object| object.id == manual_id && object.annotation == manual));
            document.undo();
            assert!(ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            document.redo();
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            // 非法几何/样式不亮；清会话后不得靠历史同形对象复活归属。
            assert!(ledger
                .selection_fully_marked(&document, kind, &[quad(20., 30., 0., 18.)], red)
                .is_err());
            assert!(ledger
                .selection_fully_marked(
                    &document,
                    kind,
                    &whole,
                    Style {
                        width: f32::NAN,
                        ..red
                    }
                )
                .is_err());
            document.undo();
            let stale = ledger.prepare(&document, kind, &whole, red).unwrap();
            let before_clear = document.items().to_vec();
            ledger.clear();
            assert!(!ledger
                .selection_fully_marked(&document, kind, &whole, red)
                .unwrap());
            assert!(!ledger.commit(&mut document, stale));
            assert_eq!(document.items(), before_clear);
        }
    }

    #[test]
    fn toggling_owned_mark_never_deletes_identical_manual_annotation() {
        let quads = [quad(20., 30., 90., 18.)];
        let style = Style::default();
        let manual = annotations(TextMark::Underline, &quads, style)
            .unwrap()
            .remove(0);
        let mut document = crate::document::Document::default();
        let manual_id = document.insert(manual.clone());
        let mut ledger = MarkLedger::default();
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            style,
        );
        assert_eq!(document.items().len(), 2);
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            style,
        );
        assert_eq!(document.items().len(), 1);
        assert_eq!(document.items()[0].id, manual_id);
        assert_eq!(document.items()[0].annotation, manual);
    }

    #[test]
    fn partial_coverage_only_adds_gaps_and_recolor_preserves_one_undo_batch() {
        let mut document = crate::document::Document::default();
        let mut ledger = MarkLedger::default();
        let red = Style {
            color: 0xff1122,
            ..Style::default()
        };
        let blue = Style {
            color: 0x1155ff,
            ..Style::default()
        };
        let first = [quad(50., 60., 20., 18.)];
        let second = [quad(60., 60., 20., 18.)];
        apply(&mut ledger, &mut document, TextMark::Underline, &first, red);
        let original_id = document.items()[0].id;
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &second,
            red,
        );
        assert_eq!(document.items().len(), 2);
        assert_eq!(document.items()[0].id, original_id);
        assert_eq!(document.items()[0].annotation.points[1].x, 70.);
        assert_eq!(document.items()[1].annotation.points[0].x, 70.);
        assert_eq!(document.items()[1].annotation.points[1].x, 80.);
        let before = document.items().to_vec();
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &second,
            blue,
        );
        assert_eq!(document.items().len(), 2);
        assert_eq!(document.items()[0].id, original_id);
        assert_eq!(document.items()[0].annotation.points[1].x, 60.);
        assert_eq!(document.items()[1].annotation.style.color, blue.color);
        assert_eq!(document.items()[1].annotation.points[0].x, 60.);
        assert_eq!(document.items()[1].annotation.points[1].x, 80.);
        document.undo();
        assert_eq!(document.items(), before);
        document.redo();
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &second,
            blue,
        );
        assert_eq!(document.items().len(), 1);
        assert_eq!(document.items()[0].annotation.style.color, red.color);
        assert_eq!(document.items()[0].annotation.points[1].x, 60.);
    }

    #[test]
    fn stale_plan_does_not_consume_redo_and_old_ownership_survives_history() {
        let mut document = crate::document::Document::default();
        let mut ledger = MarkLedger::default();
        let quads = [quad(30., 20., 90., 18.)];
        apply(
            &mut ledger,
            &mut document,
            TextMark::Highlight,
            &quads,
            Style::default(),
        );
        let stale = ledger
            .prepare(
                &document,
                TextMark::Highlight,
                &quads,
                Style {
                    color: 0x33bb99,
                    ..Style::default()
                },
            )
            .unwrap();
        document.undo();
        let revision = document.revision();
        assert!(document.can_redo());
        assert!(!ledger.commit(&mut document, stale));
        assert_eq!(document.revision(), revision);
        assert!(document.items().is_empty());
        assert!(document.can_redo());
        document.redo();
        apply(
            &mut ledger,
            &mut document,
            TextMark::Highlight,
            &quads,
            Style::default(),
        );
        assert!(document.items().is_empty());
    }

    #[test]
    fn mark_kinds_coexist_and_manual_edit_or_session_clear_revokes_management() {
        let mut document = crate::document::Document::default();
        let mut ledger = MarkLedger::default();
        let quads = [quad(30., 20., 90., 18.)];
        let style = Style::default();
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            style,
        );
        let edited_id = document.items()[0].id;
        apply(&mut ledger, &mut document, TextMark::Wave, &quads, style);
        let wave_id = document.items()[1].id;
        let mut manual_edit = document.get(edited_id).unwrap().clone();
        manual_edit.style.color = 0x221144;
        document.replace(edited_id, manual_edit.clone());
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            style,
        );
        assert_eq!(document.items().len(), 3);
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            style,
        );
        assert_eq!(document.items().len(), 2);
        assert_eq!(document.get(edited_id), Some(&manual_edit));
        assert!(document.get(wave_id).is_some());
        let edit = ledger
            .prepare(&document, TextMark::Wave, &quads, style)
            .unwrap();
        ledger.clear();
        assert!(!ledger.commit(&mut document, edit));
        assert!(document.get(wave_id).is_some());
    }

    #[test]
    fn underlay_survives_owned_history_but_rejects_original_or_manual_changes() {
        let mut document = crate::document::Document::default();
        let mut base = Annotation::new(Tool::Rectangle, Point { x: 2., y: 3. }, Style::default());
        base.points.push(Point { x: 12., y: 15. });
        document.insert(base);
        let mut ledger = MarkLedger::default();
        assert!(!ledger.matches_underlay(&document));
        let quads = [quad(30., 20., 90., 18.)];
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            Style::default(),
        );
        let generated = document.items()[1].annotation.clone();
        assert!(ledger.matches_underlay(&document));
        for _ in 0..2 {
            document.undo();
            assert!(ledger.matches_underlay(&document));
            document.redo();
            assert!(ledger.matches_underlay(&document));
        }
        document.undo();
        assert!(ledger.matches_underlay(&document));
        document.undo();
        assert!(!ledger.matches_underlay(&document));
        document.redo();
        assert!(ledger.matches_underlay(&document));
        document.redo();
        assert!(ledger.matches_underlay(&document));
        document.insert(generated);
        assert!(!ledger.matches_underlay(&document));
        document.undo();
        assert!(ledger.matches_underlay(&document));
        let owned_id = document.items()[1].id;
        let mut modified = document.get(owned_id).unwrap().clone();
        modified.style.width += 0.5;
        document.replace(owned_id, modified);
        assert!(!ledger.matches_underlay(&document));
    }

    #[test]
    fn identical_crop_bounds_do_not_hide_a_different_lasso_underlay() {
        use crate::{crop_mask::Polygon, geometry::Rect};
        let mut document = crate::document::Document::default();
        let host = Rect::full(100, 100);
        let square = Polygon::new(&quad(10., 10., 80., 80.)).unwrap();
        let triangle = Polygon::new(&[
            Point { x: 10., y: 10. },
            Point { x: 90., y: 10. },
            Point { x: 50., y: 90. },
        ])
        .unwrap();
        assert_eq!(square.output_rect(host), triangle.output_rect(host));
        document.set_crop_mask(Some(square.clone()));
        let mut ledger = MarkLedger::default();
        let quads = [quad(30., 30., 40., 18.)];
        apply(
            &mut ledger,
            &mut document,
            TextMark::Underline,
            &quads,
            Style::default(),
        );
        assert!(ledger.matches_underlay(&document));
        document.set_crop_mask(Some(triangle));
        assert!(!ledger.matches_underlay(&document));
        document.undo();
        assert!(ledger.matches_underlay(&document));
        document.redo();
        assert!(!ledger.matches_underlay(&document));
        document.undo();
        document.undo();
        assert!(ledger.matches_underlay(&document));
        document.undo();
        assert!(document.crop_mask.is_none());
        assert!(!ledger.matches_underlay(&document));
        document.redo();
        assert_eq!(document.crop_mask, Some(square));
        assert!(ledger.matches_underlay(&document));
    }

    #[test]
    fn partial_wave_cancel_and_recolor_preserve_unselected_world_path() {
        let mut document = crate::document::Document::default();
        let mut ledger = MarkLedger::default();
        let red = Style {
            color: 0xff1122,
            ..Style::default()
        };
        let blue = Style {
            color: 0x1155ff,
            ..Style::default()
        };
        let whole = [quad(30., 50., 180., 20.)];
        let middle = [quad(85., 50., 46., 20.)];
        apply(&mut ledger, &mut document, TextMark::Wave, &whole, red);
        let original = document.items().to_vec();
        let original_points = &original[0].annotation.points;
        apply(&mut ledger, &mut document, TextMark::Wave, &middle, red);
        assert_eq!(document.items().len(), 2);
        let fragments: Vec<_> = document
            .items()
            .iter()
            .map(|object| object.annotation.clone())
            .collect();
        let kept: Vec<_> = fragments
            .iter()
            .flat_map(|annotation| &annotation.points)
            .copied()
            .collect();
        for point in original_points {
            if point.x < 85. || point.x > 131. {
                assert!(kept.contains(point), "幸存的原始世界坐标丢失: {point:?}");
            }
        }
        for annotation in &fragments {
            assert!(annotation
                .points
                .windows(2)
                .all(|points| points[0] != points[1]));
            for point in &annotation.points {
                assert!(original_points.contains(point) || point.x == 85. || point.x == 131.);
            }
        }
        for boundary in [85., 131.] {
            let original_segment = original_points
                .windows(2)
                .find(|points| points[0].x <= boundary && points[1].x >= boundary)
                .unwrap();
            let t = (boundary - original_segment[0].x)
                / (original_segment[1].x - original_segment[0].x);
            let expected_y =
                original_segment[0].y + (original_segment[1].y - original_segment[0].y) * t;
            let point = kept.iter().find(|point| point.x == boundary).unwrap();
            assert!((point.y - expected_y).abs() < 0.0001);
        }
        document.undo();
        assert_eq!(document.items(), original);
        apply(&mut ledger, &mut document, TextMark::Wave, &middle, blue);
        let remaining_red: Vec<_> = document
            .items()
            .iter()
            .filter(|object| object.annotation.style.color == red.color)
            .map(|object| object.annotation.clone())
            .collect();
        assert_eq!(remaining_red, fragments);
        assert_eq!(document.items().len(), 3);
        document.undo();
        assert_eq!(document.items(), original);
        assert!(ledger.matches_underlay(&document));
    }
}

#[cfg(test)]
#[path = "ocr_markup_projected_tests.rs"]
mod projected_tests;
