//! 普通截图未确认选区的实时方向键意图；本模块不读取或移动系统鼠标。
use crate::{
    editor::Editor,
    geometry::{Rect, ScreenRect},
};
use gpui::{KeyDownEvent, KeyUpEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}
impl Direction {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "left" | "a" => Some(Self::Left),
            "right" | "d" => Some(Self::Right),
            "up" | "w" => Some(Self::Up),
            "down" | "s" => Some(Self::Down),
            _ => None,
        }
    }
    fn delta(self, step: i32) -> (i32, i32) {
        match self {
            Self::Left => (-step, 0),
            Self::Right => (step, 0),
            Self::Up => (0, -step),
            Self::Down => (0, step),
        }
    }
}

pub(crate) fn is_preview_movement_key(key: &str) -> bool {
    Direction::from_key(key).is_some()
}

/// Shift 只影响预览的其它辅助行为；物理微调保持相同的加速与键周期。
pub(crate) fn preview_movement_modifiers(modifiers: gpui::Modifiers) -> bool {
    !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function
}

/// 仅固定预览来源的重框使用实际鼠标点；其它选区和标注保留原比例约束。
pub(crate) fn preview_crop_pointer(
    editor: &Editor,
    point: (i32, i32),
    shift: bool,
    ordinary: bool,
    manual_source: bool,
) -> (i32, i32) {
    editor.constrained_pointer(
        point,
        shift && !(ordinary && manual_source && editor.is_changing_crop()),
    )
}

/// 调用方已让文字/IME/菜单等先处理；预览的 WASD 不回落到工具/确认动作。
pub(crate) fn claims_preview_wasd(editor: &Editor, ordinary: bool, event: &KeyDownEvent) -> bool {
    ordinary
        && editor.crop.is_none()
        && preview_movement_modifiers(event.keystroke.modifiers)
        && matches!(event.keystroke.key.as_str(), "w" | "a" | "s" | "d")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CropDragArrowRoute {
    Pass,
    Consume,
    Move((i32, i32)),
}

/// 已接受的截图范围拖动认领裸 Arrow；失效或回放不能落回 set_crop 取消手势。
pub(crate) fn crop_drag_arrow_route(
    editor: &Editor,
    ordinary: bool,
    accepted_left: bool,
    event: &KeyDownEvent,
    movement_ready: bool,
    replayed: bool,
) -> CropDragArrowRoute {
    if !ordinary
        || !accepted_left
        || !editor.is_changing_crop()
        || event.keystroke.modifiers.modified()
        || !matches!(
            event.keystroke.key.as_str(),
            "left" | "right" | "up" | "down"
        )
    {
        return CropDragArrowRoute::Pass;
    }
    if !movement_ready || replayed {
        return CropDragArrowRoute::Consume;
    }
    CropDragArrowRoute::Move(Direction::from_key(&event.keystroke.key).unwrap().delta(1))
}

/// 普通截图从无已确认选区接受的 New；不把 Resize/Move 或其它画布算入。
/// 原始 Down 的保存、真实捕获及源身份仍由调用端证明。
pub(crate) fn new_crop_cursor_source(editor: &Editor, ordinary: bool, accepted_left: bool) -> bool {
    ordinary && accepted_left && editor.crop_creation_before() == Some(None)
}

pub(crate) fn new_crop_cursor_ready(
    editor: &Editor,
    ordinary: bool,
    accepted_left: bool,
    owner_ready: bool,
    busy_other: bool,
    input_active: bool,
) -> bool {
    new_crop_cursor_source(editor, ordinary, accepted_left)
        && owner_ready
        && !busy_other
        && !input_active
        && editor.selected_ids().is_empty()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
}

/// 明确的 New 键盘拖动只认合法实际非零读回；失败/边缘 no-op 不伪造运动。
/// 已发生的证据不受随后 owner loss 影响，调用端 OR 到原 sticky 点击状态。
pub(crate) fn new_crop_cursor_readback_moved(
    bounds: ScreenRect,
    before_global: (i32, i32),
    actual_global: (i32, i32),
) -> bool {
    actual_global != before_global
        && [before_global, actual_global]
            .into_iter()
            .all(|point| target_in_client(bounds, point, (0, 0)).is_some())
}

/// 宿主已接受的既有范围调整；New/Lasso 和对象手势不借用此来源。
pub(crate) fn crop_adjust_cursor_source(
    editor: &Editor,
    ordinary: bool,
    accepted_left: bool,
) -> bool {
    ordinary
        && accepted_left
        && editor.is_changing_crop()
        && editor.crop_creation_before().is_none()
}

fn crop_adjust_cursor_inputs_ready(editor: &Editor, before_down: bool) -> bool {
    editor.selected_ids().is_empty()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && (editor.lasso_draft.is_none()
            || editor.tool == crate::annotation::Tool::Lasso
                && (before_down || editor.is_lasso_crop_outside_resize()))
        && !editor.watermark.has_preview()
}

/// 原 Down 前冻结资格，避免后续 finish/清选择把拒绝的入口变为已接受来源。
pub(crate) fn crop_adjust_cursor_admission(
    editor: &Editor,
    ordinary: bool,
    owner_ready: bool,
    busy_other: bool,
    input_active: bool,
) -> bool {
    ordinary
        && owner_ready
        && !busy_other
        && !input_active
        && editor
            .crop
            .is_some_and(|crop| crop.valid(editor.canvas.width, editor.canvas.height))
        && !editor.is_dragging()
        && crop_adjust_cursor_inputs_ready(editor, true)
}

pub(crate) fn crop_adjust_cursor_ready(
    editor: &Editor,
    ordinary: bool,
    accepted_left: bool,
    owner_ready: bool,
    busy_other: bool,
    input_active: bool,
) -> bool {
    crop_adjust_cursor_source(editor, ordinary, accepted_left)
        && owner_ready
        && !busy_other
        && !input_active
        && crop_adjust_cursor_inputs_ready(editor, false)
}

/// 实际成功 Set/Get 的合法非零运动与 New 共用口径，post-owner 不撤历史。
pub(crate) fn crop_adjust_cursor_readback_moved(
    bounds: ScreenRect,
    before_global: (i32, i32),
    actual_global: (i32, i32),
) -> bool {
    new_crop_cursor_readback_moved(bounds, before_global, actual_global)
}

#[cfg(test)]
mod smart_crop_adjust_cursor_tests {
    use super::*;
    use crate::annotation::Tool;

    #[test]
    fn smart_outside_resize_keeps_the_real_adjust_source_and_existing_owner_gates() {
        for down in [
            (40, 40),
            (160, 40),
            (280, 40),
            (40, 130),
            (280, 130),
            (40, 220),
            (160, 220),
            (280, 220),
        ] {
            let crop = Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            };
            let mut editor = Editor::new(Rect::full(400, 300), Some(crop));
            editor.select_tool(Tool::SmartMosaic);
            assert!(crop_adjust_cursor_admission(
                &editor, true, true, false, false
            ));
            assert!(editor.begin_screenshot_crop_outside(down, 5.));
            assert!(crop_adjust_cursor_source(&editor, true, true));
            assert!(!new_crop_cursor_source(&editor, true, true));
            assert!(crop_adjust_cursor_ready(
                &editor, true, true, true, false, false
            ));
            for (ordinary, left, owner, busy, input) in [
                (false, true, true, false, false),
                (true, false, true, false, false),
                (true, true, false, false, false),
                (true, true, true, true, false),
                (true, true, true, false, true),
            ] {
                assert!(!crop_adjust_cursor_ready(
                    &editor, ordinary, left, owner, busy, input
                ));
            }
            editor.cancel_gesture();
            assert_eq!(editor.crop, Some(crop));
            assert!(!crop_adjust_cursor_source(&editor, true, true));
        }
    }
}

#[cfg(test)]
mod lasso_crop_adjust_cursor_tests {
    use super::*;
    use crate::{
        annotation::{Point, Tool},
        crop_mask::Polygon,
        geometry::Rect,
    };
    use gpui::{Keystroke, Modifiers};

    fn key(name: &str, held: bool, shift: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers {
                    shift,
                    ..Default::default()
                },
            },
            is_held: held,
        }
    }
    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.select_tool(Tool::Lasso);
        editor.lasso_draft = Polygon::new(&[
            Point { x: 110., y: 90. },
            Point { x: 190., y: 90. },
            Point { x: 190., y: 160. },
        ]);
        assert!(editor.lasso_draft.is_some());
        editor
    }

    #[test]
    fn closed_draft_admission_requires_real_lasso_outside_resize_and_preserves_other_gates() {
        let mut editor = editor();
        assert!(crop_adjust_cursor_admission(
            &editor, true, true, false, false
        ));
        assert!(!crop_adjust_cursor_ready(
            &editor, true, true, true, false, false
        ));
        for (ordinary, owner, busy, input) in [
            (false, true, false, false),
            (true, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
        ] {
            assert!(!crop_adjust_cursor_admission(
                &editor, ordinary, owner, busy, input
            ));
        }
        let crop = editor.crop;
        let draft = editor.lasso_draft.clone();
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        assert!(editor.is_lasso_crop_outside_resize());
        assert!(crop_adjust_cursor_source(&editor, true, true));
        assert!(!new_crop_cursor_source(&editor, true, true));
        assert!(crop_adjust_cursor_ready(
            &editor, true, true, true, false, false
        ));
        assert!(!crop_adjust_cursor_admission(
            &editor, true, true, false, false
        ));
        for (ordinary, accepted, owner, busy, input) in [
            (false, true, true, false, false),
            (true, false, true, false, false),
            (true, true, false, false, false),
            (true, true, true, true, false),
            (true, true, true, false, true),
        ] {
            assert!(!crop_adjust_cursor_ready(
                &editor, ordinary, accepted, owner, busy, input
            ));
        }
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.lasso_draft, draft);
        editor.tool = Tool::Edit;
        assert!(!crop_adjust_cursor_admission(
            &editor, true, true, false, false
        ));
        editor.lasso_draft = None;
        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 6.));
        editor.tool = Tool::Lasso;
        editor.lasso_draft = draft;
        // tool 标签及闭合稿不能给已有 Move 补造 Lasso outside 来源。
        assert!(crop_adjust_cursor_source(&editor, true, true));
        assert!(!editor.is_lasso_crop_outside_resize());
        assert!(!crop_adjust_cursor_ready(
            &editor, true, true, true, false, false
        ));
        for point in [(60, 130), (160, 130)] {
            let mut other = self::editor();
            other.pointer_down(point, 6.);
            assert!(!crop_adjust_cursor_source(&other, true, true));
            assert!(!crop_adjust_cursor_ready(
                &other, true, true, true, false, false
            ));
        }
    }

    #[test]
    fn closed_draft_eight_keys_keep_acceleration_and_refuse_unknown_replay_and_cancel_cycles() {
        for shift in [false, true] {
            for (name, direction) in [
                ("left", (-1, 0)),
                ("a", (-1, 0)),
                ("up", (0, -1)),
                ("w", (0, -1)),
                ("right", (1, 0)),
                ("d", (1, 0)),
                ("down", (0, 1)),
                ("s", (0, 1)),
            ] {
                let mut editor = editor();
                let draft = editor.lasso_draft.clone();
                editor.document.set_crop_mask(draft.clone());
                let crop = editor.crop;
                let revision = editor.document.revision();
                assert!(crop_adjust_cursor_admission(
                    &editor, true, true, false, false
                ));
                assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
                let mut keys = ManualPreviewCursorKeys::default();
                let mut nudge = PreviewCursorNudge::default();
                let held = key(name, true, shift);
                assert!(keys.claim_down(&held, true));
                assert!(!keys.allows_move(&held));
                assert_eq!(nudge.down(&held, keys.allows_move(&held), false), None);
                keys.key_up(&KeyUpEvent {
                    keystroke: held.keystroke.clone(),
                });
                let fresh = key(name, false, shift);
                assert!(keys.claim_down(&fresh, crop_adjust_cursor_source(&editor, true, true)));
                let ready = crop_adjust_cursor_ready(&editor, true, true, true, false, false);
                assert_eq!(
                    nudge.down(&fresh, ready && keys.allows_move(&fresh), false),
                    Some(direction)
                );
                let mut final_delta = direction;
                for _ in 0..45 {
                    assert!(keys.claim_down(&held, true));
                    final_delta = nudge
                        .down(&held, ready && keys.allows_move(&held), false)
                        .unwrap();
                    assert!(final_delta.0.abs() <= 20 && final_delta.1.abs() <= 20);
                }
                assert_eq!(final_delta, (direction.0 * 20, direction.1 * 20));
                assert_eq!(editor.crop, crop);
                assert_eq!(editor.lasso_draft, draft);
                assert_eq!(editor.document.revision(), revision);
                assert!(editor.is_lasso_crop_outside_resize());
                keys.key_up(&KeyUpEvent {
                    keystroke: held.keystroke.clone(),
                });
                assert!(keys.claim_down(&fresh, true));
                assert_eq!(
                    nudge.down(&fresh, ready && keys.allows_move(&fresh), true),
                    None
                );
                keys.block(&fresh);
                assert!(keys.claim_down(&held, true));
                assert!(!keys.allows_move(&held));
                assert_eq!(
                    nudge.down(&held, ready && keys.allows_move(&held), false),
                    None
                );
                assert!(editor.cancel_gesture());
                keys.cancel_gesture();
                assert_eq!(editor.crop, crop);
                assert_eq!(editor.lasso_draft, draft);
                assert_eq!(editor.document.crop_mask, draft);
                assert_eq!(editor.document.revision(), revision);
                assert!(keys.claim_down(&held, false));
                assert!(!keys.allows_move(&held));
                keys.key_up(&KeyUpEvent {
                    keystroke: held.keystroke.clone(),
                });
                assert!(!keys.claim_down(&held, false));
            }
        }
    }
}

