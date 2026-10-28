//! 共用矢量路径：保角铅笔、曲线箭头和可编辑折线的预览、导出、命中几何。
use crate::annotation::{distance_to_segment, Point};
use std::fmt::Write;
const FLATNESS: f32 = 0.03;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Span {
    Line {
        start: Point,
        end: Point,
    },
    Cubic {
        start: Point,
        control1: Point,
        control2: Point,
        end: Point,
    },
}
impl Span {
    pub fn start(self) -> Point {
        match self {
            Self::Line { start, .. } | Self::Cubic { start, .. } => start,
        }
    }
    pub fn at(self, t: f32) -> Point {
        match self {
            Self::Line { start, end } => lerp(start, end, t),
            Self::Cubic {
                start,
                control1,
                control2,
                end,
            } => {
                let ab = lerp(start, control1, t);
                let bc = lerp(control1, control2, t);
                let cd = lerp(control2, end, t);
                lerp(lerp(ab, bc, t), lerp(bc, cd, t), t)
            }
        }
    }
    pub fn end(self) -> Point {
        match self {
            Self::Line { end, .. } | Self::Cubic { end, .. } => end,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VectorPath {
    pub spans: Vec<Span>,
    /// 独立的闭合填充轮廓；同一对象内与描边合并覆盖，透明度只叠加一次。
    pub fills: Vec<Vec<Point>>,
}
impl VectorPath {
    pub fn from_polyline(points: &[Point]) -> Self {
        if points.len() == 1 {
            return Self {
                spans: vec![Span::Line {
                    start: points[0],
                    end: points[0],
                }],
                ..Self::default()
            };
        }
        Self {
            spans: points
                .windows(2)
                .map(|pair| Span::Line {
                    start: pair[0],
                    end: pair[1],
                })
                .collect(),
            ..Self::default()
        }
    }
    pub fn flattened(&self) -> Vec<(Point, Point)> {
        let mut segments = Vec::with_capacity(self.spans.len() * 2);
        for &span in &self.spans {
            match span {
                Span::Line { start, end } => segments.push((start, end)),
                Span::Cubic {
                    start,
                    control1,
                    control2,
                    end,
                } => flatten(start, control1, control2, end, 0, &mut segments),
            }
        }
        segments
    }
    pub fn svg_commands(&self) -> String {
        let mut d = String::new();
        for (i, &span) in self.spans.iter().enumerate() {
            if i == 0 || self.spans[i - 1].end() != span.start() {
                let a = span.start();
                let _ = write!(d, "M {:.3} {:.3} ", a.x, a.y);
            }
            match span {
                Span::Line { end, .. } => {
                    let _ = write!(d, "L {:.3} {:.3} ", end.x, end.y);
                }
                Span::Cubic {
                    control1: a,
                    control2: b,
                    end: c,
                    ..
                } => {
                    let _ = write!(
                        d,
                        "C {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} ",
                        a.x, a.y, b.x, b.y, c.x, c.y
                    );
                }
            }
        }
        d
    }
    pub fn bounds(&self) -> Option<crate::annotation::Bounds> {
        let first = self
            .spans
            .first()
            .map(|s| s.start())
            .or_else(|| self.fills.iter().flatten().next().copied())?;
        let (mut x, mut y, mut right, mut bottom) = (first.x, first.y, first.x, first.y);
        // 逐跨度只累计端点与极值点，不为每段分配临时数组（合成每个分块都会调用）。
        let mut include = |p: Point| {
            x = x.min(p.x);
            y = y.min(p.y);
            right = right.max(p.x);
            bottom = bottom.max(p.y);
        };
        for &span in &self.spans {
            include(span.start());
            include(span.end());
            if let Span::Cubic {
                start,
                control1,
                control2,
                end,
            } = span
            {
                for (a, b, c, d) in [
                    (start.x, control1.x, control2.x, end.x),
                    (start.y, control1.y, control2.y, end.y),
                ] {
                    let aa = -a + 3. * b - 3. * c + d;
                    let bb = 2. * (a - 2. * b + c);
                    let cc = b - a;
                    if aa.abs() < 1e-6 {
                        if bb.abs() > 1e-6 {
                            let t = -cc / bb;
                            if t > 0. && t < 1. {
                                include(span.at(t));
                            }
                        }
                    } else {
                        let disc = bb * bb - 4. * aa * cc;
                        if disc >= 0. {
                            // 稳定求根：裁切后的二次曲线转换为三次时 aa 接近零，
                            // 直接相减会丢失较小的根，导致包围盒漏掉真实极值。
                            let q = -0.5 * (bb + disc.sqrt().copysign(bb));
                            for t in [
                                q / aa,
                                if q.abs() > 1e-12 {
                                    cc / q
                                } else {
                                    -bb / (2. * aa)
                                },
                            ] {
                                if t > 0. && t < 1. {
                                    include(span.at(t));
                                }
                            }
                        }
                    }
                }
            }
        }
        for &p in self.fills.iter().flatten() {
            include(p);
        }
        Some(crate::annotation::Bounds {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
    /// 基础图形保留贝塞尔几何，避免固定数量的短线段在放大后变成折边。
    pub fn rectangle(b: crate::annotation::Bounds, radius: f32) -> Self {
        let r = radius.max(0.).min(b.width / 2.).min(b.height / 2.);
        let (x, y, right, bottom) = (b.x, b.y, b.x + b.width, b.y + b.height);
        if r <= 0. {
            return Self::polyline(
                &[
                    Point { x, y },
                    Point { x: right, y },
                    Point {
                        x: right,
                        y: bottom,
                    },
                    Point { x, y: bottom },
                ],
                false,
                true,
            );
        }
        let mut path = Self::default();
        let mut last = Point { x: x + r, y };
        for (end, cx, cy, angle) in [
            (
                Point { x: right - r, y },
                right - r,
                y + r,
                -std::f32::consts::FRAC_PI_2,
            ),
            (
                Point {
                    x: right,
                    y: bottom - r,
                },
                right - r,
                bottom - r,
                0.,
            ),
            (
                Point {
                    x: x + r,
                    y: bottom,
                },
                x + r,
                bottom - r,
                std::f32::consts::FRAC_PI_2,
            ),
            (Point { x, y: y + r }, x + r, y + r, std::f32::consts::PI),
        ] {
            path.spans.push(Span::Line { start: last, end });
            let arc = Self::arc(
                Point { x: cx, y: cy },
                r,
                r,
                angle,
                std::f32::consts::FRAC_PI_2,
                2,
            );
            last = arc.spans.last().unwrap().end();
            path.spans.extend(arc.spans);
        }
        // 消除三角函数舍入带来的闭合缝隙。
        let first = path.spans[0].start();
        if let Some(Span::Cubic { end, .. }) = path.spans.last_mut() {
            *end = first;
        }
        path
    }
    /// 四角按局部 TL/TR/BR/BL 编号；有效等值复用旧联动圆角路径。
    pub fn rectangle_corners(b: crate::annotation::Bounds, radii: [f32; 4]) -> Self {
        let radii = radii.map(|r| r.max(0.).min(b.width / 2.).min(b.height / 2.));
        if radii.iter().all(|&r| r == radii[0]) {
            return Self::rectangle(b, radii[0]);
        }
        let [tl, tr, br, bl] = radii;
        let (x, y, right, bottom) = (b.x, b.y, b.x + b.width, b.y + b.height);
        let first = Point { x: x + tl, y };
        let mut last = first;
        let mut path = Self::default();
        for (end, center, radius, angle, next) in [
            (
                Point { x: right - tr, y },
                Point {
                    x: right - tr,
                    y: y + tr,
                },
                tr,
                -std::f32::consts::FRAC_PI_2,
                Point {
                    x: right,
                    y: y + tr,
                },
            ),
            (
                Point {
                    x: right,
                    y: bottom - br,
                },
                Point {
                    x: right - br,
                    y: bottom - br,
                },
                br,
                0.,
                Point {
                    x: right - br,
                    y: bottom,
                },
            ),
            (
                Point {
                    x: x + bl,
                    y: bottom,
                },
                Point {
                    x: x + bl,
                    y: bottom - bl,
                },
                bl,
                std::f32::consts::FRAC_PI_2,
                Point { x, y: bottom - bl },
            ),
            (
                Point { x, y: y + tl },
                Point {
                    x: x + tl,
                    y: y + tl,
                },
                tl,
                std::f32::consts::PI,
                first,
            ),
        ] {
            path.spans.push(Span::Line { start: last, end });
            last = end;
            if radius > 0. {
                let mut arc = Self::arc(
                    center,
                    radius,
                    radius,
                    angle,
                    std::f32::consts::FRAC_PI_2,
                    2,
                );
                // 接点使用边缘精确坐标，避免三角函数舍入形成路径缝隙。
                if let Some(Span::Cubic { start, .. }) = arc.spans.first_mut() {
                    *start = end;
                }
                if let Some(Span::Cubic { end, .. }) = arc.spans.last_mut() {
                    *end = next;
                }
                last = next;
                path.spans.extend(arc.spans);
            }
        }
        if let Some(Span::Line { end, .. } | Span::Cubic { end, .. }) = path.spans.last_mut() {
            *end = first;
        }
        path
    }
    pub fn ellipse(b: crate::annotation::Bounds) -> Self {
        let mut path = Self::arc(
            Point {
                x: b.x + b.width / 2.,
                y: b.y + b.height / 2.,
            },
            b.width / 2.,
            b.height / 2.,
            0.,
            std::f32::consts::TAU,
            8,
        );
        let first = path.spans[0].start();
        if let Some(Span::Cubic { end, .. }) = path.spans.last_mut() {
            *end = first;
        }
        path
    }
    fn arc(center: Point, rx: f32, ry: f32, start: f32, sweep: f32, count: usize) -> Self {
        let step = sweep / count as f32;
        let handle = 4. / 3. * (step / 4.).tan();
        let mut spans = Vec::with_capacity(count);
        for i in 0..count {
            let a = start + i as f32 * step;
            let b = a + step;
            let p = |t: f32| Point {
                x: center.x + rx * t.cos(),
                y: center.y + ry * t.sin(),
            };
            let (start, end) = (p(a), p(b));
            spans.push(Span::Cubic {
                start,
                control1: Point {
                    x: start.x - rx * a.sin() * handle,
                    y: start.y + ry * a.cos() * handle,
                },
                control2: Point {
                    x: end.x + rx * b.sin() * handle,
                    y: end.y - ry * b.cos() * handle,
                },
                end,
            });
        }
        Self {
            spans,
            ..Default::default()
        }
    }
    pub fn arrow(a: Point, b: Point, mid: Point, style: crate::annotation::Style) -> Self {
        crate::arrow_geometry::path(a, b, mid, style)
    }
    pub fn polyline(points: &[Point], smooth: bool, closed: bool) -> Self {
        if points.len() < 2 {
            return Self::from_polyline(points);
        }
        let n = points.len();
        let count = if closed && n >= 3 { n } else { n - 1 };
        let closed = count == n;
        let mut spans = Vec::with_capacity(count);
        for i in 0..count {
            let (a, b) = (points[i], points[(i + 1) % n]);
            if !smooth {
                spans.push(Span::Line { start: a, end: b });
                continue;
            }
            let prev = if i > 0 {
                points[i - 1]
            } else if closed {
                points[n - 1]
            } else {
                a
            };
            let next = if i + 2 < n {
                points[i + 2]
            } else if closed {
                points[(i + 2) % n]
            } else {
                b
            };
            let control = |anchor: Point, before: Point, after: Point, sign: f32| {
                let dx = (after.x - before.x) / 6. * sign;
                let dy = (after.y - before.y) / 6. * sign;
                let factor = (a.distance(b) / 3. / dx.hypot(dy).max(0.001)).min(1.);
                Point {
                    x: anchor.x + dx * factor,
                    y: anchor.y + dy * factor,
                }
            };
            spans.push(Span::Cubic {
                start: a,
                control1: control(a, prev, b, 1.),
                control2: control(b, a, next, -1.),
                end: b,
            });
        }
        Self {
            spans,
            ..Self::default()
        }
    }
}
fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}
fn flatten(a: Point, b: Point, c: Point, d: Point, depth: u8, output: &mut Vec<(Point, Point)>) {
    if depth >= 12 || distance_to_segment(b, a, d).max(distance_to_segment(c, a, d)) <= FLATNESS {
        output.push((a, d));
        return;
    }
    let (ab, bc, cd) = (lerp(a, b, 0.5), lerp(b, c, 0.5), lerp(c, d, 0.5));
    let (abc, bcd) = (lerp(ab, bc, 0.5), lerp(bc, cd, 0.5));
    let middle = lerp(abc, bcd, 0.5);
    flatten(a, ab, abc, middle, depth + 1, output);
    flatten(middle, bcd, cd, d, depth + 1, output);
}

/// 闭合轮廓的最后一条边也是命中、覆盖率与导出的组成部分。
pub fn contour_edges(points: &[Point]) -> Vec<(Point, Point)> {
    if points.len() < 3 {
        return Vec::new();
    }
    (0..points.len())
        .map(|i| (points[i], points[(i + 1) % points.len()]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Bounds;

    #[test]
    fn rectangle_corner_effective_equal_radii_keep_exact_legacy_path() {
        let bounds = Bounds {
            x: 10.,
            y: 20.,
            width: 160.,
            height: 120.,
        };
        for radius in [0., 1., 12., 60., 80., 300.] {
            assert_eq!(
                VectorPath::rectangle_corners(bounds, [radius; 4]),
                VectorPath::rectangle(bounds, radius)
            );
        }
        assert_eq!(
            VectorPath::rectangle_corners(bounds, [80., 90., 100., 300.]),
            VectorPath::rectangle(bounds, 80.)
        );
    }

    #[test]
    fn rectangle_corner_zero_arcs_and_asymmetric_caps_close_without_gaps() {
        let bounds = Bounds {
            x: 10.,
            y: 20.,
            width: 160.,
            height: 120.,
        };
        for mask in 0_u32..16 {
            let radii = [20., 30., 40., 50.];
            let radii = std::array::from_fn(|corner| {
                if mask & (1 << corner) != 0 {
                    radii[corner]
                } else {
                    0.
                }
            });
            let path = VectorPath::rectangle_corners(bounds, radii);
            assert_eq!(path.spans.len(), 4 + 2 * mask.count_ones() as usize);
            assert_eq!(
                path.spans.first().unwrap().start(),
                path.spans.last().unwrap().end()
            );
            for pair in path.spans.windows(2) {
                assert_eq!(pair[0].end(), pair[1].start());
            }
        }
        let path = VectorPath::rectangle_corners(bounds, [80., 0., 300., 10.]);
        assert_eq!(
            path.spans.first().unwrap().start(),
            Point { x: 70., y: 20. }
        );
        assert_eq!(path.spans[0].end(), Point { x: 170., y: 20. });
        assert_eq!(path.spans.last().unwrap().end(), path.spans[0].start());
        for span in path.spans {
            for step in 0..=20 {
                let point = span.at(step as f32 / 20.);
                assert!(bounds.contains(point, 0.001));
            }
        }
    }
}
