//! 多对象手势只变换对象几何，文档在结束时统一提交一次。
use crate::{
    annotation::{Annotation, Bounds, Point},
    document::ObjectId,
    geometry::Rect,
};
#[derive(Clone, Copy)]
pub enum Manipulation {
    Move,
    Scale { bounds: Bounds, handle: usize },
    Rotate { pivot: Point, angle: f32 },
}
impl Manipulation {
    pub fn scale_factor(self, p: Point) -> f32 {
        let Self::Scale { bounds, handle } = self else {
            return 1.;
        };
        let h = bounds.handles();
        let anchor = h[7 - handle];
        let (dx, dy) = (h[handle].x - anchor.x, h[handle].y - anchor.y);
        // 对象可以大于画布；64 只限制一次手势的放大倍数，避免角点拖出无界网格。
        (((p.x - anchor.x) * dx + (p.y - anchor.y) * dy) / (dx * dx + dy * dy).max(1.))
            .clamp(0.02, 64.)
    }
}
pub fn bounds(items: &[(ObjectId, Annotation)]) -> Bounds {
    crate::transform::bounds(
        items
            .iter()
            .flat_map(|(_, a)| crate::transform::corners(a.bounds())),
    )
}
/// 单对象沿自身坐标系旋转；多对象保留共同的水平外接框。
fn frame(items: &[(ObjectId, Annotation)]) -> (Bounds, crate::transform::Rotation) {
    if let [(_, a)] = items {
        (a.local_bounds(), a.rotation_transform())
    } else {
        let b = bounds(items);
        (
            b,
            crate::transform::Rotation::new(crate::transform::center(b), 0.),
        )
    }
}
pub fn pivot(items: &[(ObjectId, Annotation)]) -> Point {
    frame(items).1.center
}
#[derive(Clone, Copy)]
pub struct RotationControl {
    pub anchor: Point,
    pub position: Point,
    pub pivot: Point,
}
pub fn rotation_control(
    items: &[(ObjectId, Annotation)],
    canvas: Rect,
    gap: f32,
) -> RotationControl {
    let (b, rotation) = frame(items);
    let center = crate::transform::center(b);
    // 连接点和把手一起从局部坐标转到画布坐标，不能分别使用水平外接框。
    let candidates = [
        (
            Point {
                x: center.x,
                y: b.y,
            },
            Point {
                x: center.x,
                y: b.y - gap,
            },
        ),
        (
            Point {
                x: center.x,
                y: b.y + b.height,
            },
            Point {
                x: center.x,
                y: b.y + b.height + gap,
            },
        ),
        (
            Point {
                x: b.x + b.width,
                y: center.y,
            },
            Point {
                x: b.x + b.width + gap,
                y: center.y,
            },
        ),
        (
            Point {
                x: b.x,
                y: center.y,
            },
            Point {
                x: b.x - gap,
                y: center.y,
            },
        ),
    ]
    .map(|(anchor, position)| RotationControl {
        anchor: rotation.map(anchor),
        position: rotation.map(position),
        pivot: center,
    });
    let margin = gap / 4.;
    let left = margin.min(canvas.width as f32 / 2.);
    let top = margin.min(canvas.height as f32 / 2.);
    let right = canvas.width as f32 - left;
    let bottom = canvas.height as f32 - top;
    // 优先上边；贴近画布边缘时使用仍能完整点击的另一边。
    candidates
        .into_iter()
        .find(|c| {
            c.position.x >= left
                && c.position.x <= right
                && c.position.y >= top
                && c.position.y <= bottom
        })
        .unwrap_or_else(|| {
            let mut c = candidates[0];
            c.position.x = c.position.x.clamp(left, right);
            c.position.y = c.position.y.clamp(top, bottom);
            c
        })
}
pub fn preview(
    before: &[(ObjectId, Annotation)],
    current: &mut [(ObjectId, Annotation)],
    start: Point,
    p: Point,
    mode: Manipulation,
) {
    // 每帧只从手势基准推导，不能再次缩放上一帧已缩放的样式。
    for (a, original) in current.iter_mut().zip(before) {
        *a = original.clone();
    }
    if p.distance(start) < 4. {
        return;
    }
    match mode {
        Manipulation::Move => {
            let dx = p.x - start.x;
            let dy = p.y - start.y;
            for (_, a) in current.iter_mut() {
                a.translate(dx, dy);
            }
        }
        Manipulation::Rotate { pivot, angle } => {
            if p.distance(pivot) < 1. {
                return;
            }
            let delta = ((p.y - pivot.y).atan2(p.x - pivot.x).to_degrees() - angle + 180.)
                .rem_euclid(360.)
                - 180.;
            for (_, a) in current.iter_mut() {
                a.rotate_about(pivot, delta);
            }
        }
        Manipulation::Scale { bounds, handle } => {
            let h = bounds.handles();
            let anchor = h[7 - handle];
            let scale = mode.scale_factor(p);
            for (_, a) in current.iter_mut() {
                a.scale_about(anchor, scale);
            }
        }
    }
}
