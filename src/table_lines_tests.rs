use super::*;

fn image(background: u8) -> Image {
    Image {
        width: 161,
        height: 121,
        bgra: vec![background; 161 * 121 * 4],
        readback_ms: 0.,
    }
}
fn pixel(image: &mut Image, x: u32, y: u32, gray: u8) {
    let offset = (y * image.width + x) as usize * 4;
    image.bgra[offset..offset + 4].copy_from_slice(&[gray, gray, gray, 255]);
}
fn line(
    image: &mut Image,
    horizontal: bool,
    coordinate: u32,
    start: u32,
    end: u32,
    gray: u8,
    thickness: u32,
) {
    for p in start..=end {
        for c in coordinate - thickness / 2..=coordinate + thickness / 2 {
            if horizontal {
                pixel(image, p, c, gray);
            } else {
                pixel(image, c, p, gray);
            }
        }
    }
}
fn wired(background: u8, foreground: u8, thickness: u32) -> Image {
    let mut image = image(background);
    for y in [10, 60, 110] {
        line(&mut image, true, y, 10, 150, foreground, thickness);
    }
    for x in [10, 80, 150] {
        line(&mut image, false, x, 10, 110, foreground, thickness);
    }
    image
}

#[test]
fn table_lines_light_grid_is_two_by_two() {
    let result = detect(&wired(255, 0, 1)).unwrap();
    assert_eq!(result.kind, LineKind::Wired);
    let grid = result.grid.unwrap();
    assert_eq!((grid.rows(), grid.columns(), grid.cells.len()), (2, 2, 4));
    assert_eq!(grid.column_edges, vec![10, 80, 150]);
    assert!(grid.validate());
}

#[test]
fn table_lines_dark_grid_uses_local_contrast() {
    let result = detect(&wired(24, 180, 1)).unwrap();
    assert_eq!(result.kind, LineKind::Wired);
    assert!(result.grid.unwrap().validate());
}

#[test]
fn table_lines_striped_grid_keeps_real_borders() {
    let mut image = image(245);
    for y in 61..110 {
        for x in 11..150 {
            pixel(&mut image, x, y, 200);
        }
    }
    for y in [10, 60, 110] {
        line(&mut image, true, y, 10, 150, 0, 1);
    }
    for x in [10, 80, 150] {
        line(&mut image, false, x, 10, 110, 0, 1);
    }
    let grid = detect(&image).unwrap().grid.unwrap();
    assert_eq!((grid.rows(), grid.columns()), (2, 2));
}

#[test]
fn table_lines_thick_borders_collapse_to_one_axis() {
    let grid = detect(&wired(255, 0, 7)).unwrap().grid.unwrap();
    assert_eq!(grid.column_edges, vec![10, 80, 150]);
    assert_eq!(grid.row_edges, vec![10, 60, 110]);
}

#[test]
fn table_lines_long_header_stroke_does_not_add_a_grid_row() {
    let mut image = wired(255, 0, 1);
    for y in 11..60 {
        pixel(&mut image, 80, y, 255);
    }
    line(&mut image, true, 35, 40, 120, 0, 1);
    let grid = detect(&image).unwrap().grid.unwrap();
    assert_eq!(grid.row_edges, vec![10, 60, 110]);
    assert_eq!(grid.column_edges, vec![10, 80, 150]);
    assert_eq!(grid.cells.len(), 3);
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.row == 0 && cell.colspan == 2));
}

#[test]
fn table_lines_missing_vertical_segment_makes_colspan() {
    let mut image = wired(255, 0, 1);
    for y in 11..60 {
        pixel(&mut image, 80, y, 255);
    }
    let grid = detect(&image).unwrap().grid.unwrap();
    assert_eq!(grid.cells.len(), 3);
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.row == 0 && cell.column == 0 && cell.colspan == 2));
}

#[test]
fn table_lines_missing_horizontal_segment_makes_rowspan() {
    let mut image = wired(255, 0, 1);
    for x in 11..80 {
        pixel(&mut image, x, 60, 255);
    }
    let grid = detect(&image).unwrap().grid.unwrap();
    assert_eq!(grid.cells.len(), 3);
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.row == 0 && cell.column == 0 && cell.rowspan == 2));
}

#[test]
fn table_lines_nonrectangular_merge_is_rejected() {
    let mut image = wired(255, 0, 1);
    for y in 11..60 {
        pixel(&mut image, 80, y, 255);
    }
    for x in 11..80 {
        pixel(&mut image, x, 60, 255);
    }
    assert!(detect(&image).unwrap().grid.is_none());
}

#[test]
fn table_lines_horizontal_only_is_ruled() {
    let mut image = image(255);
    for y in [10, 60, 110] {
        line(&mut image, true, y, 10, 150, 0, 1);
    }
    assert_eq!(detect(&image).unwrap().kind, LineKind::Ruled);
}

#[test]
fn table_lines_low_contrast_and_solid_bands_do_not_make_grid() {
    assert_eq!(detect(&wired(120, 135, 1)).unwrap().kind, LineKind::None);
    let mut image = image(255);
    for y in 30..65 {
        for x in 10..150 {
            pixel(&mut image, x, y, 0);
        }
    }
    assert!(detect(&image).unwrap().grid.is_none());
}

#[test]
fn table_lines_dashed_and_oversized_input_are_rejected() {
    let mut image = image(255);
    for y in [10, 60, 110] {
        for x in 10..150 {
            if x % 10 < 5 {
                pixel(&mut image, x, y, 0);
            }
        }
    }
    assert!(detect(&image).unwrap().grid.is_none());
    image.width = 100_000;
    assert!(detect(&image).is_none());
}

/// 全屏尺寸耗时口径：浅底上叠加一张 20×8 网格与噪声文字块，`--ignored --nocapture` 查看。
#[test]
#[ignore = "计时口径，不作为断言"]
fn table_lines_full_screen_budget() {
    for (width, height) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160)] {
        let mut image = Image {
            width,
            height,
            bgra: vec![245; (width * height * 4) as usize],
            readback_ms: 0.,
        };
        let mut seed = 7u32;
        for _ in 0..(width * height / 40) {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let x = (seed >> 8) % width;
            let y = (seed >> 4) % height;
            pixel(&mut image, x, y, 40);
        }
        for row in 0..=8 {
            line(&mut image, true, 200 + row * 60, 200, 200 + 20 * 70, 60, 1);
        }
        for column in 0..=20 {
            line(
                &mut image,
                false,
                200 + column * 70,
                200,
                200 + 8 * 60,
                60,
                1,
            );
        }
        let started = std::time::Instant::now();
        let result = detect(&image).unwrap();
        println!(
            "{width}x{height}: {:.1} ms, kind={:?}, grid={:?}",
            started.elapsed().as_secs_f64() * 1000.,
            result.kind,
            result.grid.map(|grid| (grid.rows(), grid.columns()))
        );
    }
}

#[test]
fn table_lines_double_borders_merge_into_one_grid_line() {
    // 网页表格单元格间距：每条线旁 3px 再有一条平行线。
    let mut image = image(255);
    for y in [10, 60, 110] {
        line(&mut image, true, y, 10, 153, 0, 1);
        line(&mut image, true, y + 3, 10, 153, 0, 1);
    }
    for x in [10, 80, 150] {
        line(&mut image, false, x, 10, 113, 0, 1);
        line(&mut image, false, x + 3, 10, 113, 0, 1);
    }
    let result = detect(&image).unwrap();
    assert_eq!(result.kind, LineKind::Wired);
    let grid = result.grid.unwrap();
    assert_eq!((grid.rows(), grid.columns()), (2, 2));
}
