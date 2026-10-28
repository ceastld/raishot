//! The synthetic pixels pass through the actual BGRA gate and tensor pipeline.
//! Runners provide model outputs and record inference calls; no runtime/assets load.
use super::*;
use std::ops::Range;

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
            .unwrap_or_else(|| Err("未预期的额外推理".into()))
            .map(|output| match output.shape.as_slice() {
                [1, 2] => Tensor {
                    shape: vec![windows, 2],
                    values: output.values.repeat(windows),
                },
                _ => output,
            })
    }
}

fn solid(width: u32, height: u32, gray: u8) -> Image {
    Image {
        width,
        height,
        bgra: [gray, gray, gray, 255]
            .into_iter()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect(),
        readback_ms: 0.,
    }
}

fn strokes(image: &mut Image, x: Range<usize>, y: Range<usize>, gray: u8) {
    let width = image.width as usize;
    for row in y {
        for column in x.clone() {
            if (column - x.start) % 8 < 3 {
                let index = (row * width + column) * 4;
                image.bgra[index..index + 4].copy_from_slice(&[gray, gray, gray, 255]);
            }
        }
    }
}

fn single_line() -> Image {
    let mut image = solid(160, 40, 255);
    strokes(&mut image, 10..150, 12..26, 0);
    image
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

fn ctc(sequence: &[usize], classes: usize, confidence: f32) -> Tensor {
    let mut values = vec![0.; sequence.len() * classes];
    for (step, &class) in sequence.iter().enumerate() {
        values[step * classes + class] = if class == 0 { 1. } else { confidence };
    }
    Tensor {
        shape: vec![1, sequence.len(), classes],
        values,
    }
}

fn dictionary() -> Vec<String> {
    vec!["中".into(), "文".into()]
}

fn detection_map(two_lines: bool) -> Tensor {
    let mut values = vec![0.; 32 * 32];
    let bands = if two_lines {
        vec![6..11, 20..25]
    } else {
        std::iter::once(10..20).collect::<Vec<_>>()
    };
    for band in bands {
        for y in band {
            for x in 5..26 {
                values[y * 32 + x] = 0.95;
            }
        }
    }
    Tensor {
        shape: vec![1, 1, 32, 32],
        values,
    }
}

fn empty_detection() -> Tensor {
    Tensor {
        shape: vec![1, 1, 32, 32],
        values: vec![0.; 32 * 32],
    }
}

fn assert_geometry(result: &RecognizedText, image: &Image) {
    assert!(!result.lines.is_empty());
    assert_eq!(result.regions.len(), result.words.len());
    for (word, region) in result.words.iter().zip(&result.regions) {
        assert_eq!(word.rect, *region);
        assert!(region.valid(image.width, image.height));
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
fn rec_only_image_gate_accepts_distributed_light_dark_and_alpha_composited_text() {
    let light = single_line();
    let mut dark = solid(160, 40, 0);
    strokes(&mut dark, 10..150, 12..26, 255);
    let mut translucent = single_line();
    for pixel in translucent.bgra.chunks_exact_mut(4) {
        if pixel[0] == 0 {
            pixel[3] = 128;
        }
    }
    for image in [light, dark, translucent] {
        let rgb = RgbImage::from_bgra(&image).unwrap();
        let line = rec_only_line(&rgb).expect("一条分布充分的文字带");
        assert_eq!(
            line.rect,
            Rect {
                x: 8,
                y: 10,
                width: 143,
                height: 18,
            }
        );
        assert_eq!(line.quad, [[8., 10.], [151., 10.], [151., 28.], [8., 28.]]);
    }
}

#[test]
fn rec_only_image_gate_rejects_uniform_transparent_faint_and_local_edges() {
    let mut transparent = single_line();
    for pixel in transparent.bgra.chunks_exact_mut(4) {
        pixel[3] = 0;
    }
    let mut faint = solid(160, 40, 128);
    strokes(&mut faint, 10..150, 12..26, 130);
    let mut local = solid(160, 40, 255);
    strokes(&mut local, 8..50, 12..26, 0);
    let mut outline = solid(160, 40, 255);
    for y in 12..26 {
        for x in 8..152 {
            if y == 12 || y == 25 || x == 8 || x == 151 {
                let index = (y * 160 + x) * 4;
                outline.bgra[index..index + 3].fill(0);
            }
        }
    }
    let mut full_height = solid(160, 40, 255);
    strokes(&mut full_height, 10..150, 0..40, 0);
    for image in [
        solid(160, 40, 255),
        solid(160, 40, 0),
        transparent,
        faint,
        local,
        outline,
        full_height,
    ] {
        assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_none());
    }
}

#[test]
fn rec_only_image_gate_rejects_tight_lines_and_faint_secondary_line() {
    assert!(rec_only_line(&RgbImage::from_bgra(&single_line()).unwrap()).is_some());
    let mut separated = solid(160, 60, 255);
    strokes(&mut separated, 10..150, 8..18, 0);
    strokes(&mut separated, 10..150, 38..48, 0);
    let mut tight = solid(160, 40, 255);
    strokes(&mut tight, 10..150, 8..15, 0);
    strokes(&mut tight, 10..150, 17..24, 0);
    let mut one_pixel_gap = solid(160, 40, 255);
    strokes(&mut one_pixel_gap, 10..150, 8..15, 0);
    strokes(&mut one_pixel_gap, 10..150, 16..23, 0);
    let mut interrupted = single_line();
    interrupted.bgra[19 * 160 * 4..20 * 160 * 4].fill(255);
    let mut faint_secondary = solid(160, 40, 255);
    strokes(&mut faint_secondary, 10..150, 8..18, 0);
    strokes(&mut faint_secondary, 10..150, 26..34, 220);
    let mut aliased_secondary = solid(160, 40, 255);
    strokes(&mut aliased_secondary, 10..150, 8..18, 0);
    for y in 26..34 {
        for x in (10..150).step_by(2) {
            let index = (y * 160 + x) * 4;
            aliased_secondary.bgra[index..index + 3].fill(240);
        }
    }
    for image in [
        separated,
        tight,
        one_pixel_gap,
        interrupted,
        faint_secondary,
        aliased_secondary,
    ] {
        assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_none());
    }
}

#[test]
fn rec_only_image_gate_keeps_size_and_actual_tensor_width_budget() {
    for (width, height, top, band_height) in [
        (31, 12, 4, 4),
        (160, 11, 3, 4),
        (160, 61, 12, 14),
        (40, 40, 12, 14),
        (4097, 12, 4, 4),
    ] {
        let mut image = solid(width, height, 255);
        strokes(&mut image, 3..width as usize - 3, top..top + band_height, 0);
        assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_none());
    }
    assert_eq!(recognition_width(4096, 12).unwrap(), (16384, 16384));
    assert!(recognition_width(4097, 12).is_err());
    let mut image = solid(4097, 12, 255);
    strokes(&mut image, 20..4077, 4..8, 0);
    // 超宽条不能走单行识别；空检测结果仍可按新质量政策补做一次检测。
    let mut det = Scripted::new(vec![Ok(empty_detection()), Ok(empty_detection())]);
    let mut cls = Scripted::default();
    let mut rec = Scripted::default();
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert!(result.text.is_empty());
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 0, 0));
}