#[cfg(test)]
mod crop_adjust_cursor_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        geometry::Rect,
    };
    use gpui::{Keystroke, Modifiers};

    fn key(name: &str, held: bool, shift: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers {
                    shift,
                    ..Default::default()
                },
            },
            is_held: held,
        }
    }
    fn crop() -> Rect {
        Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        }
    }
    fn idle() -> Editor {
        Editor::new(Rect::full(400, 300), Some(crop()))
    }
    fn began(kind: usize) -> (Editor, (i32, i32)) {
        let mut editor = idle();
        if kind >= 8 {
            editor.select_tool(Tool::Edit);
        }
        assert!(crop_adjust_cursor_admission(
            &editor, true, true, false, false
        ));
        let start = if kind < 8 {
            let point = [
                (60, 40),
                (160, 40),
                (260, 40),
                (60, 130),
                (260, 130),
                (60, 220),
                (160, 220),
                (260, 220),
            ][kind];
            editor.pointer_down(point, 4.);
            point
        } else if kind == 8 {
            let point = (102, 82);
            assert!(editor.begin_screenshot_empty_crop_inside(point, 4.));
            point
        } else {
            let point = (160, 130);
            assert!(editor.begin_screenshot_empty_crop_center(point, 4.));
            point
        };
        assert!(crop_adjust_cursor_source(&editor, true, true));
        assert_eq!(editor.crop_creation_before(), None);
        (editor, start)
    }

    #[test]
    fn crop_adjust_source_admission_covers_resize_inside_move_and_excludes_other_gestures() {
        for kind in 0..10 {
            let (mut editor, start) = began(kind);
            assert!(crop_adjust_cursor_ready(
                &editor, true, true, true, false, false
            ));
            for (owner, busy, input) in [
                (false, false, false),
                (true, true, false),
                (true, false, true),
            ] {
                assert!(!crop_adjust_cursor_ready(
                    &editor, true, true, owner, busy, input
                ));
            }
            assert!(!crop_adjust_cursor_source(&editor, false, true));
            assert!(!crop_adjust_cursor_source(&editor, true, false));
            assert!(!crop_adjust_cursor_admission(
                &editor, true, true, false, false
            ));
            editor.pointer_up(start);
            assert!(!crop_adjust_cursor_source(&editor, true, true));
        }
        for (ordinary, owner, busy, input) in [
            (false, true, false, false),
            (true, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
        ] {
            assert!(!crop_adjust_cursor_admission(
                &idle(),
                ordinary,
                owner,
                busy,
                input
            ));
        }
        let mut new = Editor::new(Rect::full(400, 300), None);
        assert!(!crop_adjust_cursor_admission(
            &new, true, true, false, false
        ));
        new.pointer_down((40, 30), 4.);
        assert!(!crop_adjust_cursor_source(&new, true, true));
        for point in [(30, 30), (160, 130)] {
            let mut lasso = idle();
            lasso.select_tool(Tool::Lasso);
            lasso.pointer_down(point, 4.);
            assert!(!crop_adjust_cursor_source(&lasso, true, true));
        }
        // 标签不补/拒已接受来源；真实标注 Draw 仍不属于任何 Crop adjust。
        let mut drawing = idle();
        drawing.select_tool(Tool::Rectangle);
        assert!(crop_adjust_cursor_admission(
            &drawing, true, true, false, false
        ));
        drawing.pointer_down((160, 130), 4.);
        assert!(drawing.is_dragging());
        assert!(!crop_adjust_cursor_source(&drawing, true, true));
        let mut selected = idle();
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 20., y: 20. }, Style::default());
        object.update(Point { x: 50., y: 50. });
        selected.document.insert(object);
        selected.toggle_selection((20, 20), 4.);
        assert!(!selected.selected_ids().is_empty());
        assert!(!crop_adjust_cursor_admission(
            &selected, true, true, false, false
        ));
    }

    #[test]
    fn crop_adjust_eight_keys_none_shift_accelerate_without_mutating_original_geometry() {
        for kind in 0..10 {
            for shift in [false, true] {
                for (name, axis) in [
                    ("left", (-1, 0)),
                    ("a", (-1, 0)),
                    ("up", (0, -1)),
                    ("w", (0, -1)),
                    ("right", (1, 0)),
                    ("d", (1, 0)),
                    ("down", (0, 1)),
                    ("s", (0, 1)),
                ] {
                    let (editor, _) = began(kind);
                    let old = editor.crop;
                    let revision = editor.document.revision();
                    let mut keys = ManualPreviewCursorKeys::default();
                    let mut nudge = PreviewCursorNudge::default();
                    let fresh = key(name, false, shift);
                    assert!(keys.claim_down(&fresh, true));
                    let ready = crop_adjust_cursor_ready(&editor, true, true, true, false, false)
                        && keys.allows_move(&fresh);
                    assert_eq!(nudge.down(&fresh, ready, false), Some(axis));
                    let held = key(name, true, shift);
                    for repeat in 1..=42 {
                        assert!(keys.claim_down(&held, true));
                        let step = (1 + repeat / 2).min(20);
                        assert_eq!(
                            nudge.down(&held, keys.allows_move(&held), false),
                            Some((axis.0 * step, axis.1 * step))
                        );
                    }
                    assert!(editor.is_changing_crop());
                    assert_eq!(editor.crop_creation_before(), None);
                    assert_eq!(editor.crop, old);
                    assert_eq!(editor.document.revision(), revision);
                }
            }
        }
    }

    #[test]
    fn crop_adjust_replay_failure_unknown_held_and_cancel_keep_independent_key_up_ownership() {
        let (editor, _) = began(0);
        for (ready, replayed) in [(false, false), (true, true)] {
            let mut keys = ManualPreviewCursorKeys::default();
            let mut nudge = PreviewCursorNudge::default();
            for name in ["a", "d"] {
                let fresh = key(name, false, true);
                assert!(keys.claim_down(&fresh, true));
                assert_eq!(
                    nudge.down(&fresh, ready && keys.allows_move(&fresh), replayed),
                    None
                );
                keys.block(&fresh);
                let held = key(name, true, true);
                assert!(keys.claim_down(&held, true));
                assert!(!keys.allows_move(&held));
                assert_eq!(nudge.down(&held, keys.allows_move(&held), false), None);
            }
            keys.cancel_gesture();
            for name in ["a", "d"] {
                let held = key(name, true, true);
                assert!(keys.claim_down(&held, false));
                assert!(!keys.allows_move(&held));
                keys.key_up(&KeyUpEvent {
                    keystroke: held.keystroke.clone(),
                });
                assert!(!keys.claim_down(&held, false));
            }
            let fresh = key("a", false, true);
            assert!(keys.claim_down(&fresh, crop_adjust_cursor_source(&editor, true, true)));
            assert!(keys.allows_move(&fresh));
        }
        let mut keys = ManualPreviewCursorKeys::default();
        let unknown = key("s", true, false);
        assert!(keys.claim_down(&unknown, true));
        assert!(!keys.allows_move(&unknown));
        keys.cancel_gesture();
        assert!(keys.claim_down(&unknown, false));
        assert!(!keys.allows_move(&unknown));
    }

    #[test]
    fn crop_adjust_readback_stays_sticky_through_owner_loss_return_and_edge_noop() {
        let bounds = ScreenRect {
            left: -900,
            top: -150,
            width: 400,
            height: 300,
        };
        let before = (-800, -70);
        assert!(!crop_adjust_cursor_readback_moved(bounds, before, before));
        assert!(!crop_adjust_cursor_readback_moved(
            bounds,
            before,
            (-901, -70)
        ));
        assert!(!crop_adjust_cursor_readback_moved(
            bounds,
            (-901, -70),
            before
        ));
        let actual = (-799, -70);
        let mut moved = crop_adjust_cursor_readback_moved(bounds, before, actual);
        let (editor, _) = began(0);
        assert!(!crop_adjust_cursor_ready(
            &editor, true, true, false, false, false
        ));
        moved |= crop_adjust_cursor_readback_moved(bounds, actual, before);
        moved |= crop_adjust_cursor_readback_moved(bounds, before, before);
        assert!(moved);
    }

    #[test]
    fn crop_adjust_real_move_up_preserves_shift_aspect_inside_offset_cancel_and_object_history() {
        for kind in 0..10 {
            for shift in [false, true] {
                let (mut editor, start) = began(kind);
                let revision = editor.document.revision();
                let edit_revision = editor.document.edit_revision();
                let up = (start.0 + 11, start.1 + 7);
                let final_pointer = preview_crop_pointer(&editor, up, shift, true, false);
                assert_eq!(final_pointer, editor.constrained_pointer(up, shift));
                editor.pointer_move(final_pointer);
                assert_eq!(editor.document.revision(), revision);
                editor.pointer_up(final_pointer);
                assert_eq!(editor.document.revision(), revision + 1);
                assert_eq!(editor.document.edit_revision(), edit_revision);
                assert!(editor.document.items().is_empty());
                if kind == 8 && !shift {
                    assert_eq!(
                        editor.crop,
                        Some(Rect {
                            x: 111,
                            y: 87,
                            width: 109,
                            height: 93
                        })
                    );
                }
                if kind == 9 {
                    assert_eq!(
                        editor.crop,
                        Some(Rect {
                            x: 111,
                            y: 87,
                            ..crop()
                        })
                    );
                }
                if shift && [0, 2, 5, 7, 8].contains(&kind) {
                    let final_crop = editor.crop.unwrap();
                    assert!(
                        (final_crop.width as f32 - final_crop.height as f32 * 1.2).abs() <= 1.2
                    );
                }
                editor.pointer_up(final_pointer);
                assert_eq!(editor.document.revision(), revision + 1);
                for cancelled in [false, true] {
                    let (mut editor, start) = began(kind);
                    let revision = editor.document.revision();
                    editor.pointer_move(preview_crop_pointer(&editor, up, shift, true, false));
                    if cancelled {
                        assert!(editor.cancel_gesture());
                    } else {
                        // 外侧 Resize 使用实际边位置；恢复原范围时回原边，而非外侧 Down。
                        let original_edge = if kind < 8 {
                            [
                                (100, 80),
                                (160, 80),
                                (220, 80),
                                (100, 130),
                                (220, 130),
                                (100, 180),
                                (160, 180),
                                (220, 180),
                            ][kind]
                        } else {
                            start
                        };
                        let returned =
                            preview_crop_pointer(&editor, original_edge, shift, true, false);
                        editor.pointer_move(returned);
                        editor.pointer_up(returned);
                    }
                    assert_eq!(editor.crop, Some(crop()));
                    assert_eq!(editor.document.revision(), revision);
                }
            }
        }
        let mut editor = idle();
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 20., y: 20. }, Style::default());
        object.update(Point { x: 50., y: 50. });
        editor.document.insert(object);
        let objects = editor.document.items().to_vec();
        let edit_revision = editor.document.edit_revision();
        editor.pointer_down((160, 130), 4.);
        assert!(crop_adjust_cursor_source(&editor, true, true));
        editor.pointer_up((171, 137));
        assert_eq!(editor.document.items(), objects);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), objects);
    }
}

