//! 合成CTC张量验证真实发射证据及接线；不执行ORT或证明真实字形墨迹。
use super::*;
use crate::recognition::{geometry::Projection, graphemes::GraphemeBand};

struct Scripted {
    outputs: VecDeque<Tensor>,
    calls: usize,
}
impl ModelRunner for Scripted {
    fn run(&mut self, _: Tensor) -> Result<Tensor> {
        self.calls += 1;
        self.outputs
            .pop_front()
            .ok_or_else(|| "未预期额外推理".into())
    }
}

fn source() -> Image {
    Image {
        width: 256,
        height: 256,
        bgra: vec![255; 256 * 256 * 4],
        readback_ms: 0.,
    }
}
fn ctc(sequence: &[usize], dictionary_len: usize, confidence: f32) -> Tensor {
    let classes = dictionary_len + 2;
    let mut values = vec![0.; sequence.len() * classes];
    for (step, &index) in sequence.iter().enumerate() {
        values[step * classes + index] = if index == 0 { 1. } else { confidence };
    }
    Tensor {
        shape: vec![1, sequence.len(), classes],
        values,
    }
}
fn orientation(rotated: bool) -> Tensor {
    Tensor {
        shape: vec![1, 2],
        values: if rotated {
            vec![0.01, 0.99]
        } else {
            vec![0.99, 0.01]
        },
    }
}
fn detection(quad: [Point; 4], confidence: f32) -> DetectedLine {
    DetectedLine {
        quad,
        confidence,
        rect: bounds(&quad, 256, 256).unwrap(),
    }
}
fn quad(y: f32) -> [Point; 4] {
    [[20., y], [150., y], [150., y + 20.], [20., y + 20.]]
}
fn recognize_script(
    detected: &[DetectedLine],
    rotations: &[bool],
    outputs: Vec<Tensor>,
    dictionary: &[&str],
) -> RecognitionCandidate {
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let mut cls = Scripted {
        outputs: rotations.iter().copied().map(orientation).collect(),
        calls: 0,
    };
    let mut rec = Scripted {
        outputs: outputs.into(),
        calls: 0,
    };
    let dictionary: Vec<String> = dictionary
        .iter()
        .map(|entry| (*entry).to_string())
        .collect();
    let result = recognize_lines(&rgb, detected, &mut cls, &mut rec, &dictionary).unwrap();
    assert_eq!((cls.calls, rec.calls), (detected.len(), detected.len()));
    assert!(cls.outputs.is_empty() && rec.outputs.is_empty());
    result
}
fn single(dictionary: &[&str], sequence: &[usize]) -> RecognizedText {
    recognize_script(
        &[detection(quad(40.), 0.99)],
        &[false],
        vec![ctc(sequence, dictionary.len(), 0.99)],
        dictionary,
    )
    .result
}
fn bands(result: &RecognizedText, index: usize) -> Vec<GraphemeBand> {
    let line = &result.lines[index];
    let words: Vec<_> = result
        .words
        .iter()
        .filter(|word| word.line == index)
        .collect();
    line.graphemes
        .as_ref()
        .unwrap()
        .validated_bands(index, line, &words)
        .unwrap()
}
fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn decoded_emissions_keep_unequal_runs_before_legacy_voronoi_expansion() {
    let dictionary = vec!["A".into(), "B".into(), "C".into()];
    let sequence = [0, 1, 1, 0, 0, 0, 2, 2, 2, 2, 0, 3, 0, 0];
    let decoded = decode_ctc(ctc(&sequence, 3, 0.99), &dictionary).unwrap();
    assert_eq!(
        decoded.emissions.unwrap(),
        [
            SamplingRun {
                source_range: 0..1,
                steps: 1..3
            },
            SamplingRun {
                source_range: 1..2,
                steps: 6..10
            },
            SamplingRun {
                source_range: 2..3,
                steps: 11..12
            },
        ]
    );
    assert_eq!(
        decoded
            .tokens
            .iter()
            .map(|token| (token.start, token.end))
            .collect::<Vec<_>>(),
        [(0, 5), (5, 10), (9, 14)]
    );
    assert_eq!(fragments(&decoded.tokens)[0].text, "ABC");
    assert_eq!(
        (
            fragments(&decoded.tokens)[0].start,
            fragments(&decoded.tokens)[0].end
        ),
        (0, 14)
    );
    assert!((decoded.confidence - 0.99).abs() < 1e-6);
}

