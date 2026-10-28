//! 实际 SelectionText 消费的纯模型；不读取模型、桌面或剪贴板。
use super::*;
use crate::{
    geometry::Rect,
    recognition::{
        graphemes::{GraphemeSampling, SamplingRun},
        RecognizedLine,
    },
};

fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn g(points: [(f32, f32); 4]) -> TextGeometry {
    TextGeometry {
        quad: points.map(|(x, y)| p(x, y)),
    }
}
fn horizontal() -> TextGeometry {
    g([(0., 0.), (100., 0.), (100., 20.), (0., 20.)])
}
fn rect(geometry: TextGeometry) -> Rect {
    let min_x = geometry
        .quad
        .iter()
        .map(|p| p.x)
        .fold(f32::INFINITY, f32::min);
    let min_y = geometry
        .quad
        .iter()
        .map(|p| p.y)
        .fold(f32::INFINITY, f32::min);
    let max_x = geometry.quad.iter().map(|p| p.x).fold(0., f32::max);
    let max_y = geometry.quad.iter().map(|p| p.y).fold(0., f32::max);
    Rect {
        x: min_x.floor() as u32,
        y: min_y.floor() as u32,
        width: max_x.ceil() as u32 - min_x.floor() as u32,
        height: max_y.ceil() as u32 - min_y.floor() as u32,
    }
}
fn source(text: &str, geometry: TextGeometry, pieces: &[&str]) -> RecognizedText {
    let projection = Projection::new(geometry).unwrap();
    let direction =
        if projection.direction() == crate::recognition::geometry::ReadingDirection::RightToLeft {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        };
    let words = pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let start = index as f64 / pieces.len() as f64;
            let end = (index + 1) as f64 / pieces.len() as f64;
            let geometry = TextGeometry {
                quad: [
                    projection.point(start, 0.),
                    projection.point(end, 0.),
                    projection.point(end, 1.),
                    projection.point(start, 1.),
                ],
            };
            RecognizedWord {
                text: (*piece).into(),
                rect: rect(geometry),
                line: 0,
                horizontal_direction: direction,
                geometry: Some(geometry),
            }
        })
        .collect();
    RecognizedText {
        text: text.into(),
        words,
        regions: Vec::new(),
        lines: vec![RecognizedLine {
            text: text.into(),
            rect: Some(rect(geometry)),
            horizontal_direction: direction,
            geometry: Some(geometry),
            graphemes: None,
        }],
    }
}
fn runs(tokens: &[(&str, usize, usize)]) -> Vec<SamplingRun> {
    let mut byte = 0;
    tokens
        .iter()
        .map(|(token, start, end)| {
            let range = byte..byte + token.len();
            byte = range.end;
            SamplingRun {
                source_range: range,
                steps: *start..*end,
            }
        })
        .collect()
}
fn attach(source: &mut RecognizedText, tokens: &[(&str, usize, usize)]) -> bool {
    attach_dimensions(source, &runs(tokens), 10, 100, 100)
}
fn attach_dimensions(
    source: &mut RecognizedText,
    runs: &[SamplingRun],
    steps: usize,
    content_width: usize,
    tensor_width: usize,
) -> bool {
    let words: Vec<_> = source.words.iter().collect();
    let sampling = GraphemeSampling::from_ctc(
        0,
        &source.lines[0],
        &words,
        runs,
        steps,
        content_width,
        tensor_width,
    );
    let accepted = sampling.is_some();
    source.lines[0].graphemes = sampling;
    accepted
}
fn sampled_ab(geometry: TextGeometry) -> RecognizedText {
    let mut source = source("AB", geometry, &["AB"]);
    assert!(attach(&mut source, &[("A", 1, 2), ("B", 6, 9)]));
    source
}
fn assert_segment(actual: Option<[Point; 2]>, expected: [(f32, f32); 2]) {
    let actual = actual.expect("应有停点截面");
    for (actual, (x, y)) in actual.into_iter().zip(expected) {
        assert!(
            (actual.x - x).abs() <= 0.002 && (actual.y - y).abs() <= 0.002,
            "实际={actual:?}，预期=({x},{y})"
        );
    }
}
fn assert_quad(actual: [Point; 4], expected: [(f32, f32); 4]) {
    for (actual, (x, y)) in actual.into_iter().zip(expected) {
        assert!(
            (actual.x - x).abs() <= 0.002 && (actual.y - y).abs() <= 0.002,
            "实际={actual:?}，预期=({x},{y})"
        );
    }
}
type Snapshot = (
    String,
    Vec<Vec<(Range<usize>, [Point; 4])>>,
    Vec<Range<usize>>,
);
fn snapshot(text: &SelectionText) -> Snapshot {
    (
        text.text.clone(),
        text.lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|cell| (cell.range.clone(), cell_quad(cell)))
                    .collect()
            })
            .collect(),
        text.words.clone(),
    )
}
fn assert_original_fallback(source: &mut RecognizedText) {
    let actual = snapshot(&SelectionText::from_recognized(source));
    let evidence: Vec<_> = source
        .lines
        .iter_mut()
        .map(|line| line.graphemes.take())
        .collect();
    let expected = snapshot(&SelectionText::from_recognized(source));
    for (line, evidence) in source.lines.iter_mut().zip(evidence) {
        line.graphemes = evidence;
    }
    assert_eq!(actual, expected);
}

