//! 合成结果验证重试门、原图几何、调用预算和原结果保护；不代表模型准确率。
use super::*;

#[derive(Default)]
struct Scripted {
    outputs: VecDeque<Result<Tensor>>,
    inputs: Vec<Tensor>,
}
impl Scripted {
    fn new(outputs: Vec<Result<Tensor>>) -> Self {
        Self {
            outputs: outputs.into(),
            inputs: Vec::new(),
        }
    }
    fn calls(&self) -> usize {
        self.inputs.len()
    }
}
impl ModelRunner for Scripted {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        // 长行分类按窗口批量输入；脚本中的单个方向结果代表整行，逐窗重复。
        let windows = match input.shape.as_slice() {
            [windows, 3, 80, 160] => *windows,
            _ => 1,
        };
        self.inputs.push(input);
        self.outputs
            .pop_front()
            .unwrap_or_else(|| Err("未预期额外推理".into()))
            .map(|output| match output.shape.as_slice() {
                [1, 2] => Tensor {
                    shape: vec![windows, 2],
                    values: output.values.repeat(windows),
                },
                _ => output,
            })
    }
}

fn image(width: u32, height: u32, uniform: bool) -> Image {
    let mut bgra = vec![255; width as usize * height as usize * 4];
    if !uniform {
        let center = (height as usize / 2 * width as usize + width as usize / 2) * 4;
        bgra[center..center + 3].fill(0);
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
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
fn ctc(sequence: &[usize], confidence: f32) -> Tensor {
    let mut values = vec![0.; sequence.len() * 4];
    for (step, &index) in sequence.iter().enumerate() {
        values[step * 4 + index] = if index == 0 { 1. } else { confidence };
    }
    Tensor {
        shape: vec![1, sequence.len(), 4],
        values,
    }
}
fn map(bands: &[(usize, usize)], confidence: f32) -> Tensor {
    let mut values = vec![0.; 64 * 64];
    for &(top, bottom) in bands {
        for y in top..bottom {
            for x in 10..54 {
                values[y * 64 + x] = confidence;
            }
        }
    }
    Tensor {
        shape: vec![1, 1, 64, 64],
        values,
    }
}
fn dictionary() -> Vec<String> {
    vec!["中".into(), "文".into()]
}
fn detected(rect: Rect, confidence: f32) -> DetectedLine {
    let x = rect.x as f32;
    let y = rect.y as f32;
    let right = (rect.x + rect.width) as f32;
    let bottom = (rect.y + rect.height) as f32;
    DetectedLine {
        quad: [[x, y], [right, y], [right, bottom], [x, bottom]],
        rect,
        confidence,
    }
}
fn empty_candidate() -> RecognitionCandidate {
    RecognitionCandidate {
        result: RecognizedText {
            text: String::new(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        },
        confidence: 0.,
        accepted_detected: Vec::new(),
        retryable_missed: Vec::new(),
    }
}
fn candidate(rows: &[(&str, Rect, HorizontalDirection)], confidence: f32) -> RecognitionCandidate {
    let mut result = empty_candidate();
    for (index, &(text, rect, direction)) in rows.iter().enumerate() {
        result.result.lines.push(RecognizedLine {
            graphemes: None,
            text: text.into(),
            rect: Some(rect),
            horizontal_direction: direction,
            geometry: None,
        });
        result.result.words.push(RecognizedWord {
            text: text.into(),
            rect,
            line: index,
            horizontal_direction: direction,
            geometry: None,
        });
        result.result.regions.push(rect);
        result.accepted_detected.push(index);
    }
    result.result.text = rows.iter().map(|row| row.0).collect::<Vec<_>>().join("\n");
    result.confidence = confidence;
    result
}
fn rect(y: u32) -> Rect {
    Rect {
        x: 20,
        y,
        width: 120,
        height: 15,
    }
}
fn assert_contract(result: &RecognizedText, image: &Image) {
    assert_eq!(result.words.len(), result.regions.len());
    assert_eq!(
        result.text,
        result
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    for (word, region) in result.words.iter().zip(&result.regions) {
        assert_eq!(word.rect, *region);
        assert!(word.rect.valid(image.width, image.height));
        assert_eq!(
            word.horizontal_direction,
            result.lines[word.line].horizontal_direction
        );
    }
    for line in &result.lines {
        assert!(line.rect.unwrap().valid(image.width, image.height));
    }
}

#[test]
fn quality_retry_plan_keeps_normal_multiline_and_valid_single_symbol() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let lines = [
        detected(rect(10), 0.99),
        detected(rect(40), 0.99),
        detected(rect(65), 0.99),
    ];
    let mut primary = candidate(
        &[
            ("中", rect(10), HorizontalDirection::LeftToRight),
            ("文", rect(40), HorizontalDirection::RightToLeft),
        ],
        0.95,
    );
    primary.retryable_missed.push(2);
    assert!(quality_retry_plan(&image, &rgb, &primary, &lines).is_none());
    let primary = candidate(&[("!", rect(10), HorizontalDirection::LeftToRight)], 0.5);
    assert!(quality_retry_plan(&image, &rgb, &primary, &lines).is_none());
}

#[test]
fn quality_retry_sparse_plan_requires_real_high_confidence_missed_detector_box() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let mut primary = candidate(&[("中", rect(10), HorizontalDirection::LeftToRight)], 0.95);
    primary.retryable_missed.push(1);
    let strong = [detected(rect(10), 0.99), detected(rect(40), 0.65)];
    let plan = quality_retry_plan(&image, &rgb, &primary, &strong).unwrap();
    assert_eq!(plan.missing, Some(vec![rect(40)]));
    let weak = [detected(rect(10), 0.99), detected(rect(40), 0.649)];
    assert!(quality_retry_plan(&image, &rgb, &primary, &weak).is_none());
    primary.retryable_missed = vec![0];
    assert!(quality_retry_plan(&image, &rgb, &primary, &strong).is_none());
}

#[test]
fn quality_retry_uniform_gate_checks_every_composited_pixel_and_keeps_alpha_source() {
    let uniform = image(160, 80, true);
    let rgb = RgbImage::from_bgra(&uniform).unwrap();
    assert!(quality_retry_plan(&uniform, &rgb, &empty_candidate(), &[]).is_none());
    let mut edge = uniform;
    edge.bgra[0] = 254;
    let rgb = RgbImage::from_bgra(&edge).unwrap();
    assert!(quality_retry_plan(&edge, &rgb, &empty_candidate(), &[]).is_some());
    let mut transparent = image(160, 80, false);
    for pixel in transparent.bgra.chunks_exact_mut(4) {
        pixel[3] = 0;
    }
    let saved = transparent.bgra.clone();
    let rgb = RgbImage::from_bgra(&transparent).unwrap();
    assert!(quality_retry_plan(&transparent, &rgb, &empty_candidate(), &[]).is_none());
    assert_eq!(transparent.bgra, saved);
}

#[test]
fn quality_retry_input_budget_uses_actual_rounded_detector_pixels() {
    let mut image = image(1536, 1536, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let plan = quality_retry_plan(&image, &rgb, &empty_candidate(), &[]).unwrap();
    assert_eq!(detector_input_pixels(&rgb, plan.config), Some(2_359_296));
    image = self::image(1568, 1568, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    assert!(quality_retry_plan(&image, &rgb, &empty_candidate(), &[]).is_none());
    let small = self::image(1000, 80, false);
    let rgb = RgbImage::from_bgra(&small).unwrap();
    let plan = quality_retry_plan(&small, &rgb, &empty_candidate(), &[]).unwrap();
    assert_eq!(plan.config.max_side, 1536);
    assert_eq!(plan.config.padding, 24);
    assert_eq!(plan.config.unclip_ratio, 1.6);
}

#[test]
fn quality_retry_targets_only_recover_missing_regions_without_overlapping_old_line() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let mut primary = candidate(&[("中", rect(10), HorizontalDirection::RightToLeft)], 0.95);
    primary.retryable_missed.push(1);
    let original = [detected(rect(10), 0.99), detected(rect(40), 0.99)];
    let plan = quality_retry_plan(&image, &rgb, &primary, &original).unwrap();
    let retried = [
        detected(rect(10), 0.99),
        detected(rect(40), 0.99),
        detected(rect(65), 0.99),
    ];
    let selected = quality_retry_targets(&rgb, &primary, &plan, retried.into()).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].rect, rect(40));
}

#[test]
fn quality_retry_workload_rejects_more_than_eight_lines_or_width_sum_over_8192() {
    let small = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&small).unwrap();
    let eight = (0..8).map(|_| detected(rect(10), 0.99)).collect::<Vec<_>>();
    assert!(quality_retry_workload(&rgb, &eight));
    let nine = (0..9).map(|_| detected(rect(10), 0.99)).collect::<Vec<_>>();
    assert!(!quality_retry_workload(&rgb, &nine));
    let image = self::image(2048, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let line = Rect {
        x: 0,
        y: 0,
        width: 2048,
        height: 24,
    };
    assert!(quality_retry_workload(
        &rgb,
        &[detected(line, 0.99), detected(line, 0.99)]
    ));
    assert!(!quality_retry_workload(
        &rgb,
        &[
            detected(line, 0.99),
            detected(line, 0.99),
            detected(line, 0.99)
        ]
    ));
}

#[test]
fn quality_retry_candidate_rejects_low_confidence_and_broken_region_direction_contract() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let good = candidate(&[("文", rect(40), HorizontalDirection::RightToLeft)], 0.9);
    assert!(valid_quality_candidate(&good, &rgb));
    let low = candidate(
        &[("文文文文", rect(40), HorizontalDirection::RightToLeft)],
        0.899,
    );
    assert!(!valid_quality_candidate(&low, &rgb));
    let mut broken = candidate(&[("文", rect(40), HorizontalDirection::RightToLeft)], 0.99);
    broken.result.words[0].horizontal_direction = HorizontalDirection::LeftToRight;
    assert!(!valid_quality_candidate(&broken, &rgb));
    broken.result.words[0].horizontal_direction = HorizontalDirection::RightToLeft;
    broken.result.regions[0].x += 1;
    assert!(!valid_quality_candidate(&broken, &rgb));
}

