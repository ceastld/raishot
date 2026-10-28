use super::super::{RecognizedLine, RecognizedWord};
use super::*;

fn row_rect(index: usize) -> Rect {
    Rect {
        x: 20,
        y: 20 + index as u32 * 26,
        width: 200,
        height: 20,
    }
}

fn fixture(rows: &[&str], separator: &str) -> RecognizedText {
    RecognizedText {
        text: rows.join(separator),
        regions: (0..rows.len()).map(row_rect).collect(),
        words: Vec::new(),
        lines: rows
            .iter()
            .enumerate()
            .map(|(index, text)| RecognizedLine {
                graphemes: None,
                text: (*text).into(),
                rect: Some(row_rect(index)),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            })
            .collect(),
    }
}

fn full_copy(result: &RecognizedText, mode: CopyLayoutMode) -> String {
    OcrCopyLayout::new(result)
        .copy(0..result.text.len(), mode)
        .expect("合法全文范围应可复制")
}

fn assert_preserved(result: &RecognizedText) {
    let copied = OcrCopyLayout::new(result)
        .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
        .expect("合法全文范围应可复制");
    assert_eq!(copied.text, result.text);
    assert_mapping(&result.text, &copied);
}

fn assert_mapping(source: &str, copied: &CopiedText) {
    let mut source_boundaries: Vec<_> = source.grapheme_indices(true).map(|(i, _)| i).collect();
    source_boundaries.push(source.len());
    let mut output_boundaries: Vec<_> =
        copied.text.grapheme_indices(true).map(|(i, _)| i).collect();
    output_boundaries.push(copied.text.len());
    let mut source_cursor = copied.source_range.start;
    let mut output_cursor = 0;
    for span in &copied.spans {
        assert_eq!(
            span.source_range.start, source_cursor,
            "原文映射不能跳字或重叠"
        );
        assert_eq!(span.output_range.start, output_cursor, "输出映射须连续");
        assert!(span.source_range.start < span.source_range.end);
        assert!(span.source_range.end <= copied.source_range.end);
        assert!(span.output_range.start <= span.output_range.end);
        assert!(span.output_range.end <= copied.text.len());
        assert!(source_boundaries.contains(&span.source_range.start));
        assert!(source_boundaries.contains(&span.source_range.end));
        assert!(output_boundaries.contains(&span.output_range.start));
        assert!(output_boundaries.contains(&span.output_range.end));
        let original = source
            .get(span.source_range.clone())
            .expect("原文映射须位于 UTF-8 边界");
        let output = copied
            .text
            .get(span.output_range.clone())
            .expect("输出映射须位于 UTF-8 边界");
        match span.kind {
            CopySpanKind::Verbatim => assert_eq!(output.as_bytes(), original.as_bytes()),
            CopySpanKind::LineJoin => {
                assert!(matches!(original, "\n" | "\r\n"));
                assert!(matches!(output, "" | " "));
            }
        }
        source_cursor = span.source_range.end;
        output_cursor = span.output_range.end;
    }
    assert_eq!(source_cursor, copied.source_range.end, "原文范围须穷尽覆盖");
    assert_eq!(output_cursor, copied.text.len(), "输出范围须穷尽覆盖");
}

#[test]
fn original_preserves_utf8_crlf_tabs_spaces_and_emoji_bytes() {
    let result = fixture(&["  中文\tA  e\u{301}👩‍💻", "🙂\tsecond  ", ""], "\r\n");
    let layout = OcrCopyLayout::new(&result);
    assert_eq!(layout.text().as_bytes(), result.text.as_bytes());
    let copied = layout
        .copy_mapped(0..result.text.len(), CopyLayoutMode::Original)
        .expect("合法原文复制应成功");
    assert_eq!(copied.text.as_bytes(), result.text.as_bytes());
    assert_eq!(copied.spans.len(), 1);
    assert_eq!(copied.spans[0].kind, CopySpanKind::Verbatim);
    assert_mapping(&result.text, &copied);
    let start = result.text.find('A').expect("夹具含 A");
    let end = result.text.find("second").expect("夹具含 second") + "second".len();
    assert_eq!(
        layout.copy(start..end, CopyLayoutMode::Original).unwrap(),
        result.text[start..end]
    );
}