#[test]
fn ctc_band_centers_split_the_line_into_continuous_cells() {
    // A 带 0.1–0.2、B 带 0.6–0.9：以中心 0.15／0.75 的中线 0.45 切分，首尾贴到裁行两端。
    let source = sampled_ab(horizontal());
    let text = SelectionText::from_recognized(&source);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (45., 0.), (45., 20.), (0., 20.)],
    );
    assert_quad(
        text.marking_quads(1..2)[0],
        [(45., 0.), (100., 0.), (100., 20.), (45., 20.)],
    );
    assert!(text.has_text_at(p(15., 10.)));
    // 字间 blank 不再是选区空洞。
    assert!(text.has_text_at(p(40., 10.)));
    assert!(text.begin(p(40., 10.), 1).is_some());
    assert_eq!(text.highlights(0..2), text.marking_quads(0..2));
    // 同一行的连续原文只画一条带。
    assert_eq!(text.marking_quads(0..2).len(), 1);
    assert_quad(
        text.marking_quads(0..2)[0],
        [(0., 0.), (100., 0.), (100., 20.), (0., 20.)],
    );
}

#[test]
fn neighbor_midline_is_the_shared_caret_stop() {
    let text = SelectionText::from_recognized(&sampled_ab(horizontal()));
    assert_eq!(text.begin(p(11., 10.), 1).unwrap().1, 0..0);
    assert_eq!(text.begin(p(30., 10.), 1).unwrap().1, 1..1);
    assert_eq!(text.begin(p(61., 10.), 1).unwrap().1, 1..1);
    assert_eq!(text.begin(p(89., 10.), 1).unwrap().1, 2..2);
    assert_segment(text.caret_segment(0), [(0., 0.), (0., 20.)]);
    assert_segment(text.caret_segment(2), [(100., 0.), (100., 20.)]);
    // 相邻单元共用中线，byte=1 有唯一插入线。
    assert_segment(text.caret_segment(1), [(45., 0.), (45., 20.)]);
}

#[test]
fn tensor_padding_is_removed_before_original_image_projection() {
    let mut source = source("AB", horizontal(), &["AB"]);
    assert!(attach_dimensions(
        &mut source,
        &runs(&[("A", 1, 2), ("B", 3, 4)]),
        10,
        50,
        100
    ));
    let text = SelectionText::from_recognized(&source);
    // 去掉 padding 后中心为 0.3／0.7；未去掉时会是 0.15／0.35，中线落在 25。
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (50., 0.), (50., 20.), (0., 20.)],
    );
    assert_quad(
        text.marking_quads(1..2)[0],
        [(50., 0.), (100., 0.), (100., 20.), (50., 20.)],
    );
}

#[test]
fn full_crop_projection_can_extend_beyond_word_union_rect() {
    let mut source = source("AB", horizontal(), &["AB"]);
    let narrow = Rect {
        x: 10,
        y: 0,
        width: 80,
        height: 20,
    };
    source.words[0].rect = narrow;
    source.words[0].geometry = None;
    source.lines[0].rect = Some(narrow);
    assert!(attach(&mut source, &[("A", 1, 2), ("B", 6, 9)]));
    let text = SelectionText::from_recognized(&source);
    // 首单元贴到完整裁行起点 x=0，超出词框联合 rect 的 x=10。
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (45., 0.), (45., 20.), (0., 20.)],
    );
    assert_eq!(source.lines[0].rect, Some(narrow));
    assert_eq!(source.lines[0].geometry, Some(horizontal()));
}

#[test]
fn missing_evidence_preserves_original_uniform_word_selection() {
    let source = source("AB", horizontal(), &["AB"]);
    let text = SelectionText::from_recognized(&source);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (50., 0.), (50., 20.), (0., 20.)],
    );
    assert_eq!(text.caret_segment(1), Some([p(50., 0.), p(50., 20.)]));
}

#[test]
fn same_length_source_text_change_rejects_entire_line_evidence() {
    let mut source = sampled_ab(horizontal());
    source.text = "CD".into();
    source.lines[0].text = "CD".into();
    source.words[0].text = "CD".into();
    assert_original_fallback(&mut source);
}

