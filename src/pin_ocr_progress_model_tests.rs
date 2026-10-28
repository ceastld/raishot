use super::*;

fn key(kind: TaskKind, identity: &Arc<()>, id: u64) -> TaskKey {
    TaskKey {
        kind,
        identity: identity.clone(),
        id,
    }
}

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn display(x: f32, y: f32, width: f32, height: f32) -> DisplayRect {
    DisplayRect {
        x,
        y,
        width,
        height,
    }
}

fn contained(inner: DisplayRect, outer: DisplayRect) {
    assert!(inner.width > 0.0 && inner.height > 0.0);
    assert!(inner.x >= outer.x && inner.y >= outer.y);
    assert!(inner.x + inner.width <= outer.x + outer.width);
    assert!(inner.y + inner.height <= outer.y + outer.height);
}

#[test]
fn actual_task_identity_preserves_phase_and_replacement_restarts() {
    let start = Instant::now();
    let identity = Arc::new(());
    let replacement = Arc::new(());
    let mut clock = ProgressClock::default();
    assert_eq!(
        clock.observe(Some(key(TaskKind::Ordinary, &identity, 7)), start),
        Some(Duration::ZERO)
    );
    assert_eq!(
        clock.observe(
            Some(key(TaskKind::Ordinary, &identity, 7)),
            start + Duration::from_millis(40)
        ),
        Some(Duration::from_millis(40))
    );
    assert_eq!(
        clock.observe(
            Some(key(TaskKind::Ordinary, &replacement, 7)),
            start + Duration::from_millis(80)
        ),
        Some(Duration::ZERO)
    );
}

#[test]
fn same_numeric_task_from_different_source_is_not_equal() {
    let first_source = Arc::new(());
    let second_source = Arc::new(());
    assert_eq!(
        key(TaskKind::Local, &first_source, 12),
        key(TaskKind::Local, &first_source, 12)
    );
    assert_ne!(
        key(TaskKind::Local, &first_source, 12),
        key(TaskKind::Local, &second_source, 12)
    );
    assert_ne!(
        key(TaskKind::Local, &first_source, 12),
        key(TaskKind::Ordinary, &first_source, 12)
    );
    assert_ne!(
        key(TaskKind::Local, &first_source, 12),
        key(TaskKind::Local, &first_source, 13)
    );
}

#[test]
fn task_kind_or_real_request_id_change_restarts_phase() {
    let start = Instant::now();
    let identity = Arc::new(());
    let mut clock = ProgressClock::default();
    clock.observe(Some(key(TaskKind::Ordinary, &identity, 3)), start);
    assert_eq!(
        clock.observe(
            Some(key(TaskKind::Ordinary, &identity, 4)),
            start + Duration::from_millis(50)
        ),
        Some(Duration::ZERO)
    );
    assert_eq!(
        clock.observe(
            Some(key(TaskKind::Local, &identity, 4)),
            start + Duration::from_millis(100)
        ),
        Some(Duration::ZERO)
    );
}

#[test]
fn completion_cancel_and_inactivity_none_clear_phase_and_render_lease() {
    let start = Instant::now();
    let identity = Arc::new(());
    let task = key(TaskKind::Local, &identity, 3);
    let mut clock = ProgressClock::default();
    for milliseconds in [20, 40, 60] {
        let (_, lease) = clock.begin_render(Some(task.clone()), start).unwrap();
        assert!(clock.lease_is_current(&lease, Some(&task)));
        assert_eq!(
            clock.observe(None, start + Duration::from_millis(milliseconds)),
            None
        );
        assert!(!clock.lease_is_current(&lease, Some(&task)));
        assert!(clock.active.is_none() && clock.render_epoch.is_none());
        assert_eq!(
            clock.observe(
                Some(task.clone()),
                start + Duration::from_millis(milliseconds + 1)
            ),
            Some(Duration::ZERO)
        );
        clock.observe(None, start);
    }
}

