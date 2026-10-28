//! 合成真实 DTO 和共享取消槽；不推理、不启动窗口、不操作系统剪贴板。
use super::*;
use crate::recognition::{
    geometry::ReadingDirection, HorizontalDirection, RecognizedLine, RecognizedWord,
};
use std::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

fn rect_geometry(rect: Rect) -> TextGeometry {
    TextGeometry {
        quad: [
            Point {
                x: rect.x as f32,
                y: rect.y as f32,
            },
            Point {
                x: (rect.x + rect.width) as f32,
                y: rect.y as f32,
            },
            Point {
                x: (rect.x + rect.width) as f32,
                y: (rect.y + rect.height) as f32,
            },
            Point {
                x: rect.x as f32,
                y: (rect.y + rect.height) as f32,
            },
        ],
    }
}

fn row(text: &str, rect: Rect, geometry: TextGeometry) -> RecognizedText {
    let direction = if Projection::new(geometry).unwrap().point(1., 0.5).x
        < Projection::new(geometry).unwrap().point(0., 0.5).x
    {
        HorizontalDirection::RightToLeft
    } else {
        HorizontalDirection::LeftToRight
    };
    RecognizedText {
        text: text.into(),
        regions: vec![rect],
        words: vec![RecognizedWord {
            text: text.into(),
            rect,
            line: 0,
            horizontal_direction: direction,
            geometry: Some(geometry),
        }],
        lines: vec![RecognizedLine {
            text: text.into(),
            rect: Some(rect),
            horizontal_direction: direction,
            geometry: Some(geometry),
            graphemes: None,
        }],
    }
}

fn recognized() -> RecognizedText {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    row("first", rect, rect_geometry(rect))
}

fn frame() -> SourceFrame {
    SourceFrame {
        width: 256,
        height: 256,
        origin: (-1920, -100),
    }
}

fn binding() -> Binding<u64> {
    let body_identity = Arc::new(());
    Binding {
        source: 41,
        source_identity: Arc::new(()),
        result: Arc::new(recognized()),
        result_generation: 7,
        installed_body_identity: body_identity.clone(),
        body_identity,
        installed_body_revision: 0,
        body_revision: 0,
        frame: frame(),
        provider: OcrProvider::Ppocr,
    }
}

fn output(text: &str) -> RecognizedText {
    RecognizedText {
        text: text.into(),
        regions: vec![],
        words: vec![],
        lines: vec![],
    }
}

fn two_words(first: (Rect, TextGeometry), second: (Rect, TextGeometry)) -> RecognizedText {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    let mut result = row("A A", rect, rect_geometry(rect));
    result.words = [first, second]
        .into_iter()
        .map(|(rect, geometry)| RecognizedWord {
            text: "A".into(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(geometry),
        })
        .collect();
    result
}

fn request(session: &mut LocalRetry<u64>, binding: &Binding<u64>) -> LocalRequest<u64> {
    session
        .begin(binding, &binding.result.text, 1..3, 2, true)
        .unwrap()
}

fn complete(
    session: &mut LocalRetry<u64>,
    request: &LocalRequest<u64>,
    binding: &Binding<u64>,
    text: &str,
) -> Completion {
    session.complete(
        request,
        binding,
        &binding.result.text,
        true,
        Receipt::Completed(output(text)),
    )
}

#[test]
fn local_target_expands_partial_bytes_to_full_crop_not_word_union_or_character_box() {
    let mut result = recognized();
    let word_rect = Rect {
        x: 30,
        y: 35,
        width: 60,
        height: 10,
    };
    result.lines[0].rect = Some(word_rect);
    result.words[0].rect = word_rect;
    result.regions[0] = word_rect;
    result.words[0].geometry = Some(rect_geometry(word_rect));
    let geometry = result.lines[0].geometry.unwrap();
    assert!(
        geometry.validated_in(word_rect).is_none(),
        "真实完整裁行可大于词框 union"
    );
    let target = LocalTarget::resolve(&result, 1..3, frame(), 2).unwrap();
    assert_eq!(target.selected(), 1..3);
    assert_eq!(target.line_range(), 0..5);
    assert_eq!(target.line_index(), 0);
    assert_eq!(target.original_line(), "first");
    assert_eq!(
        target.roi(),
        Rect {
            x: 18,
            y: 28,
            width: 104,
            height: 24
        }
    );
    assert_eq!(target.geometry(), geometry);
    assert_eq!(target.global_roi(), (-1902, -72, 104, 24));
    assert_eq!(target.global_geometry()[0], [-1900., -70.]);
    let positive = LocalTarget::resolve(
        &result,
        1..3,
        SourceFrame {
            origin: (2560, 100),
            ..frame()
        },
        2,
    )
    .unwrap();
    assert_eq!(positive.global_roi(), (2578, 128, 104, 24));
    assert_eq!(result.text, "first");
}

#[test]
fn local_target_real_projection_preserves_180_vertical_tilt_and_perspective() {
    for (rect, quad, direction) in [
        (
            Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 20,
            },
            [
                Point { x: 120., y: 50. },
                Point { x: 20., y: 50. },
                Point { x: 20., y: 30. },
                Point { x: 120., y: 30. },
            ],
            ReadingDirection::RightToLeft,
        ),
        (
            Rect {
                x: 20,
                y: 30,
                width: 20,
                height: 100,
            },
            [
                Point { x: 40., y: 30. },
                Point { x: 40., y: 130. },
                Point { x: 20., y: 130. },
                Point { x: 20., y: 30. },
            ],
            ReadingDirection::TopToBottom,
        ),
        (
            Rect {
                x: 20,
                y: 30,
                width: 20,
                height: 100,
            },
            [
                Point { x: 20., y: 130. },
                Point { x: 20., y: 30. },
                Point { x: 40., y: 30. },
                Point { x: 40., y: 130. },
            ],
            ReadingDirection::BottomToTop,
        ),
        (
            Rect {
                x: 20,
                y: 20,
                width: 120,
                height: 40,
            },
            [
                Point { x: 30., y: 20. },
                Point { x: 140., y: 40. },
                Point { x: 130., y: 60. },
                Point { x: 20., y: 40. },
            ],
            ReadingDirection::LeftToRight,
        ),
        (
            Rect {
                x: 20,
                y: 15,
                width: 110,
                height: 55,
            },
            [
                Point { x: 20., y: 20. },
                Point { x: 120., y: 15. },
                Point { x: 130., y: 60. },
                Point { x: 25., y: 70. },
            ],
            ReadingDirection::LeftToRight,
        ),
    ] {
        let geometry = TextGeometry { quad };
        let result = row("first", rect, geometry);
        let target = LocalTarget::resolve(&result, 0..5, frame(), 4).unwrap();
        let source = Projection::new(geometry).unwrap();
        let local = Projection::new(target.roi_geometry()).unwrap();
        assert_eq!(source.direction(), direction);
        assert_eq!(local.direction(), direction);
        assert_eq!(target.geometry().quad, quad);
        for (u, v) in [(0., 0.), (1., 0.), (1., 1.), (0., 1.), (0.23, 0.67)] {
            let mapped = target.roi_to_source(local.point(u, v)).unwrap();
            let expected = source.point(u, v);
            assert!((mapped.x - expected.x).abs() < 0.0001);
            assert!((mapped.y - expected.y).abs() < 0.0001);
        }
        assert!(target.roi_to_source(Point { x: -0.1, y: 0. }).is_none());
        assert!(target.roi_to_source(Point { x: f32::NAN, y: 0. }).is_none());
    }
}

