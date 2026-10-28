use super::*;

fn png() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[10, 20, 30, 0, 40, 50, 60, 128])
            .unwrap();
        writer.finish().unwrap();
    }
    output
}

fn limits() -> PreviewLimits {
    PreviewLimits {
        max_side: 2,
        max_pixels: 2,
    }
}

fn opened() -> Arc<FrozenPreviewSource> {
    FrozenPreviewSource::open(png(), 0, limits(), &AtomicBool::new(false))
        .unwrap()
        .0
}

#[test]
fn image_preview_source_factory_keeps_the_owned_allocation_and_real_pixels() {
    let cancel = AtomicBool::new(false);
    let mut bytes = png();
    bytes.reserve_exact(4096);
    let pointer = bytes.as_ptr();
    let capacity = bytes.capacity() as u64;
    let length = bytes.len() as u64;
    let direct = crate::image_preview::png::decode(
        crate::image_preview::png::inspect(&bytes, capacity, 0, limits(), &cancel).unwrap(),
        &cancel,
    )
    .unwrap();
    let (source, preview) = FrozenPreviewSource::open(bytes, 0, limits(), &cancel).unwrap();
    assert_eq!(source.input().as_ptr(), pointer);
    assert_eq!(source.input_reserved_bytes(), capacity);
    assert_eq!(source.encoded_bytes(), length);
    assert_eq!(source.retained_bytes(), capacity + CONTROL_SLOT);
    assert_eq!(source.geometry(), direct.geometry);
    assert_eq!(source.geometry(), preview.geometry);
    assert_eq!(source.source(), direct.source);
    assert_eq!(source.source(), preview.source);
    assert_eq!(preview.image.bgra, direct.image.bgra);
    assert_eq!(preview.image.bgra, [30, 20, 10, 0, 60, 50, 40, 128]);
    assert_eq!(source.geometry().encoded_dimensions(), (2, 1));
    assert_eq!(source.geometry().raster_dimensions(), (2, 1));
    assert_eq!(preview.sampling, direct.sampling);
    assert_eq!(preview.color, direct.color);
    let alias = Arc::clone(&source);
    assert!(Arc::ptr_eq(&source, &alias));
    assert_eq!(alias.input().as_ptr(), pointer);
}

#[test]
fn image_preview_source_factory_checks_both_phases_and_prioritizes_cancel() {
    let cancel = AtomicBool::new(false);
    let bytes = png();
    let capacity = bytes.capacity() as u64;
    let workspace = crate::image_preview::png::inspect(&bytes, capacity, 0, limits(), &cancel)
        .unwrap()
        .workspace();
    let exact = MAX_BYTES - capacity - workspace.peak_bytes();
    let (source, preview) = FrozenPreviewSource::open(bytes, exact, limits(), &cancel).unwrap();

    let retained_exact = MAX_BYTES - source.retained_bytes() - preview.image.bgra.capacity() as u64;
    assert!(check_retained_budget(
        retained_exact,
        source.input_reserved_bytes(),
        preview.image.bgra.capacity() as u64,
    )
    .is_ok());
    assert!(check_retained_budget(
        retained_exact + 1,
        source.input_reserved_bytes(),
        preview.image.bgra.capacity() as u64,
    )
    .is_err());

    let bytes = png();
    let capacity = bytes.capacity() as u64;
    let peak = crate::image_preview::png::inspect(&bytes, capacity, 0, limits(), &cancel)
        .unwrap()
        .workspace()
        .peak_bytes();
    assert!(
        FrozenPreviewSource::open(bytes, MAX_BYTES - capacity - peak + 1, limits(), &cancel)
            .is_err()
    );
    assert!(FrozenPreviewSource::open(png(), u64::MAX, limits(), &cancel).is_err());
    assert!(FrozenPreviewSource::open(b"bad PNG".to_vec(), 0, limits(), &cancel).is_err());
    assert!(FrozenPreviewSource::open(
        png(),
        0,
        PreviewLimits {
            max_side: 0,
            max_pixels: 0
        },
        &cancel,
    )
    .is_err());

    cancel.store(true, Ordering::Release);
    let error = FrozenPreviewSource::open(b"bad PNG".to_vec(), u64::MAX, limits(), &cancel)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        crate::tr!("PNG预览已取消", "PNG preview canceled")
    );
}

#[test]
fn image_preview_source_residency_deduplicates_exact_identity_and_checked_exclusion() {
    let first = opened();
    let alias = Arc::clone(&first);
    let second = opened();
    assert!(!Arc::ptr_eq(&first, &second));
    // 两份内容相同的独立Vec仍要各计一次，Arc别名只计一次。
    assert_eq!(first.input(), second.input());
    let all = [&first, &alias, &second, &second];
    assert_eq!(
        source_resident_bytes(&all, None).unwrap(),
        first.retained_bytes() + second.retained_bytes()
    );
    assert_eq!(
        source_resident_bytes(&all, Some(&alias)).unwrap(),
        second.retained_bytes()
    );
    assert_eq!(
        source_resident_bytes(&all, Some(&second)).unwrap(),
        first.retained_bytes()
    );
    assert_eq!(
        source_resident_bytes(&[&first, &alias], Some(&first)).unwrap(),
        0
    );
    assert_eq!(source_resident_bytes(&[], Some(&first)).unwrap(), 0);
    let outside = opened();
    assert_eq!(
        source_resident_bytes(&all, Some(&outside)).unwrap(),
        first.retained_bytes() + second.retained_bytes()
    );
    // 不伪造私有来源或Vec容量；只检查求和器真正使用的算术溢出边界。
    assert_eq!(add(u64::MAX - 1, 1).unwrap(), u64::MAX);
    assert!(add(u64::MAX, 1).is_err());
}
