//! 独立坐标 oracle：显式给出边、锚点与平台限制，避免复制手势计算实现。
use super::{RegionGesture, RegionHandle};
use crate::{geometry::ScreenRect, stitch::ScrollAxis};

fn rect(left: i32, top: i32, width: u32, height: u32) -> ScreenRect {
    ScreenRect {
        left,
        top,
        width,
        height,
    }
}

fn gesture(handle: RegionHandle) -> RegionGesture {
    RegionGesture::new_for_axis(
        7,
        rect(100, 100, 200, 160),
        (300, 300),
        handle,
        None,
        rect(0, 0, 1000, 1000),
        ScrollAxis::Vertical,
        64,
    )
    .unwrap()
}

#[test]
fn long_region_all_eight_handles_keep_the_opposite_edge_or_corner() {
    let cases = [
        (RegionHandle::North, rect(100, 130, 200, 130)),
        (RegionHandle::NorthEast, rect(100, 130, 220, 130)),
        (RegionHandle::East, rect(100, 100, 220, 160)),
        (RegionHandle::SouthEast, rect(100, 100, 220, 190)),
        (RegionHandle::South, rect(100, 100, 200, 190)),
        (RegionHandle::SouthWest, rect(120, 100, 180, 190)),
        (RegionHandle::West, rect(120, 100, 180, 160)),
        (RegionHandle::NorthWest, rect(120, 130, 180, 130)),
    ];
    for (handle, expected) in cases {
        let drag = gesture(handle);
        assert_eq!(drag.update((320, 330)).unwrap(), expected, "{handle:?}");
        assert_eq!(drag.update((300, 300)).unwrap(), rect(100, 100, 200, 160));
        assert_eq!(drag.id(), 7);
        assert_eq!(drag.original(), rect(100, 100, 200, 160));
    }
}

#[test]
fn long_region_running_move_obeys_axis_and_pause_allows_both_coordinates() {
    let original = rect(100, 100, 200, 160);
    for (axis, expected) in [
        (ScrollAxis::Vertical, rect(100, 130, 200, 160)),
        (ScrollAxis::Horizontal, rect(120, 100, 200, 160)),
    ] {
        let drag = RegionGesture::new_for_axis(
            11,
            original,
            (300, 300),
            RegionHandle::Move,
            Some(axis),
            rect(0, 0, 1000, 1000),
            axis,
            64,
        )
        .unwrap();
        assert_eq!(drag.update((320, 330)).unwrap(), expected);
        assert!(RegionGesture::new_for_axis(
            12,
            original,
            (300, 300),
            RegionHandle::East,
            Some(axis),
            rect(0, 0, 1000, 1000),
            axis,
            64,
        )
        .is_err());
    }
    assert_eq!(
        gesture(RegionHandle::Move).update((320, 330)).unwrap(),
        rect(120, 130, 200, 160)
    );
}

#[test]
fn long_region_reverse_drag_stops_at_axis_minimum_and_retains_fixed_bands() {
    let cases = [
        (RegionHandle::West, (1000, 300), rect(268, 100, 32, 160)),
        (RegionHandle::East, (-1000, 300), rect(100, 100, 32, 160)),
        (RegionHandle::North, (300, 1000), rect(100, 196, 200, 64)),
        (RegionHandle::South, (300, -1000), rect(100, 100, 200, 64)),
        (
            RegionHandle::NorthWest,
            (1000, 1000),
            rect(268, 196, 32, 64),
        ),
        (
            RegionHandle::SouthEast,
            (-1000, -1000),
            rect(100, 100, 32, 64),
        ),
    ];
    for (handle, point, expected) in cases {
        assert_eq!(
            gesture(handle).update(point).unwrap(),
            expected,
            "{handle:?}"
        );
    }
    let horizontal = RegionGesture::new_for_axis(
        3,
        rect(-200, -100, 240, 32),
        (0, 0),
        RegionHandle::West,
        None,
        rect(-1000, -1000, 2000, 2000),
        ScrollAxis::Horizontal,
        104,
    )
    .unwrap();
    assert_eq!(
        horizontal.update((1000, 0)).unwrap(),
        rect(-64, -100, 104, 32)
    );
    let vertical = RegionGesture::new_for_axis(
        4,
        rect(-200, -100, 32, 240),
        (0, 0),
        RegionHandle::North,
        None,
        rect(-1000, -1000, 2000, 2000),
        ScrollAxis::Vertical,
        104,
    )
    .unwrap();
    assert_eq!(vertical.update((0, 1000)).unwrap(), rect(-200, 36, 32, 104));
}

