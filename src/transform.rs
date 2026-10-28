//! 物理坐标的刚体旋转与共享边界，模型、命中和像素遮罩使用同一变换。
use crate::annotation::{Bounds, Point};
#[derive(Clone, Copy)]
pub struct Rotation {
    pub center: Point,
    cos: f32,
    sin: f32,
}
impl Rotation {
    pub fn new(center: Point, degrees: f32) -> Self {
        let (sin, cos) = degrees.to_radians().sin_cos();
        Self { center, cos, sin }
    }
    pub fn map(self, p: Point) -> Point {
        let (x, y) = (p.x - self.center.x, p.y - self.center.y);
        Point {
            x: self.center.x + x * self.cos - y * self.sin,
            y: self.center.y + x * self.sin + y * self.cos,
        }
    }
    pub fn inverse(self) -> Self {
        Self {
            sin: -self.sin,
            ..self
        }
    }
}
pub fn center(b: Bounds) -> Point {
    Point {
        x: b.x + b.width / 2.,
        y: b.y + b.height / 2.,
    }
}
pub fn bounds(points: impl IntoIterator<Item = Point>) -> Bounds {
    let (mut x, mut y, mut right, mut bottom) = (
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    );
    for p in points {
        x = x.min(p.x);
        y = y.min(p.y);
        right = right.max(p.x);
        bottom = bottom.max(p.y);
    }
    if !x.is_finite() {
        return Bounds {
            x: 0.,
            y: 0.,
            width: 0.,
            height: 0.,
        };
    }
    Bounds {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}
pub fn corners(b: Bounds) -> [Point; 4] {
    let h = b.handles();
    [h[0], h[2], h[7], h[5]]
}