#[test]
fn local_target_crlf_and_unicode_require_complete_grapheme_endpoints() {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    let mut result = row("e\u{301}🙂", rect, rect_geometry(rect));
    result.text.push_str("\r\nlast");
    let second = Rect {
        x: 20,
        y: 80,
        width: 100,
        height: 20,
    };
    let next = row("last", second, rect_geometry(second));
    result.words.push(RecognizedWord {
        line: 1,
        ..next.words.into_iter().next().unwrap()
    });
    result.lines.extend(next.lines);
    let target = LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap();
    assert_eq!(target.line_range(), 0..7);
    assert_eq!(target.original_line(), "e\u{301}🙂");
    assert_eq!(
        LocalTarget::resolve(&result, 1..3, frame(), 0).unwrap_err(),
        TargetError::InvalidSelection
    );
    assert_eq!(
        LocalTarget::resolve(&result, 7..8, frame(), 0).unwrap_err(),
        TargetError::CrossLine
    );
    assert_eq!(
        LocalTarget::resolve(&result, 0..10, frame(), 0).unwrap_err(),
        TargetError::CrossLine
    );
    assert_eq!(
        LocalTarget::resolve(&result, 9..13, frame(), 0)
            .unwrap()
            .line_index(),
        1
    );
}

#[test]
fn local_target_rejects_empty_crossline_missing_line_and_mismatched_body() {
    let mut result = recognized();
    assert_eq!(
        LocalTarget::resolve(&result, 2..2, frame(), 0).unwrap_err(),
        TargetError::InvalidSelection
    );
    result.lines.clear();
    assert_eq!(
        LocalTarget::resolve(&result, 0..5, frame(), 0).unwrap_err(),
        TargetError::MissingLine
    );
    result = recognized();
    result.lines[0].text = "other".into();
    assert_eq!(
        LocalTarget::resolve(&result, 0..5, frame(), 0).unwrap_err(),
        TargetError::TextMismatch
    );
    result = recognized();
    result.text.push_str("\nlast");
    assert_eq!(
        LocalTarget::resolve(&result, 2..8, frame(), 0).unwrap_err(),
        TargetError::CrossLine
    );
}

#[test]
fn local_target_rejects_missing_uncovered_and_wrong_word_evidence() {
    for case in 0..5 {
        let mut result = recognized();
        match case {
            0 => result.words.clear(),
            1 => result.words[0].text = "firs".into(),
            2 => result.words[0].text = "irst".into(),
            3 => result.words[0].line = 1,
            _ => result.words[0].rect.width = 0,
        }
        assert_eq!(
            LocalTarget::resolve(&result, 0..5, frame(), 0).unwrap_err(),
            TargetError::MissingWords,
            "case={case}"
        );
    }
}

#[test]
fn local_target_rejects_missing_nan_degenerate_outside_and_reversed_word_geometry() {
    for case in 0..7 {
        let mut result = recognized();
        match case {
            0 => result.lines[0].geometry = None,
            1 => result.words[0].geometry = None,
            2 => result.lines[0].geometry.as_mut().unwrap().quad[0].x = f32::NAN,
            3 => result.lines[0].geometry.as_mut().unwrap().quad[1] = Point { x: 20., y: 30. },
            4 => result.lines[0].geometry.as_mut().unwrap().quad[1].x = 257.,
            5 => result.words[0].geometry.as_mut().unwrap().quad[1].x = 121.,
            _ => {
                let q = result.words[0].geometry.unwrap().quad;
                result.words[0].geometry = Some(TextGeometry {
                    quad: [q[2], q[3], q[0], q[1]],
                });
            }
        }
        assert_eq!(
            LocalTarget::resolve(&result, 0..5, frame(), 0).unwrap_err(),
            TargetError::BadGeometry,
            "case={case}"
        );
    }
}

