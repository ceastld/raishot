// 复用父测试模块的真实TIFF目录与像素样本，不依赖外部文件或原生组件。
use super::*;

const LIMIT: u64 = crate::image_decode::MAX_DECODE_BYTES;

#[test]
fn image_tiff_resident_exact_boundary_preserves_legacy_pixels_and_eight_orientations() {
    for endian in [Endian::Little, Endian::Big] {
        for mut spec in [rgb(), rgba()] {
            for orientation in 1..=8 {
                spec.orientation = orientation;
                let data = fixture(endian, &[spec.clone()]);
                let mut source =
                    TiffSource::from_reader(Cursor::new(data.bytes), &token()).unwrap();
                let output = u64::from(spec.width) * u64::from(spec.height) * 4;
                let row = u64::from(spec.width) * u64::from(spec.samples);
                let resident = LIMIT - source.retained_bytes().unwrap() - output - row;
                let before = source.reader.position();
                assert!(source
                    .decode_with_resident(0, output, resident + 1, &token())
                    .is_err());
                assert_eq!(source.reader.position(), before);
                let shared = source
                    .decode_with_resident(0, output, resident, &token())
                    .unwrap();
                assert_eq!(shared.image.bgra.len() as u64, output);
                assert!(shared.image.bgra.capacity() as u64 <= output);
                assert_eq!(shared.image.bgra, expected(&spec));
                assert_eq!(
                    shared.metadata.orientation,
                    Some(source.pages()[0].orientation)
                );
                let legacy = source.decode(0, output, &token()).unwrap();
                assert_eq!(legacy.image.bgra, shared.image.bgra);
                assert_eq!(legacy.metadata, shared.metadata);
                assert!(source
                    .decode_with_resident(0, output - 1, 0, &token())
                    .is_err());
            }
        }
    }
}

#[test]
fn image_tiff_resident_counts_directory_strip_and_error_capacities() {
    let mut unsupported = rgb();
    unsupported.compression = 5;
    let data = fixture(Endian::Little, &[rgb(), unsupported]);
    let mut source = TiffSource::from_reader(Cursor::new(data.bytes), &token()).unwrap();
    let old = source.retained_bytes().unwrap();
    let output = 24;
    let row = 9;
    let old_grant = LIMIT - old - output - row;

    let old_pages = source.pages.capacity();
    source.pages.reserve(128);
    let mut added =
        ((source.pages.capacity() - old_pages) * std::mem::size_of::<TiffPage>()) as u64;
    let old_layouts = source.layouts.capacity();
    source.layouts.reserve(128);
    added += ((source.layouts.capacity() - old_layouts) * std::mem::size_of::<Option<PageLayout>>())
        as u64;
    let strips = &mut source.layouts[0].as_mut().unwrap().strips;
    let old_strips = strips.capacity();
    strips.reserve(128);
    added += ((strips.capacity() - old_strips) * std::mem::size_of::<Strip>()) as u64;
    let reason = source.pages[1].error.as_mut().unwrap();
    let old_reason = reason.capacity();
    let old_length = reason.len();
    reason.reserve(4096);
    added += (reason.capacity() - old_reason) as u64;
    assert_eq!(reason.len(), old_length);
    assert!(reason.capacity() > reason.len());
    assert_eq!(source.retained_bytes().unwrap(), old + added);
    assert!(added > 0);

    let before = source.reader.position();
    assert!(source
        .decode_with_resident(0, output, old_grant, &token())
        .is_err());
    assert_eq!(source.reader.position(), before);
    let refreshed = old_grant - added;
    let decoded = source
        .decode_with_resident(0, output, refreshed, &token())
        .unwrap();
    assert_eq!(decoded.image.bgra, expected(&rgb()));
    assert!(!source.pages()[1].supported);
}

#[test]
fn image_tiff_resident_budget_uses_actual_output_capacity_and_checked_sums() {
    let mut output = Vec::with_capacity(96);
    output.resize(24, 0u8);
    let row = vec![0u8; 9];
    let resident = LIMIT - output.len() as u64 - row.len() as u64;
    assert!(resident_budget(&[resident, output.len() as u64, row.len() as u64]).is_ok());
    assert!(resident_budget(&[
        resident,
        capacity_bytes(&output).unwrap(),
        capacity_bytes(&row).unwrap()
    ])
    .is_err());
    assert!(resident_budget(&[u64::MAX, 1]).is_err());
    assert!(resident_budget(&[LIMIT, 1]).is_err());
}

#[test]
fn image_tiff_resident_cancellation_precedes_budget_and_read_errors_without_poisoning_source() {
    let data = fixture(Endian::Big, &[rgba()]);
    let cancel = Arc::new(token());
    let reader = mutable(data.bytes, cancel.clone());
    let armed = reader.cancel_on_read.clone();
    let reads = reader.reads.clone();
    let mut source = TiffSource::from_reader(reader, &cancel).unwrap();
    let before = reads.load(Ordering::Acquire);
    cancel.store(true, Ordering::Release);
    let expected_cancel = cancelled(&cancel).unwrap_err().to_string();
    let error = source
        .decode_with_resident(0, 8, LIMIT, &cancel)
        .err()
        .unwrap();
    assert_eq!(error, expected_cancel);
    assert_eq!(reads.load(Ordering::Acquire), before);

    cancel.store(false, Ordering::Release);
    armed.store(true, Ordering::Release);
    let error = source
        .decode_with_resident(0, 8, 1024, &cancel)
        .err()
        .unwrap();
    assert_eq!(error, expected_cancel);
    assert!(cancel.load(Ordering::Acquire));
    cancel.store(false, Ordering::Release);
    let decoded = source.decode_with_resident(0, 8, 1024, &cancel).unwrap();
    assert_eq!(decoded.image.bgra, expected(&rgba()));
}
