//! 只保护正在斜向进入已展开子菜单的鼠标轨迹，纵向换行无需等待。
use super::layout::Panel;

const CORNER_TOLERANCE: f32 = 8.;

pub(super) fn moving_toward_submenu(
    previous: (f32, f32),
    current: (f32, f32),
    submenu: Panel,
    submenu_left: bool,
) -> bool {
    if ![
        previous.0,
        previous.1,
        current.0,
        current.1,
        submenu.x,
        submenu.y,
        submenu.width,
        submenu.height,
    ]
    .into_iter()
    .all(f32::is_finite)
        || submenu.width <= 0.
        || submenu.height <= 0.
    {
        return false;
    }

    let direction = if submenu_left { -1. } else { 1. };
    let near_x = if submenu_left {
        submenu.x + submenu.width
    } else {
        submenu.x
    };
    let horizontal = (current.0 - previous.0) * direction;
    let vertical = (current.1 - previous.1).abs();
    // 以方向比例排除纵向换项的横向抖动，兼容高频鼠标和高 DPI 的微小步进。
    if horizontal <= 0. || horizontal < vertical * 0.25 {
        return false;
    }

    let previous_distance = (near_x - previous.0) * direction;
    let remaining_distance = (near_x - current.0) * direction;
    // 已进入面板或穿过近侧边后，由子菜单自己的悬停处理接管。
    if previous_distance <= 0. || remaining_distance <= 0. {
        return false;
    }

    // 用实际限幅后的面板上下角构成三角通道，左右展开共用同一计算。
    let fraction = horizontal / previous_distance;
    let top = previous.1 + (submenu.y - CORNER_TOLERANCE - previous.1) * fraction;
    let bottom =
        previous.1 + (submenu.y + submenu.height + CORNER_TOLERANCE - previous.1) * fraction;
    current.1 >= top && current.1 <= bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn right_panel() -> Panel {
        Panel {
            x: 340.,
            y: 100.,
            width: 340.,
            height: 200.,
        }
    }

    #[test]
    fn vertical_switch_and_tiny_horizontal_jitter_do_not_wait() {
        let panel = right_panel();
        for current in [(200., 180.), (200., 220.), (201., 220.), (202., 230.)] {
            assert!(!moving_toward_submenu((200., 200.), current, panel, false));
        }
    }

    #[test]
    fn rightward_diagonal_paths_keep_the_current_submenu() {
        let panel = right_panel();
        assert!(moving_toward_submenu(
            (200., 200.),
            (250., 180.),
            panel,
            false
        ));
        assert!(moving_toward_submenu(
            (200., 200.),
            (250., 230.),
            panel,
            false
        ));
        for current in [(201., 199.5), (200.5, 200.25)] {
            assert!(moving_toward_submenu((200., 200.), current, panel, false));
        }
    }

    #[test]
    fn leftward_diagonal_paths_keep_the_current_submenu() {
        let panel = Panel {
            x: 0.,
            ..right_panel()
        };
        assert!(moving_toward_submenu(
            (480., 200.),
            (430., 180.),
            panel,
            true
        ));
        assert!(moving_toward_submenu(
            (480., 200.),
            (430., 230.),
            panel,
            true
        ));
        for current in [(479., 199.5), (479.5, 200.25)] {
            assert!(moving_toward_submenu((480., 200.), current, panel, true));
        }
    }

    #[test]
    fn reverse_or_outside_corridor_switches_immediately() {
        let panel = right_panel();
        assert!(!moving_toward_submenu(
            (200., 200.),
            (190., 200.),
            panel,
            false
        ));
        assert!(!moving_toward_submenu(
            (200., 200.),
            (250., 250.),
            panel,
            false
        ));
        assert!(!moving_toward_submenu(
            (200., 200.),
            (250., 150.),
            panel,
            false
        ));
    }

    #[test]
    fn upward_clamped_panel_uses_its_visible_corners() {
        let panel = Panel {
            y: 20.,
            height: 300.,
            ..right_panel()
        };
        assert!(moving_toward_submenu(
            (200., 390.),
            (260., 345.),
            panel,
            false
        ));
        assert!(!moving_toward_submenu(
            (200., 390.),
            (260., 400.),
            panel,
            false
        ));
    }

    #[test]
    fn corner_tolerance_does_not_protect_paths_far_outside() {
        let panel = right_panel();
        assert!(moving_toward_submenu(
            (300., 110.),
            (330., 98.),
            panel,
            false
        ));
        assert!(!moving_toward_submenu(
            (300., 110.),
            (330., 90.),
            panel,
            false
        ));
    }

    #[test]
    fn unchanged_or_crossed_edge_has_no_protection() {
        let panel = right_panel();
        for (previous, current) in [
            ((200., 200.), (200., 200.)),
            ((330., 200.), (340., 200.)),
            ((330., 200.), (350., 200.)),
            ((350., 200.), (360., 200.)),
        ] {
            assert!(!moving_toward_submenu(previous, current, panel, false));
        }
        let left = Panel { x: 0., ..panel };
        assert!(!moving_toward_submenu(
            (350., 200.),
            (330., 200.),
            left,
            true
        ));
    }
}
