//! 脚本张量验证流水线接线；不执行 ORT、加载模型或证明真实图片去重准确率。
use super::*;
use crate::recognition::layout::CopyLayoutMode;

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
    let mut bgra = vec![255; 256 * 256 * 4];
    bgra[0..3].fill(0);
    Image {
        width: 256,
        height: 256,
        bgra,
        readback_ms: 0.,
    }
}

fn detection(quad: [Point; 4], confidence: f32) -> DetectedLine {
    DetectedLine {
        quad,
        confidence,
        rect: bounds(&quad, 256, 256).unwrap(),
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
        values[step * 4 + index] = confidence;
    }
    Tensor {
        shape: vec![1, sequence.len(), 4],
        values,
    }
}

fn candidate(
    detected: &[DetectedLine],
    rotations: &[bool],
    outputs: Vec<Tensor>,
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
    let result = recognize_lines(
        &rgb,
        detected,
        &mut cls,
        &mut rec,
        &["甲".into(), "乙".into()],
    )
    .unwrap();
    assert_eq!((cls.calls, rec.calls), (detected.len(), detected.len()));
    assert!(cls.outputs.is_empty());
    assert!(rec.outputs.is_empty());
    result
}

fn quad(y: f32) -> [Point; 4] {
    [[20., y], [150., y], [150., y + 20.], [20., y + 20.]]
}

fn assert_contract(result: &RecognizedText) {
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    assert!(valid_quality_contract(result, &rgb));
    for (word, region) in result.words.iter().zip(&result.regions) {
        assert_eq!(word.rect, *region);
        assert_eq!(
            word.horizontal_direction,
            result.lines[word.line].horizontal_direction
        );
        assert!(word.geometry.unwrap().validated_in(word.rect).is_some());
    }
}

#[test]
fn ordinary_duplicate_pass_keeps_best_ctc_candidate_and_all_detector_ownership() {
    let first = quad(30.);
    let second = first.map(|[x, y]| [x + 0.1, y + 0.1]);
    let detected = [detection(first, 0.99), detection(second, 0.75)];
    let result = candidate(
        &detected,
        &[false, false],
        vec![ctc(&[1, 0, 2, 0], 0.65), ctc(&[1, 0, 2, 0], 0.99)],
    );
    assert_eq!(result.result.text, "甲乙");
    assert_eq!(result.result.lines.len(), 1);
    assert_eq!(result.accepted_detected, [0, 1]);
    assert!(result.retryable_missed.is_empty());
    assert!((result.confidence - 0.65).abs() < 1e-6);
    assert_eq!(
        result.result.lines[0].geometry,
        Crop::new(second).unwrap().text_geometry(0., 1.)
    );
    let expected = candidate(&detected[1..], &[false], vec![ctc(&[1, 0, 2, 0], 0.99)]).result;
    assert_eq!(result.result.lines[0].rect, expected.lines[0].rect);
    for (actual, expected) in result.result.words.iter().zip(&expected.words) {
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.rect, expected.rect);
        assert_eq!(actual.geometry, expected.geometry);
        assert_eq!(actual.line, 0);
    }
    assert_contract(&result.result);
}

#[test]
fn deduplicated_primary_does_not_create_sparse_quality_retry_eligibility() {
    let detected = [
        detection(quad(30.), 0.99),
        detection(quad(30.), 0.99),
        detection(quad(80.), 0.99),
    ];
    let primary = candidate(
        &detected,
        &[false; 3],
        vec![
            ctc(&[1, 0, 2, 0], 0.99),
            ctc(&[1, 0, 2, 0], 0.99),
            ctc(&[0, 0], 0.99),
        ],
    );
    assert_eq!(primary.result.lines.len(), 1);
    assert_eq!(primary.accepted_detected, [0, 1]);
    assert_eq!(primary.retryable_missed, [2]);
    let image = source();
    let rgb = RgbImage::from_bgra(&image).unwrap();
    assert!(quality_retry_plan(&image, &rgb, &primary, &detected).is_none());
    // 对照真实只有一个成功det时，原遗漏框仍能产生有限补救计划。
    let single = candidate(
        &[detected[0].clone(), detected[2].clone()],
        &[false; 2],
        vec![ctc(&[1, 0, 2, 0], 0.99), ctc(&[0, 0], 0.99)],
    );
    assert!(quality_retry_plan(
        &image,
        &rgb,
        &single,
        &[detected[0].clone(), detected[2].clone()]
    )
    .is_some());
}

