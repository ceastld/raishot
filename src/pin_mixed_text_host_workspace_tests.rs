//! 混合矢量文字工作区的真实磁盘保存／读取与独立副本模型；不启动窗口。
use super::*;
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
};

const SIZE: (u32, u32) = (32, 24);
struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(new_id("mixed-text-workspace"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        for sub in ["pins", ".runtime"] {
            let _ = std::fs::remove_dir_all(self.0.join(sub));
        }
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}
fn editor() -> Editor {
    let full = Rect::full(SIZE.0, SIZE.1);
    Editor::new(full, Some(full))
}
fn preview() -> Image {
    Image {
        width: SIZE.0,
        height: SIZE.1,
        bgra: [30, 50, 70, 255].repeat((SIZE.0 * SIZE.1) as usize),
        readback_ms: 0.,
    }
}
fn placement() -> Placement {
    Placement {
        x: -1933,
        y: -711,
        width: 48,
        height: 36,
    }
}
fn source() -> Editor {
    let mut source = editor();
    let mut text = Annotation::new(
        Tool::Text,
        Point { x: 1., y: 1. },
        Style {
            color: crate::pin_text_source::colors(true).1,
            ..Default::default()
        },
    );
    text.update(Point { x: 30., y: 10. });
    text.text = "再贴一张 保留全文🙂".into();
    let mut arrow = Annotation::new(
        Tool::Arrow,
        Point { x: 4., y: 13. },
        Style {
            color: 0xc43865,
            arrow_start: true,
            ..Default::default()
        },
    );
    arrow.update(Point { x: 17., y: 20. });
    let mut blur = Annotation::new(
        Tool::Blur,
        Point { x: 18., y: 2. },
        Style {
            block_size: 17,
            ..Default::default()
        },
    );
    blur.set_effect_regions(
        vec![
            Bounds {
                x: 18.,
                y: 2.,
                width: 5.,
                height: 4.,
            },
            Bounds {
                x: 25.,
                y: 8.,
                width: 5.,
                height: 7.,
            },
        ],
        false,
    )
    .unwrap();
    let ids = source.document.insert_many(vec![text, arrow, blur]);
    assert!(source.document.set_group(&ids[..2], true));
    let mut changed = source.document.get(ids[0]).unwrap().clone();
    changed.text = "再贴一张 最新全文🙂".into();
    source.document.replace(ids[0], changed);
    source.document.remove(ids[2]);
    source.document.undo();
    source
}
fn save(dir: &Path, id: &str, document: WireTextDocument) {
    save_snapshot_with_text_document_in(
        dir,
        id,
        SIZE,
        &PinState::default(),
        placement(),
        PinStatus::Closed,
        Some(document),
        |path| export::save_png(preview(), path),
    )
    .unwrap();
}

#[test]
fn mixed_text_workspace_disk_roundtrip_and_duplicate_keep_full_independent_documents() {
    let dir = TestDir::new();
    let source = source();
    let source_revision = source.document.revision();
    let source_edit_revision = source.document.edit_revision();
    let document = WireTextDocument::try_from_vector_text_editor(&source, SIZE, true).unwrap();
    assert_eq!(document.version, 2);
    let (first, _) =
        Session::register_with_entry_in(&dir.0, PinState::default(), placement()).unwrap();
    let first_id = first.id.clone();
    save(&dir.0, &first_id, document.clone());
    let loaded = load_in(&dir.0).unwrap();
    let first_entry = loaded
        .pins
        .iter()
        .find(|entry| entry.id == first_id)
        .unwrap();
    assert_eq!(first_entry.text_document, Some(document.clone()));
    let first_asset = first_entry.image_file.clone().unwrap();
    let first_bytes = std::fs::read(asset_path_in(&dir.0, &first_asset).unwrap()).unwrap();

    // 副本消费同一保存核心取得完整 DTO，不逐项 insert 重造历史。
    let duplicate = WireTextDocument::try_from_vector_text_editor(&source, SIZE, true).unwrap();
    assert_eq!(duplicate, document);
    assert_eq!(source.document.revision(), source_revision);
    assert_eq!(source.document.edit_revision(), source_edit_revision);
    let (second, _) =
        Session::register_with_entry_in(&dir.0, PinState::default(), placement()).unwrap();
    let second_id = second.id.clone();
    save(&dir.0, &second_id, duplicate);
    drop(first);
    drop(second);
    let first_path = asset_path_in(&dir.0, &first_asset).unwrap();
    std::fs::write(&first_path, b"invalid PNG").unwrap();
    assert!(Session::claim_in(&dir.0, &first_id).is_err());
    let failed = load_in(&dir.0).unwrap();
    let preserved = failed
        .pins
        .iter()
        .find(|entry| entry.id == first_id)
        .unwrap();
    assert_eq!(preserved.text_document, Some(document.clone()));
    assert_eq!(preserved.image_file.as_deref(), Some(first_asset.as_str()));
    assert_eq!(std::fs::read(&first_path).unwrap(), b"invalid PNG");
    std::fs::write(&first_path, &first_bytes).unwrap();
    let (first_lease, first_preview, first_entry) = Session::claim_in(&dir.0, &first_id).unwrap();
    let (second_lease, second_preview, second_entry) =
        Session::claim_in(&dir.0, &second_id).unwrap();
    assert_eq!(first_preview.bgra, preview().bgra);
    assert_eq!(second_preview.bgra, first_preview.bgra);
    assert_eq!(first_entry.text_document, second_entry.text_document);
    let mut original_editor = editor();
    let mut duplicate_editor = editor();
    first_entry
        .text_document
        .as_ref()
        .unwrap()
        .restore_into(&mut original_editor, true)
        .unwrap();
    second_entry
        .text_document
        .as_ref()
        .unwrap()
        .restore_into(&mut duplicate_editor, true)
        .unwrap();
    assert_eq!(original_editor.document.items(), source.document.items());
    assert_eq!(
        duplicate_editor
            .document
            .bounded_history_snapshot()
            .unwrap(),
        original_editor.document.bounded_history_snapshot().unwrap()
    );
    let original_snapshot = original_editor.document.bounded_history_snapshot().unwrap();
    duplicate_editor.document.undo();
    assert_ne!(
        duplicate_editor.document.items(),
        original_editor.document.items()
    );
    assert_eq!(
        original_editor.document.bounded_history_snapshot().unwrap(),
        original_snapshot
    );
    assert_eq!(
        WireTextDocument::try_from_vector_text_editor(&source, SIZE, true).unwrap(),
        document
    );
    duplicate_editor.document.redo();
    assert_eq!(
        duplicate_editor.document.items(),
        original_editor.document.items()
    );
    assert_eq!(
        WireTextDocument::try_from_vector_text_editor(&duplicate_editor, SIZE, true).unwrap(),
        document
    );

    duplicate_editor.document.clear();
    let mut pending = Annotation::new(
        Tool::Rectangle,
        Point { x: 3., y: 3. },
        Style {
            color: 0x395b7d,
            ..Default::default()
        },
    );
    pending.update(Point { x: 12., y: 11. });
    let pending_id = duplicate_editor.document.insert(pending);
    duplicate_editor.document.undo();
    assert!(duplicate_editor.document.items().is_empty());
    assert!(duplicate_editor.document.can_undo() && duplicate_editor.document.can_redo());
    let empty_document =
        WireTextDocument::try_from_vector_text_editor(&duplicate_editor, SIZE, true).unwrap();
    save(&dir.0, &second_id, empty_document.clone());
    let loaded = load_in(&dir.0).unwrap();
    let unchanged = loaded
        .pins
        .iter()
        .find(|entry| entry.id == first_id)
        .unwrap();
    assert_eq!(unchanged.text_document, Some(document));
    assert_eq!(unchanged.image_file.as_deref(), Some(first_asset.as_str()));
    assert_eq!(
        std::fs::read(asset_path_in(&dir.0, &first_asset).unwrap()).unwrap(),
        first_bytes
    );
    let saved_empty = loaded
        .pins
        .iter()
        .find(|entry| entry.id == second_id)
        .unwrap()
        .text_document
        .as_ref()
        .unwrap();
    assert_eq!(saved_empty, &empty_document);
    let mut restored_empty = editor();
    saved_empty.restore_into(&mut restored_empty, true).unwrap();
    assert!(restored_empty.document.items().is_empty());
    restored_empty.document.redo();
    assert_eq!(restored_empty.document.items()[0].id, pending_id);
    restored_empty.document.undo();
    restored_empty.document.undo();
    assert_eq!(
        restored_empty.document.items(),
        original_editor.document.items()
    );
    assert_eq!(
        original_editor.document.bounded_history_snapshot().unwrap(),
        original_snapshot
    );
    drop(first_lease);
    drop(second_lease);
}