#[test]
fn invalid_ranges_reject_utf8_grapheme_zwj_and_crlf_interior() {
    let result = fixture(&["中e\u{301}👩‍💻", "tail"], "\r\n");
    let layout = OcrCopyLayout::new(&result);
    let combining = result.text.find('e').unwrap() + 1;
    let zwj = result.text.find('👩').unwrap() + '👩'.len_utf8();
    let crlf = result.text.find("\r\n").unwrap() + 1;
    for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
        for range in [
            1..result.text.len(),
            0..2,
            combining..result.text.len(),
            0..combining,
            zwj..result.text.len(),
            0..zwj,
            crlf..result.text.len(),
            0..crlf,
            result.text.len() + 1..result.text.len() + 1,
            0..usize::MAX,
        ] {
            assert!(
                layout.copy(range, mode).is_err(),
                "非法范围不能自动移动端点"
            );
        }
        let reversed = Range { start: 4, end: 3 };
        assert!(layout.copy(reversed, mode).is_err());
    }
}

#[test]
fn empty_source_and_empty_legal_ranges_return_empty_mapping() {
    let empty = fixture(&[], "\n");
    let layout = OcrCopyLayout::new(&empty);
    for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
        let copied = layout.copy_mapped(0..0, mode).unwrap();
        assert!(copied.text.is_empty());
        assert!(copied.spans.is_empty());
        assert_mapping(&empty.text, &copied);
    }
    let result = fixture(&["中文", "续行"], "\n");
    let layout = OcrCopyLayout::new(&result);
    for index in [0, "中".len(), result.text.len()] {
        let copied = layout
            .copy_mapped(index..index, CopyLayoutMode::Auto)
            .unwrap();
        assert!(copied.text.is_empty());
        assert!(copied.spans.is_empty());
        assert_mapping(&result.text, &copied);
    }
}

#[test]
fn auto_joins_cjk_continuations_without_inventing_spaces() {
    for separator in ["\n", "\r\n"] {
        let result = fixture(&["这是正文第一行", "以及正文第二行", "段落末行"], separator);
        assert_eq!(
            full_copy(&result, CopyLayoutMode::Auto),
            "这是正文第一行以及正文第二行段落末行"
        );
        assert_eq!(full_copy(&result, CopyLayoutMode::Original), result.text);
        let mapped = OcrCopyLayout::new(&result)
            .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
            .unwrap();
        assert_mapping(&result.text, &mapped);
        let joins: Vec<_> = mapped
            .spans
            .iter()
            .filter(|span| span.kind == CopySpanKind::LineJoin)
            .collect();
        assert_eq!(joins.len(), 2);
        assert!(joins.iter().all(|span| span.output_range.is_empty()));
    }
}

#[test]
fn auto_cjk_extensions_variation_selectors_kana_and_hangul_stay_whole() {
    for rows in [
        ["\u{20000}", "\u{30000}"],
        ["\u{2ebf0}", "\u{323af}"],
        ["中\u{fe0f}", "文\u{e0100}"],
        ["あいう", "カキク"],
        ["한글", "문장"],
    ] {
        let result = fixture(&rows, "\n");
        assert_eq!(full_copy(&result, CopyLayoutMode::Auto), rows.concat());
    }
}

#[test]
fn auto_latin_and_mixed_script_boundaries_have_explicit_spaces() {
    for (rows, expected) in [
        (
            ["English first line", "continues here"],
            "English first line continues here",
        ),
        (["中文English", "next中文"], "中文English next中文"),
        (["English中文", "继续English"], "English中文继续English"),
        (["中文", "English"], "中文 English"),
        (["English", "中文"], "English 中文"),
        (["café", "déjà vu"], "café déjà vu"),
    ] {
        let result = fixture(&rows, "\n");
        assert_eq!(full_copy(&result, CopyLayoutMode::Auto), expected);
    }
}

