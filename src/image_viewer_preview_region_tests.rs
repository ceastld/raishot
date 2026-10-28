use super::*;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn source() -> Arc<FrozenPreviewSource> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 4, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let row = [
            10, 20, 30, 0, 40, 50, 60, 128, 70, 80, 90, 255, 100, 110, 120, 64,
        ];
        writer.write_image_data(&row.repeat(2)).unwrap();
        writer.finish().unwrap();
    }
    let (source, overview) =
        FrozenPreviewSource::open(bytes, 0, PreviewLimits::default(), &AtomicBool::new(false))
            .unwrap();
    drop(overview);
    source
}

fn failed(request: Request, message: &str) -> Completion {
    Completion {
        serial: request.serial,
        epoch: request.epoch,
        source: request.source,
        region: request.region,
        result: Err(message.to_string()),
    }
}

#[test]
fn image_viewer_preview_region_deadline_coalescing_epoch_and_clear_reject_stale() {
    let source = source();
    let now = Instant::now();
    let mut state = State::default();
    assert!(!state.pending());
    let first = rect(0, 0, 2, 1);
    let second = rect(1, 0, 2, 1);
    let change = state.set_target(7, &source, first, now).unwrap();
    assert!(change.changed && change.retired.is_none());
    assert!(state.pending());
    assert!(state
        .due_request(now + Duration::from_millis(119), 0)
        .is_none());
    let same = state
        .set_target(7, &source, first, now + Duration::from_millis(119))
        .unwrap();
    assert!(!same.changed && same.retired.is_none());
    let first_request = state
        .due_request(now + Duration::from_millis(120), 0)
        .unwrap();
    assert!(!first_request.cancel.load(Ordering::Acquire));
    assert!(state
        .due_request(now + Duration::from_millis(121), 0)
        .is_none());
    assert!(state.set_target(7, &source, rect(4, 0, 1, 1), now).is_err());
    assert!(state.pending());

    assert!(
        state
            .set_target(7, &source, second, now + Duration::from_millis(125))
            .unwrap()
            .changed
    );
    let (accepted, retired) = state.accept(failed(first_request, "old region"), 7, &source);
    assert!(!accepted && retired.is_none());
    assert!(state.pending() && state.error().is_none());
    assert!(state
        .due_request(now + Duration::from_millis(244), 0)
        .is_none());
    let second_request = state
        .due_request(now + Duration::from_millis(245), 0)
        .unwrap();
    assert!(state.clear().is_none());
    assert!(!state.pending() && state.surface().is_none() && state.error().is_none());
    let (accepted, retired) = state.accept(failed(second_request, "after clear"), 7, &source);
    assert!(!accepted && retired.is_none());
    assert!(state.due_request(now + Duration::from_secs(1), 0).is_none());
}

#[test]
fn image_viewer_preview_region_source_identity_and_failed_refresh_keep_old_surface() {
    let first_source = source();
    let same_bytes_other_source = source();
    assert_eq!(first_source.input(), same_bytes_other_source.input());
    assert!(!Arc::ptr_eq(&first_source, &same_bytes_other_source));
    let now = Instant::now();
    let mut state = State::default();
    state
        .set_target(1, &first_source, rect(0, 0, 2, 1), now)
        .unwrap();
    let request = state
        .due_request(now + Duration::from_millis(120), 0)
        .unwrap();
    let (accepted, retired) = state.accept(complete(request), 1, &first_source);
    assert!(accepted && retired.is_none());
    let old = Arc::clone(&state.surface().unwrap().image);

    let next_time = now + Duration::from_millis(121);
    let change = state
        .set_target(1, &first_source, rect(1, 0, 2, 1), next_time)
        .unwrap();
    assert!(change.changed && change.retired.is_none());
    assert!(Arc::ptr_eq(&state.surface().unwrap().image, &old));
    let request = state
        .due_request(now + Duration::from_millis(241), 0)
        .unwrap();
    let foreign = Completion {
        serial: request.serial,
        epoch: request.epoch,
        source: Arc::clone(&same_bytes_other_source),
        region: request.region,
        result: Err("foreign source".into()),
    };
    let (accepted, retired) = state.accept(foreign, 1, &first_source);
    assert!(!accepted && retired.is_none());
    assert!(state.pending() && state.error().is_none());
    let (accepted, retired) = state.accept(failed(request, "decode failed"), 1, &first_source);
    assert!(accepted && retired.is_none());
    assert_eq!(state.error(), Some("decode failed"));
    assert!(!state.pending());
    assert!(Arc::ptr_eq(&state.surface().unwrap().image, &old));
    let change = state
        .set_target(1, &same_bytes_other_source, rect(1, 0, 2, 1), next_time)
        .unwrap();
    assert!(change.changed);
    assert!(Arc::ptr_eq(&change.retired.unwrap(), &old));
    assert!(state.surface().is_none() && state.error().is_none() && state.pending());
}

#[test]
fn image_viewer_preview_region_complete_moves_raw_alpha_and_cancel_wins() {
    let source = source();
    let cancel = Arc::new(AtomicBool::new(false));
    let completion = complete(Request {
        serial: 9,
        epoch: 4,
        source: Arc::clone(&source),
        region: Rect::full(4, 2),
        external_other_resident: 0,
        cancel: Arc::clone(&cancel),
    });
    assert_eq!((completion.serial, completion.epoch), (9, 4));
    assert!(Arc::ptr_eq(&completion.source, &source));
    assert_eq!(completion.region, Rect::full(4, 2));
    let surface = completion.result.unwrap();
    assert_eq!(surface.geometry.raster_dimensions(), (4, 2));
    assert_eq!(
        surface.geometry.normalized_source_region(),
        Rect::full(4, 2)
    );
    // 透明像素保留隐藏RGB，alpha不被棋盘烘焙为255；这里不需GPU或窗口。
    let row = [
        30, 20, 10, 0, 60, 50, 40, 128, 90, 80, 70, 255, 120, 110, 100, 64,
    ];
    assert_eq!(surface.image.as_bytes(0).unwrap(), row.repeat(2));

    cancel.store(true, Ordering::Release);
    let cancelled = complete(Request {
        serial: 9,
        epoch: 5,
        source: Arc::clone(&source),
        region: rect(4, 0, 1, 1),
        external_other_resident: u64::MAX,
        cancel,
    });
    assert_eq!(
        cancelled.result.err().unwrap(),
        crate::tr!("PNG预览已取消", "PNG preview canceled")
    );
    let budget_failed = complete(Request {
        serial: 9,
        epoch: 6,
        source,
        region: Rect::full(4, 2),
        external_other_resident: u64::MAX,
        cancel: Arc::new(AtomicBool::new(false)),
    });
    assert!(budget_failed.result.is_err());
}
