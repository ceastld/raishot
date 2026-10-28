//! 原文 byte 停点只读取现有字素截面；缺少或冲突的来源几何不插值。
use super::{cell_quad, Cell, Line, Point, Projection, SelectionText};
use unicode_segmentation::GraphemeCursor;

impl SelectionText {
    /// 返回当前字素边界在来源坐标中的截面，供预览绘制；不修改选区或文档。
    /// 行首取后字素前沿，行尾取前字素后沿；内部同时有两沿时必须是同一线段。
    /// 词间空白缺框时，只接受相邻真实字素的精确边沿，不推算空隙内停点。
    /// 同一来源的字素分段仍为既有估算，不把这条线段称作精确墨迹边界。
    pub(crate) fn caret_segment(&self, byte: usize) -> Option<[Point; 2]> {
        if !grapheme_boundary(&self.text, byte) {
            return None;
        }
        let mut lines = self.lines.iter().filter_map(|line| {
            self.text.get(line.range.clone())?;
            let mut end = line.range.end;
            // 与导航相同：OCR 按 LF 分行，CRLF 只能作为完整字素跨过。
            if self.text.as_bytes().get(end) == Some(&b'\n')
                && end > line.range.start
                && self.text.as_bytes().get(end - 1) == Some(&b'\r')
            {
                end -= 1;
            }
            (!line.cells.is_empty()
                && line.range.start <= byte
                && byte <= end
                && grapheme_boundary(&self.text, line.range.start)
                && grapheme_boundary(&self.text, end))
            .then_some((line, end))
        });
        let (line, end) = lines.next()?;
        if lines.next().is_some() {
            return None;
        }
        let (mut previous, mut next) = (None, None);
        for cell in &line.cells {
            if cell.range.start < line.range.start || cell.range.end > end {
                continue;
            }
            // 一个形状内含多个停点时没有该 byte 的截面，不能按宽度再分配。
            if cell.range.start < byte && byte < cell.range.end {
                return None;
            }
            let target = if cell.range.end == byte {
                &mut previous
            } else if cell.range.start == byte {
                &mut next
            } else {
                continue;
            };
            if target.is_some()
                || cell.range.start >= cell.range.end
                || self.text.get(cell.range.clone()).is_none()
                || !grapheme_boundary(&self.text, cell.range.start)
                || !grapheme_boundary(&self.text, cell.range.end)
            {
                return None;
            }
            *target = Some(cell_segment(line, cell, cell.range.start == byte)?);
        }
        match (previous, next) {
            (Some(previous), Some(next)) => same_segment(previous, next).then_some(next),
            (Some(segment), None) | (None, Some(segment)) => Some(segment),
            (None, None) => None,
        }
    }
}

fn grapheme_boundary(text: &str, byte: usize) -> bool {
    text.is_char_boundary(byte)
        && GraphemeCursor::new(byte, text.len(), true).is_boundary(text, 0) == Ok(true)
}

fn cell_segment(line: &Line, cell: &Cell, start: bool) -> Option<[Point; 2]> {
    if let Some(projected) = cell.projected {
        // cell_quad 为旧消费者保留矩形回退；这里不把损坏的投影当作有效截面。
        Projection::new(projected.source)?;
        if !projected.start.is_finite()
            || !projected.end.is_finite()
            || !(0.0..=1.0).contains(&projected.start)
            || !(0.0..=1.0).contains(&projected.end)
            || projected.start >= projected.end
        {
            return None;
        }
    } else if ![
        cell.bounds.x,
        cell.bounds.y,
        cell.bounds.width,
        cell.bounds.height,
    ]
    .iter()
    .all(|value| value.is_finite())
        || cell.bounds.width <= 0.
        || cell.bounds.height <= 0.
    {
        return None;
    }
    // cell_quad 已含 reverse_x 的逻辑端点顺序，不能再次按 x 翻转。
    let quad = cell_quad(cell);
    let mut segment = if start {
        [quad[0], quad[3]]
    } else {
        [quad[1], quad[2]]
    };
    if cell.projected.is_none() {
        let byte = if start {
            cell.range.start
        } else {
            cell.range.end
        };
        let mut stops = line.stops.iter().filter(|stop| stop.0 == byte);
        let x = stops.next()?.1;
        // 旧矩形右沿重建与最小形状宽度不一定保留原停点；只取来源已有唯一 x。
        // 不按屏幕 x 选择其它 byte，也不在两个相冲突的词框之间插值。
        if !x.is_finite() || stops.any(|stop| stop.1 != x) {
            return None;
        }
        for point in &mut segment {
            point.x = x;
        }
    }
    let segment = segment.map(|point| line.rotation.map(point));
    (segment
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
        && segment[0] != segment[1])
        .then_some(segment)
}