#[test]
fn auto_preserves_hyphens_and_punctuation_instead_of_rewriting_characters() {
    for (rows, expected) in [
        // 拉丁连字符断词保留连字符、不加空格；大写开头不当作断词。
        (["inter-", "national."], "inter-national."),
        (["self-", "Aware"], "self- Aware"),
        (
            ["Already finished!", "Another sentence?"],
            "Already finished! Another sentence?",
        ),
        (["中文，", "继续。"], "中文，继续。"),
    ] {
        let result = fixture(&rows, "\n");
        assert_eq!(full_copy(&result, CopyLayoutMode::Auto), expected);
        let mapped = OcrCopyLayout::new(&result)
            .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
            .unwrap();
        assert_mapping(&result.text, &mapped);
    }
}

#[test]
fn auto_preserves_bare_carriage_return_as_uncertain_separator() {
    let result = fixture(&["alpha", "beta"], "\r");
    assert_preserved(&result);
}

#[test]
fn auto_preserves_large_paragraph_gap_and_geometric_indent() {
    let mut result = fixture(&["first paragraph", "second paragraph"], "\n");
    result.lines[1].rect.as_mut().unwrap().y = 80;
    assert_preserved(&result);
    result.lines[1].rect = Some(Rect {
        x: 32,
        ..row_rect(1)
    });
    assert_preserved(&result);
}

#[test]
fn auto_preserves_heading_width_height_and_overlapping_row_uncertainty() {
    let mut result = fixture(&["heading", "body text"], "\n");
    result.lines[0].rect.as_mut().unwrap().width = 100;
    assert_preserved(&result);
    result.lines[0].rect = Some(Rect {
        height: 30,
        ..row_rect(0)
    });
    result.lines[1].rect.as_mut().unwrap().y = 56;
    assert_preserved(&result);
    result.lines[0].rect = Some(row_rect(0));
    result.lines[1].rect.as_mut().unwrap().y = 30;
    assert_preserved(&result);
}

#[test]
fn auto_preserves_indented_and_whitespace_rows_byte_exactly() {
    for rows in [
        ["regular text", " indented text"],
        ["regular text", "\tindented text"],
        ["trailing text ", "regular text"],
        ["first  column", "second column"],
        ["first\u{a0}\u{a0}column", "second column"],
        ["   ", "body text"],
        ["", "body text"],
    ] {
        assert_preserved(&fixture(&rows, "\n"));
    }
    let result = fixture(&["first paragraph", "", "second paragraph"], "\n");
    assert_preserved(&result);
}

#[test]
fn auto_preserves_lists_numbers_and_code_rows() {
    for structured in [
        "- list item",
        "+ list item",
        "* list item",
        "• list item",
        "1. numbered item",
        "(2) numbered item",
        "（三）编号条目",
        "【四】编号条目",
        "一、编号条目",
        "a) alphabetic item",
        "a. alphabetic item",
        "(a) alphabetic item",
        "IV. roman item",
        "i) roman item",
        "value = answer",
        "call();",
        "{ code }",
        "`code`",
        "<div>text</div>",
        "# heading",
        "> quoted text",
    ] {
        assert_preserved(&fixture(&["regular text", structured], "\n"));
        assert_preserved(&fixture(&[structured, "regular text"], "\n"));
    }
}

#[test]
fn auto_joins_prose_that_only_looks_like_code_words() {
    // 只凭英文词或括号无法区分正文与代码；真实代码行很少写满栏宽，交给几何判断。
    for row in [
        "if value",
        "for item",
        "while ready",
        "let value",
        "return value",
        "class Example",
        "use the (optional) flag",
        "works; that too",
    ] {
        let result = fixture(&["regular text", row], "\n");
        assert_eq!(
            full_copy(&result, CopyLayoutMode::Auto),
            format!("regular text {row}")
        );
    }
}

