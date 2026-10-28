//! 原图墨迹证据到选字首尾的集成回归；不加载模型，不修改原字素分界。
use super::*;
use crate::{
    pin_text_selection::SelectionText,
    recognition::{
        graphemes::{GraphemeSampling, SamplingRun},
        RecognizedLine, RecognizedWord,
    },
};

fn quad(points: [[f32; 2]; 4]) -> TextGeometry {
    TextGeometry {
        quad: points.map(|[x, y]| Point { x, y }),
    }
}

fn geometries() -> [TextGeometry; 5] {
    [
        quad([[30., 30.], [230., 30.], [230., 70.], [30., 70.]]),
        quad([[230., 70.], [30., 70.], [30., 30.], [230., 30.]]),
        quad([[80., 20.], [80., 160.], [40., 160.], [40., 20.]]),
        quad([[40., 160.], [40., 20.], [80., 20.], [80., 160.]]),
        quad([[30., 30.], [230., 40.], [218., 88.], [35., 78.]]),
    ]
}

fn white() -> RgbImage {
    RgbImage {
        width: 280,
        height: 190,
        pixels: vec![[255.; 3]; 280 * 190],
    }
}

fn paint(rgb: &mut RgbImage, geometry: TextGeometry, u: [f64; 2], v: [f64; 2], gray: f32) {
    let projection = Projection::new(geometry).unwrap();
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if projection
                .inverse(Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                })
                .is_some_and(|[s, t]| s >= u[0] && s <= u[1] && t >= v[0] && t <= v[1])
            {
                rgb.pixels[y * rgb.width + x] = [gray; 3];
            }
        }
    }
}

fn source(
    text: &str,
    geometry: TextGeometry,
    steps: &[(usize, usize)],
    padded: bool,
) -> RecognizedText {
    assert_eq!(text.chars().count(), steps.len());
    let projection = Projection::new(geometry).unwrap();
    let horizontal_direction = horizontal(projection);
    let left = geometry
        .quad
        .iter()
        .map(|p| p.x)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let top = geometry
        .quad
        .iter()
        .map(|p| p.y)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let right = geometry
        .quad
        .iter()
        .map(|p| p.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    let bottom = geometry
        .quad
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    let rect = Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    };
    let word = RecognizedWord {
        text: text.into(),
        rect,
        line: 0,
        horizontal_direction,
        geometry: Some(geometry),
    };
    let mut line = RecognizedLine {
        text: text.into(),
        rect: Some(rect),
        horizontal_direction,
        geometry: Some(geometry),
        graphemes: None,
    };
    let runs: Vec<_> = text
        .char_indices()
        .zip(steps)
        .map(|((byte, c), &(start, end))| SamplingRun {
            source_range: byte..byte + c.len_utf8(),
            steps: start..end,
        })
        .collect();
    line.graphemes = Some(
        GraphemeSampling::from_ctc(
            0,
            &line,
            &[&word],
            &runs,
            64,
            200,
            if padded { 256 } else { 200 },
        )
        .unwrap(),
    );
    RecognizedText {
        text: text.into(),
        regions: vec![rect],
        words: vec![word],
        lines: vec![line],
    }
}

fn run(result: RecognizedText, rgb: &RgbImage) -> RecognizedText {
    tighten(result, rgb, || Ok(())).unwrap()
}

fn assert_pixel_cells_covered(rgb: &RgbImage, result: &RecognizedText) {
    let sampling = result.lines[0].graphemes.as_ref().unwrap();
    let display = Projection::new(sampling.selection_geometry()).unwrap();
    let reading = sampling.selection_reading_range();
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if rgb.pixels[y * rgb.width + x] == [255.; 3] {
                continue;
            }
            for (px, py) in [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)] {
                let point = Point {
                    x: px as f32,
                    y: py as f32,
                };
                let [u, v] = display.inverse(point).unwrap();
                assert!(
                    u >= reading[0] - 0.0001 && u <= reading[1] + 0.0001,
                    "首尾裁掉了非背景像素单元：u={u}, range={reading:?}"
                );
                assert!((-0.0001..=1.0001).contains(&v), "高度裁掉了像素单元：v={v}");
            }
        }
    }
}

