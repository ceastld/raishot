//! 联动或矩形独立圆角的四角拖柄几何；坐标始终是全屏物理像素，显示尺寸随视口换算。
//! 不改变已有 resize/node handle 的索引和对象位置。
use crate::annotation::{Annotation, Point};

const MIN_DIAGONAL_INSET: f32 = 18.;
const HANDLE_GAP: f32 = 2.;
const HANDLE_RADIUS: f32 = 5.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadiusControl {
    /// 0=左上，1=右上，2=右下，3=左下；是对象局部角，旋转不改编号。
    pub corner: usize,
    pub position: Point,
    pub hit_radius: f32,
}

impl RadiusControl {
    pub fn hit(&self, point: Point) -> bool {
        self.position.distance(point) <= self.hit_radius
    }

    /// 用手势原始对象逆变换，按下点到当前位置的向内对角投影即半径增量。
    /// 不从柄的绝对位置反推半径，因此大圆角夹位和按在柄边缘都不会跳变。
    pub fn drag_delta(&self, annotation: &Annotation, start: Point, point: Point) -> f32 {
        let start = annotation.to_local(start);
        let point = annotation.to_local(point);
        let (dx, dy) = (point.x - start.x, point.y - start.y);
        let delta = match self.corner {
            0 => dx + dy,
            1 => -dx + dy,
            2 => -dx - dy,
            3 => dx - dy,
            _ => return 0.,
        } * std::f32::consts::FRAC_1_SQRT_2;
        if delta.is_finite() {
            delta
        } else {
            0.
        }
    }
}

pub(crate) fn controls(annotation: &Annotation, tolerance: f32) -> Vec<RadiusControl> {
    if !annotation.supports_radius()
        || !tolerance.is_finite()
        || tolerance <= 0.
        || (0..4).any(|corner| !annotation.radius_at_corner(corner).is_finite())
        || !annotation.rotation.is_finite()
    {
        return Vec::new();
    }
    let b = annotation.local_bounds();
    if [b.x, b.y, b.width, b.height].iter().any(|v| !v.is_finite())
        || b.width <= 0.
        || b.height <= 0.
    {
        return Vec::new();
    }
    // 与现有 6 DIP resize 命中容差同源，GPUI 的视口缩放/DPI 不另行猜测。
    let scale = tolerance / 6.;
    let minimum = MIN_DIAGONAL_INSET * scale;
    let separation = tolerance * 2. + HANDLE_GAP * scale;
    let shortest = b.width.min(b.height);
    // 四个柄在大圆角时也不能挤到中心：相邻柄命中范围之间留 2 DIP。
    let maximum = (shortest / 2. - separation / 2.) * std::f32::consts::SQRT_2;
    if maximum < minimum {
        return Vec::new();
    }
    let insets = [0, 1, 2, 3].map(|corner| {
        let radius = annotation
            .radius_at_corner(corner)
            .max(0.)
            .min(shortest / 2.);
        let diagonal = (minimum + radius).min(maximum);
        diagonal * std::f32::consts::FRAC_1_SQRT_2
    });
    // 可见点整体留在对象内部，10 DIP 菱形不会穿过对面的边。
    if insets.iter().any(|&inset| inset < HANDLE_RADIUS * scale) {
        return Vec::new();
    }
    let positions = [
        Point {
            x: b.x + insets[0],
            y: b.y + insets[0],
        },
        Point {
            x: b.x + b.width - insets[1],
            y: b.y + insets[1],
        },
        Point {
            x: b.x + b.width - insets[2],
            y: b.y + b.height - insets[2],
        },
        Point {
            x: b.x + insets[3],
            y: b.y + b.height - insets[3],
        },
    ];
    let rotation = annotation.rotation_transform();
    let controls: Vec<_> = positions
        .into_iter()
        .enumerate()
        .map(|(corner, position)| RadiusControl {
            corner,
            position: rotation.map(position),
            hit_radius: tolerance,
        })
        .collect();
    let resize = annotation.handles();
    for (i, control) in controls.iter().enumerate() {
        if resize
            .iter()
            .any(|&p| control.position.distance(p) < separation - 0.001 * scale)
            || controls[..i]
                .iter()
                .any(|p| control.position.distance(p.position) < separation - 0.001 * scale)
        {
            // 小对象统一隐藏，避免绘制与优先命中只留下碰撞的一部分柄。
            return Vec::new();
        }
    }
    controls
}

#[cfg(test)]
pub(crate) fn radius_after_delta(annotation: &Annotation, delta: f32) -> f32 {
    radius_after_value_delta(annotation, annotation.corner_radius(), delta)
}

pub(crate) fn radius_after_corner_delta(annotation: &Annotation, corner: usize, delta: f32) -> f32 {
    let before = annotation.radius_at_corner(corner);
    if corner >= 4 {
        return before;
    }
    radius_after_value_delta(annotation, before, delta)
}