#[test]
fn auto_joins_rows_starting_with_times_versions_or_decimals() {
    // 行首的 09:21、3.14、2.0 不是编号；只有标点后不接数字时才算列表。
    for row in [
        "09:21:42 runs from the tray",
        "10:30 meeting moved",
        "3.14 is close to pi",
        "2.0 release notes",
        "12：30 开会",
    ] {
        let result = fixture(&["regular text", row], "\n");
        assert_eq!(
            full_copy(&result, CopyLayoutMode::Auto),
            format!("regular text {row}")
        );
    }
    for marker in [
        "1.打开设置",
        "2: second item",
        "3：第三项",
        "4. fourth item",
    ] {
        assert_preserved(&fixture(&["regular text", marker], "\n"));
    }
}

#[test]
fn auto_preserves_shell_prompts_and_command_rows() {
    for rows in [
        ["$ cargo build", "$ cargo test"],
        ["PS C:\\repo> cargo build", "PS C:\\repo> cargo test"],
        ["C:\\repo> cargo build", "C:\\repo> cargo test"],
        ["user@host:~$ cargo build", "user@host:~$ cargo test"],
        ["λ cargo build", "λ cargo test"],
        ["❯ cargo build", "❯ cargo test"],
        ["cargo build", "cargo test"],
        ["git status", "git diff"],
        ["python first.py", "python second.py"],
    ] {
        assert_preserved(&fixture(&rows, "\n"));
    }
}

#[test]
fn auto_preserves_checkbox_checklist_rows() {
    for rows in [
        ["☑ build release", "☐ upload artifacts"],
        ["✓ build release", "□ upload artifacts"],
        ["✅ build release", "❌ upload artifacts"],
    ] {
        assert_preserved(&fixture(&rows, "\n"));
    }
}

#[test]
fn auto_preserves_repeated_aligned_numeric_columns_but_keeps_year_prose() {
    let mut result = fixture(&["2025 100", "2026 110"], "\n");
    for (index, texts) in [["2025", "100"], ["2026", "110"]].iter().enumerate() {
        result.lines[index].rect.as_mut().unwrap().width = 100;
        for (column, text) in texts.iter().enumerate() {
            result.words.push(RecognizedWord {
                text: (*text).into(),
                rect: Rect {
                    x: 20 + column as u32 * 60,
                    width: 40,
                    ..row_rect(index)
                },
                line: index,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            });
        }
    }
    assert_preserved(&result);
    let mut prose = fixture(&["In 2025 production", "reached 100 units"], "\n");
    for (index, texts) in [["In", "2025", "production"], ["reached", "100", "units"]]
        .iter()
        .enumerate()
    {
        for (column, text) in texts.iter().enumerate() {
            prose.words.push(RecognizedWord {
                text: (*text).into(),
                rect: Rect {
                    x: 20 + column as u32 * 60,
                    width: 40,
                    ..row_rect(index)
                },
                line: index,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            });
        }
    }
    assert_eq!(
        full_copy(&prose, CopyLayoutMode::Auto),
        "In 2025 production reached 100 units"
    );
}

#[test]
fn auto_preserves_tables_and_wide_word_gaps() {
    for row in ["name\tvalue", "name | value", "name   value", "name,value"] {
        assert_preserved(&fixture(&[row, "other row"], "\n"));
    }
    let mut result = fixture(&["name value", "other row"], "\n");
    result.words = vec![
        RecognizedWord {
            text: "name".into(),
            rect: Rect {
                width: 40,
                ..row_rect(0)
            },
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        },
        RecognizedWord {
            text: "value".into(),
            rect: Rect {
                x: 150,
                width: 70,
                ..row_rect(0)
            },
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        },
    ];
    assert_preserved(&result);
}

