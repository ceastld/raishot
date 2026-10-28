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
fn image_preview_region_geometry_all_orientations_keep_global_sample_centers() {
    // 同一编码ROI(2,1,8,5)，采样3×2：编码中心x=3/6/8、y=2/4。
    // 宽8不能被3整除，水平反射后归正中心是6/8/11，不能重采成6/9/11。
    type Case = (Rect, (u32, u32), [u32; 6], [(u32, u32); 6]);
    let cases: [Case; 8] = [
        (
            rect(2, 1, 8, 5),
            (3, 2),
            [33, 36, 38, 63, 66, 68],
            [(3, 2), (6, 2), (8, 2), (3, 4), (6, 4), (8, 4)],
        ),
        (
            rect(5, 1, 8, 5),
            (3, 2),
            [38, 36, 33, 68, 66, 63],
            [(6, 2), (8, 2), (11, 2), (6, 4), (8, 4), (11, 4)],
        ),
        (
            rect(5, 3, 8, 5),
            (3, 2),
            [68, 66, 63, 38, 36, 33],
            [(6, 4), (8, 4), (11, 4), (6, 6), (8, 6), (11, 6)],
        ),
        (
            rect(2, 3, 8, 5),
            (3, 2),
            [63, 66, 68, 33, 36, 38],
            [(3, 4), (6, 4), (8, 4), (3, 6), (6, 6), (8, 6)],
        ),
        (
            rect(1, 2, 5, 8),
            (2, 3),
            [33, 63, 36, 66, 38, 68],
            [(2, 3), (4, 3), (2, 6), (4, 6), (2, 8), (4, 8)],
        ),
        (
            rect(3, 2, 5, 8),
            (2, 3),
            [63, 33, 66, 36, 68, 38],
            [(4, 3), (6, 3), (4, 6), (6, 6), (4, 8), (6, 8)],
        ),
        (
            rect(3, 5, 5, 8),
            (2, 3),
            [68, 38, 66, 36, 63, 33],
            [(4, 6), (6, 6), (4, 8), (6, 8), (4, 11), (6, 11)],
        ),
        (
            rect(1, 5, 5, 8),
            (2, 3),
            [38, 68, 36, 66, 33, 63],
            [(2, 6), (4, 6), (2, 8), (4, 8), (2, 11), (4, 11)],
        ),
    ];
    for (index, (region, raster, ids, source_pixels)) in cases.into_iter().enumerate() {
        let orientation = Orientation::from_exif(index as u8 + 1).unwrap();
        let geometry = RegionGeometry::new((15, 9), region, (3, 2), Some(orientation)).unwrap();
        assert_eq!(geometry.encoded_dimensions(), (15, 9));
        assert_eq!(geometry.encoded_region(), rect(2, 1, 8, 5));
        assert_eq!(geometry.normalized_source_region(), region);
        assert_eq!(geometry.sampled_encoded_dimensions(), (3, 2));
        assert_eq!(geometry.raster_dimensions(), raster);
        assert_eq!(geometry.source_orientation(), Some(orientation));
        assert_eq!(
            geometry.normalized_source_dimensions(),
            if index < 4 { (15, 9) } else { (9, 15) }
        );
        for i in 0..6u32 {
            let raster_pixel = (i % raster.0, i / raster.0);
            let x = f64::from(region.x)
                + (f64::from(raster_pixel.0) + 0.5) * f64::from(region.width) / f64::from(raster.0);
            let y = f64::from(region.y)
                + (f64::from(raster_pixel.1) + 0.5) * f64::from(region.height)
                    / f64::from(raster.1);
            let sample = geometry.sample_at_source(x, y).unwrap();
            assert_eq!(sample.raster_pixel, raster_pixel);
            assert_eq!(
                sample.encoded_source_pixel.1 * 15 + sample.encoded_source_pixel.0,
                ids[i as usize]
            );
            assert_eq!(sample.normalized_source_pixel, source_pixels[i as usize]);
        }
        // producer的编码采样索引必须落到同一张literal结果表，不含源矩形原点。
        for (sample_index, id) in [33, 36, 38, 63, 66, 68].into_iter().enumerate() {
            let position = geometry
                .raster_at_sampled_encoded(sample_index as u32 % 3, sample_index as u32 / 3)
                .unwrap();
            assert_eq!(ids[(position.1 * raster.0 + position.0) as usize], id);
        }
        assert_eq!(geometry.raster_at_sampled_encoded(3, 0), None);
        assert_eq!(geometry.raster_at_sampled_encoded(0, 2), None);
    }
}

