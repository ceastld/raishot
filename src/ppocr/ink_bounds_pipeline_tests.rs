//! 仅脚本张量与构造像素，验证最终接线；不执行 ORT 或证明真实 OCR 效果。
use super::*;
use crate::recognition::{geometry::Projection, task::CancelOwner};

struct Scripted {
    outputs: VecDeque<Tensor>,
    calls: usize,
}

impl Scripted {
    fn new(outputs: Vec<Tensor>) -> Self {
        Self {
            outputs: outputs.into(),
            calls: 0,
        }
    }
}

impl ModelRunner for Scripted {
    fn run(&mut self, _: Tensor) -> Result<Tensor> {
        self.calls += 1;
        self.outputs
            .pop_front()
            .ok_or_else(|| "出现额外推理阶段".into())
    }
}

fn source() -> Image {
    let mut image = Image {
        width: 128,
        height: 96,
        bgra: vec![255; 128 * 96 * 4],
        readback_ms: 0.,
    };
    // 自有高对比块仅作像素证据，不声称它是文字或真实模型输出。
    for y in 40..52 {
        for x in 32..96 {
            image.bgra[(y * 128 + x) * 4..(y * 128 + x) * 4 + 3].fill(0);
        }
    }
    image
}

fn detected() -> DetectedLine {
    DetectedLine {
        quad: [[16., 24.], [112., 24.], [112., 72.], [16., 72.]],
        confidence: 0.99,
        rect: Rect {
            x: 16,
            y: 24,
            width: 96,
            height: 48,
        },
    }
}

fn orientation() -> Tensor {
    Tensor {
        shape: vec![1, 2],
        values: vec![0.99, 0.01],
    }
}

fn ctc(confidence: f32, blank: bool) -> Tensor {
    Tensor {
        shape: vec![1, 2, 3],
        values: if blank {
            vec![confidence, 0., 0., confidence, 0., 0.]
        } else {
            vec![0., confidence, 0., confidence, 0., 0.]
        },
    }
}

fn candidate(
    rgb: &RgbImage,
    detected: &[DetectedLine],
    outputs: Vec<Tensor>,
) -> RecognitionCandidate {
    let mut cls = Scripted::new(detected.iter().map(|_| orientation()).collect());
    let mut rec = Scripted::new(outputs);
    let result = recognize_lines(rgb, detected, &mut cls, &mut rec, &["AB".into()]).unwrap();
    assert_eq!((cls.calls, rec.calls), (detected.len(), detected.len()));
    assert!(cls.outputs.is_empty() && rec.outputs.is_empty());
    result
}

fn assert_same_centerline(original: TextGeometry, tightened: TextGeometry) {
    let a = Projection::new(original).unwrap();
    let b = Projection::new(tightened).unwrap();
    assert_eq!(a.direction(), b.direction());
    for u in [0., 0.5, 1.] {
        let a = a.point(u, 0.5);
        let b = b.point(u, 0.5);
        assert!((a.x - b.x).abs() <= 0.001 && (a.y - b.y).abs() <= 0.001);
    }
}

#[test]
fn ordinary_candidate_geometry_is_raw_until_finalization() {
    let image = source();
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let before = candidate(&rgb, &[detected()], vec![ctc(0.99, false)]);
    let raw_geometry = before.result.words[0].geometry.unwrap();
    assert_eq!(
        raw_geometry,
        Crop::new(detected().quad)
            .unwrap()
            .text_geometry(0., 1.)
            .unwrap()
    );
    let raw_regions = before.result.regions.clone();
    let raw_line_geometry = before.result.lines[0].geometry;
    let raw_line_rect = before.result.lines[0].rect;
    let result = finish_recognition(&rgb, before.result, None).unwrap();
    assert_eq!(result.text, "AB");
    assert_eq!(result.regions, raw_regions);
    assert_eq!(result.words[0].rect, raw_regions[0]);
    assert_eq!(result.words[0].line, 0);
    assert_eq!(result.lines[0].rect, raw_line_rect);
    assert_eq!(result.lines[0].geometry, raw_line_geometry);
    let tightened = result.words[0].geometry.unwrap();
    assert_ne!(tightened, raw_geometry);
    assert!(tightened.validated_in(result.words[0].rect).is_some());
    assert_same_centerline(raw_geometry, tightened);
    assert!(valid_quality_contract(&result, &rgb));
}

#[test]
fn duplicate_ownership_and_original_confidence_survive_finalization() {
    let image = source();
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let mut missed = detected();
    missed.quad = [[16., 76.], [112., 76.], [112., 92.], [16., 92.]];
    missed.rect = bounds(&missed.quad, rgb.width, rgb.height).unwrap();
    let lines = [detected(), detected(), missed];
    let mut primary = candidate(
        &rgb,
        &lines,
        vec![ctc(0.88, false), ctc(0.99, false), ctc(0.99, true)],
    );
    assert_eq!(primary.result.lines.len(), 1);
    assert_eq!(primary.accepted_detected, [0, 1]);
    assert_eq!(primary.retryable_missed, [2]);
    assert!((primary.confidence - 0.88).abs() < 1e-6);
    assert!(quality_retry_plan(&image, &rgb, &primary, &lines).is_none());
    primary.result = finish_recognition(&rgb, primary.result, None).unwrap();
    assert_eq!(primary.accepted_detected, [0, 1]);
    assert_eq!(primary.retryable_missed, [2]);
    assert!((primary.confidence - 0.88).abs() < 1e-6);
    assert!(!valid_quality_candidate(&primary, &rgb));
    assert!(quality_retry_plan(&image, &rgb, &primary, &lines).is_none());
}