#[test]
fn auto_preserves_missing_invalid_and_overflowing_row_geometry() {
    for rect in [
        None,
        Some(Rect {
            width: 0,
            ..row_rect(0)
        }),
        Some(Rect {
            height: 0,
            ..row_rect(0)
        }),
        Some(Rect {
            x: u32::MAX - 5,
            ..row_rect(0)
        }),
        Some(Rect {
            y: u32::MAX - 5,
            ..row_rect(0)
        }),
    ] {
        let mut result = fixture(&["first text", "second text"], "\n");
        result.lines[0].rect = rect;
        assert_preserved(&result);
    }
}

#[test]
fn auto_preserves_invalid_or_outside_word_geometry_and_direction() {
    for (rect, direction) in [
        (
            Rect {
                width: 0,
                ..row_rect(0)
            },
            HorizontalDirection::LeftToRight,
        ),
        (
            Rect {
                x: 19,
                ..row_rect(0)
            },
            HorizontalDirection::LeftToRight,
        ),
        (
            Rect {
                y: 19,
                ..row_rect(0)
            },
            HorizontalDirection::LeftToRight,
        ),
        (
            Rect {
                width: 201,
                ..row_rect(0)
            },
            HorizontalDirection::LeftToRight,
        ),
        (
            Rect {
                height: 21,
                ..row_rect(0)
            },
            HorizontalDirection::LeftToRight,
        ),
        (row_rect(0), HorizontalDirection::RightToLeft),
    ] {
        let mut result = fixture(&["first text", "second text"], "\n");
        result.words.push(RecognizedWord {
            text: "first text".into(),
            rect,
            line: 0,
            horizontal_direction: direction,
            geometry: None,
        });
        assert_preserved(&result);
    }
    let mut result = fixture(&["first text", "second text"], "\n");
    result.words.push(RecognizedWord {
        text: "orphan".into(),
        rect: row_rect(0),
        line: 2,
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
    });
    assert_preserved(&result);
    result.words[0].line = 0;
    result.words[0].text.clear();
    assert_preserved(&result);
}

#[test]
fn auto_requires_exact_source_lines_and_line_count() {
    let mut result = fixture(&["first text", "second text"], "\n");
    result.lines[0].text = "different text".into();
    assert_preserved(&result);
    let mut result = fixture(&["first text", "second text"], "\n");
    result.lines.pop();
    assert_preserved(&result);
    let mut result = fixture(&["first text", "second text"], "\n");
    result.lines.push(RecognizedLine {
        graphemes: None,
        text: "extra text".into(),
        rect: Some(row_rect(2)),
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
    });
    assert_preserved(&result);
}

#[test]
fn auto_splits_columns_into_blocks_and_keeps_source_reading_order() {
    // 按栏输出的源文在各栏内续行，跨栏的换行保留。
    let mut result = fixture(
        &["left first", "left second", "right first", "right second"],
        "\n",
    );
    result.lines[2].rect = Some(Rect {
        x: 300,
        ..row_rect(0)
    });
    result.lines[3].rect = Some(Rect {
        x: 300,
        ..row_rect(1)
    });
    assert_eq!(
        full_copy(&result, CopyLayoutMode::Auto),
        "left first left second\nright first right second"
    );
    // 按行交错两栏的源文没有上下堆叠的相邻行，全部保留。
    let mut result = fixture(
        &["left first", "right first", "left second", "right second"],
        "\n",
    );
    for (index, (x, row)) in [(20, 0), (300, 0), (20, 1), (300, 1)]
        .into_iter()
        .enumerate()
    {
        result.lines[index].rect = Some(Rect { x, ..row_rect(row) });
    }
    assert_preserved(&result);
    let mut result = fixture(&["left block", "right block", "next row"], "\n");
    result.lines[1].rect = Some(Rect {
        x: 300,
        ..row_rect(0)
    });
    assert_preserved(&result);
    let mut result = fixture(&["lower source first", "upper source second"], "\n");
    result.lines[0].rect = Some(row_rect(1));
    result.lines[1].rect = Some(row_rect(0));
    assert_preserved(&result);
}

