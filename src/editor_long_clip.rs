//! 剪辑命令入口：只改原图坐标下的几何，不改画布、标注或对象选择。
use super::Editor;
use crate::{long_clip::LongClip, stitch::ScrollAxis, Result};
use std::ops::Range;

impl Editor {
    pub(crate) fn long_clip(&self) -> Option<&LongClip> {
        self.document.long_clip()
    }

    fn long_clip_ready(&self) -> Result<()> {
        if self.gesture.is_some()
            || self.text_draft.is_some()
            || self.lasso_draft.is_some()
            || self.watermark.has_preview()
        {
            return Err("请先完成或取消当前编辑，再调整长图剪辑".into());
        }
        Ok(())
    }

    fn check_long_clip_source(&self, clip: &LongClip) -> Result<()> {
        clip.validate()?;
        if clip.source_size() != (self.canvas.width, self.canvas.height) {
            return Err("长图剪辑的原图尺寸与编辑画布不一致".into());
        }
        Ok(())
    }

    pub(crate) fn initialize_long_clip(&mut self, clip: LongClip) -> Result<bool> {
        self.long_clip_ready()?;
        self.check_long_clip_source(&clip)?;
        let changed = self.document.initialize_long_clip(clip)?;
        if changed {
            self.preview_revision += 1;
        }
        Ok(changed)
    }

    pub(crate) fn set_long_clip(&mut self, after: Option<LongClip>) -> Result<bool> {
        self.long_clip_ready()?;
        if let Some(clip) = after.as_ref() {
            self.check_long_clip_source(clip)?;
        }
        let changed = self.document.set_long_clip(after)?;
        if changed {
            self.preview_revision += 1;
        }
        Ok(changed)
    }

    fn change_long_clip(
        &mut self,
        edit: impl FnOnce(&mut LongClip) -> Result<bool>,
    ) -> Result<bool> {
        self.long_clip_ready()?;
        let mut clip = self.long_clip().cloned().ok_or("当前文档没有长图剪辑")?;
        if !edit(&mut clip)? {
            return Ok(false);
        }
        self.set_long_clip(Some(clip))
    }

    pub(crate) fn delete_long_clip_range(&mut self, range: Range<u32>) -> Result<bool> {
        self.change_long_clip(|clip| clip.delete(range))
    }

    /// 一次命令保留指定的原图区间；两侧删除先在候选几何上全部验证。
    pub(crate) fn keep_long_clip_range(&mut self, range: Range<u32>) -> Result<bool> {
        self.change_long_clip(|clip| {
            let (width, height) = clip.source_size();
            let length = match clip.axis() {
                ScrollAxis::Vertical => height,
                ScrollAxis::Horizontal => width,
            };
            if range.start >= range.end || range.end > length {
                return Err("仅保留的长图区间为空或超出原图".into());
            }
            let mut changed = false;
            if range.start > 0 {
                changed |= clip.delete(0..range.start)?;
            }
            if range.end < length {
                changed |= clip.delete(range.end..length)?;
            }
            Ok(changed)
        })
    }

    pub(crate) fn restore_long_clip_range(&mut self, index: usize) -> Result<bool> {
        self.change_long_clip(|clip| Ok(clip.restore(index)))
    }

    pub(crate) fn adjust_long_clip_range(
        &mut self,
        index: usize,
        range: Range<u32>,
    ) -> Result<bool> {
        self.change_long_clip(|clip| clip.adjust(index, range))
    }

    pub(crate) fn set_long_clip_cross(&mut self, range: Range<u32>) -> Result<bool> {
        self.change_long_clip(|clip| clip.set_cross(range))
    }

    /// 更新四边裁剪，保留严格位于内部的删除记录；允许向外恢复首尾。
    pub(crate) fn trim_long_clip(
        &mut self,
        long_keep: Range<u32>,
        cross_keep: Range<u32>,
    ) -> Result<bool> {
        self.change_long_clip(|clip| clip.trim(long_keep, cross_keep))
    }

    /// 恢复所有删段与裁边，保留长图身份和原始坐标。
    pub(crate) fn clear_long_clip(&mut self) -> Result<bool> {
        self.change_long_clip(|clip| Ok(clip.clear()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        geometry::Rect,
    };

    fn editor() -> Editor {
        let canvas = Rect::full(120, 600);
        let mut editor = Editor::new(canvas, Some(canvas));
        editor
            .initialize_long_clip(LongClip::new(120, 600, ScrollAxis::Vertical).unwrap())
            .unwrap();
        editor
    }

    fn mark() -> Annotation {
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 20., y: 330. }, Style::default());
        object.update(Point { x: 80., y: 410. });
        object
    }

