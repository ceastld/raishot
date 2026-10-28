//! 当前自动预览的中键贴图 / Enter 复制资格；矩形只作为输出参数，不提交截图选区。
use crate::{editor::Editor, geometry::Rect};

pub(crate) fn physical_size_matches(canvas: Rect, viewport: (f32, f32), scale: f32) -> bool {
    let width = f64::from(viewport.0) * f64::from(scale);
    let height = f64::from(viewport.1) * f64::from(scale);
    scale > 0.
        && width.is_finite()
        && height.is_finite()
        && width.round() == f64::from(canvas.width)
        && height.round() == f64::from(canvas.height)
}

/// GPUI paint 保存的待提交范围和上次已提交范围；最多两个 Rect，不把 draw 冒作 Present。
#[derive(Clone, Copy)]
pub(crate) struct PaintedPreview {
    pub(crate) rect: Option<Rect>,
    pub(crate) previous_presented: Option<Rect>,
    pub(crate) before_present: usize,
}

impl PaintedPreview {
    pub(crate) fn presented(self, canvas: Rect, serial: usize, size: u32) -> Option<Rect> {
        if serial == 0
            || canvas.width > u16::MAX as u32
            || canvas.height > u16::MAX as u32
            || size != ((canvas.width << 16) | canvas.height)
        {
            return None;
        }
        // dirty key dispatch 会 draw 尚未 Present 的新 scene；保留旧实际提交范围。
        // 新 scene 已成功提交时，即使 rect=None，也不能回退到旧框。
        if serial == self.before_present {
            self.previous_presented
        } else {
            self.rect
        }
        .filter(|rect| rect.valid(canvas.width, canvas.height))
    }
}

/// Enter 固定当前已显示范围；键消息没有鼠标坐标，也没有显示器兜底。
pub(crate) fn confirm_candidate(
    editor: &Editor,
    canvas: Rect,
    preview: Option<Rect>,
    interaction_busy: bool,
    input_active: bool,
) -> Option<Rect> {
    if !crate::image_editor::screenshot_preview_pin_idle(editor, interaction_busy, input_active) {
        return None;
    }
    preview.filter(|rect| rect.valid(canvas.width, canvas.height))
}

