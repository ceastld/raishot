//! 保守墨迹紧框纯模型定义；自造像素块只证明几何合同，不模拟字体或真实推理。
use super::super::RgbImage;
use super::*;
use crate::{
    annotation::Point,
    geometry::Rect,
    pin_text_selection::SelectionText,
    recognition::{
        geometry::Projection, layout::CopyLayoutMode, HorizontalDirection, RecognizedLine,
        RecognizedText, RecognizedWord, TextGeometry,
    },
};

fn geometry(points: [[f32; 2]; 4]) -> TextGeometry {
    TextGeometry {
        quad: points.map(|[x, y]| Point { x, y }),
    }
}

fn horizontal() -> TextGeometry {
    geometry([[32., 32.], [152., 32.], [152., 72.], [32., 72.]])
}

#[test]
fn compact_path_pixels_keep_padding_below_underscores_and_descenders() {
    // 真实反馈截图的派生样例：仅还原已知平色选择底，所有字形像素保持原值。
    // 固定检测框重放像素后处理，不把该测试当成模型识别准确率。
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/ocr-selection/compact-paths.png");
    let image = crate::export::read_image(&path).unwrap();
    let rgb = RgbImage::from_bgra(&image).unwrap();
    for (left, top, right, bottom, ink_top, ink_bottom) in [
        (30.991_335, 50.021_56, 224.773_09, 73.626_88, 54., 72.),
        (29.153_28, 93.821_236, 305.231_32, 120.225_64, 99., 117.),
        (30.090_624, 139.479_58, 372.962_25, 166.004_79, 144., 162.),
    ] {
        let original = geometry([[left, top], [right, top], [right, bottom], [left, bottom]]);
        let p = Projection::new(original).unwrap();
        let roi = Roi::new(p, &rgb).unwrap();
        let after = line_evidence(p, roi, &rgb, &mut || Ok(()))
            .unwrap()
            .unwrap();
        let padding = ((ink_bottom - ink_top) * 0.12_f32).max(2.);
        let new_top = after.geometry.quad[0].y;
        let new_bottom = after.geometry.quad[3].y;
        if (ink_top - top).min(bottom - ink_bottom) >= padding {
            assert!(ink_top - new_top >= padding - 0.001);
            assert!(new_bottom - ink_bottom >= padding - 0.001);
        } else {
            assert_eq!(after.geometry, original, "下划线已接近原边界时保留原行高");
        }
    }
}

fn rotated() -> TextGeometry {
    geometry([[152., 72.], [32., 72.], [32., 32.], [152., 32.]])
}

fn vertical(down: bool) -> TextGeometry {
    if down {
        geometry([[72., 32.], [72., 152.], [32., 152.], [32., 32.]])
    } else {
        geometry([[32., 152.], [32., 32.], [72., 32.], [72., 152.]])
    }
}

fn slanted() -> TextGeometry {
    geometry([[32., 32.], [152., 68.], [140., 108.], [20., 72.]])
}

fn perspective() -> TextGeometry {
    geometry([[32., 32.], [164., 44.], [144., 112.], [20., 80.]])
}

fn rect_of(g: TextGeometry) -> Rect {
    let min_x = g
        .quad
        .iter()
        .map(|p| p.x)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let min_y = g
        .quad
        .iter()
        .map(|p| p.y)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let max_x = g
        .quad
        .iter()
        .map(|p| p.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    let max_y = g
        .quad
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

fn direction(g: TextGeometry) -> HorizontalDirection {
    let p = Projection::new(g).expect("模型投影有效");
    if p.point(1., 0.5).x < p.point(0., 0.5).x {
        HorizontalDirection::RightToLeft
    } else {
        HorizontalDirection::LeftToRight
    }
}

fn one(text: &str, g: TextGeometry) -> RecognizedText {
    let rect = rect_of(g);
    let horizontal_direction = direction(g);
    RecognizedText {
        text: text.into(),
        regions: vec![rect],
        words: vec![RecognizedWord {
            text: text.into(),
            rect,
            line: 0,
            horizontal_direction,
            geometry: Some(g),
        }],
        lines: vec![RecognizedLine {
            graphemes: None,
            text: text.into(),
            rect: Some(rect),
            horizontal_direction,
            geometry: Some(g),
        }],
    }
}

fn image(width: usize, height: usize, gray: f32) -> RgbImage {
    RgbImage {
        width,
        height,
        pixels: vec![[gray; 3]; width * height],
    }
}

/// 遍历完整原图，用原投影逆变换像素中心；没有裁行/缩放副本或推理张量。
fn paint(rgb: &mut RgbImage, g: TextGeometry, u: [f64; 2], v: [f64; 2], color: [f32; 3]) {
    let p = Projection::new(g).expect("模型投影有效");
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if p.inverse(Point {
                x: x as f32 + 0.5,
                y: y as f32 + 0.5,
            })
            .is_some_and(|[a, b]| a >= u[0] && a <= u[1] && b >= v[0] && b <= v[1])
            {
                rgb.pixels[y * rgb.width + x] = color;
            }
        }
    }
}