#[test]
fn local_target_rejects_overlapping_or_duplicate_rows_without_screen_sorting() {
    for geometry in [true, false] {
        let mut result = recognized();
        result.text.push_str("\nlast");
        let mut other = row(
            "last",
            Rect {
                x: 50,
                y: 35,
                width: 80,
                height: 20,
            },
            rect_geometry(Rect {
                x: 50,
                y: 35,
                width: 80,
                height: 20,
            }),
        );
        if !geometry {
            other.lines[0].geometry = None;
        }
        result.lines.extend(other.lines);
        assert_eq!(
            LocalTarget::resolve(&result, 0..5, frame(), 0).unwrap_err(),
            TargetError::AmbiguousLine
        );
    }
}

#[test]
fn local_target_repeated_text_with_duplicate_word_projection_is_not_unique_evidence() {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    let result = two_words((rect, rect_geometry(rect)), (rect, rect_geometry(rect)));
    assert_eq!(
        LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap_err(),
        TargetError::BadGeometry
    );
    assert_eq!(result.text, "A A");
}

#[test]
fn local_target_reading_u_rejects_reversed_and_nearly_coincident_word_ranges() {
    let first = Rect {
        x: 60,
        y: 30,
        width: 60,
        height: 20,
    };
    let second = Rect {
        x: 20,
        y: 30,
        width: 40,
        height: 20,
    };
    let result = two_words(
        (first, rect_geometry(first)),
        (second, rect_geometry(second)),
    );
    assert_eq!(
        LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap_err(),
        TargetError::BadGeometry
    );
    let rect = Rect {
        x: 20,
        y: 30,
        width: 1,
        height: 20,
    };
    let mut first = rect_geometry(rect);
    first.quad[1].x = 20.00011;
    first.quad[2].x = 20.00011;
    let mut second = first;
    for p in &mut second.quad {
        p.x += 0.000004;
    }
    let result = two_words((rect, first), (rect, second));
    assert_eq!(
        LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap_err(),
        TargetError::BadGeometry,
        "极窄单元不能用容差重复大部分阅读 u 证据"
    );
}

#[test]
fn local_target_word_u_keeps_small_projection_backoff_and_180_reading_order() {
    let first = Rect {
        x: 20,
        y: 30,
        width: 40,
        height: 20,
    };
    let second = Rect {
        x: 59,
        y: 30,
        width: 61,
        height: 20,
    };
    let mut second_geometry = rect_geometry(second);
    second_geometry.quad[0].x = 60. - 0.00002;
    second_geometry.quad[3].x = 60. - 0.00002;
    let result = two_words((first, rect_geometry(first)), (second, second_geometry));
    assert!(LocalTarget::resolve(&result, 0..3, frame(), 0).is_ok());

    let full = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    let reverse = |rect: Rect| {
        let q = rect_geometry(rect).quad;
        TextGeometry {
            quad: [q[2], q[3], q[0], q[1]],
        }
    };
    let mut result = row("A A", full, reverse(full));
    result.words = [
        Rect {
            x: 80,
            y: 30,
            width: 40,
            height: 20,
        },
        Rect {
            x: 20,
            y: 30,
            width: 60,
            height: 20,
        },
    ]
    .into_iter()
    .map(|rect| RecognizedWord {
        text: "A".into(),
        rect,
        line: 0,
        horizontal_direction: HorizontalDirection::RightToLeft,
        geometry: Some(reverse(rect)),
    })
    .collect();
    let target = LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap();
    assert_eq!(
        Projection::new(target.geometry()).unwrap().direction(),
        ReadingDirection::RightToLeft
    );
    assert_eq!(result.words[0].rect.x, 80, "原文顺序不按桌面 x 重排");
}

#[test]
fn local_target_estimated_word_overlap_uses_ordered_distinct_reading_midpoints() {
    // 原模型的估计重叠不是墨迹相交；连嵌套带也只需真实阅读中点有序。
    for (first_x, first_width, second_x, second_width) in [(20, 60, 70, 50), (20, 80, 70, 20)] {
        let first = Rect {
            x: first_x,
            y: 30,
            width: first_width,
            height: 20,
        };
        let second = Rect {
            x: second_x,
            y: 30,
            width: second_width,
            height: 20,
        };
        let result = two_words(
            (first, rect_geometry(first)),
            (second, rect_geometry(second)),
        );
        let target = LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap();
        assert_eq!(
            target.roi(),
            Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 20
            }
        );
        assert_eq!(target.original_line(), "A A");
    }
}

