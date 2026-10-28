//! 历史列表的贴图来源：只读已有兼容图，文字从当前矢量对象提取。
use super::{downscale, Thumb, THUMB_HEIGHT, THUMB_WIDTH};
use crate::{pin_text_document::WireTextDocument, pin_workspace, Result};
use std::time::{Duration, SystemTime};

pub(super) fn revision(name: &str) -> Option<(u64, Option<SystemTime>)> {
    let path = pin_workspace::asset_path(name).ok()?;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()))
}

pub(super) fn text_preview(document: &WireTextDocument) -> Option<String> {
    let mut preview = String::new();
    for text in document.current_texts().map(str::trim) {
        if text.is_empty() {
            continue;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.extend(text.chars().take(240 - preview.chars().count()));
        if preview.chars().count() >= 240 {
            break;
        }
    }
    (!preview.is_empty()).then_some(preview)
}

pub(super) fn load(name: &str, cancelled: &dyn Fn() -> bool) -> Result<Option<Thumb>> {
    check_cancel(cancelled)?;
    let path = pin_workspace::asset_path(name)?;
    crate::image_collection::ensure_local_file(&path)?;
    let before = revision(name).ok_or("贴图预览资源不存在")?;
    // 与截图及录屏预览共用解码许可，避免列表同时保留多份完整图片。
    let _permit = loop {
        check_cancel(cancelled)?;
        if let Some(permit) = crate::history::decode_gate().try_enter() {
            break permit;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let image = crate::export::read_image(&path)?;
    check_cancel(cancelled)?;
    if revision(name) != Some(before) {
        return Err("贴图预览资源已改变，请刷新".into());
    }
    let thumb = downscale(
        image.width,
        image.height,
        &image.bgra,
        THUMB_WIDTH,
        THUMB_HEIGHT,
    );
    check_cancel(cancelled)?;
    Ok(Some(thumb))
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(tr!("缩略图解码已取消", "Thumbnail decoding cancelled").into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        editor::Editor,
        geometry::Rect,
    };

    #[test]
    fn library_pin_thumbnail_text_preview_uses_current_mixed_document() {
        let full = Rect::full(300, 200);
        let mut editor = Editor::new(full, Some(full));
        let mut annotation = Annotation::new(Tool::Text, Point { x: 2., y: 3. }, Style::default());
        annotation.update(Point { x: 290., y: 190. });
        annotation.text = "当前文字".into();
        let id = editor.document.insert(annotation.clone());
        annotation.text = "撤销的文字".into();
        editor.document.replace(id, annotation);
        editor.document.undo();
        let mut shape =
            Annotation::new(Tool::Rectangle, Point { x: 10., y: 10. }, Style::default());
        shape.update(Point { x: 20., y: 20. });
        editor.document.insert(shape);
        let document =
            WireTextDocument::try_from_vector_text_editor(&editor, (300, 200), true).unwrap();
        assert_eq!(text_preview(&document).as_deref(), Some("当前文字"));
    }

    #[test]
    fn library_pin_thumbnail_text_preview_bounds_unicode_without_rasterizing() {
        let full = Rect::full(300, 200);
        let mut editor = Editor::new(full, Some(full));
        let mut annotation = Annotation::new(Tool::Text, Point { x: 2., y: 3. }, Style::default());
        annotation.update(Point { x: 290., y: 190. });
        annotation.text = "中文🙂".repeat(100);
        editor.document.insert(annotation);
        let document = WireTextDocument::try_from_text_editor(&editor, (300, 200), true).unwrap();
        let preview = text_preview(&document).unwrap();
        assert_eq!(preview.chars().count(), 240);
        assert!(preview.ends_with('🙂'));
        assert!(load("invalid-source", &|| true).is_err());
    }
}