#[cfg(test)]
mod new_crop_cursor_tests {
    use super::*;
    use crate::{annotation::Tool, geometry::Rect};
    use gpui::{Keystroke, Modifiers};

    fn key(name: &str, held: bool, shift: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers {
                    shift,
                    ..Default::default()
                },
            },
            is_held: held,
        }
    }
    fn editor() -> Editor {
        let mut editor = Editor::new(Rect::full(400, 300), None);
        editor.pointer_down((80, 70), 4.);
        editor
    }

    #[test]
    fn new_crop_source_keeps_original_none_before_and_excludes_existing_crop() {
        let mut new = editor();
        assert!(new_crop_cursor_source(&new, true, true));
        assert!(!new_crop_cursor_source(&new, false, true));
        assert!(!new_crop_cursor_source(&new, true, false));
        for (ordinary, accepted, owner, busy, input) in [
            (false, true, true, false, false),
            (true, false, true, false, false),
            (true, true, false, false, false),
            (true, true, true, true, false),
            (true, true, true, false, true),
        ] {
            assert!(!new_crop_cursor_ready(
                &new, ordinary, accepted, owner, busy, input
            ));
        }
        assert!(new_crop_cursor_ready(&new, true, true, true, false, false));
        new.pointer_move((110, 90));
        assert!(new.crop.is_some());
        assert!(new_crop_cursor_source(&new, true, true));
        new.pointer_up((110, 90));
        assert!(!new_crop_cursor_source(&new, true, true));
        for point in [(110, 90), (80, 70), (40, 30)] {
            let mut existing = Editor::new(
                Rect::full(400, 300),
                Some(Rect {
                    x: 80,
                    y: 70,
                    width: 100,
                    height: 80,
                }),
            );
            existing.pointer_down(point, 4.);
            assert!(existing.is_changing_crop());
            assert!(!new_crop_cursor_source(&existing, true, true));
        }
        let mut annotation = Editor::new(Rect::full(400, 300), Some(Rect::full(200, 150)));
        annotation.select_tool(Tool::Rectangle);
        annotation.pointer_down((80, 70), 4.);
        assert!(annotation.is_dragging());
        assert!(!new_crop_cursor_source(&annotation, true, true));
    }

    #[test]
    fn new_crop_eight_keys_accelerate_without_ending_the_original_gesture() {
        for (name, direction) in [
            ("left", (-1, 0)),
            ("a", (-1, 0)),
            ("right", (1, 0)),
            ("d", (1, 0)),
            ("up", (0, -1)),
            ("w", (0, -1)),
            ("down", (0, 1)),
            ("s", (0, 1)),
        ] {
            let mut editor = editor();
            editor.pointer_move((110, 90));
            let crop = editor.crop;
            let revision = editor.document.edit_revision();
            let mut keys = ManualPreviewCursorKeys::default();
            let mut nudge = PreviewCursorNudge::default();
            for repeat in 0..45 {
                let event = key(name, repeat != 0, repeat % 3 == 0);
                assert!(keys.claim_down(&event, new_crop_cursor_source(&editor, true, true)));
                let ready = new_crop_cursor_ready(&editor, true, true, true, false, false)
                    && keys.allows_move(&event);
                let step = (1 + repeat / 2).min(20);
                assert_eq!(
                    nudge.down(&event, ready, false),
                    Some((direction.0 * step, direction.1 * step))
                );
                assert_eq!(editor.crop_creation_before(), Some(None));
                assert_eq!(editor.crop, crop); // KeyDown 不代替实际 Move。
                assert_eq!(editor.document.edit_revision(), revision);
            }
            // 真正的 Move/Up 仍从原 Down 出发；Shift 是普通 New 的正方形规则。
            let point = preview_crop_pointer(&editor, (112, 92), true, true, false);
            editor.pointer_move(point);
            let final_crop = editor.crop.unwrap();
            assert_eq!((final_crop.x, final_crop.y), (80, 70));
            assert_eq!(final_crop.width, final_crop.height);
            editor.pointer_up(point);
            assert_eq!(editor.crop, Some(final_crop));
            assert_eq!(editor.document.edit_revision(), revision);
            assert!(!editor.document.can_undo());
        }
    }

    #[test]
    fn replay_and_failed_new_keys_stay_blocked_until_a_real_fresh_cycle() {
        for name in ["left", "up", "right", "down", "a", "w", "d", "s"] {
            let editor = editor();
            let source = new_crop_cursor_source(&editor, true, true);
            let mut keys = ManualPreviewCursorKeys::default();
            let mut nudge = PreviewCursorNudge::default();
            let held = key(name, true, false);
            assert!(keys.claim_down(&held, source));
            assert!(!keys.allows_move(&held)); // 旧 idle/未知 held 从未迁移到 New。
            let fresh = key(name, false, true);
            assert!(keys.claim_down(&fresh, source));
            assert_eq!(nudge.down(&fresh, keys.allows_move(&fresh), true), None);
            keys.block(&fresh);
            assert!(keys.claim_down(&held, source));
            assert!(!keys.allows_move(&held));
            // 原 Down/New 可由可信 mouse replay 重建；旧键重放不授予未来 held 资格。
            assert_eq!(editor.crop_creation_before(), Some(None));
            assert!(keys.claim_down(&fresh, source));
            assert!(nudge
                .down(&fresh, keys.allows_move(&fresh), false)
                .is_some());
            keys.block(&fresh); // API/owner/readback 失败撤同周期，不能 held 重启。
            assert!(keys.claim_down(&held, source));
            assert!(!keys.allows_move(&held));
            keys.cancel_gesture(); // Up/cancel/失焦只 disarm，旧 Up 仍归本端。
            assert!(keys.claim_down(&held, false));
            assert!(!keys.allows_move(&held));
            keys.key_up(&KeyUpEvent {
                keystroke: held.keystroke.clone(),
            });
            assert!(!keys.claim_down(&held, false));
            assert!(keys.claim_down(&fresh, source));
            assert!(keys.allows_move(&fresh));
        }
    }

    #[test]
    fn new_crop_real_readback_keeps_sticky_motion_and_ordinary_shift_geometry() {
        let bounds = ScreenRect {
            left: -900,
            top: -150,
            width: 400,
            height: 300,
        };
        let start = (-820, -80);
        assert!(!new_crop_cursor_readback_moved(bounds, start, start));
        assert!(!new_crop_cursor_readback_moved(bounds, start, (-901, -80)));
        assert!(!new_crop_cursor_readback_moved(bounds, (-901, -80), start));
        let far = (-800, -80);
        let mut moved = new_crop_cursor_readback_moved(bounds, start, far);
        // 实际 far 后丢 owner 不抹历史；返回/合并 Move 后旧点击不能复活。
        assert!(moved);
        moved |= new_crop_cursor_readback_moved(bounds, far, start);
        moved |= new_crop_cursor_readback_moved(bounds, start, start);
        assert!(moved);
        assert!(new_crop_cursor_readback_moved(bounds, start, (-819, -80)));
        let mut editor = editor();
        let actual = (110, 90);
        let ordinary = preview_crop_pointer(&editor, actual, true, true, false);
        let fixed = preview_crop_pointer(&editor, actual, true, true, true);
        assert_eq!(fixed, actual);
        assert_ne!(ordinary, fixed);
        editor.pointer_move(ordinary);
        let crop = editor.crop.unwrap();
        assert_eq!((crop.x, crop.y), (80, 70));
        assert_eq!(crop.width, crop.height);
        assert_eq!(editor.crop_creation_before(), Some(None));
    }
}

pub(crate) fn preview_ready(
    editor: &Editor,
    ordinary: bool,
    owner_ready: bool,
    busy: bool,
    input_active: bool,
) -> bool {
    ordinary
        && owner_ready
        && !busy
        && !input_active
        && editor.crop.is_none()
        && editor.selected_ids().is_empty()
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
}

/// 只证明已接受的固定预览鼠标手势；系统焦点/捕获/按键由调用端实时核验。
#[derive(Clone, Copy, Default)]
pub(crate) struct ManualPreviewCursorState {
    pub(crate) manual: Option<Rect>,
    pub(crate) click: Option<Rect>,
    pub(crate) down: Option<(i32, i32)>,
    pub(crate) claimed: bool,
}

pub(crate) fn manual_preview_cursor_ready(
    editor: &Editor,
    state: ManualPreviewCursorState,
    ordinary: bool,
    owner_ready: bool,
    busy_other: bool,
    input_active: bool,
) -> bool {
    if !state.claimed || state.down.is_none() {
        return false;
    }
    if editor.is_changing_crop() {
        return ordinary
            && owner_ready
            && !busy_other
            && !input_active
            && editor.selected_ids().is_empty()
            && !editor.has_pending_drawing()
            && editor.text_draft.is_none()
            && editor.lasso_draft.is_none()
            && !editor.watermark.has_preview();
    }
    state
        .manual
        .is_some_and(|rect| rect.width > 0 && rect.height > 0 && state.click == Some(rect))
        && preview_ready(editor, ordinary, owner_ready, busy_other, input_active)
}

pub(crate) fn claims_manual_preview_cursor(claimed: bool, event: &KeyDownEvent) -> bool {
    claimed
        && preview_movement_modifiers(event.keystroke.modifiers)
        && is_preview_movement_key(&event.keystroke.key)
}

pub(crate) fn manual_preview_cursor_buttons_ready(left: bool, other: [bool; 4]) -> bool {
    left && !other.into_iter().any(|pressed| pressed)
}

fn movement_key_bit(key: &str) -> u8 {
    match key {
        "left" => 1,
        "up" => 2,
        "right" => 4,
        "down" => 8,
        "a" => 16,
        "w" => 32,
        "d" => 64,
        "s" => 128,
        _ => 0,
    }
}