#[test]
fn producer_attaches_real_unequal_bands_while_word_and_rect_stay_original() {
    let result = single(
        &["A", "B", "C"],
        &[0, 1, 1, 0, 0, 0, 2, 2, 2, 2, 0, 3, 0, 0],
    );
    assert_eq!(result.text, "ABC");
    assert_eq!(result.words.len(), 1);
    assert_eq!(result.words[0].text, "ABC");
    let expected = Rect {
        x: 20,
        y: 40,
        width: 130,
        height: 20,
    };
    assert_eq!(result.words[0].rect, expected);
    assert_eq!(result.regions, [expected]);
    assert_eq!(result.lines[0].rect, Some(expected));
    assert_eq!(result.words[0].geometry, result.lines[0].geometry);
    let actual = bands(&result, 0);
    for (band, (start, end)) in actual.iter().zip([(1., 3.), (6., 10.), (11., 12.)]) {
        close(band.start, start / 14.);
        close(band.end, end / 14.);
    }
    assert_eq!(
        actual
            .iter()
            .map(|band| band.source_range.clone())
            .collect::<Vec<_>>(),
        [0..1, 1..2, 2..3]
    );
    assert!(actual[0].end < actual[1].start && actual[1].end < actual[2].start);
}

#[test]
fn trim_rebases_source_bytes_but_keeps_original_emission_steps_and_internal_space() {
    let dictionary = vec!["A".into(), "B".into()];
    let sequence = [3, 3, 0, 1, 0, 3, 0, 2, 0, 3, 3];
    let decoded = decode_ctc(ctc(&sequence, 2, 0.99), &dictionary).unwrap();
    assert_eq!(
        decoded.emissions.unwrap(),
        [
            SamplingRun {
                source_range: 0..1,
                steps: 3..4
            },
            SamplingRun {
                source_range: 1..2,
                steps: 5..6
            },
            SamplingRun {
                source_range: 2..3,
                steps: 7..8
            },
        ]
    );
    let result = single(&["A", "B"], &sequence);
    assert_eq!(result.text, "A B");
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["A", "B"]
    );
    let actual = bands(&result, 0);
    assert_eq!(actual.len(), 3);
    close(actual[0].start, 3. / 11.);
    close(actual[1].start, 5. / 11.);
    close(actual[2].end, 8. / 11.);
}

#[test]
fn blank_separated_same_symbol_retains_two_runs_and_two_complete_graphemes() {
    let result = single(&["A"], &[1, 1, 0, 1, 1, 0]);
    assert_eq!(result.text, "AA");
    let evidence = result.lines[0].graphemes.as_ref().unwrap();
    assert_eq!((evidence.grapheme_count(), evidence.run_count()), (2, 2));
    let actual = bands(&result, 0);
    close(actual[0].end, 2. / 6.);
    close(actual[1].start, 3. / 6.);
}

#[test]
fn combining_scalar_tokens_merge_into_whole_grapheme_across_existing_words() {
    let result = single(&["e", "\u{301}", "x"], &[1, 0, 2, 0, 3, 0]);
    assert_eq!(result.text, "e\u{301}x");
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["e", "\u{301}", "x"]
    );
    let evidence = result.lines[0].graphemes.as_ref().unwrap();
    assert_eq!((evidence.grapheme_count(), evidence.run_count()), (2, 3));
    let actual = bands(&result, 0);
    assert_eq!(actual[0].source_range, 0..3);
    assert_eq!(actual[1].source_range, 3..4);
    close(actual[0].start, 0.);
    close(actual[0].end, 3. / 6.);
    close(actual[1].start, 4. / 6.);
}

