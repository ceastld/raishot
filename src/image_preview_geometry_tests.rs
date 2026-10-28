use super::*;

#[test]
fn image_preview_geometry_preserves_sampled_centers_in_all_orientations() {
    // 固定表以编码图8×6的采样中心x=1,3,5,7 / y=1,3,5为依据。
    // 尤其反射后首格是原图坐标0，不能重新取归正域的中心1。
    let first_encoded = [
        (1, 1),
        (7, 1),
        (7, 5),
        (1, 5),
        (1, 1),
        (1, 5),
        (7, 5),
        (7, 1),
    ];
    let first_normalized = [
        (1, 1),
        (0, 1),
        (0, 0),
        (1, 0),
        (1, 1),
        (0, 1),
        (0, 0),
        (1, 0),
    ];
    let last_normalized = [
        (7, 5),
        (6, 5),
        (6, 4),
        (7, 4),
        (5, 7),
        (4, 7),
        (4, 6),
        (5, 6),
    ];
    for exif in 1..=8 {
        let orientation = Orientation::from_exif(exif).unwrap();
        let geometry = PreviewGeometry::new((8, 6), (4, 3), Some(orientation)).unwrap();
        let expected_dimensions = if exif < 5 { (8, 6) } else { (6, 8) };
        let expected_raster = if exif < 5 { (4, 3) } else { (3, 4) };
        assert_eq!(geometry.encoded_dimensions(), (8, 6));
        assert_eq!(geometry.sampled_encoded_dimensions(), (4, 3));
        assert_eq!(geometry.normalized_source_dimensions(), expected_dimensions);
        assert_eq!(geometry.raster_dimensions(), expected_raster);
        assert_eq!(geometry.source_orientation(), Some(orientation));
        let first = geometry.sample_at_source(0., 0.).unwrap();
        assert_eq!(first.raster_pixel, (0, 0));
        assert_eq!(
            first.encoded_source_pixel,
            first_encoded[usize::from(exif - 1)]
        );
        assert_eq!(
            first.normalized_source_pixel,
            first_normalized[usize::from(exif - 1)]
        );
        let last = geometry
            .sample_at_source(
                f64::from(expected_dimensions.0) - 0.25,
                f64::from(expected_dimensions.1) - 0.25,
            )
            .unwrap();
        assert_eq!(
            last.raster_pixel,
            (expected_raster.0 - 1, expected_raster.1 - 1)
        );
        assert_eq!(
            last.normalized_source_pixel,
            last_normalized[usize::from(exif - 1)]
        );
    }
}

#[test]
fn image_preview_geometry_keeps_large_source_separate_from_raster() {
    for (source, raster, expected_last) in [
        ((200_000, 1), (2000, 1), (199_950, 0)),
        ((800, 50_000), (80, 500), (795, 49_950)),
        ((u32::MAX, u32::MAX), (1, 1), (2_147_483_647, 2_147_483_647)),
    ] {
        let geometry = PreviewGeometry::new(source, raster, None).unwrap();
        assert_eq!(geometry.encoded_dimensions(), source);
        assert_eq!(geometry.normalized_source_dimensions(), source);
        assert_eq!(geometry.raster_dimensions(), raster);
        assert_eq!(geometry.source_orientation(), None);
        assert_eq!(
            geometry
                .sample_at_source(f64::from(source.0) - 0.25, f64::from(source.1) - 0.25)
                .unwrap()
                .encoded_source_pixel,
            expected_last
        );
    }
}

#[test]
fn image_preview_geometry_rejects_invalid_rasters_and_source_coordinates() {
    for (source, raster) in [
        ((0, 10), (1, 1)),
        ((10, 0), (1, 1)),
        ((10, 10), (0, 1)),
        ((10, 10), (1, 0)),
        ((10, 10), (11, 1)),
        ((10, 10), (1, 11)),
        ((100_000, 100_000), (65_536, 1)),
        ((100_000, 100_000), (1, 65_536)),
        ((100_000, 100_000), (8000, 4001)),
    ] {
        assert!(PreviewGeometry::new(source, raster, None).is_err());
    }
    let geometry = PreviewGeometry::new((8, 6), (4, 3), None).unwrap();
    for (x, y) in [
        (-0.01, 0.),
        (0., -0.01),
        (8., 0.),
        (0., 6.),
        (f64::NAN, 0.),
        (0., f64::NAN),
        (f64::INFINITY, 0.),
        (0., f64::NEG_INFINITY),
    ] {
        assert_eq!(geometry.sample_at_source(x, y), None);
    }
    assert_eq!(
        geometry.sample_at_source(1.999, 0.).unwrap().raster_pixel,
        (0, 0)
    );
    assert_eq!(
        geometry.sample_at_source(2., 0.).unwrap().raster_pixel,
        (1, 0)
    );
}
