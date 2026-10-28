//! 合成裁行／CTC 模型：几何保存与回退，不执行模型或证明墨迹字框准确率。
use super::*;
use crate::recognition::geometry::{Projection, ReadingDirection};

fn source() -> Image {
    Image {
        width: 256,
        height: 256,
        bgra: vec![255; 256 * 256 * 4],
        readback_ms: 0.,
    }
}

fn assert_point(actual: TextPoint, expected: Point) {
    assert!(
        (actual.x - expected[0]).hypot(actual.y - expected[1]) < 1e-3,
        "{actual:?} != {expected:?}"
    );
}

fn assert_quad(actual: TextGeometry, expected: [Point; 4]) {
    for (actual, expected) in actual.quad.into_iter().zip(expected) {
        assert_point(actual, expected);
    }
}

struct Scripted(VecDeque<Tensor>);
impl ModelRunner for Scripted {
    fn run(&mut self, _: Tensor) -> Result<Tensor> {
        self.0.pop_front().ok_or_else(|| "未预期额外推理".into())
    }
}

fn ctc(sequence: &[usize]) -> Tensor {
    let mut values = vec![0.; sequence.len() * 4];
    for (step, &index) in sequence.iter().enumerate() {
        values[step * 4 + index] = 0.99;
    }
    Tensor {
        shape: vec![1, sequence.len(), 4],
        values,
    }
}

fn result_for(quad: [Point; 4], rotated: bool, sequence: &[usize]) -> RecognizedText {
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let detected = DetectedLine {
        quad,
        confidence: 0.99,
        rect: bounds(&quad, rgb.width, rgb.height).unwrap(),
    };
    let mut cls = Scripted(VecDeque::from([Tensor {
        shape: vec![1, 2],
        values: if rotated {
            vec![0.01, 0.99]
        } else {
            vec![0.99, 0.01]
        },
    }]));
    let mut rec = Scripted(VecDeque::from([ctc(sequence)]));
    let candidate = recognize_lines(
        &rgb,
        &[detected],
        &mut cls,
        &mut rec,
        &["甲".into(), "乙".into()],
    )
    .unwrap();
    assert!(cls.0.is_empty());
    assert!(rec.0.is_empty());
    candidate.result
}

#[test]
fn horizontal_full_crop_preserves_reading_order_after_180() {
    let quad = [[10., 20.], [150., 20.], [150., 45.], [10., 45.]];
    let mut crop = Crop::new(quad).unwrap();
    let upright = crop.text_geometry(0., 1.).unwrap();
    assert_quad(upright, quad);
    assert_eq!(
        Projection::new(upright).unwrap().direction(),
        ReadingDirection::LeftToRight
    );
    crop.rotated_180 = true;
    let reversed = crop.text_geometry(0., 1.).unwrap();
    assert_quad(reversed, [quad[2], quad[3], quad[0], quad[1]]);
    assert_eq!(
        Projection::new(reversed).unwrap().direction(),
        ReadingDirection::RightToLeft
    );
}

#[test]
fn vertical_full_crop_tracks_top_to_bottom_and_bottom_to_top() {
    let quad = [[20., 10.], [45., 10.], [45., 150.], [20., 150.]];
    let mut crop = Crop::new(quad).unwrap();
    let upright = crop.text_geometry(0., 1.).unwrap();
    assert_quad(upright, [quad[1], quad[2], quad[3], quad[0]]);
    let projection = Projection::new(upright).unwrap();
    assert_eq!(projection.direction(), ReadingDirection::TopToBottom);
    assert_point(projection.point(0., 0.5), [32.5, 10.]);
    assert_point(projection.point(1., 0.5), [32.5, 150.]);
    crop.rotated_180 = true;
    let reversed = crop.text_geometry(0., 1.).unwrap();
    assert_quad(reversed, [quad[3], quad[0], quad[1], quad[2]]);
    let projection = Projection::new(reversed).unwrap();
    assert_eq!(projection.direction(), ReadingDirection::BottomToTop);
    assert_point(projection.point(0., 0.5), [32.5, 150.]);
    assert_point(projection.point(1., 0.5), [32.5, 10.]);
}

#[test]
fn tilted_fragment_keeps_crop_normal_sides_instead_of_axis_aligned_rect() {
    let crop = Crop::new([[20., 10.], [160., 70.], [150., 95.], [10., 35.]]).unwrap();
    let geometry = crop.text_geometry(0.25, 0.75).unwrap();
    assert_quad(geometry, [[55., 25.], [125., 55.], [115., 80.], [45., 50.]]);
    let projection = Projection::new(geometry).unwrap();
    assert_point(projection.point(0.5, 0.5), [85., 52.5]);
    assert_eq!(projection.direction(), ReadingDirection::LeftToRight);
    assert_ne!(geometry.quad[0].y, geometry.quad[1].y);
}

