use super::*;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn physical(left: f64, top: f64, width: f64, height: f64) -> PhysicalRect {
    PhysicalRect {
        left,
        top,
        width,
        height,
    }
}

fn contains(rect: Rect, x: u32, y: u32) -> bool {
    x >= rect.x
        && y >= rect.y
        && u64::from(x) < u64::from(rect.x) + u64::from(rect.width)
        && u64::from(y) < u64::from(rect.y) + u64::from(rect.height)
}

#[test]
fn image_preview_paint_overview_masks_are_exact_disjoint_source_regions() {
    let detail = rect(2, 3, 5, 2);
    let masks = overview_regions((10, 8), Some(detail)).unwrap();
    assert_eq!(
        masks,
        [
            Some(rect(0, 0, 10, 3)),
            Some(rect(0, 5, 10, 3)),
            Some(rect(0, 3, 2, 2)),
            Some(rect(7, 3, 3, 2)),
        ]
    );
    let area: u64 = masks
        .iter()
        .flatten()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum();
    assert_eq!(area, 70);
    for y in 0..8 {
        for x in 0..10 {
            let layers = masks
                .iter()
                .flatten()
                .filter(|r| contains(**r, x, y))
                .count();
            assert_eq!(layers, if contains(detail, x, y) { 0 } else { 1 });
        }
    }
    assert_eq!(
        overview_regions((10, 8), None).unwrap(),
        [Some(Rect::full(10, 8)), None, None, None]
    );
    assert_eq!(
        overview_regions((10, 8), Some(Rect::full(10, 8))).unwrap(),
        [None; 4]
    );
    assert_eq!(
        overview_regions((10, 8), Some(rect(0, 0, 2, 3))).unwrap(),
        [None, Some(rect(0, 3, 10, 5)), None, Some(rect(2, 0, 8, 3)),]
    );
    for source in [(0, 8), (10, 0)] {
        assert!(overview_regions(source, None).is_err());
    }
    for invalid in [
        rect(0, 0, 0, 1),
        rect(0, 0, 1, 0),
        rect(9, 0, 2, 1),
        rect(0, 7, 1, 2),
        rect(u32::MAX, 0, 2, 1),
        rect(0, u32::MAX, 1, 2),
    ] {
        assert!(overview_regions((10, 8), Some(invalid)).is_err());
    }
}

#[test]
fn image_preview_paint_transparent_detail_replaces_overview_over_single_checker() {
    let detail = rect(1, 1, 3, 1);
    let masks = overview_regions((5, 3), Some(detail)).unwrap();
    // 一个通道的独立小样：棋盘20、总览200、详情80，alpha依次0/128/255。
    // 透明详情必须露出棋盘20，不能露出已烘焙或仍叠在下面的总览200。
    for (x, alpha, expected) in [(1, 0u32, 20), (2, 128, 50), (3, 255, 80)] {
        let overview_layers = masks
            .iter()
            .flatten()
            .filter(|r| contains(**r, x, 1))
            .count();
        assert_eq!(overview_layers, 0);
        let base = if overview_layers == 0 { 20 } else { 200 };
        let displayed = (80 * alpha + base * (255 - alpha) + 127) / 255;
        assert_eq!(displayed, expected);
    }
    for (x, y) in [(0, 1), (4, 1), (2, 0), (2, 2)] {
        assert_eq!(
            masks
                .iter()
                .flatten()
                .filter(|r| contains(**r, x, y))
                .count(),
            1
        );
    }
}

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 0.000_000_001, "{a} != {b}");
}

