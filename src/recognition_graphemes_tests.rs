use super::*;
use crate::annotation::Point;

fn rect(x: u32, width: u32) -> Rect {
    Rect {
        x,
        y: 10,
        width,
        height: 20,
    }
}
fn geometry() -> TextGeometry {
    TextGeometry {
        quad: [
            Point { x: 10., y: 10. },
            Point { x: 110., y: 10. },
            Point { x: 110., y: 30. },
            Point { x: 10., y: 30. },
        ],
    }
}
fn fixture(text: &str) -> (RecognizedLine, Vec<RecognizedWord>) {
    let frame = geometry();
    (
        RecognizedLine {
            text: text.into(),
            rect: Some(rect(10, 100)),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(frame),
            graphemes: None,
        },
        vec![RecognizedWord {
            text: text.into(),
            rect: rect(10, 100),
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(frame),
        }],
    )
}
fn run(bytes: Range<usize>, steps: Range<usize>) -> SamplingRun {
    SamplingRun {
        source_range: bytes,
        steps,
    }
}
fn refs(words: &[RecognizedWord]) -> Vec<&RecognizedWord> {
    words.iter().collect()
}
fn sampling(
    line: &RecognizedLine,
    words: &[RecognizedWord],
    runs: &[SamplingRun],
) -> Option<GraphemeSampling> {
    GraphemeSampling::from_ctc(0, line, &refs(words), runs, 20, 100, 100)
}
fn bands(
    sampling: &GraphemeSampling,
    line: &RecognizedLine,
    words: &[RecognizedWord],
) -> Vec<GraphemeBand> {
    sampling.validated_bands(0, line, &refs(words)).unwrap()
}
fn ascii_runs() -> Vec<SamplingRun> {
    vec![run(0..1, 2..3), run(1..2, 8..12)]
}

