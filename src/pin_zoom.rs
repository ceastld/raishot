//! 贴图统一使用原图像素到客户区物理像素的投影；窗口定位只在最后舍入。
use crate::{annotation::Point, pin_model::Placement, viewport::Viewport};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZoomAnchor {
    TopLeft,
    #[default]
    Pointer,
    Center,
}
impl ZoomAnchor {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::Pointer => "pointer",
            Self::Center => "center",
        }
    }
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        [Self::TopLeft, Self::Pointer, Self::Center]
            .into_iter()
            .find(|anchor| anchor.wire_name().eq_ignore_ascii_case(value))
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => tr!("图片左上角", "Image top-left"),
            Self::Pointer => tr!("鼠标位置", "Pointer"),
            Self::Center => tr!("图片中心", "Image center"),
        }
    }
}

/// 同进程窗口在外层借用结束后同步；其它贴图工作进程在激活/鼠标进入或普通滚轮时刷新。
pub(crate) fn sync_open_preferences(anchor: ZoomAnchor, cx: &mut gpui::App) {
    for window in cx.windows() {
        if let Some(pin) = window.downcast::<crate::pin_view::PinView>() {
            let _ = pin.update(cx, |v, _, cx| {
                if v.pin_zoom_anchor != anchor {
                    v.pin_zoom_anchor = anchor;
                    v.reset_zoom_wheel();
                    cx.notify();
                }
            });
        } else if let Some(settings) = window.downcast::<crate::settings_view::SettingsView>() {
            let _ = settings.update(cx, |v, _, cx| {
                v.preferences.pin_zoom_anchor = anchor;
                cx.notify();
            });
        }
    }
}

/// 按 10 个百分点调透明度；byte 先恢复显示百分比，保留菜单 25/75% 的中点取偶语义。
pub(crate) fn next_opacity(current: u8, steps: i32) -> u8 {
    if steps == 0 {
        return current;
    }
    let percentage = (f64::from(current) * 100. / 255.).round();
    let index = (percentage / 10.).round_ties_even() as i64;
    let next = index.saturating_add(steps as i64).clamp(1, 10);
    (next as f64 * 255. / 10.).round_ties_even() as u8
}

/// 居中 contain 投影；允许放大，小于 10% 的长图初始适应也不截断。
pub(crate) fn projection(image: (u32, u32), host: (u32, u32)) -> Viewport {
    let (width, height) = (image.0.max(1) as f32, image.1.max(1) as f32);
    let (host_width, host_height) = (host.0.max(1) as f32, host.1.max(1) as f32);
    let zoom = (host_width / width).min(host_height / height);
    Viewport {
        zoom,
        offset: Point {
            x: (host_width - width * zoom) / 2.,
            y: (host_height - height * zoom) / 2.,
        },
    }
}

/// Ctrl+Shift+滚轮按 1 个百分点精细调整，范围同普通滚轮（10% 到 100%）。
pub(crate) fn next_opacity_fine(current: u8, steps: i32) -> u8 {
    if steps == 0 {
        return current;
    }
    let percentage = (f64::from(current) * 100. / 255.).round() as i64;
    let next = percentage.saturating_add(steps as i64).clamp(10, 100);
    (next as f64 * 255. / 100.).round_ties_even() as u8
}

/// 10 个百分点刻度；普通滚轮范围为 10% 到 500%。
pub(crate) fn next_zoom(current: f32, steps: i32) -> f32 {
    next_zoom_by(current, steps, 10)
}

/// Shift+滚轮按 1 个百分点精细缩放，范围同普通滚轮。
pub(crate) fn next_zoom_fine(current: f32, steps: i32) -> f32 {
    next_zoom_by(current, steps, 1)
}

fn next_zoom_by(current: f32, steps: i32, percent: i64) -> f32 {
    let current = if current.is_finite() && current > 0. {
        current
    } else {
        1.
    };
    if steps == 0 {
        return current;
    }
    // 超长图的初始适应可能低于普通下限；向外滚不能反而把图片放大。
    if steps < 0 && current < 0.1 {
        return current;
    }
    // 拖边、适应屏幕或精细调整留下的非刻度倍率先沿滚动方向对齐到相邻刻度；
    // 余量吸收 1.1 这类倍率乘回刻度的浮点误差。使用宽整数避免高分辨率滚轮累计溢出。
    let position = f64::from(current) * 100. / percent as f64;
    let index = if steps > 0 {
        (position + 1e-3).floor()
    } else {
        (position - 1e-3).ceil()
    } as i64;
    let next = index
        .saturating_add(steps as i64)
        .clamp(10 / percent, 500 / percent);
    (next * percent) as f32 / 100.
}

