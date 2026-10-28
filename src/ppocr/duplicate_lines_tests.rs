//! 保守重复行去重的纯数据模型；不启动 OCR、窗口或剪贴板。
use super::*;
use crate::{
    annotation::Point,
    geometry::Rect,
    recognition::{
        geometry::Projection, layout::CopyLayoutMode, HorizontalDirection, RecognizedLine,
        RecognizedText, RecognizedWord, TextGeometry,
    },
};

struct Row {
    line: RecognizedLine,
    words: Vec<RecognizedWord>,
}

fn geometry(points: [[f32; 2]; 4]) -> TextGeometry {
    TextGeometry {
        quad: points.map(|[x, y]| Point { x, y }),
    }
}

fn horizontal(x: f32, y: f32, width: f32, height: f32) -> TextGeometry {
    geometry([
        [x, y],
        [x + width, y],
        [x + width, y + height],
        [x, y + height],
    ])
}

fn rect_of(geometry: TextGeometry) -> Rect {
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
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

fn word_union(words: &[RecognizedWord]) -> Option<Rect> {
    let left = words.iter().map(|word| word.rect.x).min()?;
    let top = words.iter().map(|word| word.rect.y).min()?;
    let right = words
        .iter()
        .map(|word| word.rect.x + word.rect.width)
        .max()?;
    let bottom = words
        .iter()
        .map(|word| word.rect.y + word.rect.height)
        .max()?;
    Some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn row(text: &str, crop: TextGeometry, fragments: &[(&str, f64, f64)]) -> Row {
    let projection = Projection::new(crop).expect("模型裁行必须有效");
    let start = projection.point(0., 0.5);
    let end = projection.point(1., 0.5);
    let direction = if end.x < start.x {
        HorizontalDirection::RightToLeft
    } else {
        HorizontalDirection::LeftToRight
    };
    let words: Vec<_> = fragments
        .iter()
        .map(|&(text, start, end)| {
            let band = TextGeometry {
                quad: [
                    projection.point(start, 0.),
                    projection.point(end, 0.),
                    projection.point(end, 1.),
                    projection.point(start, 1.),
                ],
            };
            RecognizedWord {
                text: text.to_owned(),
                rect: rect_of(band),
                line: 0,
                horizontal_direction: direction,
                geometry: Some(band),
            }
        })
        .collect();
    Row {
        line: RecognizedLine {
            graphemes: None,
            text: text.to_owned(),
            rect: word_union(&words),
            horizontal_direction: direction,
            geometry: Some(crop),
        },
        words,
    }
}

fn plain(text: &str, x: f32, y: f32) -> Row {
    row(text, horizontal(x, y, 160., 20.), &[(text, 0.1, 0.9)])
}

fn assemble(rows: Vec<Row>) -> RecognizedText {
    let mut result = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (index, mut row) in rows.into_iter().enumerate() {
        for word in &mut row.words {
            word.line = index;
            result.regions.push(word.rect);
        }
        result.words.extend(row.words);
        result.lines.push(row.line);
    }
    result.text = result
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    result
}

fn quality(scores: &[(f32, f32)]) -> Vec<LineQuality> {
    scores
        .iter()
        .map(|&(recognition, detection)| LineQuality {
            recognition,
            detection,
        })
        .collect()
}

fn run(result: RecognizedText, scores: &[(f32, f32)]) -> RecognizedText {
    deduplicate(result, &quality(scores), Rect::full(2000, 2000), || Ok(()))
        .expect("纯模型不应失败")
}

type GeometryBits = [(u32, u32); 4];
type WordState = (
    String,
    Rect,
    usize,
    HorizontalDirection,
    Option<GeometryBits>,
);
type LineState = (
    String,
    Option<Rect>,
    HorizontalDirection,
    Option<GeometryBits>,
);

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    regions: Vec<Rect>,
    words: Vec<WordState>,
    lines: Vec<LineState>,
}

fn bits(geometry: Option<TextGeometry>) -> Option<GeometryBits> {
    geometry.map(|geometry| geometry.quad.map(|p| (p.x.to_bits(), p.y.to_bits())))
}

fn snapshot(result: &RecognizedText) -> Snapshot {
    Snapshot {
        text: result.text.clone(),
        regions: result.regions.clone(),
        words: result
            .words
            .iter()
            .map(|word| {
                (
                    word.text.clone(),
                    word.rect,
                    word.line,
                    word.horizontal_direction,
                    bits(word.geometry),
                )
            })
            .collect(),
        lines: result
            .lines
            .iter()
            .map(|line| {
                (
                    line.text.clone(),
                    line.rect,
                    line.horizontal_direction,
                    bits(line.geometry),
                )
            })
            .collect(),
    }
}

fn retained(result: RecognizedText, scores: &[(f32, f32)]) {
    let before = snapshot(&result);
    let after = run(result, scores);
    assert_eq!(snapshot(&after), before);
}

fn pair() -> RecognizedText {
    assemble(vec![
        plain("重复文字", 100., 100.),
        plain("重复文字", 100., 100.),
    ])
}

#[test]
fn exact_horizontal_duplicate_keeps_one_complete_candidate() {
    let result = run(pair(), &[(0.9, 0.8), (0.9, 0.8)]);
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![plain("重复文字", 100., 100.)]))
    );
}