#[test]
fn source_rect_direction_projection_and_word_partition_changes_fall_back() {
    let mut rect_changed = sampled_ab(horizontal());
    rect_changed.words[0].rect.width = 80;
    rect_changed.lines[0].rect.as_mut().unwrap().width = 80;
    assert_original_fallback(&mut rect_changed);

    let mut direction_changed = sampled_ab(horizontal());
    direction_changed.lines[0].horizontal_direction = HorizontalDirection::RightToLeft;
    direction_changed.words[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert_original_fallback(&mut direction_changed);

    let mut projection_changed = sampled_ab(horizontal());
    projection_changed.lines[0].geometry = Some(g([(100., 20.), (0., 20.), (0., 0.), (100., 0.)]));
    assert_original_fallback(&mut projection_changed);

    let mut partition_changed = sampled_ab(horizontal());
    partition_changed.words = source("AB", horizontal(), &["A", "B"]).words;
    assert_original_fallback(&mut partition_changed);
}

#[test]
fn full_source_line_identity_is_checked_before_consuming_any_line() {
    let mut source = sampled_ab(horizontal());
    source.text = "AB\nextra".into();
    assert!(!sampling_source_ready(&source));
    assert_original_fallback(&mut source);
}

#[test]
fn corrected_180_sampling_retains_reading_u_and_transverse_sides() {
    let source = sampled_ab(g([(100., 20.), (0., 20.), (0., 0.), (100., 0.)]));
    let text = SelectionText::from_recognized(&source);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(100., 20.), (55., 20.), (55., 0.), (100., 0.)],
    );
    let (anchor, range) = text.begin(p(89., 10.), 1).unwrap();
    assert_eq!(range, 0..0);
    assert_eq!(
        text.selected(text.extend(&anchor, p(11., 10.))).unwrap(),
        "AB"
    );
}

#[test]
fn vertical_sampling_follows_original_reading_axis_in_both_directions() {
    for (geometry, expected) in [
        (
            g([(20., 0.), (20., 100.), (0., 100.), (0., 0.)]),
            [(20., 0.), (20., 45.), (0., 45.), (0., 0.)],
        ),
        (
            g([(0., 100.), (0., 0.), (20., 0.), (20., 100.)]),
            [(0., 100.), (0., 55.), (20., 55.), (20., 100.)],
        ),
    ] {
        let text = SelectionText::from_recognized(&sampled_ab(geometry));
        assert_quad(text.marking_quads(0..1)[0], expected);
        assert_eq!(text.lines[0].cells[0].range, 0..1);
    }
}

#[test]
fn slanted_sampling_preserves_original_non_axis_aligned_corners() {
    let source = sampled_ab(g([(10., 10.), (110., 50.), (100., 70.), (0., 30.)]));
    let text = SelectionText::from_recognized(&source);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(10., 10.), (55., 28.), (45., 48.), (0., 30.)],
    );
}

#[test]
fn perspective_sampling_uses_ctc_u_in_the_original_homography() {
    // 独立解析 oracle：x=(120u+40v)/(1+v), y=80v/(1+v)。
    let source = sampled_ab(g([(0., 0.), (120., 0.), (80., 40.), (20., 40.)]));
    let text = SelectionText::from_recognized(&source);
    // 中心取带的 u 中点 0.15／0.75，不取透视下四角平均点。
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (54., 0.), (47., 40.), (20., 40.)],
    );
    assert_quad(
        text.marking_quads(1..2)[0],
        [(54., 0.), (120., 0.), (80., 40.), (47., 40.)],
    );
}

#[test]
fn combining_tokens_across_word_fragments_form_one_complete_grapheme_cell() {
    let mut source = source("e\u{301}Z", horizontal(), &["e", "\u{301}", "Z"]);
    assert!(attach(
        &mut source,
        &[("e", 1, 2), ("\u{301}", 3, 4), ("Z", 7, 8)]
    ));
    let text = SelectionText::from_recognized(&source);
    assert_eq!(
        text.lines[0]
            .cells
            .iter()
            .map(|cell| cell.range.clone())
            .collect::<Vec<_>>(),
        vec![0..3, 3..4]
    );
    assert_quad(
        cell_quad(&text.lines[0].cells[0]),
        [(0., 0.), (50., 0.), (50., 20.), (0., 20.)],
    );
    assert_eq!(text.lines[0].cells[0].source_id, 0);
    assert!(text.caret_segment(1).is_none());
    assert_eq!(text.words, vec![0..4]);
    assert_eq!(text.selected(0..3).unwrap(), "e\u{301}");
}

#[test]
fn split_zwj_tokens_keep_one_cell_and_original_utf8_bytes() {
    let original = "👩\u{200d}💻X";
    let mut source = source(original, horizontal(), &[original]);
    assert!(attach(
        &mut source,
        &[("👩", 1, 2), ("\u{200d}", 3, 4), ("💻", 4, 5), ("X", 8, 9)]
    ));
    let text = SelectionText::from_recognized(&source);
    assert_eq!(
        text.lines[0]
            .cells
            .iter()
            .map(|cell| cell.range.clone())
            .collect::<Vec<_>>(),
        vec![0..11, 11..12]
    );
    assert_quad(
        cell_quad(&text.lines[0].cells[0]),
        [(0., 0.), (57.5, 0.), (57.5, 20.), (0., 20.)],
    );
    assert!(text.caret_segment(4).is_none());
    assert_eq!(text.selected(0..11).unwrap(), "👩\u{200d}💻");
    assert_eq!(text.text.as_bytes(), original.as_bytes());
}