#[test]
fn rec_only_high_confidence_accepts_without_detector_and_keeps_original_image() {
    let image = single_line();
    let saved = image.bgra.clone();
    let mut det = Scripted::default();
    let mut cls = Scripted::new(vec![Ok(orientation(false))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0, 2, 0], 4, 0.99))]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, "中文");
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
    assert_eq!(cls.inputs[0].shape, [1, 3, 80, 160]);
    assert_eq!(rec.inputs[0].shape, [1, 3, 48, 384]);
    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.words.len(), 2);
    assert_eq!(result.words[0].line, 0);
    assert_eq!(result.words[0].rect.x, 8);
    let last = &result.words[1];
    assert_eq!(last.rect.x + last.rect.width, 151);
    assert_eq!(result.lines[0].rect.unwrap().y, 10);
    assert_eq!(result.lines[0].rect.unwrap().height, 18);
    assert_geometry(&result, &image);
    assert_eq!(image.bgra, saved);
}

#[test]
fn rec_only_classifier_180_reverses_sampling_and_word_positions_together() {
    let image = single_line();
    let saved = image.bgra.clone();
    let mut inputs = Vec::new();
    let mut results = Vec::new();
    for rotated in [false, true] {
        let mut det = Scripted::default();
        let mut cls = Scripted::new(vec![Ok(orientation(rotated))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0, 2, 0], 4, 1.))]);
        results.push(recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap());
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
        inputs.push(rec.inputs.pop().unwrap());
    }
    // The 143x18 crop has 382 content columns and two zero-padding columns.
    // Classification reverses the content, not the model's right-hand padding.
    let (content_width, padded_width) = (382, 384);
    let plane = 48 * padded_width;
    for channel in 0..3 {
        for y in 0..48 {
            for x in 0..content_width {
                let upright = inputs[0].values[channel * plane + y * padded_width + x];
                let rotated = inputs[1].values
                    [channel * plane + (47 - y) * padded_width + content_width - 1 - x];
                assert!((upright - rotated).abs() < 1e-4);
            }
            for x in content_width..padded_width {
                for input in &inputs {
                    assert_eq!(input.values[channel * plane + y * padded_width + x], 0.);
                }
            }
        }
    }
    assert_eq!(results[0].text, results[1].text);
    assert_eq!(
        results[0].lines[0].horizontal_direction,
        HorizontalDirection::LeftToRight
    );
    assert_eq!(
        results[1].lines[0].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert!(results[0].words[0].rect.x < results[0].words[1].rect.x);
    assert!(results[1].words[0].rect.x > results[1].words[1].rect.x);
    for result in &results {
        assert_geometry(result, &image);
    }
    assert_eq!(image.bgra, saved);
}

