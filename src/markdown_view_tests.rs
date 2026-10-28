use super::*;
use std::cell::Cell;

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

fn image() -> Image {
    let mut pixels = vec![255; 10 * 10 * 4];
    for (x, y) in [(4, 4), (5, 5)] {
        let index = (y * 10 + x) * 4;
        pixels[index..index + 4].copy_from_slice(&[0, 0, 0, 255]);
    }
    Image {
        width: 10,
        height: 10,
        bgra: pixels,
        readback_ms: 0.,
    }
}

fn text(atoms: &[Atom]) -> String {
    atoms
        .iter()
        .map(|atom| match atom {
            Atom::Text(text, _) => text.as_str(),
            Atom::Break => "\n",
            Atom::Math(math) => math.source.as_str(),
        })
        .collect()
}

#[test]
fn markdown_view_math_cache_and_local_error_preserve_mixed_document() {
    let calls = Cell::new(0);
    let input = document(
        BlockKind::Paragraph(vec![
            Inline::Text("Before  ".into()),
            Inline::Math {
                source: "x^2".into(),
            },
            Inline::Strong(vec![Inline::Math {
                source: "x^2".into(),
            }]),
            Inline::Math {
                source: "bad".into(),
            },
            Inline::Text(" after".into()),
        ]),
        "Before  $x^2$ **$x^2$** $bad$ after",
    );
    let prepared = prepare_with(&input, true, 16., |source, dark, size| {
        calls.set(calls.get() + 1);
        assert!(dark);
        assert_eq!(size, 16.);
        if source == "bad" {
            Err("local failure".into())
        } else {
            Ok(image())
        }
    });
    assert_eq!(calls.get(), 2, "identical formula resources are shared");
    assert_eq!(prepared.images().count(), 1);
    assert_eq!(prepared.source, input.source);
    assert_eq!(prepared.diagnostics, ["local failure"]);
    assert!(prepared.matches(&input.source, true, 16.));
    assert!(!prepared.matches(&input.source, false, 16.));
    assert!(!prepared.matches(&input.source, true, 18.));
    let PreparedBlock::Paragraph(atoms) = &prepared.blocks[0] else {
        panic!()
    };
    assert_eq!(text(atoms), "Before  x^2x^2bad after");
    assert!(matches!(&atoms[atoms.len() - 1], Atom::Text(text, _) if text == "after"));
    let _ = render(&prepared, Palette::new(false));
    assert_eq!(calls.get(), 2, "element construction never rasterizes math");
}

#[test]
fn markdown_view_table_keeps_alignment_spaces_math_and_bounded_columns() {
    let input = document(
        BlockKind::Table {
            alignments: vec![Alignment::Left, Alignment::Center, Alignment::Right],
            rows: vec![
                vec![
                    vec![Inline::Text("Name".into())],
                    vec![Inline::Text("Math".into())],
                    vec![Inline::Text("ID".into())],
                ],
                vec![
                    vec![Inline::Text("a | b  c".into())],
                    vec![Inline::Math { source: "x".into() }],
                    vec![Inline::Code("00012345678901234567".into())],
                ],
            ],
        },
        "| Name | Math | ID |\n| :-- | :-: | --: |\n| a \\| b  c | $x$ | `00012345678901234567` |",
    );
    let prepared = prepare_with(&input, false, 16., |_, _, _| Ok(image()));
    let PreparedBlock::Table(table) = &prepared.blocks[0] else {
        panic!()
    };
    assert_eq!(table.widths.len(), 3);
    assert!(matches!(table.alignments[1], Alignment::Center));
    assert!(matches!(table.alignments[2], Alignment::Right));
    assert_eq!(text(&table.rows[1][0]), "a | b  c");
    assert_eq!(text(&table.rows[1][2]), "00012345678901234567");
    assert!(matches!(table.rows[1][1][0], Atom::Math(_)));
    let rows = vec![vec![
        vec![Atom::Text(
            "W".repeat(500),
            InlineStyle::default()
        )];
        32
    ]];
    let widths = table_widths(&rows, 32, 48.);
    assert!(widths.iter().all(|width| *width >= 80.));
    assert!(widths.iter().sum::<f32>() <= MAX_TABLE_WIDTH + 0.1);
    let mut cell = vec![Atom::Text(
        "long_identifier_00123  中文".into(),
        InlineStyle::default(),
    )];
    split_cell_text(&mut cell, 60., 24.);
    assert_eq!(text(&cell), "long_identifier_00123  中文");
    assert!(
        cell.len() > 4,
        "narrow cells can wrap a long literal instead of covering the next column"
    );
}

#[test]
fn markdown_view_nested_styles_code_and_breaks_keep_literal_content() {
    let input = document(
        BlockKind::Paragraph(vec![
            Inline::Strong(vec![Inline::Emphasis(vec![Inline::Text("both".into())])]),
            Inline::SoftBreak,
            Inline::Code("$x$  | <tag>".into()),
            Inline::HardBreak,
            Inline::Link {
                destination: "https://invalid.example".into(),
                content: vec![Inline::Text("link".into())],
            },
        ]),
        "**_both_** `$x$  | <tag>`\n[link](https://invalid.example)",
    );
    let prepared = prepare_with(&input, false, 16., |_, _, _| panic!("code is never math"));
    let PreparedBlock::Paragraph(atoms) = &prepared.blocks[0] else {
        panic!()
    };
    assert_eq!(text(atoms), "both $x$  | <tag>\nlink");
    assert!(matches!(&atoms[0], Atom::Text(_, style) if style.strong && style.emphasis));
    assert!(atoms
        .iter()
        .any(|atom| matches!(atom, Atom::Text(_, style) if style.code)));
    assert!(prepared.images().next().is_none());
}

#[test]
fn markdown_view_formula_crop_is_transparent_and_invalid_images_are_local_errors() {
    let cropped = trim_math(image()).unwrap();
    assert_eq!((cropped.width, cropped.height), (6, 6));
    assert_eq!(&cropped.bgra[..4], &[0, 0, 0, 0]);
    assert_eq!(
        cropped
            .bgra
            .chunks_exact(4)
            .filter(|pixel| pixel[3] != 0)
            .count(),
        2
    );
    assert!(trim_math(Image {
        width: u32::MAX,
        height: u32::MAX,
        bgra: vec![],
        readback_ms: 0.
    })
    .is_err());
    assert!(trim_math(Image {
        width: 1,
        height: 1,
        bgra: vec![255; 4],
        readback_ms: 0.
    })
    .is_err());
}

#[test]
fn markdown_view_oversized_table_falls_back_to_complete_source() {
    let input = document(
        BlockKind::Table {
            alignments: vec![Alignment::None; 33],
            rows: vec![],
        },
        "original  source\n| all | retained |",
    );
    let prepared = prepare_with(&input, false, f32::NAN, |_, _, _| panic!());
    assert_eq!(prepared.font_size, 16.);
    assert_eq!(prepared.diagnostics.len(), 1);
    assert!(
        matches!(&prepared.blocks[0], PreparedBlock::Code(None, source) if source == &input.source)
    );
}