#[test]
fn zwj_scalar_tokens_share_one_grapheme_envelope_without_new_text_or_word_split() {
    let result = single(&["👩", "\u{200d}", "💻", "x"], &[1, 0, 2, 0, 3, 0, 4, 0]);
    assert_eq!(result.text, "👩\u{200d}💻x");
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["👩", "\u{200d}", "💻", "x"]
    );
    let evidence = result.lines[0].graphemes.as_ref().unwrap();
    assert_eq!((evidence.grapheme_count(), evidence.run_count()), (2, 4));
    let actual = bands(&result, 0);
    assert_eq!(actual[0].source_range, 0..11);
    assert_eq!(actual[1].source_range, 11..12);
    close(actual[0].end, 5. / 8.);
    close(actual[1].start, 6. / 8.);
}

#[test]
fn multiscalar_single_grapheme_dictionary_entry_keeps_one_true_run() {
    let result = single(&["e\u{301}"], &[0, 1, 1, 0]);
    assert_eq!(result.text, "e\u{301}");
    let evidence = result.lines[0].graphemes.as_ref().unwrap();
    assert_eq!((evidence.grapheme_count(), evidence.run_count()), (1, 1));
    let actual = bands(&result, 0);
    assert_eq!(actual[0].source_range, 0..3);
    close(actual[0].start, 0.25);
    close(actual[0].end, 0.75);
}

#[test]
fn dictionary_token_spanning_multiple_graphemes_drops_whole_line_evidence_only() {
    let result = single(&["AB", "x"], &[0, 1, 0, 2, 0]);
    assert_eq!(result.text, "ABx");
    assert_eq!(result.words.len(), 1);
    assert_eq!(result.words[0].text, "ABx");
    assert_eq!(
        result.words[0].rect,
        Rect {
            x: 20,
            y: 40,
            width: 130,
            height: 20
        }
    );
    assert!(result.lines[0].geometry.is_some());
    assert!(result.lines[0].graphemes.is_none());
}

#[test]
fn emitting_into_tensor_padding_drops_evidence_without_clamping_new_band() {
    let narrow = [[20., 40.], [40., 40.], [40., 65.], [20., 65.]];
    let result = recognize_script(
        &[detection(narrow, 0.99)],
        &[false],
        vec![ctc(&[0, 1, 0, 2], 2, 0.99)],
        &["A", "B"],
    )
    .result;
    // 内容39px、张量40px；最后一个真实emit越过content，不冒称沿图边精确停点。
    assert_eq!(recognition_width(20, 25).unwrap(), (39, 40));
    assert_eq!(result.text, "AB");
    assert_eq!(result.words.len(), 1);
    assert_eq!(
        result.words[0].rect,
        Rect {
            x: 20,
            y: 40,
            width: 20,
            height: 25
        }
    );
    assert!(result.lines[0].geometry.is_some());
    assert!(result.lines[0].graphemes.is_none());
}

#[test]
fn empty_dictionary_emission_keeps_decoding_contract_and_disables_raw_evidence() {
    let decoded = decode_ctc(
        ctc(&[1, 0, 2, 0, 3, 0], 3, 0.99),
        &["A".into(), String::new(), "B".into()],
    )
    .unwrap();
    assert_eq!(decoded.tokens.len(), 3);
    assert_eq!(decoded.tokens[0].text, "A");
    assert!(decoded.tokens[1].text.is_empty());
    assert_eq!(decoded.tokens[2].text, "B");
    assert_eq!(decoded.tokens[0].start, 0);
    assert_eq!(decoded.tokens[2].end, 6);
    assert!((decoded.confidence - 0.99).abs() < 1e-6);
    assert!(decoded.emissions.is_none());
}

#[test]
fn physical_line_control_text_preserves_ctc_body_but_has_no_sampling_evidence() {
    for entry in ["\r\n", "\t", "\u{0}"] {
        let result = single(&["A", entry, "B"], &[1, 0, 2, 0, 3, 0]);
        assert_eq!(result.text, format!("A{entry}B"));
        assert_eq!(result.lines[0].text, format!("A{entry}B"));
        assert!(result.lines[0].graphemes.is_none());
    }
}

