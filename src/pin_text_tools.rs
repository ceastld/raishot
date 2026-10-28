//! 选字操作条只占客户区与显示器工作区的交集；无空位时保留菜单入口。
//! 输入选择保持全图物理坐标，输出布局使用客户区 DIP，不读取窗口或屏幕。
use crate::{
    annotation::{Bounds, Point},
    pin_model::Placement,
    toolbar_layout::PanelRect,
    viewport::Viewport,
};

pub(super) const FULL_WIDTH: f32 = 286.;
pub(super) const FULL_HEIGHT: f32 = 38.;
pub(super) const COMPACT_WIDTH: f32 = 180.;
pub(super) const COMPACT_HEIGHT: f32 = 64.;
#[cfg(test)]
const GAP: f64 = 8.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct StripLayout {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
    pub(super) height: f32,
    pub(super) compact: bool,
}

struct Area {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}
impl Area {
    fn intersects(&self, other: &Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }
}

/// 独立工具栏的真实全局物理矩形投到贴图客户区 DIP，只在宿主确认可见后调用。
pub(super) fn client_obstacle(
    physical: Placement,
    client_origin: (i32, i32),
    dpi: f32,
) -> Option<PanelRect> {
    if !dpi.is_finite() || dpi <= 0. || physical.width == 0 || physical.height == 0 {
        return None;
    }
    let result = PanelRect {
        x: (i64::from(physical.x) - i64::from(client_origin.0)) as f32 / dpi,
        y: (i64::from(physical.y) - i64::from(client_origin.1)) as f32 / dpi,
        width: physical.width as f32 / dpi,
        height: physical.height as f32 / dpi,
    };
    [
        result.x,
        result.y,
        result.width,
        result.height,
        result.right(),
        result.bottom(),
    ]
    .iter()
    .all(|value| value.is_finite())
    .then_some(result)
}

/// `viewport` 的 zoom/offset 是客户区物理像素；`host` 与 `work` 已是客户区 DIP。
/// 全局负屏幕原点由调用者投影到 `work`，这里不假定工作区原点为零。
/// 完整布局优先下方、再上方；不适配时同序尝试紧凑布局，始终避开整个选择 AABB。
#[cfg(test)]
pub(super) fn strip_layout(
    quads: &[[Point; 4]],
    viewport: Viewport,
    dpi: f32,
    host: (f32, f32),
    work: Bounds,
) -> Option<StripLayout> {
    strip_layout_avoiding(quads, viewport, dpi, host, work, &[])
}

