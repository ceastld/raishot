//! Profile contracts execute the actual pixel/CTC/geometry path with scripted tensors.
use super::*;
use crate::recognition::task::CancelOwner;

#[derive(Debug)]
struct GenuineFailure;
impl std::fmt::Display for GenuineFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("真实 REC 错误")
    }
}
impl std::error::Error for GenuineFailure {}

#[derive(Default)]
struct Scripted<'a> {
    outputs: VecDeque<Result<Tensor>>,
    inputs: Vec<Vec<usize>>,
    cancel_on_run: Option<&'a CancelOwner>,
}

impl Scripted<'_> {
    fn outputs(outputs: Vec<Result<Tensor>>) -> Self {
        Self {
            outputs: outputs.into(),
            ..Self::default()
        }
    }
}

impl ModelRunner for Scripted<'_> {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        self.inputs.push(input.shape);
        if let Some(owner) = self.cancel_on_run {
            owner.cancel()?;
        }
        self.outputs
            .pop_front()
            .unwrap_or_else(|| Err("没有额外推理预算".into()))
    }
}

fn image(width: u32, height: u32) -> Image {
    Image {
        width,
        height,
        bgra: vec![255; width as usize * height as usize * 4],
        readback_ms: 0.,
    }
}

fn single_line() -> Image {
    let mut source = image(160, 40);
    for y in 12..26 {
        for x in 10..150 {
            if (x - 10) % 8 < 3 {
                let index = (y * 160 + x) * 4;
                source.bgra[index..index + 3].fill(0);
            }
        }
    }
    assert!(rec_only_line(&RgbImage::from_bgra(&source).unwrap()).is_some());
    source
}

fn dictionary(profile: OcrProfile) -> Vec<String> {
    let mut dictionary = vec!["字".into(); profile.dict_class_contract().dictionary_entries];
    dictionary[0] = "中".into();
    dictionary[1] = "文".into();
    *dictionary.last_mut().unwrap() = match profile {
        OcrProfile::Tiny => "￥".into(),
        OcrProfile::Small => "🛅".into(),
    };
    dictionary
}