#[test]
fn low_contrast_finalization_retains_original_geometry() {
    let mut image = source();
    for pixel in image.bgra.chunks_exact_mut(4) {
        if pixel[0] == 0 {
            pixel[..3].fill(235);
        }
    }
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let candidate = candidate(&rgb, &[detected()], vec![ctc(0.99, false)]);
    let raw = candidate.result.words[0].geometry;
    let regions = candidate.result.regions.clone();
    let result = finish_recognition(&rgb, candidate.result, None).unwrap();
    assert_eq!(result.words[0].geometry, raw);
    assert_eq!(result.regions, regions);
    assert_eq!(result.text, "AB");
}

#[test]
fn finalization_cancel_is_error_instead_of_partial_success() {
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let candidate = candidate(&rgb, &[detected()], vec![ctc(0.99, false)]);
    let owner = CancelOwner::new();
    let task = owner.token();
    assert!(owner.cancel().unwrap());
    match finish_recognition(&rgb, candidate.result, Some(&task)) {
        Err(error) => assert!(is_cancelled(error.as_ref())),
        Ok(_) => panic!("最终紧框取消不得转换为部分成功"),
    }
}

#[test]
fn det_only_needs_one_detector_run_without_recognition_stages() {
    let mut det = Scripted::new(vec![Tensor {
        shape: vec![1, 1, 8, 8],
        values: vec![0.; 64],
    }]);
    assert!(detect(&source(), &mut det).unwrap().is_empty());
    assert_eq!(det.calls, 1);
    assert!(det.outputs.is_empty());
}

fn ordinary_fixture() -> (Image, Tensor) {
    let mut image = Image {
        width: 256,
        height: 128,
        bgra: vec![255; 256 * 128 * 4],
        readback_ms: 0.,
    };
    for y in 52..68 {
        for x in 80..168 {
            image.bgra[(y * 256 + x) * 4..(y * 256 + x) * 4 + 3].fill(0);
        }
    }
    let mut values = vec![0.; 64 * 64];
    for y in 26..38 {
        for x in 20..42 {
            values[y * 64 + x] = 0.99;
        }
    }
    (
        image,
        Tensor {
            shape: vec![1, 1, 64, 64],
            values,
        },
    )
}

#[test]
fn ordinary_success_runs_finalization_after_quality_choice() {
    let (image, map) = ordinary_fixture();
    let mut det = Scripted::new(vec![map]);
    let mut cls = Scripted::new(vec![orientation()]);
    let mut rec = Scripted::new(vec![ctc(0.99, false)]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &["AB".into()]).unwrap();
    assert_eq!((det.calls, cls.calls, rec.calls), (1, 1, 1));
    assert!(det.outputs.is_empty() && cls.outputs.is_empty() && rec.outputs.is_empty());
    assert_eq!(result.text, "AB");
    assert_eq!(result.words.len(), 1);
    let full = result.lines[0].geometry.unwrap();
    let tight = result.words[0].geometry.unwrap();
    assert_ne!(tight, full);
    assert_same_centerline(full, tight);
    let rgb = RgbImage::from_bgra(&image).unwrap();
    assert!(valid_quality_contract(&result, &rgb));
}

#[test]
fn rec_only_success_finishes_geometry_without_starting_detector() {
    // 与现有已验证的 RecOnly 门同一条纹像素形状；仅复验新最终返回接线。
    let mut image = Image {
        width: 160,
        height: 40,
        bgra: vec![255; 160 * 40 * 4],
        readback_ms: 0.,
    };
    for y in 12..26 {
        for x in 10..150 {
            if (x - 10) % 8 < 3 {
                image.bgra[(y * 160 + x) * 4..(y * 160 + x) * 4 + 3].fill(0);
            }
        }
    }
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let speculative = rec_only_line(&rgb).expect("已有安全门允许的条纹输入");
    let original = Crop::new(speculative.quad)
        .unwrap()
        .text_geometry(0., 1.)
        .unwrap();
    let mut det = Scripted::new(Vec::new());
    let mut cls = Scripted::new(vec![orientation()]);
    let mut rec = Scripted::new(vec![ctc(0.99, false)]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &["AB".into()]).unwrap();
    assert_eq!((det.calls, cls.calls, rec.calls), (0, 1, 1));
    assert!(cls.outputs.is_empty() && rec.outputs.is_empty());
    assert_eq!(result.text, "AB");
    assert_eq!(result.lines[0].geometry, Some(original));
    assert_ne!(result.words[0].geometry, Some(original));
    assert_same_centerline(original, result.words[0].geometry.unwrap());
    assert!(valid_quality_contract(&result, &rgb));
}

