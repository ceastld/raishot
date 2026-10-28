//! 真实主题静默映射产生内部等值命令后，混合矢量保存仍保留有用历史。
use super::*;

#[test]
fn mixed_text_live_theme_noop_normalizes_for_save_and_preserves_mark_rgb_and_empty_current() {
    let size = (32, 24);
    let full = Rect::full(size.0, size.1);
    let mut source = Editor::new(full, Some(full));
    let light_color = crate::pin_text_source::colors(true).1;
    let dark_color = crate::pin_text_source::colors(false).1;
    let mut text = Annotation::new(
        Tool::Text,
        Point { x: 1., y: 1. },
        Style {
            color: light_color,
            ..Default::default()
        },
    );
    text.update(Point { x: 30., y: 10. });
    text.text = "默认文字色与主题🙂".into();
    let mut marker = Annotation::new(
        Tool::Highlighter,
        Point { x: 3., y: 15. },
        Style {
            color: light_color,
            width: 5.,
            highlighter_blend: crate::annotation::HighlighterBlend::Multiply,
            highlighter_shape: crate::annotation::HighlighterShape::Line,
            ..Default::default()
        },
    );
    marker.update(Point { x: 27., y: 20. });
    let ids = source.document.insert_many(vec![text, marker.clone()]);
    let mut custom_dark = source.document.get(ids[0]).unwrap().clone();
    custom_dark.style.color = dark_color;
    source.document.replace(ids[0], custom_dark);
    let before_theme = WireTextDocument::try_from_vector_text_editor(&source, size, true).unwrap();
    assert_eq!(
        serde_json::to_value(before_theme).unwrap()["mixedDocument"]["history"]["undo"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // 当前自选色已等于深色默认色；旧 Insert 和 Replace.before 在实际静默映射后归一。
    source
        .document
        .remap_silently(|annotation| crate::pin_text_source::remap_theme(annotation, true, false));
    let revision = source.document.revision();
    let edit_revision = source.document.edit_revision();
    let document = WireTextDocument::try_from_vector_text_editor(&source, size, false).unwrap();
    assert_eq!(source.document.revision(), revision, "保存不得回写活动文档");
    assert_eq!(source.document.edit_revision(), edit_revision);
    let encoded = serde_json::to_value(&document).unwrap();
    assert_eq!(encoded["version"], 2);
    assert_eq!(
        encoded["mixedDocument"]["history"]["undo"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "只归一已经由可信主题映射造成的等值 Replace"
    );
    assert_eq!(
        source.document.get(ids[1]).unwrap(),
        &marker,
        "与默认文字同 RGB 的普通高亮保持全部参数"
    );
    let mut restored = Editor::new(full, Some(full));
    document.restore_into(&mut restored, true).unwrap();
    assert_eq!(
        restored.document.get(ids[0]).unwrap().style.color,
        light_color
    );
    assert_eq!(restored.document.get(ids[1]).unwrap(), &marker);
    restored.document.undo();
    assert!(
        restored.document.items().is_empty(),
        "首个 Undo 实际撤销插入，不残留无变化一步"
    );
    assert!(!restored.document.can_undo() && restored.document.can_redo());
    restored.document.redo();
    assert_eq!(
        restored
            .document
            .items()
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(restored.document.get(ids[1]).unwrap(), &marker);
    assert_eq!(
        WireTextDocument::try_from_vector_text_editor(&restored, size, true).unwrap(),
        document
    );

    source.document.remove(ids[0]);
    assert!(source
        .document
        .items()
        .iter()
        .all(|object| object.annotation.tool != Tool::Text));
    let marker_only = WireTextDocument::try_from_vector_text_editor(&source, size, false).unwrap();
    let mut marker_restored = Editor::new(full, Some(full));
    marker_only
        .restore_into(&mut marker_restored, true)
        .unwrap();
    assert_eq!(marker_restored.document.items().len(), 1);
    assert_eq!(marker_restored.document.get(ids[1]).unwrap(), &marker);
    source.document.clear();
    let pending_id = source.document.insert(marker.clone());
    source.document.undo();
    assert!(source.document.items().is_empty());
    let empty = WireTextDocument::try_from_vector_text_editor(&source, size, false).unwrap();
    let mut empty_restored = Editor::new(full, Some(full));
    empty.restore_into(&mut empty_restored, true).unwrap();
    assert!(empty_restored.document.items().is_empty());
    assert!(empty_restored.document.can_undo() && empty_restored.document.can_redo());
    empty_restored.document.redo();
    assert_eq!(empty_restored.document.items()[0].id, pending_id);
    empty_restored.document.undo();
    empty_restored.document.undo();
    assert_eq!(empty_restored.document.get(ids[1]).unwrap(), &marker);
}