#[test]
fn producer_keeps_original_full_projection_for_upright_flipped_vertical_slant_and_perspective() {
    let quads = [
        quad(40.),
        [[30., 20.], [50., 20.], [50., 180.], [30., 180.]],
        [[30., 30.], [160., 60.], [155., 82.], [25., 52.]],
        [[20., 30.], [180., 42.], [155., 86.], [28., 72.]],
    ];
    for quad in quads {
        for rotated in [false, true] {
            let result = recognize_script(
                &[detection(quad, 0.99)],
                &[rotated],
                vec![ctc(&[0, 1, 1, 0, 0, 2, 2, 0], 2, 0.99)],
                &["A", "B"],
            )
            .result;
            let mut crop = Crop::new(quad).unwrap();
            crop.rotated_180 = rotated;
            let evidence = result.lines[0].graphemes.as_ref().unwrap();
            assert_eq!(Some(evidence.source_geometry()), crop.text_geometry(0., 1.));
            assert_eq!(
                result.lines[0].horizontal_direction,
                crop.horizontal_direction()
            );
            let projection = Projection::new(evidence.source_geometry()).unwrap();
            let first = &bands(&result, 0)[0];
            let actual = projection.point(first.start, 0.5);
            let expected = crop.point(first.start as f32, 0.5);
            assert!((actual.x - expected[0]).abs() < 1e-3);
            assert!((actual.y - expected[1]).abs() < 1e-3);
            if rotated && quad == quads[0] {
                assert!(projection.point(0., 0.5).x > projection.point(1., 0.5).x);
            }
        }
    }
}

#[test]
fn dedup_keeps_winning_line_sampling_runs_with_original_source_geometry() {
    let detected = [detection(quad(40.), 0.95), detection(quad(40.), 0.95)];
    let result = recognize_script(
        &detected,
        &[false, false],
        vec![
            ctc(&[1, 0, 2, 0], 2, 0.65),
            ctc(&[0, 1, 1, 0, 2, 0], 2, 0.99),
        ],
        &["A", "B"],
    )
    .result;
    assert_eq!(result.text, "AB");
    assert_eq!(result.lines.len(), 1);
    let actual = bands(&result, 0);
    close(actual[0].start, 1. / 6.);
    close(actual[0].end, 3. / 6.);
    close(actual[1].start, 4. / 6.);
}

#[test]
fn quality_recovery_clones_line_local_evidence_and_reordering_keeps_valid_binding() {
    let primary = recognize_script(
        &[detection(quad(100.), 0.99)],
        &[true],
        vec![ctc(&[0, 1, 1, 0, 2, 0], 2, 0.99)],
        &["A", "B"],
    )
    .result;
    let recovered = recognize_script(
        &[detection(quad(40.), 0.99)],
        &[false],
        vec![ctc(&[1, 0, 2, 2, 0], 2, 0.99)],
        &["C", "D"],
    )
    .result;
    let before = primary.lines[0].graphemes.clone();
    let before_bands = bands(&primary, 0);
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let merged = merge_quality_recovery(&primary, recovered, &rgb).unwrap();
    assert_eq!(merged.text, "CD\nAB");
    assert_eq!(merged.lines[1].graphemes, before);
    assert_eq!(bands(&merged, 1), before_bands);
    assert_eq!(primary.lines[0].graphemes, before);
    assert_eq!(bands(&primary, 0), before_bands);
    assert!(merged
        .words
        .iter()
        .filter(|word| word.text == "AB")
        .all(|word| word.line == 1));
}

#[test]
fn rejected_quality_recovery_does_not_mutate_primary_sampling_payload() {
    let primary = single(&["A", "B"], &[0, 1, 0, 2, 0]);
    let recovered = single(&["C", "D"], &[1, 0, 2, 0]);
    let before = primary.lines[0].graphemes.clone();
    let before_bands = bands(&primary, 0);
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    assert!(merge_quality_recovery(&primary, recovered, &rgb).is_none());
    assert_eq!(primary.lines[0].graphemes, before);
    assert_eq!(bands(&primary, 0), before_bands);
}

