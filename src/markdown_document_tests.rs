use super::*;

fn paragraph(document: &Document, index: usize) -> &[Inline] {
    match &document.blocks[index].kind {
        BlockKind::Paragraph(content) => content,
        other => panic!("expected paragraph, got {other:?}"),
    }
}

fn plain(content: &[Inline]) -> String {
    content
        .iter()
        .map(|inline| match inline {
            Inline::Text(text) | Inline::Code(text) => text.clone(),
            Inline::Math { source } => source.clone(),
            Inline::Strong(children)
            | Inline::Emphasis(children)
            | Inline::Strikethrough(children)
            | Inline::Link {
                content: children, ..
            } => plain(children),
            Inline::SoftBreak | Inline::HardBreak => "\n".to_owned(),
        })
        .collect()
}

#[test]
fn mixed_chinese_and_parenthesized_math_are_separate_without_rewriting() {
    let source = r"其中 \(n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}\)";
    let document = parse_with_mode(source, true).unwrap();
    assert_eq!(document.source, source);
    assert_eq!(document.blocks.len(), 1);
    assert_eq!(document.blocks[0].source, source);
    assert_eq!(
        paragraph(&document, 0),
        &[
            Inline::Text("其中 ".into()),
            Inline::Math {
                source: r"n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}".into()
            },
        ]
    );
    assert!(
        document.diagnostics.is_empty(),
        "{:?}",
        document.diagnostics
    );
}

#[test]
fn all_math_delimiters_preserve_inner_tex_and_display_boundaries() {
    let source = "Before $a_b$ and \\(c^2\\).\n\n$$\\frac{1}{2}$$\n\n\\[x + y\\]\n\nAfter.";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(document.blocks.len(), 4);
    assert_eq!(
        paragraph(&document, 0),
        &[
            Inline::Text("Before ".into()),
            Inline::Math {
                source: "a_b".into()
            },
            Inline::Text(" and ".into()),
            Inline::Math {
                source: "c^2".into()
            },
            Inline::Text(".".into()),
        ]
    );
    assert_eq!(
        document.blocks[1].kind,
        BlockKind::Math {
            source: r"\frac{1}{2}".into()
        }
    );
    assert_eq!(
        document.blocks[2].kind,
        BlockKind::Math {
            source: "x + y".into()
        }
    );
    assert!(
        document.diagnostics.is_empty(),
        "{:?}",
        document.diagnostics
    );
}

#[test]
fn display_math_can_contain_blank_lines_and_markdown_punctuation() {
    let source = "前文 $$\\begin{matrix}a & b\\\\\n\nc & d\\end{matrix}$$ 后文";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(document.blocks.len(), 3);
    assert_eq!(plain(paragraph(&document, 0)), "前文 ");
    assert_eq!(
        document.blocks[1].kind,
        BlockKind::Math {
            source: "\\begin{matrix}a & b\\\\\n\nc & d\\end{matrix}".into()
        }
    );
    assert_eq!(plain(paragraph(&document, 2)), " 后文");
    assert!(document.diagnostics.is_empty());
}

#[test]
fn standard_markdown_keeps_nested_styles_lists_code_and_original_blocks() {
    let source = "# Title\n\nA **strong *nested*** and `code $x$` paragraph.\n\n3. first\n   - nested\n4. second\n\n> quote\n\n```rust\nlet x = \"$raw$\";\n```\n\n---\n";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(document.source, source);
    assert!(matches!(
        &document.blocks[0].kind,
        BlockKind::Heading { level: 1, .. }
    ));
    assert!(paragraph(&document, 1)
        .iter()
        .any(|inline| matches!(inline, Inline::Strong(_))));
    assert!(paragraph(&document, 1)
        .iter()
        .any(|inline| matches!(inline, Inline::Code(code) if code == "code $x$")));
    let BlockKind::List { start, items } = &document.blocks[2].kind else {
        panic!("missing list")
    };
    assert_eq!(*start, Some(3));
    assert_eq!(items.len(), 2);
    assert!(items[0]
        .iter()
        .any(|block| matches!(block.kind, BlockKind::List { .. })));
    assert!(matches!(document.blocks[3].kind, BlockKind::Quote(_)));
    // 结束围栏后的换行是块间空白；完整文档仍逐字保留它。
    assert_eq!(
        document.blocks[4].source,
        "```rust\nlet x = \"$raw$\";\n```"
    );
    assert_eq!(
        document.blocks[4].kind,
        BlockKind::Code {
            language: Some("rust".into()),
            code: "let x = \"$raw$\";\n".into()
        }
    );
    assert!(matches!(document.blocks[5].kind, BlockKind::Rule));
    assert!(document.diagnostics.is_empty());
}