// 两份真正PP-OCR CLI DTO的原始f32 bits，见本专题evidence；不推理或重造词框。
fn real_chinese_estimated_bands(reverse: bool) -> RecognizedText {
    let (text, direction, y, height, y0, y1, line_x, left, right, bands) = if reverse {
        (
            "截图文字 2026",
            HorizontalDirection::RightToLeft,
            14,
            42,
            0x42600000,
            0x41625FA4,
            83,
            0x439194F0,
            0x42A7F211,
            [
                ("截", 249, 43, 0x439194F0, 0x4379336D),
                ("图", 221, 36, 0x438018EB, 0x435D39CB),
                ("文", 193, 36, 0x43643833, 0x43414029),
                ("字", 172, 29, 0x43483E92, 0x432C44F0),
                ("2026", 83, 76, 0x431E481F, 0x42A7F211),
            ],
        )
    } else {
        (
            "截图文字2026",
            HorizontalDirection::LeftToRight,
            0,
            46,
            0x3ED595C0,
            0x423754D4,
            6,
            0x40C623E8,
            0x4356B79C,
            [
                ("截", 6, 38, 0x40C623E8, 0x422E61D3),
                ("图", 43, 31, 0x422E61D3, 0x42930972),
                ("文", 66, 38, 0x42841351, 0x42CEE1FB),
                ("字", 95, 39, 0x42BFEBD9, 0x43055D42),
                ("2026", 133, 82, 0x43055D42, 0x4356B79C),
            ],
        )
    };
    let geometry = |left, right| TextGeometry {
        quad: [
            Point {
                x: f32::from_bits(left),
                y: f32::from_bits(y0),
            },
            Point {
                x: f32::from_bits(right),
                y: f32::from_bits(y0),
            },
            Point {
                x: f32::from_bits(right),
                y: f32::from_bits(y1),
            },
            Point {
                x: f32::from_bits(left),
                y: f32::from_bits(y1),
            },
        ],
    };
    let words: Vec<_> = bands
        .into_iter()
        .map(|(text, x, width, left, right)| RecognizedWord {
            text: text.into(),
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            line: 0,
            horizontal_direction: direction,
            geometry: Some(geometry(left, right)),
        })
        .collect();
    RecognizedText {
        text: text.into(),
        regions: words.iter().map(|w| w.rect).collect(),
        words,
        lines: vec![RecognizedLine {
            text: text.into(),
            rect: Some(Rect {
                x: line_x,
                y,
                width: 209,
                height,
            }),
            horizontal_direction: direction,
            geometry: Some(geometry(left, right)),
            graphemes: None,
        }],
    }
}

#[test]
fn local_target_real_chinese_ctc_bands_at_0_and_180_keep_full_line_roi() {
    let input = SourceFrame {
        width: 300,
        height: 56,
        origin: (-1700, -900),
    };
    for (reverse, roi) in [
        (
            false,
            Rect {
                x: 2,
                y: 0,
                width: 217,
                height: 50,
            },
        ),
        (
            true,
            Rect {
                x: 79,
                y: 10,
                width: 217,
                height: 46,
            },
        ),
    ] {
        let result = real_chinese_estimated_bands(reverse);
        let projection = Projection::new(result.lines[0].geometry.unwrap()).unwrap();
        let first_end = projection
            .inverse(result.words[1].geometry.unwrap().quad[1])
            .unwrap()[0];
        let next_start = projection
            .inverse(result.words[2].geometry.unwrap().quad[0])
            .unwrap()[0];
        assert!(next_start < first_end, "真实图/文词带必须保留重叠反例");
        let target = LocalTarget::resolve(&result, 0..result.text.len(), input, 4).unwrap();
        assert_eq!(target.roi(), roi);
        assert_eq!(target.geometry(), result.lines[0].geometry.unwrap());
        assert_eq!(target.original_line(), result.text);
        assert_eq!(
            target.global_roi(),
            (
                -1700 + i64::from(roi.x),
                -900 + i64::from(roi.y),
                roi.width,
                roi.height
            )
        );
        assert_eq!(
            projection.direction(),
            if reverse {
                ReadingDirection::RightToLeft
            } else {
                ReadingDirection::LeftToRight
            }
        );
        assert!(
            result.lines[0].graphemes.is_none(),
            "局部行不强依赖字素metadata"
        );
    }
}

#[test]
fn local_target_real_chinese_bands_still_reject_duplicate_reverse_and_outside_evidence() {
    let input = SourceFrame {
        width: 300,
        height: 56,
        origin: (-1700, -900),
    };
    for reverse in [false, true] {
        for defect in 0..3 {
            let mut result = real_chinese_estimated_bands(reverse);
            match defect {
                0 => {
                    result.words[1].rect = result.words[0].rect;
                    result.words[1].geometry = result.words[0].geometry;
                }
                1 => {
                    let first = (result.words[0].rect, result.words[0].geometry);
                    result.words[0].rect = result.words[1].rect;
                    result.words[0].geometry = result.words[1].geometry;
                    result.words[1].rect = first.0;
                    result.words[1].geometry = first.1;
                }
                _ => result.words[1].geometry.as_mut().unwrap().quad[0].x = 301.,
            }
            assert_eq!(
                LocalTarget::resolve(&result, 0..result.text.len(), input, 4).unwrap_err(),
                TargetError::BadGeometry
            );
            assert_eq!(
                result.text,
                if reverse {
                    "截图文字 2026"
                } else {
                    "截图文字2026"
                }
            );
        }
    }
}

#[test]
fn local_target_distinct_estimated_quads_with_coincident_reading_midpoints_are_ambiguous() {
    let first = Rect {
        x: 20,
        y: 30,
        width: 80,
        height: 20,
    };
    let second = Rect {
        x: 40,
        y: 30,
        width: 40,
        height: 20,
    };
    let result = two_words(
        (first, rect_geometry(first)),
        (second, rect_geometry(second)),
    );
    assert_ne!(result.words[0].geometry, result.words[1].geometry);
    assert_eq!(
        LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap_err(),
        TargetError::BadGeometry
    );
}

