//! 混合文字关闭记录的真实磁盘事务模型；不启动窗口，不以 PNG 重造文档。
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
            .join(format!("mixed-text-store-{}", unique_key().unwrap()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
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
        bgra: [80, 110, 140, 255].repeat((SIZE.0 * SIZE.1) as usize),
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
fn source() -> (Editor, Vec<u64>) {
    let mut editor = editor();
    let mut text = Annotation::new(
        Tool::Text,
        Point { x: 1., y: 1. },
        Style {
            color: crate::pin_text_source::colors(true).1,
            ..Default::default()
        },
    );
    text.update(Point { x: 30., y: 10. });
    text.text = "混合原文 e\u{301}\r\n🙂".into();
    let mut rectangle = Annotation::new(
        Tool::Rectangle,
        Point { x: 6., y: 12. },
        Style {
            color: 0x8a3f65,
            filled: true,
            radius: 3.,
            ..Default::default()
        },
    );
    rectangle.update(Point { x: 17., y: 20. });
    let mut mosaic = Annotation::new(
        Tool::Mosaic,
        Point { x: 18., y: 3. },
        Style {
            block_size: 13,
            ..Default::default()
        },
    );
    mosaic
        .set_effect_regions(
            vec![
                Bounds {
                    x: 18.,
                    y: 3.,
                    width: 5.,
                    height: 4.,
                },
                Bounds {
                    x: 25.,
                    y: 8.,
                    width: 5.,
                    height: 6.,
                },
            ],
            true,
        )
        .unwrap();
    let ids = editor.document.insert_many(vec![text, rectangle, mosaic]);
    assert!(editor.document.set_group(&ids[..2], true));
    let mut changed = editor.document.get(ids[0]).unwrap().clone();
    changed.text = "修改后的文字 🙂".into();
    changed.style.bold = true;
    editor.document.replace(ids[0], changed);
    editor.document.remove(ids[2]);
    editor.document.undo();
    assert!(editor.document.can_undo() && editor.document.can_redo());
    (editor, ids)
}
fn assert_current(source: &Editor, restored: &Editor) {
    let expected: Vec<_> = source
        .document
        .items()
        .iter()
        .cloned()
        .map(|mut object| {
            crate::pin_text_source::remap_theme(&mut object.annotation, true, false);
            object
        })
        .collect();
    assert_eq!(restored.document.items(), expected);
    assert_eq!(restored.document.can_undo(), source.document.can_undo());
    assert_eq!(restored.document.can_redo(), source.document.can_redo());
}

#[test]
fn mixed_text_close_sidecar_retry_installs_real_history_before_consuming_record() {
    let dir = TestDir::new();
    let (mut source, ids) = source();
    let document = WireTextDocument::try_from_vector_text_editor(&source, SIZE, true).unwrap();
    assert_eq!(document.version, 2);
    save_text_document_in(
        &dir.0,
        preview(),
        &PinState::default(),
        placement(),
        PinWindowSizing::General,
        document.clone(),
    )
    .unwrap();
    let path = files(&dir.0).unwrap().remove(0);
    let ini = std::fs::read(&path).unwrap();
    let png = std::fs::read(path.with_extension("png")).unwrap();
    let json = std::fs::read(path.with_extension("json")).unwrap();
    let mut damaged: serde_json::Value = serde_json::from_slice(&json).unwrap();
    damaged["document"]["mixedDocument"]["history"]["undo"][0]["commands"][0]["object"]["id"] =
        0.into();
    std::fs::write(
        path.with_extension("json"),
        serde_json::to_vec(&damaged).unwrap(),
    )
    .unwrap();
    assert!(
        restore_in(&dir.0).is_err(),
        "最远历史损坏不得降级为 PNG 恢复或消费记录"
    );
    assert_eq!(std::fs::read(&path).unwrap(), ini);
    assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
    assert_eq!(files(&dir.0).unwrap(), vec![path.clone()]);
    std::fs::write(path.with_extension("json"), &json).unwrap();

    let claimed = restore_in(&dir.0).unwrap();
    assert_eq!(claimed.text_document, Some(document.clone()));
    assert!(claimed.raster_document.is_none() && claimed.processing_history.is_none());
    let wrong = Rect::full(SIZE.0 + 1, SIZE.1);
    assert!(claimed
        .text_document
        .as_ref()
        .unwrap()
        .restore_into(&mut Editor::new(wrong, Some(wrong)), false)
        .is_err());
    drop(claimed);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        ini,
        "目标安装失败释放 claim，仍可重试"
    );
    assert_eq!(std::fs::read(path.with_extension("json")).unwrap(), json);

    let claimed = restore_in(&dir.0).unwrap();
    assert_eq!(claimed.presentation_window.unwrap().placement, placement());
    assert_eq!(
        claimed.image.as_ref().unwrap().bgra,
        preview().bgra,
        "兼容预览独立于矢量文档"
    );
    let mut restored = editor();
    claimed
        .text_document
        .as_ref()
        .unwrap()
        .restore_into(&mut restored, false)
        .unwrap();
    assert_current(&source, &restored);
    assert_eq!(
        restored
            .document
            .items()
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        ids
    );
    assert!(restored.document.items()[0].group.is_some());
    assert_eq!(
        restored
            .document
            .get(ids[2])
            .unwrap()
            .effect_regions_world(),
        source.document.get(ids[2]).unwrap().effect_regions_world()
    );
    assert_eq!(restored.document.get(ids[2]).unwrap().style.block_size, 13);
    assert!(restored
        .document
        .get(ids[2])
        .unwrap()
        .effect_regions_managed());
    assert_eq!(
        WireTextDocument::try_from_vector_text_editor(&restored, SIZE, false).unwrap(),
        document
    );
    for undo in [true, true, false, false, false] {
        if undo {
            source.document.undo();
            restored.document.undo();
        } else {
            source.document.redo();
            restored.document.redo();
        }
        assert_current(&source, &restored);
    }
    assert!(
        restored.document.get(ids[2]).is_none(),
        "真实 pending Redo 删除效果对象"
    );
    assert!(path.exists(), "成功安装前后都由显式提交决定消费");
    claimed.commit().unwrap();
    assert!(
        !path.exists()
            && !path.with_extension("png").exists()
            && !path.with_extension("json").exists()
    );
    assert!(files(&dir.0).unwrap().is_empty());
}