#[test]
fn perspective_fragment_projection_composes_with_original_crop_transform() {
    let crop = Crop::new([[10., 10.], [180., 30.], [140., 100.], [25., 70.]]).unwrap();
    let geometry = crop.text_geometry(0.2, 0.8).unwrap();
    let projection = Projection::new(geometry).unwrap();
    for [u, v] in [[0., 0.], [1., 1.], [0.1, 0.9], [0.5, 0.5], [0.9, 0.2]] {
        let expected = crop.point(0.2 + 0.6 * u, v);
        let actual = projection.point(f64::from(u), f64::from(v));
        assert_point(actual, expected);
        let inverse = projection.inverse(actual).unwrap();
        assert!((inverse[0] - f64::from(u)).abs() < 1e-5);
        assert!((inverse[1] - f64::from(v)).abs() < 1e-5);
    }
}

#[test]
fn perspective_180_fragment_uses_the_same_transform_as_corrected_sampling() {
    let mut crop = Crop::new([[10., 10.], [180., 30.], [140., 100.], [25., 70.]]).unwrap();
    let expected = [
        crop.point(0.8, 1.),
        crop.point(0.2, 1.),
        crop.point(0.2, 0.),
        crop.point(0.8, 0.),
    ];
    crop.rotated_180 = true;
    let geometry = crop.text_geometry(0.2, 0.8).unwrap();
    assert_quad(geometry, expected);
    let projection = Projection::new(geometry).unwrap();
    assert_eq!(projection.direction(), ReadingDirection::RightToLeft);
    for [u, v] in [[0., 0.], [1., 1.], [0.35, 0.8]] {
        assert_point(
            projection.point(f64::from(u), f64::from(v)),
            crop.point(0.2 + 0.6 * u, v),
        );
    }
}

#[test]
fn emitted_line_and_ctc_fragments_retain_horizontal_vertical_and_slanted_geometry() {
    for quad in [
        [[10., 10.], [150., 10.], [150., 35.], [10., 35.]],
        [[10., 10.], [35., 10.], [35., 150.], [10., 150.]],
        [[20., 10.], [160., 70.], [150., 95.], [10., 35.]],
        [[10., 10.], [180., 30.], [140., 100.], [25., 70.]],
    ] {
        for rotated in [false, true] {
            let result = result_for(quad, rotated, &[1, 0, 2, 0]);
            assert_eq!(result.text, "甲乙");
            assert_eq!(result.lines.len(), 1);
            assert_eq!(result.words.len(), 2);
            let mut crop = Crop::new(quad).unwrap();
            crop.rotated_180 = rotated;
            assert_eq!(result.lines[0].geometry, crop.text_geometry(0., 1.));
            assert!(result.lines[0]
                .geometry
                .unwrap()
                .validated_in(result.lines[0].rect.unwrap())
                .is_some());
            let (content_width, tensor_width) = recognition_width(crop.width, crop.height).unwrap();
            let decoded = decode_ctc(ctc(&[1, 0, 2, 0]), &["甲".into(), "乙".into()]).unwrap();
            for (word, fragment) in result.words.iter().zip(fragments(&decoded.tokens)) {
                let start = (fragment.start as f32 / 4. * tensor_width as f32
                    / content_width as f32)
                    .clamp(0., 1.);
                let end = (fragment.end as f32 / 4. * tensor_width as f32 / content_width as f32)
                    .clamp(start, 1.);
                assert_eq!(word.geometry, crop.text_geometry(start, end));
                assert_eq!(word.line, 0);
                let geometry = word.geometry.unwrap();
                assert!(geometry.validated_in(word.rect).is_some());
                assert!(Projection::new(geometry).is_some());
            }
        }
    }
}

#[test]
fn emitted_geometry_is_image_local_and_a_roi_translation_moves_every_corner_once() {
    let local = [[5., 5.], [105., 15.], [95., 40.], [5., 35.]];
    let translated = local.map(|[x, y]| [x + 40., y + 120.]);
    let local_result = result_for(local, true, &[1, 0, 2, 0]);
    let translated_result = result_for(translated, true, &[1, 0, 2, 0]);
    let geometries = |result: &RecognizedText| {
        std::iter::once(result.lines[0].geometry.unwrap())
            .chain(result.words.iter().map(|word| word.geometry.unwrap()))
            .collect::<Vec<_>>()
    };
    for (local, translated) in geometries(&local_result)
        .into_iter()
        .zip(geometries(&translated_result))
    {
        for (local, translated) in local.quad.into_iter().zip(translated.quad) {
            assert_point(translated, [local.x + 40., local.y + 120.]);
        }
    }
    assert_eq!(local_result.text, translated_result.text);
}