pub(super) fn strip_layout_avoiding(
    quads: &[[Point; 4]],
    viewport: Viewport,
    dpi: f32,
    host: (f32, f32),
    work: Bounds,
    obstacles: &[PanelRect],
) -> Option<StripLayout> {
    if quads.is_empty()
        || !viewport.zoom.is_finite()
        || viewport.zoom <= 0.
        || !dpi.is_finite()
        || dpi <= 0.
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
        || [host.0, host.1, work.x, work.y, work.width, work.height]
            .iter()
            .any(|value| !value.is_finite())
        || host.0 <= 0.
        || host.1 <= 0.
        || work.width <= 0.
        || work.height <= 0.
    {
        return None;
    }
    let area = Area {
        left: f64::from(work.x).max(0.),
        top: f64::from(work.y).max(0.),
        right: (f64::from(work.x) + f64::from(work.width)).min(f64::from(host.0)),
        bottom: (f64::from(work.y) + f64::from(work.height)).min(f64::from(host.1)),
    };
    if area.right <= area.left || area.bottom <= area.top {
        return None;
    }
    let mut selected = Area {
        left: f64::INFINITY,
        top: f64::INFINITY,
        right: f64::NEG_INFINITY,
        bottom: f64::NEG_INFINITY,
    };
    let (zoom, dpi) = (f64::from(viewport.zoom), f64::from(dpi));
    for quad in quads {
        for point in quad {
            if !point.x.is_finite() || !point.y.is_finite() {
                return None;
            }
            let x = (f64::from(point.x) * zoom + f64::from(viewport.offset.x)) / dpi;
            let y = (f64::from(point.y) * zoom + f64::from(viewport.offset.y)) / dpi;
            selected.left = selected.left.min(x);
            selected.top = selected.top.min(y);
            selected.right = selected.right.max(x);
            selected.bottom = selected.bottom.max(y);
        }
    }
    if selected.right <= selected.left
        || selected.bottom <= selected.top
        || !selected.intersects(&area)
    {
        return None;
    }
    for (width, height, compact) in [
        (FULL_WIDTH, FULL_HEIGHT, false),
        (COMPACT_WIDTH, COMPACT_HEIGHT, true),
    ] {
        let visible = PanelRect {
            x: area.left as f32,
            y: area.top as f32,
            width: (area.right - area.left) as f32,
            height: (area.bottom - area.top) as f32,
        };
        let selected_rect = PanelRect {
            x: selected.left as f32,
            y: selected.top as f32,
            width: (selected.right - selected.left) as f32,
            height: (selected.bottom - selected.top) as f32,
        };
        let preferred_x = ((selected.left + selected.right - f64::from(width)) / 2.) as f32;
        if let Some((x, y)) = crate::ocr_panel::place_mark_bar(
            visible,
            selected_rect,
            (width, height),
            preferred_x,
            true,
            obstacles,
        ) {
            return Some(StripLayout {
                x,
                y,
                width,
                height,
                compact,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(x: f32, y: f32, width: f32, height: f32) -> [Point; 4] {
        [
            Point { x, y },
            Point { x: x + width, y },
            Point {
                x: x + width,
                y: y + height,
            },
            Point { x, y: y + height },
        ]
    }

    #[test]
    fn pin_text_tools_visible_native_toolbar_uses_actual_client_dip_and_keeps_one_row() {
        let origin = (-1600, 200);
        let dpi = 1.5;
        // 真实窗口物理位置，故意与任何预测的图片下方布局不同。
        let native = Placement {
            x: -1600,
            y: 362,
            width: 600,
            height: 57,
        };
        let obstacle = client_obstacle(native, origin, dpi).unwrap();
        assert_eq!(
            obstacle,
            PanelRect {
                x: 0.,
                y: 108.,
                width: 400.,
                height: 38.
            }
        );
        let old_obstacle = client_obstacle(Placement { y: 650, ..native }, origin, dpi).unwrap();
        let old_layout = strip_layout_avoiding(
            &[quad(165., 90., 120., 45.)],
            Viewport::default(),
            dpi,
            (400., 180.),
            Bounds {
                x: -60.,
                y: -40.,
                width: 600.,
                height: 360.,
            },
            &[old_obstacle],
        )
        .unwrap();
        assert_eq!((old_layout.x, old_layout.y), (7., 98.));
        let layout = strip_layout_avoiding(
            &[quad(165., 90., 120., 45.)],
            Viewport::default(),
            dpi,
            (400., 180.),
            Bounds {
                x: -60.,
                y: -40.,
                width: 600.,
                height: 360.,
            },
            &[obstacle],
        )
        .unwrap();
        assert!(!layout.compact);
        assert_eq!((layout.width, layout.height), (286., 38.));
        assert_eq!((layout.x, layout.y), (7., 14.));
        assert!(layout.y + layout.height + 8. <= 60.);
        assert!(layout.y + layout.height + 8. <= obstacle.y);
        assert!(client_obstacle(native, origin, 0.).is_none());
        assert!(client_obstacle(native, origin, f32::NAN).is_none());
        assert!(client_obstacle(native, origin, f32::MIN_POSITIVE / 16.).is_none());
    }

    #[test]
    fn pin_text_tools_obstacles_trigger_compact_only_after_all_full_row_positions_fail() {
        let selection = [quad(10., 65., 25., 20.)];
        let work = Bounds {
            x: 0.,
            y: 0.,
            width: 400.,
            height: 200.,
        };
        let right_panel = PanelRect {
            x: 196.,
            y: 0.,
            width: 204.,
            height: 200.,
        };
        let layout = strip_layout_avoiding(
            &selection,
            Viewport::default(),
            1.,
            (400., 200.),
            work,
            &[right_panel],
        )
        .unwrap();
        assert_eq!(
            layout,
            StripLayout {
                x: 0.,
                y: 93.,
                width: 180.,
                height: 64.,
                compact: true
            }
        );
        assert!(layout.x + layout.width + 8. <= right_panel.x);
        assert!(layout.y >= 85. + 8. && layout.y + layout.height <= 200.);

        // 未显示的原生工具栏不传障碍，不应为了它降级成两行。
        let ordinary =
            strip_layout_avoiding(&selection, Viewport::default(), 1., (400., 200.), work, &[])
                .unwrap();
        assert!(!ordinary.compact);
        assert_eq!((ordinary.width, ordinary.height), (286., 38.));
    }

    #[test]
    fn pin_text_tools_full_visible_obstruction_keeps_menu_fallback_and_rejects_bad_obstacles() {
        let selection = [quad(120., 70., 80., 20.)];
        let work = Bounds {
            x: -30.,
            y: -20.,
            width: 600.,
            height: 300.,
        };
        let blocking = PanelRect {
            x: 0.,
            y: 0.,
            width: 400.,
            height: 200.,
        };
        assert!(strip_layout_avoiding(
            &selection,
            Viewport::default(),
            1.,
            (400., 200.),
            work,
            &[blocking],
        )
        .is_none());
        assert!(strip_layout_avoiding(
            &selection,
            Viewport::default(),
            1.,
            (400., 200.),
            work,
            &[PanelRect {
                width: -1.,
                ..blocking
            }],
        )
        .is_none());
    }

    #[test]
    fn pin_text_tools_high_dpi_and_negative_work_origin_keep_client_units() {
        let selection = [quad(240., 400., 300., 40.)];
        let layout = strip_layout(
            &selection,
            Viewport {
                zoom: 0.5,
                offset: Point { x: 30., y: -100. },
            },
            1.5,
            (640., 400.),
            Bounds {
                x: -120.,
                y: -80.,
                width: 750.,
                height: 450.,
            },
        )
        .unwrap();
        assert_eq!(
            layout,
            StripLayout {
                x: 7.,
                y: 88.,
                width: FULL_WIDTH,
                height: FULL_HEIGHT,
                compact: false,
            }
        );
        // 选择投影为 x=100..200、y=66.666..80，操作条下方恰好留 8 DIP。
        assert!(layout.y >= 80. + GAP as f32);
        assert!(layout.x >= 0. && layout.x + layout.width <= 630.);
        assert!(layout.y + layout.height <= 370.);
    }

    #[test]
    fn pin_text_tools_upper_and_compact_fallback_respect_visible_intersection() {
        let full = strip_layout(
            &[quad(460., 320., 80., 40.)],
            Viewport::default(),
            1.,
            (640., 400.),
            Bounds {
                x: 20.,
                y: 30.,
                width: 600.,
                height: 350.,
            },
        )
        .unwrap();
        assert_eq!((full.x, full.y, full.compact), (334., 274., false));
        assert!(full.y + full.height + GAP as f32 <= 320.);

        let compact = strip_layout(
            &[quad(210., 70., 30., 20.)],
            Viewport::default(),
            1.,
            (320., 300.),
            Bounds {
                x: 40.,
                y: 30.,
                width: 200.,
                height: 240.,
            },
        )
        .unwrap();
        assert_eq!(
            compact,
            StripLayout {
                x: 60.,
                y: 98.,
                width: COMPACT_WIDTH,
                height: COMPACT_HEIGHT,
                compact: true,
            }
        );
    }

    #[test]
    fn pin_text_tools_reject_no_room_oversized_selection_and_invalid_inputs() {
        let host = (640., 400.);
        let work = Bounds {
            x: 0.,
            y: 0.,
            width: host.0,
            height: host.1,
        };
        let selection = [quad(100., 150., 180., 40.)];
        let layout = strip_layout;
        for quads in [
            vec![],
            vec![quad(0., 0., 640., 400.)],
            vec![quad(-100_000., -100_000., 200_000., 200_000.)],
            vec![quad(700., 500., 10., 10.)],
            vec![quad(20., 20., 0., 0.)],
            vec![quad(f32::NAN, 20., 10., 10.)],
        ] {
            assert!(layout(&quads, Viewport::default(), 1., host, work).is_none());
        }
        for value in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(layout(&selection, Viewport::default(), value, host, work).is_none());
            assert!(layout(
                &selection,
                Viewport {
                    zoom: value,
                    ..Viewport::default()
                },
                1.,
                host,
                work,
            )
            .is_none());
            assert!(layout(&selection, Viewport::default(), 1., (value, 400.), work).is_none());
            assert!(layout(
                &selection,
                Viewport::default(),
                1.,
                host,
                Bounds {
                    width: value,
                    ..work
                },
            )
            .is_none());
        }
        assert!(layout(&selection, Viewport::default(), 1., (179., 400.), work,).is_none());
        assert!(layout(
            &[quad(20., 20., 50., 10.)],
            Viewport::default(),
            1.,
            (640., 70.),
            work,
        )
        .is_none());
        assert!(layout(
            &selection,
            Viewport {
                offset: Point { x: 0., y: f32::NAN },
                ..Viewport::default()
            },
            1.,
            host,
            work,
        )
        .is_none());
        assert!(layout(
            &selection,
            Viewport::default(),
            1.,
            host,
            Bounds {
                x: f32::NAN,
                ..work
            },
        )
        .is_none());
    }
}