#[test]
fn new_render_invalidates_old_paint_without_restarting_same_task_phase() {
    let start = Instant::now();
    let identity = Arc::new(());
    let task = key(TaskKind::Ordinary, &identity, 5);
    let mut clock = ProgressClock::default();
    let (_, old) = clock.begin_render(Some(task.clone()), start).unwrap();
    let (elapsed, new) = clock
        .begin_render(Some(task.clone()), start + Duration::from_millis(70))
        .unwrap();
    assert_eq!(elapsed, Duration::from_millis(70));
    assert!(!clock.lease_is_current(&old, Some(&task)));
    assert!(clock.lease_is_current(&new, Some(&task)));
    assert!(!clock.lease_is_current(&new, None));
    assert!(clock.lease_is_current(&new.clone(), Some(&task)));
}

#[test]
fn replaced_source_and_old_task_key_cannot_renew_old_paint() {
    let start = Instant::now();
    let old_source = Arc::new(());
    let new_source = Arc::new(());
    let old_task = key(TaskKind::Local, &old_source, 9);
    let new_task = key(TaskKind::Local, &new_source, 9);
    let mut clock = ProgressClock::default();
    let (_, old_lease) = clock.begin_render(Some(old_task.clone()), start).unwrap();
    assert!(!clock.lease_is_current(&old_lease, Some(&new_task)));
    clock.observe(Some(new_task.clone()), start + Duration::from_millis(20));
    assert!(!clock.lease_is_current(&old_lease, Some(&old_task)));
    assert!(!clock.lease_is_current(&old_lease, Some(&new_task)));
    let (_, new_lease) = clock.begin_render(Some(new_task.clone()), start).unwrap();
    assert!(clock.lease_is_current(&new_lease, Some(&new_task)));
    assert!(!clock.lease_is_current(&old_lease, Some(&new_task)));
}

#[test]
fn render_lease_from_other_clock_and_empty_render_cannot_animate() {
    let start = Instant::now();
    let identity = Arc::new(());
    let task = key(TaskKind::Ordinary, &identity, 3);
    let mut first = ProgressClock::default();
    let mut second = ProgressClock::default();
    let (_, first_lease) = first.begin_render(Some(task.clone()), start).unwrap();
    let (_, second_lease) = second.begin_render(Some(task.clone()), start).unwrap();
    assert!(!first.lease_is_current(&second_lease, Some(&task)));
    assert!(!second.lease_is_current(&first_lease, Some(&task)));
    assert!(first.begin_render(None, start).is_none());
    assert!(!first.lease_is_current(&first_lease, Some(&task)));
}

#[test]
fn earlier_timestamp_saturates_without_negative_phase() {
    let start = Instant::now();
    let identity = Arc::new(());
    let task = key(TaskKind::Local, &identity, 2);
    let mut clock = ProgressClock::default();
    clock.observe(Some(task.clone()), start + Duration::from_millis(20));
    assert_eq!(clock.observe(Some(task), start), Some(Duration::ZERO));
}

#[test]
fn pin_projection_applies_zoom_and_dpi_to_roi_and_physical_pan_once() {
    let roi = rect(10, 20, 100, 50);
    assert_eq!(
        DisplayRect::project(roi, (400, 300), 3.0, 2.0, (-40.0, 20.0)),
        Some(display(-5.0, 40.0, 150.0, 75.0))
    );
    assert_eq!(roi, rect(10, 20, 100, 50));
    assert_eq!(
        DisplayRect::project(
            rect(100, 50, 300, 150),
            (800, 600),
            0.75,
            1.5,
            (-30.0, -45.0)
        ),
        Some(display(30.0, -5.0, 150.0, 75.0))
    );
}

