use super::*;

fn document(kind: BlockKind, source: &str) -> Document {
    Document {
        source: source.into(),
        blocks: vec![Block {
            source: source.into(),
            kind,
        }],
        diagnostics: vec![],
    }
}

fn ink(image: &Image) -> usize {
    image
        .bgra
        .chunks_exact(4)
        .filter(|pixel| pixel[..3] != [255, 255, 255])
        .count()
}

#[test]
fn markdown_export_native_text_math_and_table_have_visible_content() {
    let source = "# Report 报告\n\nThe value is \\(x^2+1\\).\n\n- **Strong** and *italic*\n\n| Item | Formula |\n| :-- | --: |\n| 编号 00012 | $\\frac{1}{2}$ |\n\n```text\nkeep  two spaces\nnext line\n```";
    let input = crate::markdown_document::parse_with_mode(source, false).unwrap();
    let output = export_image(&input, 640, 18.).unwrap();
    assert!(output.width >= 640 && output.height > 150);
    assert!(
        ink(&output) > 500,
        "the export contains glyphs and math, not an empty canvas"
    );
    assert!(output.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
    assert_eq!(&output.bgra[..4], &[255, 255, 255, 255]);
    assert_eq!(input.source, source);
}

#[test]
fn markdown_export_table_draws_continuous_rows_and_columns() {
    let input = document(
        BlockKind::Table {
            alignments: vec![Alignment::Left, Alignment::Right],
            rows: vec![
                vec![
                    vec![Inline::Text("ID".into())],
                    vec![Inline::Text("Value".into())],
                ],
                vec![
                    vec![Inline::Text("00012".into())],
                    vec![Inline::Math {
                        source: "x^2".into(),
                    }],
                ],
            ],
        },
        "| ID | Value |\n| -- | --: |\n| 00012 | $x^2$ |",
    );
    let output = export_image(&input, 240, 18.).unwrap();
    let row = PAD as u32;
    let black = |x: u32, y: u32| {
        let offset = ((y * output.width + x) * 4) as usize;
        output.bgra[offset..offset + 3] == [0, 0, 0]
    };
    let top_run = (PAD as u32..output.width)
        .take_while(|x| black(*x, row))
        .count();
    assert!(top_run >= 160, "a complete table top border is present");
    let left_run = (PAD as u32..output.height)
        .take_while(|y| black(PAD as u32, *y))
        .count();
    assert!(
        left_run > 65,
        "a complete table vertical border spans both rows"
    );
    assert!(
        ink(&output) > top_run + left_run + 100,
        "cell content also survives export"
    );
}

#[test]
fn markdown_export_code_wraps_at_requested_width_without_discarding_lines() {
    let code = format!("{}\nsecond  line", "long_identifier_".repeat(12));
    let input = document(
        BlockKind::Code {
            language: Some("text".into()),
            code,
        },
        "original code source",
    );
    let narrow = export_image(&input, 240, 16.).unwrap();
    let wide = export_image(&input, 640, 16.).unwrap();
    assert_eq!(narrow.width, 240);
    assert_eq!(wide.width, 640);
    assert!(narrow.height > wide.height);
    assert!(ink(&narrow) > 200 && ink(&wide) > 200);
}

#[test]
fn markdown_export_rejects_bad_math_oversize_and_diagnostics_explicitly() {
    let bad = document(
        BlockKind::Math {
            source: "\\frac{".into(),
        },
        "\\[\\frac{\\]",
    );
    assert!(export_image(&bad, 480, 18.).is_err());
    assert!(export_image(&bad, 0, 18.).is_err());
    assert!(export_image(&bad, 480, f32::NAN).is_err());
    let oversized = document(
        BlockKind::Table {
            alignments: vec![Alignment::None; 33],
            rows: vec![],
        },
        "oversized table",
    );
    assert!(export_image(&oversized, 480, 18.).is_err());
    let mut warning = document(
        BlockKind::Paragraph(vec![Inline::Text("still readable".into())]),
        "still readable",
    );
    warning
        .diagnostics
        .push(crate::markdown_document::Diagnostic {
            range: 0..1,
            message: "needs correction".into(),
        });
    assert!(export_image(&warning, 480, 18.)
        .err()
        .unwrap()
        .contains("needs correction"));
    assert!(checked_size(MAX_SIDE, MAX_SIDE).is_err());
}

#[test]
fn markdown_export_single_formula_keeps_tight_size_and_mixed_text_uses_document_width() {
    let input = document(
        BlockKind::Paragraph(vec![
            Inline::Text("  ".into()),
            Inline::SoftBreak,
            Inline::Math {
                source: "x^2+1".into(),
            },
            Inline::HardBreak,
        ]),
        "  $x^2+1$  ",
    );
    let tight = export_image(&input, 720, 26.).unwrap();
    assert!(tight.width < 200 && tight.height < 160);
    assert!(ink(&tight) > 20);
    let mixed = document(
        BlockKind::Paragraph(vec![
            Inline::Text("Value ".into()),
            Inline::Math {
                source: "x^2+1".into(),
            },
        ]),
        "Value $x^2+1$",
    );
    assert_eq!(export_image(&mixed, 720, 26.).unwrap().width, 720);
}

#[test]
fn markdown_export_very_long_identifier_reaches_grapheme_wrapping() {
    let input = document(
        BlockKind::Paragraph(vec![Inline::Text("abc_".repeat(180))]),
        "long identifier",
    );
    let output = export_image(&input, 240, 16.).unwrap();
    assert_eq!(output.width, 240);
    assert!(output.height > 300 && ink(&output) > 1000);
}
