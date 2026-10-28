//! 保守表格复制合同的纯模型定义；本隔离稿未编译、未执行。
use super::*;
use crate::annotation::Point;
use crate::recognition::{RecognizedLine, RecognizedWord};

fn geometry(rect: Rect, direction: HorizontalDirection) -> TextGeometry {
    let left = rect.x as f32;
    let top = rect.y as f32;
    let right = (rect.x + rect.width) as f32;
    let bottom = (rect.y + rect.height) as f32;
    let points = [
        Point { x: left, y: top },
        Point { x: right, y: top },
        Point {
            x: right,
            y: bottom,
        },
        Point { x: left, y: bottom },
    ];
    TextGeometry {
        quad: if direction == HorizontalDirection::RightToLeft {
            [points[2], points[3], points[0], points[1]]
        } else {
            points
        },
    }
}

fn grid_layout(
    rows: &[&[&str]],
    direction: HorizontalDirection,
    descending_rows: bool,
    descending_columns: bool,
) -> RecognizedText {
    let mut source = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (row, cells) in rows.iter().enumerate() {
        for (column, text) in cells.iter().enumerate() {
            let visual_row = if descending_rows {
                rows.len() - row - 1
            } else {
                row
            };
            let visual_column = if descending_columns {
                cells.len() - column - 1
            } else {
                column
            };
            let rect = Rect {
                x: 10 + visual_column as u32 * 150,
                y: 10 + visual_row as u32 * 40,
                width: 60,
                height: 20,
            };
            let line = source.lines.len();
            source.regions.push(rect);
            source.words.push(RecognizedWord {
                text: (*text).to_owned(),
                rect,
                line,
                horizontal_direction: direction,
                geometry: Some(geometry(rect, direction)),
            });
            source.lines.push(RecognizedLine {
                graphemes: None,
                text: (*text).to_owned(),
                rect: Some(rect),
                horizontal_direction: direction,
                geometry: Some(geometry(rect, direction)),
            });
        }
    }
    sync_text(&mut source);
    source
}

fn grid(rows: &[&[&str]]) -> RecognizedText {
    grid_layout(rows, HorizontalDirection::LeftToRight, false, false)
}

fn sample() -> RecognizedText {
    grid(&[&["甲项", "0"], &["乙项", "0012"], &["丙项", "-3.50%"]])
}

fn sync_text(source: &mut RecognizedText) {
    source.text = source
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
}

fn replace_cell_text(source: &mut RecognizedText, line: usize, text: &str) {
    source.lines[line].text = text.to_owned();
    source.words[line].text = text.to_owned();
    sync_text(source);
}

fn set_cell_rect(source: &mut RecognizedText, line: usize, rect: Rect) {
    let direction = source.lines[line].horizontal_direction;
    source.lines[line].rect = Some(rect);
    source.lines[line].geometry = Some(geometry(rect, direction));
    source.words[line].rect = rect;
    source.words[line].geometry = Some(geometry(rect, direction));
    source.regions[line] = rect;
}

fn split_first_cell(source: &mut RecognizedText, first: &str, second: &str) {
    let rect = source.lines[0].rect.unwrap();
    let first_rect = Rect { width: 25, ..rect };
    let second_rect = Rect {
        x: rect.x + 35,
        width: 25,
        ..rect
    };
    let direction = source.lines[0].horizontal_direction;
    source.words[0].text = first.to_owned();
    source.words[0].rect = first_rect;
    source.words[0].geometry = Some(geometry(first_rect, direction));
    source.regions[0] = first_rect;
    source.words.insert(
        1,
        RecognizedWord {
            text: second.to_owned(),
            rect: second_rect,
            line: 0,
            horizontal_direction: direction,
            geometry: Some(geometry(second_rect, direction)),
        },
    );
    source.regions.insert(1, second_rect);
}

fn decoded_cell(encoded: &str) -> String {
    if let Some(inner) = encoded
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
    {
        inner.replace("\"\"", "\"")
    } else {
        encoded.to_owned()
    }
}

fn assert_partition(source: &RecognizedText, table: &OcrTableCopy) {
    assert_eq!(table.source_range, 0..source.text.len());
    let cells: Vec<_> = table.rows.iter().flatten().collect();
    assert_eq!(cells.len(), source.lines.len());
    assert_eq!(table.separators.len(), cells.len() - 1);
    let mut source_cursor = 0;
    let mut output_cursor = 0;
    let mut restored = String::new();
    for (index, cell) in cells.iter().enumerate() {
        assert_eq!(cell.source_line, index);
        assert_eq!(cell.source_range.start, source_cursor);
        assert_eq!(cell.output_range.start, output_cursor);
        let original = &source.text[cell.source_range.clone()];
        let encoded = &table.text[cell.output_range.clone()];
        assert_eq!(original, source.lines[index].text);
        assert_eq!(decoded_cell(encoded), original);
        assert_eq!(cell.rect, source.lines[index].rect.unwrap());
        assert_eq!(cell.geometry, source.lines[index].geometry.unwrap());
        let original_words: Vec<_> = source
            .words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.line == index)
            .collect();
        assert_eq!(cell.words.len(), original_words.len());
        for (mapped, (word_index, word)) in cell.words.iter().zip(original_words) {
            assert_eq!(mapped.word_index, word_index);
            assert_eq!(&source.text[mapped.source_range.clone()], word.text);
            assert!(mapped.source_range.start >= cell.source_range.start);
            assert!(mapped.source_range.end <= cell.source_range.end);
            assert_eq!(mapped.rect, word.rect);
            assert_eq!(mapped.geometry, word.geometry.unwrap());
        }
        restored.push_str(&decoded_cell(encoded));
        source_cursor = cell.source_range.end;
        output_cursor = cell.output_range.end;
        if let Some(separator) = table.separators.get(index) {
            assert_eq!(separator.source_range, source_cursor..source_cursor + 1);
            assert_eq!(&source.text[separator.source_range.clone()], "\n");
            assert_eq!(separator.output_range.start, output_cursor);
            let expected = if (index + 1) % table.rows[0].len() == 0 {
                assert_eq!(separator.kind, TableSeparatorKind::Row);
                "\r\n"
            } else {
                assert_eq!(separator.kind, TableSeparatorKind::Column);
                "\t"
            };
            assert_eq!(&table.text[separator.output_range.clone()], expected);
            restored.push('\n');
            source_cursor = separator.source_range.end;
            output_cursor = separator.output_range.end;
        }
    }
    assert_eq!(source_cursor, source.text.len());
    assert_eq!(output_cursor, table.text.len());
    assert_eq!(restored, source.text);
}

#[derive(Debug, PartialEq)]
struct WordSnapshot {
    text: String,
    rect: Rect,
    line: usize,
    direction: HorizontalDirection,
    geometry: Option<TextGeometry>,
}

#[derive(Debug, PartialEq)]
struct LineSnapshot {
    text: String,
    rect: Option<Rect>,
    direction: HorizontalDirection,
    geometry: Option<TextGeometry>,
}

#[derive(Debug, PartialEq)]
struct SourceSnapshot {
    text: String,
    regions: Vec<Rect>,
    words: Vec<WordSnapshot>,
    lines: Vec<LineSnapshot>,
}

fn snapshot(source: &RecognizedText) -> SourceSnapshot {
    SourceSnapshot {
        text: source.text.clone(),
        regions: source.regions.clone(),
        words: source
            .words
            .iter()
            .map(|word| WordSnapshot {
                text: word.text.clone(),
                rect: word.rect,
                line: word.line,
                direction: word.horizontal_direction,
                geometry: word.geometry,
            })
            .collect(),
        lines: source
            .lines
            .iter()
            .map(|line| LineSnapshot {
                text: line.text.clone(),
                rect: line.rect,
                direction: line.horizontal_direction,
                geometry: line.geometry,
            })
            .collect(),
    }
}

#[test]
fn numeric_cells_keep_zero_leading_zero_sign_decimal_and_percent() {
    let source = sample();
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.text, "甲项\t0\r\n乙项\t0012\r\n丙项\t-3.50%");
    assert_eq!(table.rows.len(), 3);
    assert!(table.rows.iter().all(|row| row.len() == 2));
    assert!(!table.text.ends_with('\n'));
    assert_partition(&source, &table);
}