#[test]
fn rec_only_tight_crop_keeps_large_margins_and_180_word_coordinates() {
    let mut image = solid(380, 48, 255);
    strokes(&mut image, 40..280, 14..34, 0);
    let saved = image.bgra.clone();
    let mut rotated_image = solid(380, 48, 255);
    rotated_image.bgra = image
        .bgra
        .chunks_exact(4)
        .rev()
        .flatten()
        .copied()
        .collect();
    let rotated_saved = rotated_image.bgra.clone();
    let dictionary = vec!["RUSTSHOT".into(), "OCR".into(), "2026".into()];
    let mut results = Vec::new();
    for (source, rotated, expected) in [
        (
            &image,
            false,
            Rect {
                x: 38,
                y: 12,
                width: 239,
                height: 24,
            },
        ),
        (
            &rotated_image,
            true,
            Rect {
                x: 103,
                y: 12,
                width: 239,
                height: 24,
            },
        ),
    ] {
        let mut det = Scripted::default();
        let mut cls = Scripted::new(vec![Ok(orientation(rotated))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0, 4, 0, 2, 0, 4, 0, 3, 0], 5, 1.))]);
        let result = recognize(source, &mut det, &mut cls, &mut rec, &dictionary).unwrap();
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
        assert_eq!(rec.inputs[0].shape, [1, 3, 48, 480]);
        assert_eq!(result.text, "RUSTSHOT OCR 2026");
        assert_eq!(result.lines[0].rect, Some(expected));
        assert_eq!(result.words.len(), 3);
        assert_geometry(&result, source);
        for word in &result.words {
            assert!(word.rect.x >= expected.x);
            assert!(word.rect.x + word.rect.width <= expected.x + expected.width);
            assert_eq!((word.rect.y, word.rect.height), (12, 24));
        }
        results.push(result);
    }
    // The final fragment ends at the tight crop, rather than the 380px source edge.
    assert_eq!(
        results[0].words[2].rect,
        Rect {
            x: 212,
            y: 12,
            width: 65,
            height: 24,
        }
    );
    for (upright, rotated) in results[0].words.iter().zip(&results[1].words) {
        assert_eq!(upright.text, rotated.text);
        assert_eq!(rotated.rect.x, 380 - upright.rect.x - upright.rect.width);
        assert_eq!(rotated.rect.width, upright.rect.width);
        assert_eq!(
            upright.horizontal_direction,
            HorizontalDirection::LeftToRight
        );
        assert_eq!(
            rotated.horizontal_direction,
            HorizontalDirection::RightToLeft
        );
    }
    assert_eq!(image.bgra, saved);
    assert_eq!(rotated_image.bgra, rotated_saved);
}