/// anchor 为旧客户区物理坐标；在含留白的长窄窗口中也保持同一原图点。
pub(crate) fn anchored(
    old: Placement,
    image_size: (u32, u32),
    next: (u32, u32),
    anchor: (f32, f32),
) -> Placement {
    let before = projection(image_size, (old.width, old.height));
    let after = projection(image_size, next);
    let anchor = Point {
        x: if anchor.0.is_finite() {
            anchor.0
        } else {
            before.offset.x
        },
        y: if anchor.1.is_finite() {
            anchor.1
        } else {
            before.offset.y
        },
    };
    let image_point = before.image_point(anchor);
    let target = after.window_point(image_point);
    let shifted = |origin: i32, from: f32, to: f32| {
        (origin as f64 + from as f64 - to as f64)
            .round()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32
    };
    Placement {
        x: shifted(old.x, anchor.x, target.x),
        y: shifted(old.y, anchor.y, target.y),
        width: next.0.max(1),
        height: next.1.max(1),
    }
}

/// 连续左上角缩放复用最初的屏幕锚点，不能把每次整数定位的余量带入下一次。
#[derive(Clone, Copy, Debug)]
pub(crate) struct TopLeftAnchor {
    screen: (f64, f64),
    expected: Placement,
    local: Point,
}

impl TopLeftAnchor {
    pub(crate) fn place(
        previous: Option<Self>,
        old: Placement,
        before: Point,
        next: (u32, u32),
        after: Point,
    ) -> (Placement, Self) {
        let screen = previous
            .filter(|anchor| anchor.expected == old && anchor.local == before)
            .map_or_else(
                || {
                    (
                        f64::from(old.x) + f64::from(before.x),
                        f64::from(old.y) + f64::from(before.y),
                    )
                },
                |anchor| anchor.screen,
            );
        let next = (next.0.max(1), next.1.max(1));
        // 相同几何保持原窗口；先将大屏幕坐标转成 f32 会丢失整数像素。
        let placement = if next == (old.width, old.height) && before == after {
            old
        } else {
            let shifted = |origin: f64, local: f32| {
                (origin - f64::from(local))
                    .round()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
            };
            Placement {
                x: shifted(screen.0, after.x),
                y: shifted(screen.1, after.y),
                width: next.0,
                height: next.1,
            }
        };
        (
            placement,
            Self {
                screen,
                expected: placement,
                local: after,
            },
        )
    }
}

/// 鼠标未移动时保留最初的原图点，避免整数 HWND 定位的半像素误差逐档累积。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PointerAnchor {
    screen: (f64, f64),
    image: (f64, f64),
    expected: Placement,
    viewport: Viewport,
}