#[cfg(test)]
mod manual_preview_cursor_tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};
    use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_KEYUP};

    fn key(name: &str, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers::default(),
            },
            is_held: held,
        }
    }
    fn up(name: &str) -> KeyUpEvent {
        KeyUpEvent {
            keystroke: key(name, false).keystroke,
        }
    }

    #[test]
    fn pending_and_started_crop_keep_only_the_accepted_source_and_input_gates() {
        let mut editor = Editor::new(Rect::full(400, 300), None);
        let range = Rect::full(200, 150);
        let state = ManualPreviewCursorState {
            manual: Some(range),
            click: Some(range),
            down: Some((40, 40)),
            claimed: true,
        };
        let ready = |editor: &Editor, state, ordinary, owner, busy, input| {
            manual_preview_cursor_ready(editor, state, ordinary, owner, busy, input)
        };
        assert!(ready(&editor, state, true, true, false, false));
        for invalid in [
            ManualPreviewCursorState {
                claimed: false,
                ..state
            },
            ManualPreviewCursorState {
                down: None,
                ..state
            },
            ManualPreviewCursorState {
                click: None,
                ..state
            },
            ManualPreviewCursorState {
                manual: Some(Rect::full(201, 150)),
                ..state
            },
        ] {
            assert!(!ready(&editor, invalid, true, true, false, false));
        }
        for (ordinary, owner, busy, input) in [
            (false, true, false, false),
            (true, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
        ] {
            assert!(!ready(&editor, state, ordinary, owner, busy, input));
        }
        assert!(!manual_preview_cursor_buttons_ready(false, [false; 4]));
        for index in 0..4 {
            let mut buttons = [false; 4];
            buttons[index] = true;
            assert!(!manual_preview_cursor_buttons_ready(true, buttons));
        }
        assert!(manual_preview_cursor_buttons_ready(true, [false; 4]));

        // 真实 MouseMove 才从原 Down 创建 CropGesture；来源claim不中断WASD。
        editor.pointer_down((40, 40), 4.);
        editor.pointer_move((48, 41));
        assert!(editor.is_changing_crop());
        let dragging = ManualPreviewCursorState {
            manual: None,
            click: None,
            ..state
        };
        assert!(ready(&editor, dragging, true, true, false, false));
        assert!(!ready(&editor, dragging, true, true, false, true));
        editor.pointer_up((48, 41));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 40,
                y: 40,
                width: 8,
                height: 1
            })
        );
        assert!(!ready(&editor, dragging, true, true, false, false));
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn manual_keys_stay_consumed_after_mouse_up_until_real_key_up_or_fresh_down() {
        for name in ["left", "up", "right", "down", "a", "w", "d", "s"] {
            let mut keys = ManualPreviewCursorKeys::default();
            let mut nudge = PreviewCursorNudge::default();
            let down = key(name, false);
            assert!(keys.claim_down(&down, true));
            assert!(keys.allows_move(&down));
            assert!(nudge.down(&down, true, false).is_some());
            keys.cancel_gesture();
            let held = key(name, true);
            // MouseUp后的cropSome不决定键归属，仍消费且不能进入旧Current/Spotlight。
            assert!(keys.claim_down(&held, false));
            assert!(!keys.allows_move(&held));
            // 新鼠标Down不能重新激活尚未KeyUp的物理周期。
            assert!(keys.claim_down(&held, true));
            assert!(!keys.allows_move(&held));
            keys.key_up(&up(name));
            assert!(!keys.claim_down(&held, false));
            assert!(!keys.allows_move(&held));

            assert!(keys.claim_down(&down, true));
            keys.cancel_gesture();
            // 漏失Up后新freshDown必须过期旧认领；普通工具的新周期不被吞。
            assert!(!keys.claim_down(&down, false));
            assert!(!keys.claim_down(&held, false));
            assert!(keys.claim_down(&down, true));
            assert!(keys.allows_move(&down));
        }
    }

    #[test]
    fn failed_or_replayed_manual_key_cycle_cannot_resume_on_a_held_event() {
        let mut keys = ManualPreviewCursorKeys::default();
        let left = key("left", false);
        let a = key("a", false);
        assert!(keys.claim_down(&left, true));
        assert!(keys.claim_down(&a, true));
        keys.block(&a); // replay／API／前台资格失败，保留真实KeyUp归属。
        assert!(keys.claim_down(&key("a", true), true));
        assert!(!keys.allows_move(&a));
        assert!(keys.allows_move(&left));
        keys.key_up(&up("left"));
        assert!(keys.claim_down(&key("a", true), false));
        assert!(!keys.claim_down(&key("left", true), false));
        keys.key_up(&up("a"));
        // 没有当前会话实际Down的held与Native一致，只消费不晚启动移动。
        assert!(keys.claim_down(&key("s", true), true));
        assert!(!keys.allows_move(&key("s", true)));
        assert!(!keys.claim_down(&key("enter", false), true));
        let mut chord = key("s", true);
        chord.keystroke.modifiers.control = true;
        assert!(!keys.claim_down(&chord, true));
        assert!(keys.claim_down(&key("s", false), true));
        assert!(keys.allows_move(&key("s", false)));
    }

    #[test]
    fn native_manual_rejection_preserves_key_up_without_late_move_or_fresh_cycle_aba() {
        let mut native = NativeArrowNudge::default();
        let modifiers = Modifiers::default();
        let down = (WM_KEYDOWN, 0x41, 0);
        let held = (WM_KEYDOWN, 0x41, 1 << 30);
        assert_eq!(
            native.route(down, modifiers, true),
            NativeArrowRoute::Move((-1, 0))
        );
        // 新鼠标Down、取消或失焦撤重复资格，不能承接闲置态的旧持键。
        native.input_lost();
        assert_eq!(native.route(held, modifiers, true), NativeArrowRoute::Pass);
        assert!(native.owns_claimed_down(held, modifiers));
        assert!(native.claim_manual_down(held, modifiers));
        assert_eq!(native.route(held, modifiers, true), NativeArrowRoute::Pass);
        assert!(native.owns_claimed_down(held, modifiers));
        assert_eq!(
            native.route((WM_KEYUP, 0x41, 0), modifiers, false),
            NativeArrowRoute::ConsumeUp
        );
        assert!(!native.owns_claimed_down(held, modifiers));
        assert!(native.claim_manual_down(down, modifiers));
        native.begin_physical_down(down);
        assert!(!native.owns_claimed_down(down, modifiers));
        assert_eq!(
            native.route(down, modifiers, true),
            NativeArrowRoute::Move((-1, 0))
        );
        let mut chord = modifiers;
        chord.control = true;
        assert!(!native.claim_manual_down(held, chord));
        assert!(!native.claim_manual_down((WM_KEYDOWN, 0x43, 0), modifiers));
    }
}

/// 鼠标 Up 后停止位移，但原物理方向键仍归属本次操作，直到 KeyUp 或新 Down。
#[derive(Default)]
pub(crate) struct ManualPreviewCursorKeys {
    accepted: u8,
    blocked: u8,
}
impl ManualPreviewCursorKeys {
    /// 只接纳本端已提交的 idle 物理周期；旧 blocked 周期不能凭 held 自行恢复。
    pub(crate) fn adopt_idle(
        &mut self,
        snapshot: IdlePreviewCursorSnapshot,
        physical_held: [bool; 8],
        nudge: &mut PreviewCursorNudge,
    ) -> bool {
        let bits = snapshot.verified & physical_movement_mask(physical_held);
        if bits == 0 {
            return false;
        }
        self.accepted |= bits;
        self.blocked &= !bits;
        *nudge = snapshot.nudge;
        true
    }
    pub(crate) fn observe_down(&mut self, event: &KeyDownEvent) {
        if !event.is_held {
            let bit = movement_key_bit(&event.keystroke.key);
            self.accepted &= !bit;
            self.blocked &= !bit;
        }
    }
    pub(crate) fn claim_down(&mut self, event: &KeyDownEvent, gesture_claimed: bool) -> bool {
        self.observe_down(event);
        let bit = movement_key_bit(&event.keystroke.key);
        if !claims_manual_preview_cursor(
            gesture_claimed || event.is_held && self.accepted & bit != 0,
            event,
        ) {
            return false;
        }
        // 当前会话没接受过真实 Down 的 held 只能消费；不能在交接/新鼠标手势中晚启。
        if event.is_held && self.accepted & bit == 0 {
            self.blocked |= bit;
        }
        self.accepted |= bit;
        true
    }
    pub(crate) fn allows_move(&self, event: &KeyDownEvent) -> bool {
        let bit = movement_key_bit(&event.keystroke.key);
        bit != 0 && self.accepted & bit != 0 && self.blocked & bit == 0
    }
    pub(crate) fn block(&mut self, event: &KeyDownEvent) {
        self.blocked |= self.accepted & movement_key_bit(&event.keystroke.key);
    }
    pub(crate) fn cancel_gesture(&mut self) {
        self.blocked |= self.accepted;
    }
    pub(crate) fn key_up(&mut self, event: &KeyUpEvent) {
        let bit = movement_key_bit(&event.keystroke.key);
        self.accepted &= !bit;
        self.blocked &= !bit;
    }
}

#[derive(Clone, Default)]
pub(crate) struct PreviewCursorNudge {
    direction: Option<Direction>,
    repeats: u8,
}
impl PreviewCursorNudge {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
    /// 捕获阶段只观察边界；真正消费箭头的入口才累计重复次数。
    pub(crate) fn observe_key_down(&mut self, event: &KeyDownEvent, ready: bool, replayed: bool) {
        if Direction::from_key(&event.keystroke.key).is_none()
            || !preview_movement_modifiers(event.keystroke.modifiers)
            || !ready
            || replayed
        {
            self.reset();
        }
    }
    pub(crate) fn key_up(&mut self, event: &KeyUpEvent) {
        if Direction::from_key(&event.keystroke.key).is_some() {
            self.reset();
        }
    }
    pub(crate) fn modifiers_changed(&mut self, modifiers: gpui::Modifiers) {
        if !preview_movement_modifiers(modifiers) {
            self.reset();
        }
    }
    pub(crate) fn down(
        &mut self,
        event: &KeyDownEvent,
        ready: bool,
        replayed: bool,
    ) -> Option<(i32, i32)> {
        self.observe_key_down(event, ready, replayed);
        let direction = Direction::from_key(&event.keystroke.key)?;
        if !ready || replayed || !preview_movement_modifiers(event.keystroke.modifiers) {
            return None;
        }
        if !event.is_held || self.direction != Some(direction) {
            self.direction = Some(direction);
            self.repeats = 0;
        } else {
            self.repeats = self.repeats.saturating_add(1);
        }
        Some(direction.delta((1 + i32::from(self.repeats) / 2).min(20)))
    }
}

/// 与接受移动意图分离；只有实际 OS 提交读回或最终确认的边缘 no-op 才能迁移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdlePreviewCursorCommit {
    Requested,
    EdgeNoop,
    Rejected,
}

pub(crate) fn idle_preview_cursor_commit(
    bounds: ScreenRect,
    before: (i32, i32),
    target: (i32, i32),
    actual: Option<(i32, i32)>,
    request_succeeded: bool,
    owner_ready: bool,
) -> IdlePreviewCursorCommit {
    let Some(actual) = actual else {
        return IdlePreviewCursorCommit::Rejected;
    };
    if !owner_ready
        || [before, target, actual]
            .into_iter()
            .any(|point| target_in_client(bounds, point, (0, 0)).is_none())
    {
        return IdlePreviewCursorCommit::Rejected;
    }
    if target == before {
        if actual == before {
            IdlePreviewCursorCommit::EdgeNoop
        } else {
            IdlePreviewCursorCommit::Rejected
        }
    } else if request_succeeded {
        // ClipCursor 可限制成功请求；记录真实合法读回，不假设 actual == target。
        IdlePreviewCursorCommit::Requested
    } else {
        IdlePreviewCursorCommit::Rejected
    }
}

fn physical_movement_mask(physical_held: [bool; 8]) -> u8 {
    physical_held
        .into_iter()
        .enumerate()
        .fold(0, |mask, (index, held)| mask | (u8::from(held) << index))
}

/// 快照只在当前 runtime/View 栈内一次消费，不进入 handoff 或任何持久化数据。
pub(crate) struct IdlePreviewCursorSnapshot {
    verified: u8,
    nudge: PreviewCursorNudge,
}
impl IdlePreviewCursorSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.verified == 0
    }
}

#[derive(Clone, Default)]
pub(crate) struct IdlePreviewCursorKeys {
    verified: u8,
}
impl IdlePreviewCursorKeys {
    pub(crate) fn has_verified(&self) -> bool {
        self.verified != 0
    }
    pub(crate) fn observe_down(&mut self, event: &KeyDownEvent) {
        if !event.is_held {
            self.verified &= !movement_key_bit(&event.keystroke.key);
        }
    }
    pub(crate) fn record_commit(&mut self, event: &KeyDownEvent, commit: IdlePreviewCursorCommit) {
        let bit = movement_key_bit(&event.keystroke.key);
        if !preview_movement_modifiers(event.keystroke.modifiers) {
            self.cancel();
        } else if commit == IdlePreviewCursorCommit::Rejected {
            self.verified &= !bit;
        } else if !event.is_held {
            self.verified |= bit;
        }
        // held 的成功不能给失败/未知/重放周期补来源证明。
    }
    pub(crate) fn key_up(&mut self, event: &KeyUpEvent) {
        self.verified &= !movement_key_bit(&event.keystroke.key);
    }
    pub(crate) fn modifiers_changed(&mut self, modifiers: gpui::Modifiers) {
        if !preview_movement_modifiers(modifiers) {
            self.cancel();
        }
    }
    pub(crate) fn cancel(&mut self) {
        self.verified = 0;
    }
    pub(crate) fn snapshot(&mut self, nudge: &PreviewCursorNudge) -> IdlePreviewCursorSnapshot {
        let verified = std::mem::take(&mut self.verified);
        IdlePreviewCursorSnapshot {
            verified,
            nudge: nudge.clone(),
        }
    }
}