#[test]
fn raw_capture_budget_or_empty_entry_failure_preserves_original_tokens() {
    let token = |text: String| Token {
        text,
        start: 1,
        end: 2,
        confidence: 0.95,
    };
    let empty = vec![token(String::new())];
    assert!(capture_emissions(&empty, 4).is_none());
    assert!(empty[0].text.is_empty());
    let long = vec![token("x".repeat(MAX_LINE_BYTES + 1))];
    assert!(capture_emissions(&long, 4).is_none());
    assert_eq!(
        (long[0].text.len(), long[0].start, long[0].end),
        (MAX_LINE_BYTES + 1, 1, 2)
    );
    let many = vec![token("x".into()); MAX_EMISSION_RUNS + 1];
    assert!(capture_emissions(&many, 4).is_none());
    assert_eq!(many.len(), MAX_EMISSION_RUNS + 1);
    let ordinary = vec![token("x".into())];
    assert!(capture_emissions(&ordinary, MAX_SAMPLING_STEPS + 1).is_none());
    assert!(capture_emissions(&ordinary, 0).is_none());
    assert_eq!(
        (ordinary[0].start, ordinary[0].end, ordinary[0].confidence),
        (1, 2, 0.95)
    );
}

#[test]
fn decode_over_new_steps_budget_keeps_ctc_text_confidence_and_voronoi() {
    let mut sequence = vec![0; MAX_SAMPLING_STEPS + 1];
    sequence[1] = 1;
    let decoded = decode_ctc(ctc(&sequence, 1, 0.99), &["A".into()]).unwrap();
    assert!(decoded.emissions.is_none());
    assert_eq!(decoded.tokens.len(), 1);
    assert_eq!(decoded.tokens[0].text, "A");
    assert_eq!(
        (decoded.tokens[0].start, decoded.tokens[0].end),
        (0, sequence.len())
    );
    assert!((decoded.confidence - 0.99).abs() < 1e-6);
}

#[test]
fn total_evidence_budget_accepts_exact_limit_and_rejects_without_spending_counts() {
    let result = single(&["A", "B"], &[0, 1, 0, 2, 0]);
    let sampling = result.lines[0].graphemes.as_ref().unwrap();
    let mut budget = GraphemeEvidenceBudget {
        graphemes: MAX_TOTAL_GRAPHEMES - sampling.grapheme_count(),
        runs: MAX_TOTAL_RUNS - sampling.run_count(),
    };
    assert!(budget.can_add_runs(sampling.run_count()));
    assert!(budget.add(sampling));
    assert_eq!(
        (budget.graphemes, budget.runs),
        (MAX_TOTAL_GRAPHEMES, MAX_TOTAL_RUNS)
    );
    assert!(!budget.can_add_runs(1));
    assert!(!budget.add(sampling));
    assert_eq!(
        (budget.graphemes, budget.runs),
        (MAX_TOTAL_GRAPHEMES, MAX_TOTAL_RUNS)
    );
    // 分别越过独立预算，避免两个量同时超限掩盖其中一个守卫缺失。
    let mut runs_only = GraphemeEvidenceBudget {
        graphemes: 0,
        runs: MAX_TOTAL_RUNS - 1,
    };
    assert!(!runs_only.can_add_runs(sampling.run_count()));
    assert!(!runs_only.add(sampling));
    assert_eq!(
        (runs_only.graphemes, runs_only.runs),
        (0, MAX_TOTAL_RUNS - 1)
    );
    let mut groups_only = GraphemeEvidenceBudget {
        graphemes: MAX_TOTAL_GRAPHEMES - 1,
        runs: 0,
    };
    assert!(groups_only.can_add_runs(sampling.run_count()));
    assert!(!groups_only.add(sampling));
    assert_eq!(
        (groups_only.graphemes, groups_only.runs),
        (MAX_TOTAL_GRAPHEMES - 1, 0)
    );
}

#[test]
fn final_source_budget_drops_only_new_evidence_preserving_original_ocr_objects() {
    let mut result = single(&["A", "B"], &[0, 1, 0, 2, 0]);
    let rect = result.words[0].rect;
    let geometry = result.words[0].geometry;
    let line_geometry = result.lines[0].geometry;
    result.text = "x".repeat(MAX_SOURCE_BYTES + 1);
    let before = result.text.clone();
    bound_grapheme_evidence(&mut result);
    assert_eq!(result.text, before);
    assert_eq!(result.lines[0].text, "AB");
    assert_eq!(result.words[0].text, "AB");
    assert_eq!(result.words[0].rect, rect);
    assert_eq!(result.words[0].geometry, geometry);
    assert_eq!(result.lines[0].geometry, line_geometry);
    assert!(result.lines[0].graphemes.is_none());
}
