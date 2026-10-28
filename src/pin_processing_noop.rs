//! 处理入口的只读无变化门；不提交草稿、不建边界、不取消请求、不改任何历史。
use super::PinAction;
use crate::{editor::Editor, pin_model::Adjustments};

/// 只在饱和亮度需要区分下一批时读取。OnlyRedo仍是真实文档；这里判断内容边界，非文档存在性。
pub(crate) fn requires_content_boundary(
    editor: Option<&Editor>,
    vector_text: bool,
    capture_domain: bool,
) -> bool {
    vector_text
        || capture_domain
        || editor.is_some_and(|editor| {
            editor.document.can_undo()
                || !editor.document.items().is_empty()
                || editor.document.crop_mask.is_some()
                || editor.text_draft.is_some()
                || editor.lasso_draft.is_some()
                || editor.has_pending_drawing()
                || editor.is_dragging()
                || editor.watermark.has_preview()
        })
}

pub(crate) fn should_skip(
    action: PinAction,
    current: Adjustments,
    requires_boundary: impl FnOnce() -> bool,
) -> bool {
    match action {
        PinAction::Rotate(delta) => delta.rem_euclid(4) == 0,
        PinAction::Brightness(0) => true,
        PinAction::Brightness(delta) => {
            let next = (i32::from(current.brightness) + i32::from(delta)).clamp(-100, 100) as i16;
            // 只有仍在同一批时canonical参数相等才无变化；新Doc/文字/来源域必须进入下一批。
            next == current.brightness && !requires_boundary()
        }
        // Reset只恢复本批参数；default不能被借作Clear、seal或丢历史。
        PinAction::ResetImage => current == Adjustments::default(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        editor::TextDraft,
        geometry::Rect,
        pin_model::ProcessingHistory,
    };
    fn annotation() -> Annotation {
        let mut a = Annotation::new(Tool::Rectangle, Point { x: 4., y: 5. }, Style::default());
        a.update(Point { x: 25., y: 20. });
        a
    }
    fn document() -> Editor {
        let mut editor = Editor::new(
            Rect::full(80, 60),
            Some(Rect {
                x: 7,
                y: 8,
                width: 53,
                height: 39,
            }),
        );
        let id = editor.document.insert(annotation());
        let mut changed = editor.document.get(id).unwrap().clone();
        changed.style.color = 0x123456;
        editor.document.replace(id, changed);
        editor.document.undo();
        assert!(editor.document.can_undo() && editor.document.can_redo());
        editor
    }
    fn parameters() -> (Adjustments, ProcessingHistory) {
        let mut current = Adjustments::default();
        let mut history = ProcessingHistory::default();
        assert!(history.change(
            &mut current,
            Adjustments {
                grayscale: true,
                ..Default::default()
            }
        ));
        assert!(history.change(&mut current, Adjustments::default()));
        assert!(history.change(
            &mut current,
            Adjustments {
                brightness: 10,
                ..Default::default()
            }
        ));
        assert!(history.undo(&mut current));
        assert!(history.can_undo() && history.can_redo());
        (current, history)
    }
    #[test]
    fn identity_processing_gate_keeps_live_document_both_stacks_draft_crop_and_parameters() {
        let mut editor = document();
        let mut draft = Annotation::new(Tool::Text, Point { x: 12., y: 13. }, Style::default());
        draft.text = "尚未提交的草稿".into();
        editor.text_draft = Some(TextDraft {
            id: None,
            annotation: draft,
        });
        let doc = editor.document.bounded_history_snapshot().unwrap();
        let text = editor.text_draft.as_ref().unwrap().annotation.clone();
        let crop = editor.crop;
        let (current, history) = parameters();
        let params = history.bounded_snapshot(current).unwrap();
        for action in [
            PinAction::Rotate(0),
            PinAction::Rotate(4),
            PinAction::Rotate(-4),
            PinAction::Rotate(i8::MIN),
            PinAction::Brightness(0),
            PinAction::ResetImage,
        ] {
            assert!(should_skip(action, current, || panic!(
                "identity不读取或推进内容边界"
            )));
            assert_eq!(editor.document.bounded_history_snapshot().unwrap(), doc);
            assert_eq!(editor.text_draft.as_ref().unwrap().annotation, text);
            assert_eq!(editor.crop, crop);
            assert_eq!(history.bounded_snapshot(current).unwrap(), params);
        }
        assert!(requires_content_boundary(Some(&editor), false, false));
        assert!(!should_skip(PinAction::Rotate(1), current, || panic!(
            "真实旋转无需亮度边界查询"
        )));
        assert!(!should_skip(
            PinAction::ResetImage,
            Adjustments {
                brightness: 10,
                ..current
            },
            || panic!("非default reset不借边界变成identity")
        ));
    }
    #[test]
    fn saturated_brightness_without_new_boundary_preserves_only_redo_and_processing_redo() {
        let mut editor = Editor::new(Rect::full(80, 60), None);
        editor.document.insert(annotation());
        editor.document.undo();
        assert!(
            editor.document.items().is_empty()
                && !editor.document.can_undo()
                && editor.document.can_redo()
        );
        let doc = editor.document.bounded_history_snapshot().unwrap();
        let mut current = Adjustments::default();
        let mut history = ProcessingHistory::default();
        history.change(
            &mut current,
            Adjustments {
                brightness: 100,
                ..Default::default()
            },
        );
        history.change(
            &mut current,
            Adjustments {
                brightness: 80,
                ..Default::default()
            },
        );
        history.undo(&mut current);
        let params = history.bounded_snapshot(current).unwrap();
        assert_eq!(current.brightness, 100);
        assert!(history.can_undo() && history.can_redo());
        assert!(!requires_content_boundary(Some(&editor), false, false));
        assert!(should_skip(PinAction::Brightness(10), current, || {
            requires_content_boundary(Some(&editor), false, false)
        }));
        assert!(should_skip(
            PinAction::Brightness(-10),
            Adjustments {
                brightness: -100,
                ..current
            },
            || requires_content_boundary(Some(&editor), false, false)
        ));
        assert!(!should_skip(
            PinAction::Brightness(-10),
            current,
            || panic!("改变参数直接保留原实际处理路径")
        ));
        assert_eq!(editor.document.bounded_history_snapshot().unwrap(), doc);
        assert_eq!(history.bounded_snapshot(current).unwrap(), params);
    }
    #[test]
    fn new_document_text_or_capture_domain_keeps_saturated_brightness_as_a_new_content_batch() {
        let editor = document();
        let current = Adjustments {
            brightness: 100,
            ..Default::default()
        };
        for boundary in [
            requires_content_boundary(Some(&editor), false, false),
            requires_content_boundary(None, true, false),
            requires_content_boundary(None, false, true),
        ] {
            assert!(boundary);
            assert!(!should_skip(PinAction::Brightness(10), current, || {
                boundary
            }));
            // 真实旧seal进入新批后参数从default起算，不能把同一批的+100当作这一批已+100。
            let base = Adjustments::default();
            let next = Adjustments {
                brightness: 10,
                ..base
            };
            let mut actual = base;
            let mut history = ProcessingHistory::default();
            assert!(history.change(&mut actual, next));
            assert_eq!(actual.brightness, 10);
            assert!(should_skip(PinAction::Brightness(0), current, || panic!(
                "零增量不创建内容边界"
            )));
            assert!(should_skip(
                PinAction::ResetImage,
                Adjustments::default(),
                || panic!("默认Reset不删除文档或建立新边界")
            ));
        }
    }
}