fn dark_image(g: TextGeometry) -> RgbImage {
    let mut rgb = image(224, 224, 255.);
    paint(&mut rgb, g, [0.08, 0.92], [0.3, 0.7], [0.; 3]);
    rgb
}

fn run(result: RecognizedText, rgb: &RgbImage) -> RecognizedText {
    tighten(result, rgb, || Ok(())).expect("纯模型无取消")
}

type Bits = [(u32, u32); 4];
type WordMetadata = (String, Rect, usize, HorizontalDirection);
type LineMetadata = (String, Option<Rect>, HorizontalDirection, Option<Bits>);

fn bits(g: Option<TextGeometry>) -> Option<Bits> {
    g.map(|g| g.quad.map(|p| (p.x.to_bits(), p.y.to_bits())))
}

fn metadata(r: &RecognizedText) -> (String, Vec<Rect>, Vec<WordMetadata>, Vec<LineMetadata>) {
    (
        r.text.clone(),
        r.regions.clone(),
        r.words
            .iter()
            .map(|w| (w.text.clone(), w.rect, w.line, w.horizontal_direction))
            .collect(),
        r.lines
            .iter()
            .map(|l| {
                (
                    l.text.clone(),
                    l.rect,
                    l.horizontal_direction,
                    bits(l.geometry),
                )
            })
            .collect(),
    )
}

fn unchanged(result: RecognizedText, rgb: &RgbImage) {
    let original = metadata(&result);
    let geometry: Vec<_> = result.words.iter().map(|w| bits(w.geometry)).collect();
    let actual = run(result, rgb);
    assert_eq!(metadata(&actual), original);
    assert_eq!(
        actual
            .words
            .iter()
            .map(|w| bits(w.geometry))
            .collect::<Vec<_>>(),
        geometry
    );
}

fn assert_shrunk(original: TextGeometry, word: &RecognizedWord) {
    let p = Projection::new(original).unwrap();
    let q = word
        .geometry
        .and_then(|g| g.validated_in(word.rect))
        .expect("紧框仍在原CTC外包络内");
    assert_ne!(bits(Some(original)), bits(word.geometry));
    assert_eq!(p.direction(), q.direction());
    let corners = q.quad().map(|point| p.inverse(point).unwrap());
    assert!(corners[0][1] > 0.01 && corners[2][1] < 0.99);
    for (index, [u, _]) in corners.into_iter().enumerate() {
        let expected = if index == 0 || index == 3 { 0. } else { 1. };
        assert!((u - expected).abs() < 0.00001, "u/CTC宽度不可收紧");
    }
    assert!((corners[0][1] + corners[3][1] - 1.).abs() < 0.00001);
    assert!((corners[1][1] + corners[2][1] - 1.).abs() < 0.00001);
    for u in [0., 0.25, 0.5, 0.75, 1.] {
        let a = p.point(u, 0.5);
        let b = q.point(u, 0.5);
        assert!((a.x - b.x).hypot(a.y - b.y) <= 0.001, "阅读中线停点保持");
    }
}

fn assert_all_ink_covered(rgb: &RgbImage, background: [f32; 3], g: TextGeometry) {
    let p = Projection::new(g).unwrap();
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if rgb.pixels[y * rgb.width + x] == background {
                continue;
            }
            for (dx, dy) in [(0., 0.), (1., 0.), (1., 1.), (0., 1.)] {
                let [u, v] = p
                    .inverse(Point {
                        x: x as f32 + dx,
                        y: y as f32 + dy,
                    })
                    .unwrap();
                assert!(
                    (-0.00001..=1.00001).contains(&u) && (-0.00001..=1.00001).contains(&v),
                    "实际像素单元不能被裁掉：{x},{y}/{u},{v}"
                );
            }
        }
    }
}