#[cfg(test)]
mod idle_preview_cursor_tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    };

    fn key(name: &str, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers::default(),
            },
            is_held: held,
        }
    }
    fn up(name: &str) -> KeyUpEvent {
        KeyUpEvent {
            keystroke: key(name, false).keystroke,
        }
    }
    fn left_held() -> [bool; 8] {
        [true, false, false, false, false, false, false, false]
    }

    #[test]
    fn only_fresh_confirmed_requests_and_edge_noops_create_idle_proof() {
        let bounds = ScreenRect {
            left: -100,
            top: -50,
            width: 200,
            height: 100,
        };
        let receipt = |before, target, actual, submitted, owner| {
            idle_preview_cursor_commit(bounds, before, target, actual, submitted, owner)
        };
        assert_eq!(
            receipt((0, 0), (-1, 0), Some((-1, 0)), true, true),
            IdlePreviewCursorCommit::Requested
        );
        // 合法 ClipCursor 零位移依然是有真实读回的成功请求。
        assert_eq!(
            receipt((0, 0), (-1, 0), Some((0, 0)), true, true),
            IdlePreviewCursorCommit::Requested
        );
        let edge = receipt((-100, 0), (-100, 0), Some((-100, 0)), false, true);
        assert_eq!(edge, IdlePreviewCursorCommit::EdgeNoop);
        for invalid in [
            receipt((0, 0), (-1, 0), None, true, true),
            receipt((0, 0), (-1, 0), Some((-1, 0)), false, true),
            receipt((0, 0), (-1, 0), Some((-1, 0)), true, false),
            receipt((0, 0), (-1, 0), Some((-101, 0)), true, true),
            receipt((-100, 0), (-100, 0), Some((-99, 0)), false, true),
        ] {
            assert_eq!(invalid, IdlePreviewCursorCommit::Rejected);
        }
        for confirmed in [IdlePreviewCursorCommit::Requested, edge] {
            let mut idle = IdlePreviewCursorKeys::default();
            idle.record_commit(&key("left", true), confirmed);
            assert!(!idle.has_verified()); // 未知 held 没有 fresh 提交来源。
            idle.observe_down(&key("left", false));
            idle.record_commit(&key("left", false), confirmed);
            assert!(idle.has_verified());
        }
    }

    #[test]
    fn held_failure_replay_and_cancel_cannot_rebuild_an_idle_cycle() {
        let mut idle = IdlePreviewCursorKeys::default();
        let fresh = key("s", false);
        let held = key("s", true);
        for revoke in [0, 1, 2] {
            idle.observe_down(&fresh);
            idle.record_commit(&fresh, IdlePreviewCursorCommit::Requested);
            assert!(idle.has_verified());
            match revoke {
                0 => idle.record_commit(&held, IdlePreviewCursorCommit::Rejected),
                1 => idle.cancel(), // View replay、鼠标取消、失焦或 handoff 撤全来源。
                _ => idle.modifiers_changed(Modifiers {
                    control: true,
                    ..Default::default()
                }),
            }
            idle.record_commit(&held, IdlePreviewCursorCommit::Requested);
            assert!(idle.snapshot(&PreviewCursorNudge::default()).is_empty());
        }
        idle.observe_down(&fresh);
        idle.record_commit(&fresh, IdlePreviewCursorCommit::Requested);
        idle.modifiers_changed(Modifiers {
            shift: true,
            ..Default::default()
        });
        assert!(idle.has_verified());
        idle.observe_down(&fresh); // 漏失 Up 后新 fresh 必须先过期同键。
        idle.record_commit(&held, IdlePreviewCursorCommit::Requested);
        assert!(!idle.has_verified());
    }

    #[test]
    fn gpui_adoption_keeps_repeat_and_blocks_mouse_up_held_until_key_up() {
        let mut idle = IdlePreviewCursorKeys::default();
        let mut nudge = PreviewCursorNudge::default();
        let fresh = key("left", false);
        let held = key("left", true);
        assert_eq!(nudge.down(&fresh, true, false), Some((-1, 0)));
        idle.record_commit(&fresh, IdlePreviewCursorCommit::Requested);
        for _ in 0..38 {
            nudge.down(&held, true, false);
            idle.record_commit(&held, IdlePreviewCursorCommit::Requested);
        }
        let snapshot = idle.snapshot(&nudge);
        assert!(!snapshot.is_empty());
        assert!(idle.snapshot(&nudge).is_empty()); // 同一来源仅取一次。
        nudge.reset();
        let mut manual = ManualPreviewCursorKeys::default();
        manual.cancel_gesture();
        assert!(manual.adopt_idle(snapshot, left_held(), &mut nudge));
        assert!(manual.claim_down(&held, true));
        assert!(manual.allows_move(&held));
        assert_eq!(nudge.down(&held, true, false), Some((-20, 0)));
        manual.cancel_gesture(); // 鼠标 Up 只 disarm，保留真实 KeyUp 归属。
        assert!(manual.claim_down(&held, false));
        assert!(!manual.allows_move(&held));
        assert!(manual.claim_down(&held, true));
        assert!(!manual.allows_move(&held));
        manual.key_up(&up("left"));
        assert!(!manual.claim_down(&held, false));
    }

    #[test]
    fn physical_aliases_release_independently_and_fresh_expires_only_its_key() {
        let mut idle = IdlePreviewCursorKeys::default();
        for name in ["left", "a"] {
            idle.record_commit(&key(name, false), IdlePreviewCursorCommit::Requested);
        }
        idle.key_up(&up("a"));
        assert!(idle.has_verified());
        let mut manual = ManualPreviewCursorKeys::default();
        let mut nudge = PreviewCursorNudge::default();
        assert!(manual.adopt_idle(idle.snapshot(&nudge), [true; 8], &mut nudge));
        assert!(manual.claim_down(&key("left", true), true));
        assert!(manual.allows_move(&key("left", true)));
        assert!(manual.claim_down(&key("a", true), true));
        assert!(!manual.allows_move(&key("a", true)));
        for name in ["left", "a"] {
            idle.record_commit(&key(name, false), IdlePreviewCursorCommit::Requested);
        }
        idle.observe_down(&key("left", false));
        let snapshot = idle.snapshot(&nudge);
        assert_eq!(snapshot.verified, movement_key_bit("a"));
        assert!(!manual.adopt_idle(snapshot, left_held(), &mut nudge));
    }

    #[test]
    fn native_intent_failure_and_key_up_are_separate_from_os_commit_proof() {
        let mut native = NativeArrowNudge::default();
        let modifiers = Modifiers::default();
        let fresh = (WM_KEYDOWN, 0x25, 0);
        let held = (WM_KEYDOWN, 0x25, 1 << 30);
        assert_eq!(
            native.route(fresh, modifiers, true),
            NativeArrowRoute::Move((-1, 0))
        );
        assert!(native.idle_snapshot().is_empty()); // accepted/repeatable 只是意图。
        native.record_idle_commit(fresh, modifiers, IdlePreviewCursorCommit::Rejected);
        native.record_idle_commit(held, modifiers, IdlePreviewCursorCommit::Requested);
        assert!(native.idle_snapshot().is_empty());
        native.route(fresh, modifiers, true);
        native.record_idle_commit(fresh, modifiers, IdlePreviewCursorCommit::Requested);
        native.route((WM_KEYDOWN, 0x41, 0), modifiers, true);
        native.record_idle_commit(
            (WM_KEYDOWN, 0x41, 0),
            modifiers,
            IdlePreviewCursorCommit::Requested,
        );
        assert_eq!(
            native.route((WM_KEYUP, 0x41, 0), modifiers, false),
            NativeArrowRoute::ConsumeUp
        );
        let snapshot = native.idle_snapshot();
        assert_eq!(snapshot.verified, movement_key_bit("left"));
        native.input_lost();
        assert!(native.adopt_idle(snapshot, left_held()));
        native.route((WM_KILLFOCUS, 0, 0), modifiers, false);
        assert!(native.idle_snapshot().is_empty());
        assert_eq!(native.route(held, modifiers, true), NativeArrowRoute::Pass);
        assert_eq!(
            native.route((WM_KEYUP, 0x25, 0), modifiers, false),
            NativeArrowRoute::ConsumeUp
        );
    }

    #[test]
    fn native_snapshot_precedes_mouse_reset_and_preserves_verified_repeat_only() {
        let mut native = NativeArrowNudge::default();
        let modifiers = Modifiers {
            shift: true,
            ..Default::default()
        };
        let fresh = (WM_KEYDOWN, 0x25, 0);
        let held = (WM_KEYDOWN, 0x25, 1 << 30);
        native.route(fresh, modifiers, true);
        native.record_idle_commit(fresh, modifiers, IdlePreviewCursorCommit::EdgeNoop);
        for _ in 0..12 {
            native.route(held, modifiers, true);
            native.record_idle_commit(held, modifiers, IdlePreviewCursorCommit::Requested);
        }
        let snapshot = native.idle_snapshot();
        assert_eq!(
            native.route((WM_LBUTTONDOWN, 1, 0), modifiers, false),
            NativeArrowRoute::Pass
        );
        native.input_lost();
        assert!(native.adopt_idle(snapshot, left_held()));
        assert_eq!(
            native.route(held, modifiers, true),
            NativeArrowRoute::Move((-7, 0))
        );
        native.input_lost();
        native.record_idle_commit(held, modifiers, IdlePreviewCursorCommit::Requested);
        let empty = native.idle_snapshot();
        assert!(!native.adopt_idle(empty, [true; 8]));
        assert_eq!(native.route(held, modifiers, true), NativeArrowRoute::Pass);
        assert!(native.claim_manual_down(held, modifiers));
        assert_eq!(native.route(held, modifiers, true), NativeArrowRoute::Pass);
        assert_eq!(
            native.route((WM_KEYUP, 0x25, 0), modifiers, false),
            NativeArrowRoute::ConsumeUp
        );
    }
}

/// 客户区内当前位置是本应用的移动资格；坐标始终是带符号的物理屏幕像素。
pub(crate) fn target_in_client(
    bounds: ScreenRect,
    current: (i32, i32),
    delta: (i32, i32),
) -> Option<(i32, i32)> {
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }
    let right = i64::from(bounds.left) + i64::from(bounds.width) - 1;
    let bottom = i64::from(bounds.top) + i64::from(bounds.height) - 1;
    let right = i32::try_from(right).ok()?;
    let bottom = i32::try_from(bottom).ok()?;
    if current.0 < bounds.left || current.0 > right || current.1 < bounds.top || current.1 > bottom
    {
        return None;
    }
    Some((
        current.0.checked_add(delta.0)?.clamp(bounds.left, right),
        current.1.checked_add(delta.1)?.clamp(bounds.top, bottom),
    ))
}