#[test]
fn whitespace_has_source_bytes_and_is_covered_by_neighbor_midline() {
    let mut source = source("A B", horizontal(), &["A", "B"]);
    assert!(attach(
        &mut source,
        &[("A", 1, 2), (" ", 3, 4), ("B", 6, 7)]
    ));
    let text = SelectionText::from_recognized(&source);
    assert_eq!(
        text.lines[0]
            .cells
            .iter()
            .map(|cell| cell.range.clone())
            .collect::<Vec<_>>(),
        vec![0..1, 2..3]
    );
    // 空白不单独成格，由两侧中心的中线覆盖；同行选区不在空格处断开。
    assert!(text.has_text_at(p(35., 10.)));
    let (anchor, _) = text.begin(p(11., 10.), 1).unwrap();
    assert_eq!(
        text.selected(text.extend(&anchor, p(95., 10.))).unwrap(),
        "A B"
    );
    assert_eq!(text.marking_quads(0..3).len(), 1);
}

#[test]
fn ambiguous_multi_grapheme_dictionary_token_keeps_original_fallback() {
    let mut source = source("AB", horizontal(), &["AB"]);
    assert!(!attach(&mut source, &[("AB", 1, 8)]));
    let text = SelectionText::from_recognized(&source);
    assert_eq!(text.lines[0].cells.len(), 2);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (50., 0.), (50., 20.), (0., 20.)],
    );
}

#[test]
fn ink_tightened_word_height_does_not_split_the_line_band() {
    let mut source = sampled_ab(horizontal());
    source.words[0].geometry = Some(g([(0., 5.), (100., 5.), (100., 15.), (0., 15.)]));
    let before_text = source.text.clone();
    let before_rect = source.words[0].rect;
    let before_line = source.lines[0].geometry;
    let before_word = source.words[0].geometry;
    let text = SelectionText::from_recognized(&source);
    // 词墨迹高度只属于识别结果，选字与标记沿完整裁行高度。
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (45., 0.), (45., 20.), (0., 20.)],
    );
    assert_quad(
        text.marking_quads(1..2)[0],
        [(45., 0.), (100., 0.), (100., 20.), (45., 20.)],
    );
    assert_eq!(text.highlights(0..2).len(), 1);
    assert_eq!(source.text, before_text);
    assert_eq!(source.words[0].rect, before_rect);
    assert_eq!(source.lines[0].geometry, before_line);
    assert_eq!(source.words[0].geometry, before_word);
}

#[test]
fn off_center_word_geometry_uses_full_line_v_without_moving_ctc_stops() {
    let mut source = sampled_ab(horizontal());
    source.words[0].geometry = Some(g([(0., 3.), (100., 3.), (100., 13.), (0., 13.)]));
    let text = SelectionText::from_recognized(&source);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (45., 0.), (45., 20.), (0., 20.)],
    );
    assert_eq!(text.lines[0].cells[0].source_id, 0);
}

#[test]
fn ink_frame_with_outside_u_keeps_line_projection_and_valid_caret() {
    let mut source = sampled_ab(horizontal());
    source.words[0].geometry = Some(g([(15., 5.), (85., 5.), (85., 15.), (15., 15.)]));
    let text = SelectionText::from_recognized(&source);
    assert_eq!(text.lines[0].cells[0].source_id, 0);
    assert_eq!(text.lines[0].cells[1].source_id, 0);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(0., 0.), (45., 0.), (45., 20.), (0., 20.)],
    );
    assert_segment(text.caret_segment(0), [(0., 0.), (0., 20.)]);
    assert_segment(text.caret_segment(2), [(100., 0.), (100., 20.)]);
}

#[test]
fn total_grapheme_and_run_budgets_reject_new_evidence_before_line_consumption() {
    let body = "A".repeat(4096);
    let emissions: Vec<_> = (0..4096)
        .map(|byte| SamplingRun {
            source_range: byte..byte + 1,
            steps: byte * 2..byte * 2 + 1,
        })
        .collect();
    let mut source = RecognizedText {
        text: String::new(),
        words: Vec::new(),
        lines: Vec::new(),
        regions: Vec::new(),
    };
    for index in 0..9 {
        let mut row = self::source(&body, horizontal(), &[&body]);
        row.words[0].line = index;
        let sampling = GraphemeSampling::from_ctc(
            index,
            &row.lines[0],
            &[&row.words[0]],
            &emissions,
            8192,
            8192,
            8192,
        )
        .unwrap();
        row.lines[0].graphemes = Some(sampling);
        source.words.extend(row.words);
        source.lines.extend(row.lines);
    }
    source.text = source
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!sampling_source_ready(&source));
    assert_original_fallback(&mut source);
}