/// 中键贴当前真实呈现的 Preview；点仅属于输入元数据，不重新命中或改选范围。
pub(crate) fn candidate(
    editor: &Editor,
    canvas: Rect,
    _point: (i32, i32),
    preview: Option<Rect>,
    interaction_busy: bool,
    input_active: bool,
) -> Option<Rect> {
    if !crate::image_editor::screenshot_preview_pin_idle(editor, interaction_busy, input_active) {
        return None;
    }
    preview.filter(|rect| rect.valid(canvas.width, canvas.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        crop_mask::Polygon,
        scene::{PixelPlane, Scene, SourcePixels},
    };

    fn editor() -> Editor {
        Editor::new(Rect::full(400, 300), None)
    }

    fn preview() -> Rect {
        Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        }
    }

    fn at(editor: &Editor, point: (i32, i32), preview: Option<Rect>) -> Option<Rect> {
        candidate(editor, editor.canvas, point, preview, false, false)
    }

    #[test]
    fn enter_preview_keeps_submitted_range_until_the_new_frame_is_presented() {
        let canvas = editor().canvas;
        assert!(physical_size_matches(canvas, (320., 240.), 1.25));
        for (viewport, scale) in [
            ((321., 240.), 1.25),
            ((320., 241.), 1.25),
            ((320., 240.), 0.),
            ((320., 240.), f32::NAN),
            ((f32::INFINITY, 240.), 1.25),
        ] {
            assert!(!physical_size_matches(canvas, viewport, scale));
        }
        let painted = PaintedPreview {
            rect: Some(preview()),
            previous_presented: None,
            before_present: 8,
        };
        let size = (canvas.width << 16) | canvas.height;
        assert_eq!(painted.presented(canvas, 0, size), None);
        assert_eq!(painted.presented(canvas, 8, size), None);
        assert_eq!(painted.presented(canvas, 9, size + 1), None);
        assert_eq!(painted.presented(canvas, 9, size), Some(preview()));
        let next_rect = Rect {
            x: 200,
            y: 30,
            width: 100,
            height: 120,
        };
        // 按键前 dirty draw 未提交时仍确认旧已提交范围；提交后使用新范围。
        let next = PaintedPreview {
            rect: Some(next_rect),
            previous_presented: painted.presented(canvas, 9, size),
            before_present: 9,
        };
        assert_eq!(next.presented(canvas, 9, size), Some(preview()));
        assert_eq!(next.presented(canvas, 10, size), Some(next_rect));
        // 无范围的新 scene 未提交前保留已显示框，提交后必须清空而不回退。
        let empty = PaintedPreview {
            rect: None,
            previous_presented: next.presented(canvas, 10, size),
            before_present: 10,
        };
        assert_eq!(empty.presented(canvas, 10, size), Some(next_rect));
        assert_eq!(empty.presented(canvas, 11, size), None);
        let redrawn_empty = PaintedPreview {
            previous_presented: empty.presented(canvas, 11, size),
            before_present: 11,
            ..empty
        };
        assert_eq!(redrawn_empty.presented(canvas, 11, size), None);
        let after_empty = PaintedPreview {
            rect: Some(preview()),
            previous_presented: redrawn_empty.presented(canvas, 11, size),
            before_present: 11,
        };
        assert_eq!(after_empty.presented(canvas, 11, size), None);
        assert_eq!(after_empty.presented(canvas, 12, size), Some(preview()));
    }

    #[test]
    fn enter_preview_has_no_fallback_or_crop_commit_and_rejects_busy_drafts() {
        let mut editor = editor();
        let canvas = editor.canvas;
        assert_eq!(confirm_candidate(&editor, canvas, None, false, false), None);
        assert_eq!(
            confirm_candidate(&editor, canvas, Some(preview()), false, false),
            Some(preview())
        );
        assert_eq!(editor.crop, None);
        assert_eq!(editor.tool, Tool::Crop);
        assert!(!editor.document.can_undo());
        assert!(!editor.document.can_redo());
        for (busy, input) in [(true, false), (false, true)] {
            assert_eq!(
                confirm_candidate(&editor, canvas, Some(preview()), busy, input),
                None
            );
        }
        let invalid = Rect {
            x: u32::MAX,
            ..preview()
        };
        assert_eq!(
            confirm_candidate(&editor, canvas, Some(invalid), false, false),
            None
        );
        editor.crop = Some(preview());
        assert_eq!(
            confirm_candidate(&editor, canvas, Some(preview()), false, false),
            None
        );
        editor.crop = Some(preview());
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((140, 120), 4.);
        editor.pointer_up((140, 120));
        editor.crop = None;
        assert!(editor.has_pending_drawing());
        assert_eq!(
            confirm_candidate(&editor, canvas, Some(preview()), false, false),
            None
        );
        assert!(editor.has_pending_drawing());
    }

    #[test]
    fn accepts_the_current_parent_child_or_window_rect_without_confirming_crop() {
        let editor = editor();
        let child = Rect {
            x: 120,
            y: 100,
            width: 50,
            height: 40,
        };
        let window = Rect {
            x: 70,
            y: 60,
            width: 260,
            height: 190,
        };
        for rect in [preview(), child, window] {
            assert_eq!(at(&editor, (140, 120), Some(rect)), Some(rect));
        }
        assert_eq!(editor.crop, None);
        assert_eq!(editor.tool, Tool::Crop);
        assert!(!editor.document.can_undo());
        assert!(!editor.document.can_redo());
    }

    #[test]
    fn missing_preview_has_no_fallback_and_middle_keeps_shown_range_outside_event_point() {
        let editor = editor();
        assert_eq!(at(&editor, (140, 120), None), None);
        assert_eq!(at(&editor, (140, 120), Some(preview())), Some(preview()));
        // 中键贴图不重新命中；旧点包含门被“贴事件当时已显示范围”合同替代。
        for point in [(99, 120), (140, 79), (220, 120), (140, 180), (-1, 120)] {
            assert_eq!(at(&editor, point, Some(preview())), Some(preview()));
            assert_eq!(at(&editor, point, None), None);
        }
        for point in [(100, 80), (219, 179)] {
            assert_eq!(at(&editor, point, Some(preview())), Some(preview()));
        }
    }

    #[test]
    fn rejects_empty_outside_and_overflow_preview_without_pointer_fallback() {
        let editor = editor();
        for rect in [
            Rect {
                width: 0,
                ..preview()
            },
            Rect {
                height: 0,
                ..preview()
            },
            Rect {
                x: 390,
                ..preview()
            },
            Rect {
                y: 290,
                ..preview()
            },
            Rect {
                x: u32::MAX,
                width: 2,
                ..preview()
            },
            Rect {
                y: u32::MAX,
                height: 2,
                ..preview()
            },
        ] {
            assert_eq!(at(&editor, (140, 120), Some(rect)), None);
        }
        assert_eq!(
            candidate(
                &editor,
                Rect::full(200, 150),
                (140, 120),
                Some(preview()),
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn busy_input_and_confirmed_crop_do_not_start_preview_output() {
        let mut editor = editor();
        for (busy, input) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                candidate(
                    &editor,
                    editor.canvas,
                    (140, 120),
                    Some(preview()),
                    busy,
                    input
                ),
                None
            );
        }
        editor.crop = Some(preview());
        assert_eq!(at(&editor, (140, 120), Some(preview())), None);
        assert!(crate::image_editor::screenshot_middle_pin_ready(
            &editor,
            editor.canvas,
            false,
            false,
        ));
        assert_eq!(editor.crop, Some(preview()));
    }

    #[test]
    fn refuses_active_drag_and_unfinished_polyline_arrow_or_text_without_committing() {
        let mut dragging = editor();
        dragging.pointer_down((110, 90), 4.);
        dragging.pointer_move((140, 120));
        assert!(dragging.is_dragging());
        assert!(!crate::image_editor::screenshot_middle_pin_idle(
            &dragging, false, false
        ));
        assert_eq!(at(&dragging, (140, 120), Some(preview())), None);
        assert!(dragging.is_dragging());
        assert!(!dragging.document.can_undo());

        for tool in [Tool::Polyline, Tool::Arrow] {
            let mut draft = Editor::new(Rect::full(400, 300), Some(preview()));
            draft.select_tool(tool);
            draft.pointer_down((140, 120), 4.);
            draft.pointer_up((140, 120));
            assert!(draft.has_pending_drawing());
            assert!(!draft.is_dragging());
            // 隔离待绘图门：不调用会取消草稿的 set_crop。
            draft.crop = None;
            assert_eq!(at(&draft, (140, 120), Some(preview())), None);
            assert!(draft.has_pending_drawing());
            assert_eq!(draft.tool, tool);
            assert_eq!(draft.crop, None);
            assert!(!draft.document.can_undo());
        }

        let mut text = editor();
        text.select_tool(Tool::Text);
        text.begin_text(None, Point { x: 140., y: 120. });
        text.set_text("未提交".into());
        assert_eq!(at(&text, (140, 120), Some(preview())), None);
        assert_eq!(text.text_draft.as_ref().unwrap().annotation.text, "未提交");
        assert_eq!(text.crop, None);
        assert!(!text.document.can_undo());
    }

    #[test]
    fn refuses_lasso_and_watermark_preedit_without_applying_or_cancelling_them() {
        let mut lasso = editor();
        lasso.select_tool(Tool::Lasso);
        lasso.lasso_draft = Polygon::new(&[
            Point { x: 110., y: 90. },
            Point { x: 180., y: 90. },
            Point { x: 180., y: 150. },
            Point { x: 110., y: 150. },
        ]);
        let before = lasso.lasso_draft.clone();
        assert!(before.is_some());
        assert_eq!(at(&lasso, (140, 120), Some(preview())), None);
        assert_eq!(lasso.lasso_draft, before);
        assert_eq!(lasso.crop, None);
        assert!(!lasso.document.can_undo());

        let mut watermark = editor();
        watermark.select_tool(Tool::Watermark);
        let binding = watermark.watermark.binding();
        watermark.preview_watermark_text(binding, "水印预编辑", true);
        assert!(!watermark.has_pending_drawing());
        assert!(watermark.watermark.has_preview());
        assert_eq!(at(&watermark, (140, 120), Some(preview())), None);
        assert!(watermark.watermark.has_preview());
        assert_eq!(watermark.watermark.display_text(), "水印预编辑");
        assert_eq!(watermark.crop, None);
        assert!(!watermark.document.can_undo());
    }

    fn rectangle(start: Point, end: Point) -> Annotation {
        let mut annotation = Annotation::new(
            Tool::Rectangle,
            start,
            Style {
                color: 0xff0000,
                filled: true,
                ..Default::default()
            },
        );
        annotation.update(end);
        annotation
    }

    #[test]
    fn candidate_keeps_tool_selection_objects_and_undo_redo_at_full_canvas_coordinates() {
        let mut editor = editor();
        let annotation = rectangle(Point { x: 110., y: 90. }, Point { x: 180., y: 150. });
        let id = editor.document.insert(annotation.clone());
        editor.document.insert(rectangle(
            Point { x: 250., y: 200. },
            Point { x: 300., y: 240. },
        ));
        editor.document.undo();
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        editor.selection = vec![id];
        let revision = editor.document.revision();
        assert_eq!(at(&editor, (140, 120), Some(preview())), Some(preview()));
        assert_eq!(at(&editor, (350, 250), Some(preview())), Some(preview()));
        assert_eq!(editor.crop, None);
        assert_eq!(editor.tool, Tool::Edit);
        assert_eq!(editor.selected, Some(id));
        assert_eq!(editor.selection, vec![id]);
        assert_eq!(editor.document.get(id), Some(&annotation));
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.document.can_undo());
        assert!(editor.document.can_redo());
    }

    #[test]
    fn preview_range_only_crops_scene_output_without_moving_annotations() {
        let canvas = Rect::full(40, 30);
        let mut editor = Editor::new(canvas, None);
        let annotation = rectangle(Point { x: 18., y: 9. }, Point { x: 28., y: 16. });
        let id = editor.document.insert(annotation.clone());
        let preview = Rect {
            x: 12,
            y: 7,
            width: 20,
            height: 16,
        };
        let selected = candidate(&editor, canvas, (20, 10), Some(preview), false, false).unwrap();
        let pixels = vec![255; (canvas.width * canvas.height * 4) as usize];
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &pixels,
            }],
            overlay: None,
        };
        let scene = Scene::prepare(&editor.document_annotations(), &source);
        let mut full = source.read(canvas);
        let mut cropped = source.read(selected);
        scene.try_apply(&mut full, canvas).unwrap();
        scene.try_apply(&mut cropped, selected).unwrap();
        for y in 0..selected.height {
            for x in 0..selected.width {
                let src = (((y + selected.y) * canvas.width + x + selected.x) * 4) as usize;
                let dst = ((y * selected.width + x) * 4) as usize;
                assert_eq!(&cropped.bgra[dst..dst + 4], &full.bgra[src..src + 4]);
            }
        }
        let red = ((12 * canvas.width + 22) * 4) as usize;
        assert_eq!(&full.bgra[red..red + 4], &[0, 0, 255, 255]);
        assert_eq!(editor.crop, None);
        assert_eq!(editor.document.get(id), Some(&annotation));
        assert!(editor.document.can_undo());
    }
}