#[test]
fn local_target_overlapping_bands_use_vertical_and_skewed_line_reading_projection() {
    let geometry = |points: [(f32, f32); 4]| TextGeometry {
        quad: points.map(|(x, y)| Point { x, y }),
    };
    for (line_rect, line, first_rect, first, second_rect, second, direction) in [
        (
            Rect {
                x: 0,
                y: 30,
                width: 20,
                height: 100,
            },
            geometry([(20., 30.), (20., 130.), (0., 130.), (0., 30.)]),
            Rect {
                x: 0,
                y: 30,
                width: 20,
                height: 60,
            },
            geometry([(20., 30.), (20., 90.), (0., 90.), (0., 30.)]),
            Rect {
                x: 0,
                y: 80,
                width: 20,
                height: 50,
            },
            geometry([(20., 80.), (20., 130.), (0., 130.), (0., 80.)]),
            ReadingDirection::TopToBottom,
        ),
        (
            Rect {
                x: 12,
                y: 30,
                width: 108,
                height: 60,
            },
            geometry([(20., 30.), (120., 70.), (112., 90.), (12., 50.)]),
            Rect {
                x: 12,
                y: 30,
                width: 68,
                height: 44,
            },
            geometry([(20., 30.), (80., 54.), (72., 74.), (12., 50.)]),
            Rect {
                x: 62,
                y: 50,
                width: 58,
                height: 40,
            },
            geometry([(70., 50.), (120., 70.), (112., 90.), (62., 70.)]),
            ReadingDirection::LeftToRight,
        ),
    ] {
        let mut result = row("A A", line_rect, line);
        result.words = [(first_rect, first), (second_rect, second)]
            .into_iter()
            .map(|(rect, geometry)| RecognizedWord {
                text: "A".into(),
                rect,
                line: 0,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            })
            .collect();
        let target = LocalTarget::resolve(&result, 0..3, frame(), 0).unwrap();
        assert_eq!(target.geometry(), line);
        assert_eq!(target.roi(), line_rect);
        assert_eq!(
            Projection::new(target.geometry()).unwrap().direction(),
            direction
        );
        assert_eq!(result.text, "A A");
    }
}

#[test]
fn local_target_final_padding_roi_cannot_ingest_an_adjacent_line_or_word() {
    for case in 0..3 {
        let mut result = recognized();
        let rect = Rect {
            x: 20,
            y: 80,
            width: 100,
            height: 20,
        };
        let mut next = row("last", rect, rect_geometry(rect));
        if case >= 1 {
            next.lines[0].geometry = None;
        }
        if case == 2 {
            next.lines[0].rect = None;
        }
        result.text.push_str("\nlast");
        result.words.push(RecognizedWord {
            line: 1,
            ..next.words.into_iter().next().unwrap()
        });
        result.lines.extend(next.lines);
        let small = LocalTarget::resolve(&result, 0..5, frame(), 2).unwrap();
        assert_eq!(
            small.roi(),
            Rect {
                x: 18,
                y: 28,
                width: 104,
                height: 24
            }
        );
        assert_eq!(
            LocalTarget::resolve(&result, 0..5, frame(), 64).unwrap_err(),
            TargetError::AmbiguousLine,
            "case={case}"
        );
    }
}

#[test]
fn local_target_budget_and_edge_padding_are_bounded_before_pixel_allocation() {
    let result = recognized();
    assert_eq!(
        LocalTarget::resolve(
            &result,
            0..5,
            SourceFrame {
                origin: (i32::MAX, 0),
                ..frame()
            },
            0
        )
        .unwrap_err(),
        TargetError::InvalidFrame
    );
    assert_eq!(
        LocalTarget::resolve(
            &result,
            0..5,
            SourceFrame {
                width: 65_535,
                height: 1000,
                ..frame()
            },
            0
        )
        .unwrap_err(),
        TargetError::InvalidFrame
    );
    assert_eq!(
        LocalTarget::resolve(&result, 0..5, frame(), 65).unwrap_err(),
        TargetError::Budget
    );
    let huge_rect = Rect {
        x: 0,
        y: 0,
        width: 4000,
        height: 2000,
    };
    let huge = row("first", huge_rect, rect_geometry(huge_rect));
    assert_eq!(
        LocalTarget::resolve(
            &huge,
            0..5,
            SourceFrame {
                width: 4096,
                height: 4096,
                ..frame()
            },
            0
        )
        .unwrap_err(),
        TargetError::Budget
    );
    let wide_rect = Rect {
        x: 0,
        y: 0,
        width: 8200,
        height: 10,
    };
    let wide = row("first", wide_rect, rect_geometry(wide_rect));
    assert_eq!(
        LocalTarget::resolve(
            &wide,
            0..5,
            SourceFrame {
                width: 10_000,
                height: 100,
                ..frame()
            },
            0
        )
        .unwrap_err(),
        TargetError::Budget
    );
    let edge_rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 20,
    };
    let edge = row("first", edge_rect, rect_geometry(edge_rect));
    let target = LocalTarget::resolve(&edge, 0..5, frame(), 64).unwrap();
    assert_eq!(
        target.roi(),
        Rect {
            x: 0,
            y: 0,
            width: 256,
            height: 84
        }
    );
    let mut oversized = recognized();
    oversized.text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        LocalTarget::resolve(&oversized, 0..1, frame(), 0).unwrap_err(),
        TargetError::Budget
    );
}