#[test]
fn image_preview_region_geometry_rejects_invalid_rectangles_and_keeps_unknown_orientation() {
    let region = rect(2, 1, 8, 5);
    let unknown = RegionGeometry::new((15, 9), region, (3, 2), None).unwrap();
    let known =
        RegionGeometry::new((15, 9), region, (3, 2), Some(Orientation::NoTransforms)).unwrap();
    assert_eq!(unknown.source_orientation(), None);
    assert_eq!(known.source_orientation(), Some(Orientation::NoTransforms));
    assert_ne!(unknown, known);
    assert_eq!(
        unknown.sample_at_source(2., 1.),
        known.sample_at_source(2., 1.)
    );
    assert_eq!(
        unknown
            .sample_at_source(2., 1.)
            .unwrap()
            .encoded_source_pixel,
        (3, 2)
    );
    assert_eq!(
        unknown
            .sample_at_source(9.99, 5.99)
            .unwrap()
            .encoded_source_pixel,
        (8, 4)
    );
    for (x, y) in [
        (0., 0.),
        (1.99, 1.),
        (2., 0.99),
        (10., 1.),
        (2., 6.),
        (f64::NAN, 1.),
        (2., f64::INFINITY),
        (f64::NEG_INFINITY, 1.),
    ] {
        assert_eq!(unknown.sample_at_source(x, y), None);
    }
    assert_eq!(unknown.raster_at_sampled_encoded(u32::MAX, u32::MAX), None);
    for invalid in [
        rect(2, 1, 0, 5),
        rect(2, 1, 8, 0),
        rect(14, 1, 2, 5),
        rect(2, 8, 8, 2),
        rect(u32::MAX, 0, 2, 1),
        rect(0, u32::MAX, 1, 2),
    ] {
        assert!(RegionGeometry::new((15, 9), invalid, (1, 1), None).is_err());
    }
    for sampled in [(0, 1), (1, 0), (9, 2), (3, 6)] {
        assert!(RegionGeometry::new((15, 9), region, sampled, None).is_err());
    }
    assert!(RegionGeometry::new((0, 9), region, (1, 1), None).is_err());
    assert!(RegionGeometry::new(
        (10_000, 10_000),
        Rect::full(10_000, 10_000),
        (10_000, 10_000),
        None
    )
    .is_err());
    assert!(RegionGeometry::new((200_000, 1), Rect::full(200_000, 1), (65_536, 1), None).is_err());
}

#[test]
fn image_preview_region_geometry_large_source_preserves_crop_origin_and_exact_centers() {
    let region = rect(19_999_989, 1, 11, 1);
    let geometry = RegionGeometry::new(
        (20_000_000, 3),
        region,
        (4, 1),
        Some(Orientation::FlipHorizontal),
    )
    .unwrap();
    assert_eq!(geometry.encoded_dimensions(), (20_000_000, 3));
    assert_eq!(geometry.normalized_source_dimensions(), (20_000_000, 3));
    assert_eq!(geometry.encoded_region(), rect(0, 1, 11, 1));
    assert_eq!(geometry.raster_dimensions(), (4, 1));
    let encoded_centers = [9, 6, 4, 1];
    let normalized_centers = [19_999_990, 19_999_993, 19_999_995, 19_999_998];
    for index in 0..4u32 {
        let sample = geometry
            .sample_at_source(19_999_989. + (f64::from(index) + 0.5) * 11. / 4., 1.5)
            .unwrap();
        assert_eq!(sample.raster_pixel, (index, 0));
        assert_eq!(
            sample.encoded_source_pixel,
            (encoded_centers[index as usize], 1)
        );
        assert_eq!(
            sample.normalized_source_pixel,
            (normalized_centers[index as usize], 1)
        );
    }
    assert_eq!(geometry.sample_at_source(0.5, 1.5), None);
    assert_eq!(geometry.sample_at_source(20_000_000., 1.5), None);
}