#[test]
fn small_projection_jitter_can_merge_without_combining_candidates() {
    let result = run(
        assemble(vec![
            plain("word", 100., 100.),
            plain("word", 100.25, 100.25),
        ]),
        &[(0.8, 0.99), (0.9, 0.6)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![plain("word", 100.25, 100.25)]))
    );
}

#[test]
fn recognition_score_precedes_detector_score_and_retains_winner_ctc_bands() {
    let crop = horizontal(100., 100., 160., 20.);
    let result = run(
        assemble(vec![
            row("word", crop, &[("word", 0.10, 0.90)]),
            row("word", crop, &[("word", 0.105, 0.895)]),
        ]),
        &[(0.8, 0.99), (0.9, 0.1)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![row(
            "word",
            crop,
            &[("word", 0.105, 0.895)]
        )]))
    );
}

#[test]
fn equal_recognition_uses_detector_score_without_taking_loser_geometry() {
    let result = run(
        assemble(vec![plain("word", 100., 100.), plain("word", 100.25, 100.)]),
        &[(0.9, 0.7), (0.9, 0.8)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![plain("word", 100.25, 100.)]))
    );
}

#[test]
fn equal_quality_keeps_earliest_complete_candidate() {
    let result = run(
        assemble(vec![plain("word", 100., 100.), plain("word", 100.25, 100.)]),
        &[(0.9, 0.8), (0.9, 0.8)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![plain("word", 100., 100.)]))
    );
}

#[test]
fn full_crop_may_extend_beyond_ctc_word_union() {
    let result = pair();
    assert!(result.lines[0]
        .geometry
        .unwrap()
        .validated_in(result.lines[0].rect.unwrap())
        .is_none());
    let result = run(result, &[(0.9, 0.8), (0.9, 0.8)]);
    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.words.len(), 1);
    assert_eq!(result.regions, vec![result.words[0].rect]);
}

