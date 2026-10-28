use super::*;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn viewport(dimensions: (u32, u32), zoom: f64, offset: (f64, f64)) -> PreviewViewport {
    let mut viewport = PreviewViewport::new(dimensions).unwrap();
    viewport.set_zoom(zoom, (0., 0.)).unwrap();
    viewport.set_offset(offset).unwrap();
    viewport
}

#[test]
fn image_viewer_preview_paint_far_edge_uses_local_cells_for_overview_and_old_detail() {
    let viewport = viewport((20_000_000, 2), 8., (-159_999_981., 0.));
    let area = rect(12, 0, 20, 8);
    let domain = Rect::full(20_000_000, 2);
    let full = project(domain, viewport).unwrap();
    assert_eq!(full.left + full.width, 19.);
    assert_eq!(full.left as f32 + full.width as f32, 16.);
    assert!(!texture_is_precise(full, 1.));

    for (domain, raster, last_x) in [
        (domain, (2048, 1), 2047),
        (rect(10_000_000, 0, 10_000_000, 2), (1024, 1), 1023),
    ] {
        let plan = LayerPlan::new(
            domain,
            raster,
            viewport,
            [Some(domain), None, None, None],
            area,
            1.,
            false,
        )
        .unwrap();
        let LayerPlan::Texels(texels) = plan else {
            panic!("expected local cells")
        };
        let cells: Vec<_> = texels.cells().collect();
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].x, cells[0].y), (last_x, 0));
        assert_eq!(
            cells[0].rect,
            PhysicalRect {
                left: 12.,
                top: 0.,
                width: 7.,
                height: 8.
            }
        );
        let mut count = 0;
        LayerPlan::Texels(texels).preflight(1., &mut count).unwrap();
        assert_eq!(count, 1);
    }
}

#[test]
fn image_viewer_preview_paint_normal_projection_keeps_texture_sampling() {
    let viewport = viewport((4, 3), 1.25, (5.5, 7.25));
    let domain = Rect::full(4, 3);
    let plan = LayerPlan::new(
        domain,
        (4, 3),
        viewport,
        [Some(domain), None, None, None],
        Rect::full(100, 100),
        1.1,
        true,
    )
    .unwrap();
    let mut count = 0;
    plan.preflight(1.1, &mut count).unwrap();
    assert_eq!(count, 0);
    let LayerPlan::Texture {
        projected,
        clips,
        nearest,
    } = plan
    else {
        panic!("expected texture")
    };
    assert!(nearest);
    assert_eq!(
        projected,
        PhysicalRect {
            left: 5.5,
            top: 7.25,
            width: 5.,
            height: 3.75
        }
    );
    assert_eq!(clips, [Some(projected), None, None, None]);
    // 逻辑边可精确相加，逐项乘DPI后的f32物理边仍可能再次消抵。
    for (dpi, left, expected_right) in [
        (1.25f32, -16_777_214f32, 2.5),
        (1.1f32, -16_777_215f32, 1.100000023841858),
    ] {
        let width = 16_777_216f32;
        let projected = PhysicalRect {
            left: f64::from(left) * f64::from(dpi),
            top: 0.,
            width: f64::from(width) * f64::from(dpi),
            height: 10.,
        };
        assert!((projected.left + projected.width - expected_right).abs() < 1e-8);
        assert!((f64::from(left + width) * f64::from(dpi) - expected_right).abs() < 1e-8);
        assert_eq!(left * dpi + width * dpi, 2.);
        assert!(!texture_is_precise(projected, dpi));
    }
}

#[test]
fn image_viewer_preview_paint_cell_masks_are_disjoint_and_keep_straight_alpha() {
    let viewport = viewport((8, 6), 1., (0., 0.));
    let domain = Rect::full(8, 6);
    let detail = rect(2, 2, 4, 2);
    let overview = TexelPlan::new(
        domain,
        (4, 3),
        viewport,
        overview_regions((8, 6), Some(detail)).unwrap(),
        domain,
    )
    .unwrap();
    let detail_plan = TexelPlan::new(
        detail,
        (2, 1),
        viewport,
        [Some(detail), None, None, None],
        domain,
    )
    .unwrap();
    let overview: Vec<_> = overview.cells().collect();
    let detail_cells: Vec<_> = detail_plan.cells().collect();
    let area = |cells: &[TexelCell]| {
        cells
            .iter()
            .map(|cell| cell.rect.width * cell.rect.height)
            .sum::<f64>()
    };
    assert_eq!(area(&overview), 40.);
    assert_eq!(area(&detail_cells), 8.);
    for y in 0..6 {
        for x in 0..8 {
            let contains = |cell: &&TexelCell| {
                let (x, y) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                x >= cell.rect.left
                    && x < cell.rect.left + cell.rect.width
                    && y >= cell.rect.top
                    && y < cell.rect.top + cell.rect.height
            };
            let outside = overview.iter().filter(contains).count();
            let inside = detail_cells.iter().filter(contains).count();
            assert_eq!(outside + inside, 1);
            assert_eq!(
                inside,
                usize::from((2..6).contains(&x) && (2..4).contains(&y))
            );
        }
    }
    // 透明详情所在格没有总览层；颜色保持直alpha，由同一个棋盘底参与混合。
    assert_eq!(rgba_word(&[30, 20, 10, 0]), 0x0a141e00);
    assert_eq!(rgba_word(&[60, 50, 40, 128]), 0x28323c80);
    assert_eq!(rgba_word(&[90, 80, 70, 255]), 0x46505aff);
}

#[test]
fn image_viewer_preview_paint_fallback_work_and_combined_layers_have_fixed_caps() {
    let viewport = viewport((65_535, 65_535), 1., (0., 0.));
    let domain = Rect::full(64, 64);
    let plan = TexelPlan::new(
        domain,
        (64, 64),
        viewport,
        [Some(domain), None, None, None],
        domain,
    )
    .unwrap();
    let mut count = 0;
    LayerPlan::Texels(plan).preflight(1., &mut count).unwrap();
    assert_eq!(count, 4096);
    let one = Rect::full(1, 1);
    let one = TexelPlan::new(one, (1, 1), viewport, [Some(one), None, None, None], domain).unwrap();
    assert!(LayerPlan::Texels(one).preflight(1., &mut count).is_err());
    let huge = Rect::full(65_535, 65_535);
    assert!(TexelPlan::new(
        huge,
        (65_535, 65_535),
        viewport,
        [Some(huge), None, None, None],
        huge
    )
    .is_err());
}

#[test]
fn image_viewer_preview_paint_rejects_unrepresentable_bounds_and_invalid_clips() {
    let unit = PhysicalRect {
        left: 1.,
        top: 1.,
        width: 1.,
        height: 1.,
    };
    assert!(bounds(unit, 1.).is_ok());
    for dpi in [0., -1., f32::NAN, f32::INFINITY, f32::from_bits(1)] {
        assert!(bounds(unit, dpi).is_err());
        assert!(!texture_is_precise(unit, dpi));
    }
    assert!(bounds(
        PhysicalRect {
            left: 16_777_216.,
            ..unit
        },
        1.
    )
    .is_err());
    let viewport = viewport((100, 100), 1., (0., 0.));
    let domain = Rect::full(100, 100);
    assert!(TexelPlan::new(
        domain,
        (1, 1),
        viewport,
        [Some(domain), None, None, None],
        rect(u32::MAX, 0, 2, 1)
    )
    .is_err());
    assert!(TexelPlan::new(
        domain,
        (0, 1),
        viewport,
        [Some(domain), None, None, None],
        domain
    )
    .is_err());
}