#[test]
fn horizontal_ink_rectification_and_endpoints_feed_actual_selection_without_internal_x_shift() {
    for reverse in [false, true] {
        let mut geometry = quad([[30., 30.], [230., 38.], [228.4, 78.], [28.4, 70.]]);
        if reverse {
            geometry.quad.rotate_left(2);
        }
        let mut rgb = white();
        for y in 46..62 {
            for x in (50..100).chain(160..210) {
                rgb.pixels[y * rgb.width + x] = [0.; 3];
            }
        }
        // 最淡的上下沿与首尾像素同样必须保留。
        rgb.pixels[44 * rgb.width + 55] = [254.; 3];
        rgb.pixels[64 * rgb.width + 205] = [254.; 3];
        let original = source("AB", geometry, &[(7, 15), (44, 52)], false);
        let before = SelectionText::from_recognized(&original);
        let after = run(original.clone(), &rgb);
        let proof = after.lines[0].graphemes.as_ref().unwrap();
        assert!(proof.selection_rectified(), "应消费原图明确的水平笔画证据");
        assert!(proof.selection_reading_range()[0] > 0. && proof.selection_reading_range()[1] < 1.);
        assert_eq!(after.lines[0].geometry, original.lines[0].geometry);
        assert_pixel_cells_covered(&rgb, &after);
        let selected = SelectionText::from_recognized(&after);
        let a = before.caret_segment(1).unwrap();
        let b = selected.caret_segment(1).unwrap();
        let midpoint_x = |edge: [Point; 2]| (f64::from(edge[0].x) + f64::from(edge[1].x)) / 2.;
        assert!((midpoint_x(a) - midpoint_x(b)).abs() <= 0.001);
        assert_eq!(selected.selected(0..2), Some("AB".into()));
        for range in [0..1, 1..2] {
            let q = selected.marking_quads(range)[0];
            assert_eq!(q[0].y, q[1].y);
            assert_eq!(q[2].y, q[3].y);
        }
    }
}

fn edge_u(
    text: &SelectionText,
    source: Projection,
    range: std::ops::Range<usize>,
    edge: usize,
) -> f64 {
    let q = text.marking_quads(range)[0];
    let a = source.inverse(q[edge]).unwrap()[0];
    let b = source.inverse(q[3 - edge]).unwrap()[0];
    assert!((a - b).abs() < 0.0001);
    (a + b) / 2.
}

#[test]
fn endpoints_keep_faint_punctuation_pixel_cells_and_original_internal_ctc_boundaries() {
    for geometry in geometries() {
        let mut rgb = white();
        paint(&mut rgb, geometry, [0.27, 0.73], [0.32, 0.68], 0.);
        // 首尾只有 1 级差的浅笔画，不能因主体阈值而略掉。
        paint(&mut rgb, geometry, [0.08, 0.10], [0.42, 0.58], 254.);
        paint(&mut rgb, geometry, [0.90, 0.92], [0.42, 0.58], 254.);
        let original = source(".A.", geometry, &[(8, 10), (30, 32), (52, 54)], false);
        let before = SelectionText::from_recognized(&original);
        let after = run(original.clone(), &rgb);
        let sampling = after.lines[0].graphemes.as_ref().unwrap();
        let reading = sampling.selection_reading_range();
        assert!(reading[0] > 0. && reading[1] < 1., "首尾应消费已证实的空白");
        assert_eq!(after.text, original.text);
        assert_eq!(after.lines[0].geometry, original.lines[0].geometry);
        assert_eq!(after.lines[0].rect, original.lines[0].rect);
        assert_eq!(after.regions, original.regions);
        assert_pixel_cells_covered(&rgb, &after);
        let selected = SelectionText::from_recognized(&after);
        let projection = Projection::new(geometry).unwrap();
        for (range, edge) in [(0..1, 1), (1..2, 0), (1..2, 1), (2..3, 0)] {
            assert!(
                (edge_u(&before, projection, range.clone(), edge)
                    - edge_u(&selected, projection, range, edge))
                .abs()
                    < 0.0001
            );
        }
    }
}