#[test]
fn quality_retry_merge_preserves_old_unicode_spaces_word_bounds_and_direction() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let primary = candidate(
        &[("A 中e\u{301}👩‍💻", rect(40), HorizontalDirection::RightToLeft)],
        0.5,
    );
    let recovered = candidate(&[("文", rect(10), HorizontalDirection::LeftToRight)], 0.99);
    let merged = merge_quality_recovery(&primary.result, recovered.result, &rgb).unwrap();
    assert_eq!(merged.text, "文\nA 中e\u{301}👩‍💻");
    assert_eq!(merged.lines[1].text, "A 中e\u{301}👩‍💻");
    assert_eq!(merged.lines[1].rect, Some(rect(40)));
    assert_eq!(merged.words[1].text, "A 中e\u{301}👩‍💻");
    assert_eq!(merged.words[1].rect, rect(40));
    assert_eq!(
        merged.words[1].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_eq!(merged.words[1].line, 1);
    assert_contract(&merged, &image);
}

#[test]
fn quality_retry_merge_rejects_duplicate_or_overlapping_recovery_without_rewriting_old() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let primary = candidate(&[("中", rect(40), HorizontalDirection::RightToLeft)], 0.5);
    let recovered = candidate(
        &[("文字更多", rect(40), HorizontalDirection::LeftToRight)],
        0.99,
    );
    assert!(merge_quality_recovery(&primary.result, recovered.result, &rgb).is_none());
    assert_eq!(primary.result.text, "中");
    assert_eq!(primary.result.words[0].rect, rect(40));
    assert_eq!(
        primary.result.words[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
}

#[test]
fn quality_retry_merge_keeps_same_visual_row_left_to_right_with_y_jitter() {
    let image = image(400, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let original = Rect {
        x: 20,
        y: 20,
        width: 120,
        height: 15,
    };
    let right_word = Rect {
        x: 100,
        y: 20,
        width: 40,
        height: 15,
    };
    let left_word = Rect {
        x: 20,
        y: 20,
        width: 80,
        height: 15,
    };
    for jitter in [5, 9, 10] {
        let mut primary = candidate(
            &[("A 中文", original, HorizontalDirection::RightToLeft)],
            0.5,
        );
        primary.result.words = vec![
            RecognizedWord {
                text: "A".into(),
                rect: right_word,
                line: 0,
                horizontal_direction: HorizontalDirection::RightToLeft,
                geometry: None,
            },
            RecognizedWord {
                text: "中文".into(),
                rect: left_word,
                line: 0,
                horizontal_direction: HorizontalDirection::RightToLeft,
                geometry: None,
            },
        ];
        primary.result.regions = vec![right_word, left_word];
        let new_rect = Rect {
            x: 180,
            y: 20 - jitter,
            width: 120,
            height: 15,
        };
        let recovered = candidate(
            &[("补行", new_rect, HorizontalDirection::LeftToRight)],
            0.99,
        );
        let merged = merge_quality_recovery(&primary.result, recovered.result, &rgb).unwrap();
        let original_index = usize::from(jitter == 10);
        assert_eq!(
            merged.text,
            if jitter < 10 {
                "A 中文\n补行"
            } else {
                "补行\nA 中文"
            }
        );
        assert_eq!(merged.lines[original_index].text, "A 中文");
        assert_eq!(merged.lines[original_index].rect, Some(original));
        assert_eq!(
            merged.lines[original_index].horizontal_direction,
            HorizontalDirection::RightToLeft
        );
        let old_words: Vec<_> = merged
            .words
            .iter()
            .filter(|word| word.line == original_index)
            .collect();
        assert_eq!(old_words.len(), 2);
        assert_eq!(
            (old_words[0].text.as_str(), old_words[0].rect),
            ("A", right_word)
        );
        assert_eq!(
            (old_words[1].text.as_str(), old_words[1].rect),
            ("中文", left_word)
        );
        assert!(old_words
            .iter()
            .all(|word| word.horizontal_direction == HorizontalDirection::RightToLeft));
        let expected_regions = if jitter < 10 {
            vec![right_word, left_word, new_rect]
        } else {
            vec![new_rect, right_word, left_word]
        };
        let expected_indices = if jitter < 10 {
            vec![0, 0, 1]
        } else {
            vec![0, 1, 1]
        };
        assert_eq!(merged.regions, expected_regions);
        assert_eq!(
            merged
                .words
                .iter()
                .map(|word| word.line)
                .collect::<Vec<_>>(),
            expected_indices
        );
        assert_eq!(primary.result.text, "A 中文");
        assert_eq!(primary.result.regions, vec![right_word, left_word]);
        assert!(primary.result.words.iter().all(|word| word.line == 0));
        assert_contract(&merged, &image);
    }
}

#[test]
fn quality_retry_empty_recovery_runs_one_more_detector_and_shared_cls_ctc_path() {
    let image = image(160, 80, false);
    let saved = image.bgra.clone();
    let mut det = Scripted::new(vec![Ok(map(&[], 0.)), Ok(map(&[(12, 24)], 0.95))]);
    let mut cls = Scripted::new(vec![Ok(orientation(true))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0, 2, 0], 0.99))]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, "中文");
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 1, 1));
    assert_eq!(
        result.lines[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_contract(&result, &image);
    assert_eq!(image.bgra, saved);
}

#[test]
fn quality_retry_sparse_recovers_only_missing_line_and_never_reruns_old_line() {
    let image = image(160, 80, false);
    let mut det = Scripted::new(vec![
        Ok(map(&[(8, 16), (40, 48)], 0.95)),
        Ok(map(&[(8, 16), (40, 48)], 0.95)),
    ]);
    let mut cls = Scripted::new(vec![
        Ok(orientation(true)),
        Ok(orientation(false)),
        Ok(orientation(false)),
    ]);
    let mut rec = Scripted::new(vec![
        Ok(ctc(&[0, 1, 0], 0.55)),
        Ok(ctc(&[0, 0, 0], 1.)),
        Ok(ctc(&[0, 2, 0], 0.99)),
    ]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, "中\n文");
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 3, 3));
    assert_eq!(
        result.lines[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_eq!(
        result.lines[1].horizontal_direction,
        HorizontalDirection::LeftToRight
    );
    assert_contract(&result, &image);
}

#[test]
fn quality_retry_normal_multiline_pipeline_stays_one_detector_even_with_missed_line() {
    let image = image(160, 80, false);
    let mut det = Scripted::new(vec![Ok(map(&[(6, 12), (25, 31), (46, 52)], 0.95))]);
    let mut cls = Scripted::new(vec![
        Ok(orientation(false)),
        Ok(orientation(false)),
        Ok(orientation(false)),
    ]);
    let mut rec = Scripted::new(vec![
        Ok(ctc(&[0, 1, 0], 0.6)),
        Ok(ctc(&[0, 2, 0], 0.6)),
        Ok(ctc(&[0, 0, 0], 1.)),
    ]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, "中\n文");
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 3, 3));
}

