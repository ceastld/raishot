//! 箭头的轴线、裁切箭杆和闭合填充统一生成，不依赖 UI 或位图。
use crate::{
    annotation::{Point, Style},
    vector_path::{Span, VectorPath},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrowHead {
    #[default]
    Open,
    Triangle,
    Dart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowPreset {
    Line,
    Single,
    Double,
    FilledSingle,
    FilledDouble,
    Dart,
}
impl ArrowPreset {
    pub const ALL: [Self; 6] = [
        Self::Line,
        Self::Single,
        Self::Double,
        Self::FilledSingle,
        Self::FilledDouble,
        Self::Dart,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Single => "singlearrow",
            Self::Double => "doublearrow",
            Self::FilledSingle => "filledsinglearrow",
            Self::FilledDouble => "filleddoublearrow",
            Self::Dart => "dartarrow",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Line => tr!("直线", "Line"),
            Self::Single => tr!("单箭头", "Arrow"),
            Self::Double => tr!("双箭头", "Double arrow"),
            Self::FilledSingle => tr!("实心箭头", "Solid arrow"),
            Self::FilledDouble => tr!("实心双箭头", "Solid double arrow"),
            Self::Dart => tr!("尖尾箭头", "Dart arrow"),
        }
    }
    pub fn of(s: Style) -> Self {
        if !s.arrow_start && !s.arrow_end {
            Self::Line
        } else {
            match s.arrow_head {
                ArrowHead::Dart => Self::Dart,
                ArrowHead::Triangle => {
                    if s.arrow_start {
                        Self::FilledDouble
                    } else {
                        Self::FilledSingle
                    }
                }
                ArrowHead::Open => {
                    if s.arrow_start {
                        Self::Double
                    } else {
                        Self::Single
                    }
                }
            }
        }
    }
    pub fn apply(self, s: &mut Style) {
        s.arrow_start = matches!(self, Self::Double | Self::FilledDouble);
        s.arrow_end = self != Self::Line;
        s.arrow_head = match self {
            Self::FilledSingle | Self::FilledDouble => ArrowHead::Triangle,
            Self::Dart => ArrowHead::Dart,
            _ => ArrowHead::Open,
        };
        // 尖尾轮廓本身是完整的实心图形，不提供不可见的虚线开关。
        if self == Self::Dart {
            s.dashed = false;
        }
    }
}
/// 二级栏“端点”分段：无、单向、双向；与“头型”分段组合出全部预设。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowEnds {
    None,
    Single,
    Both,
}
impl ArrowEnds {
    pub fn of(s: Style) -> Self {
        match (s.arrow_start, s.arrow_end) {
            (false, false) => Self::None,
            (true, true) => Self::Both,
            _ => Self::Single,
        }
    }
    /// 尖尾只能单向：切到双向时头型改回开口。
    pub fn apply(self, s: &mut Style) {
        if self == Self::Both && s.arrow_head == ArrowHead::Dart {
            s.arrow_head = ArrowHead::Open;
        }
        s.arrow_end = self != Self::None;
        s.arrow_start = self == Self::Both;
        if is_dart(*s) {
            s.dashed = false;
        }
    }
}
impl ArrowHead {
    /// 换头型；尖尾时双向改为单向并取消虚线。
    pub fn apply(self, s: &mut Style) {
        s.arrow_head = self;
        if self == Self::Dart && (s.arrow_start || s.arrow_end) {
            s.arrow_start = false;
            s.arrow_end = true;
            s.dashed = false;
        }
    }
}
/// 当前是带头的尖尾箭头（不支持虚线与双向）。
pub fn is_dart(s: Style) -> bool {
    s.arrow_head == ArrowHead::Dart && (s.arrow_start || s.arrow_end)
}
fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}
fn offset(p: Point, v: Point, n: f32) -> Point {
    Point {
        x: p.x + v.x * n,
        y: p.y + v.y * n,
    }
}
fn unit(a: Point, b: Point) -> Point {
    let n = a.distance(b);
    if n < 1e-6 {
        Point { x: 1., y: 0. }
    } else {
        Point {
            x: (b.x - a.x) / n,
            y: (b.y - a.y) / n,
        }
    }
}
#[derive(Clone, Copy)]
struct Axis {
    a: Point,
    c: Point,
    b: Point,
}
impl Axis {
    fn at(self, t: f32) -> Point {
        lerp(lerp(self.a, self.c, t), lerp(self.c, self.b, t), t)
    }
    fn tangent(self, t: f32) -> Point {
        unit(lerp(self.a, self.c, t), lerp(self.c, self.b, t))
    }
    fn length(self) -> f32 {
        (0..64)
            .map(|i| {
                self.at(i as f32 / 64.)
                    .distance(self.at((i + 1) as f32 / 64.))
            })
            .sum()
    }
    fn span(self, t0: f32, t1: f32) -> Span {
        let t1 = t1.max(t0);
        let a = self.at(t0);
        let b = self.at(t1);
        let derivative = Point {
            x: 2. * ((1. - t0) * (self.c.x - self.a.x) + t0 * (self.b.x - self.c.x)),
            y: 2. * ((1. - t0) * (self.c.y - self.a.y) + t0 * (self.b.y - self.c.y)),
        };
        let c = offset(a, derivative, (t1 - t0) / 2.);
        if crate::annotation::distance_to_segment(c, a, b) < 0.0001 {
            Span::Line { start: a, end: b }
        } else {
            Span::Cubic {
                start: a,
                control1: lerp(a, c, 2. / 3.),
                control2: lerp(b, c, 2. / 3.),
                end: b,
            }
        }
    }
    fn anchor(self, tip: bool, r: f32) -> Anchor {
        let apex = if tip { self.b } else { self.a };
        let r = r.max(0.001);
        let t = |i: usize| {
            if tip {
                1. - i as f32 / 64.
            } else {
                i as f32 / 64.
            }
        };
        let mut inside = t(0);
        for i in 1..=64 {
            let outside = t(i);
            if self.at(outside).distance(apex) >= r {
                let mut outside = outside;
                for _ in 0..18 {
                    let m = (outside + inside) / 2.;
                    if self.at(m).distance(apex) >= r {
                        outside = m;
                    } else {
                        inside = m;
                    }
                }
                let p = self.at((inside + outside) / 2.);
                return Anchor {
                    t: (inside + outside) / 2.,
                    p,
                    forward: unit(p, apex),
                };
            }
            inside = outside;
        }
        // 极短或回头曲线没有圆交点：保留真实轴线端点，方向取端点切线。
        let t = if tip { 1. } else { 0. };
        let tangent = self.tangent(t);
        Anchor {
            t,
            p: apex,
            forward: if tip {
                tangent
            } else {
                Point {
                    x: -tangent.x,
                    y: -tangent.y,
                }
            },
        }
    }
}
struct Anchor {
    t: f32,
    p: Point,
    forward: Point,
}
fn stop_radius(depth: f32, half: f32, width: f32) -> f32 {
    let cover = width * 0.55;
    let desired = (width * 0.55).max(half * 0.35);
    let contained = (depth - cover * depth.hypot(half) / half.max(0.001)).max(0.);
    let bury = desired.min(contained);
    (depth.max(1.25 * width) - bury).max(depth * 0.12)
}
pub fn path(a: Point, b: Point, mid: Point, s: Style) -> VectorPath {
    if a.distance(b) < 1.
        || ![a.x, a.y, b.x, b.y, mid.x, mid.y, s.width]
            .iter()
            .all(|v| v.is_finite())
    {
        return VectorPath::default();
    }
    let axis = Axis {
        a,
        b,
        c: Point {
            x: 2. * mid.x - (a.x + b.x) / 2.,
            y: 2. * mid.y - (a.y + b.y) / 2.,
        },
    };
    let length = axis.length();
    if s.arrow_head == ArrowHead::Dart && s.arrow_end {
        return dart(axis, length, s.width);
    }
    let width = s.width.max(0.5);
    let (mut depth, mut half) = if s.arrow_head == ArrowHead::Triangle {
        (3.2 * width, 1.65 * width)
    } else {
        let l = 6. + 4.2 * width;
        let angle = 19_f32.to_radians();
        (l * angle.cos(), l * angle.sin())
    };
    let factor = (length * 0.45 / depth).min(1.);
    depth *= factor;
    half *= factor;
    let radius = if s.arrow_head == ArrowHead::Triangle {
        stop_radius(depth, half, width)
    } else {
        (width * 0.55).max(1.)
    };
    let radius = radius.min(length * 0.45).min(a.distance(b) * 0.45);
    let start = axis.anchor(false, radius);
    let end = axis.anchor(true, radius);
    let t0 = if s.arrow_start { start.t } else { 0. };
    let t1 = if s.arrow_end { end.t } else { 1. };
    let mut path = VectorPath {
        spans: vec![axis.span(t0, t1.max(t0))],
        ..VectorPath::default()
    };
    for (enabled, apex, anchor) in [(s.arrow_end, b, end), (s.arrow_start, a, start)] {
        if !enabled {
            continue;
        }
        let f = anchor.forward;
        let n = Point { x: -f.y, y: f.x };
        let center = offset(apex, f, -depth);
        let left = offset(center, n, half);
        let right = offset(center, n, -half);
        if s.arrow_head == ArrowHead::Triangle {
            path.fills.push(vec![left, apex, right]);
        } else {
            path.spans.push(Span::Line {
                start: left,
                end: apex,
            });
            path.spans.push(Span::Line {
                start: apex,
                end: right,
            });
        }
    }
    path
}
fn dart(axis: Axis, length: f32, width: f32) -> VectorPath {
    let mut neck = (1.04 * width).max(0.5);
    let mut depth = 4. * neck;
    let mut indent = neck;
    let mut barb = 3. * neck;
    let scale = (length * 0.92 / (depth + indent)).min(1.);
    neck *= scale;
    depth *= scale;
    indent *= scale;
    barb *= scale;
    let radius = stop_radius(depth, barb, width.max(0.5)).min(axis.a.distance(axis.b) * 0.9);
    let anchor = axis.anchor(true, radius);
    let f = anchor.forward;
    let n = Point { x: -f.y, y: f.x };
    let barb_mid = offset(axis.b, f, -(depth + indent));
    let mut left = Vec::new();
    let mut right = Vec::new();
    // 轮廓按曲率细分，固定像素容差；长度改变不会暴露粗糙的 48 段侧边。
    let center = VectorPath {
        spans: vec![axis.span(0., anchor.t)],
        ..VectorPath::default()
    };
    let segments = center.flattened();
    let total: f32 = segments.iter().map(|(a, b)| a.distance(*b)).sum();
    let mut distance = 0.;
    left.push(axis.a);
    right.push(axis.a);
    for (i, (a, b)) in segments.iter().enumerate() {
        distance += a.distance(*b);
        let p = if i + 1 == segments.len() {
            anchor.p
        } else {
            *b
        };
        let tangent = if i + 1 == segments.len() {
            f
        } else {
            unit(*a, *b)
        };
        let normal = Point {
            x: -tangent.y,
            y: tangent.x,
        };
        let half = neck * distance / total.max(0.001);
        left.push(offset(p, normal, half));
        right.push(offset(p, normal, -half));
    }
    left.push(offset(barb_mid, n, barb));
    left.push(axis.b);
    left.push(offset(barb_mid, n, -barb));
    left.extend(right.into_iter().rev());
    VectorPath {
        fills: vec![left],
        ..VectorPath::default()
    }
}

