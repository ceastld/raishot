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
fn image_preview_viewport_large_source_fit_and_far_end_anchor_keep_subpixels() {
    let mut viewport = PreviewViewport::new((20_000_000, 200)).unwrap();
    assert_eq!(viewport.source_dimensions(), (20_000_000, 200));
    viewport.fit(rect(10, 20, 1000, 600)).unwrap();
    assert_eq!(viewport.zoom(), 0.000_05);
    assert!(viewport.zoom() < 1e-4);
    assert_eq!(viewport.offset().0, 10.);
    assert!((viewport.offset().1 - 319.995).abs() < 1e-12);

    viewport.set_zoom(1., (0., 0.)).unwrap();
    viewport.set_offset((-19_999_999.75, -9.5)).unwrap();
    assert_eq!(
        viewport.window_point((19_999_999., 10.)),
        Some((-0.75, 0.5))
    );
    assert_eq!(viewport.window_point((20_000_000., 10.)), Some((0.25, 0.5)));
    let anchor = (0.125, 0.375);
    let at_anchor = viewport.source_point(anchor).unwrap();
    assert_eq!(at_anchor, (19_999_999.875, 9.875));
    viewport.set_zoom(8., anchor).unwrap();
    assert_eq!(viewport.zoom(), 8.);
    assert_eq!(viewport.offset(), (-159_999_998.875, -78.625));
    assert_eq!(viewport.window_point(at_anchor), Some(anchor));
    assert_eq!(viewport.source_point(anchor), Some(at_anchor));
    assert_eq!(
        viewport.window_point((at_anchor.0 + 0.125, at_anchor.1)),
        Some((anchor.0 + 1., anchor.1))
    );
}

#[test]
fn image_preview_viewport_visible_half_open_regions_padding_and_offscreen() {
    let mut viewport = PreviewViewport::new((1000, 800)).unwrap();
    viewport.set_zoom(2., (0., 0.)).unwrap();
    viewport.set_offset((-20.5, -10.25)).unwrap();
    assert_eq!(
        viewport.visible_region(rect(0, 0, 100, 80), 0).unwrap(),
        Some(rect(10, 5, 51, 41))
    );
    assert_eq!(
        viewport.visible_region(rect(0, 0, 100, 80), 2).unwrap(),
        Some(rect(8, 3, 55, 45))
    );
    assert_eq!(
        viewport
            .visible_region(rect(0, 0, 100, 80), u32::MAX)
            .unwrap(),
        Some(Rect::full(1000, 800))
    );
    viewport.set_offset((100., 0.)).unwrap();
    assert_eq!(
        viewport.visible_region(rect(0, 0, 100, 80), 50).unwrap(),
        None
    );
    viewport.set_offset((-2000., 0.)).unwrap();
    assert_eq!(
        viewport.visible_region(rect(0, 0, 100, 80), 50).unwrap(),
        None
    );
    viewport.set_offset((0.25, 0.5)).unwrap();
    assert_eq!(
        viewport.visible_region(rect(0, 0, 1, 1), 0).unwrap(),
        Some(rect(0, 0, 1, 1))
    );
    assert!(viewport.visible_region(rect(0, 0, 0, 1), 0).is_err());
    assert!(viewport.visible_region(rect(u32::MAX, 0, 1, 1), 0).is_err());
}

#[test]
fn image_preview_viewport_u32_geometry_and_invalid_updates_are_atomic() {
    assert!(PreviewViewport::new((0, 1)).is_err());
    assert!(PreviewViewport::new((1, 0)).is_err());
    let mut thin = PreviewViewport::new((u32::MAX, 1)).unwrap();
    thin.fit(Rect::full(1000, 600)).unwrap();
    let thin_bounds = thin.projected_bounds(Rect::full(u32::MAX, 1)).unwrap();
    assert!(thin_bounds.width.is_finite() && thin_bounds.height > 0.);
    assert_eq!(
        thin.visible_region(Rect::full(1000, 600), 0).unwrap(),
        Some(Rect::full(u32::MAX, 1))
    );
    let mut viewport = PreviewViewport::new((u32::MAX, u32::MAX)).unwrap();
    viewport.fit(Rect::full(1, 1)).unwrap();
    assert_eq!(viewport.zoom(), 1. / f64::from(u32::MAX));
    assert_eq!(
        viewport.visible_region(Rect::full(1, 1), 0).unwrap(),
        Some(Rect::full(u32::MAX, u32::MAX))
    );
    viewport.set_zoom(8., (0., 0.)).unwrap();
    viewport
        .set_offset((-8. * f64::from(u32::MAX) + 8., 0.))
        .unwrap();
    assert_eq!(
        viewport.visible_region(Rect::full(8, 8), 0).unwrap(),
        Some(rect(u32::MAX - 1, 0, 1, 1))
    );
    assert_eq!(
        viewport.window_point((f64::from(u32::MAX), 0.)),
        Some((8., 0.))
    );

    let original = viewport;
    for zoom in [0., -1., f64::NAN, f64::INFINITY] {
        assert!(viewport.set_zoom(zoom, (1., 1.)).is_err());
        assert_eq!(viewport, original);
    }
    for anchor in [(f64::NAN, 0.), (0., f64::INFINITY)] {
        assert!(viewport.set_zoom(1., anchor).is_err());
        assert_eq!(viewport, original);
    }
    for offset in [(f64::NAN, 0.), (0., f64::INFINITY), (f64::MAX, 0.)] {
        assert!(viewport.set_offset(offset).is_err());
        assert_eq!(viewport, original);
    }
    for area in [rect(0, 0, 0, 1), rect(u32::MAX, 0, 1, 1)] {
        assert!(viewport.fit(area).is_err());
        assert_eq!(viewport, original);
    }
    assert_eq!(viewport.window_point((f64::NAN, 0.)), None);
    assert_eq!(viewport.window_point((f64::MAX, 0.)), None);
    assert_eq!(viewport.source_point((0., f64::INFINITY)), None);
}

#[test]
fn image_preview_viewport_projects_nonzero_roi_edges_without_rounding() {
    let mut viewport = PreviewViewport::new((100, 80)).unwrap();
    viewport.set_zoom(0.5, (0., 0.)).unwrap();
    viewport.set_offset((10.25, -2.75)).unwrap();
    assert_eq!(
        viewport.projected_bounds(rect(3, 5, 7, 9)).unwrap(),
        PhysicalRect {
            left: 11.75,
            top: -0.25,
            width: 3.5,
            height: 4.5
        }
    );
    assert_eq!(viewport.source_point((11.75, -0.25)), Some((3., 5.)));
    assert!(viewport.projected_bounds(rect(100, 0, 1, 1)).is_err());
    assert!(viewport.projected_bounds(rect(0, 80, 1, 1)).is_err());
    assert!(viewport.projected_bounds(rect(0, 0, 0, 1)).is_err());
    assert!(viewport.projected_bounds(rect(u32::MAX, 0, 1, 1)).is_err());
    viewport.set_zoom(f64::MIN_POSITIVE, (0., 0.)).unwrap();
    assert_eq!(viewport.zoom(), 0.01);
    viewport.set_zoom(f64::MAX, (0., 0.)).unwrap();
    assert_eq!(viewport.zoom(), 8.);
}
