//! 固定屏幕预览的物理几何；呈现回执、编辑资格及 UIA 生命周期由宿主管理。
use crate::geometry::{Rect, ScreenRect};

fn same_selection(a: Rect, b: Rect) -> bool {
    a.width > 0
        && a.height > 0
        && b.width > 0
        && b.height > 0
        && [
            (a.x, b.x),
            (a.y, b.y),
            (a.width, b.width),
            (a.height, b.height),
        ]
        .into_iter()
        .all(|(a, b)| (i64::from(a) - i64::from(b)).abs() <= 1)
}

fn valid_canvas(canvas: ScreenRect) -> bool {
    canvas.width > 0
        && canvas.height > 0
        && canvas.width <= i32::MAX as u32
        && canvas.height <= i32::MAX as u32
        && i64::from(canvas.left) + i64::from(canvas.width) <= i64::from(i32::MAX) + 1
        && i64::from(canvas.top) + i64::from(canvas.height) <= i64::from(i32::MAX) + 1
}

fn local_monitor(canvas: ScreenRect, screen: ScreenRect) -> Option<Rect> {
    if !valid_canvas(canvas) || !valid_canvas(screen) {
        return None;
    }
    let left = i64::from(screen.left).max(i64::from(canvas.left));
    let top = i64::from(screen.top).max(i64::from(canvas.top));
    let right = (i64::from(screen.left) + i64::from(screen.width))
        .min(i64::from(canvas.left) + i64::from(canvas.width));
    let bottom = (i64::from(screen.top) + i64::from(screen.height))
        .min(i64::from(canvas.top) + i64::from(canvas.height));
    if right <= left || bottom <= top {
        return None;
    }
    let rect = Rect {
        x: u32::try_from(left - i64::from(canvas.left)).ok()?,
        y: u32::try_from(top - i64::from(canvas.top)).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    };
    rect.valid(canvas.width, canvas.height).then_some(rect)
}

/// 仅使用按键发生时的全局物理点及冻结屏幕列表；screen 包含任务栏。
pub(crate) fn snap_target(
    canvas: ScreenRect,
    screens: &[ScreenRect],
    global_point: (i32, i32),
    shown: Option<Rect>,
) -> Option<Rect> {
    if !valid_canvas(canvas) {
        return None;
    }
    let mut nearest: Option<(i128, Rect)> = None;
    for &screen in screens {
        let Some(rect) = local_monitor(canvas, screen) else {
            continue;
        };
        let left = i64::from(screen.left);
        let top = i64::from(screen.top);
        let right = left + i64::from(screen.width) - 1;
        let bottom = top + i64::from(screen.height) - 1;
        let (x, y) = (i64::from(global_point.0), i64::from(global_point.1));
        let dx = i128::from(x - x.clamp(left, right));
        let dy = i128::from(y - y.clamp(top, bottom));
        let distance = dx * dx + dy * dy;
        if nearest.is_none_or(|(previous, _)| distance < previous) {
            nearest = Some((distance, rect));
        }
        if distance == 0 {
            break;
        }
    }
    let monitor = nearest?.1;
    let full = Rect::full(canvas.width, canvas.height);
    Some(
        if shown
            .filter(|rect| rect.valid(canvas.width, canvas.height))
            .is_some_and(|rect| same_selection(rect, monitor))
            && !same_selection(monitor, full)
        {
            full
        } else {
            monitor
        },
    )
}

/// 固定范围立即成为下一次切换的当前值；输出仍由宿主另外核验真实呈现。
/// 避免 handoff 批量重放时多次 Ctrl+A 都借同一旧画面而停在 full。
pub(crate) fn snap_preview_target(
    canvas: ScreenRect,
    screens: &[ScreenRect],
    global_point: (i32, i32),
    manual: Option<Rect>,
    shown: Option<Rect>,
) -> Option<Rect> {
    snap_target(canvas, screens, global_point, manual.or(shown))
}