#[test]
fn total_run_budget_is_independent_of_grapheme_budget_and_keeps_old_fallback() {
    let count = graphemes::MAX_EMISSION_RUNS;
    let body = format!("A{}", "\u{301}".repeat(count - 1));
    assert_eq!(body.len(), 65535);
    let mut first = source(&body, horizontal(), &[&body]);
    let mut emissions = vec![SamplingRun {
        source_range: 0..1,
        steps: 0..1,
    }];
    emissions.extend((1..count).map(|index| SamplingRun {
        source_range: 1 + (index - 1) * 2..1 + index * 2,
        steps: index * 2..index * 2 + 1,
    }));
    assert!(attach_dimensions(
        &mut first,
        &emissions,
        graphemes::MAX_SAMPLING_STEPS,
        100,
        100,
    ));
    let mut second = source(
        "B",
        g([(0., 30.), (100., 30.), (100., 50.), (0., 50.)]),
        &["B"],
    );
    second.words[0].line = 1;
    second.lines[0].graphemes = GraphemeSampling::from_ctc(
        1,
        &second.lines[0],
        &[&second.words[0]],
        &runs(&[("B", 1, 2)]),
        10,
        100,
        100,
    );
    assert!(second.lines[0].graphemes.is_some());
    first.text.push_str("\nB");
    first.words.extend(second.words);
    first.lines.extend(second.lines);
    let proofs: Vec<_> = first
        .lines
        .iter()
        .map(|line| line.graphemes.as_ref().unwrap())
        .collect();
    assert!(first.text.len() < graphemes::MAX_SOURCE_BYTES);
    assert_eq!(
        proofs
            .iter()
            .map(|proof| proof.grapheme_count())
            .sum::<usize>(),
        2
    );
    assert_eq!(
        proofs.iter().map(|proof| proof.run_count()).sum::<usize>(),
        graphemes::MAX_TOTAL_RUNS + 1
    );
    assert!(!sampling_source_ready(&first));
    // 旧路线只有两个完整字素；不能由同时超过字素上限掩盖 runs 守卫。
    assert_eq!(
        SelectionText::from_recognized(&first)
            .lines
            .iter()
            .map(|line| line.cells.len())
            .sum::<usize>(),
        2
    );
    assert_original_fallback(&mut first);
}

#[test]
fn source_byte_budget_disables_only_the_new_sampling_route() {
    let mut source = sampled_ab(horizontal());
    source.text = "A".repeat(graphemes::MAX_SOURCE_BYTES + 1);
    source.lines[0].text = source.text.clone();
    source.words[0].text = source.text.clone();
    assert!(!sampling_source_ready(&source));
    assert_eq!(
        source.lines[0].graphemes.as_ref().unwrap().grapheme_count(),
        2
    );
    assert_eq!(source.text.len(), graphemes::MAX_SOURCE_BYTES + 1);
}

#[test]
fn crlf_remains_one_grapheme_while_valid_next_line_consumes_sampling() {
    let mut first = source("A\r", horizontal(), &["A\r"]);
    assert!(!attach(&mut first, &[("A", 1, 2), ("\r", 3, 4)]));
    let mut second = source(
        "B",
        g([(0., 30.), (100., 30.), (100., 50.), (0., 50.)]),
        &["B"],
    );
    second.words[0].line = 1;
    second.lines[0].graphemes = GraphemeSampling::from_ctc(
        1,
        &second.lines[0],
        &[&second.words[0]],
        &runs(&[("B", 2, 8)]),
        10,
        100,
        100,
    );
    assert!(second.lines[0].graphemes.is_some());
    first.text = "A\r\nB".into();
    first.words.extend(second.words);
    first.lines.extend(second.lines);
    let text = SelectionText::from_recognized(&first);
    assert_eq!(text.text, "A\r\nB");
    assert_eq!(text.lines[1].cells[0].range, 3..4);
    assert_quad(
        cell_quad(&text.lines[1].cells[0]),
        [(0., 30.), (100., 30.), (100., 50.), (0., 50.)],
    );
    assert!(text.caret_segment(2).is_none());
    assert!(text.caret_segment(3).is_some());
}

fn normal_inset(geometry: TextGeometry, inset: f64) -> TextGeometry {
    let projection = Projection::new(geometry).unwrap();
    TextGeometry {
        quad: [
            projection.point(0., inset),
            projection.point(1., inset),
            projection.point(1., 1. - inset),
            projection.point(0., 1. - inset),
        ],
    }
}