#[cfg(test)]
mod ends_tests {
    use super::*;

    #[test]
    fn arrow_ends_and_heads_cover_presets_and_keep_dart_single_solid() {
        for preset in ArrowPreset::ALL {
            let mut expected = Style::default();
            preset.apply(&mut expected);
            let mut style = Style {
                dashed: preset != ArrowPreset::Dart && expected.dashed,
                ..Style::default()
            };
            ArrowEnds::of(expected).apply(&mut style);
            if ArrowEnds::of(expected) != ArrowEnds::None {
                expected.arrow_head.apply(&mut style);
            }
            assert_eq!(ArrowPreset::of(style), preset);
        }
        let mut style = Style {
            dashed: true,
            ..Style::default()
        };
        ArrowEnds::Both.apply(&mut style);
        ArrowHead::Dart.apply(&mut style);
        assert_eq!(ArrowEnds::of(style), ArrowEnds::Single);
        assert!(is_dart(style) && !style.dashed);
        // 尖尾时切到双向：头型改回开口。
        ArrowEnds::Both.apply(&mut style);
        assert_eq!(
            (style.arrow_head, ArrowEnds::of(style)),
            (ArrowHead::Open, ArrowEnds::Both)
        );
        // 无端点的直线不算尖尾，仍可虚线。
        ArrowEnds::None.apply(&mut style);
        style.arrow_head = ArrowHead::Dart;
        assert!(!is_dart(style));
    }
}