#[test]
fn rec_only_tight_crop_keeps_one_level_alpha_and_equal_luma_tail_strokes() {
    let mut one_level = single_line();
    let mut alpha = single_line();
    let mut equal_luma = solid(160, 40, 128);
    strokes(&mut equal_luma, 10..150, 12..26, 0);
    for y in 12..26 {
        for x in 155..158 {
            let index = (y * 160 + x) * 4;
            one_level.bgra[index..index + 4].copy_from_slice(&[254, 254, 254, 255]);
            alpha.bgra[index..index + 4].copy_from_slice(&[0, 0, 0, 1]);
            // RGB=(188,98,128) differs by only 0.3 in the luma edge metric.
            equal_luma.bgra[index..index + 4].copy_from_slice(&[128, 98, 188, 255]);
        }
    }
    for image in [one_level, alpha, equal_luma] {
        let saved = image.bgra.clone();
        let line = rec_only_line(&RgbImage::from_bgra(&image).unwrap()).unwrap();
        assert_eq!(
            line.rect,
            Rect {
                x: 8,
                y: 10,
                width: 152,
                height: 18,
            }
        );
        assert_eq!(line.quad[2], [160., 28.]);
        let mut det = Scripted::default();
        let mut cls = Scripted::new(vec![Ok(orientation(false))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 4, 1.))]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
        assert_eq!(rec.inputs[0].shape, [1, 3, 48, 408]);
        assert_eq!(result.words[0].rect, line.rect);
        assert_geometry(&result, &image);
        assert_eq!(image.bgra, saved);
    }
}

#[test]
fn rec_only_tight_crop_rejects_weak_outer_content_and_unproven_background() {
    let mut one_level_dot = single_line();
    let index = (34 * 160 + 155) * 4;
    one_level_dot.bgra[index..index + 4].copy_from_slice(&[254, 254, 254, 255]);
    let mut equal_luma_dot = solid(160, 40, 128);
    strokes(&mut equal_luma_dot, 10..150, 12..26, 0);
    equal_luma_dot.bgra[index..index + 4].copy_from_slice(&[128, 98, 188, 255]);
    let mut floating_edge = solid(160, 40, 128);
    strokes(&mut floating_edge, 10..150, 12..26, 0);
    for (x, gray) in [(154, 1), (155, 3)] {
        let index = (19 * 160 + x) * 4;
        floating_edge.bgra[index..index + 4].copy_from_slice(&[gray, gray, gray, 128]);
    }
    let rgb = RgbImage::from_bgra(&floating_edge).unwrap();
    let (low, high) = (rgb.pixels[19 * 160 + 154][0], rgb.pixels[19 * 160 + 155][0]);
    // Neither pixel differs by one from the background, but their adjacent edge
    // is detectable. The ink-only crop must not silently discard that edge.
    assert!((low - 128.).abs() < 1. && (high - 128.).abs() < 1.);
    assert!(high - low >= 1.);
    let mut stripe = single_line();
    for y in 12..26 {
        for x in 0..160 {
            let index = (y * 160 + x) * 4;
            if stripe.bgra[index] == 255 {
                stripe.bgra[index..index + 3].fill(220);
            }
        }
    }
    for image in [one_level_dot, equal_luma_dot, floating_edge, stripe] {
        assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_none());
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = Scripted::new(vec![Ok(orientation(false))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 2, 0], 4, 1.))]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, "文");
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 1, 1));
        assert_geometry(&result, &image);
    }
}

#[test]
fn rec_only_tight_crop_rechecks_width_budget_before_speculative_inference() {
    let mut image = solid(8000, 40, 255);
    strokes(&mut image, 20..7980, 12..26, 0);
    assert_eq!(recognition_width(8000, 40).unwrap(), (9600, 9600));
    // The actual stripe support plus 2px gives 7959x18, requiring 21224 columns.
    assert!(recognition_width(7959, 18).is_err());
    assert!(rec_only_line(&RgbImage::from_bgra(&image).unwrap()).is_none());
    let mut det = Scripted::new(vec![Ok(empty_detection()), Ok(empty_detection())]);
    let mut cls = Scripted::default();
    let mut rec = Scripted::default();
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert!(result.text.is_empty());
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (2, 0, 0));
}