#[test]
fn actual_runs_keep_nonuniform_widths_and_blank_gaps() {
    let (line, words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    let actual = bands(&proof, &line, &words);
    assert_eq!(
        actual,
        vec![
            GraphemeBand {
                source_range: 0..1,
                start: 0.1,
                end: 0.15
            },
            GraphemeBand {
                source_range: 1..2,
                start: 0.4,
                end: 0.6
            }
        ]
    );
    assert_eq!(proof.runs, ascii_runs());
    assert_eq!((proof.grapheme_count(), proof.run_count()), (2, 2));
}

#[test]
fn leading_and_trailing_blank_do_not_expand_sampling_to_crop_edges() {
    let (line, words) = fixture("A");
    let proof = sampling(&line, &words, &[run(0..1, 7..9)]).unwrap();
    let actual = bands(&proof, &line, &words);
    assert_eq!((actual[0].start, actual[0].end), (0.35, 0.45));
}

#[test]
fn content_tensor_ratio_maps_actual_steps_without_clamping() {
    let (line, words) = fixture("A");
    let proof =
        GraphemeSampling::from_ctc(0, &line, &refs(&words), &[run(0..1, 8..10)], 20, 80, 100)
            .unwrap();
    let actual = bands(&proof, &line, &words);
    assert_eq!((actual[0].start, actual[0].end), (0.5, 0.625));
    assert!(
        GraphemeSampling::from_ctc(0, &line, &refs(&words), &[run(0..1, 16..17)], 20, 80, 100)
            .is_none()
    );
}

#[test]
fn full_last_step_is_exactly_one_after_integer_ratio() {
    let (line, words) = fixture("A");
    for steps in [3, 7, 49, MAX_SAMPLING_STEPS] {
        let proof = GraphemeSampling::from_ctc(
            0,
            &line,
            &refs(&words),
            &[run(0..1, steps - 1..steps)],
            steps,
            100,
            100,
        )
        .unwrap();
        assert_eq!(bands(&proof, &line, &words)[0].end, 1.);
    }
}

#[test]
fn cjk_ranges_are_original_utf8_bytes() {
    let (line, words) = fixture("中日");
    let proof = sampling(&line, &words, &[run(0..3, 2..5), run(3..6, 11..14)]).unwrap();
    assert_eq!(
        bands(&proof, &line, &words)
            .iter()
            .map(|band| band.source_range.clone())
            .collect::<Vec<_>>(),
        vec![0..3, 3..6]
    );
}

#[test]
fn combining_across_tokens_keeps_one_grapheme_and_original_runs() {
    let (line, words) = fixture("e\u{301}B");
    let runs = vec![run(0..1, 2..3), run(1..3, 5..7), run(3..4, 12..13)];
    let proof = sampling(&line, &words, &runs).unwrap();
    assert_eq!(
        bands(&proof, &line, &words),
        vec![
            GraphemeBand {
                source_range: 0..3,
                start: 0.1,
                end: 0.35
            },
            GraphemeBand {
                source_range: 3..4,
                start: 0.6,
                end: 0.65
            }
        ]
    );
    assert_eq!(proof.runs, runs);
}

#[test]
fn zwj_cross_token_sequence_is_one_complete_grapheme() {
    let (line, words) = fixture("👩‍🔬B");
    let runs = [
        run(0..4, 1..2),
        run(4..7, 4..5),
        run(7..11, 7..9),
        run(11..12, 13..14),
    ];
    let proof = sampling(&line, &words, &runs).unwrap();
    let actual = bands(&proof, &line, &words);
    assert_eq!(actual.len(), 2);
    assert_eq!(
        actual[0],
        GraphemeBand {
            source_range: 0..11,
            start: 0.05,
            end: 0.45
        }
    );
    assert_eq!(proof.run_count(), 4);
}

#[test]
fn one_multiscalar_token_with_exactly_one_grapheme_is_accepted() {
    for content in ["e\u{301}", "👩‍🔬", "🇨🇳"] {
        let (line, words) = fixture(content);
        let proof = sampling(&line, &words, &[run(0..content.len(), 3..8)]).unwrap();
        assert_eq!(proof.grapheme_count(), 1);
    }
}

#[test]
fn token_spanning_two_complete_graphemes_drops_entire_line_evidence() {
    for content in ["AB", "中日", " A", "e\u{301}B"] {
        let (line, words) = fixture(content);
        assert!(sampling(&line, &words, &[run(0..content.len(), 3..8)]).is_none());
        assert_eq!(line.text, content);
        assert_eq!(words[0].text, content);
    }
}

#[test]
fn ordinary_spaces_have_bound_source_and_emit_bands() {
    let (line, words) = fixture("A B");
    let proof = sampling(
        &line,
        &words,
        &[run(0..1, 1..2), run(1..2, 4..5), run(2..3, 10..11)],
    )
    .unwrap();
    let actual = bands(&proof, &line, &words);
    assert_eq!(
        actual
            .iter()
            .map(|band| line.text.get(band.source_range.clone()).unwrap())
            .collect::<Vec<_>>(),
        vec!["A", " ", "B"]
    );
}

#[test]
fn nonbreaking_space_remains_an_original_complete_grapheme() {
    let (line, words) = fixture("A\u{a0}B");
    let proof = sampling(
        &line,
        &words,
        &[run(0..1, 1..2), run(1..3, 4..5), run(3..4, 10..11)],
    )
    .unwrap();
    assert_eq!(bands(&proof, &line, &words)[1].source_range, 1..3);
}

#[test]
fn controls_and_crlf_never_form_sampling_for_one_physical_line() {
    for content in ["A\rB", "A\nB", "A\r\nB", "A\tB", "A\u{0}B"] {
        let (line, words) = fixture(content);
        let runs = content
            .char_indices()
            .enumerate()
            .map(|(i, (start, ch))| run(start..start + ch.len_utf8(), i * 2..i * 2 + 1))
            .collect::<Vec<_>>();
        assert!(sampling(&line, &words, &runs).is_none());
        assert_eq!(line.text, content);
    }
}

#[test]
fn complete_raw_byte_partition_is_required() {
    let (line, words) = fixture("AB");
    for runs in [
        vec![run(1..2, 4..5)],
        vec![run(0..1, 2..3)],
        vec![run(0..1, 2..3), run(0..1, 5..6)],
        vec![run(0..0, 2..3), run(0..2, 4..5)],
    ] {
        assert!(sampling(&line, &words, &runs).is_none());
    }
}

#[test]
fn raw_token_ranges_cannot_cut_utf8_scalars() {
    let (line, words) = fixture("中");
    assert!(sampling(&line, &words, &[run(0..1, 1..2), run(1..3, 4..5)]).is_none());
}

#[test]
fn unordered_overlapping_zero_width_and_past_end_steps_are_rejected() {
    let (line, words) = fixture("AB");
    for runs in [
        vec![run(0..1, 3..8), run(1..2, 7..9)],
        vec![run(0..1, 5..7), run(1..2, 2..4)],
        vec![run(0..1, 2..2), run(1..2, 4..5)],
        vec![run(0..1, 1..2), run(1..2, 19..21)],
    ] {
        assert!(sampling(&line, &words, &runs).is_none());
    }
}

#[test]
fn same_length_rewritten_line_does_not_reuse_old_metadata() {
    let (mut line, mut words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    line.text = "CD".into();
    words[0].text = "CD".into();
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}

#[test]
fn word_text_and_word_layout_change_invalidate_old_source() {
    let (line, mut words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    words[0].text = "A".into();
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
    words[0].text = "AB".into();
    words[0].rect.width -= 1;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}

#[test]
fn line_rect_direction_and_full_crop_frame_change_invalidate_source() {
    let (mut line, words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    line.rect = Some(rect(11, 99));
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
    line.rect = Some(rect(10, 100));
    line.horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
    line.horizontal_direction = HorizontalDirection::LeftToRight;
    line.geometry.as_mut().unwrap().quad[1].x -= 1.;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}

#[test]
fn word_direction_and_current_line_membership_must_match() {
    let (line, mut words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    words[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
    words[0].horizontal_direction = HorizontalDirection::LeftToRight;
    words[0].line = 1;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}

#[test]
fn complete_line_reindex_keeps_line_local_sampling() {
    let (line, mut words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    words[0].line = 7;
    assert!(proof.validated_bands(7, &line, &refs(&words)).is_some());
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}

#[test]
fn ink_height_geometry_is_not_part_of_immutable_word_snapshot() {
    let (line, mut words) = fixture("AB");
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    let mut tight = geometry();
    tight.quad[0].y = 15.;
    tight.quad[1].y = 15.;
    tight.quad[2].y = 25.;
    tight.quad[3].y = 25.;
    words[0].geometry = Some(tight);
    assert_eq!(bands(&proof, &line, &words)[0].start, 0.1);
    words[0].geometry = None;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_some());
}

#[test]
fn cross_word_combining_cluster_keeps_complete_byte_band() {
    let (mut line, mut words) = fixture("e\u{301}");
    words[0].text = "e".into();
    words[0].rect = rect(10, 40);
    words.push(RecognizedWord {
        text: "\u{301}".into(),
        rect: rect(50, 60),
        line: 0,
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
    });
    line.rect = Some(rect(10, 100));
    let proof = sampling(&line, &words, &[run(0..1, 1..3), run(1..3, 6..8)]).unwrap();
    assert_eq!(bands(&proof, &line, &words)[0].source_range, 0..3);
}

#[test]
fn cropped_word_union_does_not_clip_full_crop_geometry() {
    let (mut line, mut words) = fixture("A");
    words[0].rect = rect(30, 40);
    words[0].geometry = None;
    line.rect = Some(words[0].rect);
    let proof = sampling(&line, &words, &[run(0..1, 5..10)]).unwrap();
    assert_eq!(proof.source_geometry(), geometry());
}

#[test]
fn original_reading_frame_preserves_180_and_both_vertical_directions() {
    let (mut line, mut words) = fixture("AB");
    for indices in [[0, 1, 2, 3], [2, 3, 0, 1], [1, 2, 3, 0], [3, 0, 1, 2]] {
        let original = geometry().quad;
        let frame = TextGeometry {
            quad: indices.map(|index| original[index]),
        };
        let p = Projection::new(frame).unwrap();
        let direction = if p.point(1., 0.5).x < p.point(0., 0.5).x {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        };
        line.geometry = Some(frame);
        line.horizontal_direction = direction;
        words[0].geometry = Some(frame);
        words[0].horizontal_direction = direction;
        let proof = sampling(&line, &words, &ascii_runs()).unwrap();
        assert_eq!(proof.source_geometry(), frame);
        assert_eq!(bands(&proof, &line, &words)[0].source_range, 0..1);
    }
}

#[test]
fn tilted_perspective_frame_is_retained_without_axis_aligned_replacement() {
    let (mut line, mut words) = fixture("AB");
    let frame = TextGeometry {
        quad: [
            Point { x: 12., y: 12. },
            Point { x: 106., y: 14. },
            Point { x: 108., y: 29. },
            Point { x: 10., y: 28. },
        ],
    };
    line.geometry = Some(frame);
    words[0].geometry = Some(frame);
    let proof = sampling(&line, &words, &ascii_runs()).unwrap();
    assert_eq!(proof.source_geometry(), frame);
}

#[test]
fn missing_invalid_or_direction_conflicting_crop_has_no_new_evidence() {
    let (mut line, words) = fixture("A");
    line.geometry = None;
    assert!(sampling(&line, &words, &[run(0..1, 1..2)]).is_none());
    line.geometry = Some(TextGeometry {
        quad: [Point { x: 10., y: 10. }; 4],
    });
    assert!(sampling(&line, &words, &[run(0..1, 1..2)]).is_none());
    line.geometry = Some(geometry());
    line.horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(sampling(&line, &words, &[run(0..1, 1..2)]).is_none());
}

#[test]
fn empty_unbound_missing_tail_and_false_word_union_are_rejected() {
    let (line, mut words) = fixture("AB");
    assert!(sampling(&line, &[], &ascii_runs()).is_none());
    words[0].text = "B".into();
    assert!(sampling(&line, &words, &ascii_runs()).is_none());
    words[0].text = "A".into();
    assert!(sampling(&line, &words, &ascii_runs()).is_none());
    words[0].text = "AB".into();
    words[0].rect.width = 99;
    assert!(sampling(&line, &words, &ascii_runs()).is_none());
}

#[test]
fn repeated_words_bind_sequentially_without_skipping_unowned_body() {
    let (mut line, mut words) = fixture("A A");
    words[0].text = "A".into();
    words[0].rect = rect(10, 40);
    words.push(RecognizedWord {
        text: "A".into(),
        rect: rect(60, 50),
        line: 0,
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
    });
    let proof = sampling(
        &line,
        &words,
        &[run(0..1, 1..2), run(1..2, 4..5), run(2..3, 9..10)],
    )
    .unwrap();
    assert_eq!(proof.source_words[0].source_range, 0..1);
    assert_eq!(proof.source_words[1].source_range, 2..3);
    line.text = "AXA".into();
    assert!(sampling(
        &line,
        &words,
        &[run(0..1, 1..2), run(1..2, 4..5), run(2..3, 9..10)]
    )
    .is_none());
}

#[test]
fn dimension_budget_and_zero_content_fail_without_altering_source() {
    let (line, words) = fixture("A");
    for (steps, content, tensor) in [
        (0, 100, 100),
        (20, 0, 100),
        (20, 101, 100),
        (MAX_SAMPLING_STEPS + 1, 100, 100),
        (20, 100, MAX_SAMPLING_STEPS + 1),
    ] {
        assert!(GraphemeSampling::from_ctc(
            0,
            &line,
            &refs(&words),
            &[run(0..1, 1..2)],
            steps,
            content,
            tensor
        )
        .is_none());
    }
    assert_eq!(line.text, "A");
}

#[test]
fn line_byte_budget_is_enforced_before_sampling_allocation() {
    let content = format!("a{}", "\u{301}".repeat((MAX_LINE_BYTES - 1) / 2));
    let (line, words) = fixture(&content);
    assert!(sampling(&line, &words, &[run(0..content.len(), 1..2)]).is_some());
    let content = format!("é{}", "\u{301}".repeat((MAX_LINE_BYTES - 2) / 2));
    let (line, words) = fixture(&content);
    assert_eq!(content.len(), MAX_LINE_BYTES);
    assert!(sampling(&line, &words, &[run(0..content.len(), 1..2)]).is_some());
    let content = format!("a{}", "\u{301}".repeat(MAX_LINE_BYTES / 2));
    let (line, words) = fixture(&content);
    assert!(sampling(&line, &words, &[run(0..content.len(), 1..2)]).is_none());
}

#[test]
fn grapheme_limit_accepts_boundary_and_rejects_next_complete_cluster() {
    for count in [MAX_LINE_GRAPHEMES, MAX_LINE_GRAPHEMES + 1] {
        let content = "A".repeat(count);
        let (line, words) = fixture(&content);
        let runs = (0..count)
            .map(|i| run(i..i + 1, i * 2..i * 2 + 1))
            .collect::<Vec<_>>();
        let actual = GraphemeSampling::from_ctc(0, &line, &refs(&words), &runs, 10_000, 100, 100);
        assert_eq!(actual.is_some(), count == MAX_LINE_GRAPHEMES);
    }
}

#[test]
fn actual_run_limit_accepts_boundary_and_rejects_next_run() {
    for count in [MAX_EMISSION_RUNS, MAX_EMISSION_RUNS + 1] {
        let content = format!(
            "{}{}",
            "A".repeat(MAX_LINE_GRAPHEMES),
            "\u{301}".repeat(count - MAX_LINE_GRAPHEMES)
        );
        let (line, words) = fixture(&content);
        let mut runs = (0..MAX_LINE_GRAPHEMES)
            .map(|i| run(i..i + 1, i..i + 1))
            .collect::<Vec<_>>();
        runs.extend((MAX_LINE_GRAPHEMES..count).map(|i| {
            run(
                MAX_LINE_GRAPHEMES + (i - MAX_LINE_GRAPHEMES) * 2
                    ..MAX_LINE_GRAPHEMES + (i - MAX_LINE_GRAPHEMES + 1) * 2,
                i..i + 1,
            )
        }));
        assert!(content.len() <= MAX_LINE_BYTES);
        assert_eq!(runs.len(), count);
        let proof = GraphemeSampling::from_ctc(
            0,
            &line,
            &refs(&words),
            &runs,
            MAX_SAMPLING_STEPS,
            100,
            100,
        );
        assert_eq!(proof.is_some(), count == MAX_EMISSION_RUNS);
    }
}

#[test]
fn corrupted_private_run_or_group_cannot_be_consumed() {
    let (line, words) = fixture("AB");
    let mut proof = sampling(&line, &words, &ascii_runs()).unwrap();
    proof.runs[1].steps.end = 21;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
    let mut proof = sampling(&line, &words, &ascii_runs()).unwrap();
    proof.groups[0].source_range.end = 2;
    assert!(proof.validated_bands(0, &line, &refs(&words)).is_none());
}
