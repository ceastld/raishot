//! 局部样式在编辑、撤销与三种文档格式中的保存恢复。
use super::*;
use crate::annotation::rich::{Paragraph, RichText, Run, RunStyle};
use crate::pin_raster_document::WireRasterDocument;
use std::sync::Arc;

const TEXT: &str = "标题 link\n正文";

fn rich_text() -> Annotation {
    let mut a = Annotation::new(Tool::Text, Point { x: 2., y: 3. }, Style::default());
    a.update(Point { x: 200., y: 90. });
    a.text = TEXT.into();
    let mut rich = RichText::new(TEXT);
    rich.links.push("https://example.com/".into());
    rich.runs = vec![
        Run {
            start: 0,
            end: 6,
            style: RunStyle {
                size: Some(1.5),
                bold: Some(true),
                font: Some(FontFamily::default()),
                ..Default::default()
            },
        },
        Run {
            start: 7,
            end: 11,
            style: RunStyle {
                color: Some(0x0645ad),
                link: Some(0),
                ..Default::default()
            },
        },
    ];
    rich.paragraphs[0] = Paragraph {
        space_after: 0.5,
        ..Default::default()
    };
    a.rich = Some(Arc::new(rich));
    assert!(a.valid());
    a
}

fn editor_with(annotations: Vec<Annotation>) -> (Editor, Vec<u64>) {
    let full = Rect::full(240, 120);
    let mut e = Editor::new(full, Some(full));
    let ids = annotations
        .into_iter()
        .map(|a| e.document.insert(a))
        .collect();
    (e, ids)
}

fn current(e: &Editor) -> Vec<Annotation> {
    e.document
        .items()
        .iter()
        .map(|o| o.annotation.clone())
        .collect()
}

#[test]
fn rich_text_store_pure_text_uses_version_3_and_round_trips_history() {
    let mut plain = rich_text();
    plain.rich = None;
    let (mut e, ids) = editor_with(vec![plain.clone()]);
    e.document.replace(ids[0], rich_text());
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), true).unwrap();
    assert_eq!(doc.version, 3);
    let json = serde_json::to_value(&doc).unwrap();
    assert_eq!(json["objects"][0]["rich"]["runs"][1]["link"], 0);
    let decoded: WireTextDocument = serde_json::from_value(json).unwrap();
    let full = Rect::full(240, 120);
    let mut restored = Editor::new(full, Some(full));
    decoded.restore_into(&mut restored, true).unwrap();
    assert_eq!(current(&restored), current(&e));
    restored.document.undo();
    assert_eq!(current(&restored), vec![plain]);
}

#[test]
fn rich_text_store_history_only_rich_keeps_version_3_and_plain_stays_version_1() {
    // 当前对象已无局部样式，但撤销栈里还有：仍须让旧版拒读。
    let mut plain = rich_text();
    plain.rich = None;
    let (mut e, ids) = editor_with(vec![rich_text()]);
    e.document.replace(ids[0], plain.clone());
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), true).unwrap();
    assert_eq!(doc.version, 3);
    let (e, _) = editor_with(vec![plain]);
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), true).unwrap();
    assert_eq!(doc.version, 1);
    assert!(!serde_json::to_string(&doc).unwrap().contains("rich"));
}

#[test]
fn rich_text_store_rejects_version_mismatch_and_bad_spans() {
    let (e, _) = editor_with(vec![rich_text()]);
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), true).unwrap();
    let json = serde_json::to_value(&doc).unwrap();
    let rejects = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut bad = json.clone();
        edit(&mut bad);
        let bad: WireTextDocument = serde_json::from_value(bad).unwrap();
        assert!(bad.validate().is_err());
    };
    rejects(&|v| v["version"] = serde_json::json!(1));
    rejects(&|v| v["objects"][0]["rich"]["runs"][0]["end"] = serde_json::json!(4));
    rejects(&|v| v["objects"][0]["rich"]["links"][0] = serde_json::json!("file:///C:/a"));
    let mut plain = json.clone();
    plain["objects"][0].as_object_mut().unwrap().remove("rich");
    plain["history"]["undo"] = serde_json::json!([]);
    let mut plain: WireTextDocument = serde_json::from_value(plain).unwrap();
    assert!(plain.validate().is_err(), "无局部样式却写 3");
    plain.version = 1;
    plain.validate().unwrap();
}