#[test]
fn invalid_source_zoom_dpi_offsets_and_nonfinite_projection_are_rejected() {
    for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            DisplayRect::project(Rect::full(10, 10), (10, 10), invalid, 1.0, (0.0, 0.0)).is_none()
        );
        assert!(
            DisplayRect::project(Rect::full(10, 10), (10, 10), 1.0, invalid, (0.0, 0.0)).is_none()
        );
    }
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            DisplayRect::project(Rect::full(10, 10), (10, 10), 1.0, 1.0, (invalid, 0.0)).is_none()
        );
        assert!(
            DisplayRect::project(Rect::full(10, 10), (10, 10), 1.0, 1.0, (0.0, invalid)).is_none()
        );
    }
    for roi in [
        rect(0, 0, 0, 1),
        rect(0, 0, 1, 0),
        rect(9, 0, 2, 1),
        rect(0, 9, 1, 2),
        rect(u32::MAX, 0, 2, 1),
    ] {
        assert!(DisplayRect::project(roi, (10, 10), 1.0, 1.0, (0.0, 0.0)).is_none());
    }
    assert!(DisplayRect::project(Rect::full(1, 1), (0, 0), 1.0, 1.0, (0.0, 0.0)).is_none());
    assert!(DisplayRect::project(
        Rect::full(10, 10),
        (10, 10),
        f32::MAX,
        f32::MIN_POSITIVE,
        (0.0, 0.0)
    )
    .is_none());
}

#[test]
fn projected_roi_clips_to_visible_window_and_disjoint_roi_is_absent() {
    let roi = display(-20.0, -50.0, 200.0, 120.0);
    let window = display(0.0, 0.0, 120.0, 80.0);
    assert_eq!(
        roi.intersection(window),
        Some(display(0.0, 0.0, 120.0, 70.0))
    );
    assert!(display(200.0, 0.0, 20.0, 20.0)
        .intersection(window)
        .is_none());
    assert!(display(120.0, 0.0, 20.0, 20.0)
        .intersection(window)
        .is_none());
    assert!(display(0.0, 0.0, -1.0, 20.0).intersection(window).is_none());
    assert!(display(f32::NAN, 0.0, 1.0, 1.0)
        .intersection(window)
        .is_none());
}

#[test]
fn scan_repeats_every_1600ms_and_always_stays_inside_roi_and_window() {
    let roi = display(-20.0, -50.0, 200.0, 120.0);
    let window = display(0.0, 0.0, 120.0, 80.0);
    let visible = roi.intersection(window).unwrap();
    let first = ScanFrame::new(roi, window, Duration::ZERO).unwrap();
    let middle = ScanFrame::new(roi, window, Duration::from_millis(800)).unwrap();
    let repeated = ScanFrame::new(roi, window, Duration::from_millis(1600)).unwrap();
    assert_eq!(first.band, repeated.band);
    assert_eq!(first.band.y, visible.y);
    assert_eq!(
        middle.band.y + middle.band.height,
        visible.y + visible.height
    );
    for milliseconds in (0..6400).step_by(13) {
        let frame = ScanFrame::new(roi, window, Duration::from_millis(milliseconds)).unwrap();
        contained(frame.band, roi);
        contained(frame.band, window);
        assert!((72..=108).contains(&frame.edge_alpha));
    }
}

#[test]
fn tiny_visible_roi_keeps_scan_inside_and_omits_badge() {
    let roi = display(-10.0, 40.0, 1.0, 1.0);
    let window = display(-20.0, 0.0, 100.0, 100.0);
    for milliseconds in [0, 200, 800, 1599] {
        assert_eq!(
            ScanFrame::new(roi, window, Duration::from_millis(milliseconds))
                .unwrap()
                .band,
            roi
        );
    }
    assert!(roi.badge(36.0).is_none());
    assert!(ScanFrame::new(roi, display(0.0, 0.0, 100.0, 100.0), Duration::ZERO).is_none());
}

#[test]
fn actual_short_chinese_and_english_text_widths_fit_or_badge_is_omitted() {
    let visible = display(120.0, 80.0, 160.0, 64.0);
    for text_width in [36.0, 68.0] {
        let badge = visible.badge(text_width).unwrap();
        contained(badge, visible);
        assert!(badge.x >= visible.x + 8.0 && badge.y >= visible.y + 8.0);
        assert!(badge.x + badge.width <= visible.x + visible.width - 8.0);
        assert!(badge.y + badge.height <= visible.y + visible.height - 8.0);
    }
    for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY, 150.0] {
        assert!(visible.badge(invalid).is_none());
    }
    assert!(display(0.0, 0.0, 63.0, 80.0).badge(20.0).is_none());
    assert!(display(0.0, 0.0, 160.0, 41.0).badge(20.0).is_none());
    assert!(display(0.0, 0.0, 160.0, 42.0).badge(20.0).is_some());
}