    #[test]
    fn editor_long_clip_commands_keep_canvas_objects_and_selection_in_original_coordinates() {
        let mut editor = editor();
        let canvas = editor.canvas;
        let crop = editor.crop;
        let annotation = mark();
        let id = editor.document.insert(annotation.clone());
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        editor.selection = vec![id];
        let selection = editor.selection.clone();
        editor.delete_long_clip_range(100..180).unwrap();
        editor.trim_long_clip(20..550, 10..110).unwrap();
        assert_eq!(editor.canvas, canvas);
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.get(id), Some(&annotation));
        assert_eq!(editor.selection, selection);
        assert_eq!(editor.selected, Some(id));
        assert_eq!(editor.tool, Tool::Edit);
        assert_eq!(editor.long_clip().unwrap().output_size(), (100, 450));
    }

    #[test]
    fn editor_long_clip_keep_restore_adjust_clear_each_commit_once() {
        let mut editor = editor();
        let start = editor.document.edit_revision();
        editor.keep_long_clip_range(100..500).unwrap();
        assert_eq!(editor.document.edit_revision(), start + 1);
        assert_eq!(editor.long_clip().unwrap().removed(), &[0..100, 500..600]);
        editor.adjust_long_clip_range(0, 0..80).unwrap();
        assert_eq!(editor.document.edit_revision(), start + 2);
        editor.restore_long_clip_range(0).unwrap();
        assert_eq!(editor.document.edit_revision(), start + 3);
        editor.clear_long_clip().unwrap();
        assert_eq!(editor.document.edit_revision(), start + 4);
        assert!(editor.long_clip().unwrap().is_identity());
        assert!(!editor.clear_long_clip().unwrap());
        assert_eq!(editor.document.edit_revision(), start + 4);
    }

    #[test]
    fn editor_long_clip_failures_and_cancelled_drawing_preserve_history() {
        let mut editor = editor();
        editor.delete_long_clip_range(100..200).unwrap();
        editor.undo();
        let before = editor.document.exact_history_snapshot().unwrap();
        let revisions = (editor.document.revision(), editor.document.edit_revision());
        assert!(editor.delete_long_clip_range(0..600).is_err());
        assert!(editor.delete_long_clip_range(500..601).is_err());
        assert!(editor.keep_long_clip_range(50..50).is_err());
        assert!(editor.set_long_clip_cross(120..121).is_err());
        assert!(editor.adjust_long_clip_range(7, 30..40).is_err());
        assert!(!editor.restore_long_clip_range(7).unwrap());
        assert!(!editor.delete_long_clip_range(100..100).unwrap());
        assert_eq!(editor.document.exact_history_snapshot().unwrap(), before);
        assert_eq!(
            (editor.document.revision(), editor.document.edit_revision()),
            revisions
        );
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((20, 20), 2.);
        assert!(editor.delete_long_clip_range(100..200).is_err());
        editor.cancel_gesture();
        assert_eq!(editor.document.exact_history_snapshot().unwrap(), before);
        assert!(editor.document.can_redo());
    }

    #[test]
    fn editor_long_clip_mixed_undo_redo_retains_geometric_and_object_steps() {
        let mut editor = editor();
        editor.delete_long_clip_range(100..150).unwrap();
        let annotation = mark();
        let id = editor.document.insert(annotation.clone());
        editor.delete_long_clip_range(400..440).unwrap();
        let final_clip = editor.long_clip().unwrap().clone();
        editor.undo();
        assert_eq!(editor.long_clip().unwrap().removed(), &[100..150]);
        assert_eq!(editor.document.get(id), Some(&annotation));
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.undo();
        assert!(editor.long_clip().unwrap().is_identity());
        editor.redo();
        editor.redo();
        editor.redo();
        assert_eq!(editor.long_clip(), Some(&final_clip));
        assert_eq!(editor.document.get(id), Some(&annotation));
    }

    #[test]
    fn editor_long_clip_pin_conversions_and_restore_cannot_silently_drop_geometry() {
        use crate::{pin_raster_document::WireRasterDocument, pin_text_document::WireTextDocument};
        let mut source = editor();
        assert!(WireRasterDocument::from_editor(&source, (120, 600)).is_err());
        assert!(WireTextDocument::try_from_text_editor(&source, (120, 600), false).is_err());
        assert!(WireRasterDocument::from_vector_text_editor(&source, (120, 600), false).is_err());
        source.set_long_clip(None).unwrap();
        assert!(source.long_clip().is_none());
        assert!(WireRasterDocument::from_editor(&source, (120, 600)).is_err());
        assert!(WireTextDocument::try_from_text_editor(&source, (120, 600), false).is_err());

        let canvas = Rect::full(120, 600);
        let mut ordinary = Editor::new(canvas, Some(canvas));
        ordinary.document.insert(mark());
        let wire = WireRasterDocument::from_editor(&ordinary, (120, 600)).unwrap();
        let mut target = editor();
        let before = target.document.exact_history_snapshot().unwrap();
        assert!(wire.restore_into(&mut target).is_err());
        assert_eq!(target.document.exact_history_snapshot().unwrap(), before);
    }
}