#[test]
fn empty_primary_recovery_finishes_only_after_candidate_is_accepted() {
    let (image, map) = ordinary_fixture();
    let mut det = Scripted::new(vec![
        Tensor {
            shape: vec![1, 1, 64, 64],
            values: vec![0.; 64 * 64],
        },
        map,
    ]);
    let mut cls = Scripted::new(vec![orientation()]);
    let mut rec = Scripted::new(vec![ctc(0.99, false)]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &["AB".into()]).unwrap();
    assert_eq!((det.calls, cls.calls, rec.calls), (2, 1, 1));
    assert!(det.outputs.is_empty() && cls.outputs.is_empty() && rec.outputs.is_empty());
    assert_eq!(result.text, "AB");
    assert_eq!(result.words.len(), 1);
    let original = result.lines[0].geometry.unwrap();
    assert_ne!(result.words[0].geometry, Some(original));
    assert_same_centerline(original, result.words[0].geometry.unwrap());
    let rgb = RgbImage::from_bgra(&image).unwrap();
    assert!(valid_quality_contract(&result, &rgb));
}

#[test]
fn bgra_alpha_composite_preserves_source_and_only_tightens_supported_ink() {
    for alpha in [0u8, 1, 128] {
        let mut image = source();
        for pixel in image.bgra.chunks_exact_mut(4) {
            if pixel[0] == 0 {
                pixel[3] = alpha;
            }
        }
        // 与主体相接的浅灰边缘，经实际半透明合成后仍需纳入几何。
        for y in 34..40 {
            for x in 64..68 {
                let offset = (y * 128 + x) * 4;
                image.bgra[offset..offset + 4].copy_from_slice(&[245, 245, 245, alpha]);
            }
        }
        // 位于 ROI 外环的隐藏 RGB 若误作墨迹，会破坏背景资格。
        let hidden = (46 * 128 + 15) * 4;
        image.bgra[hidden..hidden + 4].copy_from_slice(&[17, 99, 241, 0]);
        let original_bytes = image.bgra.clone();
        let rgb = RgbImage::from_bgra(&image).unwrap();
        assert_eq!(image.bgra, original_bytes, "白底合成不得改写源 BGRA");
        assert_eq!(rgb.pixels[46 * 128 + 15], [255.; 3]);

        let before = candidate(&rgb, &[detected()], vec![ctc(0.99, false)]);
        let raw_geometry = before.result.words[0].geometry.unwrap();
        let raw_regions = before.result.regions.clone();
        let raw_line_geometry = before.result.lines[0].geometry;
        let raw_line_rect = before.result.lines[0].rect;
        let result = finish_recognition(&rgb, before.result, None).unwrap();
        assert_eq!(image.bgra, original_bytes, "最终几何处理不得改写源 BGRA");
        assert_eq!(result.text, "AB");
        assert_eq!(result.regions, raw_regions);
        assert_eq!(result.words[0].rect, raw_regions[0]);
        assert_eq!(result.words[0].line, 0);
        assert_eq!(result.lines[0].rect, raw_line_rect);
        assert_eq!(result.lines[0].geometry, raw_line_geometry);

        match alpha {
            0 => {
                assert!(rgb.pixels.iter().all(|pixel| *pixel == [255.; 3]));
                assert_eq!(result.words[0].geometry, Some(raw_geometry));
            }
            1 => {
                assert!(rgb.pixels[46 * 128 + 64]
                    .iter()
                    .all(|channel| (*channel - 254.).abs() < 0.001));
                assert_eq!(
                    result.words[0].geometry,
                    Some(raw_geometry),
                    "真实 Alpha 1 合成偏差不得丢弃后收紧"
                );
            }
            128 => {
                let weak = rgb.pixels[34 * 128 + 64];
                assert!(weak
                    .iter()
                    .all(|channel| (2. ..40.).contains(&(255. - *channel))));
                let tightened = result.words[0].geometry.unwrap();
                assert_ne!(tightened, raw_geometry);
                assert!(tightened.validated_in(result.words[0].rect).is_some());
                assert_same_centerline(raw_geometry, tightened);
                let projection = Projection::new(tightened).unwrap();
                let mut covered_weak = 0;
                for (index, pixel) in rgb.pixels.iter().enumerate() {
                    if *pixel == [255.; 3] {
                        continue;
                    }
                    let x = index % rgb.width;
                    let y = index / rgb.width;
                    // 验证完整像素足迹，包括实际合成得到的浅灰边缘。
                    for (x, y) in [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)] {
                        let local = projection
                            .inverse(crate::annotation::Point {
                                x: x as f32,
                                y: y as f32,
                            })
                            .unwrap();
                        assert!(
                            local
                                .iter()
                                .all(|value| (-0.00001..=1.00001).contains(value)),
                            "Alpha 128 收紧不得裁掉原图中的完整合成像素"
                        );
                    }
                    if (64..68).contains(&x) && (34..40).contains(&y) {
                        covered_weak += 1;
                    }
                }
                assert_eq!(covered_weak, 24);
            }
            _ => unreachable!(),
        }
        assert!(valid_quality_contract(&result, &rgb));
    }
}