fn ctc(profile: OcrProfile) -> Tensor {
    let contract = profile.dict_class_contract();
    let sequence = [
        0,
        1,
        1,
        0,
        contract.dictionary_entries,
        0,
        contract.space_index,
        0,
        2,
        0,
    ];
    let mut values = vec![0.; sequence.len() * contract.ctc_classes];
    for (step, index) in sequence.into_iter().enumerate() {
        values[step * contract.ctc_classes + index] = 0.99;
    }
    Tensor {
        shape: vec![1, sequence.len(), contract.ctc_classes],
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

fn detection() -> Tensor {
    let mut values = vec![0.; 32 * 32];
    for y in 10..20 {
        for x in 5..26 {
            values[y * 32 + x] = 0.95;
        }
    }
    Tensor {
        shape: vec![1, 1, 32, 32],
        values,
    }
}

#[test]
fn strict_entry_rejects_cross_profile_dictionary_before_preprocessing_or_inference() {
    let owner = CancelOwner::new();
    let mut invalid = image(1, 1);
    invalid.bgra.clear();
    for (profile, other) in [
        (OcrProfile::Small, OcrProfile::Tiny),
        (OcrProfile::Tiny, OcrProfile::Small),
    ] {
        let mut det = Scripted::default();
        let mut cls = Scripted::default();
        let mut rec = Scripted::default();
        let error = recognize_profile_cancellable(
            &invalid,
            &mut det,
            &mut cls,
            &mut rec,
            &dictionary(other),
            profile,
            &owner.token(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains(profile.as_str()));
        assert!(det.inputs.is_empty() && cls.inputs.is_empty() && rec.inputs.is_empty());
    }
}

#[test]
fn cancelled_strict_entry_wins_over_an_invalid_dictionary_without_inference() {
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let mut det = Scripted::default();
    let mut cls = Scripted::default();
    let mut rec = Scripted::default();
    let error = recognize_profile_cancellable(
        &image(1, 1),
        &mut det,
        &mut cls,
        &mut rec,
        &[],
        OcrProfile::Small,
        &owner.token(),
    )
    .err()
    .unwrap();
    assert!(is_cancelled(error.as_ref()));
    assert!(det.inputs.is_empty() && cls.inputs.is_empty() && rec.inputs.is_empty());
}

#[test]
fn both_profiles_share_rec_only_ctc_space_unicode_and_180_degree_inverse_geometry() {
    for profile in [OcrProfile::Tiny, OcrProfile::Small] {
        for rotated in [false, true] {
            let source = single_line();
            let pixels = source.bgra.clone();
            let owner = CancelOwner::new();
            let mut det = Scripted::default();
            let mut cls = Scripted::outputs(vec![Ok(orientation(rotated))]);
            let mut rec = Scripted::outputs(vec![Ok(ctc(profile))]);
            let result = recognize_profile_cancellable(
                &source,
                &mut det,
                &mut cls,
                &mut rec,
                &dictionary(profile),
                profile,
                &owner.token(),
            )
            .unwrap();
            let expected = match profile {
                OcrProfile::Tiny => "中￥ 文",
                OcrProfile::Small => "中🛅 文",
            };
            assert_eq!(result.text, expected);
            assert_eq!(result.lines.len(), 1);
            assert_eq!(result.words.len(), 3);
            assert_eq!(result.regions.len(), result.words.len());
            assert!(result.words.iter().all(|word| word.rect.valid(160, 40)));
            assert_eq!(
                result.lines[0].horizontal_direction,
                if rotated {
                    HorizontalDirection::RightToLeft
                } else {
                    HorizontalDirection::LeftToRight
                }
            );
            assert_eq!(result.words[0].rect.x > result.words[2].rect.x, rotated);
            assert!(result.lines[0].geometry.is_some());
            assert!(result.lines[0].graphemes.is_some());
            assert!(det.inputs.is_empty());
            assert_eq!(cls.inputs, [vec![1, 3, 80, 160]]);
            assert_eq!(rec.inputs[0][..3], [1, 3, 48]);
            assert_eq!(source.bgra, pixels);
        }
    }
}

#[test]
fn small_detector_route_rejects_tiny_rec_classes_without_switching_profile() {
    let owner = CancelOwner::new();
    let mut det = Scripted::outputs(vec![Ok(detection())]);
    let mut cls = Scripted::outputs(vec![Ok(orientation(false))]);
    let mut rec = Scripted::outputs(vec![Ok(ctc(OcrProfile::Tiny))]);
    let error = recognize_profile_cancellable(
        &image(320, 160),
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(OcrProfile::Small),
        OcrProfile::Small,
        &owner.token(),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("small"));
    assert!(error.to_string().contains("18710"));
    assert_eq!(
        (det.inputs.len(), cls.inputs.len(), rec.inputs.len()),
        (1, 1, 1)
    );
}

#[test]
fn selected_profile_rejects_wrong_batch_rank_and_class_shapes() {
    let owner = CancelOwner::new();
    let token = owner.token();
    for shape in [vec![2, 1, 18710], vec![1, 18710], vec![1, 1, 6906]] {
        let mut inner = Scripted::outputs(vec![Ok(Tensor {
            shape,
            values: Vec::new(),
        })]);
        let mut rec = ProfileRecognizer {
            inner: &mut inner,
            profile: OcrProfile::Small,
            task: &token,
        };
        assert!(rec
            .run(Tensor {
                shape: vec![1, 3, 48, 48],
                values: Vec::new()
            })
            .is_err());
        assert_eq!(inner.inputs.len(), 1);
    }
}

#[test]
fn cancelled_successful_rec_does_not_publish_text_or_run_detector() {
    let owner = CancelOwner::new();
    let mut det = Scripted::default();
    let mut cls = Scripted::outputs(vec![Ok(orientation(false))]);
    let mut rec = Scripted::outputs(vec![Ok(ctc(OcrProfile::Small))]);
    rec.cancel_on_run = Some(&owner);
    let error = recognize_profile_cancellable(
        &single_line(),
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(OcrProfile::Small),
        OcrProfile::Small,
        &owner.token(),
    )
    .err()
    .unwrap();
    assert!(is_cancelled(error.as_ref()));
    assert_eq!(
        (det.inputs.len(), cls.inputs.len(), rec.inputs.len()),
        (0, 1, 1)
    );
}

#[test]
fn genuine_run_failure_is_preserved_if_cancellation_is_also_requested() {
    let owner = CancelOwner::new();
    let token = owner.token();
    let mut inner = Scripted::outputs(vec![Err(Box::new(GenuineFailure))]);
    inner.cancel_on_run = Some(&owner);
    let mut rec = ProfileRecognizer {
        inner: &mut inner,
        profile: OcrProfile::Small,
        task: &token,
    };
    let error = rec
        .run(Tensor {
            shape: vec![1, 3, 48, 48],
            values: Vec::new(),
        })
        .err()
        .unwrap();
    assert!(error.downcast_ref::<GenuineFailure>().is_some());
    assert!(!is_cancelled(error.as_ref()));
    assert!(token.is_cancelled());
}

#[test]
fn successful_run_cancellation_wins_over_mismatched_profile_output() {
    let owner = CancelOwner::new();
    let token = owner.token();
    let mut inner = Scripted::outputs(vec![Ok(ctc(OcrProfile::Tiny))]);
    inner.cancel_on_run = Some(&owner);
    let mut rec = ProfileRecognizer {
        inner: &mut inner,
        profile: OcrProfile::Small,
        task: &token,
    };
    let error = rec
        .run(Tensor {
            shape: vec![1, 3, 48, 48],
            values: Vec::new(),
        })
        .err()
        .unwrap();
    assert!(is_cancelled(error.as_ref()));
    assert_eq!(inner.inputs.len(), 1);
}
