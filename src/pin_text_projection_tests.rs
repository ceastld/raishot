//! 共享截图／贴图选字的纯几何模型；不读取模型、桌面或剪贴板。
use super::*;
use crate::{
    geometry::Rect,
    recognition::{RecognizedLine, RecognizedWord},
};

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn geometry(coordinates: [(f32, f32); 4]) -> TextGeometry {
    TextGeometry {
        quad: coordinates.map(|(x, y)| point(x, y)),
    }
}
fn word(text: &str, rect: Rect, geometry: Option<TextGeometry>, line: usize) -> RecognizedWord {
    RecognizedWord {
        text: text.into(),
        rect,
        line,
        geometry,
        horizontal_direction: HorizontalDirection::LeftToRight,
    }
}
fn recognized(text: &str, words: Vec<RecognizedWord>, lines: Vec<RecognizedLine>) -> SelectionText {
    SelectionText::from_recognized(&RecognizedText {
        text: text.into(),
        words,
        lines,
        regions: Vec::new(),
    })
}
fn vertical(down: bool, text: &str) -> SelectionText {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 80,
    };
    let quad = if down {
        [(30., 10.), (30., 90.), (10., 90.), (10., 10.)]
    } else {
        [(10., 90.), (10., 10.), (30., 10.), (30., 90.)]
    };
    recognized(
        text,
        vec![word(text, rect, Some(geometry(quad)), 0)],
        Vec::new(),
    )
}
fn assert_quad(actual: [Point; 4], expected: [(f32, f32); 4]) {
    for (actual, (x, y)) in actual.into_iter().zip(expected) {
        assert!(
            (actual.x - x).abs() < 0.001 && (actual.y - y).abs() < 0.001,
            "actual={actual:?}, expected=({x},{y})"
        );
    }
}