#[test]
fn selection_height_tightens_without_changing_sampling_or_reading_stops() {
    for original in [
        horizontal(),
        g([(100., 20.), (0., 20.), (0., 0.), (100., 0.)]),
        g([(20., 0.), (20., 100.), (0., 100.), (0., 0.)]),
        g([(0., 100.), (0., 0.), (20., 0.), (20., 100.)]),
        g([(10., 10.), (150., 20.), (130., 70.), (20., 60.)]),
    ] {
        let mut source = sampled_ab(original);
        let original_projection = Projection::new(original).unwrap();
        let before = SelectionText::from_recognized(&source);
        let words: Vec<_> = source.words.iter().collect();
        let bands = source.lines[0]
            .graphemes
            .as_ref()
            .unwrap()
            .validated_bands(0, &source.lines[0], &words)
            .unwrap();
        let tightened = normal_inset(original, 0.25);
        let proof = source.lines[0].graphemes.as_mut().unwrap();
        assert!(proof.tighten_selection_geometry(tightened));
        assert_eq!(proof.source_geometry(), original);
        assert_eq!(proof.selection_geometry(), tightened);
        assert_eq!(source.lines[0].geometry, Some(original));
        assert_eq!(
            source.lines[0].graphemes.as_ref().unwrap().validated_bands(
                0,
                &source.lines[0],
                &words
            ),
            Some(bands)
        );
        let after = SelectionText::from_recognized(&source);
        assert_eq!(after.selected(0..2), before.selected(0..2));
        assert_eq!(after.words, before.words);
        for (range, start, end) in [(0..1, 0., 0.45), (1..2, 0.45, 1.)] {
            let quad = after.marking_quads(range)[0];
            let expected = [
                original_projection.point(start, 0.25),
                original_projection.point(end, 0.25),
                original_projection.point(end, 0.75),
                original_projection.point(start, 0.75),
            ];
            assert_quad(quad, expected.map(|p| (p.x, p.y)));
        }
        for (byte, u) in [(0, 0.), (1, 0.45), (2, 1.)] {
            let segment = after.caret_segment(byte).unwrap();
            let expected = [
                original_projection.point(u, 0.25),
                original_projection.point(u, 0.75),
            ];
            assert_segment(Some(segment), expected.map(|p| (p.x, p.y)));
        }
        // 原行两端和法向中心不移，字间分界的命中与复制范围保持。
        for u in [0.1, 0.3, 0.6, 0.9] {
            let point = original_projection.point(u, 0.5);
            assert_eq!(
                after.begin(point, 1).map(|(_, range)| range),
                before.begin(point, 1).map(|(_, range)| range)
            );
        }
        assert!(!after.has_text_at(original_projection.point(0.5, 0.1)));
        assert!(after.has_text_at(original_projection.point(0.5, 0.5)));
    }
}

#[test]
fn selection_height_rejects_offsets_reading_axis_changes_and_invalid_geometry() {
    let mut source = sampled_ab(horizontal());
    let proof = source.lines[0].graphemes.as_mut().unwrap();
    assert_eq!(proof.selection_geometry(), horizontal());
    for invalid in [
        g([(0., 4.), (100., 4.), (100., 14.), (0., 14.)]),
        g([(10., 5.), (90., 5.), (90., 15.), (10., 15.)]),
        g([(0., -2.), (100., -2.), (100., 22.), (0., 22.)]),
        g([(0., 5.), (100., 6.), (100., 15.), (0., 15.)]),
        g([(100., 15.), (0., 15.), (0., 5.), (100., 5.)]),
        g([(0., 5.), (0., 5.), (100., 15.), (0., 15.)]),
        g([(f32::NAN, 5.), (100., 5.), (100., 15.), (0., 15.)]),
    ] {
        assert!(!proof.tighten_selection_geometry(invalid));
        assert_eq!(proof.selection_geometry(), horizontal());
    }
    let tightened = normal_inset(horizontal(), 0.25);
    assert!(proof.tighten_selection_geometry(tightened));
    assert!(!proof.tighten_selection_geometry(normal_inset(horizontal(), 0.1)));
    assert_eq!(proof.selection_geometry(), tightened);
    // 来源一旦发生变化，不能只凭仍合法的显示几何继续消费原采样证据。
    source.lines[0].text = "CD".into();
    source.words[0].text = "CD".into();
    source.text = "CD".into();
    assert_original_fallback(&mut source);
}