#[test]
fn table_keeps_empty_cells_alignment_code_escaped_pipes_and_math_pipes() {
    let source = "| Name | Value | Empty |\n| :--- | ---: | :---: |\n| `a` | $|x| + a_b$ | |\n| A\\|B | \\(n \\in N\\) | 001 |\n";
    let document = parse_with_mode(source, false).unwrap();
    let BlockKind::Table { alignments, rows } = &document.blocks[0].kind else {
        panic!("expected table: {:?}", document.blocks)
    };
    assert_eq!(document.blocks[0].source, source);
    assert_eq!(
        alignments,
        &[Alignment::Left, Alignment::Right, Alignment::Center]
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].len(), 3);
    assert_eq!(rows[1][0], vec![Inline::Code("a".into())]);
    assert_eq!(
        rows[1][1],
        vec![Inline::Math {
            source: "|x| + a_b".into()
        }]
    );
    assert!(rows[1][2].is_empty());
    assert_eq!(plain(&rows[2][0]), "A|B");
    assert_eq!(
        rows[2][1],
        vec![Inline::Math {
            source: r"n \in N".into()
        }]
    );
    assert_eq!(plain(&rows[2][2]), "001");
    assert!(
        document.diagnostics.is_empty(),
        "{:?}",
        document.diagnostics
    );
}

#[test]
fn malformed_math_remains_visible_without_guessing_or_completing() {
    for source in [
        r"其中 \(x + 1",
        r"text \] tail",
        "$x",
        "$$",
        "before $ $ after",
    ] {
        let document = parse_with_mode(source, true).unwrap();
        assert_eq!(document.source, source);
        assert!(
            !document.diagnostics.is_empty(),
            "missing diagnostic for {source}"
        );
        assert_eq!(plain(paragraph(&document, 0)), source);
        assert!(!paragraph(&document, 0)
            .iter()
            .any(|inline| matches!(inline, Inline::Math { .. })));
    }
}

#[test]
fn escaped_dollars_and_code_do_not_create_math_or_diagnostics() {
    let source = "Price \\$5. `\\(not math\\)` and `$x$`.\n\n```tex\n\\[unclosed\n```\n";
    let document = parse_with_mode(source, false).unwrap();
    assert!(
        document.diagnostics.is_empty(),
        "{:?}",
        document.diagnostics
    );
    assert!(!paragraph(&document, 0)
        .iter()
        .any(|inline| matches!(inline, Inline::Math { .. })));
    assert!(matches!(document.blocks[1].kind, BlockKind::Code { .. }));
}

#[test]
fn html_and_images_are_visible_source_without_loading_resources() {
    let source = "<script>alert('$x$')</script>\n\n![secret](https://example.invalid/a.png)";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(
        plain(paragraph(&document, 0)),
        "<script>alert('$x$')</script>\n"
    );
    assert_eq!(
        plain(paragraph(&document, 1)),
        "![secret](https://example.invalid/a.png)"
    );
    assert!(document.diagnostics.is_empty());
}

#[test]
fn bare_latex_is_only_a_formula_mode_fallback() {
    for source in [
        r"\frac{1}{2}",
        "x^2 + 1",
        "a_n = 2n - 1",
        r"\text{中文} + x",
        "x",
    ] {
        let formula = parse_with_mode(source, true).unwrap();
        assert_eq!(
            formula.blocks[0].kind,
            BlockKind::Math {
                source: source.into()
            },
            "{source}"
        );
        assert!(!matches!(
            parse_with_mode(source, false).unwrap().blocks[0].kind,
            BlockKind::Math { .. }
        ));
    }
    for source in [
        "这是普通说明",
        "Given x = 1",
        "# heading",
        "- item",
        "| A | B |\n|---|---|\n|1|2|",
    ] {
        assert!(
            !matches!(
                parse_with_mode(source, true).unwrap().blocks[0].kind,
                BlockKind::Math { .. }
            ),
            "{source}"
        );
    }
}