#[test]
fn vertical_down_first_cell_and_end_stop_follow_y() {
    let text = vertical(true, "AB");
    let (anchor, range) = text.begin(point(20., 11.), 1).unwrap();
    assert_eq!(range, 0..0);
    assert_eq!(text.extend(&anchor, point(20., 89.)), 0..2);
    assert_quad(
        text.highlights(0..1)[0],
        [(30., 10.), (30., 50.), (10., 50.), (10., 10.)],
    );
    assert_eq!(text.highlights(0..1), text.marking_quads(0..1));
}
#[test]
fn vertical_up_first_cell_and_end_stop_reverse_y() {
    let text = vertical(false, "AB");
    let (anchor, range) = text.begin(point(20., 89.), 1).unwrap();
    assert_eq!(range, 0..0);
    assert_eq!(text.extend(&anchor, point(20., 11.)), 0..2);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(10., 90.), (10., 50.), (30., 50.), (30., 90.)],
    );
}
#[test]
fn vertical_single_click_chooses_reading_edges_instead_of_global_x() {
    for (down, near_start, near_end) in [(true, 11., 49.), (false, 89., 51.)] {
        let text = vertical(down, "AB");
        assert_eq!(text.begin(point(20., near_start), 1).unwrap().1, 0..0);
        assert_eq!(text.begin(point(20., near_end), 1).unwrap().1, 1..1);
    }
}
#[test]
fn slanted_line_rejects_empty_axis_aligned_corners() {
    let text = recognized(
        "AB",
        vec![word(
            "AB",
            Rect {
                x: 0,
                y: 10,
                width: 90,
                height: 60,
            },
            Some(geometry([(10., 10.), (90., 50.), (80., 70.), (0., 30.)])),
            0,
        )],
        Vec::new(),
    );
    assert!(!text.has_text_at(point(85., 15.)));
    assert!(text.begin(point(85., 15.), 1).is_none());
    assert!(text.has_text_at(point(25., 30.)));
    assert_quad(
        text.marking_quads(0..1)[0],
        [(10., 10.), (50., 30.), (40., 50.), (0., 30.)],
    );
}
#[test]
fn slanted_drag_crosses_shared_character_edges() {
    let text = recognized(
        "AB",
        vec![word(
            "AB",
            Rect {
                x: 0,
                y: 10,
                width: 90,
                height: 60,
            },
            Some(geometry([(10., 10.), (90., 50.), (80., 70.), (0., 30.)])),
            0,
        )],
        Vec::new(),
    );
    let (anchor, _) = text.begin(point(6., 21.), 1).unwrap();
    let range = text.extend(&anchor, point(84., 59.));
    assert_eq!(text.selected(range).unwrap(), "AB");
    assert_eq!(text.highlights(0..2), text.marking_quads(0..2));
}
#[test]
fn perspective_halves_share_projected_edge_instead_of_rect_midpoint() {
    // 独立解析 oracle：x=(120u+40v)/(1+v), y=80v/(1+v)。
    let text = recognized(
        "AB",
        vec![word(
            "AB",
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 40,
            },
            Some(geometry([(0., 0.), (120., 0.), (80., 40.), (20., 40.)])),
            0,
        )],
        Vec::new(),
    );
    assert_quad(
        text.highlights(0..1)[0],
        [(0., 0.), (60., 0.), (50., 40.), (20., 40.)],
    );
    assert_quad(
        text.marking_quads(1..2)[0],
        [(60., 0.), (120., 0.), (80., 40.), (50., 40.)],
    );
    assert!(!text.has_text_at(point(5., 35.)));
    assert!(text.has_text_at(point(33.333_332, 26.666_666)));
}
#[test]
fn corrected_180_degrees_preserves_reading_order_and_transverse_side() {
    let text = recognized(
        "AB",
        vec![word(
            "AB",
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 30,
            },
            Some(geometry([(110., 40.), (10., 40.), (10., 10.), (110., 10.)])),
            0,
        )],
        Vec::new(),
    );
    let (anchor, range) = text.begin(point(109., 25.), 1).unwrap();
    assert_eq!(range, 0..0);
    assert_eq!(text.extend(&anchor, point(11., 25.)), 0..2);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(110., 40.), (60., 40.), (60., 10.), (110., 10.)],
    );
}
#[test]
fn roi_translation_moves_hit_stops_highlights_and_marks_together() {
    let mut text = vertical(true, "AB");
    let before = text.marking_quads(0..1)[0];
    text.translate(240., 180.);
    assert!(!text.has_text_at(point(20., 11.)));
    let (anchor, range) = text.begin(point(260., 191.), 1).unwrap();
    assert_eq!(range, 0..0);
    assert_eq!(text.extend(&anchor, point(260., 269.)), 0..2);
    for (after, before) in text.marking_quads(0..1)[0].into_iter().zip(before) {
        assert_eq!(after, point(before.x + 240., before.y + 180.));
    }
    assert_eq!(text.highlights(0..1), text.marking_quads(0..1));
}
#[test]
fn negative_canvas_translation_preserves_projected_reading_range() {
    let mut text = vertical(false, "AB");
    text.translate(-300., -100.);
    let (anchor, _) = text.begin(point(-280., -11.), 1).unwrap();
    assert_eq!(
        text.selected(text.extend(&anchor, point(-280., -89.)))
            .unwrap(),
        "AB"
    );
    assert_quad(
        text.marking_quads(0..1)[0],
        [(-290., -10.), (-290., -50.), (-270., -50.), (-270., -10.)],
    );
}
#[test]
fn projected_graphemes_combining_zwj_and_flags_stay_indivisible() {
    let content = "e\u{301}👨‍👩‍👧🇨🇳X";
    let text = vertical(true, content);
    let ranges: Vec<_> = text.lines[0]
        .cells
        .iter()
        .map(|cell| cell.range.clone())
        .collect();
    let expected: Vec<_> = content
        .grapheme_indices(true)
        .map(|(byte, cluster)| byte..byte + cluster.len())
        .collect();
    assert_eq!(ranges, expected);
    for cell in &text.lines[0].cells {
        assert_eq!(text.marking_quads(cell.range.clone()).len(), 1);
        assert!(text.selected(cell.range.clone()).is_some());
    }
    assert_eq!(text.marking_quads(0..content.len()).len(), 1);
}
#[test]
fn projected_real_whitespace_has_selection_geometry_but_no_persistent_mark() {
    let text = vertical(true, "A \tB");
    assert_eq!(text.selected(1..3).unwrap(), " \t");
    assert!(!text.highlights(1..3).is_empty());
    assert!(text.marking_quads(1..3).is_empty());
    assert!(!text.has_text_at(point(20., 40.)));
    // 同一裁行的连续原文合成一条标记带，空白不把它切成两段。
    assert_eq!(text.marking_quads(0..4).len(), 1);
}
#[test]
fn projected_newline_highlight_extends_along_vertical_reading_end() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 80,
    };
    let text = recognized(
        "AB\nC",
        vec![
            word(
                "AB",
                rect,
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            word(
                "C",
                Rect {
                    x: 50,
                    y: 10,
                    width: 20,
                    height: 20,
                },
                None,
                1,
            ),
        ],
        Vec::new(),
    );
    let newline = text.highlights(2..3);
    assert_eq!(newline.len(), 1);
    assert_quad(newline[0], [(30., 90.), (30., 95.), (10., 95.), (10., 90.)]);
    assert!(text.marking_quads(2..3).is_empty());
    assert_eq!(text.marking_quads(0..3).len(), 1);
}
#[test]
fn projected_newline_highlight_follows_vertical_up_and_180_end() {
    for (quad, rect, expected) in [
        (
            [(10., 90.), (10., 10.), (30., 10.), (30., 90.)],
            Rect {
                x: 10,
                y: 10,
                width: 20,
                height: 80,
            },
            [(10., 10.), (10., 5.), (30., 5.), (30., 10.)],
        ),
        (
            [(110., 40.), (10., 40.), (10., 10.), (110., 10.)],
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 30,
            },
            [(10., 40.), (5., 40.), (5., 10.), (10., 10.)],
        ),
    ] {
        let text = recognized(
            "AB\n",
            vec![word("AB", rect, Some(geometry(quad)), 0)],
            Vec::new(),
        );
        assert_quad(text.highlights(2..3)[0], expected);
        assert!(text.marking_quads(2..3).is_empty());
    }
}
#[test]
fn mixed_line_retains_projection_and_safe_rect_fallback_without_guessing() {
    let text = recognized(
        "AB CD",
        vec![
            word(
                "AB",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 80,
                },
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            word(
                "CD",
                Rect {
                    x: 50,
                    y: 10,
                    width: 40,
                    height: 20,
                },
                None,
                0,
            ),
        ],
        Vec::new(),
    );
    assert!(text.has_text_at(point(20., 20.)) && text.has_text_at(point(60., 20.)));
    assert_eq!(text.marking_quads(0..5).len(), 2);
    assert_quad(
        text.marking_quads(3..5)[0],
        [(50., 10.), (90., 10.), (90., 30.), (50., 30.)],
    );
    assert_eq!(text.selected(0..5).unwrap(), "AB CD");
}
#[test]
fn mixed_line_rtl_rect_fallback_preserves_physical_mark_bottom_edge() {
    let mut second = word(
        "CD",
        Rect {
            x: 50,
            y: 10,
            width: 40,
            height: 20,
        },
        None,
        0,
    );
    second.horizontal_direction = HorizontalDirection::RightToLeft;
    let text = recognized(
        "AB CD",
        vec![
            word(
                "AB",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 80,
                },
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            second,
        ],
        Vec::new(),
    );
    let mark = text.marking_quads(3..4)[0];
    assert_quad(mark, [(70., 10.), (90., 10.), (90., 30.), (70., 30.)]);
    // 公共标记以下沿 q3→q2 画 underline；缺失投影不能翻成上沿。
    assert_eq!((mark[3], mark[2]), (point(70., 30.), point(90., 30.)));
    assert_eq!(text.highlights(3..4), text.marking_quads(3..4));
    assert_eq!(text.begin(point(89., 20.), 1).unwrap().1, 3..3);
}
#[test]
fn mixed_line_invalid_fine_quad_falls_back_to_safe_rect() {
    let text = recognized(
        "AB CD",
        vec![
            word(
                "AB",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 80,
                },
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            word(
                "CD",
                Rect {
                    x: 50,
                    y: 10,
                    width: 40,
                    height: 20,
                },
                Some(geometry([(50., 10.), (90., 30.), (90., 10.), (50., 30.)])),
                0,
            ),
        ],
        Vec::new(),
    );
    assert!(text.lines[0].projected);
    assert!(text.has_text_at(point(60., 20.)));
    assert_quad(
        text.marking_quads(3..5)[0],
        [(50., 10.), (90., 10.), (90., 30.), (50., 30.)],
    );
    assert!(text.lines[0]
        .cells
        .iter()
        .filter(|cell| cell.range.start >= 3)
        .all(|cell| cell.projected.is_none()));
}
#[test]
fn projected_crlf_preserves_copy_and_excludes_both_tail_bytes_from_marks() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 80,
    };
    let text = recognized(
        "AB\r\nC",
        vec![
            word(
                "AB",
                rect,
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            word(
                "C",
                Rect {
                    x: 50,
                    y: 10,
                    width: 20,
                    height: 20,
                },
                None,
                1,
            ),
        ],
        Vec::new(),
    );
    assert_eq!(text.selected(2..4).unwrap(), "\r\n");
    assert_eq!(text.highlights(2..4).len(), 1);
    assert!(text.marking_quads(2..4).is_empty());
    assert_eq!(text.selected(0..5).unwrap(), "AB\r\nC");
}
#[test]
fn touching_different_word_sources_are_not_merged_as_one_projection() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 40,
    };
    let text = recognized(
        "AB",
        vec![
            word(
                "A",
                rect,
                Some(geometry([(30., 10.), (30., 50.), (10., 50.), (10., 10.)])),
                0,
            ),
            word(
                "B",
                Rect {
                    x: 10,
                    y: 50,
                    width: 20,
                    height: 40,
                },
                Some(geometry([(30., 50.), (30., 90.), (10., 90.), (10., 50.)])),
                0,
            ),
        ],
        Vec::new(),
    );
    assert_eq!(text.highlights(0..2).len(), 2);
    assert_eq!(text.marking_quads(0..2).len(), 2);
}
#[test]
fn projected_and_horizontal_rows_use_actual_nearest_row_for_drag() {
    let text = recognized(
        "AB\nCD",
        vec![
            word(
                "AB",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 80,
                },
                Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
                0,
            ),
            word(
                "CD",
                Rect {
                    x: 60,
                    y: 10,
                    width: 80,
                    height: 20,
                },
                None,
                1,
            ),
        ],
        Vec::new(),
    );
    let (anchor, _) = text.begin(point(20., 11.), 1).unwrap();
    let range = text.extend(&anchor, point(139., 20.));
    assert_eq!(text.selected(range).unwrap(), "AB\nCD");
    assert_eq!(text.begin(point(100., 20.), 3).unwrap().1, 3..5);
}
#[test]
fn exact_line_text_can_fallback_to_projected_line_geometry() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 80,
    };
    let line = RecognizedLine {
        graphemes: None,
        text: "AB".into(),
        rect: Some(rect),
        geometry: Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
        horizontal_direction: HorizontalDirection::LeftToRight,
    };
    let text = recognized("AB", vec![word("?", rect, None, 0)], vec![line]);
    assert_quad(
        text.marking_quads(0..1)[0],
        [(30., 10.), (30., 50.), (10., 50.), (10., 10.)],
    );
    assert_eq!(text.begin(point(20., 11.), 3).unwrap().1, 0..2);
}
#[test]
fn mismatched_line_text_never_borrows_other_projected_geometry() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 20,
        height: 80,
    };
    let line = RecognizedLine {
        graphemes: None,
        text: "AC".into(),
        rect: Some(rect),
        geometry: Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
        horizontal_direction: HorizontalDirection::LeftToRight,
    };
    let text = recognized("AB", vec![word("?", rect, None, 0)], vec![line]);
    assert!(!text.has_geometry());
    assert!(text.marking_quads(0..2).is_empty());
}
#[test]
fn invalid_fine_geometry_keeps_old_safe_rect_and_direction() {
    let rect = Rect {
        x: 10,
        y: 10,
        width: 80,
        height: 20,
    };
    for quad in [
        [(10., 10.), (90., 30.), (90., 10.), (10., 30.)],
        [(9., 10.), (90., 10.), (90., 30.), (10., 30.)],
        [(10., 10.), (10., 10.), (90., 30.), (10., 30.)],
        [(f32::NAN, 10.), (90., 10.), (90., 30.), (10., 30.)],
        [(10., 10.), (90., 10.), (90., f32::INFINITY), (10., 30.)],
    ] {
        let text = recognized(
            "AB",
            vec![word("AB", rect, Some(geometry(quad)), 0)],
            Vec::new(),
        );
        assert!(!text.lines[0].projected);
        assert_quad(
            text.marking_quads(0..1)[0],
            [(10., 10.), (50., 10.), (50., 30.), (10., 30.)],
        );
    }
}
#[test]
fn invalid_rect_cannot_be_rescued_by_plausible_projected_quad() {
    let text = recognized(
        "AB",
        vec![word(
            "AB",
            Rect {
                x: 10,
                y: 10,
                width: 0,
                height: 80,
            },
            Some(geometry([(30., 10.), (30., 90.), (10., 90.), (10., 10.)])),
            0,
        )],
        Vec::new(),
    );
    assert!(!text.has_geometry());
    assert!(text.highlights(0..2).is_empty() && text.marking_quads(0..2).is_empty());
}
#[test]
fn projected_word_and_line_clicks_keep_original_byte_ranges() {
    let text = vertical(true, "hello");
    assert_eq!(text.begin(point(20., 20.), 2).unwrap().1, 0..5);
    assert_eq!(text.begin(point(20., 20.), 3).unwrap().1, 0..5);
    assert_eq!(text.selected(0..5).unwrap(), "hello");
}
#[test]
fn projected_finish_drag_samples_final_point_and_capture_loss_keeps_last_range() {
    let text = vertical(true, "AB");
    let (start, range) = text.begin(point(20., 11.), 1).unwrap();
    let mut anchor = Some(start);
    let mut selected = Some(range);
    assert!(text.finish_drag(&mut anchor, &mut selected, point(20., 89.), true));
    assert_eq!(selected, Some(0..2));
    let (start, range) = text.begin(point(20., 11.), 1).unwrap();
    anchor = Some(start);
    selected = Some(range);
    assert!(text.drag(&mut anchor, &mut selected, point(20., 89.), true, true));
    assert_eq!(selected, Some(0..2));
    assert!(text.drag(&mut anchor, &mut selected, point(20., 11.), true, false));
    assert!(anchor.is_none());
    assert_eq!(selected, Some(0..2));
}
