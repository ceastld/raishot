use super::*;

#[derive(Clone, PartialEq)]
struct Source {
    image: u64,
    installation: u64,
    body_revision: u64,
}

fn source() -> Source {
    Source {
        image: 4,
        installation: 7,
        body_revision: 0,
    }
}

fn regions() -> Vec<Bounds> {
    vec![
        Bounds {
            x: 18.,
            y: 20.,
            width: 80.,
            height: 16.,
        },
        Bounds {
            x: 126.,
            y: 20.,
            width: 100.,
            height: 16.,
        },
    ]
}

fn original() -> Annotation {
    let mut annotation = Annotation::new(Tool::Mosaic, Point { x: 3., y: 4. }, Style::default());
    annotation
        .set_effect_regions(
            vec![Bounds {
                x: 3.,
                y: 4.,
                width: 10.,
                height: 8.,
            }],
            true,
        )
        .unwrap();
    annotation
}

fn with_redo() -> Document {
    let mut document = Document::default();
    document.insert(original());
    document.undo();
    document
}

fn state(document: &Document) -> (Vec<crate::document::Object>, u64, u64, bool, bool) {
    (
        document.items().to_vec(),
        document.revision(),
        document.edit_revision(),
        document.can_undo(),
        document.can_redo(),
    )
}

#[test]
fn sensitive_effect_multiple_candidates_share_one_unmanaged_undo() {
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut document = Document::default();
        let prior = original();
        document.insert(prior.clone());
        let before = state(&document);
        let identity = Arc::new(());
        let edit = SensitiveEffectEdit::prepare(
            &document,
            source(),
            identity.clone(),
            regions(),
            tool,
            Style::default(),
        )
        .unwrap()
        .unwrap();
        let annotations = edit.annotations(&document).unwrap();
        assert_eq!(annotations.len(), 2);
        assert_eq!(annotations[0], prior);
        assert_eq!(annotations[1].effect_regions_world(), regions());
        assert!(!annotations[1].effect_regions_managed());
        let added = annotations[1].clone();
        assert!(edit.commit(&mut document, &source(), &identity, true));
        assert_eq!(document.items().len(), 2);
        assert_eq!(document.revision(), before.1 + 1);
        assert_eq!(document.edit_revision(), before.2 + 1);
        document.undo();
        assert_eq!(document.items(), before.0.as_slice());
        assert_eq!(document.edit_revision(), before.2 + 1);
        assert!(document.can_redo());
        document.redo();
        assert_eq!(document.items().len(), 2);
        assert_eq!(document.items()[0].annotation, prior);
        assert_eq!(document.items()[1].annotation, added);
    }
}

#[test]
fn sensitive_effect_empty_invalid_and_cancel_keep_redo() {
    let mut document = with_redo();
    let before = state(&document);
    let identity = Arc::new(());
    assert!(SensitiveEffectEdit::prepare(
        &document,
        source(),
        identity.clone(),
        Vec::new(),
        Tool::Mosaic,
        Style::default(),
    )
    .unwrap()
    .is_none());
    assert_eq!(state(&document), before);
    for (tool, region, opacity) in [
        (Tool::Rectangle, regions()[0], 1.),
        (
            Tool::Mosaic,
            Bounds {
                width: 0.,
                ..regions()[0]
            },
            1.,
        ),
        (
            Tool::Blur,
            Bounds {
                x: f32::NAN,
                ..regions()[0]
            },
            1.,
        ),
        (Tool::Mosaic, regions()[0], f32::NAN),
    ] {
        assert!(SensitiveEffectEdit::prepare(
            &document,
            source(),
            identity.clone(),
            vec![region],
            tool,
            Style {
                opacity,
                ..Style::default()
            },
        )
        .is_err());
        assert_eq!(state(&document), before);
    }
    let edit = SensitiveEffectEdit::prepare(
        &document,
        source(),
        identity.clone(),
        regions(),
        Tool::Blur,
        Style::default(),
    )
    .unwrap()
    .unwrap();
    // 同一个拒绝入口用于取消、窗口/输入失活以及 Scene 准备错误。
    assert!(!edit.commit(&mut document, &source(), &identity, false));
    assert_eq!(state(&document), before);
    document.redo();
    assert_eq!(document.items()[0].annotation, original());
}

#[test]
fn sensitive_effect_stale_source_preview_and_document_keep_history() {
    for current in [
        Source {
            image: 5,
            ..source()
        },
        Source {
            installation: 8,
            ..source()
        },
        Source {
            body_revision: 2,
            ..source()
        },
    ] {
        let mut document = with_redo();
        let before = state(&document);
        let identity = Arc::new(());
        let edit = SensitiveEffectEdit::prepare(
            &document,
            source(),
            identity.clone(),
            regions(),
            Tool::Mosaic,
            Style::default(),
        )
        .unwrap()
        .unwrap();
        // source 修订负责正文改走后改回同 bytes 的拒绝；不能只比较文本。
        assert!(!edit.commit(&mut document, &current, &identity, true));
        assert_eq!(state(&document), before);
        document.redo();
        assert_eq!(document.items()[0].annotation, original());
    }
    let mut document = with_redo();
    let identity = Arc::new(());
    let edit = SensitiveEffectEdit::prepare(
        &document,
        source(),
        identity,
        regions(),
        Tool::Mosaic,
        Style::default(),
    )
    .unwrap()
    .unwrap();
    let before = state(&document);
    assert!(!edit.commit(&mut document, &source(), &Arc::new(()), true));
    assert_eq!(state(&document), before);
    document.redo();
    assert_eq!(document.items()[0].annotation, original());

    let mut document = Document::default();
    let identity = Arc::new(());
    let edit = SensitiveEffectEdit::prepare(
        &document,
        source(),
        identity.clone(),
        regions(),
        Tool::Blur,
        Style::default(),
    )
    .unwrap()
    .unwrap();
    document.insert(original());
    document.undo(); // 内容恢复但 revision 不恢复：旧计划不能复活。
    let before = state(&document);
    assert!(edit.annotations(&document).is_none());
    assert!(!edit.commit(&mut document, &source(), &identity, true));
    assert_eq!(state(&document), before);
    document.redo();
    assert_eq!(document.items()[0].annotation, original());
}