#[test]
fn auto_preserves_180_direction_and_mixed_direction_without_reordering() {
    let mut result = fixture(&["倒置原文第一行", "倒置原文第二行"], "\n");
    for line in &mut result.lines {
        line.horizontal_direction = HorizontalDirection::RightToLeft;
    }
    assert_preserved(&result);
    result.lines[0].rect = Some(row_rect(1));
    result.lines[1].rect = Some(row_rect(0));
    assert_preserved(&result);
    let mut result = fixture(&["normal first line", "倒置第二行"], "\n");
    result.lines[1].horizontal_direction = HorizontalDirection::RightToLeft;
    assert_preserved(&result);
}

#[test]
fn auto_partial_selection_joins_only_when_both_sides_are_selected() {
    let result = fixture(&["alpha beta", "gamma delta"], "\r\n");
    let layout = OcrCopyLayout::new(&result);
    let newline = result.text.find("\r\n").unwrap();
    let start = result.text.find("beta").unwrap();
    let end = result.text.find("gamma").unwrap() + "gamma".len();
    let copied = layout
        .copy_mapped(start..end, CopyLayoutMode::Auto)
        .unwrap();
    assert_eq!(copied.text, "beta gamma");
    assert_eq!(copied.source_range, start..end);
    assert_mapping(&result.text, &copied);
    for range in [newline..newline + 2, start..newline + 2, newline..end] {
        assert_eq!(
            layout.copy(range.clone(), CopyLayoutMode::Auto).unwrap(),
            result.text[range]
        );
    }
}

#[test]
fn auto_partial_cjk_selection_keeps_zero_length_output_join_mapping() {
    let result = fixture(&["首行中文", "继续正文"], "\n");
    let layout = OcrCopyLayout::new(&result);
    let start = "首行".len();
    let end = result.text.find("正文").unwrap();
    let copied = layout
        .copy_mapped(start..end, CopyLayoutMode::Auto)
        .unwrap();
    assert_eq!(copied.text, "中文继续");
    assert_mapping(&result.text, &copied);
    let join = copied
        .spans
        .iter()
        .find(|span| span.kind == CopySpanKind::LineJoin)
        .unwrap();
    assert_eq!(&result.text[join.source_range.clone()], "\n");
    assert!(join.output_range.is_empty());
}

#[test]
fn auto_preserves_newlines_when_join_would_recombine_graphemes() {
    for rows in [["か", "\u{3099}き"], ["alpha", "\u{0301}beta"]] {
        let result = fixture(&rows, "\n");
        assert_preserved(&result);
        let layout = OcrCopyLayout::new(&result);
        let copied = layout
            .copy_mapped(0..layout.text().len(), CopyLayoutMode::Auto)
            .unwrap();
        assert!(copied
            .spans
            .iter()
            .all(|span| span.kind == CopySpanKind::Verbatim));
    }
}

#[test]
fn every_legal_subrange_has_complete_exact_mapping_and_copy_consistency() {
    let result = fixture(&["中e\u{301}👩‍💻文", "继续🙂text", "末尾"], "\r\n");
    let layout = OcrCopyLayout::new(&result);
    let mut boundaries: Vec<_> = result.text.grapheme_indices(true).map(|(i, _)| i).collect();
    boundaries.push(result.text.len());
    for (index, start) in boundaries.iter().enumerate() {
        for end in &boundaries[index..] {
            for mode in [CopyLayoutMode::Original, CopyLayoutMode::Auto] {
                let range = *start..*end;
                let mapped = layout.copy_mapped(range.clone(), mode).unwrap();
                assert_eq!(mapped.source_range, range);
                assert_eq!(layout.copy(range.clone(), mode).unwrap(), mapped.text);
                assert_mapping(&result.text, &mapped);
                if mode == CopyLayoutMode::Original {
                    assert_eq!(mapped.text.as_bytes(), result.text[range].as_bytes());
                }
            }
        }
    }
}

