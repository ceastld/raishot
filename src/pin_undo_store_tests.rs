//! 在 pin_store 的 cfg(test) 子模块中接入；只定义关闭恢复事务模型。
use super::*;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
};

struct StoreFixture(PathBuf);
impl StoreFixture {
    fn new(label: &str) -> Self {
        let root = PathBuf::from("target/qa");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("pin-undo-store-{label}-{}", unique_key().unwrap()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for StoreFixture {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn closed_document(
    fixture: &StoreFixture,
    style: Option<SelectionFinishStyle>,
) -> (PathBuf, WireRasterDocument, Vec<Annotation>) {
    let full = Rect::full(9, 7);
    let mut editor = Editor::new(full, Some(full));
    let mut mark = Annotation::new(Tool::Rectangle, Point { x: 1., y: 1. }, Style::default());
    mark.update(Point { x: 6., y: 5. });
    editor.document.insert(mark);
    let mut second = Annotation::new(Tool::Rectangle, Point { x: 2., y: 2. }, Style::default());
    second.update(Point { x: 7., y: 6. });
    editor.document.insert(second);
    editor.document.undo();
    assert!(editor.document.can_undo() && editor.document.can_redo());
    let marks = editor
        .document
        .items()
        .iter()
        .map(|object| object.annotation.clone())
        .collect();
    let document = WireRasterDocument::from_editor(&editor, (9, 7)).unwrap();
    save_raster_document_in(
        &fixture.0,
        Image {
            width: 9,
            height: 7,
            bgra: [10, 30, 50, 123].repeat(9 * 7),
            readback_ms: 0.,
        },
        &PinState::default(),
        Placement {
            x: -19,
            y: 13,
            width: 9,
            height: 7,
        },
        PinWindowSizing::ExactCapture,
        style,
        document.clone(),
    )
    .unwrap();
    let paths = files(&fixture.0).unwrap();
    assert_eq!(paths.len(), 1, "夹具独占一条新记录，不触发保留策略");
    (paths[0].clone(), document, marks)
}

#[test]
fn corrupt_closed_history_keeps_original_record_bytes_and_releases_lease() {
    let fixture = StoreFixture::new("corrupt-history");
    let (path, document, _) = closed_document(&fixture, None);
    let png = path.with_extension("png");
    let json = path.with_extension("json");
    let ini_bytes = std::fs::read(&path).unwrap();
    let png_bytes = std::fs::read(&png).unwrap();
    let original_json = std::fs::read(&json).unwrap();
    let mut encoded: serde_json::Value = serde_json::from_slice(&original_json).unwrap();
    assert!(
        encoded["document"]["history"].is_object(),
        "真实编辑快照须已保存有界历史"
    );
    encoded["document"]["history"] = serde_json::Value::String("损坏历史类型".into());
    let corrupt_json = serde_json::to_vec(&encoded).unwrap();
    std::fs::write(&json, &corrupt_json).unwrap();

    assert!(restore_in(&fixture.0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), ini_bytes, "失败不得消费 INI");
    assert_eq!(std::fs::read(&png).unwrap(), png_bytes, "失败不得修改原图");
    assert_eq!(
        std::fs::read(&json).unwrap(),
        corrupt_json,
        "失败保留损坏记录以便修复"
    );

    std::fs::write(&json, original_json).unwrap();
    let restored = restore_in(&fixture.0).unwrap();
    assert!(restored.presentation_style.is_none(), "普通文档不强加外观");
    assert_eq!(restored.raster_document, Some(document.clone()));
    assert_eq!(
        restored.image.as_ref().unwrap().bgra,
        [10, 30, 50, 123].repeat(9 * 7)
    );
    let full = Rect::full(9, 7);
    let mut editor = Editor::new(full, Some(full));
    restored
        .raster_document
        .as_ref()
        .unwrap()
        .restore_into(&mut editor)
        .unwrap();
    assert!(editor.document.can_undo(), "实际保存的撤销栈可达");
    assert!(editor.document.can_redo(), "实际保存的重做栈可达");
    assert_eq!(editor.document.items()[0].id, 1);
    assert_eq!(
        WireRasterDocument::from_editor(&editor, (9, 7)).unwrap(),
        document,
        "对象、双栈及分配器身份完整恢复"
    );
    editor.document.redo();
    assert_eq!(editor.document.items()[1].id, 2, "重做沿用原对象身份");
    drop(restored);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        ini_bytes,
        "放弃初始化仍不消费"
    );
    restore_in(&fixture.0).unwrap().commit().unwrap();
    assert!(!path.exists() && !png.exists() && !json.exists());
}

#[test]
fn legacy_closed_document_without_history_restores_marks_with_empty_undo_redo() {
    let fixture = StoreFixture::new("legacy-history");
    let style = SelectionFinishStyle::default();
    let (path, _, marks) = closed_document(&fixture, Some(style));
    let png = path.with_extension("png");
    let json = path.with_extension("json");
    let ini_bytes = std::fs::read(&path).unwrap();
    let mut encoded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    assert!(encoded["style"].is_object(), "旧外观对象仍可解为 Some");
    assert!(encoded["document"]
        .as_object_mut()
        .unwrap()
        .remove("history")
        .is_some());
    std::fs::write(&json, serde_json::to_vec(&encoded).unwrap()).unwrap();

    let restored = restore_in(&fixture.0).unwrap();
    assert_eq!(restored.presentation_style, Some(style));
    let full = Rect::full(9, 7);
    let mut editor = Editor::new(full, Some(full));
    restored
        .raster_document
        .as_ref()
        .unwrap()
        .restore_into(&mut editor)
        .unwrap();
    let actual: Vec<_> = editor
        .document
        .items()
        .iter()
        .map(|object| object.annotation.clone())
        .collect();
    assert_eq!(actual, marks, "旧文档保留实际标注");
    assert_eq!(editor.crop, Some(full));
    assert!(!editor.document.can_undo(), "恢复不能伪造一次插入命令");
    assert!(!editor.document.can_redo(), "缺历史字段默认为空历史");
    drop(restored);
    assert_eq!(std::fs::read(&path).unwrap(), ini_bytes);
    assert!(png.exists() && json.exists());
    restore_in(&fixture.0).unwrap().commit().unwrap();
    assert!(!path.exists() && !png.exists() && !json.exists());
}