#[test]
fn endpoints_can_shrink_when_normal_margin_is_too_small_to_shrink_height() {
    let geometry = geometries()[0];
    let mut rgb = white();
    paint(&mut rgb, geometry, [0.25, 0.75], [0.03, 0.97], 0.);
    let original = source("A", geometry, &[(24, 40)], false);
    let after = run(original, &rgb);
    let sampling = after.lines[0].graphemes.as_ref().unwrap();
    assert_eq!(
        sampling.selection_geometry(),
        geometry,
        "高度余量不足时应原样保留"
    );
    let [start, end] = sampling.selection_reading_range();
    assert!(start > 0.1 && end < 0.9, "只有首尾空白也应能收紧");
    assert_pixel_cells_covered(&rgb, &after);
}

#[test]
fn endpoints_crossing_first_internal_midline_keep_height_and_other_valid_endpoint() {
    let geometry = geometries()[0];
    let mut rgb = white();
    paint(&mut rgb, geometry, [0.25, 0.75], [0.3, 0.7], 0.);
    // 首两字的实际 CTC 发射中心非常靠前：端部证据不能越过其原内部边界。
    let original = source("ABC", geometry, &[(1, 2), (3, 4), (52, 54)], false);
    let before = SelectionText::from_recognized(&original);
    let after = run(original, &rgb);
    let sampling = after.lines[0].graphemes.as_ref().unwrap();
    assert_ne!(
        sampling.selection_geometry(),
        geometry,
        "端部不可用不应丢掉高度证据"
    );
    let reading = sampling.selection_reading_range();
    assert_eq!(reading[0], 0., "首端不能跨过第一处分界");
    assert!(reading[1] < 1., "另一端安全时仍可单独收紧");
    let selected = SelectionText::from_recognized(&after);
    let projection = Projection::new(geometry).unwrap();
    assert!(
        (edge_u(&before, projection, 0..1, 1) - edge_u(&selected, projection, 0..1, 1)).abs()
            < 0.0001
    );
    assert_pixel_cells_covered(&rgb, &after);
}

#[test]
fn endpoints_preserve_large_interior_blank_and_tensor_padding_boundaries() {
    let geometry = geometries()[0];
    for padded in [false, true] {
        let mut rgb = white();
        paint(&mut rgb, geometry, [0.20, 0.28], [0.3, 0.7], 0.);
        paint(&mut rgb, geometry, [0.72, 0.80], [0.3, 0.7], 0.);
        let original = source("AB", geometry, &[(10, 12), (38, 40)], padded);
        let before = SelectionText::from_recognized(&original);
        let after = run(original, &rgb);
        let sampling = after.lines[0].graphemes.as_ref().unwrap();
        assert!(sampling.selection_reading_range()[0] > 0.);
        assert!(sampling.selection_reading_range()[1] < 1.);
        let selected = SelectionText::from_recognized(&after);
        let projection = Projection::new(geometry).unwrap();
        let middle = edge_u(&before, projection, 0..1, 1);
        assert!((edge_u(&selected, projection, 0..1, 1) - middle).abs() < 0.0001);
        assert!((edge_u(&selected, projection, 1..2, 0) - middle).abs() < 0.0001);
        assert!(
            selected.has_text_at(projection.point(middle, 0.5)),
            "字间大空白仍属于连续选字单元"
        );
        assert_pixel_cells_covered(&rgb, &after);
    }
}