#[test]
fn rich_text_store_mixed_and_raster_documents_version_the_inner_payload() {
    let rectangle = Annotation::new(Tool::Rectangle, Point { x: 1., y: 1. }, Style::default());
    let mut rectangle = rectangle;
    rectangle.update(Point { x: 20., y: 20. });
    let (e, _) = editor_with(vec![rich_text(), rectangle]);
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), true).unwrap();
    assert_eq!(doc.version, 2);
    let json = serde_json::to_value(&doc).unwrap();
    assert_eq!(json["mixedDocument"]["version"], 2);
    let full = Rect::full(240, 120);
    let mut restored = Editor::new(full, Some(full));
    doc.restore_into(&mut restored, true).unwrap();
    assert_eq!(current(&restored), current(&e));

    let raster = WireRasterDocument::from_editor(&e, (240, 120)).unwrap();
    let mut json = serde_json::to_value(&raster).unwrap();
    assert_eq!(json["version"], 2);
    let mut restored = Editor::new(full, Some(full));
    raster.restore_into(&mut restored).unwrap();
    assert_eq!(current(&restored), current(&e));
    json["version"] = serde_json::json!(1);
    let old: WireRasterDocument = serde_json::from_value(json).unwrap();
    assert!(old.validate().is_err());
    let mut on_shape = serde_json::to_value(&raster).unwrap();
    on_shape["objects"][1]["rich"] = on_shape["objects"][0]["rich"].clone();
    let on_shape: WireRasterDocument = serde_json::from_value(on_shape).unwrap();
    assert!(on_shape.validate().is_err(), "只有文字可带局部样式");
}

#[test]
fn rich_text_store_dark_theme_keeps_card_fill_colors() {
    let mut card = rich_text();
    let (_, dark_fg) = crate::pin_text_source::colors(false);
    card.style.color = dark_fg;
    Arc::make_mut(card.rich.as_mut().unwrap()).card_fill = Some(0x1e1e1e);
    let (e, _) = editor_with(vec![card.clone()]);
    let doc = WireTextDocument::try_from_vector_text_editor(&e, (240, 120), false).unwrap();
    assert_eq!(
        doc.objects[0].style.color, dark_fg,
        "整卡来源配色不换回浅色基准"
    );
}

#[test]
fn rich_text_editor_edits_rebase_spans_and_object_style_clears_overrides() {
    let (mut e, ids) = editor_with(vec![rich_text()]);
    e.begin_text(Some(ids[0]), Point { x: 2., y: 3. });
    // 在“标”后插入：继承粗体；“link”后续写不扩大链接。
    e.replace_text(3..3, "大");
    e.set_text(format!(
        "{}!",
        e.text_draft.as_ref().unwrap().annotation.text
    ));
    e.finish_text();
    let a = e.document.get(ids[0]).unwrap().clone();
    assert_eq!(a.text, "标大题 link\n正文!");
    let rich = a.rich.as_ref().unwrap();
    assert_eq!((rich.runs[0].start, rich.runs[0].end), (0, 9));
    assert_eq!((rich.runs[1].start, rich.runs[1].end), (10, 14));
    assert_eq!(rich.style_at(a.text.len() - 1), RunStyle::default());
    assert!(a.valid());

    e.selected = Some(ids[0]);
    e.style = a.style;
    let mut red = a.style;
    red.color = 0xff0000;
    e.set_style(red);
    let a = e.document.get(ids[0]).unwrap();
    assert_eq!(a.style.color, 0xff0000);
    let rich = a.rich.as_ref().unwrap();
    assert!(rich.runs.iter().all(|r| r.style.color.is_none()));
    assert_eq!(rich.runs[1].style.link, Some(0), "只清颜色，链接保留");
    assert_eq!(rich.runs[0].style.size, Some(1.5));
}