#[test]
fn selection_reading_ends_keep_every_internal_boundary_in_original_coordinates() {
    for original in [
        horizontal(),
        g([(100., 20.), (0., 20.), (0., 0.), (100., 0.)]),
        g([(20., 0.), (20., 100.), (0., 100.), (0., 0.)]),
        g([(0., 100.), (0., 0.), (20., 0.), (20., 100.)]),
        g([(10., 10.), (150., 20.), (130., 70.), (20., 60.)]),
    ] {
        let mut source = source("ABC", original, &["ABC"]);
        assert!(attach(
            &mut source,
            &[("A", 1, 2), ("B", 4, 5), ("C", 8, 9)]
        ));
        let height = normal_inset(original, 0.2);
        assert!(source.lines[0]
            .graphemes
            .as_mut()
            .unwrap()
            .tighten_selection_geometry(height));
        let before = SelectionText::from_recognized(&source);
        let original_proof = source.lines[0].graphemes.clone().unwrap();
        let proof = source.lines[0].graphemes.as_mut().unwrap();
        assert_eq!(proof.selection_reading_range(), [0., 1.]);
        assert!(proof.tighten_selection_reading_range([0.08, 0.94]));
        assert_eq!(proof.source_geometry(), original);
        assert_eq!(proof.selection_geometry(), height);
        assert_eq!(proof.run_count(), original_proof.run_count());
        let words: Vec<_> = source.words.iter().collect();
        assert_eq!(
            source.lines[0].graphemes.as_ref().unwrap().validated_bands(
                0,
                &source.lines[0],
                &words
            ),
            original_proof.validated_bands(0, &source.lines[0], &words)
        );
        let after = SelectionText::from_recognized(&source);
        assert_eq!(after.selected(0..3), Some("ABC".into()));
        assert_eq!(after.words, before.words);
        for (index, (after, before)) in after.lines[0]
            .cells
            .iter()
            .zip(&before.lines[0].cells)
            .enumerate()
        {
            let after = after.projected.unwrap();
            let before = before.projected.unwrap();
            assert_eq!(after.source, before.source);
            if index > 0 {
                assert_eq!(after.start, before.start);
            }
            if index < 2 {
                assert_eq!(after.end, before.end);
            }
        }
        for byte in [1, 2] {
            assert_eq!(after.caret_segment(byte), before.caret_segment(byte));
        }
        let projection = Projection::new(height).unwrap();
        for (byte, u) in [(0, 0.08), (3, 0.94)] {
            assert_ne!(after.caret_segment(byte), before.caret_segment(byte));
            assert_segment(
                after.caret_segment(byte),
                [projection.point(u, 0.), projection.point(u, 1.)].map(|p| (p.x, p.y)),
            );
        }
        assert!(!after.has_text_at(projection.point(0.02, 0.5)));
        assert!(!after.has_text_at(projection.point(0.98, 0.5)));
        for u in [0.15, 0.4, 0.6, 0.85] {
            let point = projection.point(u, 0.5);
            assert_eq!(
                after.begin(point, 1).map(|(_, range)| range),
                before.begin(point, 1).map(|(_, range)| range)
            );
        }
    }
}

#[test]
fn selection_reading_ends_allow_one_grapheme_and_ignore_whitespace_cells() {
    let mut single = source("中", horizontal(), &["中"]);
    assert!(attach(&mut single, &[("中", 3, 7)]));
    assert!(single.lines[0]
        .graphemes
        .as_mut()
        .unwrap()
        .tighten_selection_reading_range([0.24, 0.79]));
    let text = SelectionText::from_recognized(&single);
    assert_eq!(text.lines[0].cells.len(), 1);
    assert_eq!(text.selected(0..3), Some("中".into()));
    assert_quad(
        text.marking_quads(0..3)[0],
        [(24., 0.), (79., 0.), (79., 20.), (24., 20.)],
    );

    let mut spaced = source(" A B ", horizontal(), &["A", "B"]);
    assert!(attach(
        &mut spaced,
        &[
            (" ", 0, 1),
            ("A", 1, 3),
            (" ", 4, 5),
            ("B", 6, 8),
            (" ", 9, 10)
        ]
    ));
    let before = SelectionText::from_recognized(&spaced);
    assert!(spaced.lines[0]
        .graphemes
        .as_mut()
        .unwrap()
        .tighten_selection_reading_range([0.12, 0.85]));
    let after = SelectionText::from_recognized(&spaced);
    assert_eq!(after.lines[0].cells.len(), 2);
    assert_eq!(after.selected(0..5), Some(" A B ".into()));
    assert_eq!(after.caret_segment(2), before.caret_segment(2));
    assert_eq!(after.caret_segment(3), before.caret_segment(3));
    assert_eq!(
        after.lines[0].cells[0].projected.unwrap().end,
        before.lines[0].cells[0].projected.unwrap().end
    );
    assert_eq!(
        after.lines[0].cells[1].projected.unwrap().start,
        before.lines[0].cells[1].projected.unwrap().start
    );
    assert_quad(
        after.marking_quads(0..5)[0],
        [(12., 0.), (85., 0.), (85., 20.), (12., 20.)],
    );
}

#[test]
fn selection_reading_ends_reject_invalid_or_crossing_ranges_without_losing_height() {
    let mut recognized = sampled_ab(horizontal());
    let height = normal_inset(horizontal(), 0.25);
    assert!(recognized.lines[0]
        .graphemes
        .as_mut()
        .unwrap()
        .tighten_selection_geometry(height));
    let before = snapshot(&SelectionText::from_recognized(&recognized));
    for invalid in [
        [f64::NAN, 0.9],
        [0.1, f64::INFINITY],
        [-0.1, 0.9],
        [0.1, 1.1],
        [0.8, 0.2],
        [0.5, 0.5],
        [0.45, 0.9],
        [0.1, 0.45],
    ] {
        let proof = recognized.lines[0].graphemes.as_mut().unwrap();
        assert!(!proof.tighten_selection_reading_range(invalid));
        assert_eq!(proof.selection_reading_range(), [0., 1.]);
        assert_eq!(proof.selection_geometry(), height);
        assert_eq!(
            snapshot(&SelectionText::from_recognized(&recognized)),
            before
        );
    }
    let proof = recognized.lines[0].graphemes.as_mut().unwrap();
    assert!(proof.tighten_selection_reading_range([0.1, 0.9]));
    assert!(!proof.tighten_selection_reading_range([0.09, 0.9]));
    assert!(!proof.tighten_selection_reading_range([0.1, 0.91]));
    assert_eq!(proof.selection_reading_range(), [0.1, 0.9]);
    recognized.lines[0].text = "CD".into();
    recognized.words[0].text = "CD".into();
    recognized.text = "CD".into();
    assert_original_fallback(&mut recognized);

    let missing = source("AB", horizontal(), &["AB"]);
    let text = SelectionText::from_recognized(&missing);
    assert_quad(
        text.marking_quads(0..2)[0],
        [(0., 0.), (100., 0.), (100., 20.), (0., 20.)],
    );
}