#[test]
fn recognized_text_copy_table_exposes_the_same_pure_result() {
    let source = sample();
    assert_eq!(source.copy_table(), OcrTableCopy::new(&source));
    let ordinary = grid(&[&["一句正文"], &["下一句正文"], &["末句正文"]]);
    assert_eq!(ordinary.copy_table(), None);
}

#[test]
fn utf8_mapping_uses_bytes_for_multibyte_cell_and_newline_boundaries() {
    let source = grid(&[&["汉🙂", "10"], &["乙", "20"], &["丙", "30"]]);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.rows[0][0].source_range, 0..7);
    assert_eq!(table.rows[0][0].output_range, 0..7);
    assert_eq!(table.separators[0].source_range, 7..8);
    assert_eq!(table.rows[0][1].source_range, 8..10);
    assert_partition(&source, &table);
}

#[test]
fn ordinary_spaces_and_multiple_words_remain_one_cell() {
    let mut source = grid(&[&["华 东", " 0012 "], &["西区", "0"], &["北区", "+2.0%"]]);
    split_first_cell(&mut source, "华", "东");
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.text, "华 东\t 0012 \r\n西区\t0\r\n北区\t+2.0%");
    assert_eq!(table.rows[0][0].words.len(), 2);
    assert_eq!(table.rows[0][0].words[0].source_range, 0..3);
    assert_eq!(table.rows[0][0].words[1].source_range, 4..7);
    assert_partition(&source, &table);

    // CTC 估算带量化后可重叠一个物理像素，原文空格仍是来源的一部分。
    let mut quantized = grid(&[&["华 东", "1"], &["西区", "2"], &["北区", "3"]]);
    split_first_cell(&mut quantized, "华", "东");
    quantized.words[0].rect.width = 30;
    quantized.words[1].rect.x = 39;
    quantized.words[1].rect.width = 31;
    for index in [0, 1] {
        quantized.words[index].geometry = Some(geometry(
            quantized.words[index].rect,
            HorizontalDirection::LeftToRight,
        ));
        quantized.regions[index] = quantized.words[index].rect;
    }
    let table = OcrTableCopy::new(&quantized).unwrap();
    assert_partition(&quantized, &table);
    quantized.words[1].rect.x = 38;
    quantized.words[1].rect.width = 32;
    quantized.words[1].geometry = Some(geometry(
        quantized.words[1].rect,
        HorizontalDirection::LeftToRight,
    ));
    quantized.regions[1] = quantized.words[1].rect;
    assert!(OcrTableCopy::new(&quantized).is_none());
}

#[test]
fn different_word_baselines_cannot_form_one_cell_from_their_rect_union() {
    let mut source = sample();
    replace_cell_text(&mut source, 0, "华 东");
    split_first_cell(&mut source, "华", "东");
    source.words[0].rect.height = 10;
    source.words[1].rect.y = 20;
    source.words[1].rect.height = 10;
    for index in [0, 1] {
        source.words[index].geometry = Some(geometry(
            source.words[index].rect,
            HorizontalDirection::LeftToRight,
        ));
        source.regions[index] = source.words[index].rect;
    }
    assert_eq!(
        source.lines[0].rect,
        Some(Rect {
            x: 10,
            y: 10,
            width: 60,
            height: 20
        })
    );
    let before = snapshot(&source);
    assert!(OcrTableCopy::new(&source).is_none());
    assert_eq!(snapshot(&source), before);
}

#[test]
fn quotes_are_encoded_for_tsv_while_mapping_restores_original_cell() {
    let source = grid(&[&["A\"B", "1"], &["\"", "2"], &["plain", "3"]]);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.text, "\"A\"\"B\"\t1\r\n\"\"\"\"\t2\r\nplain\t3");
    assert_eq!(table.rows[0][0].source_range, 0..3);
    assert_eq!(table.rows[0][0].output_range, 0..6);
    assert_partition(&source, &table);
}

#[test]
fn formula_shaped_text_is_not_parsed_or_rewritten() {
    let source = grid(&[&["=SUM(A1:A2)", "0"], &["+A1", "0012"], &["@name", "3"]]);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.text, "=SUM(A1:A2)\t0\r\n+A1\t0012\r\n@name\t3");
    assert_partition(&source, &table);
}

#[test]
fn one_complete_header_precedes_three_numeric_data_rows() {
    let source = grid(&[
        &["项目", "数量"],
        &["甲", "0"],
        &["乙", "0012"],
        &["丙", "-3.50%"],
    ]);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(table.rows.len(), 4);
    assert_eq!(table.text, "项目\t数量\r\n甲\t0\r\n乙\t0012\r\n丙\t-3.50%");
    assert_partition(&source, &table);
}

#[test]
fn repeated_cell_text_keeps_distinct_source_ranges_and_geometry() {
    let mut source = grid(&[&["同 同", "1"], &["同 同", "1"], &["同 同", "1"]]);
    split_first_cell(&mut source, "同", "同");
    let table = OcrTableCopy::new(&source).unwrap();
    assert_ne!(table.rows[0][0].source_range, table.rows[1][0].source_range);
    assert_ne!(table.rows[0][0].geometry, table.rows[1][0].geometry);
    assert_eq!(table.rows[0][0].words[0].source_range, 0..3);
    assert_eq!(table.rows[0][0].words[1].source_range, 4..7);
    assert_eq!(table.rows[2][0].source_line, 4);
    assert_partition(&source, &table);
}

#[test]
fn upright_source_order_survives_descending_rows_or_columns() {
    for (descending_rows, descending_columns) in [(true, false), (false, true), (true, true)] {
        let source = grid_layout(
            &[&["甲", "0"], &["乙", "0012"], &["丙", "3"]],
            HorizontalDirection::LeftToRight,
            descending_rows,
            descending_columns,
        );
        let table = OcrTableCopy::new(&source).unwrap();
        assert_eq!(table.text, "甲\t0\r\n乙\t0012\r\n丙\t3");
        assert_partition(&source, &table);
    }
}

#[test]
fn rotated_180_content_keeps_existing_reading_sequence() {
    for (descending_rows, descending_columns) in [(false, false), (true, true)] {
        let source = grid_layout(
            &[&["甲", "0"], &["乙", "0012"], &["丙", "3"]],
            HorizontalDirection::RightToLeft,
            descending_rows,
            descending_columns,
        );
        let table = OcrTableCopy::new(&source).unwrap();
        assert_eq!(table.horizontal_direction, HorizontalDirection::RightToLeft);
        assert_eq!(table.text, "甲\t0\r\n乙\t0012\r\n丙\t3");
        assert_partition(&source, &table);
    }
}

#[test]
fn variable_width_numeric_column_can_have_stable_right_anchor() {
    let mut source = sample();
    for (line, width) in [(1, 20), (3, 40), (5, 60)] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(
            &mut source,
            line,
            Rect {
                x: 220 - width,
                width,
                ..old
            },
        );
    }
    let table = OcrTableCopy::new(&source).unwrap();
    assert_partition(&source, &table);
}

#[test]
fn variable_width_numeric_column_can_have_stable_center_anchor() {
    let mut source = sample();
    for (line, width) in [(1, 20), (3, 40), (5, 60)] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(
            &mut source,
            line,
            Rect {
                x: 190 - width / 2,
                width,
                ..old
            },
        );
    }
    let table = OcrTableCopy::new(&source).unwrap();
    assert_partition(&source, &table);
}

#[test]
fn construction_never_changes_source_text_regions_words_or_lines() {
    let mut source = sample();
    let before = snapshot(&source);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(snapshot(&source), before);
    assert_partition(&source, &table);
    replace_cell_text(&mut source, 1, "not numeric");
    let before_rejected = snapshot(&source);
    assert!(OcrTableCopy::new(&source).is_none());
    assert_eq!(snapshot(&source), before_rejected);
}

#[test]
fn tightened_word_height_can_keep_original_rect_envelope() {
    let mut source = sample();
    for word in &mut source.words {
        word.geometry = Some(geometry(
            Rect {
                y: word.rect.y + 5,
                height: 10,
                ..word.rect
            },
            word.horizontal_direction,
        ));
    }
    let table = OcrTableCopy::new(&source).unwrap();
    assert_partition(&source, &table);
}

#[test]
fn full_crop_projection_can_extend_beyond_word_union_rect() {
    let mut source = sample();
    for line in &mut source.lines {
        let rect = line.rect.unwrap();
        line.geometry = Some(geometry(
            Rect {
                x: rect.x - 2,
                y: rect.y - 2,
                width: rect.width + 4,
                height: rect.height + 4,
            },
            line.horizontal_direction,
        ));
    }
    let table = OcrTableCopy::new(&source).unwrap();
    assert_partition(&source, &table);
}

