//! 贴图选字标记的来源资格与整批提交；固定小图夹具，不启动窗口或 OCR。
use super::{text_mark_history_eligible, TextSelectionStamp};
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    document::Document,
    geometry::Rect,
    ocr_markup::{MarkLedger, TextMark},
    pin_annotation::prepare_text_mark,
    pin_text_selection::SelectionText,
    recognition::{RecognizedText, RecognizedWord},
    scene::{PixelPlane, Scene, SourcePixels},
    transform,
};

fn pixels() -> Vec<u8> {
    [31, 47, 61, 255].repeat(8 * 64)
}
fn source(bgra: &[u8]) -> SourcePixels<'_> {
    let full = Rect::full(8, 64);
    SourcePixels {
        fill: None,
        canvas: full,
        planes: vec![PixelPlane { rect: full, bgra }],
        overlay: None,
    }
}
fn quad(x: f32, y: f32, width: f32, height: f32) -> [Point; 4] {
    transform::corners(Bounds {
        x,
        y,
        width,
        height,
    })
}
fn annotations(document: &Document) -> Vec<Annotation> {
    document
        .items()
        .iter()
        .map(|object| object.annotation.clone())
        .collect()
}
fn stamp(document: &Document) -> TextSelectionStamp {
    (
        17,
        None,
        Some(document.revision()),
        Some(Rect::full(8, 64)),
        true,
    )
}
fn underlay() -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Rectangle,
        Point { x: 1., y: 50. },
        Style {
            color: 0x2ecc71,
            filled: true,
            width: 1.,
            ..Style::default()
        },
    );
    annotation.points.push(Point { x: 7., y: 58. });
    annotation
}

#[test]
fn pin_text_mark_history_preserves_owned_undo_redo_but_rejects_changed_underlay() {
    let bgra = pixels();
    let source = source(&bgra);
    let mut document = Document::default();
    document.insert(underlay());
    let original = document.items().to_vec();
    let before = stamp(&document);
    let scene = Scene::prepare(&annotations(&document), &source);
    let mut ledger = MarkLedger::default();
    let quads = [quad(1., 4., 6., 8.), quad(1., 30., 6., 8.)];
    let (edit, prepared) = prepare_text_mark(
        &ledger,
        &document,
        &scene,
        &source,
        TextMark::Underline,
        &quads,
        Style::default(),
    )
    .unwrap()
    .unwrap();
    assert!(prepared.first_error().is_none());
    assert!(ledger.commit(&mut document, edit));
    let marked = document.items().to_vec();
    assert_eq!(marked.len(), original.len() + 2);
    assert!(ledger.matches_underlay(&document));
    assert!(text_mark_history_eligible(
        Some(before),
        stamp(&document),
        ledger.matches_underlay(&document)
    ));

    for _ in 0..2 {
        document.undo();
        assert_eq!(document.items(), original);
        assert_ne!(stamp(&document).2, before.2);
        assert!(text_mark_history_eligible(
            Some(before),
            stamp(&document),
            ledger.matches_underlay(&document)
        ));
        document.redo();
        assert_eq!(document.items(), marked);
        assert!(text_mark_history_eligible(
            Some(before),
            stamp(&document),
            ledger.matches_underlay(&document)
        ));
    }
    // 再撤销识别前的原对象，不能把来源 revision 的豁免扩大到下层变化。
    document.undo();
    document.undo();
    assert!(document.items().is_empty());
    assert!(!ledger.matches_underlay(&document));
    assert!(!text_mark_history_eligible(
        Some(before),
        stamp(&document),
        ledger.matches_underlay(&document)
    ));
    document.redo();
    assert_eq!(document.items(), original);
    assert!(text_mark_history_eligible(
        Some(before),
        stamp(&document),
        ledger.matches_underlay(&document)
    ));
}

