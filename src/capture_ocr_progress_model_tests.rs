use super::*;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[test]
fn full_screen_physical_roi_projects_once_at_fractional_dpi_and_pan() {
    let roi = rect(180, 75, 600, 300);
    let display = DisplayRect::project(roi, Rect::full(1920, 1080), 1.5, (-20., 12.)).unwrap();
    assert_eq!(
        display,
        DisplayRect {
            x: 100.,
            y: 62.,
            width: 400.,
            height: 200.
        }
    );
    assert_eq!(roi, rect(180, 75, 600, 300));
}

#[test]
fn invalid_scale_offset_or_physical_region_never_produces_an_overlay() {
    for scale in [0., -1., f32::NAN, f32::INFINITY] {
        assert!(
            DisplayRect::project(Rect::full(10, 10), Rect::full(10, 10), scale, (0., 0.)).is_none()
        );
    }
    for roi in [
        rect(0, 0, 0, 1),
        rect(0, 0, 1, 0),
        rect(9, 0, 2, 1),
        rect(u32::MAX, 0, 2, 1),
    ] {
        assert!(DisplayRect::project(roi, Rect::full(10, 10), 1., (0., 0.)).is_none());
    }
    assert!(
        DisplayRect::project(Rect::full(10, 10), Rect::full(10, 10), 1., (f32::NAN, 0.)).is_none()
    );
    assert!(DisplayRect::project(
        Rect::full(u32::MAX, 1),
        Rect::full(u32::MAX, 1),
        f32::MIN_POSITIVE,
        (0., 0.)
    )
    .is_none());
}

#[test]
fn scan_ping_pongs_inside_roi_and_repeats_without_claiming_percent() {
    let roi = DisplayRect {
        x: 30.,
        y: 70.,
        width: 180.,
        height: 60.,
    };
    let start = ScanFrame::new(roi, Duration::ZERO);
    let midpoint = ScanFrame::new(roi, Duration::from_millis(800));
    let repeated = ScanFrame::new(roi, Duration::from_millis(1600));
    assert_eq!(start.center_y, 70.);
    assert_eq!(midpoint.center_y, 130.);
    assert_eq!(start.center_y, repeated.center_y);
    for millis in 0..5000 {
        let scan = ScanFrame::new(roi, Duration::from_millis(millis));
        assert!((70. ..=130.).contains(&scan.center_y));
        assert!(scan.band_height > 0. && scan.band_height <= roi.height);
        assert!((72..=108).contains(&scan.edge_alpha));
    }
}

#[test]
fn tiny_selection_has_a_clipped_scan_and_no_center_badge() {
    let roi = DisplayRect {
        x: -10.,
        y: 40.,
        width: 1.,
        height: 1.,
    };
    assert_eq!(
        ScanFrame::new(roi, Duration::from_nanos(99)).band_height,
        1.
    );
    assert!(roi.badge(36.).is_none());
}

#[test]
fn measured_chinese_and_english_badges_remain_inside_selection() {
    let roi = DisplayRect {
        x: 120.,
        y: 80.,
        width: 160.,
        height: 64.,
    };
    for width in [36., 68.] {
        let badge = roi.badge(width).unwrap();
        assert!(badge.x >= roi.x + 8. && badge.y >= roi.y + 8.);
        assert!(badge.x + badge.width <= roi.x + roi.width - 8.);
        assert!(badge.y + badge.height <= roi.y + roi.height - 8.);
    }
    for width in [f32::NAN, -1., 0., 150.] {
        assert!(roi.badge(width).is_none());
    }
}

#[test]
fn same_task_keeps_phase_and_new_task_or_task_kind_restarts() {
    let now = Instant::now();
    let mut clock = ProgressClock::default();
    let key = TaskKey {
        kind: TaskKind::Initial,
        id: 1,
    };
    assert_eq!(clock.observe(Some(key), now), Some(Duration::ZERO));
    assert_eq!(
        clock.observe(Some(key), now + Duration::from_millis(40)),
        Some(Duration::from_millis(40))
    );
    assert_eq!(
        clock.observe(
            Some(TaskKey {
                kind: TaskKind::Initial,
                id: 2
            }),
            now + Duration::from_millis(60)
        ),
        Some(Duration::ZERO)
    );
    assert_eq!(
        clock.observe(
            Some(TaskKey {
                kind: TaskKind::Foreground,
                id: 2
            }),
            now + Duration::from_millis(90)
        ),
        Some(Duration::ZERO)
    );
}

#[test]
fn completion_cancel_inactivity_or_invalid_geometry_resets_clock_without_a_worker() {
    let now = Instant::now();
    let mut clock = ProgressClock::default();
    let key = TaskKey {
        kind: TaskKind::Local,
        id: 1,
    };
    assert_eq!(clock.observe(Some(key), now), Some(Duration::ZERO));
    assert_eq!(clock.observe(None, now + Duration::from_millis(40)), None);
    assert!(clock.active.is_none());
    assert_eq!(clock.observe(None, now + Duration::from_millis(90)), None);
    assert_eq!(
        clock.observe(Some(key), now + Duration::from_millis(100)),
        Some(Duration::ZERO)
    );
}

#[test]
fn local_roi_is_relative_to_saved_input_and_cannot_escape_original_selection() {
    let selected = rect(140, 60, 300, 90);
    assert_eq!(
        local_roi(selected, rect(20, 5, 90, 22), Rect::full(800, 600)),
        Some(rect(160, 65, 90, 22))
    );
    assert!(local_roi(selected, rect(299, 0, 2, 1), Rect::full(800, 600)).is_none());
    assert!(local_roi(rect(799, 0, 2, 1), Rect::full(2, 1), Rect::full(800, 600)).is_none());
    assert!(local_roi(selected, rect(u32::MAX, 0, 2, 1), Rect::full(800, 600)).is_none());
}