#[test]
fn rec_only_keeps_spaces_unicode_fragments_and_line_indices() {
    let image = single_line();
    let dictionary = vec!["A".into(), "中".into(), "e\u{301}".into(), "👩‍💻".into()];
    let mut det = Scripted::default();
    let mut cls = Scripted::new(vec![Ok(orientation(false))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0, 5, 0, 2, 0, 3, 0, 4, 0], 6, 0.99))]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary).unwrap();
    assert_eq!(result.text, "A 中e\u{301}👩‍💻");
    assert_eq!(result.lines[0].text, result.text);
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        ["A", "中", "e\u{301}", "👩‍💻"]
    );
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
    assert!(result.words.iter().all(|word| word.line == 0));
    assert_geometry(&result, &image);
}

#[test]
fn rec_only_quality_boundary_accepts_point_nine_and_lower_scores_fall_back_once() {
    let image = single_line();
    let mut det = Scripted::default();
    let mut cls = Scripted::new(vec![Ok(orientation(false))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 4, 0.9))]);
    assert_eq!(
        recognize(&image, &mut det, &mut cls, &mut rec, &dictionary())
            .unwrap()
            .text,
        "中"
    );
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 1, 1));
    for confidence in [0.899, 0.5, 0.49] {
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = Scripted::new(vec![Ok(orientation(false)), Ok(orientation(false))]);
        let mut rec = Scripted::new(vec![
            Ok(ctc(&[0, 1, 0], 4, confidence)),
            Ok(ctc(&[0, 2, 0], 4, 1.)),
        ]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, "文");
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 2, 2));
        assert_geometry(&result, &image);
    }
}

#[test]
fn rec_only_empty_or_whitespace_output_falls_back_once() {
    let image = single_line();
    for sequence in [&[0, 0, 0][..], &[0, 3, 0][..]] {
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = Scripted::new(vec![Ok(orientation(false)), Ok(orientation(true))]);
        let mut rec = Scripted::new(vec![Ok(ctc(sequence, 4, 1.)), Ok(ctc(&[0, 2, 0], 4, 1.))]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, "文");
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 2, 2));
        assert_eq!(
            result.lines[0].horizontal_direction,
            HorizontalDirection::RightToLeft
        );
        assert_geometry(&result, &image);
    }
}

#[test]
fn rec_only_failed_classifier_or_recognizer_uses_one_ordinary_detector_pass() {
    let image = single_line();
    for classifier_failed in [true, false] {
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = if classifier_failed {
            Scripted::new(vec![Err("直通分类失败".into()), Ok(orientation(false))])
        } else {
            Scripted::new(vec![Ok(orientation(false)), Ok(orientation(false))])
        };
        let mut rec = if classifier_failed {
            Scripted::new(vec![Ok(ctc(&[0, 2, 0], 4, 1.))])
        } else {
            Scripted::new(vec![Err("直通识别失败".into()), Ok(ctc(&[0, 2, 0], 4, 1.))])
        };
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, "文");
        assert_eq!(det.calls(), 1);
        assert_eq!(cls.calls(), 2);
        assert_eq!(rec.calls(), if classifier_failed { 1 } else { 2 });
    }
}

#[test]
fn rec_only_invalid_model_tensor_falls_back_without_reusing_failed_geometry() {
    let image = single_line();
    for classifier_invalid in [true, false] {
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = if classifier_invalid {
            Scripted::new(vec![
                Ok(Tensor {
                    shape: vec![1, 2],
                    values: vec![f32::NAN, 0.],
                }),
                Ok(orientation(true)),
            ])
        } else {
            Scripted::new(vec![Ok(orientation(false)), Ok(orientation(true))])
        };
        let mut rec = if classifier_invalid {
            Scripted::new(vec![Ok(ctc(&[0, 2, 0], 4, 1.))])
        } else {
            Scripted::new(vec![
                Ok(Tensor {
                    shape: vec![1, 3, 5],
                    values: vec![0.; 15],
                }),
                Ok(ctc(&[0, 2, 0], 4, 1.)),
            ])
        };
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, "文");
        assert_eq!(det.calls(), 1);
        assert_eq!(cls.calls(), 2);
        assert_eq!(rec.calls(), if classifier_invalid { 1 } else { 2 });
        assert_eq!(
            result.lines[0].horizontal_direction,
            HorizontalDirection::RightToLeft
        );
        assert_geometry(&result, &image);
    }
}