#[test]
fn table_bounds_fall_back_to_complete_visible_source() {
    let columns = MAX_TABLE_COLUMNS + 1;
    let row = format!("|{}|\n", vec!["x"; columns].join("|"));
    let separator = format!("|{}|\n", vec!["---"; columns].join("|"));
    let source = format!("{row}{separator}{row}");
    let document = parse_with_mode(&source, false).unwrap();
    assert_eq!(document.blocks.len(), 1);
    assert_eq!(document.blocks[0].source, source);
    assert_eq!(plain(paragraph(&document, 0)), source);
    assert_eq!(document.diagnostics.len(), 1);
}

#[test]
fn document_limits_reject_before_unbounded_conversion() {
    assert!(parse_with_mode(&"x".repeat(MAX_SOURCE_BYTES + 1), false).is_err());
    let nested = format!("{}x\n", "> ".repeat(MAX_NESTING + 1));
    assert!(parse_with_mode(&nested, false).is_err());
}

#[test]
fn rich_content_gate_does_not_treat_plain_paragraphs_as_markdown_layout() {
    for source in ["ordinary text", "第一段\n\n第二段", "left\tright\n1\t2"] {
        assert!(!parse_with_mode(source, false).unwrap().has_rich_content());
    }
    for source in [
        "$x$",
        "**bold**",
        "# title",
        "- item",
        "`code`",
        "| A | B |\n|---|---|\n|1|2|",
    ] {
        assert!(
            parse_with_mode(source, false).unwrap().has_rich_content(),
            "{source}"
        );
    }
}

#[test]
fn currency_prices_do_not_become_math_or_trigger_errors() {
    for source in [
        "Price $5.00",
        "Prices $5 and $10",
        "Range $5-$10",
        "USD $.50 per page",
    ] {
        let document = parse_with_mode(source, false).unwrap();
        assert_eq!(plain(paragraph(&document, 0)), source);
        assert!(!document.has_rich_content());
        assert!(
            document.diagnostics.is_empty(),
            "{source}: {:?}",
            document.diagnostics
        );
    }
    assert_eq!(
        paragraph(&parse_with_mode("$5$", false).unwrap(), 0),
        &[Inline::Math { source: "5".into() }]
    );
}

#[test]
fn unclosed_math_cannot_consume_a_code_span_or_its_dollars() {
    let source = "before $x and `$code$` after";
    let document = parse_with_mode(source, false).unwrap();
    assert!(!document.diagnostics.is_empty());
    assert!(paragraph(&document, 0)
        .iter()
        .any(|inline| matches!(inline, Inline::Code(code) if code == "$code$")));
    assert!(!paragraph(&document, 0)
        .iter()
        .any(|inline| matches!(inline, Inline::Math { .. })));
    assert_eq!(plain(paragraph(&document, 0)), "before $x and $code$ after");
}

#[test]
fn ambiguous_dollar_runs_remain_source_instead_of_guessing_a_formula() {
    let source = "$$$x$$$";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(plain(paragraph(&document, 0)), source);
    assert!(!document.diagnostics.is_empty());
    assert!(!document.has_rich_content());
}

#[test]
fn masks_restore_original_unicode_and_control_text_without_placeholder_collisions() {
    let source = "x\u{1}xxxx 舍入 \\(\\text{中文} + α_1\\) 后文 $x";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(document.source, source);
    assert_eq!(
        paragraph(&document, 0),
        &[
            Inline::Text("x\u{1}xxxx 舍入 ".into()),
            Inline::Math {
                source: "\\text{中文} + α_1".into()
            },
            Inline::Text(" 后文 $x".into()),
        ]
    );
    assert_eq!(document.diagnostics.len(), 1);
}

#[test]
fn repetitive_unclosed_math_is_bounded_before_quadratic_work() {
    let source = r"\(".repeat(12_000);
    assert!(parse_with_mode(&source, false).is_err());
}

#[test]
fn oversized_list_start_does_not_overflow_or_turn_into_a_list() {
    let source = "184467440737095516150. item";
    let document = parse_with_mode(source, false).unwrap();
    assert_eq!(plain(paragraph(&document, 0)), source);
    assert!(!document.has_rich_content());
}