/// 7 DIP 命中边距；不把选择范围本身扩大或移动。
pub(crate) fn hit_preview(rect: Rect, point: (i32, i32), scale: f32) -> bool {
    if rect.width == 0 || rect.height == 0 || !scale.is_finite() || scale <= 0. {
        return false;
    }
    let pad = 7. * f64::from(scale);
    let (x, y) = (f64::from(point.0), f64::from(point.1));
    x >= f64::from(rect.x) - pad
        && x <= f64::from(rect.x) + f64::from(rect.width) + pad
        && y >= f64::from(rect.y) - pad
        && y <= f64::from(rect.y) + f64::from(rect.height) + pad
}

/// 超过 5 DIP 圆形容差才重框；坐标差先拓宽，原点可为负。
pub(crate) fn preview_dragged(start: (i32, i32), point: (i32, i32), scale: f32) -> bool {
    if !scale.is_finite() || scale <= 0. {
        return true;
    }
    let dx = (i64::from(point.0) - i64::from(start.0)) as f64;
    let dy = (i64::from(point.1) - i64::from(start.1)) as f64;
    let threshold = 5. * f64::from(scale);
    dx * dx + dy * dy > threshold * threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(left: i32, top: i32, width: u32, height: u32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn snap_uses_negative_physical_monitor_then_full_then_new_monitor() {
        let canvas = screen(-1920, -240, 4480, 1680);
        let screens = [screen(-1920, -240, 1920, 1080), screen(0, 0, 2560, 1440)];
        let first = snap_target(canvas, &screens, (-1700, -100), None).unwrap();
        assert_eq!(first, Rect::full(1920, 1080));
        let full = snap_target(canvas, &screens, (-1700, -100), Some(first)).unwrap();
        assert_eq!(full, Rect::full(4480, 1680));
        assert_eq!(
            snap_target(canvas, &screens, (-1700, -100), Some(full)),
            Some(first)
        );
        assert_eq!(
            snap_target(canvas, &screens, (250, 300), Some(full)),
            Some(Rect {
                x: 1920,
                y: 240,
                width: 2560,
                height: 1440
            })
        );
    }

    #[test]
    fn shown_preview_each_field_within_one_pixel_toggles_but_two_pixels_do_not() {
        let canvas = screen(-1000, -500, 3000, 2000);
        let screens = [screen(-400, 100, 800, 600)];
        let monitor = Rect {
            x: 600,
            y: 600,
            width: 800,
            height: 600,
        };
        for field in 0..4 {
            for delta in [-2_i64, -1, 1, 2] {
                let mut fields = [600_i64, 600, 800, 600];
                fields[field] += delta;
                let shown = Rect {
                    x: fields[0] as u32,
                    y: fields[1] as u32,
                    width: fields[2] as u32,
                    height: fields[3] as u32,
                };
                let expected = if delta.abs() <= 1 {
                    Rect::full(canvas.width, canvas.height)
                } else {
                    monitor
                };
                assert_eq!(
                    snap_target(canvas, &screens, (-200, 300), Some(shown)),
                    Some(expected),
                    "field {field}, delta {delta}"
                );
            }
        }
        assert!(!same_selection(Rect::full(0, 10), Rect::full(1, 10)));
        assert!(!same_selection(
            Rect {
                x: u32::MAX,
                ..monitor
            },
            monitor
        ));
        assert_eq!(
            snap_target(
                canvas,
                &screens,
                (-200, 300),
                Some(Rect {
                    x: 601,
                    y: 599,
                    width: 801,
                    height: 599
                })
            ),
            Some(Rect::full(3000, 2000))
        );
    }

    #[test]
    fn monitor_and_full_within_one_pixel_stay_monitor_but_two_pixels_allow_full() {
        let canvas = screen(-10, -20, 1000, 800);
        let full = Rect::full(1000, 800);
        for monitor in [
            full,
            Rect {
                x: 1,
                y: 0,
                width: 999,
                height: 800,
            },
            Rect {
                x: 0,
                y: 1,
                width: 1000,
                height: 799,
            },
            Rect {
                x: 1,
                y: 1,
                width: 999,
                height: 799,
            },
        ] {
            let screens = [screen(
                canvas.left + monitor.x as i32,
                canvas.top + monitor.y as i32,
                monitor.width,
                monitor.height,
            )];
            assert_eq!(
                snap_target(canvas, &screens, (50, 50), Some(monitor)),
                Some(monitor)
            );
        }
        assert_eq!(
            snap_target(
                canvas,
                &[screen(-10, -20, 998, 800)],
                (50, 50),
                Some(Rect::full(998, 800))
            ),
            Some(full)
        );
    }

    #[test]
    fn single_screen_gap_nearest_and_clipping_keep_checked_ranges() {
        let canvas = screen(-100, -50, 800, 600);
        assert_eq!(
            snap_target(canvas, &[canvas], (20, 30), Some(Rect::full(800, 600))),
            Some(Rect::full(800, 600))
        );
        let canvas = screen(-500, 0, 1500, 600);
        let screens = [screen(-500, 0, 400, 600), screen(100, 0, 900, 600)];
        assert_eq!(
            snap_target(canvas, &screens, (50, 300), None),
            Some(Rect {
                x: 600,
                y: 0,
                width: 900,
                height: 600
            })
        );
        assert_eq!(
            snap_target(
                screen(0, 0, 100, 80),
                &[screen(-20, -10, 90, 60)],
                (5, 5),
                None
            ),
            Some(Rect::full(70, 50))
        );
        assert_eq!(
            snap_target(
                screen(i32::MIN, 0, 20, 20),
                &[screen(i32::MIN, 0, 20, 20)],
                (i32::MAX, 0),
                None
            ),
            Some(Rect::full(20, 20))
        );
    }

    #[test]
    fn invalid_geometry_and_missing_monitors_never_create_a_range() {
        let canvas = screen(0, 0, 100, 80);
        for invalid in [
            screen(0, 0, 0, 80),
            screen(i32::MAX, 0, 2, 80),
            screen(0, 0, u32::MAX, 80),
        ] {
            assert_eq!(snap_target(invalid, &[canvas], (5, 5), None), None);
            assert_eq!(snap_target(canvas, &[invalid], (5, 5), None), None);
        }
        assert_eq!(snap_target(canvas, &[], (5, 5), None), None);
        assert_eq!(
            snap_target(canvas, &[screen(1000, 1000, 30, 30)], (5, 5), None),
            None
        );
        assert_eq!(
            snap_target(
                canvas,
                &[canvas],
                (5, 5),
                Some(Rect {
                    x: u32::MAX,
                    y: 0,
                    width: 10,
                    height: 10
                })
            ),
            Some(Rect::full(100, 80))
        );
    }

    #[test]
    fn repeated_pending_snap_uses_current_manual_range_before_the_old_frame() {
        let canvas = screen(-1920, -240, 4480, 1680);
        let screens = [screen(-1920, -240, 1920, 1080), screen(0, 0, 2560, 1440)];
        let monitor = Rect::full(1920, 1080);
        let point = (-1700, -100);
        let full =
            snap_preview_target(canvas, &screens, point, Some(monitor), Some(monitor)).unwrap();
        assert_eq!(full, Rect::full(4480, 1680));
        assert_eq!(
            snap_preview_target(canvas, &screens, point, Some(full), Some(monitor)),
            Some(monitor)
        );
        assert_eq!(
            snap_preview_target(canvas, &screens, point, None, Some(monitor)),
            Some(full)
        );
        assert_eq!(
            snap_preview_target(canvas, &screens, point, None, None),
            Some(monitor)
        );
    }

    #[test]
    fn preview_hit_and_drag_use_dip_tolerance_without_altering_the_range() {
        let rect = Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        };
        assert!(hit_preview(rect, (93, 73), 1.));
        assert!(!hit_preview(rect, (92, 73), 1.));
        assert!(hit_preview(rect, (86, 66), 2.));
        assert!(!hit_preview(rect, (85, 66), 2.));
        assert!(!preview_dragged((-100, -100), (-97, -96), 1.));
        assert!(preview_dragged((-100, -100), (-96, -96), 1.));
        assert!(!preview_dragged((0, 0), (6, 8), 2.));
        assert!(preview_dragged((0, 0), (7, 8), 2.));
        assert!(preview_dragged((i32::MIN, 0), (i32::MAX, 0), 1.));
        for scale in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(!hit_preview(rect, (100, 80), scale));
            assert!(preview_dragged((0, 0), (0, 0), scale));
        }
        assert_eq!(
            rect,
            Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100
            }
        );
    }
}