#[test]
fn neighboring_rows_with_same_text_are_retained() {
    retained(
        assemble(vec![
            plain("重复文字", 100., 100.),
            plain("重复文字", 100., 121.),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn same_text_at_distant_positions_is_retained() {
    for (x, y) in [(500., 100.), (100., 500.), (500., 500.)] {
        retained(
            assemble(vec![plain("相同按钮", 100., 100.), plain("相同按钮", x, y)]),
            &[(0.9, 0.8), (0.99, 0.99)],
        );
    }
}

#[test]
fn text_comparison_preserves_case_spaces_punctuation_and_unicode_spelling() {
    for (a, b) in [
        ("Word", "word"),
        ("word", "word "),
        ("word", " word"),
        ("a b", "ab"),
        ("a b", "a\tb"),
        ("word.", "word"),
        ("你好，", "你好,"),
        ("é", "e\u{301}"),
        ("Ａ", "A"),
        ("繁體", "繁体"),
    ] {
        retained(
            assemble(vec![plain(a, 100., 100.), plain(b, 100., 100.)]),
            &[(0.9, 0.8), (0.99, 0.99)],
        );
    }
}

#[test]
fn matching_vertical_reading_directions_can_merge() {
    for crop in [
        geometry([[120., 100.], [120., 260.], [100., 260.], [100., 100.]]),
        geometry([[100., 260.], [100., 100.], [120., 100.], [120., 260.]]),
    ] {
        let result = run(
            assemble(vec![
                row("竖文", crop, &[("竖文", 0.1, 0.9)]),
                row("竖文", crop, &[("竖文", 0.1, 0.9)]),
            ]),
            &[(0.9, 0.8), (0.9, 0.8)],
        );
        assert_eq!(result.lines.len(), 1);
        assert_eq!(bits(result.lines[0].geometry), bits(Some(crop)));
    }
}

#[test]
fn matching_180_degree_reading_direction_can_merge() {
    let crop = geometry([[260., 120.], [100., 120.], [100., 100.], [260., 100.]]);
    let result = run(
        assemble(vec![
            row("reverse", crop, &[("reverse", 0.1, 0.9)]),
            row("reverse", crop, &[("reverse", 0.1, 0.9)]),
        ]),
        &[(0.9, 0.8), (0.9, 0.8)],
    );
    assert_eq!(result.lines.len(), 1);
    assert_eq!(
        result.lines[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_eq!(bits(result.lines[0].geometry), bits(Some(crop)));
}

#[test]
fn opposite_180_degree_reading_directions_are_retained() {
    retained(
        assemble(vec![
            plain("word", 100., 100.),
            row(
                "word",
                geometry([[260., 120.], [100., 120.], [100., 100.], [260., 100.]]),
                &[("word", 0.1, 0.9)],
            ),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn opposite_vertical_reading_directions_are_retained() {
    retained(
        assemble(vec![
            row(
                "竖文",
                geometry([[120., 100.], [120., 260.], [100., 260.], [100., 100.]]),
                &[("竖文", 0.1, 0.9)],
            ),
            row(
                "竖文",
                geometry([[100., 260.], [100., 100.], [120., 100.], [120., 260.]]),
                &[("竖文", 0.1, 0.9)],
            ),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn matching_tilted_and_perspective_projections_can_merge() {
    for crop in [
        geometry([[100., 100.], [260., 112.], [260., 132.], [100., 120.]]),
        geometry([[100., 100.], [260., 100.], [250., 120.], [110., 120.]]),
    ] {
        let result = run(
            assemble(vec![
                row("倾斜文字", crop, &[("倾斜文字", 0.1, 0.9)]),
                row("倾斜文字", crop, &[("倾斜文字", 0.1, 0.9)]),
            ]),
            &[(0.9, 0.8), (0.9, 0.8)],
        );
        assert_eq!(result.lines.len(), 1);
        assert_eq!(bits(result.lines[0].geometry), bits(Some(crop)));
    }
}

#[test]
fn opposite_tilts_with_identical_axis_aligned_bounds_are_retained() {
    let a = geometry([[100., 100.], [260., 112.], [260., 132.], [100., 120.]]);
    let b = geometry([[100., 112.], [260., 100.], [260., 120.], [100., 132.]]);
    assert_eq!(rect_of(a), rect_of(b));
    retained(
        assemble(vec![
            row("斜行", a, &[("斜行", 0.1, 0.9)]),
            row("斜行", b, &[("斜行", 0.1, 0.9)]),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn different_reading_angles_are_retained() {
    retained(
        assemble(vec![
            plain("word", 100., 100.),
            row(
                "word",
                geometry([[100., 100.], [260., 115.], [260., 135.], [100., 120.]]),
                &[("word", 0.1, 0.9)],
            ),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn different_crop_heights_are_retained() {
    retained(
        assemble(vec![
            plain("word", 100., 100.),
            row(
                "word",
                horizontal(100., 94., 160., 32.),
                &[("word", 0.1, 0.9)],
            ),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn nested_crop_extents_are_retained() {
    retained(
        assemble(vec![
            plain("word", 100., 100.),
            row(
                "word",
                horizontal(140., 100., 80., 20.),
                &[("word", 0.1, 0.9)],
            ),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn different_fragment_patterns_are_retained() {
    let crop = horizontal(100., 100., 160., 20.);
    retained(
        assemble(vec![
            row("abcd", crop, &[("ab", 0.1, 0.4), ("cd", 0.5, 0.9)]),
            row("abcd", crop, &[("abcd", 0.1, 0.9)]),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn different_word_projections_are_retained_despite_matching_full_crop() {
    let crop = horizontal(100., 100., 160., 20.);
    retained(
        assemble(vec![
            row("word", crop, &[("word", 0.1, 0.5)]),
            row("word", crop, &[("word", 0.3, 0.7)]),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn missing_line_or_word_projection_is_retained() {
    for missing_line in [true, false] {
        let mut result = pair();
        if missing_line {
            result.lines[1].geometry = None;
        } else {
            result.words[1].geometry = None;
        }
        retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
    }
}

#[test]
fn malformed_projection_is_preserved_without_using_aabb_as_evidence() {
    let malformed = [
        geometry([[f32::NAN, 100.], [260., 100.], [260., 120.], [100., 120.]]),
        geometry([
            [f32::INFINITY, 100.],
            [260., 100.],
            [260., 120.],
            [100., 120.],
        ]),
        geometry([[100., 100.], [260., 120.], [260., 100.], [100., 120.]]),
        geometry([[100., 100.], [100., 100.], [260., 120.], [100., 120.]]),
        geometry([[100., 100.], [100., 120.], [260., 120.], [260., 100.]]),
        horizontal(1990., 100., 160., 20.),
    ];
    for bad in malformed {
        for bad_line in [true, false] {
            let mut result = pair();
            if bad_line {
                result.lines[1].geometry = Some(bad);
            } else {
                result.words[1].geometry = Some(bad);
            }
            retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
        }
    }
}

#[test]
fn word_projection_outside_its_ctc_rect_is_retained() {
    let mut result = pair();
    for word in &mut result.words {
        word.geometry = Some(horizontal(90., 100., 160., 20.));
    }
    retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
}

#[test]
fn word_band_not_on_crop_full_height_is_retained() {
    let mut result = pair();
    for word in &mut result.words {
        word.geometry = Some(horizontal(116., 105., 128., 10.));
    }
    retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
}

#[test]
fn reversed_word_start_or_end_order_is_retained() {
    let crop = horizontal(100., 100., 160., 20.);
    for fragments in [
        vec![("ab", 0.5, 0.9), ("cd", 0.1, 0.4)],
        vec![("ab", 0.1, 0.9), ("cd", 0.5, 0.7)],
    ] {
        retained(
            assemble(vec![
                row("abcd", crop, &fragments),
                row("abcd", crop, &fragments),
            ]),
            &[(0.9, 0.8), (0.99, 0.99)],
        );
    }
}

#[test]
fn matching_ctc_floor_ceil_overlap_is_valid_duplicate_evidence() {
    // 对应真实 decode_ctc [1, 0, 2, 0] 可生成的估算带；
    // .25.. .5 的重叠不是墨迹重叠，也不能使正常候选失去去重资格。
    let crop = horizontal(100., 100., 160., 20.);
    let result = run(
        assemble(vec![
            row("甲乙", crop, &[("甲", 0., 0.5), ("乙", 0.25, 1.)]),
            row("甲乙", crop, &[("甲", 0., 0.5), ("乙", 0.25, 1.)]),
        ]),
        &[(0.9, 0.8), (0.99, 0.99)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![row(
            "甲乙",
            crop,
            &[("甲", 0., 0.5), ("乙", 0.25, 1.)]
        )]))
    );
}

#[test]
fn bad_geometry_does_not_discard_an_independent_valid_duplicate_group() {
    let mut result = assemble(vec![
        plain("坏几何", 500., 100.),
        plain("正常", 100., 100.),
        plain("正常", 100., 100.),
    ]);
    result.lines[0].geometry = None;
    let preserved_bad = snapshot(&result).lines.remove(0);
    let result = run(result, &[(0.9, 0.8), (0.9, 0.8), (0.99, 0.99)]);
    assert_eq!(result.text, "坏几何\n正常");
    assert_eq!(snapshot(&result).lines[0], preserved_bad);
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.line)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn invalid_region_count_or_pairing_preserves_whole_result() {
    for count_mismatch in [true, false] {
        let mut result = pair();
        if count_mismatch {
            result.regions.pop();
        } else {
            result.regions[1].x += 1;
        }
        retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
    }
}

#[test]
fn bad_line_index_or_line_word_union_preserves_whole_result() {
    for invalid_index in [true, false] {
        let mut result = pair();
        if invalid_index {
            result.words[1].line = 999;
        } else {
            result.lines[1].rect.as_mut().unwrap().width += 1;
        }
        retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
    }
}

#[test]
fn invalid_source_text_direction_or_rect_preserves_whole_result() {
    for mutation in 0..6 {
        let mut result = pair();
        match mutation {
            0 => result.text.push('!'),
            1 => result.words[1].horizontal_direction = HorizontalDirection::RightToLeft,
            2 => {
                result.words[1].rect.width = 0;
                result.regions[1] = result.words[1].rect;
                result.lines[1].rect = Some(result.words[1].rect);
            }
            3 => result.lines[1].rect = None,
            4 => result.words[1].text.clear(),
            _ => {
                result.lines[1].text = "   ".to_owned();
                result.text = "重复文字\n   ".to_owned();
            }
        }
        retained(result, &[(0.9, 0.8), (0.99, 0.99)]);
    }
}

#[test]
fn missing_extra_or_invalid_quality_is_conservatively_retained() {
    for scores in [
        vec![],
        vec![(0.9, 0.8)],
        vec![(0.9, 0.8), (0.9, 0.8), (0.9, 0.8)],
        vec![(0.9, 0.8), (f32::NAN, 0.99)],
        vec![(0.9, 0.8), (f32::INFINITY, 0.99)],
        vec![(0.9, 0.8), (-0.1, 0.99)],
        vec![(0.9, 0.8), (1.1, 0.99)],
        vec![(0.9, 0.8), (0.99, f32::NAN)],
        vec![(0.9, 0.8), (0.99, f32::INFINITY)],
        vec![(0.9, 0.8), (0.99, -0.1)],
        vec![(0.9, 0.8), (0.99, 1.1)],
    ] {
        retained(pair(), &scores);
    }
}

#[test]
fn invalid_or_overflowing_image_rect_preserves_whole_result() {
    for image in [
        Rect::full(0, 2000),
        Rect {
            x: u32::MAX,
            y: 0,
            width: 20,
            height: 2000,
        },
        Rect {
            x: 0,
            y: u32::MAX,
            width: 2000,
            height: 20,
        },
    ] {
        let result = pair();
        let before = snapshot(&result);
        let after = deduplicate(result, &quality(&[(0.9, 0.8), (0.99, 0.99)]), image, || {
            Ok(())
        })
        .unwrap();
        assert_eq!(snapshot(&after), before);
    }
}

#[test]
fn nonzero_image_origin_uses_actual_image_bounds() {
    let result = deduplicate(
        pair(),
        &quality(&[(0.9, 0.8), (0.99, 0.99)]),
        Rect {
            x: 50,
            y: 50,
            width: 500,
            height: 500,
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(result.lines.len(), 1);
}

#[test]
fn more_than_one_thousand_lines_are_retained_without_pairwise_work() {
    let result = assemble((0..1001).map(|_| plain("word", 100., 100.)).collect());
    let before = snapshot(&result);
    let after = run(result, &vec![(0.9, 0.8); 1001]);
    assert_eq!(snapshot(&after), before);
}

#[test]
fn chain_similarity_does_not_transitively_swallow_distinct_endpoints() {
    let result = run(
        assemble(vec![
            plain("word", 100., 100.),
            plain("word", 101.5, 100.),
            plain("word", 103., 100.),
        ]),
        &[(0.8, 0.8), (0.9, 0.8), (0.99, 0.99)],
    );
    assert_eq!(
        snapshot(&result),
        snapshot(&assemble(vec![
            plain("word", 101.5, 100.),
            plain("word", 103., 100.)
        ]))
    );
}

#[test]
fn candidate_matching_multiple_groups_does_not_bridge_groups() {
    retained(
        assemble(vec![
            plain("word", 100., 100.),
            plain("word", 103., 100.),
            plain("word", 101.5, 100.),
        ]),
        &[(0.8, 0.8), (0.9, 0.8), (0.99, 0.99)],
    );
}

#[test]
fn groups_keep_first_slot_order_and_rebuild_all_word_line_indices() {
    let a_crop = horizontal(100., 100., 160., 20.);
    let a_jitter = horizontal(100.25, 100., 160., 20.);
    let b_crop = horizontal(100., 160., 160., 20.);
    let result = run(
        assemble(vec![
            row("ab cd", a_crop, &[("ab", 0.1, 0.4), ("cd", 0.5, 0.9)]),
            plain("中间行", 500., 130.),
            row("ab cd", a_jitter, &[("ab", 0.1, 0.4), ("cd", 0.5, 0.9)]),
            row("ef gh", b_crop, &[("ef", 0.1, 0.4), ("gh", 0.5, 0.9)]),
            row("ef gh", b_crop, &[("ef", 0.1, 0.4), ("gh", 0.5, 0.9)]),
        ]),
        &[(0.8, 0.8), (0.9, 0.8), (0.99, 0.8), (0.99, 0.8), (0.9, 0.8)],
    );
    let expected = assemble(vec![
        row("ab cd", a_jitter, &[("ab", 0.1, 0.4), ("cd", 0.5, 0.9)]),
        plain("中间行", 500., 130.),
        row("ef gh", b_crop, &[("ef", 0.1, 0.4), ("gh", 0.5, 0.9)]),
    ]);
    assert_eq!(snapshot(&result), snapshot(&expected));
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.line)
            .collect::<Vec<_>>(),
        vec![0, 0, 1, 2, 2]
    );
    assert_eq!(
        result.regions,
        result
            .words
            .iter()
            .map(|word| word.rect)
            .collect::<Vec<_>>()
    );
}

#[test]
fn copy_layout_and_selection_marks_use_retained_source_text_and_projections() {
    let crop = geometry([[100., 100.], [260., 112.], [260., 132.], [100., 120.]]);
    let result = run(
        assemble(vec![
            row("倾斜文字", crop, &[("倾斜", 0.1, 0.4), ("文字", 0.5, 0.9)]),
            plain("后续文字", 100., 160.),
            row("倾斜文字", crop, &[("倾斜", 0.1, 0.4), ("文字", 0.5, 0.9)]),
        ]),
        &[(0.8, 0.8), (0.9, 0.8), (0.99, 0.99)],
    );
    let expected = assemble(vec![
        row("倾斜文字", crop, &[("倾斜", 0.1, 0.4), ("文字", 0.5, 0.9)]),
        plain("后续文字", 100., 160.),
    ]);
    let layout = result.copy_layout();
    let expected_layout = expected.copy_layout();
    for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
        for range in [
            0..result.text.len(),
            0.."倾斜".len(),
            "倾斜文字\n".len()..result.text.len(),
        ] {
            assert_eq!(
                layout.copy_mapped(range.clone(), mode).unwrap(),
                expected_layout.copy_mapped(range, mode).unwrap()
            );
        }
    }
    let selection = crate::pin_text_selection::SelectionText::from_recognized(&result);
    let expected_selection = crate::pin_text_selection::SelectionText::from_recognized(&expected);
    for range in [0.."倾斜文字".len(), "倾斜文字\n".len()..result.text.len()] {
        assert_eq!(
            selection.selected(range.clone()),
            expected_selection.selected(range.clone())
        );
        let actual = selection
            .marking_quads(range.clone())
            .into_iter()
            .map(|quad| bits(Some(TextGeometry { quad })))
            .collect::<Vec<_>>();
        let expected = expected_selection
            .marking_quads(range)
            .into_iter()
            .map(|quad| bits(Some(TextGeometry { quad })))
            .collect::<Vec<_>>();
        assert!(!actual.is_empty());
        assert_eq!(actual, expected);
    }
}

#[test]
fn auto_continuation_mapping_uses_rebuilt_retained_line_order() {
    let first = "这是正文第一行";
    let second = "以及正文第二行";
    let first_crop = horizontal(100., 100., 160., 20.);
    let second_crop = horizontal(100., 126., 160., 20.);
    let result = run(
        assemble(vec![
            row(first, first_crop, &[(first, 0., 1.)]),
            row(second, second_crop, &[(second, 0., 1.)]),
            row(first, first_crop, &[(first, 0., 1.)]),
        ]),
        &[(0.8, 0.8), (0.9, 0.8), (0.99, 0.99)],
    );
    let expected = assemble(vec![
        row(first, first_crop, &[(first, 0., 1.)]),
        row(second, second_crop, &[(second, 0., 1.)]),
    ]);
    let layout = result.copy_layout();
    assert_eq!(
        layout
            .copy(0..result.text.len(), CopyLayoutMode::Original)
            .unwrap(),
        format!("{first}\n{second}")
    );
    assert_eq!(
        layout
            .copy(0..result.text.len(), CopyLayoutMode::Auto)
            .unwrap(),
        format!("{first}{second}")
    );
    assert_eq!(
        layout
            .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
            .unwrap(),
        expected
            .copy_layout()
            .copy_mapped(0..expected.text.len(), CopyLayoutMode::Auto)
            .unwrap()
    );
}

#[test]
fn every_checkpoint_failure_propagates_instead_of_returning_partial_result() {
    let fixture = || {
        assemble(vec![
            plain("word", 100., 100.),
            plain("word", 100.25, 100.),
            plain("其它", 500., 160.),
        ])
    };
    let scores = quality(&[(0.8, 0.8), (0.99, 0.99), (0.9, 0.8)]);
    let mut total = 0;
    let complete = deduplicate(fixture(), &scores, Rect::full(2000, 2000), || {
        total += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(complete.lines.len(), 2);
    assert!(total >= 4, "至少覆盖入口、校验／比较和重建的检查边界");
    for cancel_at in 1..=total {
        let mut calls = 0;
        let outcome = deduplicate(fixture(), &scores, Rect::full(2000, 2000), || {
            calls += 1;
            if calls == cancel_at {
                Err("纯模型取消哨兵".into())
            } else {
                Ok(())
            }
        });
        match outcome {
            Err(error) => assert_eq!(error.to_string(), "纯模型取消哨兵"),
            Ok(_) => panic!("第 {cancel_at} 个检查边界错误不得转换为部分成功"),
        }
        assert_eq!(calls, cancel_at);
    }
}

#[test]
fn initial_checkpoint_error_precedes_unchanged_invalid_contract_return() {
    let mut result = pair();
    result.words[1].line = usize::MAX;
    let mut calls = 0;
    let outcome = deduplicate(
        result,
        &quality(&[(0.9, 0.8), (0.99, 0.99)]),
        Rect::full(2000, 2000),
        || {
            calls += 1;
            Err("入口取消哨兵".into())
        },
    );
    match outcome {
        Err(error) => assert_eq!(error.to_string(), "入口取消哨兵"),
        Ok(_) => panic!("入口取消不得被保守原样返回吞掉"),
    }
    assert_eq!(calls, 1);
}