#[test]
fn image_preview_paint_checker_keeps_window_phase_and_bounded_clipped_cells() {
    let image = physical(-10.5, -7.25, 80., 60.);
    let area = rect(13, 9, 37, 29);
    let mut grid = CheckerGrid::new(image, area, 1.1).unwrap();
    close(grid.side, 17.6);
    let expected = [
        (physical(13., 9., 4.6, 8.6), false),
        (physical(17.6, 9., 17.6, 8.6), true),
        (physical(35.2, 9., 14.8, 8.6), false),
        (physical(13., 17.6, 4.6, 17.6), true),
        (physical(17.6, 17.6, 17.6, 17.6), false),
        (physical(35.2, 17.6, 14.8, 17.6), true),
        (physical(13., 35.2, 4.6, 2.8), false),
        (physical(17.6, 35.2, 17.6, 2.8), true),
        (physical(35.2, 35.2, 14.8, 2.8), false),
    ];
    let mut total_area = 0.;
    for (expected_rect, alternate) in expected {
        let cell = grid.next().unwrap();
        close(cell.rect.left, expected_rect.left);
        close(cell.rect.top, expected_rect.top);
        close(cell.rect.width, expected_rect.width);
        close(cell.rect.height, expected_rect.height);
        assert_eq!(cell.alternate, alternate);
        assert!(cell.rect.left >= 13. && cell.rect.top >= 9.);
        assert!(cell.rect.left + cell.rect.width <= 50.000_000_001);
        assert!(cell.rect.top + cell.rect.height <= 38.000_000_001);
        total_area += cell.rect.width * cell.rect.height;
    }
    close(total_area, 37. * 29.);
    assert_eq!(grid.next(), None);
    assert_eq!(grid.next(), None);
    assert_eq!(grid.size_hint(), (0, Some(0)));
    // 改变裁剪区不能重置棋盘起点；首格仍是窗口第1列、第0行的alternate。
    let cropped = CheckerGrid::new(image, rect(25, 17, 25, 21), 1.1)
        .unwrap()
        .next()
        .unwrap();
    close(cropped.rect.left, 25.);
    close(cropped.rect.top, 17.);
    assert!(cropped.alternate);

    let mut huge = CheckerGrid::new(
        physical(-0.5, -0.25, 4_000_000_001., 4_000_000_001.),
        Rect::full(4_000_000_000, 4_000_000_000),
        1.,
    )
    .unwrap();
    assert!(huge.side > 16.);
    let mut count = 0;
    for cell in huge.by_ref() {
        assert!(cell.rect.width > 0. && cell.rect.height > 0.);
        assert!(cell.rect.left >= 0. && cell.rect.top >= 0.);
        assert!(cell.rect.left + cell.rect.width <= 4_000_000_000.);
        assert!(cell.rect.top + cell.rect.height <= 4_000_000_000.);
        count += 1;
    }
    assert!(count > 0 && count <= 4096);
    assert_eq!(
        CheckerGrid::new(physical(-50., -50., 10., 10.), Rect::full(5, 5), 1.)
            .unwrap()
            .count(),
        0
    );
    assert!(
        CheckerGrid::new(
            physical(0., 0., 1., 1.),
            Rect::full(1, 1),
            f64::MIN_POSITIVE
        )
        .unwrap()
        .count()
            <= 4096
    );

    for dpi in [0., -1., f64::NAN, f64::INFINITY, f64::MAX] {
        assert!(CheckerGrid::new(image, area, dpi).is_err());
    }
    for invalid in [
        physical(f64::NAN, 0., 1., 1.),
        physical(0., f64::INFINITY, 1., 1.),
        physical(0., 0., 0., 1.),
        physical(0., 0., 1., -1.),
        physical(0., 0., f64::INFINITY, 1.),
        physical(0., 0., 1., f64::NAN),
        physical(f64::MAX, 0., f64::MAX, 1.),
        physical(f64::MAX, 0., 1., 1.),
    ] {
        assert!(CheckerGrid::new(invalid, area, 1.).is_err());
    }
    for invalid in [rect(0, 0, 0, 1), rect(0, 0, 1, 0), rect(u32::MAX, 0, 2, 1)] {
        assert!(CheckerGrid::new(image, invalid, 1.).is_err());
    }
}
