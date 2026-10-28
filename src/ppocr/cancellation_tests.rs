//! 合成像素与脚本Runner只验证阶段取消、错误优先及原回退合同；不启动ORT。
use super::*;
use crate::recognition::task::CancelOwner;

#[derive(Debug)]
struct GenuineFailure(&'static str);

impl std::fmt::Display for GenuineFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for GenuineFailure {}

enum Step {
    Output(Tensor),
    Failure,
    Cancel,
    CancelWithOutput(Tensor),
    CancelWithFailure,
}

struct Scripted<'a> {
    owner: &'a CancelOwner,
    token: TaskToken,
    steps: VecDeque<Step>,
    calls: usize,
}

impl<'a> Scripted<'a> {
    fn new(owner: &'a CancelOwner, steps: Vec<Step>) -> Self {
        Self {
            owner,
            token: owner.token(),
            steps: steps.into(),
            calls: 0,
        }
    }
}

impl ModelRunner for Scripted<'_> {
    fn run(&mut self, _: Tensor) -> Result<Tensor> {
        self.calls += 1;
        match self.steps.pop_front().expect("没有额外推理预算") {
            Step::Output(output) => Ok(output),
            Step::Failure => Err(Box::new(GenuineFailure("真实模型错误"))),
            Step::Cancel => {
                self.owner.cancel().unwrap();
                Err(self.token.checkpoint().unwrap_err())
            }
            Step::CancelWithOutput(output) => {
                self.owner.cancel().unwrap();
                Ok(output)
            }
            Step::CancelWithFailure => {
                self.owner.cancel().unwrap();
                Err(Box::new(GenuineFailure("真实模型错误")))
            }
        }
    }
}

fn image(width: u32, height: u32) -> Image {
    let mut bgra = vec![255; width as usize * height as usize * 4];
    let center = (height as usize / 2 * width as usize + width as usize / 2) * 4;
    bgra[center..center + 3].fill(0);
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}

fn rec_only_image() -> Image {
    let mut image = image(160, 40);
    image.bgra.fill(255);
    for y in 12..26 {
        for x in 10..150 {
            if (x - 10) % 8 < 3 {
                let index = (y * 160 + x) * 4;
                image.bgra[index..index + 3].fill(0);
            }
        }
    }
    assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_some());
    image
}

fn orientation() -> Tensor {
    Tensor {
        shape: vec![1, 2],
        values: vec![0.99, 0.01],
    }
}

fn ctc() -> Tensor {
    Tensor {
        shape: vec![1, 5, 4],
        values: vec![
            1., 0., 0., 0., 0., 0.99, 0., 0., 1., 0., 0., 0., 0., 0., 0.99, 0., 1., 0., 0., 0.,
        ],
    }
}

fn map(bands: &[(usize, usize)]) -> Tensor {
    let mut values = vec![0.; 64 * 64];
    for &(top, bottom) in bands {
        for y in top..bottom {
            for x in 10..54 {
                values[y * 64 + x] = 0.95;
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

fn assert_cancelled<T>(result: Result<T>) {
    let error = result.err().expect("取消必须返回专门错误");
    assert!(is_cancelled(error.as_ref()));
}

fn assert_genuine<T>(result: Result<T>) {
    let error = result.err().expect("真实模型错误必须原样返回");
    assert!(!is_cancelled(error.as_ref()));
    assert_eq!(
        error.downcast_ref::<GenuineFailure>().unwrap().0,
        "真实模型错误"
    );
}

fn assert_invalid_output<T>(result: Result<T>, expected: &str) {
    let error = result.err().expect("非法输出必须保留真实错误");
    assert!(!is_cancelled(error.as_ref()));
    assert_eq!(error.to_string(), expected);
}

#[test]
fn cancelled_pipeline_entry_does_not_preprocess_or_run_models() {
    let owner = CancelOwner::new();
    let token = owner.token();
    owner.cancel().unwrap();
    let invalid = Image {
        width: 160,
        height: 80,
        bgra: Vec::new(),
        readback_ms: 0.,
    };
    let mut det = Scripted::new(&owner, vec![]);
    let mut cls = Scripted::new(&owner, vec![]);
    let mut rec = Scripted::new(&owner, vec![]);
    assert_cancelled(detect_cancellable(&invalid, &mut det, &token));
    assert_cancelled(recognize_cancellable(
        &invalid,
        &mut det,
        &mut cls,
        &mut rec,
        &[],
        &token,
    ));
    assert_eq!((det.calls, cls.calls, rec.calls), (0, 0, 0));
}

#[test]
fn cancellation_during_detector_stops_before_classification() {
    for step in [Step::Cancel, Step::CancelWithOutput(map(&[(12, 24)]))] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![step]);
        let mut cls = Scripted::new(&owner, vec![]);
        let mut rec = Scripted::new(&owner, vec![]);
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls, rec.calls), (1, 0, 0));
    }
}