#[cfg(test)]
mod crop_drag_arrow_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        crop_mask::Polygon,
        geometry::Rect,
    };
    use gpui::{Keystroke, Modifiers};

    fn key(key: &str, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: Modifiers::default(),
            },
            is_held: held,
        }
    }
    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 20., y: 20. }, Style::default());
        object.update(Point { x: 50., y: 50. });
        editor.document.insert(object);
        editor
    }
    fn mask() -> Polygon {
        Polygon::new(&[
            Point { x: 100., y: 80. },
            Point { x: 220., y: 80. },
            Point { x: 220., y: 180. },
            Point { x: 100., y: 180. },
        ])
        .unwrap()
    }
    fn local_target(editor: &Editor, key: &str, current: (i32, i32)) -> (i32, i32) {
        let CropDragArrowRoute::Move(delta) =
            crop_drag_arrow_route(editor, true, true, &self::key(key, false), true, false)
        else {
            panic!("real crop gesture must admit its bare arrow");
        };
        let bounds = ScreenRect {
            left: -900,
            top: -150,
            width: 400,
            height: 300,
        };
        let target = target_in_client(
            bounds,
            (bounds.left + current.0, bounds.top + current.1),
            delta,
        )
        .unwrap();
        (target.0 - bounds.left, target.1 - bounds.top)
    }

    #[test]
    fn crop_move_arrow_keeps_real_gesture_and_existing_object_history() {
        let mut editor = editor();
        let objects = editor.document.items().to_vec();
        let edit_revision = editor.document.edit_revision();
        editor.pointer_down((160, 130), 4.);
        let target = local_target(&editor, "right", (160, 130));
        assert!(editor.is_changing_crop());
        assert_eq!(editor.crop.unwrap().x, 100);
        editor.pointer_move(target);
        assert!(editor.is_changing_crop());
        assert_eq!(editor.crop.unwrap().x, 101);
        assert_eq!(editor.document.items(), objects);
        editor.pointer_up(target);
        assert!(!editor.is_dragging());
        assert_eq!(editor.crop.unwrap().x, 101);
        assert_eq!(editor.tool, Tool::Crop);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items(), objects);
    }

    #[test]
    fn crop_resize_and_new_arrow_use_physical_targets_until_real_up() {
        let mut resize = editor();
        resize.pointer_down((220, 130), 4.);
        let target = local_target(&resize, "right", (220, 130));
        resize.pointer_move(target);
        assert!(resize.is_changing_crop());
        assert_eq!(resize.crop.unwrap().width, 121);
        assert_eq!(
            crop_drag_arrow_route(&resize, true, true, &key("right", true), true, false),
            CropDragArrowRoute::Move((1, 0))
        );
        resize.pointer_up(target);
        assert!(!resize.is_dragging());
        assert_eq!(resize.crop.unwrap().width, 121);

        let mut new_crop = Editor::new(Rect::full(400, 300), None);
        new_crop.pointer_down((200, 200), 4.);
        let left = local_target(&new_crop, "left", (200, 200));
        new_crop.pointer_move(left);
        let up = local_target(&new_crop, "up", left);
        new_crop.pointer_move(up);
        assert!(new_crop.is_changing_crop());
        new_crop.pointer_up(up);
        assert_eq!(
            new_crop.crop,
            Some(Rect {
                x: 199,
                y: 199,
                width: 1,
                height: 1,
            })
        );
        assert!(!new_crop.is_dragging());
        assert!(!new_crop.document.can_undo());
    }

    #[test]
    fn crop_arrow_cancel_and_return_keep_mask_and_undo_order() {
        let mut editor = editor();
        editor.document.set_crop_mask(Some(mask()));
        let before = editor.crop;
        let before_mask = editor.document.crop_mask.clone();
        let objects = editor.document.items().to_vec();
        let revision = editor.document.revision();
        editor.pointer_down((160, 130), 4.);
        editor.pointer_move(local_target(&editor, "right", (160, 130)));
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert_eq!(editor.document.revision(), revision);

        editor.pointer_down((160, 130), 4.);
        let right = local_target(&editor, "right", (160, 130));
        editor.pointer_move(right);
        editor.pointer_up(local_target(&editor, "left", right));
        assert_eq!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert_eq!(editor.document.revision(), revision);
        editor.undo();
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.items(), objects);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items(), objects);
        editor.redo();
        assert_eq!(editor.document.crop_mask, before_mask);
        assert!(!editor.document.can_redo());
    }

    #[test]
    fn claimed_crop_arrow_rejection_and_replay_do_not_enter_old_crop_setter() {
        let mut editor = editor();
        editor.document.set_crop_mask(Some(mask()));
        let before = editor.crop;
        let before_mask = editor.document.crop_mask.clone();
        let objects = editor.document.items().to_vec();
        let revision = editor.document.revision();
        editor.pointer_down((160, 130), 4.);
        for (ready, replayed) in [(false, false), (false, true), (true, true)] {
            assert_eq!(
                crop_drag_arrow_route(&editor, true, true, &key("right", false), ready, replayed),
                CropDragArrowRoute::Consume
            );
            assert!(editor.is_changing_crop());
            assert_eq!(editor.crop, before);
            assert_eq!(editor.document.crop_mask, before_mask);
            assert_eq!(editor.document.items(), objects);
            assert_eq!(editor.document.revision(), revision);
        }
        assert_eq!(
            crop_drag_arrow_route(&editor, false, true, &key("right", false), true, false),
            CropDragArrowRoute::Pass
        );
        assert_eq!(
            crop_drag_arrow_route(&editor, true, false, &key("right", false), true, false),
            CropDragArrowRoute::Pass
        );
        for name in ["w", "a", "s", "d"] {
            assert_eq!(
                crop_drag_arrow_route(&editor, true, true, &key(name, false), true, false),
                CropDragArrowRoute::Pass
            );
        }
        for modifiers in [
            Modifiers {
                shift: true,
                ..Default::default()
            },
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
            let mut modified = key("right", false);
            modified.keystroke.modifiers = modifiers;
            assert_eq!(
                crop_drag_arrow_route(&editor, true, true, &modified, true, false),
                CropDragArrowRoute::Pass
            );
        }
        editor.pointer_up((160, 130));
        assert!(!editor.is_dragging());
        assert_eq!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert_eq!(editor.document.revision(), revision);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeArrowRoute {
    Pass,
    ConsumeUp,
    Move((i32, i32)),
}

/// 外层 FIFO 的局部领取状态；accepted 只负责对应 Up，不成为新绘图/重放状态。
#[derive(Clone, Default)]
pub(crate) struct NativeArrowNudge {
    nudge: PreviewCursorNudge,
    accepted: u8,
    repeatable: u8,
    idle: IdlePreviewCursorKeys,
}
impl NativeArrowNudge {
    pub(crate) fn clear_idle(&mut self) {
        self.idle.cancel();
    }
    pub(crate) fn idle_snapshot(&mut self) -> IdlePreviewCursorSnapshot {
        self.idle.snapshot(&self.nudge)
    }
    pub(crate) fn record_idle_commit(
        &mut self,
        raw: (u32, usize, isize),
        modifiers: gpui::Modifiers,
        commit: IdlePreviewCursorCommit,
    ) {
        let Some((key, _)) = Self::key(raw.1) else {
            return;
        };
        if !Self::movement_down(raw, modifiers) {
            self.clear_idle();
            return;
        }
        self.idle.record_commit(
            &KeyDownEvent {
                keystroke: gpui::Keystroke {
                    key: key.into(),
                    key_char: None,
                    modifiers,
                },
                is_held: raw.2 & (1 << 30) != 0,
            },
            commit,
        );
    }
    pub(crate) fn adopt_idle(
        &mut self,
        snapshot: IdlePreviewCursorSnapshot,
        physical_held: [bool; 8],
    ) -> bool {
        self.clear_idle();
        let bits = snapshot.verified & physical_movement_mask(physical_held);
        if bits == 0 {
            return false;
        }
        self.accepted |= bits;
        self.repeatable = bits;
        self.nudge = snapshot.nudge;
        true
    }
    pub(crate) fn reset_repeat(&mut self) {
        self.nudge.reset();
    }
    pub(crate) fn input_lost(&mut self) {
        self.reset_repeat();
        self.repeatable = 0;
        self.clear_idle();
    }
    pub(crate) fn loses_input(raw: (u32, usize, isize)) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            WA_INACTIVE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CANCELMODE, WM_KILLFOCUS,
        };
        matches!(raw.0, WM_KILLFOCUS | WM_CANCELMODE)
            || raw.0 == WM_ACTIVATE && raw.1 & 0xffff == WA_INACTIVE as usize
            || raw.0 == WM_ACTIVATEAPP && raw.1 == 0
    }
    fn key(raw: usize) -> Option<(&'static str, u8)> {
        match raw {
            0x25 => Some(("left", 1)),
            0x26 => Some(("up", 2)),
            0x27 => Some(("right", 4)),
            0x28 => Some(("down", 8)),
            0x41 => Some(("a", 16)),
            0x57 => Some(("w", 32)),
            0x44 => Some(("d", 64)),
            0x53 => Some(("s", 128)),
            _ => None,
        }
    }
    pub(crate) fn movement_down(raw: (u32, usize, isize), modifiers: gpui::Modifiers) -> bool {
        raw.0 == windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN
            && Self::key(raw.1).is_some()
            && preview_movement_modifiers(modifiers)
    }
    pub(crate) fn shift_transition(raw: (u32, usize, isize), modifiers: gpui::Modifiers) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_KEYUP};
        matches!(raw.0, WM_KEYDOWN | WM_KEYUP)
            && matches!(raw.1, 0x10 | 0xa0 | 0xa1)
            && preview_movement_modifiers(modifiers)
    }
    pub(crate) fn begin_physical_down(&mut self, raw: (u32, usize, isize)) {
        use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_SYSKEYDOWN};
        if matches!(raw.0, WM_KEYDOWN | WM_SYSKEYDOWN) && raw.2 & (1 << 30) == 0 {
            if let Some((_, bit)) = Self::key(raw.1) {
                // 先提交新物理周期，后续资格复核失败也不能恢复漏失 Up 的旧领取。
                self.accepted &= !bit;
                self.repeatable &= !bit;
                self.idle.verified &= !bit;
            }
        }
    }
    /// 调用方先过期新物理周期；这仅保留已领取的 held Down，不赋予移动资格。
    pub(crate) fn owns_claimed_down(
        &self,
        raw: (u32, usize, isize),
        modifiers: gpui::Modifiers,
    ) -> bool {
        Self::movement_down(raw, modifiers)
            && raw.2 & (1 << 30) != 0
            && Self::key(raw.1).is_some_and(|(_, bit)| self.accepted & bit != 0)
    }
    /// 已接受手势的拒绝/交接路径只消费键周期，禁止之后的 held 获得迟到位移。
    pub(crate) fn claim_manual_down(
        &mut self,
        raw: (u32, usize, isize),
        modifiers: gpui::Modifiers,
    ) -> bool {
        self.begin_physical_down(raw);
        if !Self::movement_down(raw, modifiers) {
            return false;
        }
        let (_, bit) = Self::key(raw.1).unwrap();
        self.accepted |= bit;
        self.repeatable &= !bit;
        self.idle.verified &= !bit;
        self.reset_repeat();
        true
    }
    pub(crate) fn route(
        &mut self,
        raw: (u32, usize, isize),
        modifiers: gpui::Modifiers,
        eligible: bool,
    ) -> NativeArrowRoute {
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP,
        };
        self.begin_physical_down(raw);
        let Some((key, bit)) = Self::key(raw.1)
            .filter(|_| matches!(raw.0, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP))
        else {
            if Self::loses_input(raw) {
                self.input_lost();
            }
            // 真正的鼠标移动（包括 OS 位移产生的消息）和绘制不打断按住方向键。
            if !matches!(raw.0, WM_MOUSEMOVE | WM_PAINT) && !Self::shift_transition(raw, modifiers)
            {
                self.reset_repeat();
                self.clear_idle();
            }
            return NativeArrowRoute::Pass;
        };
        if matches!(raw.0, WM_KEYUP | WM_SYSKEYUP) {
            self.reset_repeat();
            let accepted = self.accepted & bit != 0;
            self.accepted &= !bit;
            self.repeatable &= !bit;
            self.idle.verified &= !bit;
            return if accepted {
                NativeArrowRoute::ConsumeUp
            } else {
                NativeArrowRoute::Pass
            };
        }
        let held = raw.2 & (1 << 30) != 0;
        if raw.0 != WM_KEYDOWN
            || !eligible
            || !preview_movement_modifiers(modifiers)
            || held && self.repeatable & bit == 0
        {
            self.reset_repeat();
            self.idle.verified &= !bit;
            self.idle.modifiers_changed(modifiers);
            return NativeArrowRoute::Pass;
        }
        let event = KeyDownEvent {
            keystroke: gpui::Keystroke {
                key: key.into(),
                key_char: None,
                modifiers,
            },
            is_held: held,
        };
        let Some(delta) = self.nudge.down(&event, true, false) else {
            return NativeArrowRoute::Pass;
        };
        self.accepted |= bit;
        self.repeatable |= bit;
        NativeArrowRoute::Move(delta)
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        geometry::Rect,
    };
    use gpui::Modifiers;
    use windows::Win32::UI::WindowsAndMessaging::*;

    #[test]
    fn fifo_repeats_are_live_once_and_only_accepted_arrow_release_is_consumed() {
        let mut state = NativeArrowNudge::default();
        let m = Modifiers::default();
        assert_eq!(
            state.route((WM_KEYDOWN, 0x27, 1 << 30), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x27, 0), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x27, 0), m, true),
            NativeArrowRoute::Move((1, 0))
        );
        for step in [1, 2, 2, 3] {
            assert_eq!(
                state.route((WM_KEYDOWN, 0x27, 1 << 30), m, true),
                NativeArrowRoute::Move((step, 0))
            );
        }
        assert_eq!(
            state.route((WM_KEYUP, 0x27, 0), m, false),
            NativeArrowRoute::ConsumeUp
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x27, 0), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x25, 0), m, false),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x25, 1 << 30), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x25, 0), m, false),
            NativeArrowRoute::Pass
        );
        // 同方向别名共享加速方向，但对应 Up 的领取必须按八个物理键独立保留。
        for (arrow, alias, delta) in [
            (0x25, 0x41, (-1, 0)),
            (0x26, 0x57, (0, -1)),
            (0x27, 0x44, (1, 0)),
            (0x28, 0x53, (0, 1)),
        ] {
            let mut state = NativeArrowNudge::default();
            assert_eq!(
                state.route((WM_KEYDOWN, arrow, 0), m, true),
                NativeArrowRoute::Move(delta)
            );
            assert_eq!(
                state.route((WM_KEYUP, alias, 0), m, false),
                NativeArrowRoute::Pass
            );
            assert_eq!(
                state.route((WM_KEYDOWN, arrow, 1 << 30), m, true),
                NativeArrowRoute::Move(delta)
            );
            assert_eq!(
                state.route((WM_KEYDOWN, alias, 1 << 30), m, true),
                NativeArrowRoute::Pass
            );
            assert_eq!(
                state.route((WM_KEYDOWN, alias, 0), m, true),
                NativeArrowRoute::Move(delta)
            );
            assert_eq!(
                state.route((WM_KEYUP, arrow, 0), m, false),
                NativeArrowRoute::ConsumeUp
            );
            assert_eq!(
                state.route((WM_KEYDOWN, alias, 1 << 30), m, true),
                NativeArrowRoute::Move(delta)
            );
            assert_eq!(
                state.route((WM_KEYUP, alias, 0), m, false),
                NativeArrowRoute::ConsumeUp
            );
            assert_eq!(
                state.route((WM_KEYUP, arrow, 0), m, false),
                NativeArrowRoute::Pass
            );
        }
    }

    #[test]
    fn handoff_drain_focus_and_chords_preserve_release_but_never_generate_late_movement() {
        let mut state = NativeArrowNudge::default();
        let m = Modifiers::default();
        state.route((WM_KEYDOWN, 0x26, 0), m, true);
        for boundary in [
            (WM_KILLFOCUS, 0, 0),
            (WM_KEYDOWN, 0x10, 0),
            (WM_KEYDOWN, 0x41, 0),
        ] {
            assert_eq!(state.route(boundary, m, false), NativeArrowRoute::Pass);
        }
        // 失焦后的 held 不能继承旧 Down；对应旧 Up 仍须收尾。
        assert_eq!(
            state.route((WM_KEYDOWN, 0x26, 1 << 30), m, true),
            NativeArrowRoute::Pass
        );
        // targetSome、finish drain 或无 token/IME/前台的接入方统一传 false。
        assert_eq!(
            state.route((WM_KEYDOWN, 0x26, 1 << 30), m, false),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x26, 0), m, false),
            NativeArrowRoute::ConsumeUp
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x28, 0), Modifiers { alt: true, ..m }, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_SYSKEYDOWN, 0x28, 0), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x28, 0), m, false),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x28, 0), m, true),
            NativeArrowRoute::Move((0, 1))
        );
        // 旧 Down 已领取但 Up 漏失；新周期被拒绝时，只过期该方向旧领取位。
        for (key, delta) in [
            (0x26, (0, -1)),
            (0x57, (0, -1)),
            (0x41, (-1, 0)),
            (0x44, (1, 0)),
            (0x53, (0, 1)),
        ] {
            for new_down in [WM_KEYDOWN, WM_SYSKEYDOWN] {
                for focus_lost in [false, true] {
                    let mut state = NativeArrowNudge::default();
                    assert_eq!(
                        state.route((WM_KEYDOWN, key, 0), m, true),
                        NativeArrowRoute::Move(delta)
                    );
                    assert_eq!(
                        state.route((WM_KEYDOWN, 0x25, 0), m, true),
                        NativeArrowRoute::Move((-1, 0))
                    );
                    if focus_lost {
                        assert_eq!(
                            state.route((WM_KILLFOCUS, 0, 0), m, false),
                            NativeArrowRoute::Pass
                        );
                    }
                    assert_eq!(
                        state.route((new_down, key, 0), m, new_down == WM_SYSKEYDOWN),
                        NativeArrowRoute::Pass
                    );
                    assert_eq!(
                        state.route((WM_KEYDOWN, key, 1 << 30), m, true),
                        NativeArrowRoute::Pass
                    );
                    assert_eq!(
                        state.route((WM_SYSKEYUP, key, 0), m, false),
                        NativeArrowRoute::Pass
                    );
                    assert_eq!(
                        state.route((WM_KEYUP, 0x25, 0), m, false),
                        NativeArrowRoute::ConsumeUp
                    );
                    assert_eq!(
                        state.route((WM_KEYDOWN, key, 0), m, true),
                        NativeArrowRoute::Move(delta)
                    );
                    assert_eq!(
                        state.route((WM_KEYUP, key, 0), m, false),
                        NativeArrowRoute::ConsumeUp
                    );
                }
            }
        }
    }

    #[test]
    fn edit_and_permission_gates_reject_intents_without_changing_document_or_history() {
        let mut editor = Editor::new(Rect::full(400, 300), None);
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 40., y: 30. }, Style::default());
        object.update(Point { x: 80., y: 70. });
        let id = editor.document.insert(object);
        let items = editor.document.items().to_vec();
        let mut state = NativeArrowNudge::default();
        for (owner, busy, input) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let gate = preview_ready(&editor, true, owner, busy, input);
            for raw_key in [0x27, 0x57, 0x41, 0x53, 0x44] {
                assert_eq!(
                    state.route((WM_KEYDOWN, raw_key, 0), Modifiers::default(), gate),
                    NativeArrowRoute::Pass
                );
            }
        }
        editor.crop = Some(Rect::full(100, 80));
        assert!(!preview_ready(&editor, true, true, false, false));
        editor.crop = None;
        editor.selected = Some(id);
        assert!(!preview_ready(&editor, true, true, false, false));
        editor.selected = None;
        assert!(preview_ready(&editor, true, true, false, false));
        assert_eq!(editor.document.items(), items);
        editor.undo();
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items(), items);
    }

    #[test]
    fn signed_native_intents_and_failed_attempts_do_not_turn_into_replayed_offsets() {
        let m = Modifiers::default();
        let mut state = NativeArrowNudge::default();
        let bounds = ScreenRect {
            left: -1920,
            top: -200,
            width: 3840,
            height: 1280,
        };
        let NativeArrowRoute::Move(delta) = state.route((WM_KEYDOWN, 0x25, 0), m, true) else {
            panic!("expected live intent")
        };
        assert_eq!(
            target_in_client(bounds, (-1910, -100), delta),
            Some((-1911, -100))
        );
        state.reset_repeat(); // 已尝试 OS 移动失败，只清加速，accepted Up 仍需收尾。
        assert_eq!(
            state.route((WM_KEYUP, 0x25, 0), m, false),
            NativeArrowRoute::ConsumeUp
        );
        assert_eq!(
            state.route((WM_KEYDOWN, 0x27, 0), m, false),
            NativeArrowRoute::Pass
        );
        // 真实路线先提交周期过期，再创建暂定 intent；最终资格失败丢弃该 intent。
        assert_eq!(
            state.route((WM_KEYDOWN, 0x25, 0), m, true),
            NativeArrowRoute::Move((-1, 0))
        );
        let next_down = (WM_KEYDOWN, 0x25, 0);
        state.begin_physical_down(next_down);
        let mut tentative = state.clone();
        assert_eq!(
            tentative.route(next_down, m, true),
            NativeArrowRoute::Move((-1, 0))
        );
        // 对应最终点为 None / Err / 越界的拒绝分支，仅清真实状态加速、不提交 tentative。
        state.reset_repeat();
        assert_eq!(
            state.route((WM_KEYDOWN, 0x25, 1 << 30), m, true),
            NativeArrowRoute::Pass
        );
        assert_eq!(
            state.route((WM_KEYUP, 0x25, 0), m, false),
            NativeArrowRoute::Pass
        );
        assert_eq!(target_in_client(bounds, (-1921, 0), (1, 0)), None);
        let extreme = ScreenRect {
            left: i32::MAX - 1,
            top: 0,
            width: 2,
            height: 2,
        };
        assert_eq!(target_in_client(extreme, (i32::MAX, 0), (1, 0)), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        geometry::Rect,
    };
    use gpui::{Keystroke, Modifiers};

    fn key(key: &str, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: Modifiers::default(),
            },
            is_held: held,
        }
    }
    fn editor() -> Editor {
        Editor::new(Rect::full(400, 300), None)
    }
    fn ready(editor: &Editor) -> bool {
        preview_ready(editor, true, true, false, false)
    }
    fn object() -> Annotation {
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 40., y: 30. }, Style::default());
        object.update(Point { x: 80., y: 70. });
        object
    }

    #[test]
    fn first_repeat_direction_and_release_follow_one_to_twenty_pixels() {
        let mut state = PreviewCursorNudge::default();
        assert_eq!(state.down(&key("right", false), true, false), Some((1, 0)));
        for step in [1, 2, 2, 3, 3] {
            // Capture 和 View 两次观察不能把一个真实 Down 累计两遍。
            state.observe_key_down(&key("right", true), true, false);
            assert_eq!(
                state.down(&key("right", true), true, false),
                Some((step, 0))
            );
        }
        for _ in 0..300 {
            state.down(&key("right", true), true, false);
        }
        assert_eq!(state.down(&key("right", true), true, false), Some((20, 0)));
        assert_eq!(state.down(&key("up", true), true, false), Some((0, -1)));
        assert_eq!(state.down(&key("up", true), true, false), Some((0, -1)));
        state.key_up(&KeyUpEvent {
            keystroke: key("up", false).keystroke,
        });
        assert_eq!(state.down(&key("up", true), true, false), Some((0, -1)));
        assert_eq!(state.down(&key("left", false), true, false), Some((-1, 0)));
        for (alias, arrow, delta) in [
            ("a", "left", (-1, 0)),
            ("w", "up", (0, -1)),
            ("d", "right", (1, 0)),
            ("s", "down", (0, 1)),
        ] {
            let mut state = PreviewCursorNudge::default();
            assert_eq!(state.down(&key(alias, false), true, false), Some(delta));
            assert_eq!(state.down(&key(alias, true), true, false), Some(delta));
            assert_eq!(
                state.down(&key(arrow, true), true, false),
                Some((delta.0 * 2, delta.1 * 2))
            );
            state.key_up(&KeyUpEvent {
                keystroke: key(alias, false).keystroke,
            });
            assert_eq!(state.down(&key(arrow, true), true, false), Some(delta));
        }
    }

    #[test]
    fn editor_host_guards_keep_crop_objects_text_and_busy_routes_owned() {
        let mut editor = editor();
        assert!(ready(&editor));
        assert!(!preview_ready(&editor, false, true, false, false));
        assert!(!preview_ready(&editor, true, false, false, false));
        assert!(!preview_ready(&editor, true, true, true, false));
        assert!(!preview_ready(&editor, true, true, false, true));
        editor.set_crop(Some(Rect::full(100, 80)));
        assert!(!ready(&editor));
        for alias in ["w", "a", "s", "d"] {
            let mut state = PreviewCursorNudge::default();
            assert_eq!(state.down(&key(alias, false), ready(&editor), false), None);
            assert!(!claims_preview_wasd(&editor, true, &key(alias, false)));
            for (ordinary, owner, busy, input) in [
                (false, true, false, false),
                (true, false, false, false),
                (true, true, true, false),
                (true, true, false, true),
            ] {
                let preview = Editor::new(Rect::full(400, 300), None);
                assert_eq!(
                    claims_preview_wasd(&preview, ordinary, &key(alias, false)),
                    ordinary
                );
                assert_eq!(
                    state.down(
                        &key(alias, false),
                        preview_ready(&preview, ordinary, owner, busy, input),
                        false
                    ),
                    None
                );
            }
            for modifiers in [
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                Modifiers {
                    function: true,
                    ..Modifiers::default()
                },
            ] {
                let mut modified = key(alias, false);
                modified.keystroke.modifiers = modifiers;
                assert_eq!(state.down(&modified, true, false), None);
                let preview = Editor::new(Rect::full(400, 300), None);
                assert!(!claims_preview_wasd(&preview, true, &modified));
            }
        }
        editor.set_crop(None);
        let id = editor.document.insert(object());
        assert!(ready(&editor)); // 未选中的标注不随鼠标微调移动。
        editor.selected = Some(id);
        assert!(!ready(&editor));
        editor.selected = None;
        editor.crop = Some(Rect::full(400, 300));
        editor.select_tool(Tool::Arrow);
        editor.pointer_down((200, 200), 3.);
        editor.crop = None;
        assert!(!ready(&editor));
        editor.pointer_up((200, 200));
        assert!(editor.has_pending_drawing());
        assert!(!ready(&editor));
        editor.cancel_gesture();
        editor.crop = Some(Rect::full(400, 300));
        editor.select_tool(Tool::Text);
        editor.pointer_down((120, 100), 3.);
        editor.pointer_up((120, 100));
        editor.crop = None;
        assert!(editor.text_draft.is_some());
        assert!(!ready(&editor));
    }

    #[test]
    fn physical_intents_use_signed_screen_coordinates_and_reject_overflow() {
        let bounds = ScreenRect {
            left: -1920,
            top: -200,
            width: 3840,
            height: 1280,
        };
        assert_eq!(
            target_in_client(bounds, (-1900, -100), (-20, 1)),
            Some((-1920, -99))
        );
        assert_eq!(
            target_in_client(bounds, (-1920, -200), (-1, -1)),
            Some((-1920, -200))
        );
        assert_eq!(
            target_in_client(bounds, (1919, 1079), (20, 20)),
            Some((1919, 1079))
        );
        assert_eq!(target_in_client(bounds, (1920, 100), (-1, 0)), None);
        assert_eq!(
            target_in_client(ScreenRect { width: 0, ..bounds }, (0, 0), (1, 0)),
            None
        );
        let extreme = ScreenRect {
            left: i32::MAX - 1,
            top: i32::MIN,
            width: 2,
            height: 2,
        };
        assert_eq!(
            target_in_client(extreme, (i32::MAX, i32::MIN), (1, 0)),
            None
        );
        assert_eq!(
            target_in_client(extreme, (i32::MAX - 1, i32::MIN), (0, -1)),
            None
        );
        assert_eq!(
            target_in_client(
                ScreenRect {
                    width: 3,
                    ..extreme
                },
                (i32::MAX, i32::MIN),
                (0, 0)
            ),
            None
        );
    }

    #[test]
    fn replayed_arrows_and_failed_window_ownership_never_generate_live_intents() {
        let mut state = PreviewCursorNudge::default();
        for _ in 0..8 {
            state.down(&key("down", true), true, false);
        }
        assert_eq!(state.down(&key("down", false), true, true), None);
        assert_eq!(state.down(&key("down", true), true, false), Some((0, 1)));
        assert_eq!(state.down(&key("down", true), false, false), None);
        assert_eq!(state.down(&key("down", true), true, false), Some((0, 1)));
        state.reset(); // 真实 OS API/前台资格失败由接入层调用，模型不运行 OS API。
        assert_eq!(state.down(&key("down", true), true, false), Some((0, 1)));
        for queued_key in ["left", "right", "up", "down", "w", "a", "s", "d"] {
            // handoff 使用同一成员 helper；旧队列无论首按或 held 都不得重新产生当前位移。
            let replayed = is_preview_movement_key(queued_key);
            assert!(replayed);
            for held in [false, true] {
                assert_eq!(state.down(&key(queued_key, held), true, replayed), None);
                assert_eq!(
                    claims_preview_wasd(&editor(), true, &key(queued_key, held)),
                    matches!(queued_key, "w" | "a" | "s" | "d")
                );
            }
        }
        assert!(!is_preview_movement_key("v"));
    }

    #[test]
    fn event_boundaries_reset_repeat_while_document_history_remains_unchanged() {
        let mut editor = editor();
        editor.document.insert(object());
        let objects = editor.document.items().to_vec();
        let stamp = editor.scene_stamp();
        let mut state = PreviewCursorNudge::default();
        for _ in 0..6 {
            state.down(&key("right", true), ready(&editor), false);
        }
        state.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        assert_eq!(
            state.down(&key("right", true), ready(&editor), false),
            Some((1, 0))
        );
        state.observe_key_down(&key("v", false), ready(&editor), false);
        assert_eq!(
            state.down(&key("right", true), ready(&editor), false),
            Some((1, 0))
        );
        let mut modified = key("right", true);
        modified.keystroke.modifiers.alt = true;
        assert_eq!(state.down(&modified, ready(&editor), false), None);
        assert_eq!(
            state.down(&key("right", true), ready(&editor), false),
            Some((1, 0))
        );
        state.reset(); // 与失活/其它 Action 使用相同重置入口。
        assert_eq!(editor.crop, None);
        assert_eq!(editor.document.items(), objects);
        assert_eq!(editor.scene_stamp(), stamp);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items(), objects);
    }
}

