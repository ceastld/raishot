//! 普通截图 Edit 空对象方向键回退；标注仍使用冻结画布的全屏坐标。
use crate::{annotation::Tool, editor::Editor};
use gpui::Modifiers;

/// true 表示本方向键属于选区路线，到画布边界无变化时也不转成对象移动。
pub(super) fn move_empty_edit_crop(
    editor: &mut Editor,
    delta: (i32, i32),
    modifiers: Modifiers,
    ordinary: bool,
    busy: bool,
    input_active: bool,
) -> bool {
    if !ordinary
        || busy
        || input_active
        || editor.tool != Tool::Edit
        || modifiers.control
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
        || !editor.selected_ids().is_empty()
        || editor.is_dragging()
        || editor.has_pending_drawing()
        || editor.text_draft.is_some()
        || editor.lasso_draft.is_some()
        || editor.watermark.has_preview()
    {
        return false;
    }
    let Some(crop) = editor
        .crop
        .filter(|crop| crop.valid(editor.canvas.width, editor.canvas.height))
    else {
        return false;
    };
    let next = crate::geometry::Rect {
        x: (i64::from(crop.x) + i64::from(delta.0))
            .clamp(0, i64::from(editor.canvas.width - crop.width)) as u32,
        y: (i64::from(crop.y) + i64::from(delta.1))
            .clamp(0, i64::from(editor.canvas.height - crop.height)) as u32,
        ..crop
    };
    if next != crop {
        // 与既有 Crop 移动相同，仅撤销失效的裁剪 mask；不切 Edit 或平移标注。
        editor.document.invalidate_crop_mask();
        editor.crop = Some(next);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style},
        crop_mask::Polygon,
        geometry::Rect,
    };

    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(300, 200),
            Some(Rect {
                x: 30,
                y: 20,
                width: 100,
                height: 80,
            }),
        );
        editor.select_tool(Tool::Edit);
        editor
    }
    fn apply(editor: &mut Editor, delta: (i32, i32), shift: bool) -> bool {
        move_empty_edit_crop(
            editor,
            delta,
            Modifiers {
                shift,
                ..Default::default()
            },
            true,
            false,
            false,
        )
    }
    fn mask() -> Polygon {
        Polygon::new(&[
            Point { x: 40., y: 30. },
            Point { x: 90., y: 30. },
            Point { x: 90., y: 65. },
        ])
        .unwrap()
    }
    fn object() -> Annotation {
        let mut item = Annotation::new(Tool::Rectangle, Point { x: 50., y: 40. }, Style::default());
        item.update(Point { x: 90., y: 70. });
        item
    }

    #[test]
    fn empty_edit_moves_only_crop_with_existing_one_and_shift_ten_pixel_steps() {
        let mut editor = editor();
        editor.document.insert(object());
        let items = editor.document.items().to_vec();
        let edit_revision = editor.document.edit_revision();
        let style = editor.style;
        assert!(apply(&mut editor, (1, 0), false));
        assert!(apply(&mut editor, (0, -10), true));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 31,
                y: 10,
                width: 100,
                height: 80
            })
        );
        assert_eq!(editor.tool, Tool::Edit);
        assert_eq!(editor.style, style);
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        let crop = editor.crop;
        editor.document.undo();
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.crop, crop);
        editor.document.redo();
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.crop, crop);
    }

    #[test]
    fn all_canvas_edges_clamp_size_and_no_change_keeps_mask_while_changed_crop_invalidates_it() {
        let mut editor = editor();
        editor.crop = Some(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 80,
        });
        editor.document.set_crop_mask(Some(mask()));
        let original_mask = editor.document.crop_mask.clone();
        assert!(apply(&mut editor, (-10, -10), true));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 80
            }
        );
        assert_eq!(editor.document.crop_mask, original_mask);
        assert!(apply(&mut editor, (i32::MAX, i32::MAX), false));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 200,
                y: 120,
                width: 100,
                height: 80
            }
        );
        assert!(editor.document.crop_mask.is_none());
        assert!(apply(&mut editor, (10, 10), true));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 200,
                y: 120,
                width: 100,
                height: 80
            }
        );
    }

    #[test]
    fn selected_object_retains_existing_nudge_and_undo_without_moving_crop() {
        let mut editor = editor();
        let id = editor.document.insert(object());
        editor.selected = Some(id);
        let before = editor.document.get(id).unwrap().clone();
        let crop = editor.crop;
        if !apply(&mut editor, (1, 0), false) {
            editor.nudge(1., 0.);
        } else {
            panic!("选中对象必须保留原 nudge 路线");
        }
        assert_eq!(editor.crop, crop);
        assert_eq!(
            editor.document.get(id).unwrap().bounds().x,
            before.bounds().x + 1.
        );
        editor.document.undo();
        assert_eq!(editor.document.get(id), Some(&before));
        assert_eq!(editor.crop, crop);
    }

    #[test]
    fn other_hosts_modifiers_input_empty_crop_and_unfinished_gestures_never_take_crop_route() {
        let mut editor = editor();
        let original = editor.crop;
        for modifiers in [
            Modifiers {
                alt: true,
                ..Default::default()
            },
            Modifiers {
                control: true,
                ..Default::default()
            },
            Modifiers {
                platform: true,
                ..Default::default()
            },
            Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            assert!(!move_empty_edit_crop(
                &mut editor,
                (1, 0),
                modifiers,
                true,
                false,
                false
            ));
        }
        for (ordinary, busy, input) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert!(!move_empty_edit_crop(
                &mut editor,
                (1, 0),
                Modifiers::default(),
                ordinary,
                busy,
                input
            ));
        }
        assert_eq!(editor.crop, original);
        assert!(editor.begin_screenshot_crop_outside((10, 50), 3.));
        assert!(!apply(&mut editor, (1, 0), false));
        editor.cancel_gesture();
        editor.lasso_draft = Some(mask());
        assert!(!apply(&mut editor, (1, 0), false));
        assert!(editor.lasso_draft.is_some());
        editor.lasso_draft = None;
        editor.select_tool(Tool::Text);
        editor.pointer_down((50, 45), 3.);
        assert!(editor.text_draft.is_some());
        assert!(!apply(&mut editor, (1, 0), false));
        assert!(editor.text_draft.is_some());
        editor.set_crop(None);
        editor.select_tool(Tool::Edit);
        assert!(!apply(&mut editor, (1, 0), false));
    }
}