#[test]
fn detector_genuine_error_wins_over_concurrent_cancellation() {
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![Step::CancelWithFailure]);
    assert_genuine(detect_cancellable(
        &image(160, 80),
        &mut det,
        &owner.token(),
    ));
    assert_eq!(det.calls, 1);
}

#[test]
fn detector_invalid_output_wins_over_concurrent_cancellation() {
    let owner = CancelOwner::new();
    let invalid = Tensor {
        shape: vec![1, 1, 1, 1],
        values: vec![2.],
    };
    let mut det = Scripted::new(&owner, vec![Step::CancelWithOutput(invalid)]);
    assert_invalid_output(
        detect_cancellable(&image(160, 80), &mut det, &owner.token()),
        "OCR 检测概率图无效",
    );
    assert_eq!(det.calls, 1);
}

#[test]
fn cancellation_during_classifier_does_not_start_recognizer_or_next_line() {
    for step in [Step::Cancel, Step::CancelWithOutput(orientation())] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(8, 16), (40, 48)]))]);
        let mut cls = Scripted::new(&owner, vec![step]);
        let mut rec = Scripted::new(&owner, vec![]);
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls, rec.calls), (1, 1, 0));
    }
}

#[test]
fn cancellation_during_recognizer_discards_partial_lines_and_stops_next_line() {
    for step in [Step::Cancel, Step::CancelWithOutput(ctc())] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(8, 16), (40, 48)]))]);
        let mut cls = Scripted::new(&owner, vec![Step::Output(orientation())]);
        let mut rec = Scripted::new(&owner, vec![step]);
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls, rec.calls), (1, 1, 1));
    }
}

#[test]
fn cancellation_after_one_recognized_line_does_not_return_partial_text() {
    for step in [Step::Cancel, Step::CancelWithOutput(ctc())] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(8, 16), (40, 48)]))]);
        let mut cls = Scripted::new(
            &owner,
            vec![Step::Output(orientation()), Step::Output(orientation())],
        );
        let mut rec = Scripted::new(&owner, vec![Step::Output(ctc()), step]);
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls, rec.calls), (1, 2, 2));
    }
}

#[test]
fn classifier_and_recognizer_genuine_errors_win_over_concurrent_cancellation() {
    for classifier_failed in [true, false] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(12, 24)]))]);
        let mut cls = Scripted::new(
            &owner,
            if classifier_failed {
                vec![Step::CancelWithFailure]
            } else {
                vec![Step::Output(orientation())]
            },
        );
        let mut rec = Scripted::new(
            &owner,
            if classifier_failed {
                vec![]
            } else {
                vec![Step::CancelWithFailure]
            },
        );
        assert_genuine(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls), (1, 1));
        assert_eq!(rec.calls, usize::from(!classifier_failed));
    }
}

#[test]
fn classifier_invalid_output_wins_over_concurrent_cancellation() {
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(12, 24)]))]);
    let mut cls = Scripted::new(
        &owner,
        vec![Step::CancelWithOutput(Tensor {
            shape: vec![1, 3],
            values: vec![0., 0., 1.],
        })],
    );
    let mut rec = Scripted::new(&owner, vec![]);
    assert_invalid_output(
        recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ),
        "OCR 方向分类模型输出无效",
    );
    assert_eq!((det.calls, cls.calls, rec.calls), (1, 1, 0));
}

#[test]
fn ctc_invalid_output_wins_over_concurrent_cancellation() {
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(12, 24)]))]);
    let mut cls = Scripted::new(&owner, vec![Step::Output(orientation())]);
    let mut rec = Scripted::new(
        &owner,
        vec![Step::CancelWithOutput(Tensor {
            shape: vec![1, 1, 3],
            values: vec![0., 1., 0.],
        })],
    );
    assert_invalid_output(
        recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ),
        "OCR 模型类别数 3 与词表 2 不匹配",
    );
    assert_eq!((det.calls, cls.calls, rec.calls), (1, 1, 1));
}

#[test]
fn rec_only_typed_cancellation_never_falls_through_to_detector() {
    let image = rec_only_image();
    for classifier_cancelled in [true, false] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![]);
        let mut cls = Scripted::new(
            &owner,
            if classifier_cancelled {
                vec![Step::Cancel]
            } else {
                vec![Step::Output(orientation())]
            },
        );
        let mut rec = Scripted::new(
            &owner,
            if classifier_cancelled {
                vec![]
            } else {
                vec![Step::Cancel]
            },
        );
        assert_cancelled(recognize_cancellable(
            &image,
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls), (0, 1));
        assert_eq!(rec.calls, usize::from(!classifier_cancelled));
    }
}