#[test]
fn duplicate_removal_does_not_raise_quality_retry_confidence_gate() {
    let detected = [detection(quad(30.), 0.99), detection(quad(30.), 0.99)];
    let candidate = candidate(
        &detected,
        &[false; 2],
        vec![ctc(&[1, 0, 2, 0], 0.88), ctc(&[1, 0, 2, 0], 0.99)],
    );
    assert_eq!(candidate.result.lines.len(), 1);
    assert!((candidate.confidence - 0.88).abs() < 1e-6);
    assert!(!valid_quality_candidate(
        &candidate,
        &RgbImage::from_bgra(&source()).unwrap()
    ));
}

#[test]
fn ordinary_projected_duplicate_words_keep_real_line_indices_and_original_text() {
    let detected = [
        detection(quad(30.), 0.99),
        detection(quad(80.), 0.99),
        detection(quad(30.), 0.99),
    ];
    let result = candidate(
        &detected,
        &[false; 3],
        vec![
            ctc(&[1, 0, 3, 0, 2, 0], 0.91),
            ctc(&[2, 0, 1, 0], 0.99),
            ctc(&[1, 0, 3, 0, 2, 0], 0.99),
        ],
    )
    .result;
    assert_eq!(result.text, "甲 乙\n乙甲");
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.line)
            .collect::<Vec<_>>(),
        [0, 0, 1, 1]
    );
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["甲", "乙", "乙", "甲"]
    );
    assert_contract(&result);
    assert_eq!(
        result
            .copy_layout()
            .copy(0..result.text.len(), CopyLayoutMode::Original)
            .unwrap(),
        result.text
    );
}

#[test]
fn opposite_classifier_directions_remain_distinct_after_ordinary_recognition() {
    let detected = [detection(quad(30.), 0.99), detection(quad(30.), 0.99)];
    let result = candidate(
        &detected,
        &[false, true],
        vec![ctc(&[1, 0, 2, 0], 0.99), ctc(&[1, 0, 2, 0], 0.99)],
    )
    .result;
    assert_eq!(result.text, "甲乙\n甲乙");
    assert_eq!(result.lines.len(), 2);
    assert_ne!(
        result.lines[0].horizontal_direction,
        result.lines[1].horizontal_direction
    );
    assert_contract(&result);
}

#[test]
fn quality_recovery_keeps_deduplicated_primary_and_rejects_overlapping_additions() {
    let primary = candidate(
        &[detection(quad(140.), 0.99), detection(quad(140.), 0.99)],
        &[false; 2],
        vec![ctc(&[1, 0, 2, 0], 0.91), ctc(&[1, 0, 2, 0], 0.99)],
    )
    .result;
    assert_eq!(primary.lines.len(), 1);
    let overlap = candidate(
        &[detection(quad(140.), 0.99)],
        &[false],
        vec![ctc(&[1, 0, 2, 0], 0.99)],
    )
    .result;
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    assert!(merge_quality_recovery(&primary, overlap, &rgb).is_none());
    let vertical = [[180., 10.], [205., 10.], [205., 60.], [180., 60.]];
    let recovered = candidate(
        &[detection(vertical, 0.99)],
        &[false],
        vec![ctc(&[2, 0, 1, 0], 0.99)],
    )
    .result;
    let original_line = primary.lines[0].geometry;
    let original_words = primary
        .words
        .iter()
        .map(|word| word.geometry)
        .collect::<Vec<_>>();
    let recovered_line = recovered.lines[0].geometry;
    let merged = merge_quality_recovery(&primary, recovered, &rgb).unwrap();
    assert_eq!(merged.text, "乙甲\n甲乙");
    assert_eq!(merged.lines[0].geometry, recovered_line);
    assert_eq!(merged.lines[1].geometry, original_line);
    assert_eq!(
        merged.words[2..]
            .iter()
            .map(|word| word.geometry)
            .collect::<Vec<_>>(),
        original_words
    );
    assert_contract(&merged);
    assert_eq!(
        merged
            .copy_layout()
            .copy(0..merged.text.len(), CopyLayoutMode::Auto)
            .unwrap(),
        merged.text
    );
}