#[test]
fn compact_selection_keeps_visible_padding_and_the_original_internal_caret() {
    for (height, ink_height) in [(20, 16), (20, 2), (40, 16), (20, 18)] {
        let geometry = quad([
            [30., 30.],
            [230., 30.],
            [230., 30. + height as f32],
            [30., 30. + height as f32],
        ]);
        let mut rgb = white();
        let top = 30 + (height - ink_height) / 2;
        let bottom = top + ink_height;
        for y in top..bottom {
            for x in 35..225 {
                rgb.pixels[y * rgb.width + x] = [0.; 3];
            }
        }
        let original = source("AB", geometry, &[(10, 12), (38, 40)], false);
        let before = SelectionText::from_recognized(&original);
        let after = run(original, &rgb);
        let sampling = after.lines[0].graphemes.as_ref().unwrap();
        let p = Projection::new(sampling.selection_geometry()).unwrap();
        let range = sampling.selection_reading_range();
        let padding = (ink_height as f32 * 0.12).max(2.);
        let display_top = p.point(0.5, 0.).y;
        let display_bottom = p.point(0.5, 1.).y;
        assert!(display_bottom - display_top >= height as f32 * 0.6 - 0.001);
        if (height - ink_height) as f32 / 2. >= padding {
            assert!(top as f32 - display_top >= padding - 0.001);
            assert!(display_bottom - bottom as f32 >= padding - 0.001);
        } else {
            assert_eq!(
                sampling.selection_geometry(),
                geometry,
                "原框已经紧时不能再压缩"
            );
        }
        assert!(35. - p.point(range[0], 0.5).x >= padding - 0.001);
        assert!(p.point(range[1], 0.5).x - 225. >= padding - 0.001);
        let selected = SelectionText::from_recognized(&after);
        assert_eq!(
            before
                .caret_segment(1)
                .map(|edge| (edge[0].x + edge[1].x) * 0.5),
            selected
                .caret_segment(1)
                .map(|edge| (edge[0].x + edge[1].x) * 0.5),
            "增加留白不能移动已有字间分界"
        );
        assert_pixel_cells_covered(&rgb, &after);
    }
}

#[test]
fn sheared_selection_measures_padding_to_the_edge_instead_of_along_the_skew() {
    let geometry = quad([[30., 30.], [170., 30.], [230., 50.], [90., 50.]]);
    let mut rgb = white();
    paint(&mut rgb, geometry, [0.2, 0.8], [0.15, 0.85], 0.);
    let after = run(source("AB", geometry, &[(10, 12), (38, 40)], false), &rgb);
    let sampling = after.lines[0].graphemes.as_ref().unwrap();
    let display = sampling.selection_geometry();
    assert_ne!(display, geometry, "这份充分的像素证据仍应能保守收紧");
    let top = display.quad[0].y;
    let bottom = display.quad[3].y;
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if rgb.pixels[y * rgb.width + x] != [255.; 3] {
                assert!(y as f32 - top >= 2. - 0.001);
                assert!(bottom - (y + 1) as f32 >= 2. - 0.001);
            }
        }
    }
    assert_pixel_cells_covered(&rgb, &after);
}

#[test]
fn a_tight_valid_ink_band_keeps_qualification_for_the_direction_check() {
    let geometry = quad([[30., 30.], [230., 30.], [230., 50.], [30., 50.]]);
    let mut rgb = white();
    for y in 31..49 {
        for x in 31..229 {
            rgb.pixels[y * rgb.width + x] = [0.; 3];
        }
    }
    let p = Projection::new(geometry).unwrap();
    let evidence = line_evidence(p, Roi::new(p, &rgb).unwrap(), &rgb, &mut || Ok(()))
        .unwrap()
        .expect("不再收紧不能同时丢掉可信像素的方向检查资格");
    assert_eq!(evidence.geometry, geometry);
    assert_eq!(evidence.reading, [0., 1.]);
}