#[test]
fn rec_only_success_followed_by_cancellation_never_returns_or_detects() {
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![]);
    let mut cls = Scripted::new(&owner, vec![Step::Output(orientation())]);
    let mut rec = Scripted::new(&owner, vec![Step::CancelWithOutput(ctc())]);
    assert_cancelled(recognize_cancellable(
        &rec_only_image(),
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(),
        &owner.token(),
    ));
    assert_eq!((det.calls, cls.calls, rec.calls), (0, 1, 1));
}

#[test]
fn rec_only_real_error_with_concurrent_cancellation_keeps_error_and_stops_fallback() {
    for classifier_failed in [true, false] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![]);
        let mut cls = Scripted::new(
            &owner,
            if classifier_failed {
                vec![Step::CancelWithFailure]
            } else {
                vec![Step::Output(orientation())]
            },
        );
        let mut rec = Scripted::new(
            &owner,
            if classifier_failed {
                vec![]
            } else {
                vec![Step::CancelWithFailure]
            },
        );
        assert_genuine(recognize_cancellable(
            &rec_only_image(),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls), (0, 1));
        assert_eq!(rec.calls, usize::from(!classifier_failed));
    }
}

#[test]
fn rec_only_real_failure_keeps_one_detector_fallback_with_live_task() {
    let image = rec_only_image();
    let saved = image.bgra.clone();
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![Step::Output(map(&[(12, 24)]))]);
    let mut cls = Scripted::new(&owner, vec![Step::Failure, Step::Output(orientation())]);
    let mut rec = Scripted::new(&owner, vec![Step::Output(ctc())]);
    let result = recognize_cancellable(
        &image,
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(),
        &owner.token(),
    )
    .unwrap();
    assert_eq!(result.text, "中文");
    assert_eq!((det.calls, cls.calls, rec.calls), (1, 2, 1));
    assert_eq!(image.bgra, saved);
}

#[test]
fn quality_recovery_detector_cancellation_does_not_return_primary() {
    for step in [Step::Cancel, Step::CancelWithOutput(map(&[(12, 24)]))] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(&owner, vec![Step::Output(map(&[])), step]);
        let mut cls = Scripted::new(&owner, vec![]);
        let mut rec = Scripted::new(&owner, vec![]);
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls, rec.calls), (2, 0, 0));
    }
}

#[test]
fn quality_recovery_classifier_or_recognizer_cancellation_does_not_return_primary() {
    for classifier_cancelled in [true, false] {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(
            &owner,
            vec![Step::Output(map(&[])), Step::Output(map(&[(12, 24)]))],
        );
        let mut cls = Scripted::new(
            &owner,
            if classifier_cancelled {
                vec![Step::Cancel]
            } else {
                vec![Step::Output(orientation())]
            },
        );
        let mut rec = Scripted::new(
            &owner,
            if classifier_cancelled {
                vec![]
            } else {
                vec![Step::Cancel]
            },
        );
        assert_cancelled(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!((det.calls, cls.calls), (2, 1));
        assert_eq!(rec.calls, usize::from(!classifier_cancelled));
    }
}

#[test]
fn quality_recovery_real_failure_keeps_primary_with_live_task() {
    let owner = CancelOwner::new();
    let mut det = Scripted::new(&owner, vec![Step::Output(map(&[])), Step::Failure]);
    let mut cls = Scripted::new(&owner, vec![]);
    let mut rec = Scripted::new(&owner, vec![]);
    let result = recognize_cancellable(
        &image(160, 80),
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(),
        &owner.token(),
    )
    .unwrap();
    assert!(result.text.is_empty());
    assert_eq!((det.calls, cls.calls, rec.calls), (2, 0, 0));
}

#[test]
fn quality_recovery_real_error_with_concurrent_cancellation_keeps_error_not_primary() {
    for failed_stage in 0..3 {
        let owner = CancelOwner::new();
        let mut det = Scripted::new(
            &owner,
            vec![
                Step::Output(map(&[])),
                if failed_stage == 0 {
                    Step::CancelWithFailure
                } else {
                    Step::Output(map(&[(12, 24)]))
                },
            ],
        );
        let mut cls = Scripted::new(
            &owner,
            match failed_stage {
                0 => vec![],
                1 => vec![Step::CancelWithFailure],
                _ => vec![Step::Output(orientation())],
            },
        );
        let mut rec = Scripted::new(
            &owner,
            if failed_stage == 2 {
                vec![Step::CancelWithFailure]
            } else {
                vec![]
            },
        );
        assert_genuine(recognize_cancellable(
            &image(160, 80),
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(),
            &owner.token(),
        ));
        assert_eq!(det.calls, 2);
        assert_eq!(cls.calls, usize::from(failed_stage > 0));
        assert_eq!(rec.calls, usize::from(failed_stage == 2));
    }
}