#[test]
fn local_target_crop_preserves_exact_packed_bgra_alpha_and_source_bytes() {
    let target = LocalTarget::resolve(&recognized(), 1..3, frame(), 2).unwrap();
    let mut image = Image {
        width: 256,
        height: 256,
        bgra: vec![],
        readback_ms: 17.,
    };
    for y in 0..256 {
        for x in 0..256 {
            image
                .bgra
                .extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, x as u8]);
        }
    }
    let original = image.bgra.clone();
    let cropped = target.extract_pixels(&image).unwrap();
    assert_eq!((cropped.width, cropped.height), (104, 24));
    let roi = target.roi();
    for row in 0..roi.height as usize {
        let start = ((row + roi.y as usize) * 256 + roi.x as usize) * 4;
        assert_eq!(
            &cropped.bgra[row * 416..(row + 1) * 416],
            &original[start..start + 416]
        );
    }
    assert_eq!(image.bgra, original);
    image.bgra.pop();
    assert!(matches!(
        target.extract_pixels(&image),
        Err(TargetError::InvalidPixels)
    ));
}

#[test]
fn local_target_word_and_line_count_limits_reject_before_geometry_work() {
    let mut too_many_words = recognized();
    let rect = too_many_words.words[0].rect;
    too_many_words.words = (0..=MAX_WORDS)
        .map(|_| RecognizedWord {
            text: "first".into(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(rect_geometry(rect)),
        })
        .collect();
    assert_eq!(
        LocalTarget::resolve(&too_many_words, 0..5, frame(), 0).unwrap_err(),
        TargetError::Budget
    );
    let mut too_many_lines = recognized();
    too_many_lines.lines = (0..=MAX_LINES)
        .map(|_| RecognizedLine {
            text: "first".into(),
            rect: None,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
            graphemes: None,
        })
        .collect();
    assert_eq!(
        LocalTarget::resolve(&too_many_lines, 0..5, frame(), 0).unwrap_err(),
        TargetError::Budget
    );
    let mut too_many_body_rows = recognized();
    too_many_body_rows.text = "first\n".repeat(MAX_LINES);
    assert_eq!(
        LocalTarget::resolve(&too_many_body_rows, 0..5, frame(), 0).unwrap_err(),
        TargetError::Budget
    );
}

#[test]
fn local_candidate_completion_does_not_replace_original_selection_mark_or_undo() {
    use crate::{
        annotation::Style,
        document::Document,
        ocr_markup::{MarkLedger, TextMark},
        pin_text_selection::SelectionText,
    };
    let binding = binding();
    let selection = SelectionText::from_recognized(&binding.result);
    let selected = 1..3;
    let cursor = selection
        .keyboard_selection_from_range(selected.clone())
        .unwrap();
    let mut document = Document::default();
    let mut ledger = MarkLedger::default();
    let edit = ledger
        .prepare(
            &document,
            TextMark::Highlight,
            &[binding.result.lines[0].geometry.unwrap().quad],
            Style::default(),
        )
        .unwrap();
    assert!(ledger.commit(&mut document, edit));
    let marked = document.items().to_vec();
    let revision = document.revision();
    let mut session = LocalRetry::default();
    let req = request(&mut session, &binding);
    assert_eq!(
        complete(&mut session, &req, &binding, " corrected 🙂 \r\n"),
        Completion::Ready
    );
    assert!(
        req.token().is_cancelled(),
        "共享 Owner 已释放，不能追认完成为取消"
    );
    assert_eq!(
        session.candidate(&binding, "first", true).unwrap().text(),
        " corrected 🙂 \r\n"
    );
    assert_eq!(binding.result.text, "first");
    assert_eq!(selection.text, "first");
    assert_eq!(cursor.range(), selected);
    assert_eq!(document.revision(), revision);
    assert_eq!(document.items(), marked);
    assert!(ledger.matches_underlay(&document));
    let retry = request(&mut session, &binding);
    assert_eq!(
        session.complete(
            &retry,
            &binding,
            "first",
            true,
            Receipt::Failed("局部失败".into())
        ),
        Completion::Failed
    );
    assert_eq!(document.items(), marked);
    assert_eq!(document.revision(), revision);
    assert_eq!(cursor.range(), selected);
    let retry = request(&mut session, &binding);
    session.cancel("局部用户取消");
    assert!(retry.token().is_cancelled());
    assert_eq!(document.items(), marked);
    assert_eq!(document.revision(), revision);
    assert_eq!(cursor.range(), selected);
    assert_eq!(binding.result.text, "first");
    document.undo();
    assert!(document.items().is_empty());
    document.redo();
    assert_eq!(document.items(), marked);
}

#[test]
fn local_candidate_same_line_retry_replaces_intent_and_rejects_old_receipt() {
    let binding = binding();
    let mut session = LocalRetry::default();
    let old = request(&mut session, &binding);
    let current = request(&mut session, &binding);
    assert!(old.token().is_cancelled());
    current.token().checkpoint().unwrap();
    assert_eq!(
        complete(&mut session, &old, &binding, "late"),
        Completion::Stale
    );
    current.token().checkpoint().unwrap();
    assert_eq!(
        complete(&mut session, &current, &binding, "new"),
        Completion::Ready
    );
    assert_eq!(
        session.candidate(&binding, "first", true).unwrap().text(),
        "new"
    );
    assert_eq!(
        complete(&mut session, &current, &binding, "duplicate"),
        Completion::Stale
    );
}

#[test]
fn local_candidate_new_real_line_intent_replaces_old_line_without_rewriting_selection() {
    let mut result = recognized();
    let rect = Rect {
        x: 20,
        y: 80,
        width: 100,
        height: 20,
    };
    let next = row("last", rect, rect_geometry(rect));
    result.text.push_str("\nlast");
    result.words.push(RecognizedWord {
        line: 1,
        ..next.words.into_iter().next().unwrap()
    });
    result.lines.extend(next.lines);
    let mut binding = binding();
    binding.result = Arc::new(result);
    let mut session = LocalRetry::default();
    let old = request(&mut session, &binding);
    let selected = 6..10;
    let current = session
        .begin(&binding, &binding.result.text, selected.clone(), 2, true)
        .unwrap();
    assert_eq!(current.target().line_index(), 1);
    assert_eq!(current.target().line_range(), 6..10);
    assert_eq!(current.target().selected(), selected);
    assert_eq!(current.provider(), OcrProvider::Ppocr);
    assert!(old.token().is_cancelled());
    assert_eq!(
        complete(&mut session, &old, &binding, "old first"),
        Completion::Stale
    );
    current.token().checkpoint().unwrap();
    assert_eq!(
        complete(&mut session, &current, &binding, "new last"),
        Completion::Ready
    );
    let candidate = session
        .candidate(&binding, &binding.result.text, true)
        .unwrap();
    assert_eq!(candidate.target().original_line(), "last");
    assert_eq!(candidate.text(), "new last");
    assert_eq!(binding.result.text, "first\nlast");
}

#[test]
fn local_candidate_cross_session_same_numeric_task_id_never_releases_other_owner() {
    let binding = binding();
    let mut a = LocalRetry::default();
    let mut b = LocalRetry::default();
    let ra = request(&mut a, &binding);
    let rb = request(&mut b, &binding);
    assert_eq!(ra.task.id, rb.task.id);
    assert_eq!(complete(&mut a, &rb, &binding, "wrong"), Completion::Stale);
    ra.token().checkpoint().unwrap();
    rb.token().checkpoint().unwrap();
    a.cancel("仅 A 取消");
    assert!(ra.token().is_cancelled());
    rb.token().checkpoint().unwrap();
    assert_eq!(complete(&mut b, &rb, &binding, "B"), Completion::Ready);
}

#[test]
fn local_candidate_source_result_generation_body_provider_and_roi_changes_invalidate() {
    for case in 0..9 {
        let initial = binding();
        let mut current = initial.clone();
        let mut session = LocalRetry::default();
        let req = request(&mut session, &initial);
        match case {
            0 => current.source = 42,
            1 => current.source_identity = Arc::new(()),
            2 => current.result = Arc::new(recognized()),
            3 => current.result_generation += 1,
            4 => current.body_revision += 1,
            5 => current.body_identity = Arc::new(()),
            6 => current.provider = OcrProvider::Windows,
            7 => current.frame.origin.0 += 1,
            _ => current.frame.width -= 1,
        }
        assert_eq!(
            complete(&mut session, &req, &current, "candidate"),
            Completion::Invalidated,
            "case={case}"
        );
        assert!(req.token().is_cancelled());
        assert!(session.candidate(&initial, "first", true).is_none());
        assert_eq!(initial.result.text, "first");
    }
}

#[test]
fn local_candidate_different_body_bytes_fail_even_without_a_revision_event() {
    let binding = binding();
    let mut session = LocalRetry::default();
    let req = request(&mut session, &binding);
    assert_eq!(
        session.complete(
            &req,
            &binding,
            "other",
            true,
            Receipt::Completed(output("candidate"))
        ),
        Completion::Invalidated
    );
    assert!(session.candidate(&binding, "first", true).is_none());
    assert!(matches!(
        session.begin(&binding, "other", 0..5, 0, true),
        Err(TargetError::EditedBody)
    ));
    assert_eq!(binding.result.text, "first");
}

#[test]
fn local_candidate_actual_editor_undo_and_revision_wrap_do_not_restore_geometry_qualification() {
    use crate::capture_view::text_result::ResultEditor;
    for revision in [0, u64::MAX - 1] {
        let mut initial = binding();
        initial.installed_body_revision = revision;
        initial.body_revision = revision;
        let mut editor = ResultEditor::new("first".into(), 0..0);
        editor.revision = revision;
        let mut session = LocalRetry::default();
        let req = request(&mut session, &initial);
        editor.select_all();
        assert!(editor.replace(None, "edited", false, None));
        editor.undo();
        assert_eq!(editor.text, "first");
        let mut current = initial.clone();
        current.body_revision = editor.revision;
        current.body_identity = Arc::new(()); // 实际修改事件刷新；Undo 不复用安装身份。
        assert_eq!(
            session.complete(
                &req,
                &current,
                &editor.text,
                true,
                Receipt::Completed(output("candidate"))
            ),
            Completion::Invalidated
        );
        assert!(matches!(
            session.begin(&current, &editor.text, 0..5, 2, true),
            Err(TargetError::EditedBody)
        ));
        assert_eq!(initial.result.text, "first");
    }
    // 即使调用方的数值 revision 恰好回到安装值，真实编辑身份也不能 ABA。
    let initial = binding();
    let mut edited = initial.clone();
    edited.body_identity = Arc::new(());
    let mut session = LocalRetry::default();
    assert!(matches!(
        session.begin(&edited, "first", 0..5, 0, true),
        Err(TargetError::EditedBody)
    ));
}

#[test]
fn local_candidate_cancel_focus_loss_and_drop_only_cancel_the_local_request() {
    let binding = binding();
    let mut full = TaskSlot::default();
    let full_request = full.start(41u64);
    let mut local = LocalRetry::default();
    let req = request(&mut local, &binding);
    assert!(local.observe(&binding, "first", false));
    assert!(req.token().is_cancelled());
    full_request.token.checkpoint().unwrap();
    assert_eq!(
        complete(&mut local, &req, &binding, "late"),
        Completion::Stale
    );
    let req = request(&mut local, &binding);
    assert!(local.cancel("用户取消局部候选"));
    assert!(!local.cancel("重复取消"));
    assert!(req.token().is_cancelled());
    full_request.token.checkpoint().unwrap();
    let req = request(&mut local, &binding);
    drop(local);
    assert!(req.token().is_cancelled());
    full_request.token.checkpoint().unwrap();
}

#[test]
fn local_candidate_failure_typed_cancel_empty_and_oversized_outputs_preserve_full_result() {
    for receipt in [
        Receipt::Failed("实际提供者错误".into()),
        Receipt::from_result(Err(crate::recognition::task::OcrCancelled.into())),
        Receipt::Completed(output(" \r\n")),
        Receipt::Completed(output(&"x".repeat(MAX_CANDIDATE_BYTES + 1))),
    ] {
        let binding = binding();
        let mut session = LocalRetry::default();
        let req = request(&mut session, &binding);
        let expected = if matches!(&receipt, Receipt::Cancelled) {
            Completion::Cancelled
        } else {
            Completion::Failed
        };
        assert_eq!(
            session.complete(&req, &binding, "first", true, receipt),
            expected
        );
        assert!(session.candidate(&binding, "first", true).is_none());
        assert_eq!(binding.result.text, "first");
        let retry = request(&mut session, &binding);
        assert_eq!(
            complete(&mut session, &retry, &binding, "retry"),
            Completion::Ready
        );
    }
}

#[test]
fn local_candidate_copy_failure_retries_exact_text_without_clearing_the_candidate() {
    let binding = binding();
    let mut session = LocalRetry::default();
    let req = request(&mut session, &binding);
    let expected = " corrected\t🙂\r\n";
    assert_eq!(
        complete(&mut session, &req, &binding, expected),
        Completion::Ready
    );
    assert_eq!(
        session.copy_candidate(&binding, "first", true, |text| {
            assert_eq!(text, expected);
            Err("发布失败")
        }),
        Err("发布失败")
    );
    assert_eq!(
        session.candidate(&binding, "first", true).unwrap().text(),
        expected
    );
    let mut written = String::new();
    assert_eq!(
        session.copy_candidate(&binding, "first", true, |text| {
            written.push_str(text);
            Ok::<_, ()>(())
        }),
        Ok(true)
    );
    assert_eq!(written, expected);
    assert_eq!(
        session
            .candidate(&binding, "first", true)
            .unwrap()
            .target()
            .original_line(),
        "first"
    );
    let called = Cell::new(false);
    let mut changed = binding.clone();
    changed.source_identity = Arc::new(());
    assert_eq!(
        session.copy_candidate(&changed, "first", true, |_| {
            called.set(true);
            Ok::<_, ()>(())
        }),
        Ok(false)
    );
    assert!(!called.get());
    assert_eq!(
        session.copy_candidate(&binding, "first", false, |_| {
            called.set(true);
            Ok::<_, ()>(())
        }),
        Ok(false)
    );
    assert!(!called.get());
    assert!(session.observe(&changed, "first", true));
    assert!(
        session.candidate(&binding, "first", true).is_none(),
        "观察来源变化后不能恢复旧候选"
    );
}

#[test]
fn local_candidate_invalid_new_target_keeps_prior_request_and_candidate() {
    let binding = binding();
    let mut session = LocalRetry::default();
    let req = request(&mut session, &binding);
    assert!(session.pending(&binding, "first", true));
    assert_eq!(
        session.target(&binding, "first", true).unwrap().selected(),
        1..3
    );
    assert!(session.target(&binding, "other", true).is_none());
    assert!(matches!(
        session.begin(&binding, "first", 2..2, 0, true),
        Err(TargetError::InvalidSelection)
    ));
    req.token().checkpoint().unwrap();
    assert_eq!(
        complete(&mut session, &req, &binding, "candidate"),
        Completion::Ready
    );
    assert!(!session.pending(&binding, "first", true));
    assert_eq!(
        session
            .target(&binding, "first", true)
            .unwrap()
            .original_line(),
        "first"
    );
    assert!(matches!(
        session.begin(&binding, "first", 0..5, 0, false),
        Err(TargetError::Inactive)
    ));
    assert_eq!(
        session.candidate(&binding, "first", true).unwrap().text(),
        "candidate"
    );
    let retry = request(&mut session, &binding);
    assert!(session.candidate(&binding, "first", true).is_none());
    assert_eq!(
        complete(&mut session, &retry, &binding, "same line retry"),
        Completion::Ready
    );
}

struct CancelCounter(Arc<AtomicUsize>);
impl crate::recognition::task::RunCancellation for CancelCounter {
    fn terminate(&self) -> crate::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn local_candidate_shared_native_control_is_called_once_and_error_text_is_bounded() {
    let binding = binding();
    let mut session = LocalRetry::default();
    let req = request(&mut session, &binding);
    let count = Arc::new(AtomicUsize::new(0));
    let registration = req
        .token()
        .register(Arc::new(CancelCounter(count.clone())))
        .unwrap();
    assert!(session.cancel("局部取消"));
    assert!(!session.cancel("重复取消"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    drop(registration);
    let retry = request(&mut session, &binding);
    assert_eq!(
        session.complete(
            &retry,
            &binding,
            "first",
            true,
            Receipt::Failed("错".repeat(3000))
        ),
        Completion::Failed
    );
    let error = session.error(&binding, "first", true).unwrap();
    assert!(error.len() <= MAX_ERROR_BYTES);
    assert!(error.chars().all(|c| c == '错'));
    assert!(session.error(&binding, "different body", true).is_none());
}
