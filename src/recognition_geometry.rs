//! 识别裁行的原图投影。四角沿阅读坐标，而不是按屏幕左上角重新排序。
//! 几何只证明裁行映射；缺少有效字素采样证据时沿用词内估算，采样带亦非字形墨迹。
use crate::{annotation::Point, geometry::Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingDirection {
    LeftToRight,
    RightToLeft,
    TopToBottom,
    BottomToTop,
}
impl ReadingDirection {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::LeftToRight => "ltr",
            Self::RightToLeft => "rtl",
            Self::TopToBottom => "ttb",
            Self::BottomToTop => "btt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextGeometry {
    /// (u=0,v=0), (1,0), (1,1), (0,1)；u 沿识别原文递增。
    /// 180°纠正已进入四角顺序。不能再根据屏幕 x/y 排序这些点。
    pub quad: [Point; 4],
}
impl TextGeometry {
    /// 提供者缺失／无效投影时，消费端可明确退回原有效轴对齐 rect。
    pub fn validated_in(self, rect: Rect) -> Option<Projection> {
        let right = rect.x.checked_add(rect.width)?;
        let bottom = rect.y.checked_add(rect.height)?;
        if rect.width == 0 || rect.height == 0 {
            return None;
        }
        if self.quad.iter().any(|p| {
            f64::from(p.x) < f64::from(rect.x)
                || f64::from(p.x) > f64::from(right)
                || f64::from(p.y) < f64::from(rect.y)
                || f64::from(p.y) > f64::from(bottom)
        }) {
            return None;
        }
        Projection::new(self)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Projection {
    geometry: TextGeometry,
    forward: [f64; 8],
    inverse: [f64; 9],
}
impl Projection {
    pub fn new(geometry: TextGeometry) -> Option<Self> {
        let quad = geometry.quad;
        if quad.iter().any(|p| {
            !p.x.is_finite() || !p.y.is_finite() || p.x.abs() > 1_000_000. || p.y.abs() > 1_000_000.
        }) {
            return None;
        }
        let q = quad.map(|p| [f64::from(p.x), f64::from(p.y)]);
        for i in 0..4 {
            let a = q[i];
            let b = q[(i + 1) % 4];
            let c = q[(i + 2) % 4];
            let ab = [b[0] - a[0], b[1] - a[1]];
            let bc = [c[0] - b[0], c[1] - b[1]];
            let length = ab[0].hypot(ab[1]);
            // 正绕向、严格凸、可表达边长；自交、镜像和重复点不用于停点。
            if !(0.0001..=65_535.).contains(&length) || ab[0] * bc[1] - ab[1] * bc[0] <= 1e-8 {
                return None;
            }
        }
        let dx1 = q[1][0] - q[2][0];
        let dx2 = q[3][0] - q[2][0];
        let dy1 = q[1][1] - q[2][1];
        let dy2 = q[3][1] - q[2][1];
        let dx3 = q[0][0] - q[1][0] + q[2][0] - q[3][0];
        let dy3 = q[0][1] - q[1][1] + q[2][1] - q[3][1];
        let denominator = dx1 * dy2 - dx2 * dy1;
        if denominator.abs() <= 1e-8 {
            return None;
        }
        let g = (dx3 * dy2 - dx2 * dy3) / denominator;
        let h = (dx1 * dy3 - dx3 * dy1) / denominator;
        let forward = [
            q[1][0] - q[0][0] + g * q[1][0],
            q[3][0] - q[0][0] + h * q[3][0],
            q[0][0],
            q[1][1] - q[0][1] + g * q[1][1],
            q[3][1] - q[0][1] + h * q[3][1],
            q[0][1],
            g,
            h,
        ];
        if forward.iter().any(|n| !n.is_finite())
            || [1., 1. + g, 1. + h, 1. + g + h].iter().any(|d| *d <= 1e-8)
        {
            return None;
        }
        let [a, b, c, d, e, f, g, h] = forward;
        let inverse = [
            e - f * h,
            c * h - b,
            b * f - c * e,
            f * g - d,
            a - c * g,
            c * d - a * f,
            d * h - e * g,
            b * g - a * h,
            a * e - b * d,
        ];
        let determinant = a * inverse[0] + b * inverse[3] + c * inverse[6];
        if !determinant.is_finite() || determinant.abs() <= 1e-8 {
            return None;
        }
        Some(Self {
            geometry,
            forward,
            inverse,
        })
    }
    pub fn quad(self) -> [Point; 4] {
        self.geometry.quad
    }
    pub fn point(self, u: f64, v: f64) -> Point {
        let t = self.forward;
        let denominator = t[6] * u + t[7] * v + 1.;
        Point {
            x: ((t[0] * u + t[1] * v + t[2]) / denominator) as f32,
            y: ((t[3] * u + t[4] * v + t[5]) / denominator) as f32,
        }
    }
    pub fn inverse(self, point: Point) -> Option<[f64; 2]> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return None;
        }
        let (x, y) = (f64::from(point.x), f64::from(point.y));
        let t = self.inverse;
        let denominator = t[6] * x + t[7] * y + t[8];
        if denominator.abs() <= 1e-8 {
            return None;
        }
        let local = [
            (t[0] * x + t[1] * y + t[2]) / denominator,
            (t[3] * x + t[4] * y + t[5]) / denominator,
        ];
        local.iter().all(|n| n.is_finite()).then_some(local)
    }
    pub fn direction(self) -> ReadingDirection {
        let (start, end) = (self.point(0., 0.5), self.point(1., 0.5));
        let (dx, dy) = (end.x - start.x, end.y - start.y);
        if dx.abs() >= dy.abs() {
            if dx >= 0. {
                ReadingDirection::LeftToRight
            } else {
                ReadingDirection::RightToLeft
            }
        } else if dy >= 0. {
            ReadingDirection::TopToBottom
        } else {
            ReadingDirection::BottomToTop
        }
    }
}

#[cfg(test)]
#[path = "recognition_geometry_tests.rs"]
mod tests;