#[test]
fn pin_text_mark_source_gate_rejects_image_crop_or_visibility_despite_owned_ledger() {
    let bgra = pixels();
    let source = source(&bgra);
    let mut document = Document::default();
    let mut ledger = MarkLedger::default();
    let before = stamp(&document);
    let scene = Scene::prepare(&[], &source);
    let (edit, _) = prepare_text_mark(
        &ledger,
        &document,
        &scene,
        &source,
        TextMark::Highlight,
        &[quad(1., 4., 6., 8.)],
        Style::default(),
    )
    .unwrap()
    .unwrap();
    assert!(ledger.commit(&mut document, edit));
    let current = stamp(&document);
    assert!(ledger.matches_underlay(&document));
    assert!(text_mark_history_eligible(Some(before), current, true));
    assert!(!text_mark_history_eligible(None, current, true));
    assert!(!text_mark_history_eligible(Some(before), current, false));
    let changed_sources = [
        (18, current.1, current.2, current.3, current.4),
        (
            current.0,
            current.1,
            current.2,
            Some(Rect::full(8, 60)),
            current.4,
        ),
        (current.0, current.1, current.2, None, current.4),
        (current.0, current.1, current.2, current.3, false),
    ];
    for changed in changed_sources {
        assert!(ledger.matches_underlay(&document));
        assert!(!text_mark_history_eligible(
            Some(before),
            changed,
            ledger.matches_underlay(&document)
        ));
    }
}

#[test]
fn pin_text_mark_preflight_is_atomic_and_multiline_commit_has_one_undo() {
    let bgra = pixels();
    let source = source(&bgra);
    let mut document = Document::default();
    document.insert(underlay());
    document.undo();
    assert!(!document.can_undo() && document.can_redo());
    let revisions = (document.revision(), document.edit_revision());
    let scene = Scene::prepare(&[], &source);
    let mut ledger = MarkLedger::default();
    let selection_text = SelectionText::from_recognized(&RecognizedText {
        text: "AB\nCD".into(),
        regions: Vec::new(),
        words: [4, 30]
            .into_iter()
            .enumerate()
            .map(|(line, y)| RecognizedWord {
                text: if line == 0 { "AB" } else { "CD" }.into(),
                rect: Rect {
                    x: 1,
                    y,
                    width: 6,
                    height: 8,
                },
                line,
                horizontal_direction: Default::default(),
                geometry: None,
            })
            .collect(),
        lines: Vec::new(),
    });
    // 范围与字形仍由选字模型独立持有，不交给文档标记计划消费。
    let selected = 1..4;
    let quads = selection_text.marking_quads(selected.clone());
    assert_eq!(quads.len(), 2);
    assert_eq!(
        selection_text.selected(selected.clone()).as_deref(),
        Some("B\nC")
    );
    let mut invalid = quads.clone();
    invalid[1][2].x = f32::NAN;
    assert!(prepare_text_mark(
        &ledger,
        &document,
        &scene,
        &source,
        TextMark::Highlight,
        &invalid,
        Style::default(),
    )
    .is_err());
    assert!(document.items().is_empty());
    assert!(!document.can_undo() && document.can_redo());
    assert_eq!((document.revision(), document.edit_revision()), revisions);
    assert!(!ledger.matches_underlay(&document));
    assert!(scene.layers.is_empty());

    let (edit, prepared) = prepare_text_mark(
        &ledger,
        &document,
        &scene,
        &source,
        TextMark::Highlight,
        &quads,
        Style {
            color: 0xe74c3c,
            ..Style::default()
        },
    )
    .unwrap()
    .unwrap();
    assert!(prepared.first_error().is_none());
    assert!(document.items().is_empty());
    assert_eq!((document.revision(), document.edit_revision()), revisions);
    assert!(ledger.commit(&mut document, edit));
    assert_eq!(document.items().len(), 2);
    assert!(document.revision() > revisions.0);
    // Document 的渲染 revision 可随 Batch 子项递增，新编辑计数只增加一次。
    assert_eq!(document.edit_revision(), revisions.1 + 1);
    assert!(document.can_undo() && !document.can_redo());
    let marked = document.items().to_vec();
    let mut output = source.read(source.canvas);
    prepared.apply(&mut output, source.canvas);
    assert_ne!(output.bgra, bgra);
    assert_eq!(
        selection_text.selected(selected.clone()).as_deref(),
        Some("B\nC")
    );
    assert_eq!(selection_text.marking_quads(selected.clone()), quads);

    document.undo();
    assert!(document.items().is_empty());
    assert!(!document.can_undo() && document.can_redo());
    assert!(ledger.matches_underlay(&document));
    assert_eq!(selection_text.marking_quads(selected.clone()), quads);
    document.redo();
    assert_eq!(document.items(), marked);
    assert_eq!(selection_text.selected(selected).as_deref(), Some("B\nC"));
}