#[test]
fn long_region_negative_multiscreen_bounds_and_extreme_pointer_delta_are_safe() {
    let bounds = rect(-1920, -1080, 4480, 2520);
    let original = rect(-200, -100, 320, 240);
    let drag = RegionGesture::new_for_axis(
        9,
        original,
        (i32::MAX, i32::MAX),
        RegionHandle::Move,
        None,
        bounds,
        ScrollAxis::Vertical,
        64,
    )
    .unwrap();
    assert_eq!(
        drag.update((i32::MIN, i32::MIN)).unwrap(),
        rect(-1920, -1080, 320, 240)
    );
    let drag = RegionGesture::new_for_axis(
        10,
        original,
        (i32::MIN, i32::MIN),
        RegionHandle::Move,
        None,
        bounds,
        ScrollAxis::Vertical,
        64,
    )
    .unwrap();
    assert_eq!(
        drag.update((i32::MAX, i32::MAX)).unwrap(),
        rect(2240, 1200, 320, 240)
    );
    let west = RegionGesture::new_for_axis(
        13,
        original,
        (0, 0),
        RegionHandle::West,
        None,
        bounds,
        ScrollAxis::Vertical,
        64,
    )
    .unwrap();
    assert_eq!(
        west.update((-5000, 0)).unwrap(),
        rect(-1920, -100, 2040, 240)
    );
    let north = RegionGesture::new_for_axis(
        14,
        original,
        (0, 0),
        RegionHandle::North,
        None,
        bounds,
        ScrollAxis::Vertical,
        64,
    )
    .unwrap();
    assert_eq!(
        north.update((0, -5000)).unwrap(),
        rect(-200, -1080, 320, 1220)
    );
    assert_eq!(original, rect(-200, -100, 320, 240));
}

#[test]
fn long_region_side_and_area_budgets_do_not_move_the_opposite_anchor() {
    let bounds = rect(-20_000, -20_000, 60_000, 60_000);
    for (original, handle, expected) in [
        (
            rect(0, 0, 64, 64),
            RegionHandle::East,
            rect(0, 0, 16_384, 64),
        ),
        (
            rect(0, 0, 64, 64),
            RegionHandle::South,
            rect(0, 0, 64, 16_384),
        ),
        (
            rect(0, 0, 4000, 4000),
            RegionHandle::East,
            rect(0, 0, 8000, 4000),
        ),
        (
            rect(0, 0, 4000, 4000),
            RegionHandle::North,
            rect(0, -4000, 4000, 8000),
        ),
    ] {
        let drag = RegionGesture::new_for_axis(
            15,
            original,
            (0, 0),
            handle,
            None,
            bounds,
            ScrollAxis::Vertical,
            64,
        )
        .unwrap();
        let point = if handle == RegionHandle::North {
            (40_000, -40_000)
        } else {
            (40_000, 40_000)
        };
        assert_eq!(drag.update(point).unwrap(), expected);
    }
    for handle in [
        RegionHandle::NorthWest,
        RegionHandle::NorthEast,
        RegionHandle::SouthEast,
        RegionHandle::SouthWest,
    ] {
        let point = match handle {
            RegionHandle::NorthWest => (-40_000, -40_000),
            RegionHandle::NorthEast => (40_000, -40_000),
            RegionHandle::SouthWest => (-40_000, 40_000),
            _ => (40_000, 40_000),
        };
        let result = RegionGesture::new_for_axis(
            16,
            rect(100, 100, 4000, 4000),
            (0, 0),
            handle,
            None,
            bounds,
            ScrollAxis::Vertical,
            64,
        )
        .unwrap()
        .update(point)
        .unwrap();
        assert!(result.width <= 16_384 && result.height <= 16_384);
        assert!(u64::from(result.width) * u64::from(result.height) <= 32_000_000);
        if matches!(handle, RegionHandle::NorthWest | RegionHandle::SouthWest) {
            assert_eq!(i64::from(result.left) + i64::from(result.width), 4100);
        } else {
            assert_eq!(result.left, 100);
        }
        if matches!(handle, RegionHandle::NorthWest | RegionHandle::NorthEast) {
            assert_eq!(i64::from(result.top) + i64::from(result.height), 4100);
        } else {
            assert_eq!(result.top, 100);
        }
    }
    // 极大的固定边带不能在双轴预算收缩中被缩短。
    let result = RegionGesture::new_for_axis(
        17,
        rect(0, 0, 32, 16_384),
        (0, 0),
        RegionHandle::SouthEast,
        None,
        bounds,
        ScrollAxis::Vertical,
        16_384,
    )
    .unwrap()
    .update((40_000, 40_000))
    .unwrap();
    assert_eq!(result, rect(0, 0, 1953, 16_384));
}

#[test]
fn long_region_invalid_original_or_bounds_fail_without_mutating_the_inputs() {
    let bounds = rect(-20_000, -20_000, 60_000, 60_000);
    for original in [
        rect(0, 0, 0, 64),
        rect(0, 0, 31, 64),
        rect(0, 0, 32, 63),
        rect(0, 0, 16_385, 64),
        rect(0, 0, 8001, 4000),
        rect(-20_001, 0, 64, 64),
        rect(i32::MAX - 10, 0, 64, 64),
    ] {
        let saved = original;
        assert!(RegionGesture::new_for_axis(
            18,
            original,
            (0, 0),
            RegionHandle::Move,
            None,
            bounds,
            ScrollAxis::Vertical,
            64,
        )
        .is_err());
        assert_eq!(original, saved);
    }
    for invalid_bounds in [rect(0, 0, 0, 1000), rect(i32::MAX - 10, 0, 1000, 1000)] {
        assert!(RegionGesture::new_for_axis(
            19,
            rect(0, 0, 64, 64),
            (0, 0),
            RegionHandle::Move,
            None,
            invalid_bounds,
            ScrollAxis::Vertical,
            64,
        )
        .is_err());
    }
    assert!(RegionGesture::new_for_axis(
        20,
        rect(0, 0, 64, 64),
        (0, 0),
        RegionHandle::Move,
        None,
        bounds,
        ScrollAxis::Vertical,
        16_385,
    )
    .is_err());
    assert_eq!(bounds, rect(-20_000, -20_000, 60_000, 60_000));
}