#[test]
fn rec_only_does_not_hide_errors_from_fallback_detector_classifier_or_recognizer() {
    let image = single_line();
    for stage in 0..3 {
        let expected = ["普通检测失败", "普通分类失败", "普通识别失败"][stage];
        let mut det = Scripted::new(vec![if stage == 0 {
            Err(expected.into())
        } else {
            Ok(detection_map(false))
        }]);
        let mut cls = Scripted::new(vec![
            Ok(orientation(false)),
            if stage == 1 {
                Err(expected.into())
            } else {
                Ok(orientation(false))
            },
        ]);
        let mut rec = Scripted::new(vec![
            Ok(ctc(&[0, 1, 0], 4, 0.7)),
            if stage == 2 {
                Err(expected.into())
            } else {
                Ok(ctc(&[0, 2, 0], 4, 1.))
            },
        ]);
        let error = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary())
            .err()
            .unwrap();
        assert_eq!(error.to_string(), expected);
        assert_eq!(det.calls(), 1);
        assert_eq!(cls.calls(), if stage == 0 { 1 } else { 2 });
        assert_eq!(rec.calls(), if stage == 2 { 2 } else { 1 });
    }
}

#[test]
fn rec_only_multiline_gate_keeps_detection_two_lines_and_word_indices() {
    let mut image = solid(160, 60, 255);
    strokes(&mut image, 10..150, 8..18, 0);
    strokes(&mut image, 10..150, 38..48, 0);
    let saved = image.bgra.clone();
    let mut det = Scripted::new(vec![Ok(detection_map(true))]);
    let mut cls = Scripted::new(vec![Ok(orientation(false)), Ok(orientation(true))]);
    let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 4, 1.)), Ok(ctc(&[0, 2, 0], 4, 1.))]);
    let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
    assert_eq!(result.text, "中\n文");
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 2, 2));
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| word.line)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(
        result.lines[0].horizontal_direction,
        HorizontalDirection::LeftToRight
    );
    assert_eq!(
        result.lines[1].horizontal_direction,
        HorizontalDirection::RightToLeft
    );
    assert_geometry(&result, &image);
    assert_eq!(image.bgra, saved);
}

#[test]
fn rec_only_uniform_empty_detection_does_not_invoke_classifier_or_recognizer() {
    let mut det = Scripted::new(vec![Ok(empty_detection())]);
    let mut cls = Scripted::default();
    let mut rec = Scripted::default();
    let result = recognize(
        &solid(160, 40, 255),
        &mut det,
        &mut cls,
        &mut rec,
        &dictionary(),
    )
    .unwrap();
    assert!(result.text.is_empty());
    assert!(result.regions.is_empty());
    assert!(result.words.is_empty());
    assert!(result.lines.is_empty());
    assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 0, 0));
}

#[test]
fn rec_only_gate_does_not_change_detector_only_route_or_ordinary_confidence() {
    let mut det = Scripted::new(vec![Ok(detection_map(false))]);
    assert!(!detect(&single_line(), &mut det).unwrap().is_empty());
    assert_eq!(det.calls(), 1);
    for confidence in [0.5, 0.49] {
        let image = solid(160, 61, 255);
        let mut det = Scripted::new(vec![Ok(detection_map(false))]);
        let mut cls = Scripted::new(vec![Ok(orientation(false))]);
        let mut rec = Scripted::new(vec![Ok(ctc(&[0, 1, 0], 4, confidence))]);
        let result = recognize(&image, &mut det, &mut cls, &mut rec, &dictionary()).unwrap();
        assert_eq!(result.text, if confidence >= 0.5 { "中" } else { "" });
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (1, 1, 1));
    }
}

#[test]
fn rec_only_rejects_invalid_source_or_dictionary_before_any_inference() {
    let mut source = single_line();
    source.bgra.pop();
    for (image, words) in [
        (source, dictionary()),
        (single_line(), Vec::new()),
        (single_line(), vec![String::new()]),
    ] {
        let mut det = Scripted::default();
        let mut cls = Scripted::default();
        let mut rec = Scripted::default();
        assert!(recognize(&image, &mut det, &mut cls, &mut rec, &words).is_err());
        assert_eq!((det.calls(), cls.calls(), rec.calls()), (0, 0, 0));
    }
}