#[test]
fn quality_retry_sparse_secondary_error_keeps_original_low_confidence_line_and_bounds() {
    let image = image(160, 80, false);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let original_map = || map(&[(8, 16), (40, 48)], 0.95);
    let detected = detect_rgb(
        &rgb,
        &mut Scripted::new(vec![Ok(original_map())]),
        default_config(&image, 1.6),
    )
    .unwrap();
    let mut cls = Scripted::new(vec![Ok(orientation(true)), Ok(orientation(false))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 0.55)), Ok(ctc(&[0, 0, 0], 1.))]);
    let expected = recognize_lines(&rgb, &detected, &mut cls, &mut rec, &dictionary())
        .unwrap()
        .result;
    let mut det = Scripted::new(vec![Ok(original_map()), Ok(original_map())]);
    let mut cls = Scripted::new(vec![
        Ok(orientation(true)),
        Ok(orientation(false)),
        Ok(orientation(false)),
    ]);
    let mut rec = Scripted::new(vec![
        Ok(ctc(&[0, 1, 0], 0.55)),
        Ok(ctc(&[0, 0, 0], 1.)),
        Err("补救失败".into()),
    ]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, expected.text);
    assert_eq!(result.lines[0].rect, expected.lines[0].rect);
    assert_eq!(result.words[0].text, expected.words[0].text);
    assert_eq!(result.words[0].rect, expected.words[0].rect);
    assert_eq!(
        result.words[0].horizontal_direction,
        expected.words[0].horizontal_direction
    );
    assert_eq!(result.regions, expected.regions);
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 3, 3));
}