impl PointerAnchor {
    pub(crate) fn place(
        previous: Option<Self>,
        old: Placement,
        before: Viewport,
        next: (u32, u32),
        after: Viewport,
        pointer: (f32, f32),
    ) -> (Placement, Self) {
        let local = (
            if pointer.0.is_finite() {
                pointer.0
            } else {
                before.offset.x
            },
            if pointer.1.is_finite() {
                pointer.1
            } else {
                before.offset.y
            },
        );
        let screen = (
            f64::from(old.x) + f64::from(local.0),
            f64::from(old.y) + f64::from(local.1),
        );
        let (screen, image) = previous
            .filter(|anchor| {
                // 物理坐标经过 DIP 往返后可能差几个 f32 ULP；容差远小于一个鼠标像素。
                anchor.expected == old
                    && anchor.viewport == before
                    && (anchor.screen.0 - screen.0).abs() <= 0.01
                    && (anchor.screen.1 - screen.1).abs() <= 0.01
            })
            .map_or_else(
                || {
                    (
                        screen,
                        (
                            (f64::from(local.0) - f64::from(before.offset.x))
                                / f64::from(before.zoom),
                            (f64::from(local.1) - f64::from(before.offset.y))
                                / f64::from(before.zoom),
                        ),
                    )
                },
                // 屏幕锚点和原图点一起复用，不能把本次的微小误差带入下一档。
                |anchor| (anchor.screen, anchor.image),
            );
        let next = (next.0.max(1), next.1.max(1));
        let placement = if next == (old.width, old.height) && before == after {
            old
        } else {
            let shifted = |screen: f64, image: f64, offset: f32| {
                (screen - f64::from(offset) - image * f64::from(after.zoom))
                    .round()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
            };
            Placement {
                x: shifted(screen.0, image.0, after.offset.x),
                y: shifted(screen.1, image.1, after.offset.y),
                width: next.0,
                height: next.1,
            }
        };
        (
            placement,
            Self {
                screen,
                image,
                expected: placement,
                viewport: after,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.00001,
            "实际{actual}，预期{expected}"
        );
    }

    #[test]
    fn pin_zoom_pointer_roundtrips_keep_the_original_image_point() {
        let image = (24, 35);
        let initial = Placement {
            x: 100,
            y: 100,
            width: image.0,
            height: image.1,
        };
        let scales = [
            0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.,
        ];
        let screen = (112, 118);
        let mut legacy = initial;
        for _ in 0..4 {
            for zoom in scales {
                legacy = anchored(
                    legacy,
                    image,
                    crate::pin_model::size(image, zoom),
                    ((screen.0 - legacy.x) as f32, (screen.1 - legacy.y) as f32),
                );
            }
        }
        assert_ne!(legacy, initial, "夹具应覆盖旧算法的逐档量化漂移");

        for origin in [(100, 100), (-30_000, -7000)] {
            let initial = Placement {
                x: origin.0,
                y: origin.1,
                ..initial
            };
            let screen = (origin.0 + 12, origin.1 + 18);
            let mut placement = initial;
            let mut anchor = None;
            for _ in 0..128 {
                for zoom in scales {
                    let before = projection(image, (placement.width, placement.height));
                    let next = crate::pin_model::size(image, zoom);
                    let after = projection(image, next);
                    let (placed, retained) = PointerAnchor::place(
                        anchor,
                        placement,
                        before,
                        next,
                        after,
                        (
                            (screen.0 - placement.x) as f32,
                            (screen.1 - placement.y) as f32,
                        ),
                    );
                    for (origin, image, offset, screen) in [
                        (placed.x, retained.image.0, after.offset.x, screen.0),
                        (placed.y, retained.image.1, after.offset.y, screen.1),
                    ] {
                        let error =
                            (f64::from(origin) + f64::from(offset) + image * f64::from(after.zoom)
                                - f64::from(screen))
                            .abs();
                        assert!(error <= 0.50001, "鼠标下图像点偏移 {error}px");
                    }
                    placement = placed;
                    anchor = Some(retained);
                }
                assert_eq!(placement, initial);
            }
        }
    }

    #[test]
    fn pin_zoom_pointer_rebuilds_after_pointer_move_placement_or_projection_change() {
        let old = Placement {
            x: -700,
            y: 320,
            width: 101,
            height: 59,
        };
        let before = projection((101, 59), (old.width, old.height));
        let after = projection((101, 59), (151, 89));
        let (placed, retained) =
            PointerAnchor::place(None, old, before, (151, 89), after, (31., 21.));
        let pointer = (
            (retained.screen.0 - f64::from(placed.x)) as f32,
            (retained.screen.1 - f64::from(placed.y)) as f32,
        );
        for (actual, viewport, pointer) in [
            (placed, after, (pointer.0 + 0.25, pointer.1)),
            (
                Placement {
                    x: placed.x + 1,
                    ..placed
                },
                after,
                pointer,
            ),
            (
                placed,
                Viewport {
                    offset: Point {
                        x: after.offset.x + 0.5,
                        ..after.offset
                    },
                    ..after
                },
                pointer,
            ),
        ] {
            let reused = PointerAnchor::place(
                Some(retained),
                actual,
                viewport,
                (201, 117),
                before,
                pointer,
            );
            let fresh = PointerAnchor::place(None, actual, viewport, (201, 117), before, pointer);
            assert_eq!(reused.0, fresh.0);
            assert_eq!(reused.1.image, fresh.1.image);
        }
    }

    #[test]
    fn pin_zoom_pointer_fractional_dpi_roundtrips_retain_screen_and_image_without_drift() {
        let image = (101, 59);
        let scales = [
            1.1, 1.2, 1.3, 1.4, 1.5, 1.4, 1.3, 1.2, 1.1, 1., 0.9, 0.8, 0.7, 0.8, 0.9, 1.,
        ];
        let dpi = 1.75_f32;
        for origin in [(100, 100), (-30_000, -7000)] {
            for local in [(31., 57.), (62., 31.), (31.125, 57.5)] {
                let initial = Placement {
                    x: origin.0,
                    y: origin.1,
                    width: image.0,
                    height: image.1,
                };
                let viewport = projection(image, image);
                let (_, first) =
                    PointerAnchor::place(None, initial, viewport, image, viewport, local);
                let mut anchor = Some(first);
                let mut placement = initial;
                let mut saw_dpi_rounding = false;
                for _ in 0..128 {
                    for zoom in scales {
                        let before = projection(image, (placement.width, placement.height));
                        let next = crate::pin_model::size(image, zoom);
                        let after = projection(image, next);
                        let physical = (
                            (first.screen.0 - f64::from(placement.x)) as f32,
                            (first.screen.1 - f64::from(placement.y)) as f32,
                        );
                        let pointer = (physical.0 / dpi * dpi, physical.1 / dpi * dpi);
                        saw_dpi_rounding |= pointer != physical;
                        let (placed, retained) =
                            PointerAnchor::place(anchor, placement, before, next, after, pointer);
                        assert_eq!(retained.screen, first.screen);
                        assert_eq!(retained.image, first.image);
                        placement = placed;
                        anchor = Some(retained);
                    }
                    assert_eq!(placement, initial);
                }
                assert!(saw_dpi_rounding, "夹具应包含 175% DPI 坐标往返误差");
            }
        }
    }

    #[test]
    fn pin_zoom_pointer_same_geometry_preserves_large_integer_screen_coordinates() {
        for origin in [(i32::MAX - 101, i32::MIN + 59), (-30_000, -7000)] {
            let old = Placement {
                x: origin.0,
                y: origin.1,
                width: 101,
                height: 59,
            };
            let viewport = projection((101, 59), (101, 59));
            let (placed, anchor) =
                PointerAnchor::place(None, old, viewport, (101, 59), viewport, (31., 21.));
            assert_eq!(placed, old);
            assert_eq!(
                PointerAnchor::place(Some(anchor), old, viewport, (101, 59), viewport, (31., 21.),)
                    .0,
                old
            );
        }
    }

    #[test]
    fn pin_zoom_preferences_opacity_matches_ten_percent_ticks_and_menu_midpoints() {
        assert_eq!(next_opacity(255, -1), 230);
        assert_eq!(next_opacity(230, -1), 204);
        assert_eq!(next_opacity(204, 1), 230);
        assert_eq!(next_opacity(191, 1), 230);
        assert_eq!(next_opacity(64, 1), 76);
        assert_eq!(next_opacity(26, -1), 26);
        assert_eq!(next_opacity(255, i32::MAX), 255);
        assert_eq!(next_opacity(128, i32::MIN), 26);
        assert_eq!(next_opacity(191, 0), 191);
    }

    #[test]
    fn pin_fine_wheel_steps_one_percent_within_the_same_ranges() {
        assert_eq!(next_opacity_fine(255, -1), 252);
        assert_eq!(next_opacity_fine(204, 1), 207);
        assert_eq!(next_opacity_fine(26, -1), 26);
        assert_eq!(next_opacity_fine(255, 1), 255);
        assert_eq!(next_opacity_fine(128, i32::MIN), 26);
        assert_eq!(next_opacity_fine(191, 0), 191);
        assert_eq!(next_zoom_fine(1., 1), 1.01);
        assert_eq!(next_zoom_fine(1., -1), 0.99);
        assert_eq!(next_zoom_fine(1.1, 1), 1.11);
        assert_eq!(next_zoom_fine(0.1, -1), 0.1);
        assert_eq!(next_zoom_fine(5., 1), 5.);
        assert_eq!(next_zoom_fine(0.015, -1), 0.015);
        assert_eq!(next_zoom_fine(0.015, 1), 0.1);
        assert_eq!(next_zoom_fine(1.234, 1), 1.24);
        assert_eq!(next_zoom_fine(1.234, -1), 1.23);
        // 精细调整后的普通滚轮沿方向回到相邻 10% 刻度，不跳过一档。
        assert_eq!(next_zoom(1.27, 1), 1.3);
        assert_eq!(next_zoom(1.27, -1), 1.2);
        assert_eq!(next_zoom(1.23, 1), 1.3);
        let mut zoom = 1.;
        for _ in 0..7 {
            zoom = next_zoom_fine(zoom, 1);
        }
        assert_eq!(zoom, 1.07);
    }

    #[test]
    fn pin_zoom_projection_centers_letterbox_and_can_magnify() {
        let view = projection((100, 100), (250, 125));
        assert_eq!(view.zoom, 1.25);
        assert_eq!(view.offset, Point { x: 62.5, y: 0. });
        assert_eq!(
            view.window_point(Point { x: 100., y: 100. }),
            Point { x: 187.5, y: 125. }
        );
        assert_eq!(
            view.image_point(Point { x: 125., y: 62.5 }),
            Point { x: 50., y: 50. }
        );
        let portrait = projection((100, 200), (100, 500));
        assert_eq!(portrait.zoom, 1.);
        assert_eq!(portrait.offset, Point { x: 0., y: 150. });
        let magnified = projection((100, 50), (500, 250));
        assert_eq!(magnified.zoom, 5.);
        assert_eq!(
            magnified.window_point(Point { x: 20., y: 10. }),
            Point { x: 100., y: 50. }
        );
        let empty = projection((0, 0), (0, 0));
        assert_eq!(empty, Viewport::default());
        let zero_width = projection((0, 100), (0, 0));
        assert!(zero_width.zoom.is_finite() && zero_width.zoom > 0.);
        assert!(zero_width.offset.x.is_finite() && zero_width.offset.y.is_finite());
    }

    #[test]
    fn pin_zoom_wheel_uses_percentage_steps_and_preserves_initial_long_fit() {
        assert_eq!(next_zoom(1., 1), 1.1);
        assert_eq!(next_zoom(2., 1), 2.1);
        assert_eq!(next_zoom(1., -1), 0.9);
        assert_eq!(next_zoom(0.1, -1), 0.1);
        assert_eq!(next_zoom(5., 1), 5.);
        assert_eq!(next_zoom(0.25, 1), 0.3);
        assert_eq!(next_zoom(0.25, -1), 0.2);
        assert_eq!(next_zoom(0.015, 1), 0.1);
        assert_eq!(next_zoom(0.015, -1), 0.015);
        assert_eq!(next_zoom(0.015, 0), 0.015);
        assert_eq!(next_zoom(f32::NAN, 1), 1.1);
        assert_eq!(next_zoom(f32::INFINITY, -1), 0.9);
        assert_eq!(next_zoom(-2., 1), 1.1);
        assert_eq!(next_zoom(1., i32::MAX), 5.);
        assert_eq!(next_zoom(1., i32::MIN), 0.1);
        let mut zoom = 0.1;
        for _ in 0..49 {
            zoom = next_zoom(zoom, 1);
        }
        assert_eq!(zoom, 5.);
        for _ in 0..49 {
            zoom = next_zoom(zoom, -1);
        }
        assert_eq!(zoom, 0.1);
    }

    #[test]
    fn pin_zoom_one_hundred_percent_is_physical_pixels_at_125_percent_dpi() {
        let source_point = Point { x: 200., y: 150. };
        let one_to_one = projection((800, 600), (800, 600));
        assert_eq!(one_to_one.window_point(source_point), source_point);
        assert_eq!(one_to_one.image_point(source_point), source_point);
        let physical = one_to_one.window_point(source_point);
        // GPUI 在边界把物理坐标除以 DPI，125% 系统缩放不会再放大一次底图。
        assert_eq!((physical.x / 1.25, physical.y / 1.25), (160., 120.));
        let doubled = projection((800, 600), (1600, 1200)).window_point(source_point);
        assert_eq!((doubled.x / 1.25, doubled.y / 1.25), (320., 240.));
    }

    #[test]
    fn pin_zoom_long_narrow_letterbox_preserves_image_edge_in_negative_screen() {
        let old = Placement {
            x: -1000,
            y: -500,
            width: 24,
            height: 3200,
        };
        let view = projection((4, 32000), (24, 3200));
        close(view.zoom, 0.1);
        close(view.offset.x, 11.8);
        let top_left = anchored(old, (4, 32000), (24, 16000), (11.8, 0.));
        assert_eq!(
            top_left,
            Placement {
                x: -999,
                y: -500,
                width: 24,
                height: 16000
            }
        );
        let lower_left = anchored(old, (4, 32000), (24, 16000), (11.8, 3200.));
        assert_eq!(
            lower_left,
            Placement {
                x: -999,
                y: -13300,
                width: 24,
                height: 16000
            }
        );
        let center = anchored(old, (4, 32000), (24, 16000), (12., 1600.));
        assert_eq!(
            center,
            Placement {
                x: -1000,
                y: -6900,
                width: 24,
                height: 16000
            }
        );
    }

    #[test]
    fn pin_zoom_all_anchor_locations_stay_within_half_a_physical_pixel() {
        let cases = [
            ((601, 359), (601, 359), (661, 395)),
            ((100, 100), (250, 125), (173, 287)),
            ((4, 32000), (24, 3200), (24, 16000)),
            ((32000, 4), (3200, 24), (16000, 24)),
            ((65535, 64), (400, 24), (16384, 160)),
            ((64, 65535), (24, 400), (160, 16384)),
            ((4096, 2160), (410, 216), (16384, 8640)),
        ];
        for (image, host, next) in cases {
            let before = projection(image, host);
            let old = Placement {
                x: -30000,
                y: 7000,
                width: host.0,
                height: host.1,
            };
            for source in [
                Point { x: 0., y: 0. },
                Point {
                    x: image.0 as f32,
                    y: 0.,
                },
                Point {
                    x: 0.,
                    y: image.1 as f32,
                },
                Point {
                    x: image.0 as f32,
                    y: image.1 as f32,
                },
                Point {
                    x: image.0 as f32 / 2.,
                    y: image.1 as f32 / 2.,
                },
                Point {
                    x: image.0 as f32 * 0.173,
                    y: image.1 as f32 * 0.617,
                },
            ] {
                let anchor = before.window_point(source);
                let moved = anchored(old, image, next, (anchor.x, anchor.y));
                let target = projection(image, next).window_point(source);
                for (old_origin, local, new_origin, new_local) in [
                    (old.x, anchor.x, moved.x, target.x),
                    (old.y, anchor.y, moved.y, target.y),
                ] {
                    let error =
                        (old_origin as f64 + local as f64 - new_origin as f64 - new_local as f64)
                            .abs();
                    assert!(error <= 0.51, "image={image:?},host={host:?},next={next:?},source={source:?}，锚点偏移{error}px");
                }
            }
        }
    }

    #[test]
    fn pin_zoom_top_left_repeated_roundtrips_do_not_accumulate_rounding() {
        let image = (42, 945);
        let initial = Placement {
            x: 100,
            y: 100,
            width: image.0,
            height: image.1,
        };
        let scales = [1.1, 1.2, 1.3, 1.4, 1.5, 1.4, 1.3, 1.2, 1.1, 1.];
        // 旧算法只保证单次误差；完整往返后仍会向下漂移两个像素。
        let mut legacy = initial;
        for zoom in scales {
            let before = projection(image, (legacy.width, legacy.height)).offset;
            legacy = anchored(
                legacy,
                image,
                crate::pin_model::size(image, zoom),
                (before.x, before.y),
            );
        }
        assert_eq!(legacy.y, 102);

        for origin in [(100, 100), (-30_000, -7000)] {
            let initial = Placement {
                x: origin.0,
                y: origin.1,
                ..initial
            };
            let mut placement = initial;
            let mut anchor = None;
            for _ in 0..128 {
                for zoom in scales {
                    let before = projection(image, (placement.width, placement.height)).offset;
                    let next = crate::pin_model::size(image, zoom);
                    let after = projection(image, next).offset;
                    let (placed, retained) =
                        TopLeftAnchor::place(anchor, placement, before, next, after);
                    assert!(
                        (f64::from(placed.x) + f64::from(after.x) - f64::from(initial.x)).abs()
                            <= 0.5
                    );
                    assert!(
                        (f64::from(placed.y) + f64::from(after.y) - f64::from(initial.y)).abs()
                            <= 0.5
                    );
                    placement = placed;
                    anchor = Some(retained);
                }
                assert_eq!(placement, initial);
            }
        }
    }

    #[test]
    fn pin_zoom_top_left_odd_thin_and_finished_images_keep_the_original_screen_anchor() {
        use crate::{pin_presentation_host, selection_finish::SelectionFinishStyle};

        let border = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 8.,
            ..Default::default()
        });
        let shadow = Some(SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 7.25,
            ..Default::default()
        });
        for (content, style) in [
            ((601, 359), None),
            ((43, 945), border),
            ((431, 947), shadow),
            ((4, 32000), None),
            ((32000, 4), None),
        ] {
            let outer = pin_presentation_host::window_size(content, style).unwrap();
            let host = crate::pin_model::size(outer, 1.);
            let initial = Placement {
                x: -30_000,
                y: 7000,
                width: host.0,
                height: host.1,
            };
            let initial_local =
                pin_presentation_host::content_viewport(content, host, style).offset;
            let screen = (
                f64::from(initial.x) + f64::from(initial_local.x),
                f64::from(initial.y) + f64::from(initial_local.y),
            );
            let mut placement = initial;
            let mut anchor = None;
            for _ in 0..32 {
                for zoom in [0.1, 0.2, 0.3, 0.4, 0.5, 1.1, 1.2, 1.3, 1.4, 1.5, 5., 1.] {
                    let before = pin_presentation_host::content_viewport(
                        content,
                        (placement.width, placement.height),
                        style,
                    )
                    .offset;
                    let next = crate::pin_model::size(outer, zoom);
                    let after =
                        pin_presentation_host::content_viewport(content, next, style).offset;
                    let (placed, retained) =
                        TopLeftAnchor::place(anchor, placement, before, next, after);
                    assert!((f64::from(placed.x) + f64::from(after.x) - screen.0).abs() <= 0.5);
                    assert!((f64::from(placed.y) + f64::from(after.y) - screen.1).abs() <= 0.5);
                    placement = placed;
                    anchor = Some(retained);
                }
                assert_eq!(placement, initial, "content={content:?},style={style:?}");
            }
        }
    }

    #[test]
    fn pin_zoom_top_left_rebuilds_after_move_resize_or_local_origin_change() {
        let old = Placement {
            x: -1000,
            y: 500,
            width: 101,
            height: 59,
        };
        let before = Point { x: 0.25, y: 0.75 };
        let after = Point { x: 0.75, y: 0.25 };
        let (placed, retained) = TopLeftAnchor::place(None, old, before, (121, 71), after);
        assert_eq!(retained.screen, (-999.75, 500.75));
        let replacements = [
            (
                Placement {
                    x: placed.x + 17,
                    ..placed
                },
                after,
            ),
            (
                Placement {
                    width: placed.width + 1,
                    ..placed
                },
                after,
            ),
            (
                placed,
                Point {
                    x: after.x + 0.5,
                    ..after
                },
            ),
        ];
        for (actual, local) in replacements {
            let (placed, rebuilt) =
                TopLeftAnchor::place(Some(retained), actual, local, (141, 83), before);
            assert_eq!(
                rebuilt.screen,
                (
                    f64::from(actual.x) + f64::from(local.x),
                    f64::from(actual.y) + f64::from(local.y)
                )
            );
            assert!((f64::from(placed.x) + f64::from(before.x) - rebuilt.screen.0).abs() <= 0.5);
            assert!((f64::from(placed.y) + f64::from(before.y) - rebuilt.screen.1).abs() <= 0.5);
        }
    }

    #[test]
    fn pin_zoom_top_left_same_geometry_preserves_large_integer_screen_coordinates() {
        for (x, y) in [(20_000_001, -20_000_001), (i32::MIN, i32::MAX)] {
            let old = Placement {
                x,
                y,
                width: 101,
                height: 59,
            };
            let local = Point { x: 0.25, y: 0.75 };
            let (placed, anchor) = TopLeftAnchor::place(None, old, local, (101, 59), local);
            assert_eq!(placed, old);
            assert_eq!(anchor.screen, (f64::from(x) + 0.25, f64::from(y) + 0.75));
            assert_eq!(
                TopLeftAnchor::place(Some(anchor), old, local, (101, 59), local).0,
                old
            );
        }
    }
}