fn same_segment(a: [Point; 2], b: [Point; 2]) -> bool {
    // 只接受完全相同的现有物理截面；异来源的近似邻接也不能证明唯一停点。
    a == b || a == [b[1], b[0]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::Bounds,
        geometry::Rect,
        recognition::{
            HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
        },
        transform::Rotation,
    };
    use unicode_segmentation::UnicodeSegmentation;

    fn point(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    fn word(
        text: &str,
        rect: Rect,
        quad: Option<[(f32, f32); 4]>,
        direction: HorizontalDirection,
        line: usize,
    ) -> RecognizedWord {
        RecognizedWord {
            text: text.into(),
            rect,
            line,
            geometry: quad.map(|quad| TextGeometry {
                quad: quad.map(|(x, y)| point(x, y)),
            }),
            horizontal_direction: direction,
        }
    }
    fn recognized(text: &str, words: Vec<RecognizedWord>) -> SelectionText {
        SelectionText::from_recognized(&RecognizedText {
            text: text.into(),
            words,
            lines: Vec::new(),
            regions: Vec::new(),
        })
    }
    fn row(text: &str, direction: HorizontalDirection) -> SelectionText {
        recognized(
            text,
            vec![word(
                text,
                Rect {
                    x: 10,
                    y: 10,
                    width: 100,
                    height: 30,
                },
                None,
                direction,
                0,
            )],
        )
    }
    fn projected(text: &str, rect: Rect, quad: [(f32, f32); 4]) -> SelectionText {
        recognized(
            text,
            vec![word(
                text,
                rect,
                Some(quad),
                HorizontalDirection::LeftToRight,
                0,
            )],
        )
    }
    fn assert_segment(actual: Option<[Point; 2]>, expected: [(f32, f32); 2]) {
        let actual = actual.expect("实际停点必须有来源截面");
        for (actual, (x, y)) in actual.into_iter().zip(expected) {
            assert!(
                (actual.x - x).abs() < 0.001 && (actual.y - y).abs() < 0.001,
                "actual={actual:?}, expected=({x},{y})"
            );
        }
    }

    #[test]
    fn caret_ltr_line_start_middle_end_use_reading_cross_sections() {
        let text = row("AB", HorizontalDirection::LeftToRight);
        for (byte, x) in [(0, 10.), (1, 60.), (2, 110.)] {
            assert_segment(text.caret_segment(byte), [(x, 10.), (x, 40.)]);
        }
    }

    #[test]
    fn caret_rtl_rect_keeps_logical_edges_without_inventing_upside_down() {
        let text = row("AB", HorizontalDirection::RightToLeft);
        // 旧矩形只能证明物理左右停点；它本来已有的沿序与真实 180° 投影不同。
        for (byte, x) in [(0, 110.), (1, 60.), (2, 10.)] {
            assert_segment(text.caret_segment(byte), [(x, 40.), (x, 10.)]);
        }
    }

    #[test]
    fn caret_rect_uses_existing_stops_instead_of_minimum_cell_width() {
        let mut text = row("AB", HorizontalDirection::LeftToRight);
        // 与原生来源的 max(1px) Cell 一致：形状宽度不能反推出精确停点。
        text.lines[0].stops = vec![(0, 10.), (1, 10.25), (2, 110.)];
        text.lines[0].cells[0].bounds.width = 1.;
        text.lines[0].cells[1].bounds.x = 10.25;
        text.lines[0].cells[1].bounds.width = 99.75;
        assert_segment(text.caret_segment(1), [(10.25, 10.), (10.25, 40.)]);
        text.lines[0].stops.push((1, 10.5));
        assert!(text.caret_segment(1).is_none());
        text.lines[0].stops.retain(|stop| stop.0 != 0);
        assert!(text.caret_segment(0).is_none());
    }

    #[test]
    fn caret_projected_180_keeps_transverse_sides_and_byte_order() {
        let text = projected(
            "AB",
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 30,
            },
            [(110., 40.), (10., 40.), (10., 10.), (110., 10.)],
        );
        for (byte, x) in [(0, 110.), (1, 60.), (2, 10.)] {
            assert_segment(text.caret_segment(byte), [(x, 40.), (x, 10.)]);
        }
    }

    #[test]
    fn caret_vertical_down_and_up_follow_original_bytes() {
        let rect = Rect {
            x: 10,
            y: 10,
            width: 20,
            height: 80,
        };
        for (quad, expected) in [
            (
                [(30., 10.), (30., 90.), (10., 90.), (10., 10.)],
                [
                    ((30., 10.), (10., 10.)),
                    ((30., 50.), (10., 50.)),
                    ((30., 90.), (10., 90.)),
                ],
            ),
            (
                [(10., 90.), (10., 10.), (30., 10.), (30., 90.)],
                [
                    ((10., 90.), (30., 90.)),
                    ((10., 50.), (30., 50.)),
                    ((10., 10.), (30., 10.)),
                ],
            ),
        ] {
            let text = projected("AB", rect, quad);
            for (byte, (start, end)) in expected.into_iter().enumerate() {
                assert_segment(text.caret_segment(byte), [start, end]);
            }
        }
    }

    #[test]
    fn caret_slanted_and_perspective_share_real_projection_edges() {
        let slanted = projected(
            "AB",
            Rect {
                x: 0,
                y: 10,
                width: 90,
                height: 60,
            },
            [(10., 10.), (90., 50.), (80., 70.), (0., 30.)],
        );
        assert_segment(slanted.caret_segment(1), [(50., 30.), (40., 50.)]);
        let perspective = projected(
            "AB",
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 40,
            },
            [(0., 0.), (120., 0.), (80., 40.), (20., 40.)],
        );
        // 独立解析 oracle：x=(120u+40v)/(1+v), y=80v/(1+v)。
        assert_segment(perspective.caret_segment(1), [(60., 0.), (50., 40.)]);
    }

    #[test]
    fn caret_grapheme_combining_zwj_flags_and_crlf_reject_internal_bytes() {
        let content = "e\u{301}👨‍👩‍👧🇨🇳X";
        let text = row(content, HorizontalDirection::LeftToRight);
        let boundaries: Vec<_> = content
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(content.len()))
            .collect();
        for byte in 0..=content.len() {
            assert_eq!(
                text.caret_segment(byte).is_some(),
                boundaries.contains(&byte),
                "byte={byte}"
            );
        }
        assert!(text.caret_segment(usize::MAX).is_none());
        let text = recognized(
            "A\r\nB",
            vec![
                word(
                    "A",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
                word(
                    "B",
                    Rect {
                        x: 0,
                        y: 40,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    1,
                ),
            ],
        );
        assert_segment(text.caret_segment(1), [(20., 0.), (20., 20.)]);
        assert!(text.caret_segment(2).is_none());
        assert_segment(text.caret_segment(3), [(0., 40.), (0., 60.)]);
        let fallback = SelectionText::from_recognized(&RecognizedText {
            text: "A\r\nB".into(),
            words: Vec::new(),
            regions: Vec::new(),
            lines: ["A\r", "B"]
                .into_iter()
                .enumerate()
                .map(|(index, text)| RecognizedLine {
                    graphemes: None,
                    text: text.into(),
                    rect: Some(Rect {
                        x: 0,
                        y: index as u32 * 40,
                        width: if index == 0 { 40 } else { 20 },
                        height: 20,
                    }),
                    geometry: None,
                    horizontal_direction: HorizontalDirection::LeftToRight,
                })
                .collect(),
        });
        assert_segment(fallback.caret_segment(1), [(20., 0.), (20., 20.)]);
        assert!(fallback.caret_segment(2).is_none());
        assert_segment(fallback.caret_segment(3), [(0., 40.), (0., 60.)]);
        assert!(row("", HorizontalDirection::LeftToRight)
            .caret_segment(0)
            .is_none());
    }

    #[test]
    fn caret_missing_space_geometry_never_interpolates_gap() {
        let text = recognized(
            "A  B",
            vec![
                word(
                    "A",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
                word(
                    "B",
                    Rect {
                        x: 100,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
            ],
        );
        assert_segment(text.caret_segment(1), [(20., 0.), (20., 20.)]);
        assert!(text.caret_segment(2).is_none());
        assert_segment(text.caret_segment(3), [(100., 0.), (100., 20.)]);
        let missing = recognized(
            "A\n\nB",
            vec![
                word(
                    "A",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
                word(
                    "B",
                    Rect {
                        x: 0,
                        y: 60,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    2,
                ),
            ],
        );
        assert!(missing.caret_segment(2).is_none());
    }

    #[test]
    fn caret_source_seams_require_unique_physical_cross_section() {
        let first = || {
            word(
                "A",
                Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 20,
                },
                None,
                HorizontalDirection::LeftToRight,
                0,
            )
        };
        let separated = recognized(
            "AB",
            vec![
                first(),
                word(
                    "B",
                    Rect {
                        x: 40,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
            ],
        );
        assert!(separated.caret_segment(1).is_none());
        let adjacent = recognized(
            "AB",
            vec![
                first(),
                word(
                    "B",
                    Rect {
                        x: 20,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    None,
                    HorizontalDirection::LeftToRight,
                    0,
                ),
            ],
        );
        assert_segment(adjacent.caret_segment(1), [(20., 0.), (20., 20.)]);
        let reverse_v = recognized(
            "AB",
            vec![
                first(),
                word(
                    "B",
                    Rect {
                        x: 0,
                        y: 0,
                        width: 20,
                        height: 20,
                    },
                    Some([(20., 20.), (0., 20.), (0., 0.), (20., 0.)]),
                    HorizontalDirection::LeftToRight,
                    0,
                ),
            ],
        );
        assert_segment(reverse_v.caret_segment(1), [(20., 20.), (20., 0.)]);
    }

    #[test]
    fn caret_ambiguous_lines_and_cell_evidence_are_hidden() {
        let mut duplicate = row("AB", HorizontalDirection::LeftToRight);
        let cell = &duplicate.lines[0].cells[0];
        let second = Cell {
            range: cell.range.clone(),
            bounds: cell.bounds,
            projected: cell.projected,
            source_id: cell.source_id,
            reverse_x: cell.reverse_x,
        };
        duplicate.lines[0].cells.push(second);
        assert!(duplicate.caret_segment(1).is_none());
        let mut lines = SelectionText::from_recognized(&RecognizedText {
            text: "A\nB".into(),
            words: Vec::new(),
            regions: Vec::new(),
            lines: ["A", "B"]
                .into_iter()
                .enumerate()
                .map(|(index, text)| RecognizedLine {
                    graphemes: None,
                    text: text.into(),
                    rect: Some(Rect {
                        x: 0,
                        y: index as u32 * 40,
                        width: 20,
                        height: 20,
                    }),
                    geometry: None,
                    horizontal_direction: HorizontalDirection::LeftToRight,
                })
                .collect(),
        });
        lines.lines[1].range = 0..1;
        assert!(lines.caret_segment(0).is_none());
        let mut shape = row("AB", HorizontalDirection::LeftToRight);
        shape.lines[0].cells[0].range = 0..2;
        assert!(shape.caret_segment(1).is_none());
    }

    #[test]
    fn caret_invalid_geometry_is_not_replaced_with_rect_or_line_guess() {
        let mut text = projected(
            "AB",
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 20,
            },
            [(0., 0.), (40., 0.), (40., 20.), (0., 20.)],
        );
        text.lines[0].cells[0]
            .projected
            .as_mut()
            .unwrap()
            .source
            .quad[0]
            .x = f32::NAN;
        assert!(text.caret_segment(0).is_none());
        let mut rect = row("AB", HorizontalDirection::LeftToRight);
        rect.lines[0].cells[0].bounds = Bounds {
            x: 0.,
            y: 0.,
            width: 0.,
            height: 20.,
        };
        assert!(rect.caret_segment(0).is_none());
    }

    #[test]
    fn caret_rotation_and_translation_reuse_original_source_geometry() {
        let mut text = row("AB", HorizontalDirection::LeftToRight);
        text.lines[0].rotation = Rotation::new(point(10., 10.), 90.);
        assert_segment(text.caret_segment(1), [(10., 60.), (-20., 60.)]);
        text.translate(-300., -100.);
        assert_segment(text.caret_segment(1), [(-290., -40.), (-320., -40.)]);
        let mut perspective = projected(
            "AB",
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 40,
            },
            [(0., 0.), (120., 0.), (80., 40.), (20., 40.)],
        );
        perspective.translate(-300., -100.);
        assert_segment(
            perspective.caret_segment(1),
            [(-240., -100.), (-250., -60.)],
        );
    }

    #[test]
    fn caret_read_only_geometry_preserves_selection_marks_and_keyboard_direction() {
        let text = row("AB", HorizontalDirection::LeftToRight);
        let content = text.text.clone();
        let highlights = text.highlights(0..2);
        let marks = text.marking_quads(0..2);
        let snapshot = || {
            text.lines
                .iter()
                .map(|line| {
                    (
                        line.range.clone(),
                        line.bounds,
                        [point(0., 0.), point(1., 0.), point(0., 1.)]
                            .map(|point| line.rotation.map(point)),
                        line.stops.clone(),
                        line.projected,
                        line.cells
                            .iter()
                            .map(|cell| {
                                (
                                    cell.range.clone(),
                                    cell.bounds,
                                    cell.projected.map(|projection| {
                                        (
                                            projection.source,
                                            projection.source_id,
                                            projection.start,
                                            projection.end,
                                        )
                                    }),
                                    cell.source_id,
                                    cell.reverse_x,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let original_lines = snapshot();
        let original_words = text.words.clone();
        let (anchor, _) = text.begin(point(109., 25.), 1).unwrap();
        let cursor = text
            .keyboard_selection_from_drag(&anchor, point(11., 25.))
            .unwrap();
        assert_eq!(cursor.range(), 0..2);
        assert_eq!(cursor.caret_byte(), 0);
        assert_segment(
            text.caret_segment(cursor.caret_byte()),
            [(10., 10.), (10., 40.)],
        );
        let end = text
            .navigate_keyboard(cursor, super::super::NavigationKey::End)
            .unwrap();
        assert_eq!(end.range(), 2..2);
        assert_segment(
            text.caret_segment(end.caret_byte()),
            [(110., 10.), (110., 40.)],
        );
        let home = text
            .navigate_keyboard(end, super::super::NavigationKey::Home)
            .unwrap();
        assert_eq!(home.range(), 0..0);
        assert_segment(
            text.caret_segment(home.caret_byte()),
            [(10., 10.), (10., 40.)],
        );
        assert_eq!(text.text, content);
        assert_eq!(text.highlights(0..2), highlights);
        assert_eq!(text.marking_quads(0..2), marks);
        assert_eq!(text.selected(cursor.range()), Some("AB".to_owned()));
        assert_eq!(snapshot(), original_lines);
        assert_eq!(text.words, original_words);
    }
}
