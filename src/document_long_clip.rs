//! 长图剪辑只保存几何，沿用对象文档的增量命令和同一条撤销历史。
use super::{BoundedHistorySnapshot, Change, Document};
use crate::{long_clip::LongClip, Result};

fn contains_clip(change: &Change) -> bool {
    match change {
        Change::LongClip { .. } => true,
        Change::Batch(children) => children.iter().any(contains_clip),
        _ => false,
    }
}

impl Document {
    pub(crate) fn long_clip(&self) -> Option<&LongClip> {
        self.long_clip.as_ref()
    }

    /// 仅用于新打开的长图；不能用初始化绕过已有文档的撤销与修订。
    pub(crate) fn initialize_long_clip(&mut self, clip: LongClip) -> Result<bool> {
        clip.validate()?;
        if self.long_clip.as_ref() == Some(&clip) {
            return Ok(false);
        }
        if self.long_clip.is_some()
            || !self.items.is_empty()
            || self.crop_mask.is_some()
            || !self.undo.is_empty()
            || !self.redo.is_empty()
            || self.revision != 0
            || self.edit_revision != 0
        {
            return Err("已有内容或历史的文档不能重新初始化长图剪辑".into());
        }
        self.long_clip = Some(clip);
        self.revision += 1;
        Ok(true)
    }

    pub(crate) fn set_long_clip(&mut self, after: Option<LongClip>) -> Result<bool> {
        Self::validate_long_clip_pair(self.long_clip.as_ref(), after.as_ref())?;
        if self.long_clip == after {
            return Ok(false);
        }
        self.commit(Change::LongClip {
            before: self.long_clip.clone(),
            after,
        });
        Ok(true)
    }

    pub(super) fn validate_long_clip_pair(
        before: Option<&LongClip>,
        after: Option<&LongClip>,
    ) -> Result<()> {
        for clip in [before, after].into_iter().flatten() {
            clip.validate()?;
        }
        if let (Some(before), Some(after)) = (before, after) {
            if before.source_size() != after.source_size() || before.axis() != after.axis() {
                return Err("长图剪辑不能在编辑命令中更换原图尺寸或方向".into());
            }
        }
        Ok(())
    }

    pub(super) fn long_clip_payloads(clip: Option<&LongClip>) -> usize {
        clip.map_or(0, |clip| clip.removed().len().saturating_add(1))
    }

    /// 旧贴图磁盘格式暂未保存剪辑；显式拒绝当前态和双栈，不能只丢掉几何。
    pub(crate) fn ensure_no_long_clip(&self) -> Result<()> {
        if self.long_clip.is_some() || self.undo.iter().chain(&self.redo).any(contains_clip) {
            return Err("当前持久化格式尚不支持长图剪辑，请先导出图片".into());
        }
        Ok(())
    }
}

impl BoundedHistorySnapshot {
    pub(crate) fn ensure_no_long_clip(&self) -> Result<()> {
        if self.long_clip.is_some() || self.undo.iter().chain(&self.redo).any(contains_clip) {
            return Err("当前持久化格式尚不支持长图剪辑历史".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        stitch::ScrollAxis,
    };

    fn clip() -> LongClip {
        LongClip::new(120, 600, ScrollAxis::Vertical).unwrap()
    }

    fn rectangle() -> Annotation {
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 20., y: 330. }, Style::default());
        object.update(Point { x: 80., y: 410. });
        object
    }

    #[test]
    fn document_long_clip_and_annotations_share_exact_ordered_history() {
        let mut document = Document::default();
        document.initialize_long_clip(clip()).unwrap();
        assert!(!document.can_undo());
        assert_eq!(document.edit_revision(), 0);
        let mut first = clip();
        first.delete(100..160).unwrap();
        document.set_long_clip(Some(first.clone())).unwrap();
        let annotation = rectangle();
        let id = document.insert(annotation.clone());
        let mut second = first.clone();
        second.delete(400..450).unwrap();
        document.set_long_clip(Some(second.clone())).unwrap();
        let saved = document.exact_history_snapshot().unwrap();
        assert_eq!(saved.undo.len(), 3);
        let mut restored = Document::from_bounded_history_snapshot(saved.clone()).unwrap();
        for target in [&mut document, &mut restored] {
            target.undo();
            assert_eq!(target.long_clip(), Some(&first));
            assert_eq!(target.get(id), Some(&annotation));
            target.undo();
            assert!(target.items().is_empty());
            assert_eq!(target.long_clip(), Some(&first));
            target.undo();
            assert_eq!(target.long_clip(), Some(&clip()));
            assert!(!target.can_undo());
            target.redo();
            target.redo();
            target.redo();
            assert_eq!(target.long_clip(), Some(&second));
            assert_eq!(target.get(id), Some(&annotation));
            assert_eq!(target.exact_history_snapshot().unwrap(), saved);
        }
    }

    #[test]
    fn document_long_clip_noop_and_invalid_source_leave_history_and_revision_unchanged() {
        let mut document = Document::default();
        document.initialize_long_clip(clip()).unwrap();
        let mut changed = clip();
        changed.delete(100..180).unwrap();
        document.set_long_clip(Some(changed)).unwrap();
        document.undo();
        let saved = document.exact_history_snapshot().unwrap();
        let revision = (document.revision(), document.edit_revision());
        assert!(!document.set_long_clip(Some(clip())).unwrap());
        assert!(document
            .set_long_clip(Some(LongClip::new(121, 600, ScrollAxis::Vertical).unwrap()))
            .is_err());
        assert!(document
            .initialize_long_clip(LongClip::new(120, 600, ScrollAxis::Horizontal).unwrap())
            .is_err());
        assert_eq!(document.exact_history_snapshot().unwrap(), saved);
        assert_eq!((document.revision(), document.edit_revision()), revision);
        assert!(document.can_redo());
    }

    #[test]
    fn document_long_clip_snapshot_rejects_discontinuous_geometry() {
        let mut document = Document::default();
        document.initialize_long_clip(clip()).unwrap();
        let mut changed = clip();
        changed.delete(100..180).unwrap();
        document.set_long_clip(Some(changed)).unwrap();
        let mut snapshot = document.exact_history_snapshot().unwrap();
        snapshot.long_clip = Some(clip());
        assert!(Document::from_bounded_history_snapshot(snapshot).is_err());
    }

    #[test]
    fn document_long_clip_legacy_format_gate_rejects_current_and_redo_even_after_detach() {
        let mut document = Document::default();
        document.initialize_long_clip(clip()).unwrap();
        assert!(document.ensure_no_long_clip().is_err());
        document.set_long_clip(None).unwrap();
        assert!(document.ensure_no_long_clip().is_err());
        assert!(document
            .exact_history_snapshot()
            .unwrap()
            .ensure_no_long_clip()
            .is_err());
        document.undo();
        assert!(document.ensure_no_long_clip().is_err());
        assert!(Document::default().ensure_no_long_clip().is_ok());
    }
}