#[test]
fn strong_black_on_white_tightens_only_normal_height() {
    let g = horizontal();
    let r = one("中文AB12", g);
    let before = metadata(&r);
    let rgb = dark_image(g);
    let after = run(r, &rgb);
    assert_eq!(metadata(&after), before);
    assert_shrunk(g, &after.words[0]);
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn line_height_includes_punctuation_and_faint_antialiasing_without_changing_ctc() {
    use crate::recognition::graphemes::{GraphemeSampling, SamplingRun};
    for g in [
        horizontal(),
        rotated(),
        vertical(true),
        vertical(false),
        slanted(),
        perspective(),
    ] {
        let mut rgb = dark_image(g);
        paint(&mut rgb, g, [0.35, 0.65], [0.19, 0.31], [254.; 3]);
        let mut original = one("甲。", g);
        let sampling = GraphemeSampling::from_ctc(
            0,
            &original.lines[0],
            &[&original.words[0]],
            &[
                SamplingRun {
                    source_range: 0..3,
                    steps: 1..2,
                },
                SamplingRun {
                    source_range: 3..6,
                    steps: 3..4,
                },
            ],
            6,
            120,
            120,
        )
        .unwrap();
        original.lines[0].graphemes = Some(sampling.clone());
        let before = metadata(&original);
        let after = run(original, &rgb);
        assert_eq!(metadata(&after), before);
        assert_eq!(after.words[0].geometry, Some(g), "旧逐词路线仍保留标点回退");
        let current = after.lines[0].graphemes.as_ref().unwrap();
        let selected = current.selection_geometry();
        if g == perspective() {
            assert_eq!(selected, g, "透视使全行物理留白不足时应保留原高");
        } else {
            assert_ne!(selected, g, "充分的整行像素证据应能保守收紧");
        }
        assert_all_ink_covered(&rgb, [255.; 3], selected);
        assert_eq!(current.source_geometry(), sampling.source_geometry());
        let words = [&after.words[0]];
        assert_eq!(
            current.validated_bands(0, &after.lines[0], &words),
            sampling.validated_bands(0, &after.lines[0], &words)
        );
    }
}

#[test]
fn line_height_keeps_ambiguous_background_and_unseparated_neighbor() {
    let g = horizontal();
    for bad in [true, false] {
        let mut rgb = dark_image(g);
        if bad {
            rgb.pixels[31 * rgb.width + 40] = [250.; 3];
        } else {
            paint(&mut rgb, g, [0.2, 0.8], [0.02, 0.12], [0.; 3]);
        }
        let p = Projection::new(g).unwrap();
        assert!(
            evidence(p, Roi::new(p, &rgb).unwrap(), &rgb, 1, true, &mut || Ok(()))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn strong_white_on_black_uses_same_conservative_geometry_contract() {
    let g = horizontal();
    let mut rgb = image(224, 224, 0.);
    paint(&mut rgb, g, [0.08, 0.92], [0.3, 0.7], [255.; 3]);
    let after = run(one("Rust2026", g), &rgb);
    assert_shrunk(g, &after.words[0]);
    assert_all_ink_covered(&rgb, [0.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn supported_chinese_english_digits_and_extension_scalar_remain_verbatim() {
    for text in ["中文", "RustShot", "012345", "甲A2乙", "𠀀"] {
        let r = one(text, horizontal());
        let before = metadata(&r);
        let after = run(r, &dark_image(horizontal()));
        assert_eq!(metadata(&after), before);
        assert_shrunk(horizontal(), &after.words[0]);
    }
}

#[test]
fn rotated_180_projection_keeps_original_reading_axis_and_pixel_coverage() {
    let g = rotated();
    let rgb = dark_image(g);
    let after = run(one("甲乙12", g), &rgb);
    assert_shrunk(g, &after.words[0]);
    assert_eq!(
        after.words[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn vertical_both_directions_keep_projected_axis_and_pixel_coverage() {
    for g in [vertical(true), vertical(false)] {
        let rgb = dark_image(g);
        let after = run(one("甲乙", g), &rgb);
        assert_shrunk(g, &after.words[0]);
        assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
    }
}

#[test]
fn slanted_original_pixels_shrink_in_local_normal_and_keep_coverage() {
    let g = slanted();
    let rgb = dark_image(g);
    let after = run(one("AB中文", g), &rgb);
    assert_shrunk(g, &after.words[0]);
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn full_perspective_mapping_is_kept_instead_of_axis_aligned_replacement() {
    let g = perspective();
    let rgb = dark_image(g);
    let after = run(one("甲AB2", g), &rgb);
    assert_shrunk(g, &after.words[0]);
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
    let q = after.words[0].geometry.unwrap().quad;
    assert!((q[1].y - q[0].y).abs() > 1. && (q[3].x - q[0].x).abs() > 1.);
}

#[test]
fn low_contrast_and_blank_pixels_keep_original_geometry() {
    for (bg, ink) in [(255., 230.), (0., 25.), (128., 128.)] {
        let mut rgb = image(224, 224, bg);
        paint(&mut rgb, horizontal(), [0.12, 0.88], [0.3, 0.7], [ink; 3]);
        unchanged(one("甲乙", horizontal()), &rgb);
    }
}

#[test]
fn variable_background_and_inverse_background_boundary_keep_original() {
    for inverse in [false, true] {
        let mut rgb = dark_image(horizontal());
        for y in 30..74 {
            let x = 31;
            rgb.pixels[y * rgb.width + x] = if inverse {
                [0.; 3]
            } else {
                [if y % 2 == 0 { 255. } else { 250. }; 3]
            };
        }
        unchanged(one("AB", horizontal()), &rgb);
    }
}

#[test]
fn opposite_ink_polarities_on_same_background_are_ambiguous() {
    let g = horizontal();
    let mut rgb = image(224, 224, 128.);
    paint(&mut rgb, g, [0.12, 0.45], [0.3, 0.7], [0.; 3]);
    paint(&mut rgb, g, [0.55, 0.88], [0.3, 0.7], [255.; 3]);
    unchanged(one("甲乙", g), &rgb);
}

#[test]
fn separated_normal_bands_are_not_combined_into_false_text_height() {
    let g = horizontal();
    let mut rgb = image(224, 224, 255.);
    paint(&mut rgb, g, [0.12, 0.88], [0.2, 0.35], [0.; 3]);
    paint(&mut rgb, g, [0.12, 0.88], [0.65, 0.8], [0.; 3]);
    unchanged(one("甲乙", g), &rgb);
}

#[test]
fn reading_or_normal_boundary_contact_keeps_original_geometry() {
    for (u, v) in [
        ([0., 0.5], [0.3, 0.7]),
        ([0.5, 1.], [0.3, 0.7]),
        ([0.12, 0.88], [0., 0.4]),
        ([0.12, 0.88], [0.6, 1.]),
    ] {
        let mut rgb = image(224, 224, 255.);
        paint(&mut rgb, horizontal(), u, v, [0.; 3]);
        unchanged(one("AB", horizontal()), &rgb);
    }
}

#[test]
fn neighboring_ink_in_halo_does_not_justify_tightening_this_word() {
    let mut rgb = dark_image(horizontal());
    for y in 44..61 {
        rgb.pixels[y * rgb.width + 31] = [0.; 3];
    }
    unchanged(one("甲", horizontal()), &rgb);
}

#[test]
fn slanted_aabb_ink_outside_real_projection_is_not_used_as_word_evidence() {
    let g = slanted();
    let mut rgb = dark_image(g);
    // 位于原AABB内、真实四边形外，不能把外包络当可采样词带。
    rgb.pixels[35 * rgb.width + 140] = [0.; 3];
    unchanged(one("甲乙", g), &rgb);
}

#[test]
fn strong_body_does_not_crop_weak_pixels_outside_body() {
    for value in [253., 250., 225.] {
        let g = horizontal();
        let mut rgb = dark_image(g);
        paint(&mut rgb, g, [0.35, 0.65], [0.10, 0.14], [value; 3]);
        let after = run(one("AB", g), &rgb);
        // 保留原框或扩大保护带均可；低于主体阈值的真实像素单元仍须完整覆盖。
        assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
    }
}

#[test]
fn chromatic_weak_deviation_is_protected_even_without_large_luminance_delta() {
    let g = horizontal();
    let mut rgb = dark_image(g);
    paint(&mut rgb, g, [0.35, 0.65], [0.10, 0.14], [253., 255., 255.]);
    let after = run(one("AB", g), &rgb);
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn punctuation_combining_graphemes_emoji_whitespace_and_empty_word_fall_back() {
    for text in [
        ".",
        "，",
        "甲。",
        "A-B",
        "A B",
        " ",
        "",
        "e\u{301}",
        "a\u{308}",
        "👨‍👩‍👧",
        "🙂",
        "\n",
        "\t",
    ] {
        unchanged(one(text, horizontal()), &dark_image(horizontal()));
    }
}

#[test]
fn missing_nonfinite_degenerate_and_self_crossed_geometry_fall_back() {
    let g = horizontal();
    let mut invalids = vec![None];
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut bad = g;
        bad.quad[0].x = value;
        invalids.push(Some(bad));
    }
    let mut repeated = g;
    repeated.quad[1] = repeated.quad[0];
    invalids.push(Some(repeated));
    let mut crossed = g;
    crossed.quad.swap(1, 2);
    invalids.push(Some(crossed));
    for bad in invalids {
        let mut r = one("AB", g);
        r.words[0].geometry = bad;
        unchanged(r, &dark_image(g));
    }
}

#[test]
fn projection_outside_word_rect_and_word_rect_outside_image_fall_back() {
    let g = horizontal();
    let mut r = one("AB", g);
    r.words[0].rect.width -= 1;
    r.regions[0] = r.words[0].rect;
    r.lines[0].rect = Some(r.words[0].rect);
    unchanged(r, &dark_image(g));
    let mut r = one("AB", g);
    r.words[0].rect = Rect {
        x: 220,
        y: 32,
        width: 120,
        height: 40,
    };
    unchanged(r, &dark_image(g));
}

#[test]
fn same_direction_word_outside_actual_line_projection_keeps_original_geometry() {
    let line = horizontal();
    let word = geometry([[32., 90.], [152., 90.], [152., 130.], [32., 130.]]);
    let mut r = one("AB", word);
    // 共同外包络有效且union合同完备，不能替代所属line真实投影的证据。
    let envelope = Rect {
        x: 32,
        y: 32,
        width: 120,
        height: 98,
    };
    r.words[0].rect = envelope;
    r.regions[0] = envelope;
    r.lines[0].rect = Some(envelope);
    r.lines[0].geometry = Some(line);
    unchanged(r, &dark_image(word));
}

#[test]
fn zero_and_overflowing_rect_are_kept_without_arithmetic_wrap() {
    for rect in [
        Rect {
            x: 32,
            y: 32,
            width: 0,
            height: 40,
        },
        Rect {
            x: u32::MAX,
            y: 32,
            width: 120,
            height: 40,
        },
        Rect {
            x: 32,
            y: u32::MAX,
            width: 120,
            height: 40,
        },
    ] {
        let mut r = one("AB", horizontal());
        r.words[0].rect = rect;
        r.regions[0] = rect;
        unchanged(r, &dark_image(horizontal()));
    }
}

#[test]
fn truncated_extra_zero_and_overflowing_pixel_dimensions_fall_back() {
    let mut short = dark_image(horizontal());
    short.pixels.pop();
    unchanged(one("AB", horizontal()), &short);
    let mut extra = dark_image(horizontal());
    extra.pixels.push([255.; 3]);
    unchanged(one("AB", horizontal()), &extra);
    for (width, height) in [(0, 224), (224, 0), (usize::MAX, 2)] {
        unchanged(
            one("AB", horizontal()),
            &RgbImage {
                width,
                height,
                pixels: Vec::new(),
            },
        );
    }
}

#[test]
fn nonfinite_and_out_of_range_pixels_anywhere_in_complete_image_fall_back() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1., 256.] {
        let mut rgb = dark_image(horizontal());
        let last = rgb.pixels.len() - 1;
        rgb.pixels[last][1] = value;
        unchanged(one("AB", horizontal()), &rgb);
    }
}

#[test]
fn roi_above_scan_budget_keeps_original_word() {
    let g = geometry([[32., 32.], [332., 32.], [332., 332.], [32., 332.]]);
    let mut rgb = image(384, 384, 255.);
    paint(&mut rgb, g, [0.12, 0.88], [0.3, 0.7], [0.; 3]);
    unchanged(one("AB", g), &rgb);
}

fn repeated_words(count: usize, g: TextGeometry) -> RecognizedText {
    let rect = rect_of(g);
    let text = "AB".repeat(count);
    RecognizedText {
        text: text.clone(),
        regions: vec![rect; count],
        words: (0..count)
            .map(|_| RecognizedWord {
                text: "AB".into(),
                rect,
                line: 0,
                horizontal_direction: direction(g),
                geometry: Some(g),
            })
            .collect(),
        lines: vec![RecognizedLine {
            graphemes: None,
            text,
            rect: Some(rect),
            horizontal_direction: direction(g),
            geometry: Some(g),
        }],
    }
}

#[test]
fn excessive_word_count_keeps_all_original_objects() {
    unchanged(
        repeated_words(4097, horizontal()),
        &dark_image(horizontal()),
    );
}

#[test]
fn cumulative_pixel_budget_does_not_tighten_unscanned_tail_or_edit_metadata() {
    let g = geometry([[12., 12.], [212., 12.], [212., 212.], [12., 212.]]);
    let mut rgb = image(224, 224, 255.);
    paint(&mut rgb, g, [0.12, 0.88], [0.3, 0.7], [0.; 3]);
    let r = repeated_words(60, g);
    let before = metadata(&r);
    let after = run(r, &rgb);
    assert_eq!(metadata(&after), before);
    assert!(after
        .words
        .iter()
        .all(|word| bits(word.geometry) == bits(Some(g))));
}

#[test]
fn adjacent_letter_blocks_with_real_gap_keep_ctc_width_and_are_not_deleted() {
    let g = horizontal();
    let mut rgb = image(224, 224, 255.);
    paint(&mut rgb, g, [0.12, 0.40], [0.3, 0.7], [0.; 3]);
    paint(&mut rgb, g, [0.60, 0.88], [0.3, 0.7], [0.; 3]);
    let r = one("AB", g);
    let before = metadata(&r);
    let after = run(r, &rgb);
    assert_eq!(metadata(&after), before);
    assert_shrunk(g, &after.words[0]);
    assert_all_ink_covered(&rgb, [255.; 3], after.words[0].geometry.unwrap());
}

#[test]
fn closed_border_is_not_assumed_to_be_letter_ink() {
    let g = horizontal();
    let mut rgb = image(224, 224, 255.);
    for (u, v) in [
        ([0.2, 0.8], [0.25, 0.275]),
        ([0.2, 0.8], [0.725, 0.75]),
        ([0.2, 0.21], [0.25, 0.75]),
        ([0.79, 0.8], [0.25, 0.75]),
    ] {
        paint(&mut rgb, g, u, v, [0.; 3]);
    }
    unchanged(one("AB", g), &rgb);
}

#[test]
fn complete_halo_unavailable_at_image_edge_keeps_original_geometry() {
    for g in [
        geometry([[1., 32.], [121., 32.], [121., 72.], [1., 72.]]),
        geometry([[32., 0.], [152., 0.], [152., 40.], [32., 40.]]),
        geometry([[103., 32.], [223., 32.], [223., 72.], [103., 72.]]),
    ] {
        unchanged(one("AB", g), &dark_image(g));
    }
}

#[test]
fn incomplete_shared_line_word_and_region_contracts_keep_whole_result() {
    for defect in 0..7 {
        let mut r = one("AB", horizontal());
        match defect {
            0 => r.regions.clear(),
            1 => r.regions[0].width -= 1,
            2 => r.words[0].line = 1,
            3 => r.text = "不同全文".into(),
            4 => r.lines[0].rect = None,
            5 => r.lines[0].rect.as_mut().unwrap().width += 1,
            6 => r.words[0].horizontal_direction = HorizontalDirection::RightToLeft,
            _ => unreachable!(),
        }
        unchanged(r, &dark_image(horizontal()));
    }
}

#[test]
fn missing_or_invalid_line_projection_does_not_replace_it_with_word_evidence() {
    for line_geometry in [
        None,
        Some(geometry([[32., 32.], [32., 32.], [152., 72.], [32., 72.]])),
    ] {
        let mut r = one("AB", horizontal());
        r.lines[0].geometry = line_geometry;
        unchanged(r, &dark_image(horizontal()));
    }
}

#[test]
fn overly_long_word_is_kept_even_when_otherwise_eligible() {
    unchanged(
        one(&"A".repeat(257), horizontal()),
        &dark_image(horizontal()),
    );
}

#[test]
fn sub_threshold_distant_pixels_and_fractional_deviation_keep_original_frame() {
    for value in [254., 254.5] {
        let g = horizontal();
        let mut rgb = dark_image(g);
        paint(&mut rgb, g, [0.35, 0.65], [0.10, 0.14], [value; 3]);
        unchanged(one("AB", g), &rgb);
    }
    let mut rgb = image(224, 224, 255.);
    paint(&mut rgb, horizontal(), [0.12, 0.88], [0.3, 0.7], [254.; 3]);
    unchanged(one("AB", horizontal()), &rgb);
}

#[test]
fn strong_neighbor_cannot_supply_evidence_for_faint_or_blank_grapheme_cell() {
    let g = horizontal();
    for faint in [255., 253., 230.] {
        let mut rgb = image(224, 224, 255.);
        paint(&mut rgb, g, [0.08, 0.45], [0.3, 0.7], [0.; 3]);
        paint(&mut rgb, g, [0.55, 0.92], [0.3, 0.7], [faint; 3]);
        unchanged(one("AB", g), &rgb);
    }
}

#[test]
fn wrong_word_text_non_whitespace_holes_and_incomplete_graphemes_keep_all_geometry() {
    let g = horizontal();
    for defect in 0..4 {
        let mut r = one("AB", g);
        match defect {
            0 => r.words[0].text = "AC".into(),
            1 => {
                r.text = "XAB".into();
                r.lines[0].text = r.text.clone();
            }
            2 => {
                r.text = "ABX".into();
                r.lines[0].text = r.text.clone();
            }
            3 => {
                r.text = "e\u{301}".into();
                r.lines[0].text = r.text.clone();
                r.words[0].text = "e".into();
            }
            _ => unreachable!(),
        }
        unchanged(r, &dark_image(g));
    }
}

#[test]
fn cancellation_at_each_success_path_checkpoint_propagates_exact_error() {
    let g = horizontal();
    let rgb = dark_image(g);
    let mut checkpoints = 0usize;
    tighten(one("AB", g), &rgb, || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert!(checkpoints > 1, "完整像素扫描必须有取消边界");
    for fail_at in 0..checkpoints {
        let mut visited = 0usize;
        let expected = format!("墨迹纯模型取消 {fail_at}");
        let result = tighten(one("AB", g), &rgb, || {
            let index = visited;
            visited += 1;
            if index == fail_at {
                Err(expected.clone().into())
            } else {
                Ok(())
            }
        });
        match result {
            Err(error) => assert_eq!(error.to_string(), expected),
            Ok(_) => panic!("检查点 {fail_at} 的取消不得变成成功"),
        }
        assert_eq!(visited, fail_at + 1);
    }
}

#[test]
fn cancellation_is_checked_before_invalid_input_fallback() {
    let result = tighten(
        one("AB", horizontal()),
        &RgbImage {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        },
        || Err("首边界取消".into()),
    );
    match result {
        Err(error) => assert_eq!(error.to_string(), "首边界取消"),
        Ok(_) => panic!("非法数据兜底不能吞取消"),
    }
}

#[test]
fn empty_recognized_result_returns_without_pixel_scan_but_checks_cancellation() {
    let empty = || RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    // 空结果没有待收紧几何，因此不需要完整像素证据；入口取消仍先执行。
    let malformed = RgbImage {
        width: usize::MAX,
        height: 2,
        pixels: Vec::new(),
    };
    let mut visited = 0usize;
    let result = tighten(empty(), &malformed, || {
        visited += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(visited, 1);
    assert_eq!(metadata(&result), metadata(&empty()));
    let cancelled = tighten(empty(), &malformed, || Err("空结果入口取消".into()));
    match cancelled {
        Err(error) => assert_eq!(error.to_string(), "空结果入口取消"),
        Ok(_) => panic!("空结果不得吞掉入口取消"),
    }
}

fn spaced_rows() -> RecognizedText {
    let g1 = geometry([[24., 24.], [160., 24.], [160., 64.], [24., 64.]]);
    let g2 = geometry([[24., 88.], [160., 88.], [160., 128.], [24., 128.]]);
    let mut r = RecognizedText {
        text: "中文 A12\n下一行 B34".into(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (index, (text, g, fragments)) in [
        ("中文 A12", g1, [("中文", 0., 0.45), ("A12", 0.55, 1.)]),
        ("下一行 B34", g2, [("下一行", 0., 0.45), ("B34", 0.55, 1.)]),
    ]
    .into_iter()
    .enumerate()
    {
        let p = Projection::new(g).unwrap();
        for (text, left, right) in fragments {
            let band = TextGeometry {
                quad: [
                    p.point(left, 0.),
                    p.point(right, 0.),
                    p.point(right, 1.),
                    p.point(left, 1.),
                ],
            };
            let rect = rect_of(band);
            r.regions.push(rect);
            r.words.push(RecognizedWord {
                text: text.into(),
                rect,
                line: index,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(band),
            });
        }
        r.lines.push(RecognizedLine {
            graphemes: None,
            text: text.into(),
            rect: Some(rect_of(g)),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(g),
        });
    }
    r
}

#[test]
fn line_word_order_spaces_regions_and_original_line_geometry_are_unchanged() {
    let r = spaced_rows();
    let before = metadata(&r);
    let mut rgb = image(224, 224, 255.);
    for w in &r.words {
        paint(
            &mut rgb,
            w.geometry.unwrap(),
            [0.12, 0.88],
            [0.3, 0.7],
            [0.; 3],
        );
    }
    let originals: Vec<_> = r.words.iter().map(|w| w.geometry.unwrap()).collect();
    let after = run(r, &rgb);
    assert_eq!(metadata(&after), before);
    assert_eq!(
        after.words.iter().map(|w| w.line).collect::<Vec<_>>(),
        [0, 0, 1, 1]
    );
    for (g, w) in originals.into_iter().zip(&after.words) {
        assert_shrunk(g, w);
    }
}

#[test]
fn original_and_auto_copy_mapped_ranges_are_preserved_for_spaced_multiple_rows() {
    let r = spaced_rows();
    let mut rgb = image(224, 224, 255.);
    for w in &r.words {
        paint(
            &mut rgb,
            w.geometry.unwrap(),
            [0.12, 0.88],
            [0.3, 0.7],
            [0.; 3],
        );
    }
    let before = r.copy_layout();
    let length = r.text.len();
    let after = run(r, &rgb).copy_layout();
    for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
        for range in [0..length, 0..6, 7..length] {
            assert_eq!(
                before.copy_mapped(range.clone(), mode).unwrap(),
                after.copy_mapped(range, mode).unwrap()
            );
        }
    }
}

#[test]
fn real_cjk_auto_continuation_and_source_mapping_survive_normal_tightening() {
    // 自动续行要求正文宽度至少为行高的八倍；短行应保留原有换行。
    let mut first = one(
        "这是第一行需要自动续接的正文",
        geometry([[24., 24.], [384., 24.], [384., 64.], [24., 64.]]),
    );
    let mut second = one(
        "这是第二行需要自动续接的正文",
        geometry([[24., 76.], [384., 76.], [384., 116.], [24., 116.]]),
    );
    second.words[0].line = 1;
    let first_end = first.text.len();
    first.text.push('\n');
    first.text.push_str(&second.text);
    first.regions.extend(second.regions);
    first.words.extend(second.words);
    first.lines.extend(second.lines);
    let before_metadata = metadata(&first);
    let before = first.copy_layout();
    let length = first.text.len();
    let auto_before = before.copy_mapped(0..length, CopyLayoutMode::Auto).unwrap();
    assert_eq!(
        auto_before.text,
        "这是第一行需要自动续接的正文这是第二行需要自动续接的正文"
    );
    assert_ne!(
        auto_before.text, first.text,
        "本例确实走自动续行而非原文兜底"
    );
    let mut rgb = image(448, 224, 255.);
    let originals: Vec<_> = first
        .words
        .iter()
        .map(|word| word.geometry.unwrap())
        .collect();
    for word in &first.words {
        paint(
            &mut rgb,
            word.geometry.unwrap(),
            [0.02, 0.98],
            [0.3, 0.7],
            [0.; 3],
        );
    }
    let after_result = run(first, &rgb);
    assert_eq!(metadata(&after_result), before_metadata);
    for (original, word) in originals.into_iter().zip(&after_result.words) {
        assert_shrunk(original, word);
    }
    let after = after_result.copy_layout();
    assert_eq!(
        after.copy_mapped(0..length, CopyLayoutMode::Auto).unwrap(),
        auto_before
    );
    for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
        let range = 3..first_end + 1 + 6;
        assert_eq!(
            before.copy_mapped(range.clone(), mode).unwrap(),
            after.copy_mapped(range, mode).unwrap()
        );
    }
}

#[test]
fn unchanged_midline_ctc_grapheme_byte_stops_feed_selection_and_line_height_markers() {
    for g in [
        horizontal(),
        rotated(),
        vertical(true),
        vertical(false),
        slanted(),
        perspective(),
    ] {
        let r = one("甲乙AB", g);
        let before = SelectionText::from_recognized(&r);
        let after_r = run(r, &dark_image(g));
        assert_shrunk(g, &after_r.words[0]);
        let after = SelectionText::from_recognized(&after_r);
        assert_eq!(before.text, after.text);
        let p = Projection::new(g).unwrap();
        for (u, byte) in [
            (0.001, 0usize),
            (0.251, 3),
            (0.501, 6),
            (0.751, 7),
            (0.999, 8),
        ] {
            let point = p.point(u, 0.5);
            assert_eq!(before.begin(point, 1).unwrap().1, byte..byte);
            assert_eq!(after.begin(point, 1).unwrap().1, byte..byte);
        }
        for range in [0..3, 3..6, 6..7, 7..8] {
            assert_eq!(
                before.selected(range.clone()),
                after.selected(range.clone())
            );
            let old = before.marking_quads(range.clone());
            let new = after.marking_quads(range);
            assert_eq!(old.len(), 1);
            assert_eq!(new.len(), 1);
            let old_p = Projection::new(TextGeometry { quad: old[0] }).unwrap();
            let new_p = Projection::new(TextGeometry { quad: new[0] }).unwrap();
            let old_height = old_p.point(0.5, 0.).distance(old_p.point(0.5, 1.));
            let new_height = new_p.point(0.5, 0.).distance(new_p.point(0.5, 1.));
            // 墨迹只收紧词几何；选字标记沿完整裁行，同行不出现高低不一的碎块。
            assert!((new_height - old_height).abs() <= 0.01);
        }
    }
}

#[test]
fn mixed_eligible_and_punctuation_words_change_only_eligible_geometry() {
    let mut r = spaced_rows();
    r.words[1].text = "A12.".into();
    r.lines[0].text = "中文 A12.".into();
    r.text = "中文 A12.\n下一行 B34".into();
    let before = metadata(&r);
    let punctuation = bits(r.words[1].geometry);
    let original = r.words[0].geometry.unwrap();
    let mut rgb = image(224, 224, 255.);
    for w in &r.words {
        paint(
            &mut rgb,
            w.geometry.unwrap(),
            [0.12, 0.88],
            [0.3, 0.7],
            [0.; 3],
        );
    }
    let after = run(r, &rgb);
    assert_eq!(metadata(&after), before);
    assert_eq!(bits(after.words[1].geometry), punctuation);
    assert_shrunk(original, &after.words[0]);
}