fn radius_after_value_delta(annotation: &Annotation, before: f32, delta: f32) -> f32 {
    if !annotation.supports_radius() || !before.is_finite() || !delta.is_finite() {
        return before;
    }
    let b = annotation.local_bounds();
    if !b.width.is_finite() || !b.height.is_finite() {
        return before;
    }
    let before = before.max(0.);
    let limit = b.width.min(b.height).max(0.) / 2.;
    // 缩放可留下 >50 或超过实际半边的值；不在按下或反向首帧突然截断。
    // 普通值受 50 与短边限制，已有大值向外不增大、向内逐步回调。
    (before + delta).clamp(0., before.max(limit.min(50.)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Style;
    use crate::annotation::Tool;

    fn shape(tool: Tool, width: f32, height: f32, radius: f32) -> Annotation {
        let mut a = Annotation::new(
            tool,
            Point { x: 20., y: 30. },
            Style {
                radius,
                ..Default::default()
            },
        );
        a.update(Point {
            x: 20. + width,
            y: 30. + height,
        });
        a
    }

    #[test]
    fn rectangle_independent_controls_drag_local_corner_only_after_rotation() {
        let mut a = shape(Tool::Rectangle, 400., 300., 0.);
        a.style.rectangle_radii = Some([0., 10., 20., 30.]);
        a.rotation = -37.;
        let controls = a.radius_controls(6.);
        assert_eq!(controls.len(), 4);
        let bounds = a.local_bounds();
        let origins = [
            Point {
                x: bounds.x,
                y: bounds.y,
            },
            Point {
                x: bounds.x + bounds.width,
                y: bounds.y,
            },
            Point {
                x: bounds.x + bounds.width,
                y: bounds.y + bounds.height,
            },
            Point {
                x: bounds.x,
                y: bounds.y + bounds.height,
            },
        ];
        let directions = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)];
        for control in controls {
            let local = a.to_local(control.position);
            let distance = local.distance(origins[control.corner]);
            assert!((distance - (18. + control.corner as f32 * 10.)).abs() < 0.001);
            assert!(a.handles().iter().all(|&p| !control.hit(p)));
            let start = a.to_world(Point {
                x: local.x + 0.4,
                y: local.y - 0.7,
            });
            let (dx, dy) = directions[control.corner];
            let point = a.to_world(Point {
                x: local.x + 0.4 + dx * 12. * std::f32::consts::FRAC_1_SQRT_2,
                y: local.y - 0.7 + dy * 12. * std::f32::consts::FRAC_1_SQRT_2,
            });
            let delta = control.drag_delta(&a, start, point);
            assert!((delta - 12.).abs() < 0.001);
            let before = a.style.rectangle_radii.unwrap();
            let value = a.radius_after_corner_delta(control.corner, delta);
            a.set_corner_handle_radius(control.corner, value);
            let after = a.style.rectangle_radii.unwrap();
            for corner in 0..4 {
                let expected = before[corner] + if corner == control.corner { 12. } else { 0. };
                assert!((after[corner] - expected).abs() < 0.001);
            }
            assert_eq!(a.style.radius, after[0]);
        }
    }

    #[test]
    fn rectangle_independent_controls_hide_together_and_keep_large_delta_monotonic() {
        let mut a = shape(Tool::Rectangle, 400., 60., 80.);
        a.style.rectangle_radii = Some([80., 0., 20., 200.]);
        for scale in [0.5, 1.] {
            let controls = a.radius_controls(6. * scale);
            assert_eq!(controls.len(), 4);
            for (index, control) in controls.iter().enumerate() {
                assert!(a.handles().iter().all(|&p| !control.hit(p)));
                assert!(controls[..index]
                    .iter()
                    .all(|p| control.position.distance(p.position) >= 13.99 * scale));
                assert_eq!(
                    a.radius_after_corner_delta(control.corner, 0.),
                    a.radius_at_corner(control.corner)
                );
            }
        }
        assert_eq!(a.radius_after_corner_delta(0, 1.), 80.);
        assert_eq!(a.radius_after_corner_delta(0, -1.), 79.);
        assert_eq!(a.radius_after_corner_delta(1, 100.), 30.);
        assert_eq!(a.radius_after_corner_delta(3, 1.), 200.);
        assert_eq!(a.radius_after_corner_delta(3, -1.), 199.);
        a.update(Point { x: 420., y: 65. });
        assert!(a.radius_controls(6.).is_empty());
        a.update(Point { x: 420., y: 90. });
        a.style.rectangle_radii = Some([80., 0., f32::NAN, 200.]);
        assert!(a.radius_controls(6.).is_empty());
    }

    #[test]
    fn radius_controls_keep_four_corners_separate_from_resize_at_view_scales() {
        for tool in [Tool::Rectangle, Tool::Spotlight] {
            for scale in [0.5, 1., 2., 4.] {
                let a = shape(tool, 800., 600., 0.);
                let points = a.points.clone();
                let old_handles = a.handles();
                let controls = a.radius_controls(6. * scale);
                assert_eq!(controls.len(), 4);
                for (i, control) in controls.iter().enumerate() {
                    assert_eq!(control.corner, i);
                    assert!(control.hit(control.position));
                    for &p in &old_handles {
                        assert!(!control.hit(p));
                        assert!(control.position.distance(p) / scale >= 13.99);
                    }
                    for other in &controls[..i] {
                        assert!(control.position.distance(other.position) / scale >= 13.99);
                    }
                    assert_eq!(
                        control.drag_delta(&a, control.position, control.position),
                        0.
                    );
                }
                assert_eq!(a.radius_controls(6. * scale).first(), Some(&controls[0]));
                assert_eq!(a.points, points);
                assert_eq!(a.handles(), old_handles);
            }
        }
    }

    #[test]
    fn radius_corner_drag_uses_inverse_rotation_without_grab_offset_jump() {
        let mut a = shape(Tool::Spotlight, 300., 240., 17.);
        a.rotation = 37.;
        let directions = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)];
        for control in a.radius_controls(6.) {
            let local = a.to_local(control.position);
            let start = a.to_world(Point {
                x: local.x + 0.4,
                y: local.y - 0.7,
            });
            let (dx, dy) = directions[control.corner];
            let distance = 12. * std::f32::consts::FRAC_1_SQRT_2;
            let point = a.to_world(Point {
                x: local.x + 0.4 + dx * distance,
                y: local.y - 0.7 + dy * distance,
            });
            let delta = control.drag_delta(&a, start, point);
            assert!(
                (delta - 12.).abs() < 0.001,
                "corner {} delta {delta}",
                control.corner
            );
            assert!((a.radius_after_delta(delta) - 29.).abs() < 0.001);
            assert_eq!(control.drag_delta(&a, start, start), 0.);
        }
    }

    #[test]
    fn radius_controls_hide_small_shapes_and_clamp_large_insets_without_collisions() {
        assert!(shape(Tool::Rectangle, 20., 100., 0.)
            .radius_controls(6.)
            .is_empty());
        assert!(shape(Tool::Spotlight, 100., 35., 17.)
            .radius_controls(6.)
            .is_empty());
        assert!(shape(Tool::Ellipse, 100., 100., 17.)
            .radius_controls(6.)
            .is_empty());
        assert!(shape(Tool::Rectangle, 100., 100., 17.)
            .radius_controls(0.)
            .is_empty());
        let mut a = shape(Tool::Rectangle, 100., 100., 50.);
        a.points.reverse();
        a.rotation = -121.;
        let controls = a.radius_controls(6.);
        assert_eq!(controls.len(), 4);
        for (i, control) in controls.iter().enumerate() {
            for other in &controls[..i] {
                assert!(control.position.distance(other.position) >= 13.99);
            }
            assert_eq!(
                a.radius_after_delta(control.drag_delta(&a, control.position, control.position)),
                50.
            );
        }
    }

    #[test]
    fn radius_drag_limit_respects_short_edge_and_preserves_scaled_values_monotonically() {
        let mut a = shape(Tool::Rectangle, 400., 60., 5.);
        assert_eq!(a.radius_after_delta(100.), 30.);
        assert_eq!(a.radius_after_delta(-100.), 0.);
        a.style.radius = 80.;
        assert_eq!(a.radius_after_delta(0.), 80.);
        assert_eq!(a.radius_after_delta(1.), 80.);
        assert_eq!(a.radius_after_delta(-1.), 79.);
        assert_eq!(a.radius_after_delta(-100.), 0.);
        a.style.radius = 0.;
        assert_eq!(a.radius_after_delta(12.), 12.);
        a.style.radius = f32::NAN;
        assert!(a.radius_controls(6.).is_empty());
    }

    #[test]
    fn highlight_radius_controls_follow_rotation_and_keep_geometry_limits() {
        let mut a = shape(Tool::HighlighterRect, 300., 240., 0.);
        a.style.width = 80.;
        a.rotation = -53.;
        let original = a.clone();
        let directions = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)];
        for scale in [0.5, 1., 2.] {
            let controls = a.radius_controls(6. * scale);
            assert_eq!(controls.len(), 4);
            for control in controls {
                assert!(control.hit(control.position));
                assert!(a.handles().iter().all(|&p| !control.hit(p)));
                let start = a.to_local(control.position);
                let (dx, dy) = directions[control.corner];
                let point = a.to_world(Point {
                    x: start.x + dx * 14. * std::f32::consts::FRAC_1_SQRT_2,
                    y: start.y + dy * 14. * std::f32::consts::FRAC_1_SQRT_2,
                });
                let delta = control.drag_delta(&a, control.position, point);
                assert!((a.radius_after_delta(delta) - 14.).abs() < 0.001);
                assert_eq!(
                    control.drag_delta(&a, control.position, control.position),
                    0.
                );
            }
        }
        assert_eq!(a, original);
        assert!(shape(Tool::HighlighterRect, 30., 100., 0.)
            .radius_controls(6.)
            .is_empty());
        a.style.radius = 80.;
        assert_eq!(a.radius_after_delta(1.), 80.);
        assert_eq!(a.radius_after_delta(-1.), 79.);
    }
}