#[test]
fn full_frame_corners_are_valid_but_outside_frame_geometry_falls_back_without_clipping_quad() {
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let crop = Crop::new([[0., 0.], [256., 0.], [256., 60.], [0., 60.]]).unwrap();
    assert!(geometry_within_image(
        crop.text_geometry(0., 1.).unwrap(),
        &rgb
    ));
    let outside = [[-10., 20.], [150., 20.], [150., 45.], [-10., 45.]];
    let geometry = Crop::new(outside).unwrap().text_geometry(0., 1.).unwrap();
    assert!(!geometry_within_image(geometry, &rgb));
    let result = result_for(outside, false, &[1, 1, 1, 1]);
    assert_eq!(result.text, "甲");
    assert!(result.lines[0].geometry.is_none());
    assert!(result.words[0].geometry.is_none());
    assert!(result.words[0].rect.valid(256, 256));
    assert_eq!(result.words[0].rect.x, 0);
}

#[test]
fn invalid_fragment_intervals_and_non_finite_transforms_do_not_emit_geometry() {
    let crop = Crop::new([[10., 10.], [150., 10.], [150., 35.], [10., 35.]]).unwrap();
    for (start, end) in [
        (-0.1, 0.8),
        (0.2, 1.1),
        (0.5, 0.5),
        (0.8, 0.2),
        (f32::NAN, 0.5),
        (0.5, f32::INFINITY),
    ] {
        assert!(crop.text_geometry(start, end).is_none());
    }
    let invalid = Crop {
        transform: [f64::INFINITY; 8],
        ..crop
    };
    assert!(invalid.text_geometry(0., 1.).is_none());
}

#[test]
fn whitespace_keeps_source_text_without_creating_projected_word_evidence() {
    // Explicit dictionary-space class is 3. Interior whitespace survives in the
    // line text, while only the surrounding CTC fragments have estimated bands.
    let quad = [[10., 10.], [150., 10.], [150., 35.], [10., 35.]];
    let result = result_for(quad, false, &[1, 0, 3, 0, 2, 0]);
    assert_eq!(result.text, "甲 乙");
    assert_eq!(result.lines[0].text, "甲 乙");
    assert_eq!(result.words.len(), 2);
    assert_eq!(result.words[0].text, "甲");
    assert_eq!(result.words[1].text, "乙");
    assert!(result.words.iter().all(|word| word.geometry.is_some()));
    assert_eq!(
        result.lines[0].geometry,
        Crop::new(quad).unwrap().text_geometry(0., 1.)
    );
}

#[test]
fn recovery_merge_keeps_original_and_recovered_projected_geometry_after_reindexing() {
    let primary_quad = [[10., 160.], [150., 170.], [145., 195.], [10., 185.]];
    let recovered_quad = [[20., 10.], [45., 10.], [45., 140.], [20., 140.]];
    let primary = result_for(primary_quad, true, &[1, 0, 2, 0]);
    let recovered = result_for(recovered_quad, false, &[2, 0, 1, 0]);
    let primary_line = primary.lines[0].geometry;
    let recovered_line = recovered.lines[0].geometry;
    let primary_words: Vec<_> = primary.words.iter().map(|word| word.geometry).collect();
    let recovered_words: Vec<_> = recovered.words.iter().map(|word| word.geometry).collect();
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let merged = merge_quality_recovery(&primary, recovered, &rgb).unwrap();
    assert_eq!(merged.text, "乙甲\n甲乙");
    assert_eq!(merged.lines[0].geometry, recovered_line);
    assert_eq!(merged.lines[1].geometry, primary_line);
    assert_eq!(
        merged
            .words
            .iter()
            .map(|word| word.geometry)
            .collect::<Vec<_>>(),
        recovered_words
            .into_iter()
            .chain(primary_words)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        merged
            .words
            .iter()
            .map(|word| word.line)
            .collect::<Vec<_>>(),
        [0, 0, 1, 1]
    );
}

#[test]
fn recovery_merge_keeps_missing_geometry_as_explicit_fallback() {
    let mut primary = result_for(
        [[10., 160.], [150., 160.], [150., 185.], [10., 185.]],
        false,
        &[1, 1, 1, 1],
    );
    primary.lines[0].geometry = None;
    primary.words[0].geometry = None;
    let recovered = result_for(
        [[10., 10.], [150., 10.], [150., 35.], [10., 35.]],
        true,
        &[2, 2, 2, 2],
    );
    let rgb = RgbImage::from_bgra(&source()).unwrap();
    let merged = merge_quality_recovery(&primary, recovered, &rgb).unwrap();
    assert!(merged.lines[0].geometry.is_some());
    assert!(merged.words[0].geometry.is_some());
    assert!(merged.lines[1].geometry.is_none());
    assert!(merged.words[1].geometry.is_none());
}