#[test]
fn copy_layout_preserves_dto_text_words_geometry_and_direction() {
    let mut result = fixture(&["alpha beta", "gamma delta"], "\n");
    result.words = vec![
        RecognizedWord {
            text: "beta".into(),
            rect: Rect {
                x: 80,
                width: 60,
                ..row_rect(0)
            },
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        },
        RecognizedWord {
            text: "alpha".into(),
            rect: Rect {
                width: 50,
                ..row_rect(0)
            },
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        },
    ];
    let original_text = result.text.clone();
    let original_regions = result.regions.clone();
    let original_words: Vec<_> = result
        .words
        .iter()
        .map(|word| {
            (
                word.text.clone(),
                word.rect,
                word.line,
                word.horizontal_direction,
            )
        })
        .collect();
    let original_lines: Vec<_> = result
        .lines
        .iter()
        .map(|line| (line.text.clone(), line.rect, line.horizontal_direction))
        .collect();
    let layout = result.copy_layout();
    assert_eq!(
        layout
            .copy(0..layout.text().len(), CopyLayoutMode::Auto)
            .unwrap(),
        "alpha beta gamma delta"
    );
    assert_eq!(result.text, original_text);
    assert_eq!(result.regions, original_regions);
    assert_eq!(
        result
            .words
            .iter()
            .map(|word| (
                word.text.clone(),
                word.rect,
                word.line,
                word.horizontal_direction
            ))
            .collect::<Vec<_>>(),
        original_words
    );
    assert_eq!(
        result
            .lines
            .iter()
            .map(|line| (line.text.clone(), line.rect, line.horizontal_direction))
            .collect::<Vec<_>>(),
        original_lines
    );
    assert_eq!(layout.text(), original_text);
}

#[test]
fn layout_mode_parse_is_explicit_and_original_is_default() {
    assert_eq!(CopyLayoutMode::default(), CopyLayoutMode::Original);
    assert_eq!(
        CopyLayoutMode::parse("original").unwrap(),
        CopyLayoutMode::Original
    );
    assert_eq!(CopyLayoutMode::parse("auto").unwrap(), CopyLayoutMode::Auto);
    assert_eq!(CopyLayoutMode::Original.wire_name(), "original");
    assert_eq!(CopyLayoutMode::Auto.wire_name(), "auto");
    assert!(CopyLayoutMode::parse("automatic").is_err());
}

#[test]
fn auto_cjk_punctuation_seams_require_neighboring_cjk_body_context() {
    for separator in ["\n", "\r\n"] {
        for (rows, expected) in [
            (["中文，", "继续。"], "中文，继续。"),
            (["中文。", "继续中文"], "中文。继续中文"),
            (["中文？！", "继续中文"], "中文？！继续中文"),
            (["中文。」", "「继续中文"], "中文。」「继续中文"),
            (["首段中文", "（继续正文）"], "首段中文（继续正文）"),
            (["中文（", "内容）"], "中文（内容）"),
            (["正文", "）续文"], "正文）续文"),
            (["首段中文）", "（继续正文"], "首段中文）（继续正文"),
            (["中文，", "“继续中文”"], "中文，“继续中文”"),
            (["日本語、", "「続き」"], "日本語、「続き」"),
        ] {
            let result = fixture(&rows, separator);
            let original = result.text.clone();
            let layout = OcrCopyLayout::new(&result);
            let copied = layout
                .copy_mapped(0..original.len(), CopyLayoutMode::Auto)
                .unwrap();
            assert_eq!(copied.text, expected);
            assert_mapping(&original, &copied);
            assert_eq!(layout.text().as_bytes(), original.as_bytes());
            assert_eq!(result.text.as_bytes(), original.as_bytes());
            assert_eq!(full_copy(&result, CopyLayoutMode::Original), original);
        }
    }
}

