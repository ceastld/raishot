//! 用真实查找范围、可信图上几何与 MarkLedger 串接四种整批标记。
use super::*;
use crate::{
    annotation::{Style, Tool},
    document::Document,
    geometry::Rect,
    ocr_markup::{MarkLedger, TextMark},
    recognition::{HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord},
};

fn access() -> OcrFindAccess {
    OcrFindAccess {
        active: true,
        idle: true,
        source_current: true,
        composing: false,
    }
}
fn source() -> OcrFindDocument<u64> {
    OcrFindDocument::new(
        OcrFindStamp {
            source: 41,
            result_generation: 8,
            content_revision: 5,
        },
        Arc::from("A A"),
    )
    .unwrap()
}
fn selection() -> SelectionText {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 60,
        height: 20,
    };
    let geometry = Some(TextGeometry {
        quad: [
            Point { x: 20., y: 30. },
            Point { x: 80., y: 30. },
            Point { x: 80., y: 50. },
            Point { x: 20., y: 50. },
        ],
    });
    SelectionText::from_recognized(&RecognizedText {
        text: "A A".into(),
        regions: Vec::new(),
        lines: vec![RecognizedLine {
            text: "A A".into(),
            rect: Some(rect),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
            graphemes: None,
        }],
        words: vec![RecognizedWord {
            text: "A A".into(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
        }],
    })
}
fn clip() -> Bounds {
    Bounds {
        x: 0.,
        y: 0.,
        width: 120.,
        height: 80.,
    }
}

#[test]
fn four_bulk_find_marks_use_real_ranges_toggle_and_one_undo() {
    let body = source();
    let mut find = OcrFind::default();
    find.update(&body, "A", access()).unwrap();
    let ticket = find.all_ticket(&body, access()).unwrap();
    assert_eq!(
        find.resolve_all_ticket(&ticket, &body, access()).unwrap(),
        &[0..1, 2..3]
    );
    let quads = find
        .resolve_all_quads(&ticket, &body, &selection(), clip(), access())
        .unwrap();
    assert_eq!(quads.len(), 2);
    for kind in [
        TextMark::Highlight,
        TextMark::Underline,
        TextMark::Wave,
        TextMark::Strike,
    ] {
        let mut document = Document::default();
        let mut ledger = MarkLedger::default();
        let style = Style {
            color: 0x3faaff,
            width: 2.,
            opacity: 0.9,
            ..Style::default()
        };
        let edit = ledger.prepare(&document, kind, &quads, style).unwrap();
        let annotations = edit.annotations(&document);
        assert_eq!(annotations.len(), 2);
        for annotation in &annotations {
            match kind {
                TextMark::Highlight => assert_eq!(annotation.tool, Tool::HighlighterRect),
                TextMark::Underline | TextMark::Strike => {
                    assert_eq!(annotation.tool, Tool::Pen);
                    assert_eq!(annotation.points.len(), 2);
                    let expected = if kind == TextMark::Strike { 41. } else { 50. };
                    assert!(annotation
                        .points
                        .iter()
                        .all(|point| (point.y - expected).abs() < 0.001));
                }
                TextMark::Wave => {
                    assert_eq!(annotation.tool, Tool::Pen);
                    assert!(annotation.points.len() > 2);
                    assert!(annotation.points.iter().any(|point| point.y > 50.));
                    assert!(annotation.points.iter().any(|point| point.y < 50.));
                }
            }
        }
        assert!(ledger.commit(&mut document, edit));
        let marked = document.items().to_vec();
        assert_eq!(document.bounded_history_snapshot().unwrap().undo.len(), 1);
        document.undo();
        assert!(document.items().is_empty());
        assert!(!document.can_undo());
        document.redo();
        assert_eq!(document.items(), marked);
        let toggle = ledger.prepare(&document, kind, &quads, style).unwrap();
        assert!(toggle.annotations(&document).is_empty());
        assert!(ledger.commit(&mut document, toggle));
        assert!(document.items().is_empty());
        document.undo();
        assert_eq!(document.items(), marked);
    }
}

#[test]
fn bulk_mark_source_or_geometry_failure_preserves_document_and_redo() {
    let body = source();
    let mut find = OcrFind::default();
    find.update(&body, "A", access()).unwrap();
    let old = find.all_ticket(&body, access()).unwrap();
    let selection = selection();
    let quads = find
        .resolve_all_quads(&old, &body, &selection, clip(), access())
        .unwrap();
    let mut document = Document::default();
    let mut ledger = MarkLedger::default();
    let edit = ledger
        .prepare(&document, TextMark::Underline, &quads, Style::default())
        .unwrap();
    assert!(ledger.commit(&mut document, edit));
    document.undo();
    let revision = (document.revision(), document.edit_revision());
    let history = document.bounded_history_snapshot().unwrap();
    // 同字节与数值 stamp 的新来源也不能借用旧整批票据。
    assert!(matches!(
        find.resolve_all_quads(&old, &source(), &selection, clip(), access()),
        Err(OcrFindError::StaleIntent)
    ));
    // 一个字形越界就拒绝全部，不能只保留第一个仍在界内的命中。
    assert!(matches!(
        find.resolve_all_quads(
            &old,
            &body,
            &selection,
            Bounds {
                width: 50.,
                ..clip()
            },
            access()
        ),
        Err(OcrFindError::OutsideVisibleBounds)
    ));
    let mut missing = selection;
    missing.lines[0].cells.pop();
    assert!(matches!(
        find.resolve_all_quads(&old, &body, &missing, clip(), access()),
        Err(OcrFindError::MissingGlyphGeometry)
    ));
    find.update(&body, " ", access()).unwrap();
    find.update(&body, "A", access()).unwrap();
    assert!(matches!(
        find.resolve_all_ticket(&old, &body, access()),
        Err(OcrFindError::StaleIntent)
    ));
    assert_eq!((document.revision(), document.edit_revision()), revision);
    assert!(document.items().is_empty());
    let after = document.bounded_history_snapshot().unwrap();
    assert_eq!(after.undo, history.undo);
    assert_eq!(after.redo, history.redo);
    assert!(document.can_redo());
}