#[test]
fn quality_retry_recovery_with_one_unrecognized_line_is_rejected_atomically() {
    let image = image(160, 80, false);
    let mut det = Scripted::new(vec![Ok(map(&[], 0.)), Ok(map(&[(8, 16), (40, 48)], 0.95))]);
    let mut cls = Scripted::new(vec![Ok(orientation(false)), Ok(orientation(false))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 0.99)), Ok(ctc(&[0, 0, 0], 1.))]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert!(result.text.is_empty() && result.words.is_empty() && result.lines.is_empty());
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 2, 2));
}

#[test]
fn quality_retry_more_than_eight_detected_lines_are_not_truncated_or_recognized() {
    let image = image(160, 320, false);
    let bands: Vec<_> = (0..9).map(|index| (index * 7 + 1, index * 7 + 4)).collect();
    let mut det = Scripted::new(vec![Ok(map(&[], 0.)), Ok(map(&bands, 0.95))]);
    let mut cls = Scripted::default();
    let mut rec = Scripted::default();
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert!(result.text.is_empty());
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 0, 0));
}

#[test]
fn quality_retry_secondary_failures_preserve_successful_empty_primary_result() {
    for stage in 0..3 {
        let image = image(160, 80, false);
        let mut det = Scripted::new(vec![
            Ok(map(&[], 0.)),
            if stage == 0 {
                Err("补救检测失败".into())
            } else {
                Ok(map(&[(12, 24)], 0.95))
            },
        ]);
        let mut cls = Scripted::new(vec![if stage == 1 {
            Err("补救分类失败".into())
        } else {
            Ok(orientation(false))
        }]);
        let mut rec = Scripted::new(vec![Err("补救识别失败".into())]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert!(
            result.text.is_empty()
                && result.lines.is_empty()
                && result.words.is_empty()
                && result.regions.is_empty()
        );
        assert_eq!(det.calls(), 2);
        assert_eq!(cls.calls(), usize::from(stage != 0));
        assert_eq!(rec.calls(), usize::from(stage == 2));
    }
}

#[test]
fn quality_retry_first_pass_hard_errors_are_not_hidden_by_speculative_recovery() {
    for stage in 0..3 {
        let image = image(160, 80, false);
        let expected = ["首检测失败", "首分类失败", "首识别失败"][stage];
        let mut det = Scripted::new(vec![if stage == 0 {
            Err(expected.into())
        } else {
            Ok(map(&[(12, 24)], 0.95))
        }]);
        let mut cls = Scripted::new(vec![if stage == 1 {
            Err(expected.into())
        } else {
            Ok(orientation(false))
        }]);
        let mut rec = Scripted::new(vec![Err(expected.into())]);
        assert_eq!(
            recognize(&image, &mut det, &mut cls, &mut rec, &dictionary())
                .err()
                .unwrap()
                .to_string(),
            expected
        );
        assert_eq!(det.calls(), 1);
        assert_eq!(cls.calls(), usize::from(stage != 0));
        assert_eq!(rec.calls(), usize::from(stage == 2));
    }
}

#[test]
fn quality_retry_lower_candidate_confidence_or_still_empty_never_causes_third_attempt() {
    for confidence in [0., 0.49, 0.5, 0.899] {
        let image = image(160, 80, false);
        let mut det = Scripted::new(vec![Ok(map(&[], 0.)), Ok(map(&[(12, 24)], 0.95))]);
        let mut cls = Scripted::new(vec![Ok(orientation(false))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], confidence))]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert!(result.text.is_empty());
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 1, 1));
    }
}

#[test]
fn quality_retry_detector_only_remains_one_pass_without_classifier_or_recognizer() {
    let image = image(160, 80, false);
    let mut det = Scripted::new(vec![Ok(map(&[], 0.))]);
    assert!(detect(&image, &mut det).unwrap().is_empty());
    assert_eq!(det.calls(), 1);
}