#[cfg(test)]
mod preview_shift_cursor_tests {
    use super::*;
    use crate::annotation::Tool;
    use gpui::{Keystroke, Modifiers};
    use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN};

    fn key(name: &str, held: bool, shift: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: name.into(),
                key_char: None,
                modifiers: Modifiers {
                    shift,
                    ..Modifiers::default()
                },
            },
            is_held: held,
        }
    }

    #[test]
    fn bare_and_shift_eight_keys_share_acceleration_across_shift_edges() {
        for (name, direction) in [
            ("left", (-1, 0)),
            ("a", (-1, 0)),
            ("right", (1, 0)),
            ("d", (1, 0)),
            ("up", (0, -1)),
            ("w", (0, -1)),
            ("down", (0, 1)),
            ("s", (0, 1)),
        ] {
            let mut nudge = PreviewCursorNudge::default();
            for repeat in 0..45 {
                let shift = repeat % 4 < 2;
                nudge.modifiers_changed(Modifiers {
                    shift,
                    ..Modifiers::default()
                });
                let step = (1 + repeat / 2).min(20);
                assert_eq!(
                    nudge.down(&key(name, repeat != 0, shift), true, false),
                    Some((direction.0 * step, direction.1 * step))
                );
            }
            nudge.key_up(&KeyUpEvent {
                keystroke: key(name, false, false).keystroke,
            });
            assert_eq!(
                nudge.down(&key(name, false, true), true, false),
                Some(direction)
            );
        }
    }

    #[test]
    fn forbidden_chords_preserve_shortcuts_and_reset_only_preview_acceleration() {
        let editor = Editor::new(Rect::full(400, 300), None);
        for bits in 0..32 {
            let modifiers = Modifiers {
                shift: bits & 1 != 0,
                control: bits & 2 != 0,
                alt: bits & 4 != 0,
                platform: bits & 8 != 0,
                function: bits & 16 != 0,
            };
            let allowed = bits < 2;
            for name in ["left", "up", "right", "down", "w", "a", "s", "d"] {
                let mut event = key(name, false, false);
                event.keystroke.modifiers = modifiers;
                assert_eq!(claims_manual_preview_cursor(true, &event), allowed);
                assert_eq!(
                    claims_preview_wasd(&editor, true, &event),
                    allowed && matches!(name, "w" | "a" | "s" | "d")
                );
                assert!(!claims_preview_wasd(&editor, false, &event));
                let mut nudge = PreviewCursorNudge::default();
                assert_eq!(nudge.down(&event, true, false).is_some(), allowed);
                assert_eq!(nudge.down(&event, true, true), None);
            }
            assert_eq!(
                NativeArrowNudge::movement_down((WM_KEYDOWN, 0x25, 0), modifiers),
                allowed
            );
            assert!(!NativeArrowNudge::movement_down(
                (WM_SYSKEYDOWN, 0x25, 0),
                modifiers
            ));
            if !allowed {
                let mut nudge = PreviewCursorNudge::default();
                nudge.down(&key("right", false, true), true, false);
                for _ in 0..8 {
                    nudge.down(&key("right", true, true), true, false);
                }
                nudge.modifiers_changed(modifiers);
                assert_eq!(
                    nudge.down(&key("right", true, true), true, false),
                    Some((1, 0))
                );
            }
        }
    }

    #[test]
    fn shift_manual_key_cycles_keep_release_and_reject_unknown_or_cancelled_repeats() {
        for name in ["left", "up", "right", "down", "a", "w", "d", "s"] {
            let mut keys = ManualPreviewCursorKeys::default();
            let unknown = key(name, true, true);
            assert!(keys.claim_down(&unknown, true));
            assert!(!keys.allows_move(&unknown));
            assert!(keys.claim_down(&key(name, true, false), true));
            assert!(!keys.allows_move(&unknown));
            let down = key(name, false, true);
            assert!(keys.claim_down(&down, true));
            assert!(keys.allows_move(&down));
            let held = key(name, true, false);
            assert!(keys.claim_down(&held, true));
            assert!(keys.allows_move(&held));
            keys.cancel_gesture();
            assert!(keys.claim_down(&unknown, false));
            assert!(!keys.allows_move(&unknown));
            assert!(keys.claim_down(&unknown, true));
            assert!(!keys.allows_move(&unknown));
            keys.key_up(&KeyUpEvent {
                keystroke: key(name, false, false).keystroke,
            });
            assert!(!keys.claim_down(&unknown, false));
            assert!(keys.claim_down(&down, true));
            assert!(keys.allows_move(&down));
            keys.cancel_gesture();
            assert!(!keys.claim_down(&key(name, false, false), false));
        }
    }

    #[test]
    fn native_shift_edges_preserve_acceleration_and_release_each_physical_key() {
        for (virtual_key, direction) in [
            (0x25, (-1, 0)),
            (0x41, (-1, 0)),
            (0x27, (1, 0)),
            (0x44, (1, 0)),
            (0x26, (0, -1)),
            (0x57, (0, -1)),
            (0x28, (0, 1)),
            (0x53, (0, 1)),
        ] {
            let mut native = NativeArrowNudge::default();
            let shift = Modifiers {
                shift: true,
                ..Modifiers::default()
            };
            assert_eq!(
                native.route((WM_KEYDOWN, virtual_key, 0), shift, true),
                NativeArrowRoute::Move(direction)
            );
            for repeat in 1..6 {
                let modifiers = if repeat % 2 == 0 {
                    shift
                } else {
                    Modifiers::default()
                };
                for shift_key in [0x10, 0xa0, 0xa1] {
                    let edge = (
                        if modifiers.shift {
                            WM_KEYDOWN
                        } else {
                            WM_KEYUP
                        },
                        shift_key,
                        0,
                    );
                    assert!(NativeArrowNudge::shift_transition(edge, modifiers));
                    assert_eq!(native.route(edge, modifiers, true), NativeArrowRoute::Pass);
                }
                let step = 1 + repeat / 2;
                assert_eq!(
                    native.route((WM_KEYDOWN, virtual_key, 1 << 30), modifiers, true),
                    NativeArrowRoute::Move((direction.0 * step, direction.1 * step))
                );
            }
            native.input_lost();
            let held = (WM_KEYDOWN, virtual_key, 1 << 30);
            assert_eq!(native.route(held, shift, true), NativeArrowRoute::Pass);
            assert!(native.claim_manual_down(held, shift));
            assert_eq!(
                native.route(held, Modifiers::default(), true),
                NativeArrowRoute::Pass
            );
            assert_eq!(
                native.route((WM_KEYUP, virtual_key, 0), Modifiers::default(), false),
                NativeArrowRoute::ConsumeUp
            );
            assert_eq!(
                native.route((WM_KEYUP, virtual_key, 0), shift, false),
                NativeArrowRoute::Pass
            );
            assert!(!NativeArrowNudge::shift_transition(
                (WM_SYSKEYDOWN, 0x10, 0),
                shift
            ));
        }
    }

    #[test]
    fn manual_source_shift_crop_keeps_real_rectangle_through_final_up() {
        for (ordinary, source) in [(true, true), (true, false), (false, true)] {
            let mut editor = Editor::new(Rect::full(400, 300), None);
            editor.pointer_down((40, 50), 4.);
            let actual = preview_crop_pointer(&editor, (80, 60), true, ordinary, source);
            editor.pointer_move(actual);
            let actual_up = preview_crop_pointer(&editor, (83, 62), true, ordinary, source);
            editor.pointer_move(actual_up);
            editor.pointer_up(actual_up);
            let crop = editor.crop.unwrap();
            if ordinary && source {
                assert_eq!(
                    crop,
                    Rect {
                        x: 40,
                        y: 50,
                        width: 43,
                        height: 12
                    }
                );
            } else {
                assert_eq!(crop.width, crop.height);
            }
            assert!(!editor.is_dragging());
            assert!(!editor.document.can_undo());
        }
    }

    #[test]
    fn manual_source_marker_does_not_disable_annotation_shift_constraints() {
        let mut editor = Editor::new(Rect::full(400, 300), Some(Rect::full(400, 300)));
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((40, 50), 4.);
        assert!(!editor.is_changing_crop());
        let expected = editor.constrained_pointer((80, 60), true);
        assert_ne!(expected, (80, 60));
        let actual = preview_crop_pointer(&editor, (80, 60), true, true, true);
        assert_eq!(actual, expected);
        editor.pointer_move(actual);
        editor.pointer_up(actual);
        assert_eq!(editor.document.items().len(), 1);
        let object = &editor.document.items()[0];
        assert_eq!(
            object.annotation.bounds().width,
            object.annotation.bounds().height
        );
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items().len(), 1);
    }
}