#[test]
fn ordinary_single_column_paragraph_is_not_a_table() {
    let source = grid(&[&["第一句正文"], &["第二句正文"], &["第三句正文"]]);
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn aligned_two_column_prose_without_numeric_evidence_is_not_a_table() {
    let source = grid(&[
        &["左侧正文", "右侧正文"],
        &["下一句", "另一句"],
        &["末段", "尾句"],
    ]);
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn two_rows_and_header_with_only_two_data_rows_are_insufficient() {
    let short = grid(&[&["甲", "1"], &["乙", "2"]]);
    assert!(OcrTableCopy::new(&short).is_none());
    let short_header = grid(&[&["项目", "数量"], &["甲", "1"], &["乙", "2"]]);
    assert!(OcrTableCopy::new(&short_header).is_none());
}

#[test]
fn spaces_inside_lines_do_not_create_columns() {
    let source = grid(&[&["甲  0"], &["乙  0012"], &["丙  3"]]);
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn missing_cell_and_full_width_merged_row_are_not_filled_in() {
    let missing = grid(&[&["甲", "0"], &["乙"], &["丙", "3"]]);
    assert!(OcrTableCopy::new(&missing).is_none());
    let mut merged = grid(&[&["甲", "0"], &["合并内容"], &["丙", "3"]]);
    let old = merged.lines[2].rect.unwrap();
    set_cell_rect(&mut merged, 2, Rect { width: 210, ..old });
    assert!(OcrTableCopy::new(&merged).is_none());
}

#[test]
fn title_spanning_columns_does_not_become_a_partial_table() {
    let mut source = grid(&[&["报表标题"], &["甲", "0"], &["乙", "1"], &["丙", "2"]]);
    let old = source.lines[0].rect.unwrap();
    set_cell_rect(&mut source, 0, Rect { width: 210, ..old });
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn unstable_column_anchor_rejects_ambiguous_assignment() {
    let mut source = sample();
    let old = source.lines[3].rect.unwrap();
    set_cell_rect(
        &mut source,
        3,
        Rect {
            x: old.x + 12,
            ..old
        },
    );
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn narrow_common_gutter_rejects_prose_or_column_ambiguity() {
    let mut source = sample();
    for line in [1, 3, 5] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(&mut source, line, Rect { x: 100, ..old });
    }
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn inconsistent_cell_heights_reject_the_whole_candidate() {
    let mut source = sample();
    let old = source.lines[2].rect.unwrap();
    set_cell_rect(&mut source, 2, Rect { height: 35, ..old });
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn irregular_row_spacing_rejects_mixed_sections() {
    let mut source = sample();
    for line in [4, 5] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(&mut source, line, Rect { y: 125, ..old });
    }
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn source_rows_must_progress_in_one_visual_direction() {
    let mut source = sample();
    for (line, y) in [(2, 90), (3, 90), (4, 50), (5, 50)] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(&mut source, line, Rect { y, ..old });
    }
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn column_source_sequence_must_match_across_rows() {
    let mut source = sample();
    for (line, x) in [(2, 160), (3, 10)] {
        let old = source.lines[line].rect.unwrap();
        set_cell_rect(&mut source, line, Rect { x, ..old });
    }
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn mixed_180_and_upright_cells_are_not_one_trusted_table() {
    let mut source = sample();
    let rect = source.lines[2].rect.unwrap();
    source.lines[2].horizontal_direction = HorizontalDirection::RightToLeft;
    source.lines[2].geometry = Some(geometry(rect, HorizontalDirection::RightToLeft));
    source.words[2].horizontal_direction = HorizontalDirection::RightToLeft;
    source.words[2].geometry = source.lines[2].geometry;
    assert!(OcrTableCopy::new(&source).is_none());
    let mut mismatched_word = sample();
    mismatched_word.words[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(OcrTableCopy::new(&mismatched_word).is_none());
}

#[test]
fn vertical_reading_projection_is_not_horizontal_table_evidence() {
    let mut source = sample();
    let q = source.lines[0].geometry.unwrap().quad;
    source.lines[0].geometry = Some(TextGeometry {
        quad: [q[1], q[2], q[3], q[0]],
    });
    source.words[0].geometry = source.lines[0].geometry;
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn slanted_projection_is_not_snapped_into_table_columns() {
    let mut source = sample();
    let mut geom = source.lines[0].geometry.unwrap();
    geom.quad[1].y += 2.;
    geom.quad[2].y += 2.;
    source.lines[0].geometry = Some(geom);
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn perspective_crop_is_not_replaced_by_axis_aligned_bounds() {
    let mut source = sample();
    let mut geom = source.lines[0].geometry.unwrap();
    geom.quad[1].x -= 3.;
    source.lines[0].geometry = Some(geom);
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn missing_projection_or_word_outside_line_crop_is_insufficient() {
    let mut missing = sample();
    missing.lines[0].geometry = None;
    assert!(OcrTableCopy::new(&missing).is_none());
    let mut outside = sample();
    let rect = outside.lines[0].rect.unwrap();
    outside.lines[0].geometry = Some(geometry(
        Rect {
            x: rect.x + 1,
            width: rect.width - 1,
            ..rect
        },
        HorizontalDirection::LeftToRight,
    ));
    assert!(OcrTableCopy::new(&outside).is_none());
}

#[test]
fn region_word_union_and_line_index_must_all_agree() {
    let mut regions = sample();
    regions.regions[0].width -= 1;
    assert!(OcrTableCopy::new(&regions).is_none());
    let mut union = sample();
    union.lines[0].rect.as_mut().unwrap().width += 1;
    assert!(OcrTableCopy::new(&union).is_none());
    let mut index = sample();
    index.words[0].line = index.lines.len();
    assert!(OcrTableCopy::new(&index).is_none());
}

#[test]
fn nonfinite_degenerate_and_overflow_geometry_are_rejected() {
    let mut nonfinite = sample();
    nonfinite.lines[0].geometry.as_mut().unwrap().quad[0].x = f32::NAN;
    assert!(OcrTableCopy::new(&nonfinite).is_none());
    let mut degenerate = sample();
    let first = degenerate.lines[0].geometry.unwrap().quad[0];
    degenerate.lines[0].geometry.as_mut().unwrap().quad[1] = first;
    assert!(OcrTableCopy::new(&degenerate).is_none());
    let mut overflow = sample();
    overflow.lines[0].rect.as_mut().unwrap().x = u32::MAX;
    overflow.words[0].rect.x = u32::MAX;
    overflow.regions[0].x = u32::MAX;
    assert!(OcrTableCopy::new(&overflow).is_none());
}

#[test]
fn full_text_and_word_ownership_must_match_original_lines_exactly() {
    let mut text = sample();
    text.text.push('!');
    assert!(OcrTableCopy::new(&text).is_none());
    let mut words = sample();
    words.words[0].text = "错误文本".to_owned();
    assert!(OcrTableCopy::new(&words).is_none());
    let mut gap = sample();
    replace_cell_text(&mut gap, 0, "甲乙");
    gap.words[0].text = "甲".to_owned();
    assert!(OcrTableCopy::new(&gap).is_none());
}

#[test]
fn combining_grapheme_cannot_be_split_between_word_sources() {
    let mut source = sample();
    replace_cell_text(&mut source, 0, "甲e\u{301}");
    split_first_cell(&mut source, "甲e", "\u{301}");
    assert!(OcrTableCopy::new(&source).is_none());
}

#[test]
fn embedded_tab_newline_control_and_unicode_line_separator_are_rejected() {
    for text in [
        "甲\t乙",
        "甲\r乙",
        "甲\n乙",
        "甲\0乙",
        "甲\u{7f}乙",
        "甲\u{2028}乙",
        "甲\u{2029}乙",
    ] {
        let mut source = sample();
        replace_cell_text(&mut source, 0, text);
        assert!(
            OcrTableCopy::new(&source).is_none(),
            "禁止控制文本：{text:?}"
        );
    }
}

#[test]
fn unicode_blank_cells_are_not_admitted_by_neighbor_numeric_evidence() {
    for blank in ["\u{a0}", "\u{2000}", "\u{3000}"] {
        let source = grid(&[&[blank, "1"], &[blank, "2"], &[blank, "3"]]);
        assert!(OcrTableCopy::new(&source).is_none());
    }
    let source = grid(&[&["甲\u{a0}乙", "1"], &["丙", "2"], &["丁", "3"]]);
    let table = OcrTableCopy::new(&source).unwrap();
    assert_eq!(
        &table.text[table.rows[0][0].output_range.clone()],
        "甲\u{a0}乙"
    );
    assert_partition(&source, &table);
}

#[test]
fn numeric_evidence_accepts_only_complete_ascii_numeric_shape() {
    for value in [
        "1,000", "1e3", ".5", "3.", "+", "--1", "１", "1%%", "12a", "=1",
    ] {
        let mut source = sample();
        replace_cell_text(&mut source, 3, value);
        assert!(
            OcrTableCopy::new(&source).is_none(),
            "非完整数字形状：{value}"
        );
    }
}

#[test]
fn long_cells_and_bounded_row_column_limits_reject_without_partial_output() {
    let mut largest_cell = sample();
    replace_cell_text(&mut largest_cell, 0, &"甲".repeat(32));
    assert!(OcrTableCopy::new(&largest_cell).is_some());
    let mut long_cell = sample();
    replace_cell_text(&mut long_cell, 0, &"甲".repeat(33));
    assert!(OcrTableCopy::new(&long_cell).is_none());
    let many_rows: Vec<&[&str]> = vec![&["甲", "1"]; 257];
    assert!(OcrTableCopy::new(&grid(&many_rows[..256])).is_some());
    assert!(OcrTableCopy::new(&grid(&many_rows)).is_none());
    let wide_row: Vec<&str> = vec!["1"; 17];
    let largest_columns = grid(&[&wide_row[..16], &wide_row[..16], &wide_row[..16]]);
    assert!(OcrTableCopy::new(&largest_columns).is_some());
    let many_columns = grid(&[&wide_row, &wide_row, &wide_row]);
    assert!(OcrTableCopy::new(&many_columns).is_none());
}

#[test]
fn excessive_word_sources_reject_before_grouping_or_partial_output() {
    let mut source = sample();
    let rect = source.words[0].rect;
    for _ in source.words.len()..32_769 {
        source.regions.push(rect);
        source.words.push(RecognizedWord {
            text: "甲项".to_owned(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(geometry(rect, HorizontalDirection::LeftToRight)),
        });
    }
    let before = snapshot(&source);
    assert!(OcrTableCopy::new(&source).is_none());
    assert_eq!(snapshot(&source), before);
}

// 第二阶段：一个原始 RecognizedLine 包含一整物理行。以下仅定义纯模型，未执行。
fn inline_layout(
    rows: &[&[&str]],
    direction: HorizontalDirection,
    descending_rows: bool,
) -> RecognizedText {
    let mut source = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (row, cells) in rows.iter().enumerate() {
        let visual_row = if descending_rows {
            rows.len() - row - 1
        } else {
            row
        };
        let y = 10 + visual_row as u32 * 40;
        let line_rect = Rect {
            x: 10,
            y,
            width: (cells.len() as u32 - 1) * 150 + 60,
            height: 20,
        };
        source.lines.push(RecognizedLine {
            graphemes: None,
            text: cells.join("    "),
            rect: Some(line_rect),
            horizontal_direction: direction,
            geometry: Some(geometry(line_rect, direction)),
        });
        for (column, text) in cells.iter().enumerate() {
            let visual_column = if direction == HorizontalDirection::RightToLeft {
                cells.len() - column - 1
            } else {
                column
            };
            let rect = Rect {
                x: 10 + visual_column as u32 * 150,
                y,
                width: 60,
                height: 20,
            };
            source.regions.push(rect);
            source.words.push(RecognizedWord {
                text: text.trim_matches(' ').to_owned(),
                rect,
                line: row,
                horizontal_direction: direction,
                geometry: Some(geometry(rect, direction)),
            });
        }
    }
    sync_text(&mut source);
    source
}

fn inline_grid(rows: &[&[&str]]) -> RecognizedText {
    inline_layout(rows, HorizontalDirection::LeftToRight, false)
}

fn inline_sample() -> RecognizedText {
    inline_grid(&[
        &["甲项", "0", "东区"],
        &["乙项", "0012", "西区"],
        &["丙项", "-3.50%", "北区"],
    ])
}

fn inline_refresh_line(source: &mut RecognizedText, line_index: usize) {
    let words: Vec<_> = source
        .words
        .iter()
        .filter(|word| word.line == line_index)
        .collect();
    let x = words.iter().map(|word| word.rect.x).min().unwrap();
    let y = words.iter().map(|word| word.rect.y).min().unwrap();
    let right = words
        .iter()
        .map(|word| word.rect.x + word.rect.width)
        .max()
        .unwrap();
    let bottom = words
        .iter()
        .map(|word| word.rect.y + word.rect.height)
        .max()
        .unwrap();
    let rect = Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    };
    let line = &mut source.lines[line_index];
    line.rect = Some(rect);
    line.geometry = Some(geometry(rect, line.horizontal_direction));
}

fn inline_set_word_rect(source: &mut RecognizedText, word_index: usize, rect: Rect) {
    let word = &mut source.words[word_index];
    word.rect = rect;
    word.geometry = Some(geometry(rect, word.horizontal_direction));
    let line = word.line;
    source.regions[word_index] = rect;
    inline_refresh_line(source, line);
}

fn inline_split_word(source: &mut RecognizedText, word_index: usize, first: &str, second: &str) {
    let original = &source.words[word_index];
    let rect = original.rect;
    let direction = original.horizontal_direction;
    let line = original.line;
    let (first_x, second_x) = if direction == HorizontalDirection::RightToLeft {
        (rect.x + 35, rect.x)
    } else {
        (rect.x, rect.x + 35)
    };
    let first_rect = Rect {
        x: first_x,
        width: 25,
        ..rect
    };
    let second_rect = Rect {
        x: second_x,
        width: 25,
        ..rect
    };
    source.words[word_index].text = first.to_owned();
    source.words[word_index].rect = first_rect;
    source.words[word_index].geometry = Some(geometry(first_rect, direction));
    source.regions[word_index] = first_rect;
    source.words.insert(
        word_index + 1,
        RecognizedWord {
            text: second.to_owned(),
            rect: second_rect,
            line,
            horizontal_direction: direction,
            geometry: Some(geometry(second_rect, direction)),
        },
    );
    source.regions.insert(word_index + 1, second_rect);
}

fn inline_assert_partition(source: &RecognizedText, table: &OcrTableCopy) {
    assert_eq!(table.source_range, 0..source.text.len());
    assert_eq!(table.rows.len(), source.lines.len());
    let columns = table.rows[0].len();
    assert!(columns >= 2);
    assert!(table.rows.iter().all(|row| row.len() == columns));
    let cells: Vec<_> = table.rows.iter().flatten().collect();
    assert_eq!(table.separators.len(), cells.len() - 1);
    let mut source_cursor = 0;
    let mut output_cursor = 0;
    let mut word_cursor = 0;
    let mut restored = String::new();
    for (index, cell) in cells.iter().enumerate() {
        assert_eq!(cell.source_line, index / columns);
        assert_eq!(cell.source_range.start, source_cursor);
        assert_eq!(cell.output_range.start, output_cursor);
        let original = &source.text[cell.source_range.clone()];
        let encoded = &table.text[cell.output_range.clone()];
        assert_eq!(decoded_cell(encoded), original);
        assert!(!cell.words.is_empty());
        for mapped in &cell.words {
            let word = &source.words[word_cursor];
            assert_eq!(mapped.word_index, word_cursor);
            assert_eq!(word.line, cell.source_line);
            assert_eq!(&source.text[mapped.source_range.clone()], word.text);
            assert!(mapped.source_range.start >= cell.source_range.start);
            assert!(mapped.source_range.end <= cell.source_range.end);
            assert_eq!(mapped.rect, word.rect);
            assert_eq!(mapped.geometry, word.geometry.unwrap());
            word_cursor += 1;
        }
        restored.push_str(&decoded_cell(encoded));
        source_cursor = cell.source_range.end;
        output_cursor = cell.output_range.end;
        if let Some(separator) = table.separators.get(index) {
            assert_eq!(separator.source_range.start, source_cursor);
            assert_eq!(separator.output_range.start, output_cursor);
            let original_separator = &source.text[separator.source_range.clone()];
            let expected = if (index + 1) % columns == 0 {
                assert_eq!(separator.kind, TableSeparatorKind::Row);
                assert_eq!(original_separator, "\n");
                "\r\n"
            } else {
                assert_eq!(separator.kind, TableSeparatorKind::Column);
                assert!(!original_separator.is_empty());
                assert!(original_separator.bytes().all(|byte| byte == b' '));
                "\t"
            };
            assert_eq!(&table.text[separator.output_range.clone()], expected);
            restored.push_str(original_separator);
            source_cursor = separator.source_range.end;
            output_cursor = separator.output_range.end;
        }
    }
    assert_eq!(source_cursor, source.text.len());
    assert_eq!(output_cursor, table.text.len());
    assert_eq!(word_cursor, source.words.len());
    assert_eq!(restored, source.text);
}

fn inline_accept(source: &RecognizedText) -> OcrTableCopy {
    let before = snapshot(source);
    let table = OcrTableCopy::new(source).expect("完整且可信的同行多单元格");
    assert_eq!(snapshot(source), before);
    inline_assert_partition(source, &table);
    table
}

fn inline_reject(source: &RecognizedText) {
    let before = snapshot(source);
    assert!(OcrTableCopy::new(source).is_none());
    assert_eq!(snapshot(source), before);
}

fn inline_assert_geometry(actual: TextGeometry, expected: TextGeometry) {
    for (actual, expected) in actual.quad.into_iter().zip(expected.quad) {
        assert!((actual.x - expected.x).abs() <= 0.001);
        assert!((actual.y - expected.y).abs() <= 0.001);
    }
}

#[test]
fn inline_three_complete_rows_keep_numeric_spelling_and_repeated_line_ownership() {
    let source = inline_sample();
    let table = inline_accept(&source);
    assert_eq!(
        table.text,
        "甲项\t0\t东区\r\n乙项\t0012\t西区\r\n丙项\t-3.50%\t北区"
    );
    assert_eq!(table.rows[0][0].source_line, 0);
    assert_eq!(table.rows[0][2].source_line, 0);
    assert_eq!(table.rows[1][0].source_line, 1);
    assert_eq!(source.copy_table(), Some(table));
}

#[test]
fn inline_real_column_spaces_are_separate_while_cell_internal_and_outer_spaces_survive() {
    let mut source = inline_grid(&[
        &["  华 东", "0", "东区  "],
        &["  华 东", "0012", "西区  "],
        &["  华 东", "3", "北区  "],
    ]);
    for word in [6, 3, 0] {
        inline_split_word(&mut source, word, "华", "东");
    }
    let table = inline_accept(&source);
    assert_eq!(
        table.text,
        "  华 东\t0\t东区  \r\n  华 东\t0012\t西区  \r\n  华 东\t3\t北区  "
    );
    assert_eq!(
        &source.text[table.rows[0][0].source_range.clone()],
        "  华 东"
    );
    assert_eq!(
        &source.text[table.separators[0].source_range.clone()],
        "    "
    );
    assert_eq!(table.rows[0][0].words.len(), 2);
    assert_eq!(table.rows[0][0].words[0].source_range, 2..5);
    assert_eq!(table.rows[0][0].words[1].source_range, 6..9);
}

#[test]
fn inline_repeated_cjk_and_complete_emoji_combining_graphemes_keep_distinct_byte_sources() {
    let mut source = inline_grid(&[
        &["同同", "0", "e\u{301} 👩‍💻"],
        &["同同", "1", "e\u{301} 👩‍💻"],
        &["同同", "2", "e\u{301} 👩‍💻"],
    ]);
    for word in [8, 5, 2] {
        inline_split_word(&mut source, word, "e\u{301}", "👩‍💻");
    }
    for word in [8, 4, 0] {
        inline_split_word(&mut source, word, "同", "同");
    }
    let table = inline_accept(&source);
    assert_eq!(table.rows[0][0].words[0].source_range, 0..3);
    assert_eq!(table.rows[0][0].words[1].source_range, 3..6);
    assert_eq!(table.rows[0][2].words.len(), 2);
    assert_ne!(
        table.rows[0][0].words[0].source_range,
        table.rows[2][0].words[0].source_range
    );
    assert_eq!(table.text.matches("👩‍💻").count(), 3);
}

#[test]
fn inline_quote_encoding_and_formula_shaped_labels_are_lossless() {
    let source = inline_grid(&[
        &["A\"B", "0", "=SUM(A1:A2)"],
        &["\"", "0012", "+A1"],
        &["plain", "3", "@name"],
    ]);
    let table = inline_accept(&source);
    assert_eq!(
        table.text,
        "\"A\"\"B\"\t0\t=SUM(A1:A2)\r\n\"\"\"\"\t0012\t+A1\r\nplain\t3\t@name"
    );
    assert_eq!(table.rows[0][0].source_range, 0..3);
    assert_eq!(table.rows[0][0].output_range, 0..6);
}

#[test]
fn inline_complete_nonnumeric_header_requires_three_following_data_rows() {
    let rows: &[&[&str]] = &[
        &["项目", "数量", "地区"],
        &["甲", "0", "东区"],
        &["乙", "0012", "西区"],
        &["丙", "3", "北区"],
    ];
    let table = inline_accept(&inline_grid(rows));
    assert_eq!(table.rows.len(), 4);
    assert!(table.text.starts_with("项目\t数量\t地区\r\n"));
    inline_reject(&inline_grid(&rows[..3]));
    inline_reject(&inline_grid(&[
        &["项目", "0", "地区"],
        &["甲", "文字", "东区"],
        &["乙", "1", "西区"],
        &["丙", "2", "北区"],
    ]));
}

#[test]
fn inline_rotated_180_and_descending_rows_preserve_existing_source_reading_order() {
    for direction in [
        HorizontalDirection::LeftToRight,
        HorizontalDirection::RightToLeft,
    ] {
        for descending_rows in [false, true] {
            let source = inline_layout(
                &[
                    &["甲", "0", "东"],
                    &["乙", "0012", "西"],
                    &["丙", "3", "北"],
                ],
                direction,
                descending_rows,
            );
            let table = inline_accept(&source);
            assert_eq!(table.horizontal_direction, direction);
            assert_eq!(table.text, "甲\t0\t东\r\n乙\t0012\t西\r\n丙\t3\t北");
            if direction == HorizontalDirection::RightToLeft {
                assert!(table.rows[0][0].rect.x > table.rows[0][1].rect.x);
                assert!(table.rows[0][0].geometry.quad[0].x > table.rows[0][0].geometry.quad[1].x);
            }
        }
    }
}

#[test]
fn inline_cell_geometry_uses_original_full_crop_v_and_word_u_in_both_directions() {
    for direction in [
        HorizontalDirection::LeftToRight,
        HorizontalDirection::RightToLeft,
    ] {
        let mut source = inline_layout(
            &[&["甲", "0", "东"], &["乙", "1", "西"], &["丙", "2", "北"]],
            direction,
            false,
        );
        for line in &mut source.lines {
            let rect = line.rect.unwrap();
            line.geometry = Some(geometry(
                Rect {
                    x: rect.x - 2,
                    y: rect.y - 2,
                    width: rect.width + 4,
                    height: rect.height + 4,
                },
                direction,
            ));
        }
        for word in &mut source.words {
            word.geometry = Some(geometry(
                Rect {
                    x: word.rect.x + 5,
                    y: word.rect.y + 5,
                    width: word.rect.width - 10,
                    height: 10,
                },
                direction,
            ));
        }
        let table = inline_accept(&source);
        for cell in table.rows.iter().flatten() {
            let original = &source.words[cell.words[0].word_index];
            assert_eq!(cell.rect, original.rect);
            inline_assert_geometry(
                cell.geometry,
                geometry(
                    Rect {
                        x: original.rect.x + 5,
                        y: original.rect.y - 2,
                        width: original.rect.width - 10,
                        height: 24,
                    },
                    direction,
                ),
            );
            assert_ne!(cell.geometry, geometry(cell.rect, direction));
        }
    }
}

#[test]
fn inline_ctc_one_pixel_overlap_stays_in_cell_but_larger_overlap_is_rejected() {
    let mut source = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    inline_split_word(&mut source, 0, "华", "东");
    let first = source.words[0].rect;
    let second = source.words[1].rect;
    inline_set_word_rect(&mut source, 0, Rect { width: 30, ..first });
    inline_set_word_rect(
        &mut source,
        1,
        Rect {
            x: 39,
            width: 31,
            ..second
        },
    );
    let table = inline_accept(&source);
    assert_eq!(table.rows[0][0].words.len(), 2);
    inline_set_word_rect(
        &mut source,
        1,
        Rect {
            x: 38,
            width: 32,
            ..second
        },
    );
    inline_reject(&source);
}

#[test]
fn inline_ink_tight_height_does_not_turn_ordinary_original_rect_gap_into_column() {
    let mut source = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    inline_split_word(&mut source, 0, "华", "东");
    let first = source.words[0].rect;
    inline_set_word_rect(&mut source, 0, Rect { width: 20, ..first });
    for word in &mut source.words {
        word.geometry = Some(geometry(
            Rect {
                y: word.rect.y + 7,
                height: 6,
                ..word.rect
            },
            word.horizontal_direction,
        ));
    }
    let table = inline_accept(&source);
    assert_eq!(table.rows[0][0].words.len(), 2);
    assert_eq!(&table.text[table.rows[0][0].output_range.clone()], "华 东");
}

#[test]
fn inline_whole_row_over_32_graphemes_is_allowed_only_with_bounded_individual_cells() {
    let label = "甲".repeat(32);
    let source = inline_grid(&[
        &[&label, "0", "东区"],
        &[&label, "1", "西区"],
        &[&label, "2", "北区"],
    ]);
    let table = inline_accept(&source);
    assert_eq!(&table.text[table.rows[0][0].output_range.clone()], label);
    let too_long = "甲".repeat(33);
    inline_reject(&inline_grid(&[
        &[&too_long, "0", "东区"],
        &[&too_long, "1", "西区"],
        &[&too_long, "2", "北区"],
    ]));
}

#[test]
fn inline_fallback_preserves_existing_atomic_two_column_and_all_numeric_contracts() {
    let two_column = sample();
    let table = OcrTableCopy::new(&two_column).unwrap();
    assert_partition(&two_column, &table);
    let atomic_numeric = grid(&[&["1", "2", "3"], &["4", "5", "6"], &["7", "8", "9"]]);
    let table = OcrTableCopy::new(&atomic_numeric).unwrap();
    assert_partition(&atomic_numeric, &table);
    let inline_numeric = inline_grid(&[&["1", "2", "3"], &["4", "5", "6"], &["7", "8", "9"]]);
    let inline_table = inline_accept(&inline_numeric);
    assert_eq!(inline_table.text, table.text);
    assert_eq!(
        inline_table.horizontal_direction,
        table.horizontal_direction
    );
    for (inline_cell, atomic_cell) in inline_table
        .rows
        .iter()
        .flatten()
        .zip(table.rows.iter().flatten())
    {
        assert_eq!(inline_cell.rect, atomic_cell.rect);
        assert_eq!(inline_cell.geometry, atomic_cell.geometry);
        assert_eq!(inline_cell.words.len(), atomic_cell.words.len());
        for (inline_word, atomic_word) in inline_cell.words.iter().zip(&atomic_cell.words) {
            assert_eq!(inline_word.word_index, atomic_word.word_index);
            assert_eq!(inline_word.rect, atomic_word.rect);
            assert_eq!(inline_word.geometry, atomic_word.geometry);
        }
    }
}

#[test]
fn inline_ordinary_paragraph_with_large_word_gaps_has_no_complete_numeric_column() {
    inline_reject(&inline_grid(&[
        &["这个段落", "含有空隙", "仍是正文"],
        &["第二句话", "继续说明", "相同布局"],
        &["最后一句", "收束说明", "普通文本"],
    ]));
    inline_reject(&inline_grid(&[
        &["项目甲", "1", "介绍文字"],
        &["项目乙", "第二点", "更多文字"],
        &["项目丙", "3", "末行文字"],
    ]));
}

#[test]
fn inline_ambiguous_two_column_anchor_or_two_data_rows_are_insufficient() {
    let mut unstable = inline_grid(&[&["甲", "1"], &["乙", "2"], &["丙", "3"]]);
    let rect = unstable.words[3].rect;
    inline_set_word_rect(
        &mut unstable,
        3,
        Rect {
            x: rect.x + 12,
            ..rect
        },
    );
    inline_reject(&unstable);
    inline_reject(&inline_grid(&[&["甲", "1", "东"], &["乙", "2", "西"]]));
}

#[test]
fn inline_missing_cell_title_or_mixed_atomic_source_is_not_silently_dropped() {
    inline_reject(&inline_grid(&[
        &["甲", "1", "东"],
        &["乙", "2"],
        &["丙", "3", "北"],
    ]));
    inline_reject(&inline_grid(&[
        &["全宽报表标题"],
        &["甲", "1", "东"],
        &["乙", "2", "西"],
        &["丙", "3", "北"],
    ]));
    // 只保留第一物理行的完整 line，其余格仍各自为原子 line。
    let mut mixed = grid(&[&["甲", "1", "东"], &["乙", "2", "西"], &["丙", "3", "北"]]);
    mixed.lines[0].text = "甲    1    东".to_owned();
    mixed.lines.drain(1..3);
    for (index, word) in mixed.words.iter_mut().enumerate() {
        word.line = if index < 3 { 0 } else { index - 2 };
    }
    inline_refresh_line(&mut mixed, 0);
    sync_text(&mut mixed);
    inline_reject(&mixed);
}

#[test]
fn inline_each_row_big_gap_is_insufficient_when_shared_global_gutter_is_narrow() {
    let mut source = inline_sample();
    for row in 0..3 {
        let first = source.words[row * 3].rect;
        let second = source.words[row * 3 + 1].rect;
        let x = 140 + row as u32 * 4;
        inline_set_word_rect(
            &mut source,
            row * 3,
            Rect {
                width: 90 + row as u32 * 4,
                ..first
            },
        );
        inline_set_word_rect(
            &mut source,
            row * 3 + 1,
            Rect {
                x,
                width: 200 - x,
                ..second
            },
        );
    }
    // 各行第一空隙均40px；全行共有空隙108..140仅32px，小于2×原词高。
    inline_reject(&source);
}

#[test]
fn inline_intermediate_gap_and_extra_row_specific_gap_are_ambiguous() {
    let mut intermediate = inline_sample();
    for row in 0..3 {
        let rect = intermediate.words[row * 3 + 1].rect;
        inline_set_word_rect(&mut intermediate, row * 3 + 1, Rect { x: 100, ..rect });
    }
    inline_reject(&intermediate);
    let mut extra = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    inline_split_word(&mut extra, 0, "华", "东");
    let rect = extra.words[1].rect;
    inline_set_word_rect(&mut extra, 1, Rect { x: 80, ..rect });
    inline_reject(&extra);
}

#[test]
fn inline_projected_and_original_rect_gap_must_agree_even_with_tight_ink() {
    for (first_width, second_x) in [(10, 50), (5, 55)] {
        let mut source = inline_grid(&[
            &["华 东", "0", "东区"],
            &["西区", "1", "西区"],
            &["北区", "2", "北区"],
        ]);
        inline_split_word(&mut source, 0, "华", "东");
        let first = source.words[0].rect;
        let second = source.words[1].rect;
        inline_set_word_rect(&mut source, 0, Rect { width: 30, ..first });
        inline_set_word_rect(
            &mut source,
            1,
            Rect {
                x: 40,
                width: 30,
                ..second
            },
        );
        source.words[0].geometry = Some(geometry(
            Rect {
                width: first_width,
                ..source.words[0].rect
            },
            HorizontalDirection::LeftToRight,
        ));
        source.words[1].geometry = Some(geometry(
            Rect {
                x: second_x,
                width: 70 - second_x,
                ..source.words[1].rect
            },
            HorizontalDirection::LeftToRight,
        ));
        // 原rect gap为0，投影 gap为30或40，禁止按较紧Ink投影猜测新增列。
        inline_reject(&source);
    }
}

#[test]
fn inline_big_geometric_gap_requires_nonempty_ascii_source_spaces() {
    for separator in ["", "\u{a0}", "\u{3000}", " ! "] {
        let mut source = inline_sample();
        for line in &mut source.lines {
            line.text = line.text.replace("    ", separator);
        }
        sync_text(&mut source);
        inline_reject(&source);
    }
}

#[test]
fn inline_spaces_inside_one_word_do_not_supply_missing_geometric_column_boundaries() {
    let source = grid(&[&["甲    0    东"], &["乙    1    西"], &["丙    2    北"]]);
    inline_reject(&source);
    let mut source = inline_sample();
    source.lines[0].text.push_str(" 未归属尾字");
    sync_text(&mut source);
    inline_reject(&source);
}

#[test]
fn inline_words_on_different_baselines_are_not_hidden_by_line_rect_union() {
    let mut source = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    inline_split_word(&mut source, 0, "华", "东");
    let first = source.words[0].rect;
    let second = source.words[1].rect;
    inline_set_word_rect(
        &mut source,
        0,
        Rect {
            height: 10,
            ..first
        },
    );
    inline_set_word_rect(
        &mut source,
        1,
        Rect {
            y: 20,
            height: 10,
            ..second
        },
    );
    assert_eq!(source.lines[0].rect.unwrap().height, 20);
    inline_reject(&source);
}

#[test]
fn inline_word_start_and_end_order_must_both_be_monotonic() {
    let mut starts = inline_sample();
    let first = starts.words[0].rect;
    let second = starts.words[1].rect;
    inline_set_word_rect(&mut starts, 0, Rect { x: 160, ..first });
    inline_set_word_rect(&mut starts, 1, Rect { x: 10, ..second });
    inline_reject(&starts);
    let mut ends = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    inline_split_word(&mut ends, 0, "华", "东");
    let first = ends.words[0].rect;
    let second = ends.words[1].rect;
    inline_set_word_rect(&mut ends, 0, Rect { width: 60, ..first });
    inline_set_word_rect(
        &mut ends,
        1,
        Rect {
            x: 20,
            width: 10,
            ..second
        },
    );
    inline_reject(&ends);
}

#[test]
fn inline_mixed_reading_direction_and_vertical_projection_are_rejected() {
    let mut mixed = inline_sample();
    let rect = mixed.words[0].rect;
    mixed.words[0].horizontal_direction = HorizontalDirection::RightToLeft;
    mixed.words[0].geometry = Some(geometry(rect, HorizontalDirection::RightToLeft));
    inline_reject(&mixed);
    let mut vertical = inline_sample();
    let q = vertical.lines[0].geometry.unwrap().quad;
    vertical.lines[0].geometry = Some(TextGeometry {
        quad: [q[1], q[2], q[3], q[0]],
    });
    inline_reject(&vertical);
}

#[test]
fn inline_perspective_and_slanted_parent_crop_are_not_replaced_by_aabb() {
    for slanted in [false, true] {
        let mut source = inline_sample();
        let quad = &mut source.lines[0].geometry.as_mut().unwrap().quad;
        if slanted {
            quad[1].y += 2.;
            quad[2].y += 2.;
        } else {
            quad[1].x -= 3.;
        }
        inline_reject(&source);
    }
}

#[test]
fn inline_fallback_still_requires_exact_source_regions_line_union_and_word_ownership() {
    let mut text = inline_sample();
    text.text.push('!');
    inline_reject(&text);
    let mut region = inline_sample();
    region.regions[0].width -= 1;
    inline_reject(&region);
    let mut union = inline_sample();
    union.lines[0].rect.as_mut().unwrap().width += 1;
    inline_reject(&union);
    let mut index = inline_sample();
    index.words[0].line = index.lines.len();
    inline_reject(&index);
    let mut word = inline_sample();
    word.words[0].text = "未绑定正文".to_owned();
    inline_reject(&word);
    let mut missing = inline_sample();
    missing.lines[0].geometry = None;
    inline_reject(&missing);
}

#[test]
fn inline_word_boundary_cannot_cut_combining_or_zwj_grapheme() {
    for (cell, first, second) in [("甲e\u{301}", "甲e", "\u{301}"), ("👩‍💻", "👩", "‍💻")]
    {
        let mut source = inline_grid(&[&[cell, "0", "东"], &["乙", "1", "西"], &["丙", "2", "北"]]);
        inline_split_word(&mut source, 0, first, second);
        inline_reject(&source);
    }
}

#[test]
fn inline_control_or_bidi_text_is_rejected_before_partial_table_output() {
    for text in [
        "甲\t乙",
        "甲\n乙",
        "甲\r乙",
        "甲\0乙",
        "甲\u{2028}乙",
        "甲\u{202e}乙",
    ] {
        let source = inline_grid(&[&[text, "0", "东"], &["乙", "1", "西"], &["丙", "2", "北"]]);
        inline_reject(&source);
    }
}

#[test]
fn inline_row_and_column_budgets_reject_whole_candidate_without_truncation() {
    let rows: Vec<&[&str]> = vec![&["甲", "1", "东"]; 257];
    assert_eq!(inline_accept(&inline_grid(&rows[..256])).rows.len(), 256);
    inline_reject(&inline_grid(&rows));
    let mut cells = vec!["1"; 17];
    cells[0] = "项目";
    assert_eq!(
        inline_accept(&inline_grid(&[&cells[..16], &cells[..16], &cells[..16]])).rows[0].len(),
        16
    );
    inline_reject(&inline_grid(&[&cells, &cells, &cells]));
}

#[test]
fn inline_source_byte_budget_counts_large_column_separator_spaces() {
    let mut source = inline_sample();
    source.lines[0].text = source.lines[0]
        .text
        .replacen("    ", &" ".repeat(1_048_576), 1);
    sync_text(&mut source);
    assert!(source.text.len() > 1_048_576);
    assert_eq!(source.words.len(), 9);
    inline_reject(&source);
}

#[test]
fn inline_excessive_word_budget_and_invalid_coordinate_do_not_return_partial_rows() {
    let mut words = inline_sample();
    let rect = words.words[0].rect;
    for _ in words.words.len()..32_769 {
        words.words.push(RecognizedWord {
            text: "甲项".to_owned(),
            rect,
            line: 2,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(geometry(rect, HorizontalDirection::LeftToRight)),
        });
        words.regions.push(rect);
    }
    inline_reject(&words);
    let mut coordinate = inline_sample();
    coordinate.words[0].rect.x = u32::MAX;
    coordinate.regions[0].x = u32::MAX;
    coordinate.lines[0].rect.as_mut().unwrap().x = u32::MAX;
    inline_reject(&coordinate);
}

#[test]
fn inline_cell_projection_contains_every_word_despite_permitted_micro_retreat() {
    let mut source = inline_grid(&[
        &["华 东", "0", "东区"],
        &["西区", "1", "西区"],
        &["北区", "2", "北区"],
    ]);
    for row in 0..3 {
        for (column, x, width) in [(0, 10, 1), (1, 25_010, 60), (2, 49_950, 60)] {
            let rect = source.words[row * 3 + column].rect;
            inline_set_word_rect(&mut source, row * 3 + column, Rect { x, width, ..rect });
        }
    }
    inline_split_word(&mut source, 0, "华", "东");
    for word_index in [0, 1] {
        let rect = source.words[word_index].rect;
        inline_set_word_rect(
            &mut source,
            word_index,
            Rect {
                x: 10,
                width: 1,
                ..rect
            },
        );
    }
    // 父投影宽50000px，既有u容差允许起止各微回退约0.0005px。
    // 两个原1px词框重叠恰1px，墨迹四角仍各在原框内，中心基线一致。
    for (word_index, left, right) in [(0, 10.2, 10.7), (1, 10.1995, 10.6995)] {
        source.words[word_index].geometry = Some(TextGeometry {
            quad: [
                Point { x: left, y: 10. },
                Point { x: right, y: 10. },
                Point { x: right, y: 30. },
                Point { x: left, y: 30. },
            ],
        });
    }
    let table = inline_accept(&source);
    let cell = &table.rows[0][0];
    assert_eq!(cell.words.len(), 2);
    let first = source.words[0].geometry.unwrap().quad;
    let last = source.words[1].geometry.unwrap().quad;
    assert!(cell.geometry.quad[0].x <= last[0].x + 0.000001);
    assert!(cell.geometry.quad[1].x >= first[1].x - 0.000001);
    assert_eq!(cell.rect.width, 1);
}

// v2：不以三列或标签列门槛规避可信的两列与全数值矩阵。
#[test]
fn inline_v2_common_two_column_text_numeric_rows_have_complete_byte_mapping() {
    let source = inline_grid(&[&["甲项", "0"], &["乙项", "0012"], &["丙项", "-3.50%"]]);
    let table = inline_accept(&source);
    assert_eq!(table.text, "甲项\t0\r\n乙项\t0012\r\n丙项\t-3.50%");
    assert!(table.rows.iter().all(|row| row.len() == 2));
    assert_eq!(table.rows[0][0].source_range, 0..6);
    assert_eq!(table.separators[0].source_range, 6..10);
    assert_eq!(table.rows[0][1].source_range, 10..11);
    assert_eq!(table.separators[1].source_range, 11..12);
    assert_eq!(table.rows[0][0].words[0].source_range, 0..6);
    assert_eq!(table.rows[0][1].words[0].source_range, 10..11);
    assert_eq!(table.rows[0][0].source_line, 0);
    assert_eq!(table.rows[0][1].source_line, 0);
}

#[test]
fn inline_v2_complete_all_numeric_two_column_matrix_keeps_original_spelling() {
    let source = inline_grid(&[&["0", "0012"], &["-3.50%", "+2.0"], &["100", "0.5%"]]);
    let table = inline_accept(&source);
    assert_eq!(table.text, "0\t0012\r\n-3.50%\t+2.0\r\n100\t0.5%");
    assert!(table.rows.iter().all(|row| row.len() == 2));
}

#[test]
fn inline_v2_two_column_header_accepts_three_all_numeric_rows_but_not_two() {
    let rows: &[&[&str]] = &[
        &["编号", "数值"],
        &["0", "0012"],
        &["1", "-3.50%"],
        &["2", "+2.0"],
    ];
    let table = inline_accept(&inline_grid(rows));
    assert_eq!(table.text, "编号\t数值\r\n0\t0012\r\n1\t-3.50%\r\n2\t+2.0");
    inline_reject(&inline_grid(&rows[..3]));
    inline_reject(&inline_grid(&rows[1..3]));
}

#[test]
fn inline_v2_rotated_two_column_numeric_matrix_preserves_reading_and_row_sequence() {
    for descending_rows in [false, true] {
        let source = inline_layout(
            &[&["0", "0012"], &["1", "-3.50%"], &["2", "+2.0"]],
            HorizontalDirection::RightToLeft,
            descending_rows,
        );
        let table = inline_accept(&source);
        assert_eq!(table.text, "0\t0012\r\n1\t-3.50%\r\n2\t+2.0");
        assert_eq!(table.horizontal_direction, HorizontalDirection::RightToLeft);
        assert!(table.rows.iter().all(|row| row[0].rect.x > row[1].rect.x));
        for cell in table.rows.iter().flatten() {
            assert_eq!(
                cell.geometry,
                source.words[cell.words[0].word_index].geometry.unwrap()
            );
        }
    }
}

#[test]
fn inline_v2_two_column_multiword_cells_preserve_outer_spaces_and_entire_separator() {
    let mut source = inline_grid(&[
        &["  华 东", "0  "],
        &["  华 东", "0012  "],
        &["  华 东", "3  "],
    ]);
    for word in [4, 2, 0] {
        inline_split_word(&mut source, word, "华", "东");
    }
    for line in &mut source.lines {
        line.text = line.text.replacen("    ", "         ", 1);
    }
    sync_text(&mut source);
    let table = inline_accept(&source);
    assert_eq!(
        table.text,
        "  华 东\t0  \r\n  华 东\t0012  \r\n  华 东\t3  "
    );
    assert_eq!(table.rows[0][0].words.len(), 2);
    assert_eq!(table.rows[0][0].source_range, 0..9);
    assert_eq!(table.separators[0].source_range, 9..18);
    assert_eq!(
        &source.text[table.separators[0].source_range.clone()],
        "         "
    );
    assert_eq!(table.rows[0][1].source_range, 18..21);
    assert_eq!(table.rows[0][0].words[0].source_range, 2..5);
    assert_eq!(table.rows[0][0].words[1].source_range, 6..9);
}

#[test]
fn inline_v2_two_column_quoted_and_formula_shaped_labels_are_not_rewritten() {
    let source = inline_grid(&[&["A\"B", "0"], &["=SUM(A1:A2)", "0012"], &["@name", "3"]]);
    let table = inline_accept(&source);
    assert_eq!(table.text, "\"A\"\"B\"\t0\r\n=SUM(A1:A2)\t0012\r\n@name\t3");
    assert_eq!(table.rows[0][0].source_range, 0..3);
    assert_eq!(table.rows[0][0].output_range, 0..6);
}

#[test]
fn inline_v2_two_column_numeric_widths_may_have_shared_right_or_center_anchor() {
    for centered in [false, true] {
        let mut source = inline_grid(&[&["甲", "1"], &["乙", "0012"], &["丙", "-3.50%"]]);
        for (row, width) in [(0, 20), (1, 40), (2, 60)] {
            let word_index = row * 2 + 1;
            let rect = source.words[word_index].rect;
            let x = if centered {
                190 - width / 2
            } else {
                220 - width
            };
            inline_set_word_rect(&mut source, word_index, Rect { x, width, ..rect });
        }
        let table = inline_accept(&source);
        assert_eq!(table.text, "甲\t1\r\n乙\t0012\r\n丙\t-3.50%");
        for row in &table.rows {
            let rect = row[1].rect;
            if centered {
                assert_eq!(rect.x + rect.width / 2, 190);
            } else {
                assert_eq!(rect.x + rect.width, 220);
            }
        }
    }
}

#[test]
fn inline_v2_all_numeric_two_columns_still_need_a_large_shared_gutter() {
    let mut source = inline_grid(&[&["0", "1"], &["2", "3"], &["4", "5"]]);
    for row in 0..3 {
        let first = source.words[row * 2].rect;
        let second = source.words[row * 2 + 1].rect;
        let x = 140 + row as u32 * 4;
        inline_set_word_rect(
            &mut source,
            row * 2,
            Rect {
                width: 90 + row as u32 * 4,
                ..first
            },
        );
        inline_set_word_rect(
            &mut source,
            row * 2 + 1,
            Rect {
                x,
                width: 200 - x,
                ..second
            },
        );
    }
    // 各行均有40px空隙、列anchor各自稳定，跨行共有32px仍不足。
    inline_reject(&source);
}

#[test]
fn inline_v2_two_column_numeric_matrix_rejects_medium_or_disagreeing_gaps() {
    let mut medium = inline_grid(&[&["0", "1"], &["2", "3"], &["4", "5"]]);
    for row in 0..3 {
        let rect = medium.words[row * 2 + 1].rect;
        inline_set_word_rect(&mut medium, row * 2 + 1, Rect { x: 100, ..rect });
    }
    inline_reject(&medium);
    let mut disagreement = inline_grid(&[&["0", "1"], &["2", "3"], &["4", "5"]]);
    for row in 0..3 {
        let word_index = row * 2;
        let rect = disagreement.words[word_index].rect;
        inline_set_word_rect(&mut disagreement, word_index, Rect { width: 130, ..rect });
        disagreement.words[word_index].geometry = Some(geometry(
            Rect { width: 100, ..rect },
            HorizontalDirection::LeftToRight,
        ));
    }
    // 原rect gap20px归格内，较窄Ink投影gap50px似列界，不能猜测。
    inline_reject(&disagreement);
}

#[test]
fn inline_v2_two_column_acceptance_cannot_bypass_source_or_grapheme_binding() {
    let mut unowned = inline_grid(&[&["甲", "0"], &["乙", "1"], &["丙", "2"]]);
    unowned.lines[0].text.push_str(" 未归属尾字");
    sync_text(&mut unowned);
    inline_reject(&unowned);
    let mut word = inline_grid(&[&["甲", "0"], &["乙", "1"], &["丙", "2"]]);
    word.words[1].text = "00".to_owned();
    inline_reject(&word);
    let mut grapheme = inline_grid(&[&["甲e\u{301}", "0"], &["乙", "1"], &["丙", "2"]]);
    inline_split_word(&mut grapheme, 0, "甲e", "\u{301}");
    inline_reject(&grapheme);
    let mut blank_gap = inline_grid(&[&["0", "1"], &["2", "3"], &["4", "5"]]);
    for line in &mut blank_gap.lines {
        line.text = line.text.replace("    ", "");
    }
    sync_text(&mut blank_gap);
    inline_reject(&blank_gap);
}

#[test]
fn inline_v2_numeric_values_scattered_between_two_columns_do_not_form_complete_evidence() {
    inline_reject(&inline_grid(&[
        &["0", "文字"],
        &["正文", "1"],
        &["2", "末句"],
    ]));
    inline_reject(&inline_grid(&[
        &["项目", "数值"],
        &["0", "文字"],
        &["正文", "1"],
        &["2", "末句"],
    ]));
}
