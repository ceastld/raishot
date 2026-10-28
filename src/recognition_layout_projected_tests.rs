//! 投影方向下的段落复制：垂直、180°、明显倾斜与透视保留原换行。
use super::*;
use crate::{
    annotation::Point,
    recognition::{RecognizedLine, RecognizedWord},
};

fn rect(index: usize) -> Rect {
    Rect {
        x: 20,
        y: 20 + index as u32 * 26,
        width: 200,
        height: 20,
    }
}
fn axis(r: Rect) -> TextGeometry {
    let (x, y, w, h) = (r.x as f32, r.y as f32, r.width as f32, r.height as f32);
    TextGeometry {
        quad: [
            Point { x, y },
            Point { x: x + w, y },
            Point { x: x + w, y: y + h },
            Point { x, y: y + h },
        ],
    }
}
fn source() -> RecognizedText {
    let rows = ["first readable text", "second readable text"];
    RecognizedText {
        text: rows.join("\n"),
        regions: vec![rect(0), rect(1)],
        words: Vec::new(),
        lines: rows
            .iter()
            .enumerate()
            .map(|(index, text)| RecognizedLine {
                graphemes: None,
                text: (*text).into(),
                rect: Some(rect(index)),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            })
            .collect(),
    }
}
fn copy(result: &RecognizedText, mode: CopyLayoutMode) -> String {
    OcrCopyLayout::new(result)
        .copy(0..result.text.len(), mode)
        .unwrap()
}
fn unsafe_geometries(r: Rect) -> Vec<TextGeometry> {
    let q = axis(r).quad;
    // 明显错切（约 22°）；检测框 1px 级的倾斜噪声由段落判定容忍。
    let mut slant = q;
    slant[0].x += 6.;
    slant[1].y += 5.;
    slant[2].x -= 6.;
    slant[3].y -= 5.;
    let mut perspective = q;
    perspective[2].x -= 5.;
    perspective[3].x += 5.;
    vec![
        TextGeometry {
            quad: [q[1], q[2], q[3], q[0]],
        }, // 向下
        TextGeometry {
            quad: [q[3], q[0], q[1], q[2]],
        }, // 向上
        TextGeometry {
            quad: [q[2], q[3], q[0], q[1]],
        }, // 180°
        TextGeometry { quad: slant },
        TextGeometry { quad: perspective },
    ]
}

#[test]
fn legacy_missing_projection_keeps_existing_auto_join_contract() {
    let result = source();
    assert_eq!(
        copy(&result, CopyLayoutMode::Auto),
        "first readable text second readable text"
    );
    assert_eq!(copy(&result, CopyLayoutMode::Original), result.text);
}

#[test]
fn forward_axis_aligned_projection_still_allows_proven_continuation() {
    let mut result = source();
    for (index, line) in result.lines.iter_mut().enumerate() {
        line.geometry = Some(axis(rect(index)));
    }
    assert_eq!(
        copy(&result, CopyLayoutMode::Auto),
        "first readable text second readable text"
    );
}

#[test]
fn vertical_slanted_perspective_and_half_turn_line_geometry_keep_source_separator() {
    for geometry in unsafe_geometries(rect(0)) {
        assert!(geometry.validated_in(rect(0)).is_some());
        let mut result = source();
        result.lines[0].geometry = Some(geometry);
        // 旧水平字段刻意保持 LTR，避免错误字段掩盖本次新投影 guard。
        assert_eq!(copy(&result, CopyLayoutMode::Auto), result.text);
        assert_eq!(copy(&result, CopyLayoutMode::Original), result.text);
    }
}

#[test]
fn unsafe_word_projection_prevents_join_even_when_line_projection_is_forward() {
    for geometry in unsafe_geometries(rect(0)) {
        let mut result = source();
        result.lines[0].geometry = Some(axis(rect(0)));
        result.words.push(RecognizedWord {
            text: result.lines[0].text.clone(),
            rect: rect(0),
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(geometry),
        });
        assert_eq!(copy(&result, CopyLayoutMode::Auto), result.text);
    }
}

#[test]
fn invalid_or_outside_projected_evidence_preserves_original_newline() {
    let mut nonfinite = axis(rect(0));
    nonfinite.quad[0].x = f32::NAN;
    let mut outside = axis(rect(0));
    outside.quad[1].x += 1.;
    let mut crossing = axis(rect(0));
    crossing.quad.swap(1, 3);
    for geometry in [nonfinite, outside, crossing] {
        let mut result = source();
        result.lines[0].geometry = Some(geometry);
        assert_eq!(copy(&result, CopyLayoutMode::Auto), result.text);
    }
}

#[test]
fn projected_copy_keeps_graphemes_spaces_and_crlf_bytes_in_selected_source_range() {
    let mut result = source();
    result.text = "  中e\u{301} 👩‍💻 \r\nsecond readable text".into();
    result.lines[0].text = "  中e\u{301} 👩‍💻 ".into();
    result.lines[0].geometry = Some(unsafe_geometries(rect(0))[0]);
    let layout = OcrCopyLayout::new(&result);
    let start = result.text.find('中').unwrap();
    let end = result.text.find("second").unwrap() + "second".len();
    let copied = layout
        .copy_mapped(start..end, CopyLayoutMode::Auto)
        .unwrap();
    assert_eq!(copied.text, result.text[start..end]);
    assert!(copied
        .spans
        .iter()
        .all(|span| span.kind == CopySpanKind::Verbatim));
    assert!(layout.copy(start + 1..end, CopyLayoutMode::Auto).is_err());
}