#[test]
fn auto_cjk_punctuation_does_not_infer_language_from_symbols_or_scan_past_other_text() {
    for separator in ["\n", "\r\n"] {
        for rows in [
            ["English，", "继续"],
            ["中文，", "（English"],
            ["中文English）", "继续"],
            ["123，", "继续"],
            ["中文，", "（123"],
            ["，", "（"],
            ["中文,", "继续"],
            ["中文，", "(继续"],
            ["中文 ，", "继续"],
            ["中文，", "（ 继续"],
            ["中文，\u{301}", "继续"],
            ["中文，", "（\u{301}继续"],
        ] {
            let result = fixture(&rows, separator);
            let copied = OcrCopyLayout::new(&result)
                .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
                .unwrap();
            assert_eq!(copied.text, format!("{} {}", rows[0], rows[1]));
            assert_mapping(&result.text, &copied);
            assert_eq!(full_copy(&result, CopyLayoutMode::Original), result.text);
        }
    }
}

#[test]
fn auto_cjk_punctuation_partial_selection_uses_source_context_and_complete_zero_length_join_map() {
    for separator in ["\n", "\r\n"] {
        let result = fixture(&["首段中文）", "（继续正文"], separator);
        let layout = OcrCopyLayout::new(&result);
        let start = result.text.find('）').unwrap();
        let newline = result.text.find(separator).unwrap();
        let end = newline + separator.len() + '（'.len_utf8();
        // 接缝在 new 时依完整可信来源决定；部分选择不重新猜测语种。
        let copied = layout
            .copy_mapped(start..end, CopyLayoutMode::Auto)
            .unwrap();
        assert_eq!(copied.text, "）（");
        assert_eq!(copied.source_range, start..end);
        assert_mapping(&result.text, &copied);
        let joins: Vec<_> = copied
            .spans
            .iter()
            .filter(|span| span.kind == CopySpanKind::LineJoin)
            .collect();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].source_range, newline..newline + separator.len());
        assert!(joins[0].output_range.is_empty());
        assert_eq!(
            layout.copy(start..end, CopyLayoutMode::Original).unwrap(),
            result.text[start..end]
        );
        for range in [
            newline..newline + separator.len(),
            start..newline + separator.len(),
        ] {
            assert_eq!(
                layout.copy(range.clone(), CopyLayoutMode::Auto).unwrap(),
                result.text[range]
            );
        }
        assert!(layout.copy(start + 1..end, CopyLayoutMode::Auto).is_err());
        if separator == "\r\n" {
            assert!(layout
                .copy(start..newline + 1, CopyLayoutMode::Auto)
                .is_err());
        }
    }
}

#[test]
fn auto_cjk_punctuation_keeps_source_whitespace_and_existing_geometry_and_list_guards() {
    for rows in [
        ["中文， ", "继续。"],
        ["中文，", " 继续。"],
        ["中文，", "\t继续。"],
        ["一、中文，", "继续。"],
        ["中文，", "- 继续。"],
    ] {
        assert_preserved(&fixture(&rows, "\r\n"));
    }
    let mut result = fixture(&["中文，", "继续。"], "\n");
    result.lines[1].rect.as_mut().unwrap().y = 100;
    assert_preserved(&result);

    let mut result = fixture(&["中文，", "继续。"], "\n");
    for (index, line) in result.lines.iter_mut().enumerate() {
        let r = row_rect(index);
        let (x, y, width, height) = (r.x as f32, r.y as f32, r.width as f32, r.height as f32);
        line.geometry = Some(TextGeometry {
            quad: [
                crate::annotation::Point { x, y },
                crate::annotation::Point { x: x + width, y },
                crate::annotation::Point {
                    x: x + width,
                    y: y + height,
                },
                crate::annotation::Point { x, y: y + height },
            ],
        });
    }
    result.words = result
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| RecognizedWord {
            text: line.text.clone(),
            rect: row_rect(index),
            line: index,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: line.geometry,
        })
        .collect();
    assert_eq!(full_copy(&result, CopyLayoutMode::Auto), "中文，继续。");
    result.lines[0].geometry.as_mut().unwrap().quad.swap(1, 3);
    assert_preserved(&result);
    result.lines[0].geometry = None;
    result.lines[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert_preserved(&result);
}
