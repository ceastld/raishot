//! 独立补强定义：真实 CTC 发射→Ink 最终化→共享选字消费。
//! 只使用脚本张量和自有像素，不执行 ORT、GUI 或剪贴板，不复制产品算法。
use super::*;
use crate::{
    annotation::Point as ImagePoint,
    pin_text_selection::SelectionText,
    recognition::{geometry::Projection, graphemes::GraphemeBand},
};

struct Scripted {
    outputs: VecDeque<Tensor>,
    calls: usize,
}
impl ModelRunner for Scripted {
    fn run(&mut self, _: Tensor) -> Result<Tensor> {
        self.calls += 1;
        self.outputs
            .pop_front()
            .ok_or_else(|| "补强模型出现额外推理".into())
    }
}

fn source() -> Image {
    let mut image = Image {
        width: 256,
        height: 256,
        bgra: vec![255; 256 * 256 * 4],
        readback_ms: 0.,
    };
    // 沿实际裁行中线对称的明确像素证据，不声称这是某种真实字体。
    for y in 46..54 {
        for x in 35..135 {
            image.bgra[(y * 256 + x) * 4..(y * 256 + x) * 4 + 3].fill(0);
        }
    }
    image
}

fn recognition_tensor() -> Tensor {
    let classes = 4; // blank、独立 A、独立 B、space。
    let sequence = [0, 1, 0, 0, 0, 0, 2, 2, 2, 0];
    let mut values = vec![0.; sequence.len() * classes];
    for (step, index) in sequence.into_iter().enumerate() {
        values[step * classes + index] = 0.99;
    }
    Tensor {
        shape: vec![1, 10, classes],
        values,
    }
}

fn sampling_bands(result: &RecognizedText) -> Vec<GraphemeBand> {
    let line = &result.lines[0];
    let words: Vec<_> = result.words.iter().filter(|word| word.line == 0).collect();
    line.graphemes
        .as_ref()
        .expect("A/B 独立发射应有完整字素来源")
        .validated_bands(0, line, &words)
        .expect("真实行和词仍匹配发射来源")
}

#[test]
fn raw_ctc_survives_real_ink_and_selection_for_upright_and_180() {
    for rotated in [false, true] {
        let image = source();
        let original_bytes = image.bgra.clone();
        let rgb = RgbImage::from_bgra(&image).unwrap();
        let mut cls = Scripted {
            outputs: vec![Tensor {
                shape: vec![1, 2],
                values: if rotated {
                    vec![0.01, 0.99]
                } else {
                    vec![0.99, 0.01]
                },
            }]
            .into(),
            calls: 0,
        };
        let mut rec = Scripted {
            outputs: vec![recognition_tensor()].into(),
            calls: 0,
        };
        let detected = DetectedLine {
            quad: [[20., 40.], [150., 40.], [150., 60.], [20., 60.]],
            confidence: 0.99,
            rect: Rect {
                x: 20,
                y: 40,
                width: 130,
                height: 20,
            },
        };
        let candidate = recognize_lines(
            &rgb,
            &[detected],
            &mut cls,
            &mut rec,
            &["A".into(), "B".into()],
        )
        .unwrap();
        assert_eq!((cls.calls, rec.calls), (1, 1));
        assert!(cls.outputs.is_empty() && rec.outputs.is_empty());
        let raw = candidate.result;
        let original_sampling = raw.lines[0].graphemes.clone().unwrap();
        let original_bands = sampling_bands(&raw);
        assert_eq!(
            original_bands
                .iter()
                .map(|band| (band.source_range.clone(), band.start, band.end))
                .collect::<Vec<_>>(),
            vec![(0..1, 0.1, 0.2), (1..2, 0.6, 0.9)]
        );
        let raw_word = raw.words[0].geometry.unwrap();
        let original_line = raw.lines[0].geometry;
        let original_rect = raw.words[0].rect;
        let before = SelectionText::from_recognized(&raw);
        let finished = finish_recognition(&rgb, raw, None).unwrap();
        assert_eq!(finished.text, "AB");
        assert_eq!(finished.lines[0].geometry, original_line);
        assert_eq!(finished.words[0].rect, original_rect);
        assert_eq!(finished.regions, vec![original_rect]);
        let sampling = finished.lines[0].graphemes.as_ref().unwrap();
        assert_eq!(
            sampling.source_geometry(),
            original_sampling.source_geometry()
        );
        assert_ne!(
            sampling.selection_geometry(),
            original_sampling.selection_geometry()
        );
        assert_eq!(sampling_bands(&finished), original_bands);
        let word = finished.words[0].geometry.unwrap();
        assert_ne!(word, raw_word, "真实最终化夹具应有可收紧的墨迹证据");
        let line_projection = Projection::new(original_line.unwrap()).unwrap();
        assert_eq!(
            line_projection.direction(),
            if rotated {
                crate::recognition::geometry::ReadingDirection::RightToLeft
            } else {
                crate::recognition::geometry::ReadingDirection::LeftToRight
            }
        );
        let after = SelectionText::from_recognized(&finished);
        // 带中心 0.15／0.75 的中线 0.45 保持；整行统一收紧 v，不改字间分界。
        let reading = sampling.selection_reading_range();
        for (range, start, end) in [(0..1, reading[0], 0.45), (1..2, 0.45, reading[1])] {
            let raw_quad = before.marking_quads(range.clone())[0];
            let current_quad = after.marking_quads(range)[0];
            assert_ne!(current_quad, raw_quad);
            for (edge, u) in [(0, start), (1, end)] {
                let center = line_projection.point(u, 0.5);
                let middle = ImagePoint {
                    x: (current_quad[edge].x + current_quad[3 - edge].x) * 0.5,
                    y: (current_quad[edge].y + current_quad[3 - edge].y) * 0.5,
                };
                assert!((middle.x - center.x).abs() <= 0.001);
                assert!((middle.y - center.y).abs() <= 0.001);
                let local = line_projection.inverse(current_quad[edge]).unwrap();
                assert!((local[0] - u).abs() <= 0.0001);
                assert!(local[1] > 0.01 && local[1] < 0.5);
            }
        }
        assert!(after.has_text_at(line_projection.point(0.15, 0.5)));
        assert!(after.has_text_at(line_projection.point(0.4, 0.5)));
        assert!(after.caret_segment(1).is_some(), "相邻单元共用中线停点");
        assert_eq!(after.selected(0..2).as_deref(), Some("AB"));
        assert_eq!(image.bgra, original_bytes);
    }
}