fn horizontal_rectification(original: TextGeometry, top: f32, bottom: f32) -> TextGeometry {
    let projection = Projection::new(original).unwrap();
    let start = projection.point(0., 0.5).x;
    let end = projection.point(1., 0.5).x;
    let (v0, v1) = if start <= end {
        (top, bottom)
    } else {
        (bottom, top)
    };
    g([(start, v0), (end, v0), (end, v1), (start, v1)])
}

#[test]
fn selection_rectification_flattens_supported_rows_without_moving_internal_caret_x() {
    for original in [
        g([(10., 20.), (110., 22.), (110., 42.), (10., 40.)]),
        g([(110., 42.), (10., 40.), (10., 20.), (110., 22.)]),
    ] {
        let mut source = sampled_ab(original);
        let before = SelectionText::from_recognized(&source);
        let original_proof = source.lines[0].graphemes.clone().unwrap();
        let geometry = horizontal_rectification(original, 25., 37.);
        let proof = source.lines[0].graphemes.as_mut().unwrap();
        assert!(!proof.selection_rectified());
        assert!(proof.tighten_selection_reading_range([0.1, 0.9]));
        assert!(proof.rectify_selection_geometry(geometry));
        assert!(proof.selection_rectified());
        assert_eq!(proof.source_geometry(), original);
        assert_eq!(proof.selection_reading_range(), [0.1, 0.9]);
        let words: Vec<_> = source.words.iter().collect();
        assert_eq!(
            source.lines[0].graphemes.as_ref().unwrap().validated_bands(
                0,
                &source.lines[0],
                &words
            ),
            original_proof.validated_bands(0, &source.lines[0], &words)
        );
        let after = SelectionText::from_recognized(&source);
        assert_eq!(after.selected(0..2), Some("AB".into()));
        let before_caret = before.caret_segment(1).unwrap();
        let after_caret = after.caret_segment(1).unwrap();
        for (before, after) in before_caret.into_iter().zip(after_caret) {
            assert!((before.x - after.x).abs() <= 0.001);
        }
        assert_ne!(before.caret_segment(1), after.caret_segment(1));
        let (v0, v1) = (geometry.quad[0].y, geometry.quad[2].y);
        for cell in &after.lines[0].cells {
            let quad = cell_quad(cell);
            assert_eq!(quad[0].y, v0);
            assert_eq!(quad[1].y, v0);
            assert_eq!(quad[2].y, v1);
            assert_eq!(quad[3].y, v1);
        }
        for byte in [0, 1, 2] {
            let segment = after.caret_segment(byte).unwrap();
            assert_eq!([segment[0].y, segment[1].y], [v0, v1]);
        }
    }
}

#[test]
fn selection_rectification_rejects_unsupported_tilt_perspective_and_changed_reading_x() {
    for original in [
        g([(10., 20.), (110., 28.), (110., 48.), (10., 40.)]),
        g([(20., 0.), (20., 100.), (0., 100.), (0., 0.)]),
        g([(10., 20.), (110., 22.), (95., 42.), (20., 40.)]),
    ] {
        let mut source = sampled_ab(original);
        let before = snapshot(&SelectionText::from_recognized(&source));
        let proof = source.lines[0].graphemes.as_mut().unwrap();
        assert!(!proof.rectify_selection_geometry(horizontal_rectification(original, 25., 37.)));
        assert!(!proof.selection_rectified());
        assert_eq!(proof.selection_geometry(), original);
        assert_eq!(snapshot(&SelectionText::from_recognized(&source)), before);
    }
    let original = g([(10., 20.), (110., 22.), (110., 42.), (10., 40.)]);
    let mut source = sampled_ab(original);
    let proof = source.lines[0].graphemes.as_mut().unwrap();
    assert!(!proof.rectify_selection_geometry(original));
    assert!(!proof.rectify_selection_geometry(g([
        (11., 25.),
        (109., 25.),
        (109., 37.),
        (11., 37.)
    ])));
    assert!(!proof.selection_rectified());
    let default = SelectionText::from_recognized(&source);
    assert_quad(
        default.marking_quads(0..2)[0],
        original.quad.map(|p| (p.x, p.y)),
    );
    source.lines[0].graphemes = None;
    let missing = SelectionText::from_recognized(&source);
    assert_quad(
        missing.marking_quads(0..2)[0],
        original.quad.map(|p| (p.x, p.y)),
    );
}
