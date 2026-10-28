//! GPUI 接入层：转发输入、缓存绘制场景、处理复制/保存。
use crate::{
    annotation::Tool,
    canvas_source::CanvasSource,
    capture::Image,
    capture_cursor_nudge as cursor_nudge,
    editor::Editor,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    scene::{PixelPlane, Scene, SourcePixels},
    toolbar_groups,
    toolbar_layout::{ToolbarLayout, ToolbarPanel},
    Result,
};
use gpui::{
    AppContext, Context, FocusHandle, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    RenderImage, Window,
};
use std::{cell::Cell, rc::Rc, sync::Arc};
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture, VK_LSHIFT,
        VK_P, VK_RSHIFT, VK_SHIFT,
    },
    WindowsAndMessaging::{GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK},
};

#[path = "capture_edit_arrow.rs"]
mod edit_arrow;
#[path = "capture_guides.rs"]
mod guide_toggle;
#[path = "capture_image_translate.rs"]
mod image_translate_host;
#[path = "capture_quick_action.rs"]
mod quick_action_host;
#[path = "capture_refresh_host.rs"]
mod refresh_host;
#[path = "toolbar_drag_host.rs"]
mod toolbar_drag_host;
pub(crate) use guide_toggle::GuideToggleGesture;
#[path = "ocr_local_retry.rs"]
pub(crate) mod local_retry;
#[path = "capture_ocr_progress.rs"]
pub(crate) mod ocr_progress;
#[path = "capture_ocr_task.rs"]
pub(crate) mod ocr_task;
#[path = "capture_range_shortcuts.rs"]
mod range_shortcuts;
#[path = "capture_smart_mosaic.rs"]
mod smart_candidates;
#[path = "capture_text_result.rs"]
pub(crate) mod text_result;
#[path = "capture_toolbar_visibility.rs"]
mod toolbar_visibility;

pub(crate) fn text_copy_editor_is_idle(editor: &Editor) -> bool {
    !editor.is_dragging() && !editor.has_pending_drawing() && editor.text_draft.is_none()
}

/// 仅给原路由已拒绝的绘制模式 Ctrl+A 提示，不修改对象或选区。
fn drawing_select_all_feedback(editor: &Editor, event: &KeyDownEvent, ready: bool) -> bool {
    let m = event.keystroke.modifiers;
    ready
        && !event.is_held
        && event.keystroke.key == "a"
        && m.control
        && !m.shift
        && !m.alt
        && !m.platform
        && !m.function
        && editor.crop.is_some()
        && editor.tool.draws()
        && text_copy_editor_is_idle(editor)
        && !editor.watermark.has_preview()
        && editor.local_fill_candidate().is_none()
}

fn show_screenshot_toolbar(state: &mut toolbar_visibility::ToolbarVisibility, editor: &Editor) {
    // 两字段会话状态归显：立即观察已确认 crop，不伪造 Space，也不修改 Editor。
    *state = Default::default();
    state.observe_crop(editor);
}

#[cfg(test)]
mod screenshot_toolbar_menu_show_tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    #[test]
    fn show_is_idempotent_and_preserves_crop_mask_objects_and_undo() {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((130, 100), 3.);
        editor.pointer_up((160, 140));
        editor.select_tool(Tool::Edit);
        editor
            .document
            .set_crop_mask(crate::crop_mask::Polygon::new(&[
                crate::annotation::Point { x: 100., y: 80. },
                crate::annotation::Point { x: 220., y: 80. },
                crate::annotation::Point { x: 220., y: 180. },
                crate::annotation::Point { x: 100., y: 180. },
            ]));
        let crop = editor.crop;
        let mask = editor.document.crop_mask.clone();
        let items = editor.document.items().to_vec();
        let stamp = editor.scene_stamp();
        let mut state = toolbar_visibility::ToolbarVisibility::default();
        let space = KeyDownEvent {
            keystroke: Keystroke {
                key: "space".into(),
                key_char: None,
                modifiers: Modifiers::default(),
            },
            is_held: false,
        };
        assert!(state.space(&editor, &space, true, false, false, ToolbarPanel::None));
        assert!(!state.visible());
        show_screenshot_toolbar(&mut state, &editor);
        assert!(state.visible());
        show_screenshot_toolbar(&mut state, &editor);
        assert!(state.visible());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, mask);
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.scene_stamp(), stamp);
        assert_eq!(editor.tool, Tool::Edit);
        editor.undo();
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.items(), items);
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), items);
        assert!(editor.document.crop_mask.is_none());
        editor.redo();
        assert_eq!(editor.document.crop_mask, mask);
        assert_eq!(editor.document.items(), items);
    }
}

/// 只清理已取得 GPUI 所有权的普通截图失活，不借此提交或取消待续草稿。
fn cleanup_ordinary_activation(
    editor: &mut Editor,
    wheel: &mut crate::interaction::WheelAccumulator,
    ordinary: bool,
    owner_ready: bool,
    inactive: bool,
    foreground_owned: bool,
    accepted_down: bool,
) -> bool {
    if !ordinary || !owner_ready || !inactive || foreground_owned {
        return false;
    }
    let cancel_continuous = accepted_down && editor.is_dragging();
    wheel.reset();
    if cancel_continuous {
        // cancel_gesture 原合同会丢弃已完成套索；失活只撤销当前连续预览。
        let finished_lasso = editor.lasso_draft.take();
        editor.cancel_gesture();
        editor.lasso_draft = finished_lasso;
    }
    true
}

#[derive(Clone, Copy)]
pub enum Action {
    Tool(Tool),
    Undo,
    Redo,
    Delete,
    DeleteNode,
    ApplyLasso,
    ClearLasso,
    StraightenArrow,
    Clear,
    Current,
    All,
    Reselect,
    Refresh,
    FollowToolbar,
    Help,
    Confirm,
    Copy,
    Save,
    Cancel,
    Settings,
    History,
    Qr,
    PickColor,
    Magnifier,
    Guides,
    Cursor,
    LongCapture,
    Record,
    Pin,
    WindowPin,
    Ocr,
    TableOcr,
    /// 识别下拉中的一项；记为主按钮默认项后转为具体动作。
    Recognize(crate::toolbar_groups::RecognizeMode),
    TranslateImage,
    ControlText,
    SmartMosaic,
    SmartEffect(Tool),
    QuickSave,
    Duplicate,
    Group,
    Ungroup,
    EnterGroup,
    ExitGroup,
    RootGroup,
    Rotate(f32),
    ResetRotation,
    Front,
    Back,
    EditText,
    Theme,
    More,
    OpenImage,
    FitImage,
    ActualImage,
    ZoomImage(f32),
    Panel(ToolbarPanel),
}
pub(crate) struct TableInput {
    target: text_result::OcrTarget,
    recognized: crate::recognition::RecognizedText,
}
pub(crate) struct SmartEffectRequest {
    effect: Tool,
    style: crate::annotation::Style,
    selected: Rect,
    revision: u64,
    edit_revision: u64,
    stamp: (u64, u64),
    identity: crate::scene::SourceIdentity,
}
#[derive(Clone, Copy)]
pub(crate) struct ManualPreviewReplay {
    pub(crate) global_point: Option<(i32, i32)>,
    pub(crate) shown_preview: Option<Rect>,
}

/// 本 View 的移鼠来源；idle 实际提交或已接受调整 Down，不随配置序列化。
struct IdlePreviewCursorOwner {
    window: usize,
    thread: u32,
    bounds: crate::geometry::ScreenRect,
    source: crate::scene::SourceIdentity,
    session: crate::capture_lock::NativeCaptureCancelToken,
}

fn idle_preview_cursor_snapshot_ready(
    replayed: bool,
    click_count: usize,
    modifiers: gpui::Modifiers,
    preview_ready: bool,
) -> bool {
    !replayed
        && click_count == 1
        && cursor_nudge::preview_movement_modifiers(modifiers)
        && preview_ready
}

fn admitted_new_crop_cursor_origin(
    editor: &Editor,
    original_down: Option<(i32, i32)>,
    ordinary: bool,
    ready_before_down: bool,
) -> Option<(i32, i32)> {
    original_down.filter(|_| {
        ready_before_down && cursor_nudge::new_crop_cursor_source(editor, ordinary, true)
    })
}

fn admitted_crop_adjust_cursor_origin(
    editor: &Editor,
    original_down: Option<(i32, i32)>,
    ordinary: bool,
    ready_before_down: bool,
) -> Option<(i32, i32)> {
    original_down.filter(|_| {
        ready_before_down && cursor_nudge::crop_adjust_cursor_source(editor, ordinary, true)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LassoOutsideKeyAction {
    Pass,
    ConsumeEnter,
    CancelResize,
}

fn lasso_outside_key_action(editor: &Editor, key: &str, ordinary: bool) -> LassoOutsideKeyAction {
    if !ordinary || !editor.is_lasso_crop_outside_resize() {
        return LassoOutsideKeyAction::Pass;
    }
    match key {
        "enter" => LassoOutsideKeyAction::ConsumeEnter,
        "escape" => LassoOutsideKeyAction::CancelResize,
        _ => LassoOutsideKeyAction::Pass,
    }
}

#[cfg(test)]
mod lasso_outside_host_tests {
    use super::*;
    use crate::{annotation::Point, crop_mask::Polygon};

    fn crop() -> Rect {
        Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        }
    }
    fn closed() -> Polygon {
        Polygon::new(&[
            Point { x: 110., y: 90. },
            Point { x: 210., y: 90. },
            Point { x: 160., y: 170. },
        ])
        .unwrap()
    }
    fn idle(draft: bool) -> Editor {
        let mut editor = Editor::new(Rect::full(400, 300), Some(crop()));
        editor.select_tool(Tool::Lasso);
        editor.document.set_crop_mask(Some(closed()));
        editor.lasso_draft = draft.then(closed);
        editor
    }
    fn key(held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: gpui::Keystroke {
                key: "a".into(),
                key_char: None,
                modifiers: gpui::Modifiers::default(),
            },
            is_held: held,
        }
    }

    #[test]
    fn lasso_outside_enter_and_escape_require_real_ordinary_resize_and_preserve_active_draft() {
        for draft in [false, true] {
            let mut editor = idle(draft);
            let old_draft = editor.lasso_draft.clone();
            let old_mask = editor.document.crop_mask.clone();
            let revision = editor.document.revision();
            assert!(editor.begin_screenshot_crop_outside((60, 130), 4.));
            assert_eq!(editor.crop_creation_before(), None);
            for ordinary in [false, true] {
                assert_eq!(
                    lasso_outside_key_action(&editor, "enter", ordinary),
                    if ordinary {
                        LassoOutsideKeyAction::ConsumeEnter
                    } else {
                        LassoOutsideKeyAction::Pass
                    }
                );
                assert_eq!(
                    lasso_outside_key_action(&editor, "escape", ordinary),
                    if ordinary {
                        LassoOutsideKeyAction::CancelResize
                    } else {
                        LassoOutsideKeyAction::Pass
                    }
                );
            }
            assert_eq!(
                lasso_outside_key_action(&editor, "a", true),
                LassoOutsideKeyAction::Pass
            );
            assert!(!editor.apply_lasso());
            assert!(editor.is_lasso_crop_outside_resize());
            assert_eq!(editor.lasso_draft, old_draft);
            assert_eq!(editor.document.crop_mask, old_mask);
            assert_eq!(editor.document.revision(), revision);
        }
        for has_crop in [false, true] {
            let mut editor = Editor::new(Rect::full(400, 300), has_crop.then(crop));
            editor.select_tool(Tool::Lasso);
            // 通用 Pin/Image/fixed Preview 入口保持原 New，不冒认宿主 outside Resize。
            editor.pointer_down((60, 130), 4.);
            assert_eq!(editor.crop_creation_before(), Some(has_crop.then(crop)));
            assert_eq!(
                lasso_outside_key_action(&editor, "enter", true),
                LassoOutsideKeyAction::Pass
            );
        }
        let mut drawing = idle(false);
        drawing.pointer_down((160, 130), 4.);
        assert!(drawing.drawing_lasso());
        assert_eq!(
            lasso_outside_key_action(&drawing, "escape", true),
            LassoOutsideKeyAction::Pass
        );
        let mut edit = idle(false);
        edit.select_tool(Tool::Edit);
        assert!(edit.begin_screenshot_crop_outside((60, 130), 4.));
        assert_eq!(
            lasso_outside_key_action(&edit, "enter", true),
            LassoOutsideKeyAction::Pass
        );
    }

    #[test]
    fn lasso_outside_cancel_keeps_closed_draft_then_next_escape_discards_only_draft_and_old_keys() {
        let mut editor = idle(true);
        let old_draft = editor.lasso_draft.clone();
        let old_mask = editor.document.crop_mask.clone();
        let revision = editor.document.revision();
        assert!(editor.begin_screenshot_crop_outside((60, 130), 4.));
        editor.pointer_move((50, 130));
        let mut keys = cursor_nudge::ManualPreviewCursorKeys::default();
        let mut nudge = cursor_nudge::PreviewCursorNudge::default();
        let fresh = key(false);
        assert!(keys.claim_down(&fresh, true));
        assert_eq!(
            nudge.down(&fresh, keys.allows_move(&fresh), false),
            Some((-1, 0))
        );
        assert_eq!(
            lasso_outside_key_action(&editor, "escape", true),
            LassoOutsideKeyAction::CancelResize
        );
        assert!(editor.cancel_gesture());
        keys.cancel_gesture();
        nudge.reset();
        assert_eq!(editor.crop, Some(crop()));
        assert_eq!(editor.lasso_draft, old_draft);
        assert_eq!(editor.document.crop_mask, old_mask);
        assert_eq!(editor.document.revision(), revision);
        let held = key(true);
        assert!(keys.claim_down(&held, false));
        assert!(!keys.allows_move(&held));
        assert_eq!(nudge.down(&held, keys.allows_move(&held), false), None);
        keys.key_up(&gpui::KeyUpEvent {
            keystroke: held.keystroke.clone(),
        });
        assert!(!keys.claim_down(&held, false));
        assert_eq!(
            lasso_outside_key_action(&editor, "escape", true),
            LassoOutsideKeyAction::Pass
        );
        // 下一次真实 Esc 进入原 Lasso 取消合同，而非重复取消已结束的范围。
        assert!(editor.cancel_gesture());
        assert!(editor.lasso_draft.is_none());
        assert_eq!(editor.document.crop_mask, old_mask);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.tool, Tool::Lasso);
    }

    #[test]
    fn lasso_outside_closed_draft_origin_keeps_original_down_but_replay_and_unknown_held_do_not_move(
    ) {
        let mut editor = idle(true);
        let admitted =
            cursor_nudge::crop_adjust_cursor_admission(&editor, true, true, false, false);
        let down = (60, 130);
        assert!(admitted);
        assert!(editor.begin_screenshot_crop_outside(down, 4.));
        assert!(cursor_nudge::crop_adjust_cursor_ready(
            &editor, true, true, true, false, false
        ));
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, admitted),
            Some(down)
        );
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, false),
            None
        );
        assert!(!cursor_nudge::crop_adjust_cursor_ready(
            &editor, true, true, false, false, false
        ));
        editor.pointer_move((30, 130));
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, admitted),
            Some(down)
        );
        let mut keys = cursor_nudge::ManualPreviewCursorKeys::default();
        let mut nudge = cursor_nudge::PreviewCursorNudge::default();
        let held = key(true);
        assert!(keys.claim_down(&held, true));
        assert!(!keys.allows_move(&held));
        let fresh = key(false);
        assert!(keys.claim_down(&fresh, true));
        assert_eq!(nudge.down(&fresh, keys.allows_move(&fresh), true), None);
        keys.block(&fresh);
        assert!(keys.claim_down(&held, true));
        assert!(!keys.allows_move(&held));
        assert!(keys.claim_down(&fresh, true));
        assert_eq!(
            nudge.down(&fresh, keys.allows_move(&fresh), false),
            Some((-1, 0))
        );
        assert_eq!(editor.tool, Tool::Lasso);
        assert!(editor.is_lasso_crop_outside_resize());
    }
}

#[cfg(test)]
mod crop_adjust_cursor_origin_tests {
    use super::*;

    #[test]
    fn accepted_adjust_mouse_down_keeps_original_point_and_replay_never_adopts_old_keys() {
        let down = (102, 82);
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.select_tool(Tool::Edit);
        let admitted =
            cursor_nudge::crop_adjust_cursor_admission(&editor, true, true, false, false);
        assert!(editor.begin_screenshot_empty_crop_inside(down, 4.));
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, admitted),
            Some(down)
        );
        for (original, ordinary, before) in [
            (None, true, true),
            (Some(down), false, true),
            (Some(down), true, false),
        ] {
            assert_eq!(
                admitted_crop_adjust_cursor_origin(&editor, original, ordinary, before),
                None
            );
        }
        editor.pointer_move((114, 92));
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, admitted),
            Some(down)
        );
        let mut keys = cursor_nudge::ManualPreviewCursorKeys::default();
        let mut nudge = cursor_nudge::PreviewCursorNudge::default();
        let mut event = KeyDownEvent {
            keystroke: gpui::Keystroke {
                key: "a".into(),
                key_char: None,
                modifiers: gpui::Modifiers::default(),
            },
            is_held: true,
        };
        // 可信 MouseDown 只建立几何来源；未知 held 与旧 keyboard replay 仍没有移动许可。
        assert!(keys.claim_down(&event, true));
        assert!(!keys.allows_move(&event));
        event.is_held = false;
        assert!(keys.claim_down(&event, true));
        assert_eq!(nudge.down(&event, keys.allows_move(&event), true), None);
        keys.block(&event);
        event.is_held = true;
        assert!(keys.claim_down(&event, true));
        assert!(!keys.allows_move(&event));
        event.is_held = false;
        assert!(keys.claim_down(&event, true));
        assert_eq!(
            nudge.down(&event, keys.allows_move(&event), false),
            Some((-1, 0))
        );
        editor.cancel_gesture();
        assert_eq!(
            admitted_crop_adjust_cursor_origin(&editor, Some(down), true, admitted),
            None
        );
    }
}

#[cfg(test)]
mod new_crop_cursor_origin_tests {
    use super::*;

    #[test]
    fn accepted_native_replay_down_keeps_geometry_but_does_not_adopt_old_keys() {
        let down = (81, 72);
        let mut editor = Editor::new(Rect::full(400, 300), None);
        editor.pointer_down(down, 4.);
        assert_eq!(
            admitted_new_crop_cursor_origin(&editor, Some(down), true, true),
            Some(down)
        );
        for (original, ordinary, before) in [
            (None, true, true),
            (Some(down), false, true),
            (Some(down), true, false),
        ] {
            assert_eq!(
                admitted_new_crop_cursor_origin(&editor, original, ordinary, before),
                None
            );
        }
        // 原 mouse replay 与 keyboard replay 是两条资格；source 成立不等于 held 已接受。
        let held = KeyDownEvent {
            keystroke: gpui::Keystroke {
                key: "a".into(),
                key_char: None,
                modifiers: gpui::Modifiers::default(),
            },
            is_held: true,
        };
        let mut keys = cursor_nudge::ManualPreviewCursorKeys::default();
        assert!(keys.claim_down(&held, true));
        assert!(!keys.allows_move(&held));
        editor.pointer_move((112, 91));
        assert_eq!(
            admitted_new_crop_cursor_origin(&editor, Some(down), true, true),
            Some(down)
        );
        editor.pointer_up((112, 91));
        assert_eq!(
            admitted_new_crop_cursor_origin(&editor, Some(down), true, true),
            None
        );
    }
}

fn capture_cursor_client_matches(
    own: windows::Win32::Foundation::HWND,
    bounds: crate::geometry::ScreenRect,
) -> Result<bool> {
    use windows::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::ClientToScreen,
        UI::WindowsAndMessaging::GetClientRect,
    };
    let mut client = RECT::default();
    unsafe { GetClientRect(own, &mut client)? };
    let mut origin = POINT {
        x: client.left,
        y: client.top,
    };
    if !unsafe { ClientToScreen(own, &mut origin) }.as_bool() {
        return Err(windows::core::Error::from_win32().into());
    }
    Ok(origin.x == bounds.left
        && origin.y == bounds.top
        && client
            .right
            .checked_sub(client.left)
            .and_then(|v| u32::try_from(v).ok())
            == Some(bounds.width)
        && client
            .bottom
            .checked_sub(client.top)
            .and_then(|v| u32::try_from(v).ok())
            == Some(bounds.height))
}

#[cfg(test)]
mod idle_preview_cursor_snapshot_tests {
    use super::idle_preview_cursor_snapshot_ready;
    use gpui::Modifiers;

    #[test]
    fn replay_busy_and_repeated_click_never_offer_an_idle_transfer() {
        for shift in [false, true] {
            let modifiers = Modifiers {
                shift,
                ..Default::default()
            };
            assert!(idle_preview_cursor_snapshot_ready(
                false, 1, modifiers, true
            ));
            assert!(!idle_preview_cursor_snapshot_ready(
                true, 1, modifiers, true
            ));
            assert!(!idle_preview_cursor_snapshot_ready(
                false, 1, modifiers, false
            ));
            for count in [0, 2, 3] {
                assert!(!idle_preview_cursor_snapshot_ready(
                    false, count, modifiers, true
                ));
            }
            for rejected in [
                Modifiers {
                    control: true,
                    ..modifiers
                },
                Modifiers {
                    alt: true,
                    ..modifiers
                },
                Modifiers {
                    platform: true,
                    ..modifiers
                },
                Modifiers {
                    function: true,
                    ..modifiers
                },
            ] {
                assert!(!idle_preview_cursor_snapshot_ready(
                    false, 1, rejected, true
                ));
            }
        }
    }
}

/// 只记真实 OS 读回的位移证据，不改 pointer、原始 Down 或 Editor。
/// 鼠标消息可合并远移再返回，已越过的点击容差不能由后来返回点抹掉。
fn record_manual_preview_cursor_motion(
    dragged: &mut bool,
    start: (i32, i32),
    before: (i32, i32),
    actual: (i32, i32),
    bounds: crate::geometry::ScreenRect,
    scale: f32,
) {
    if actual == before || cursor_nudge::target_in_client(bounds, actual, (0, 0)).is_none() {
        return;
    }
    let Some(point) = actual
        .0
        .checked_sub(bounds.left)
        .zip(actual.1.checked_sub(bounds.top))
    else {
        return;
    };
    *dragged |= crate::capture_monitor_snap::preview_dragged(start, point, scale);
}

#[cfg(test)]
mod manual_preview_cursor_readback_tests {
    use super::*;
    use crate::geometry::ScreenRect;

    fn bounds() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -200,
            width: 3840,
            height: 1280,
        }
    }

    #[test]
    fn actual_zero_and_within_dpi_tolerance_keep_original_click_confirmation() {
        let mut dragged = false;
        let start = (100, 100);
        // ClipCursor 可把成功请求保留原点；请求目标不能作为实际位移证据。
        record_manual_preview_cursor_motion(
            &mut dragged,
            start,
            (-1800, -100),
            (-1800, -100),
            bounds(),
            1.,
        );
        assert!(!dragged);
        record_manual_preview_cursor_motion(
            &mut dragged,
            start,
            (-1820, -100),
            (-1817, -96),
            bounds(),
            1.,
        );
        assert!(!dragged); // 3/4 三角形恰为 5 DIP，保持点击。
        record_manual_preview_cursor_motion(
            &mut dragged,
            start,
            (-1817, -96),
            (-1810, -100),
            bounds(),
            2.,
        );
        assert!(!dragged); // 2× DPI 下恰为 10px，仍保持点击。
    }

    #[test]
    fn actual_far_return_stays_dragged_and_invalid_readback_adds_no_proof() {
        let mut dragged = false;
        let start = (100, 100);
        record_manual_preview_cursor_motion(
            &mut dragged,
            start,
            (-1820, -100),
            (-1809, -100),
            bounds(),
            2.,
        );
        assert!(dragged);
        record_manual_preview_cursor_motion(
            &mut dragged,
            start,
            (-1809, -100),
            (-1820, -100),
            bounds(),
            2.,
        );
        assert!(dragged); // OS 合并远返 MouseMove 后，Up 仍须重框。

        let mut invalid = false;
        record_manual_preview_cursor_motion(
            &mut invalid,
            start,
            (-1820, -100),
            (-1921, -100),
            bounds(),
            1.,
        );
        assert!(!invalid);
        record_manual_preview_cursor_motion(
            &mut invalid,
            start,
            (0, 0),
            (i32::MAX - 2, 0),
            ScreenRect {
                left: i32::MIN,
                top: 0,
                width: u32::MAX,
                height: 1,
            },
            1.,
        );
        assert!(!invalid); // 可表示的全局点不等于可表示的本地 i32 Down 坐标。
    }
}

pub struct CaptureView {
    pub(crate) quick_action: crate::quick_action_flow::QuickActionFlow,
    quick_action_epoch: u64,
    quick_translation_ready: bool,
    quick_save_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    quick_link_pending: bool,
    quick_completion_message: String,
    pub(crate) quick_completion_warnings: Vec<String>,
    pub(crate) pin_mode: bool,
    pub(crate) pin_owner: Option<gpui::WeakEntity<crate::pin_view::PinView>>,
    pub(crate) pin_window: Option<gpui::WindowHandle<crate::pin_view::PinView>>,
    pub(crate) pin_editing: bool,
    /// 独立图片编辑窗口最近一次打开或输出时的文档代次与裁剪；关闭前据此判断有无未输出的修改。
    pub(crate) image_saved: Option<(u64, Option<Rect>)>,
    /// 已提示“再按一次放弃”时的编辑状态；之后再改动需要重新确认。
    pub(crate) image_discard_armed: Option<(u64, Option<Rect>)>,
    pub(crate) pin_presentation_style: Option<crate::selection_finish::SelectionFinishStyle>,
    pub(crate) pin_window_sizing: crate::pin_model::PinWindowSizing,
    pub(crate) pin_content_rect: Option<Rect>,
    pub(crate) pin_content_drawn_size: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) pin_presentation_preview: crate::pin_presentation::CornerCache,
    /// 贴图先按目标尺寸出帧、再移动窗口期间，可见域按目标矩形计算，不读尚未移动的 HWND。
    pub(crate) pin_render_placement: Option<crate::pin_model::Placement>,
    pub(crate) pin_annotation_preview: Option<crate::pin_annotation::AnnotationPreviewVisibility>,
    pub capture_lock: Option<crate::gpui_capture::CaptureLock>,
    pub include_cursor: bool,
    pub cursor: Option<crate::cursor::CapturedCursor>,
    pub cursor_preview: Option<Arc<RenderImage>>,
    pub magnifier: bool,
    pub guides: bool,
    pub show_guides: bool,
    guide_toggle: guide_toggle::GuideToggleGesture,
    preview_cursor_nudge: cursor_nudge::PreviewCursorNudge,
    idle_preview_cursor_keys: cursor_nudge::IdlePreviewCursorKeys,
    idle_preview_cursor_owner: Option<IdlePreviewCursorOwner>,
    /// 交接前的方向键不能在回放时读取用户此刻的鼠标并追加过时偏移。
    pub(crate) preview_cursor_replay: bool,
    pub window_targets: Vec<crate::selection_assist::WindowTarget>,
    pub hover_window: Option<usize>,
    pub(crate) auto_adsorb: bool,
    pub(crate) control_selection_ready: bool,
    pub(crate) control_request_pending: bool,
    pub(crate) last_control_request: Option<(i32, i32)>,
    pub(crate) control_retry_after: Option<std::time::Instant>,
    pub(crate) control_retries_at_point: u8,
    pub(crate) control_pending_navigation: i32,
    pub(crate) control_wheel_remainder: f32,
    pub(crate) control_hover: Option<crate::capture_control_selection::ControlHover>,
    pub(crate) control_tracking: crate::capture_control_selection::ControlTracking,
    pub(crate) selection_click_target: Option<Rect>,
    pub(crate) selection_pointer_dragged: bool,
    /// 完成的原生/画布首击均受控件、菜单、按键和滚轮的既有清除入口约束。
    pub(crate) startup_click_seed: Option<crate::capture_double_click::ClickSeed>,
    ordinary_clicks: crate::capture_double_click::CanvasClickSequence,
    text_core_press: Option<crate::capture_text_selection::TextCorePress>,
    text_core_activation_observed: bool,
    pub(crate) replay_canvas_click: Option<crate::capture_double_click::CanvasClickReplay>,
    /// 原生阶段没有无 crop 预览快照；该次重放不能借后来的 GPUI hover 创建贴图。
    pub(crate) preview_middle_replay: bool,
    pub(crate) preview_enter_replay: bool,
    pub(crate) preview_text_replay: bool,
    pub(crate) painted_preview: Rc<Cell<Option<crate::preview_middle_pin::PaintedPreview>>>,
    /// Ctrl+A 比较实际已呈现的 crop/Preview；输出仍使用独立的 painted_preview。
    pub(crate) painted_selection: Rc<Cell<Option<crate::preview_middle_pin::PaintedPreview>>>,
    /// Ctrl+A 的固定 Preview 独立于已确认 crop，鼠标悬停和 UIA 回包不能移动它。
    pub(crate) manual_preview: Option<Rect>,
    pub(crate) manual_preview_click: Option<Rect>,
    pub(crate) manual_preview_dragged: bool,
    /// 从固定 Preview 的已接受 LeftDown 起认领到 Up/取消；转入 Crop 后仍保留 WASD。
    manual_preview_cursor_claimed: bool,
    /// 普通 New 只保存实际接受的原 Down；不成为固定来源或跨端按键资格。
    new_crop_cursor_origin: Option<(i32, i32)>,
    /// 已接受 Resize/ResizeFromInside/Move；失效仍保留键周期消费，不能补旧 Down。
    crop_adjust_cursor_origin: Option<(i32, i32)>,
    crop_adjust_cursor_owner: Option<IdlePreviewCursorOwner>,
    /// 范围手势结束后仍消费该键周期的 held，真实 KeyUp/新 Down 才释放。
    manual_preview_cursor_keys: cursor_nudge::ManualPreviewCursorKeys,
    /// Native 排队时保存的指针与可见范围；外层 Some 禁止重采交接后的鼠标。
    pub(crate) manual_preview_replay_point: Option<ManualPreviewReplay>,
    pub magnifier_image: Option<Arc<crate::capture::Image>>,
    pub magnifier_cursor: (u32, u32),
    magnifier_sample_point: Option<((i32, i32), bool)>,
    pub(crate) magnifier_color: Option<u32>,
    capture_color_display: crate::pin_color_display::ColorDisplay,
    capture_color_keys_task: Option<gpui::Task<()>>,
    capture_color_keys_eligible: bool,
    capture_color_p_down: bool,
    pub(crate) capture_color_replay: bool,
    pub pointer_down_start: Option<(i32, i32)>,
    pencil_samples: crate::pencil_samples::PencilSamples,
    pub processing: bool,
    pub(crate) smart_candidates: smart_candidates::Candidates,
    pub(crate) pin_handoff_pending: bool,
    pub recognition_result: Option<String>,
    pub(crate) ocr_overlay: Option<crate::capture_ocr_overlay::OcrOverlay>,
    result_selection: text_result::Selection,
    result_generation: u64,
    pub(crate) ocr_find: Option<crate::pin_view::find_ui::FindUi<text_result::OcrTarget>>,
    pub(crate) capture_find_click: Rc<
        std::cell::RefCell<
            crate::pin_view::find_ui::FindClickIntent<crate::capture_ocr_overlay::CaptureFindClick>,
        >,
    >,
    pub(crate) local_ocr: Option<crate::capture_ocr_overlay::local_retry_host::LocalOcrState>,
    pub(crate) ocr_result_installation: u64,
    result_body_identity: Arc<()>,
    table_copy: Option<crate::pin_view::table_copy::TableCopy<(text_result::OcrTarget, u64)>>,
    table_copy_click: Option<text_result::TableClickStamp<text_result::OcrTarget>>,
    table_copy_down: Option<crate::capture_ocr_overlay::inline_click::DownProof>,
    typed_pointer_epoch: Arc<()>,
    typed_pointer_subscriptions: Vec<gpui::Subscription>,
    pub(crate) translation: Option<crate::translate::Translation>,
    translation_request: u64,
    translation_copy_click: Option<crate::translate::TranslationTicket>,
    translation_close_click: Option<crate::translate::TranslationTicket>,
    translation_copy_down: Option<crate::capture_ocr_overlay::inline_click::DownProof>,
    translation_close_down: Option<crate::capture_ocr_overlay::inline_click::DownProof>,
    /// 图片翻译：段落译文作为带底色的文字标注落图。
    pub(crate) image_translation: image_translate_host::ImageTranslation,
    /// 仅绑定自动复制失败的这一结果代次；新会话/全文编辑会撤销该重试语义。
    auto_copy_recovery: Option<(u64, text_result::InputTarget)>,
    pub(crate) painted_result_output: Rc<Cell<Option<text_result::ResultFrame>>>,
    pub(crate) painted_result_projection:
        Rc<std::cell::RefCell<Option<crate::ocr_projection::ProjectionPaint>>>,
    pub(crate) ocr_projection_epoch: Arc<()>,
    pub(crate) observed_ocr_projection: Option<crate::ocr_projection::OcrProjection>,
    pub(crate) result_click:
        Rc<std::cell::RefCell<crate::capture_ocr_result_click::ResultClickState>>,
    pub(crate) result_body_paint:
        Rc<std::cell::RefCell<Option<crate::capture_ocr_result_click::ResultBodyPaint>>>,
    recognition_session: text_result::RecognitionSession,
    // 截图/图片的原位结果共用 Owner；贴图、智能效果与 UIA 补救使用此独立槽。
    pub(crate) foreground_ocr: ocr_task::TaskSlot<text_result::InputTarget>,
    ocr_progress_clock: ocr_progress::ProgressClock,
    pub(crate) ocr_click_trace: Option<ocr_progress::ClickTrace>,
    pub(crate) result_editor: Option<text_result::ResultEditor>,
    pub(crate) result_input_handler: Option<gpui::Entity<crate::text_input::ResultInputHandler>>,
    pub(crate) result_input_layout:
        Rc<std::cell::RefCell<Option<crate::text_input::ResultInputLayout>>>,
    pub(crate) result_reveal_caret: Rc<Cell<bool>>,
    pub(crate) result_editor_scroll: gpui::ScrollHandle,
    pub result_title: String,
    pub source: CanvasSource,
    pub image_canvas: Option<crate::image_editor::ImageCanvas>,
    pub(crate) long_clip_ui: Option<crate::long_clip_ui::LongClipUi>,
    pub(crate) long_clip_on_done:
        Option<Box<dyn FnOnce(crate::long_output::LongOutcome, &mut gpui::App)>>,
    pub(crate) long_clip_output_done: bool,
    pub(crate) long_clip_placement: Option<crate::export::ClipboardPlacement>,
    pub previews: Vec<Arc<RenderImage>>,
    pub editor: Editor,
    pub(crate) watermark_field: Option<gpui::Entity<crate::text_field::TextField>>,
    pub(crate) watermark_binding: Option<crate::watermark_edit::BindingToken>,
    pub(crate) watermark_field_revision: Option<u64>,
    pub(crate) watermark_input_focused: bool,
    pub(crate) watermark_input_window: Option<gpui::AnyWindowHandle>,
    pub(crate) watermark_subscription: Option<gpui::Subscription>,
    pub(crate) watermark_previews: Vec<crate::watermark_preview::WatermarkPreview>,
    pub(crate) text_previews: Vec<crate::text_preview::TextPreview>,
    tool_presets: crate::tool_presets::ToolPresets,
    pub focus: FocusHandle,
    pub pointer: (i32, i32),
    pub pointer_sample: crate::annotation::Point,
    pub toolbar_monitor: usize,
    pub status: String,
    pub saving: bool,
    pub(crate) color_picker_pending: bool,
    pub(crate) color_picker: Option<gpui::Entity<crate::capture_color_dialog::ColorPicker>>,
    /// 最近一次绘制的二级栏收起级别；颜色弹层据此决定是否并入不透明度。
    pub(crate) property_level: std::cell::Cell<u8>,
    pub ui_positions:
        std::rc::Rc<std::cell::RefCell<std::collections::HashMap<&'static str, (f32, f32)>>>,
    /// 静态标注层的跨帧 GPU 缓存状态，由画布 paint 回调读取并更新。
    pub(crate) retained_annotations:
        std::rc::Rc<std::cell::RefCell<crate::vector_paint::RetainedAnnotations>>,
    pub input: crate::text_input::TextInputState,
    pub light_theme: bool,
    pub toolbar_panel: ToolbarPanel,
    /// 外观面板显示全部效果，还是选区条某个效果箭头打开的单项卡片。
    pub(crate) finish_section: crate::selection_finish_view::FinishSection,
    /// 正在拖动的选区圆角控制点。
    pub(crate) finish_radius_drag: Option<crate::selection_finish_view::FinishRadiusDrag>,
    pub(crate) selection_size_input:
        Option<gpui::Entity<crate::capture_size_input::SelectionSizeInput>>,
    toolbar_visibility: toolbar_visibility::ToolbarVisibility,
    pub(crate) screenshot_toolbar_menu: crate::capture_interaction::ScreenshotToolbarMenu,
    pub toolbar_position: Option<(f32, f32)>,
    pub(crate) toolbar_manual_monitor: Option<usize>,
    pub(crate) toolbar_snap_preview: Option<crate::toolbar_layout::PanelRect>,
    pub(crate) toolbar_config: crate::toolbar_config::ToolbarConfig,
    /// 分组按钮的最近使用项与悬停下拉状态；只在本次会话内保留。
    pub(crate) tool_groups: crate::toolbar_groups::GroupMemory,
    pub(crate) flyout: crate::toolbar_groups::FlyoutHover,
    /// 识别下拉最近使用项，只在本次会话保留。
    pub(crate) recognize_mode: crate::toolbar_groups::RecognizeMode,
    /// 识别并翻译／表格识别：OCR 结果安装时打开对应面板页。
    ocr_intent: Option<crate::toolbar_groups::RecognizeMode>,
    /// 最近一次原位 OCR 的结果与后台线检测，供“表格”页强制按表格识别时复用。
    table_input: Option<TableInput>,
    /// 当前表格缓存来自放宽规则的强制识别。
    table_forced: bool,
    /// 当前译文请求是按段落翻译的原位全文，可逐段对照。
    pub(crate) translation_by_paragraph: bool,
    /// 表格页悬停的单元格序号；图上同步加深。
    pub(crate) table_hover: Option<usize>,
    /// 选区内自动识别到的二维码与其悬浮按钮、弹层状态。
    pub(crate) qr: crate::capture_qr::QrState,
    pub wheel_input: crate::interaction::WheelAccumulator,
    pub wheel_settings: (f32, f32),
    pub(crate) toolbar_drag: Option<crate::toolbar_drag::ToolbarDrag>,
    pub(crate) refresh_request: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub raster_previews: Vec<(Arc<Image>, Arc<RenderImage>)>,
    pub(crate) retired_raster_previews: Vec<Arc<RenderImage>>,
    pub(crate) editor_preview_updates: u64,
    pub(crate) editor_preview_releases: u64,
    pub(crate) retired_editor_preview_ids: Vec<usize>,
    pub(crate) finish_style: crate::selection_finish::SelectionFinishStyle,
    pub(crate) finish_effect: crate::selection_finish::FinishEffect,
    pub(crate) finish_effect_intent: Option<crate::selection_finish::FinishEffect>,
    pub(crate) finish_advanced: bool,
    /// 当前样式；`finish_style` 始终等于它在 `finish_looks` 里的槽位（原样为全关）。
    pub(crate) finish_preset: crate::selection_finish::FinishPreset,
    pub(crate) finish_looks: crate::selection_finish::FinishLooks,
    /// 指针停在选区条某个样式上时只预览、不提交；导出始终用 `finish_style`。
    pub(crate) finish_hover: Option<crate::selection_finish::FinishPreset>,
    /// 样式滚轮切换累积的不足一格的刻度。
    pub(crate) finish_look_wheel: f32,
    pub(crate) finish_store: crate::selection_finish_store::SelectionFinishStore,
    pub(crate) finish_preview: Option<crate::selection_finish_view::FinishPreview>,
    pub(crate) scene: Arc<Scene>,
    pub(crate) scene_stamp: Option<(u64, u64)>,
}
impl CaptureView {
    /// 图片的“当前范围”始终是原图，显示 ROI 不能成为截图选区。
    pub(crate) fn current_capture_rect(&self) -> Rect {
        if self.image_canvas.is_some() {
            self.source.full()
        } else {
            self.source
                .screen_rect(self.source.monitor_at(self.pointer))
        }
    }
    fn snap_screen_preview(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        let cancel_crop = self.editor.tool == Tool::Crop
            && self.editor.is_changing_crop()
            && self.editor.document_annotations().is_empty();
        if !modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.function
            || self.pin_mode
            || self.image_canvas.is_some()
            || !self.control_selection_ready
            || self.processing
            || self.saving
            || self.pin_handoff_pending
            || (self.pointer_down_start.is_some()
                && self.manual_preview_click.is_none()
                && !cancel_crop)
            || self.toolbar_drag.is_some()
            || self.watermark_input_focused
            || self.input.marked.is_some()
            || self.result_editor.is_some()
            || self.recognition_result.is_some()
            || self.toolbar_panel != ToolbarPanel::None
            || self.screenshot_toolbar_menu.presentation().is_some()
            || (self.editor.is_dragging() && !cancel_crop)
            || self.editor.has_pending_drawing()
            || self.editor.text_draft.is_some()
            || self.editor.watermark.has_preview()
            || (self.editor.crop.is_some()
                && (self.editor.tool != Tool::Crop
                    || !self.editor.document_annotations().is_empty()))
        {
            return false;
        }
        let (point, shown) = if let Some(snapshot) = self.manual_preview_replay_point {
            (snapshot.global_point, snapshot.shown_preview)
        } else {
            let mut point = windows::Win32::Foundation::POINT::default();
            (
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) }
                    .is_ok()
                    .then_some((point.x, point.y)),
                self.presented_selection(window),
            )
        };
        // 原生事件采样失败也由该事件消费，不能改读交接后的鼠标。
        let Some(point) = point else {
            return true;
        };
        let screens: Vec<_> = (0..self.source.screen_count())
            .filter_map(|index| {
                let rect = self.source.screen_rect(index);
                Some(crate::geometry::ScreenRect {
                    left: self
                        .source
                        .bounds
                        .left
                        .checked_add(i32::try_from(rect.x).ok()?)?,
                    top: self
                        .source
                        .bounds
                        .top
                        .checked_add(i32::try_from(rect.y).ok()?)?,
                    width: rect.width,
                    height: rect.height,
                })
            })
            .collect();
        if let Some(rect) = crate::capture_monitor_snap::snap_preview_target(
            self.source.bounds,
            &screens,
            point,
            self.manual_preview,
            shown,
        ) {
            if cancel_crop {
                self.editor.cancel_gesture();
            }
            self.editor.set_crop(None);
            self.invalidate_click_sequence();
            self.clear_pointer_tracking();
            self.release_capture();
            self.wheel_input.reset();
            self.preview_cursor_nudge.reset();
            self.set_manual_preview(Some(rect));
            self.status.clear();
            cx.notify();
        }
        true
    }
    pub(crate) fn flush_tool_presets(&mut self) {
        for (tool, style) in self.editor.tool_styles() {
            self.tool_presets.update(tool, style);
        }
        if let Err(error) = self.tool_presets.save() {
            eprintln!("工具参数保存失败：{error}");
        }
        let saved = self.finish_store.save_session(
            self.finish_style,
            self.finish_preset,
            self.finish_looks,
            self.finish_effect_intent,
        );
        match saved {
            Ok(_) => self.finish_effect_intent = None,
            Err(error) => eprintln!("选区外观保存失败：{error}"),
        }
    }
    pub(crate) fn cancel_pointer(&mut self) {
        self.cancel_translation_clicks();
        self.wheel_input.reset();
        self.clear_pointer_tracking();
        self.release_capture();
    }
    pub(crate) fn cleanup_inactive_screenshot(
        &mut self,
        inactive: bool,
        foreground_owned: bool,
    ) -> bool {
        // 刷新主动隐藏宿主，不能把这次失焦当作取消编辑或识别结果。
        if self.refresh_request.is_some() {
            return false;
        }
        let mut result_changed = false;
        if !self.pin_mode && self.image_canvas.is_none() && inactive && !foreground_owned {
            result_changed |= self.cancel_translation_clicks();
            if self.smart_candidates.pointer {
                self.clear_pointer_tracking();
            }
            self.cancel_smart_pointer();
            self.smart_candidates.core.leave();
            result_changed |= self.manual_preview.is_some();
            self.set_manual_preview(None);
            result_changed |= self.stop_capture_color();
            result_changed |= self.result_selection_end(self.result_generation);
            if let Some(editor) = &mut self.result_editor {
                result_changed |= editor.cancel_composition();
            }
            result_changed |= self.invalidate_translation_source();
            self.result_input_layout.borrow_mut().take();
            self.result_reveal_caret.set(false);
            self.guide_toggle.reset_tracking();
            self.preview_cursor_nudge.reset();
            self.manual_preview_cursor_claimed = false;
            self.new_crop_cursor_origin = None;
            self.crop_adjust_cursor_owner = None;
            self.manual_preview_cursor_keys.cancel_gesture();
            self.clear_idle_preview_cursor();
            self.screenshot_toolbar_menu.dismiss();
            self.replay_canvas_click = None;
            self.ordinary_clicks.consumed();
            // 原生交接仍按原有规则处理；已完成画布首击不能穿过失焦边界。
            self.startup_click_seed = self.startup_click_seed.filter(|seed| seed.native());
        }
        if !cleanup_ordinary_activation(
            &mut self.editor,
            &mut self.wheel_input,
            !self.pin_mode && self.image_canvas.is_none(),
            self.control_selection_ready,
            inactive,
            foreground_owned,
            self.pointer_down_start.is_some(),
        ) {
            return result_changed;
        }
        self.invalidate_click_sequence();
        self.clear_pointer_tracking();
        self.cancel_toolbar_drag();
        true
    }
    pub(crate) fn reset_guide_toggle(&mut self) {
        if !self.pin_mode && self.image_canvas.is_none() {
            self.guide_toggle.reset();
        }
    }
    pub(crate) fn guide_modifiers_changed(&mut self, event: &gpui::ModifiersChangedEvent) {
        if !self.pin_mode && self.image_canvas.is_none() {
            let allowed = self.control_selection_ready
                && !self.saving
                && !self.processing
                && !self.pin_handoff_pending
                && self.editor.text_draft.is_none()
                && self.input.marked.is_none()
                && !self.watermark_input_focused
                && self.toolbar_panel == ToolbarPanel::None
                && self.toolbar_drag.is_none()
                && self.recognition_result.is_none();
            if self.guide_toggle.observe(event, allowed) {
                self.guides = !self.guides;
            }
            self.show_guides = self.guides;
        } else {
            self.show_guides = self.guides || event.modifiers.alt;
        }
    }
    pub(crate) fn screenshot_toolbar_visible(&mut self) -> bool {
        if self.pin_mode || self.image_canvas.is_some() {
            self.screenshot_toolbar_menu.dismiss();
            return true;
        }
        self.toolbar_visibility.observe_crop(&self.editor);
        let shown = self.toolbar_visibility.visible();
        let busy = self.screenshot_toolbar_menu_busy();
        self.screenshot_toolbar_menu
            .observe(&self.editor, true, shown, busy);
        shown
    }
    pub(crate) fn screenshot_toolbar_is_shown(&self) -> bool {
        self.toolbar_visibility.visible()
    }
    pub(crate) fn screenshot_toolbar_menu_busy(&self) -> bool {
        !self.control_selection_ready
            || self.saving
            || self.processing
            || self.pin_handoff_pending
            || self.toolbar_drag.is_some()
            || self.recognition_result.is_some()
            || self.toolbar_panel != ToolbarPanel::None
            || self.pointer_down_start.is_some()
            || self.watermark_input_focused
            || self.input.marked.is_some()
            || self.input.drag_anchor.is_some()
    }
    pub(crate) fn open_screenshot_toolbar_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &Window,
    ) -> bool {
        let shown = self.screenshot_toolbar_visible();
        let point = self.physical(event.position, window);
        if !crate::capture_interaction::screenshot_toolbar_menu_ready(
            &self.editor,
            point,
            !self.pin_mode && self.image_canvas.is_none(),
            shown,
            self.screenshot_toolbar_menu_busy(),
        ) {
            return false;
        }
        let screen = self.source.screen_rect(self.source.monitor_at(point));
        // monitor_at 在屏间黑隙可能返回默认屏；真实事件须位于该屏幕范围。
        if !screen.valid(self.source.bounds.width, self.source.bounds.height)
            || !screen.contains(point)
        {
            return false;
        }
        let Some(visible) = crate::capture_interaction::screenshot_toolbar_menu_bounds(
            screen,
            (
                f32::from(window.viewport_size().width),
                f32::from(window.viewport_size().height),
            ),
            window.scale_factor(),
        ) else {
            return false;
        };
        let opened = self.screenshot_toolbar_menu.open(
            &self.editor,
            (f32::from(event.position.x), f32::from(event.position.y)),
            visible,
        );
        if opened {
            self.clear_idle_preview_cursor();
        }
        opened
    }
    pub(crate) fn screenshot_toolbar_menu_command(
        &mut self,
        command: crate::capture_interaction::ScreenshotToolbarMenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let busy = self.screenshot_toolbar_menu_busy();
        if !self.screenshot_toolbar_menu.observe(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.toolbar_visibility.visible(),
            busy,
        ) {
            cx.notify();
            return;
        }
        self.screenshot_toolbar_menu.dismiss();
        match command {
            crate::capture_interaction::ScreenshotToolbarMenuCommand::Show => {
                show_screenshot_toolbar(&mut self.toolbar_visibility, &self.editor);
            }
            crate::capture_interaction::ScreenshotToolbarMenuCommand::Reselect => {
                self.perform(Action::Reselect, window, cx);
            }
            crate::capture_interaction::ScreenshotToolbarMenuCommand::Cancel => {
                self.perform(Action::Cancel, window, cx);
            }
        }
        cx.notify();
    }
    fn screenshot_toolbar_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let was_open = self.screenshot_toolbar_menu.presentation().is_some();
        self.screenshot_toolbar_visible();
        let command = self.screenshot_toolbar_menu.key(event);
        if was_open {
            self.clear_idle_preview_cursor();
            cx.notify();
        }
        match command {
            Some(crate::capture_interaction::ScreenshotToolbarMenuKey::Consumed) => true,
            Some(crate::capture_interaction::ScreenshotToolbarMenuKey::Command(command)) => {
                self.screenshot_toolbar_menu_command(command, window, cx);
                true
            }
            None => false,
        }
    }
    fn toggle_screenshot_toolbar(&mut self, event: &KeyDownEvent) -> bool {
        let handled = self.toolbar_visibility.space(
            &self.editor,
            event,
            !self.pin_mode && self.image_canvas.is_none(),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.toolbar_drag.is_some()
                || self.recognition_result.is_some(),
            self.watermark_input_focused || self.input.marked.is_some(),
            self.toolbar_panel,
        );
        if handled && !self.toolbar_visibility.visible() {
            self.toolbar_panel = ToolbarPanel::None;
            self.selection_size_input = None;
            self.flyout.close();
        }
        handled
    }
    pub(crate) fn inactive_screenshot_release_ready(&self) -> bool {
        !self.pin_mode
            && self.image_canvas.is_none()
            && self.control_selection_ready
            && self.pointer_down_start.is_none()
            && self.toolbar_drag.is_none()
    }
    pub(crate) fn canvas_click_modifiers_changed(&mut self, modifiers: gpui::Modifiers) {
        // 双击仍拒绝所有修饰；Preview 的 Shift 边沿保持同一物理微调的加速。
        self.ordinary_clicks.modifiers_changed(modifiers.modified());
        self.preview_cursor_nudge.modifiers_changed(modifiers);
        self.idle_preview_cursor_keys.modifiers_changed(modifiers);
        if !self.idle_preview_cursor_keys.has_verified() {
            self.idle_preview_cursor_owner = None;
        }
    }
    fn preview_cursor_ready(&self) -> bool {
        cursor_nudge::preview_ready(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.control_selection_ready
                && self
                    .capture_lock
                    .as_ref()
                    .is_some_and(|owner| !owner.is_cancelled()),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.pointer_down_start.is_some()
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
        )
    }
    fn manual_preview_cursor_ready(&self) -> bool {
        cursor_nudge::manual_preview_cursor_ready(
            &self.editor,
            cursor_nudge::ManualPreviewCursorState {
                manual: self.manual_preview,
                click: self.manual_preview_click,
                down: self.pointer_down_start,
                claimed: self.manual_preview_cursor_claimed,
            },
            !self.pin_mode && self.image_canvas.is_none(),
            self.control_selection_ready
                && self
                    .capture_lock
                    .as_ref()
                    .is_some_and(|owner| !owner.is_cancelled()),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.result_editor.is_some()
                || self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
        )
    }
    fn crop_drag_cursor_ready(&self) -> bool {
        !self.pin_mode
            && self.image_canvas.is_none()
            && self.pointer_down_start.is_some()
            && self.editor.is_changing_crop()
            && self.control_selection_ready
            && self
                .capture_lock
                .as_ref()
                .is_some_and(|owner| !owner.is_cancelled())
            && !self.saving
            && !self.processing
            && !self.pin_handoff_pending
            && self.toolbar_drag.is_none()
            && !self.watermark_input_focused
            && self.input.marked.is_none()
            && self.recognition_result.is_none()
            && self.toolbar_panel == ToolbarPanel::None
            && self.screenshot_toolbar_menu.presentation().is_none()
            && !self.editor.has_pending_drawing()
            && self.editor.text_draft.is_none()
            && self.editor.lasso_draft.is_none()
            && !self.editor.watermark.has_preview()
    }
    fn new_crop_cursor_ready(&self) -> bool {
        cursor_nudge::new_crop_cursor_ready(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none() && !self.manual_preview_cursor_claimed,
            self.new_crop_cursor_origin
                .is_some_and(|start| self.pointer_down_start == Some(start)),
            self.control_selection_ready
                && self
                    .capture_lock
                    .as_ref()
                    .is_some_and(|owner| !owner.is_cancelled()),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.result_editor.is_some()
                || self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
        )
    }
    pub(crate) fn crop_adjust_cursor_down_ready(&self) -> bool {
        cursor_nudge::crop_adjust_cursor_admission(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.control_selection_ready
                && self
                    .capture_lock
                    .as_ref()
                    .is_some_and(|owner| !owner.is_cancelled()),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.pointer_down_start.is_some()
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.result_editor.is_some()
                || self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
        )
    }
    fn accept_crop_adjust_cursor_down(&mut self, ready_before_down: bool, window: &Window) {
        self.crop_adjust_cursor_origin = admitted_crop_adjust_cursor_origin(
            &self.editor,
            self.pointer_down_start,
            !self.pin_mode && self.image_canvas.is_none(),
            ready_before_down,
        );
        self.crop_adjust_cursor_owner = self.crop_adjust_cursor_origin.and_then(|_| {
            self.capture_lock
                .as_ref()
                .map(|owner| IdlePreviewCursorOwner {
                    window: hwnd(window).0 as usize,
                    thread: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
                    bounds: self.source.bounds,
                    source: crate::scene::SourceIdentity::of(&self.source_pixels()),
                    session: owner.native_cancel_token(),
                })
        });
    }
    fn crop_adjust_cursor_ready(&self) -> bool {
        cursor_nudge::crop_adjust_cursor_ready(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none() && !self.manual_preview_cursor_claimed,
            self.crop_adjust_cursor_origin
                .is_some_and(|start| self.pointer_down_start == Some(start)),
            self.control_selection_ready
                && self
                    .capture_lock
                    .as_ref()
                    .is_some_and(|owner| !owner.is_cancelled())
                && self.crop_adjust_cursor_owner.as_ref().is_some_and(|owner| {
                    owner.bounds == self.source.bounds
                        && owner.source == crate::scene::SourceIdentity::of(&self.source_pixels())
                        && owner.session.session_is_live()
                }),
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.result_editor.is_some()
                || self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
        )
    }
    fn crop_adjust_cursor_window_matches(&self, window: &Window) -> bool {
        use windows::Win32::{
            System::Threading::GetCurrentThreadId,
            UI::WindowsAndMessaging::GetWindowThreadProcessId,
        };
        self.crop_adjust_cursor_owner.as_ref().is_some_and(|owner| {
            owner.window == hwnd(window).0 as usize
                && owner.thread == unsafe { GetCurrentThreadId() }
                && owner.thread == unsafe { GetWindowThreadProcessId(hwnd(window), None) }
        })
    }
    pub(crate) fn preview_cursor_key_down(&mut self, event: &KeyDownEvent) {
        self.manual_preview_cursor_keys.observe_down(event);
        self.idle_preview_cursor_keys.observe_down(event);
        if !self.preview_cursor_ready()
            || self.preview_cursor_replay
            || !cursor_nudge::preview_movement_modifiers(event.keystroke.modifiers)
        {
            self.clear_idle_preview_cursor();
        }
        let ready = self.preview_cursor_ready()
            || self.manual_preview_cursor_ready()
            || self.new_crop_cursor_ready()
            || self.crop_adjust_cursor_ready();
        self.preview_cursor_nudge
            .observe_key_down(event, ready, self.preview_cursor_replay);
    }
    pub(crate) fn preview_cursor_key_up(&mut self, event: &gpui::KeyUpEvent) {
        self.manual_preview_cursor_keys.key_up(event);
        self.idle_preview_cursor_keys.key_up(event);
        if !self.idle_preview_cursor_keys.has_verified() {
            self.idle_preview_cursor_owner = None;
        }
        self.preview_cursor_nudge.key_up(event);
    }
    fn clear_idle_preview_cursor(&mut self) {
        self.idle_preview_cursor_keys.cancel();
        self.idle_preview_cursor_owner = None;
    }
    fn idle_preview_cursor_owner_ready(&self, window: &Window, manual: bool) -> bool {
        use windows::Win32::{
            System::Threading::GetCurrentThreadId,
            UI::{
                Input::KeyboardAndMouse::{
                    GetFocus, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
                },
                WindowsAndMessaging::{
                    GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SM_SWAPBUTTON,
                },
            },
        };
        let own = hwnd(window);
        if self.preview_cursor_replay
            || if manual {
                !self.manual_preview_cursor_ready()
            } else {
                !self.preview_cursor_ready() || self.result_editor.is_some()
            }
        {
            return false;
        }
        unsafe {
            let (left, right) = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                (VK_RBUTTON, VK_LBUTTON)
            } else {
                (VK_LBUTTON, VK_RBUTTON)
            };
            let left_held = GetAsyncKeyState(left.0 as i32) < 0;
            let others = [right, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                .map(|key| GetAsyncKeyState(key.0 as i32) < 0);
            IsWindow(Some(own)).as_bool()
                && GetForegroundWindow() == own
                && GetFocus() == own
                && GetWindowThreadProcessId(own, None) == GetCurrentThreadId()
                && if manual {
                    GetCapture() == own
                        && cursor_nudge::manual_preview_cursor_buttons_ready(left_held, others)
                } else {
                    GetCapture().is_invalid() && !left_held && !others.into_iter().any(|v| v)
                }
        }
    }
    fn record_idle_preview_cursor_commit(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        commit: cursor_nudge::IdlePreviewCursorCommit,
    ) {
        if commit != cursor_nudge::IdlePreviewCursorCommit::Rejected {
            let same_source = self.idle_preview_cursor_owner.as_ref().is_none_or(|owner| {
                owner.window == hwnd(window).0 as usize
                    && owner.bounds == self.source.bounds
                    && owner.session.session_is_live()
                    && owner.source == crate::scene::SourceIdentity::of(&self.source_pixels())
            });
            if !same_source {
                self.clear_idle_preview_cursor();
            }
        }
        self.idle_preview_cursor_keys.record_commit(event, commit);
        if self.idle_preview_cursor_keys.has_verified() {
            if self.idle_preview_cursor_owner.is_none() {
                if let Some(owner) = self.capture_lock.as_ref() {
                    self.idle_preview_cursor_owner = Some(IdlePreviewCursorOwner {
                        window: hwnd(window).0 as usize,
                        thread: unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
                        bounds: self.source.bounds,
                        source: crate::scene::SourceIdentity::of(&self.source_pixels()),
                        session: owner.native_cancel_token(),
                    });
                } else {
                    self.clear_idle_preview_cursor();
                }
            }
        } else {
            self.idle_preview_cursor_owner = None;
        }
    }
    fn adopt_idle_preview_cursor(
        &mut self,
        snapshot: cursor_nudge::IdlePreviewCursorSnapshot,
        owner: IdlePreviewCursorOwner,
        window: &Window,
        modifiers: gpui::Modifiers,
    ) {
        use windows::Win32::{
            Foundation::POINT,
            System::Threading::GetCurrentThreadId,
            UI::{
                Input::KeyboardAndMouse::{
                    VK_A, VK_CONTROL, VK_D, VK_DOWN, VK_LEFT, VK_LWIN, VK_MENU, VK_RIGHT, VK_RWIN,
                    VK_S, VK_UP, VK_W,
                },
                WindowsAndMessaging::GetCursorPos,
            },
        };
        // 回放/拒绝从 Down 入口已撤候选；这里仅在原 Down 和捕获建立之后复核。
        if snapshot.is_empty()
            || !cursor_nudge::preview_movement_modifiers(modifiers)
            || owner.window != hwnd(window).0 as usize
            || owner.thread != unsafe { GetCurrentThreadId() }
            || owner.bounds != self.source.bounds
            || !owner.session.session_is_live()
            || owner.source != crate::scene::SourceIdentity::of(&self.source_pixels())
            || !self.idle_preview_cursor_owner_ready(window, true)
            || !capture_cursor_client_matches(hwnd(window), self.source.bounds).unwrap_or(false)
        {
            return;
        }
        let mut actual = POINT::default();
        if unsafe { GetCursorPos(&mut actual) }.is_err()
            || cursor_nudge::target_in_client(self.source.bounds, (actual.x, actual.y), (0, 0))
                .is_none()
            || [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
                .into_iter()
                .any(|key| unsafe { GetAsyncKeyState(key.0 as i32) } < 0)
        {
            return;
        }
        let held = [VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN, VK_A, VK_W, VK_D, VK_S]
            .map(|key| unsafe { GetAsyncKeyState(key.0 as i32) } < 0);
        if !owner.session.session_is_live()
            || !self.idle_preview_cursor_owner_ready(window, true)
            || !capture_cursor_client_matches(hwnd(window), self.source.bounds).unwrap_or(false)
        {
            return;
        }
        self.manual_preview_cursor_keys
            .adopt_idle(snapshot, held, &mut self.preview_cursor_nudge);
    }
    fn move_preview_cursor(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let new_crop = cursor_nudge::new_crop_cursor_source(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.pointer_down_start.is_some(),
        ) && !self.manual_preview_cursor_claimed;
        let crop_adjust = cursor_nudge::crop_adjust_cursor_source(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.pointer_down_start.is_some(),
        ) && self.crop_adjust_cursor_origin.is_some()
            && !self.manual_preview_cursor_claimed;
        let claimed_held = self.manual_preview_cursor_keys.claim_down(
            event,
            self.manual_preview_cursor_claimed || new_crop || crop_adjust,
        );
        let manual_held = claimed_held && self.manual_preview_cursor_claimed;
        let new_crop_held = claimed_held && new_crop;
        let crop_adjust_held = claimed_held && crop_adjust;
        let crop_drag = if claimed_held {
            cursor_nudge::CropDragArrowRoute::Pass
        } else {
            cursor_nudge::crop_drag_arrow_route(
                &self.editor,
                !self.pin_mode && self.image_canvas.is_none(),
                self.pointer_down_start.is_some(),
                event,
                self.crop_drag_cursor_ready(),
                self.preview_cursor_replay,
            )
        };
        let delta = if claimed_held {
            let ready = (manual_held && self.manual_preview_cursor_ready()
                || new_crop_held && self.new_crop_cursor_ready()
                || crop_adjust_held && self.crop_adjust_cursor_ready())
                && self.manual_preview_cursor_keys.allows_move(event);
            let Some(delta) =
                self.preview_cursor_nudge
                    .down(event, ready, self.preview_cursor_replay)
            else {
                // 无资格/重放仍属于原范围手势，不能触发旧 WASD 工具或确认动作。
                self.manual_preview_cursor_keys.block(event);
                return true;
            };
            delta
        } else {
            match crop_drag {
                cursor_nudge::CropDragArrowRoute::Consume => return true,
                cursor_nudge::CropDragArrowRoute::Move(delta) => delta,
                cursor_nudge::CropDragArrowRoute::Pass => {
                    let ready = self.preview_cursor_ready();
                    let Some(delta) =
                        self.preview_cursor_nudge
                            .down(event, ready, self.preview_cursor_replay)
                    else {
                        // 旧队列/暂不具备移动资格的无 crop 裸 WASD 也不改走 Current/Spotlight。
                        return cursor_nudge::claims_preview_wasd(
                            &self.editor,
                            !self.pin_mode && self.image_canvas.is_none(),
                            event,
                        );
                    };
                    delta
                }
            }
        };
        let dragging_crop = matches!(crop_drag, cursor_nudge::CropDragArrowRoute::Move(_));
        let idle = !claimed_held && !dragging_crop;
        let mut idle_commit = cursor_nudge::IdlePreviewCursorCommit::Rejected;
        let moved = (|| -> Result<bool> {
            use windows::Win32::{
                Foundation::POINT,
                UI::{
                    Input::KeyboardAndMouse::{
                        GetAsyncKeyState, GetFocus, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
                        VK_XBUTTON1, VK_XBUTTON2,
                    },
                    WindowsAndMessaging::{
                        GetCursorPos, GetForegroundWindow, GetSystemMetrics, IsWindow,
                        SetCursorPos, SM_SWAPBUTTON,
                    },
                },
            };
            let own = hwnd(window);
            let ready_now = |this: &Self| {
                if claimed_held {
                    (manual_held && this.manual_preview_cursor_ready()
                        || new_crop_held && this.new_crop_cursor_ready()
                        || crop_adjust_held
                            && this.crop_adjust_cursor_ready()
                            && this.crop_adjust_cursor_window_matches(window))
                        && !this.preview_cursor_replay
                        && unsafe {
                            let (physical_left, physical_right) =
                                if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                                    (VK_RBUTTON, VK_LBUTTON)
                                } else {
                                    (VK_LBUTTON, VK_RBUTTON)
                                };
                            GetFocus() == own
                                && GetCapture() == own
                                && cursor_nudge::manual_preview_cursor_buttons_ready(
                                    GetAsyncKeyState(physical_left.0 as i32) < 0,
                                    [physical_right, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                                        .map(|key| GetAsyncKeyState(key.0 as i32) < 0),
                                )
                        }
                } else if dragging_crop {
                    this.crop_drag_cursor_ready()
                        && unsafe {
                            let physical_left = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                                VK_RBUTTON
                            } else {
                                VK_LBUTTON
                            };
                            GetCapture() == own && GetAsyncKeyState(physical_left.0 as i32) < 0
                        }
                } else {
                    this.preview_cursor_ready()
                }
            };
            if !unsafe { IsWindow(Some(own)) }.as_bool()
                || unsafe { GetForegroundWindow() } != own
                || !ready_now(self)
            {
                return Ok(false);
            }
            // 与冻结底图一致的实际客户区；不把 DIP 或 DPI 缩放混入物理微调。
            if !capture_cursor_client_matches(own, self.source.bounds)? {
                return Ok(false);
            }
            let mut current = POINT::default();
            unsafe {
                GetCursorPos(&mut current)?;
            }
            let Some(target) =
                cursor_nudge::target_in_client(self.source.bounds, (current.x, current.y), delta)
            else {
                return Ok(false);
            };
            // 不夺取焦点；尽量缩短资格复核与实际移动之间的间隙。
            if unsafe { GetForegroundWindow() } != own || !ready_now(self) {
                return Ok(false);
            }
            if target != (current.x, current.y) {
                unsafe {
                    SetCursorPos(target.0, target.1)?;
                }
                if dragging_crop || manual_held || new_crop_held || crop_adjust_held {
                    // ClipCursor 可把成功请求限制在原点；只认实际读回的非零移动。
                    let mut actual = POINT::default();
                    unsafe {
                        GetCursorPos(&mut actual)?;
                    }
                    if new_crop_held || crop_adjust_held {
                        self.selection_pointer_dragged |= if crop_adjust_held {
                            cursor_nudge::crop_adjust_cursor_readback_moved(
                                self.source.bounds,
                                (current.x, current.y),
                                (actual.x, actual.y),
                            )
                        } else {
                            cursor_nudge::new_crop_cursor_readback_moved(
                                self.source.bounds,
                                (current.x, current.y),
                                (actual.x, actual.y),
                            )
                        };
                        if cursor_nudge::target_in_client(
                            self.source.bounds,
                            (actual.x, actual.y),
                            (0, 0),
                        )
                        .is_none()
                        {
                            if crop_adjust_held {
                                self.crop_adjust_cursor_owner = None;
                            }
                            return Ok(false);
                        }
                    } else if (actual.x, actual.y) != (current.x, current.y) {
                        if manual_held && self.manual_preview_click.is_some() {
                            if let Some(start) = self.pointer_down_start {
                                record_manual_preview_cursor_motion(
                                    &mut self.manual_preview_dragged,
                                    start,
                                    (current.x, current.y),
                                    (actual.x, actual.y),
                                    self.source.bounds,
                                    window.scale_factor(),
                                );
                            }
                        }
                        if dragging_crop || manual_held && self.editor.is_changing_crop() {
                            // Up 不应用旧窗口吸附范围覆盖明确的 1px 键盘拖动。
                            self.selection_pointer_dragged = true;
                        }
                    }
                }
            }
            if crop_adjust_held
                && (unsafe { GetForegroundWindow() } != own
                    || !ready_now(self)
                    || !capture_cursor_client_matches(own, self.source.bounds).unwrap_or(false))
            {
                // 实际轨迹已保存；整个旧调整来源撤销，fresh 也不能补旧 owner。
                self.crop_adjust_cursor_owner = None;
                return Ok(false);
            }
            if new_crop_held
                && (unsafe { GetForegroundWindow() } != own
                    || !ready_now(self)
                    || !capture_cursor_client_matches(own, self.source.bounds)?)
            {
                // 先保留真实 sticky 运动，再撤未来同键移动；不能让 owner loss 抹历史。
                return Ok(false);
            }
            if idle {
                // idle 提交也读取真实结果；边缘 no-op 不复用宽泛的 Ok(true) 作为来源。
                let mut actual = POINT::default();
                unsafe { GetCursorPos(&mut actual)? };
                let owner_ready = self.idle_preview_cursor_owner_ready(window, false)
                    && capture_cursor_client_matches(own, self.source.bounds)?;
                idle_commit = cursor_nudge::idle_preview_cursor_commit(
                    self.source.bounds,
                    (current.x, current.y),
                    target,
                    Some((actual.x, actual.y)),
                    target != (current.x, current.y),
                    owner_ready,
                );
            }
            // 后续真实 MouseMove 更新 hover/UIA 或原 CropGesture，不伪造 View::moved。
            Ok(true)
        })();
        match moved {
            Ok(true) => {}
            Ok(false) => {
                self.preview_cursor_nudge.reset();
                if claimed_held {
                    self.manual_preview_cursor_keys.block(event);
                }
            }
            Err(error) => {
                self.preview_cursor_nudge.reset();
                if claimed_held {
                    self.manual_preview_cursor_keys.block(event);
                }
                self.status = trf!("鼠标微调失败：{error}", "Pointer nudge failed: {error}");
                cx.notify();
            }
        }
        if idle {
            self.record_idle_preview_cursor_commit(event, window, idle_commit);
        }
        // 资格、回放或 API 失败也不让已认领的 Arrow 落回 set_crop 取消手势。
        true
    }
    pub(crate) fn invalidate_click_sequence(&mut self) {
        self.cancel_translation_clicks();
        self.startup_click_seed = None;
        self.replay_canvas_click = None;
        self.ordinary_clicks.consumed();
    }
    pub(crate) fn clear_pointer_tracking(&mut self) {
        self.end_ocr_overlay_drag();
        self.text_core_press = None;
        self.cancel_smart_pointer();
        self.result_selection_end(self.result_generation);
        self.replay_canvas_click = None;
        self.ordinary_clicks.consumed();
        self.startup_click_seed = self.startup_click_seed.filter(|seed| seed.native());
        self.pointer_down_start = None;
        self.manual_preview_click = None;
        self.manual_preview_dragged = false;
        self.manual_preview_cursor_claimed = false;
        self.new_crop_cursor_origin = None;
        self.crop_adjust_cursor_origin = None;
        self.crop_adjust_cursor_owner = None;
        self.manual_preview_cursor_keys.cancel_gesture();
        self.clear_idle_preview_cursor();
        self.selection_click_target = None;
        self.selection_pointer_dragged = false;
        self.input.drag_anchor = None;
        self.pencil_samples.clear();
    }
    pub fn new(
        source: CanvasSource,
        previews: Vec<Arc<RenderImage>>,
        focus: FocusHandle,
        crop: Option<Rect>,
        capture_lock: Option<crate::gpui_capture::CaptureLock>,
        cursor: Option<crate::cursor::CapturedCursor>,
        window_targets: Vec<crate::selection_assist::WindowTarget>,
    ) -> Self {
        let current = source.screen_rect(source.current);
        let toolbar_monitor = source.current;
        let mut editor = Editor::new(source.full(), crop);
        let tool_presets = crate::tool_presets::ToolPresets::load().unwrap_or_default();
        editor.restore_tool_styles(tool_presets.styles());
        let preferences = crate::preferences::Preferences::load();
        let mut finish_store = crate::selection_finish_store::SelectionFinishStore::load();
        // 新配置恢复当前样式的完整槽位；旧配置只在迁移时合成一次实际外观。
        let (mut finish_style, mut finish_preset, mut finish_looks) =
            finish_store.session_appearance();
        if source.is_image() {
            // 文件编辑以原样开始，各样式记住的参数保留。
            finish_style.corner_enabled = false;
            finish_style.shadow_enabled = false;
            finish_style.border_enabled = false;
            finish_preset = crate::selection_finish::FinishPreset::Raw;
        }
        finish_looks.set(finish_preset, finish_style);
        finish_store.reset_session_style(finish_style);
        finish_store.reset_session_preset(finish_preset);
        finish_store.reset_session_looks(finish_looks);
        let cursor_preview = cursor.as_ref().map(|c| {
            Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::from_raw(c.image.width, c.image.height, c.image.bgra.clone())
                    .unwrap(),
            )]))
        });
        let view = Self {
            quick_action: Default::default(),
            quick_action_epoch: 0,
            quick_translation_ready: false,
            quick_save_cancel: None,
            quick_link_pending: false,
            quick_completion_message: String::new(),
            quick_completion_warnings: Vec::new(),
            pin_mode: false,
            pin_owner: None,
            pin_window: None,
            pin_editing: false,
            image_saved: None,
            image_discard_armed: None,
            pin_presentation_style: None,
            pin_window_sizing: crate::pin_model::PinWindowSizing::General,
            pin_content_rect: None,
            pin_content_drawn_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pin_presentation_preview: Default::default(),
            pin_render_placement: None,
            pin_annotation_preview: None,
            include_cursor: preferences.include_cursor,
            cursor,
            cursor_preview,
            magnifier: preferences.magnifier,
            guides: preferences.guides,
            show_guides: preferences.guides,
            guide_toggle: Default::default(),
            preview_cursor_nudge: Default::default(),
            idle_preview_cursor_keys: Default::default(),
            idle_preview_cursor_owner: None,
            preview_cursor_replay: false,
            window_targets,
            hover_window: None,
            auto_adsorb: preferences.auto_adsorb,
            control_selection_ready: false,
            control_request_pending: false,
            last_control_request: None,
            control_retry_after: None,
            control_retries_at_point: 0,
            control_pending_navigation: 0,
            control_wheel_remainder: 0.,
            control_hover: None,
            control_tracking: Default::default(),
            selection_click_target: None,
            selection_pointer_dragged: false,
            startup_click_seed: None,
            ordinary_clicks: Default::default(),
            text_core_press: None,
            text_core_activation_observed: false,
            replay_canvas_click: None,
            preview_middle_replay: false,
            preview_enter_replay: false,
            preview_text_replay: false,
            painted_preview: Rc::new(Cell::new(None)),
            painted_selection: Rc::new(Cell::new(None)),
            manual_preview: None,
            manual_preview_click: None,
            manual_preview_dragged: false,
            manual_preview_cursor_claimed: false,
            new_crop_cursor_origin: None,
            crop_adjust_cursor_origin: None,
            crop_adjust_cursor_owner: None,
            manual_preview_cursor_keys: Default::default(),
            manual_preview_replay_point: None,
            magnifier_image: None,
            magnifier_cursor: (12, 12),
            magnifier_sample_point: None,
            magnifier_color: None,
            capture_color_display: Default::default(),
            capture_color_keys_task: None,
            capture_color_keys_eligible: false,
            capture_color_p_down: false,
            capture_color_replay: false,
            pointer_down_start: None,
            pencil_samples: Default::default(),
            capture_lock,
            processing: false,
            smart_candidates: Default::default(),
            pin_handoff_pending: false,
            recognition_result: None,
            ocr_overlay: None,
            result_selection: Default::default(),
            result_generation: 0,
            ocr_find: None,
            capture_find_click: Default::default(),
            local_ocr: None,
            ocr_result_installation: 0,
            result_body_identity: Arc::new(()),
            table_copy: None,
            table_copy_click: None,
            table_copy_down: None,
            typed_pointer_epoch: Arc::new(()),
            typed_pointer_subscriptions: Vec::new(),
            translation: None,
            translation_request: 0,
            translation_copy_click: None,
            translation_close_click: None,
            translation_copy_down: None,
            translation_close_down: None,
            image_translation: Default::default(),
            auto_copy_recovery: None,
            painted_result_output: Rc::new(Cell::new(None)),
            painted_result_projection: Rc::new(std::cell::RefCell::new(None)),
            ocr_projection_epoch: Arc::new(()),
            observed_ocr_projection: None,
            result_click: Rc::new(std::cell::RefCell::new(Default::default())),
            result_body_paint: Rc::new(std::cell::RefCell::new(None)),
            recognition_session: Default::default(),
            foreground_ocr: Default::default(),
            ocr_progress_clock: Default::default(),
            ocr_click_trace: None,
            result_editor: None,
            result_input_handler: None,
            result_input_layout: Rc::new(std::cell::RefCell::new(None)),
            result_reveal_caret: Rc::new(Cell::new(false)),
            result_editor_scroll: gpui::ScrollHandle::new(),
            result_title: tr!("本机文字识别", "Local text recognition").into(),
            source,
            image_canvas: None,
            long_clip_ui: None,
            long_clip_on_done: None,
            long_clip_output_done: false,
            long_clip_placement: None,
            previews,
            editor,
            watermark_field: None,
            watermark_binding: None,
            watermark_field_revision: None,
            watermark_input_focused: false,
            watermark_input_window: None,
            watermark_subscription: None,
            watermark_previews: Vec::new(),
            text_previews: Vec::new(),
            tool_presets,
            focus,
            pointer: (
                (current.x + current.width / 2) as i32,
                (current.y + current.height / 2) as i32,
            ),
            pointer_sample: crate::annotation::Point {
                x: (current.x + current.width / 2) as f32,
                y: (current.y + current.height / 2) as f32,
            },
            toolbar_monitor,
            status: String::new(),
            saving: false,
            color_picker_pending: false,
            color_picker: None,
            property_level: Default::default(),
            ui_positions: Default::default(),
            retained_annotations: Default::default(),
            input: Default::default(),
            light_theme: preferences
                .theme
                .is_light(gpui::WindowAppearance::default()),
            toolbar_panel: ToolbarPanel::None,
            finish_section: Default::default(),
            finish_radius_drag: None,
            selection_size_input: None,
            toolbar_visibility: Default::default(),
            screenshot_toolbar_menu: Default::default(),
            toolbar_position: None,
            toolbar_manual_monitor: None,
            toolbar_snap_preview: None,
            toolbar_config: crate::toolbar_config::ToolbarConfig::load(),
            tool_groups: Default::default(),
            flyout: Default::default(),
            recognize_mode: Default::default(),
            ocr_intent: None,
            table_input: None,
            table_forced: false,
            translation_by_paragraph: false,
            table_hover: None,
            qr: Default::default(),
            toolbar_drag: None,
            refresh_request: None,
            wheel_input: Default::default(),
            wheel_settings: crate::capture_interaction::wheel_settings(),
            raster_previews: Vec::new(),
            retired_raster_previews: Vec::new(),
            editor_preview_updates: 0,
            editor_preview_releases: 0,
            retired_editor_preview_ids: Vec::new(),
            finish_style,
            finish_effect: finish_style.exclusive_effect().unwrap_or_default(),
            finish_effect_intent: None,
            finish_advanced: false,
            finish_preset,
            finish_looks,
            finish_hover: None,
            finish_look_wheel: 0.,
            finish_store,
            finish_preview: None,
            scene: Arc::new(Scene::default()),
            scene_stamp: None,
        };
        for notice in crate::config_recovery::take_notices() {
            view.notify_output_warning(&notice);
        }
        view
    }
    pub fn source_pixels(&self) -> SourcePixels<'_> {
        if let Some(pixels) = self.source.capture_pixels() {
            let mut source = pixels.source_pixels();
            source.overlay = if self.include_cursor {
                self.cursor.as_ref().map(|c| c.plane())
            } else {
                None
            };
            return source;
        }
        SourcePixels {
            fill: self.source.solid_fill(),
            canvas: self.source.full(),
            overlay: if self.include_cursor {
                self.cursor.as_ref().map(|c| c.plane())
            } else {
                None
            },
            planes: self
                .previews
                .iter()
                .enumerate()
                .map(|(i, image)| PixelPlane {
                    rect: self.source.original_rect(i),
                    bgra: self
                        .source
                        .original(image)
                        .as_bytes(0)
                        .expect("original frozen pixels"),
                })
                .collect(),
        }
    }
    fn capture_color_keys() -> (bool, bool) {
        unsafe {
            (
                [VK_SHIFT, VK_LSHIFT, VK_RSHIFT]
                    .iter()
                    .any(|key| GetAsyncKeyState(key.0 as i32) < 0),
                GetAsyncKeyState(VK_P.0 as i32) < 0,
            )
        }
    }
    fn capture_color_available(&self, window: &Window, require_preview: bool) -> bool {
        crate::capture_color_pick::eligible(
            &self.editor,
            crate::capture_color_pick::Availability {
                ordinary: !self.pin_mode && self.image_canvas.is_none(),
                ready: self.control_selection_ready
                    && crate::preview_middle_pin::physical_size_matches(
                        self.source.full(),
                        (
                            f32::from(window.viewport_size().width),
                            f32::from(window.viewport_size().height),
                        ),
                        window.scale_factor(),
                    ),
                enabled: self.magnifier,
                preview_available: !require_preview
                    || (self.magnifier_image.is_some()
                        && self.magnifier_color.is_some()
                        && self.magnifier_sample_point
                            == Some((self.pointer, self.include_cursor))),
                active: window.is_window_active(),
                busy: self.saving
                    || self.processing
                    || self.pin_handoff_pending
                    || self.pointer_down_start.is_some()
                    || self.toolbar_drag.is_some()
                    || self.recognition_result.is_some(),
                input_active: self.watermark_input_focused
                    || self.editor.watermark.has_preview()
                    || self.input.marked.is_some()
                    || self.input.drag_anchor.is_some()
                    || self.result_editor.is_some(),
                menu_open: self.toolbar_panel != ToolbarPanel::None
                    || self.screenshot_toolbar_menu.presentation().is_some(),
                replay: self.capture_color_replay,
            },
        )
    }
    fn clear_magnifier_sample(&mut self) -> bool {
        let changed = self.magnifier_image.take().is_some();
        self.magnifier_sample_point = None;
        self.magnifier_color = None;
        changed
    }
    fn stop_capture_color(&mut self) -> bool {
        let changed = self.clear_magnifier_sample()
            | self.capture_color_keys_task.take().is_some()
            | self.capture_color_keys_eligible;
        self.capture_color_keys_eligible = false;
        self.capture_color_p_down = false;
        changed
    }
    /// 冻结底图的旧24像素邻域保留；标签和复制读取同一裁片的实际中心。
    fn refresh_magnifier_sample(&mut self) -> bool {
        let canvas = self.source.full();
        if !self.magnifier || self.editor.crop.is_some() || !canvas.contains(self.pointer) {
            self.clear_magnifier_sample();
            return false;
        }
        if self.magnifier_sample_point == Some((self.pointer, self.include_cursor))
            && self.magnifier_image.is_some()
            && self.magnifier_color.is_some()
        {
            return true;
        }
        let Some(sample) = crate::capture_color_pick::sample(&self.source_pixels(), self.pointer)
        else {
            self.clear_magnifier_sample();
            return false;
        };
        // 放大镜按 CPU 小片逐格绘制，保持像素硬边，不上传纹理。
        self.magnifier_cursor = sample.center;
        self.magnifier_color = Some(sample.rgb);
        self.magnifier_sample_point = Some((self.pointer, self.include_cursor));
        self.magnifier_image = Some(Arc::new(sample.pixels));
        true
    }
    fn current_capture_color_pointer(&mut self) -> bool {
        let mut cursor = windows::Win32::Foundation::POINT::default();
        if unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) }.is_err() {
            return false;
        }
        let Some(pointer) = cursor
            .x
            .checked_sub(self.source.bounds.left)
            .zip(cursor.y.checked_sub(self.source.bounds.top))
        else {
            return false;
        };
        if !self.source.full().contains(pointer) {
            return false;
        }
        if self.pointer != pointer {
            self.pointer = pointer;
            self.pointer_sample = crate::annotation::Point::from_tuple(pointer);
            self.invalidate_control_hover();
        }
        true
    }
    pub(crate) fn observe_capture_color_keys(&mut self, window: &Window, _cx: &mut Context<Self>) {
        if self.pin_mode || self.image_canvas.is_some() || !self.magnifier {
            return;
        }
        let allowed = self.capture_color_available(window, true);
        if allowed && !self.capture_color_keys_eligible {
            // Native 状态只用于恢复播种；后续格式边沿由前台事件单独拥有。
            let (shift, p) = Self::capture_color_keys();
            self.capture_color_p_down = p;
            self.capture_color_display.observe(shift, p, false);
        }
        self.capture_color_keys_eligible = allowed;
    }
    fn update_capture_color_keys(
        &mut self,
        shift: bool,
        p: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let allowed = self.capture_color_available(window, true);
        // 每次资格恢复播种当前按键，保留同一截图内的格式，拒绝迟到边沿。
        let changed = self.capture_color_display.observe(
            shift,
            p,
            allowed && self.capture_color_keys_eligible,
        );
        self.capture_color_keys_eligible = allowed;
        if changed {
            cx.notify();
        }
    }
    pub(crate) fn capture_color_modifiers_changed(
        &mut self,
        shift: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.pin_mode || self.image_canvas.is_some() || !self.magnifier {
            return;
        }
        self.update_capture_color_keys(shift, self.capture_color_p_down, window, cx);
    }
    pub(crate) fn capture_color_key_up(
        &mut self,
        event: &gpui::KeyUpEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.pin_mode
            || self.image_canvas.is_some()
            || !self.magnifier
            || event.keystroke.key != "p"
        {
            return;
        }
        self.capture_color_p_down = false;
        self.update_capture_color_keys(event.keystroke.modifiers.shift, false, window, cx);
    }
    pub(crate) fn prepare_capture_magnifier(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.capture_color_available(window, false) {
            self.stop_capture_color();
            return;
        }
        if self.magnifier_image.is_none() && !self.current_capture_color_pointer() {
            self.stop_capture_color();
            return;
        }
        if !self.refresh_magnifier_sample() {
            self.stop_capture_color();
            return;
        }
        self.observe_capture_color_keys(window, cx);
        if self.capture_color_keys_task.is_none() {
            self.capture_color_keys_task = Some(cx.spawn_in(window, async move |view, cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let keep = view
                    .update_in(cx, |view, window, cx| {
                        if !view.capture_color_available(window, true) {
                            if view.stop_capture_color() {
                                cx.notify();
                            }
                            return false;
                        }
                        view.observe_capture_color_keys(window, cx);
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }));
        }
    }
    pub(crate) fn capture_color_labels(
        &self,
        preview: Option<Rect>,
        window: &Window,
    ) -> Option<crate::capture_color_pick::Labels> {
        if !self.capture_color_available(window, true) {
            return None;
        }
        crate::capture_color_pick::labels(
            &self.capture_color_display,
            self.magnifier_color?,
            self.pointer,
            self.source.bounds,
            preview,
        )
    }
    fn copy_capture_color(&mut self, window: &Window, cx: &mut Context<Self>) {
        // 已显示的放大镜才拥有这组新取色键；失效时不能复制旧样本或回落图片输出。
        if !self.capture_color_available(window, true)
            || !self.current_capture_color_pointer()
            || !self.refresh_magnifier_sample()
        {
            self.clear_magnifier_sample();
            self.status = tr!(
                "当前放大镜取色不可用",
                "Color picking from the magnifier is unavailable"
            )
            .into();
            cx.notify();
            return;
        }
        let Some(labels) = self.capture_color_labels(None, window) else {
            self.clear_magnifier_sample();
            self.status = tr!("当前取色坐标无效", "The color picking position is invalid").into();
            cx.notify();
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(labels.color.clone()));
        self.status = trf!("已复制颜色 {}", "Color copied {}", labels.color);
        cx.notify();
    }
    pub fn prepared_scene(&mut self) -> Arc<Scene> {
        let stamp = self.editor.scene_stamp();
        if self.scene_stamp != Some(stamp) {
            self.scene = Arc::new(self.scene.updated_live(
                &self.editor.annotations(),
                &self.source_pixels(),
                self.editor.live_stroke_draft_index(),
                &self.editor.live_object_indices(),
            ));
            self.scene_stamp = Some(stamp);
            if let Some(error) = self.scene.errors.first() {
                self.status = error.clone();
            }
            let old = std::mem::take(&mut self.raster_previews);
            for layer in &self.scene.layers {
                let images: Vec<_> = match layer {
                    crate::scene::Layer::Raster { image, .. }
                    | crate::scene::Layer::Restore { image, .. } => vec![image],
                    crate::scene::Layer::Highlighter { tiles, .. } => {
                        tiles.iter().map(|tile| &tile.image).collect()
                    }
                    crate::scene::Layer::LocalFill { patches } => {
                        patches.iter().map(|patch| &patch.image).collect()
                    }
                    crate::scene::Layer::EffectRegions { patches } => {
                        patches.iter().map(|patch| &patch.image).collect()
                    }
                    _ => Vec::new(),
                };
                for image in images {
                    let preview = self
                        .raster_previews
                        .iter()
                        .chain(old.iter())
                        .find(|(cached, _)| Arc::ptr_eq(cached, image))
                        .map(|(_, p)| p.clone())
                        .unwrap_or_else(|| {
                            let buffer = image::RgbaImage::from_raw(
                                image.width,
                                image.height,
                                if matches!(layer, crate::scene::Layer::Restore { .. }) {
                                    let rect = match layer {
                                        crate::scene::Layer::Restore { rect, .. } => *rect,
                                        _ => unreachable!(),
                                    };
                                    self.source.restore_preview(image, rect)
                                } else {
                                    image.bgra.clone()
                                },
                            )
                            .expect("annotation dimensions");
                            Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
                        });
                    self.raster_previews.push((image.clone(), preview));
                }
            }
            for (_, preview) in old {
                if !self
                    .raster_previews
                    .iter()
                    .any(|(_, current)| Arc::ptr_eq(current, &preview))
                    && !self
                        .retired_raster_previews
                        .iter()
                        .any(|retired| Arc::ptr_eq(retired, &preview))
                {
                    self.retired_raster_previews.push(preview);
                }
            }
        }
        self.scene.clone()
    }
    /// 所有结束草稿的入口共用准备门。取色只使用已提交文档，不含蓝色预览。
    pub(crate) fn try_finish_local_fill(&mut self) -> Result<()> {
        let Some(candidate) = self.editor.local_fill_candidate() else {
            return Ok(());
        };
        let prepared_scene = {
            let source = self.source_pixels();
            let below = self
                .scene
                .updated(&self.editor.document_annotations(), &source);
            if !below.errors.is_empty() {
                return Err(below.errors.join(";").into());
            }
            match crate::local_fill_scene::prepare(&candidate, &source, &below)? {
                Some(prepared) => Some(below.with_prepared_fill(&candidate, prepared, &source)?),
                None => None,
            }
        };
        if let Some(scene) = prepared_scene {
            if !self.editor.commit_prepared_local_fill(&candidate) {
                return Err(tr!(
                    "局部填充区域已变化，请重试",
                    "The local fill area changed; please retry"
                )
                .into());
            }
            self.scene = Arc::new(scene);
        } else {
            // 完全没有有效覆盖为主动 no-op；不插入对象，不清 redo。
            self.editor.cancel_gesture();
        }
        self.scene_stamp = None;
        Ok(())
    }
    pub(crate) fn try_finish_content(&mut self) -> Result<()> {
        if self.text_composing() || (self.text_core_press.is_some() && self.input.marked.is_some())
        {
            return Err(tr!("请先完成输入法组合文字", "Finish the IME composition first").into());
        }
        self.try_finish_local_fill()?;
        if !self.editor.finish_gesture() {
            return Err(tr!("局部填充尚未准备完成", "Local fill isn't ready yet").into());
        }
        let owner = self
            .text_core_press
            .as_ref()
            .and_then(|press| press.capture_owner);
        if crate::capture_text_selection::terminate_text_core_press(
            &mut self.text_core_press,
            &mut self.pointer_down_start,
            &self.input,
        ) {
            self.clear_pointer_tracking();
            if owner.is_some_and(|owner| owner != 0 && unsafe { GetCapture() }.0 as usize == owner)
            {
                self.release_capture();
            }
        }
        self.editor.finish_text();
        self.editor.finish_watermark_input();
        self.reset_input();
        Ok(())
    }
    pub(crate) fn finish_content_or_status(&mut self) -> bool {
        match self.try_finish_content() {
            Ok(()) => true,
            Err(error) => {
                self.status = error.to_string();
                false
            }
        }
    }
    pub fn selected(&self) -> Result<Rect> {
        crate::capture_pin_output::selected_host(
            self.source.full(),
            self.editor.crop,
            self.pin_content_rect.filter(|_| self.pin_mode),
        )
    }
    pub fn output_rect(&self) -> Result<Rect> {
        self.output_rect_for(self.selected()?)
    }
    pub(crate) fn output_rect_for(&self, host: Rect) -> Result<Rect> {
        crate::capture_pin_output::content_output_rect(
            self.source.full(),
            Some(host),
            self.pin_content_rect.filter(|_| self.pin_mode),
            self.editor
                .lasso_draft
                .as_ref()
                .or(self.editor.document.crop_mask.as_ref()),
        )
    }
    pub fn selected_image(&mut self) -> Result<Image> {
        let image = self.selected_content_image()?;
        self.effective_finish_style().compose(image)
    }
    /// 只有冻结桌面截图拥有可恢复的屏幕位置；文件编辑与贴图导出没有截图原点。
    pub(crate) fn clipboard_image_origin(&self, content: Rect) -> Option<(i32, i32)> {
        (!self.pin_mode && self.image_canvas.is_none()).then(|| self.finished_origin(content))
    }
    /// 输出装饰不进入识别坐标；标注和套索依然使用全屏坐标合成。
    pub(crate) fn selected_content_image(&mut self) -> Result<Image> {
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.drag.is_some() || ui.draft.is_some() || ui.parameters_pending)
        {
            return Err(tr!(
                "请先完成或取消剪辑手势",
                "Finish or cancel the clip gesture first"
            )
            .into());
        }
        self.try_finish_local_fill()?;
        self.flush_tool_presets();
        if self.editor.document.long_clip().is_some() {
            let scene = self.prepared_scene();
            return crate::capture_pin_output::clipped_content_image(
                &self.source,
                &scene,
                self.editor.document.long_clip().expect("剪辑文档"),
                self.editor
                    .lasso_draft
                    .as_ref()
                    .or(self.editor.document.crop_mask.as_ref()),
            );
        }
        let selected = self.output_rect()?;
        self.content_image_in(selected)
    }
    /// 显式输出范围只裁剪现有全屏场景；不提交草稿或修改 crop、选择和历史。
    pub(crate) fn content_image_in(&mut self, selected: Rect) -> Result<Image> {
        let scene = self.prepared_scene();
        crate::capture_pin_output::content_image(
            &self.source,
            &scene,
            selected,
            self.cursor.as_ref().filter(|_| self.include_cursor),
            self.editor
                .lasso_draft
                .as_ref()
                .or(self.editor.document.crop_mask.as_ref()),
        )
    }
    pub(crate) fn physical(&self, p: gpui::Point<gpui::Pixels>, window: &Window) -> (i32, i32) {
        self.world_point(p, window).rounded()
    }
    pub(crate) fn world_point(
        &self,
        p: gpui::Point<gpui::Pixels>,
        window: &Window,
    ) -> crate::annotation::Point {
        let p = crate::image_editor::client_point(p, window);
        self.clip_source_point(self.canvas_viewport(window).image_point(p))
    }
    pub(crate) fn drawing_pointer(&self, shift: bool) -> crate::annotation::Point {
        self.drawing_pointer_with_preview_source(shift, self.manual_preview_cursor_claimed)
    }
    fn drawing_down_pointer(&self, shift: bool) -> crate::annotation::Point {
        // 草稿续点前先用真实指针命中已有折线或箭头，避免按草稿锚点吸附而点偏。
        // 同一宿主覆盖全屏截图、图片编辑和贴图编辑；Move/Up 保留原约束路线。
        if self.editor.has_pending_drawing() {
            if self.image_canvas.is_some() {
                self.pointer_sample
            } else {
                crate::annotation::Point::from_tuple(self.pointer)
            }
        } else {
            self.drawing_pointer(shift)
        }
    }
    fn drawing_pointer_with_preview_source(
        &self,
        shift: bool,
        manual_source: bool,
    ) -> crate::annotation::Point {
        if self.image_canvas.is_some() {
            self.editor.constrained_point(self.pointer_sample, shift)
        } else {
            crate::annotation::Point::from_tuple(cursor_nudge::preview_crop_pointer(
                &self.editor,
                self.pointer,
                shift,
                !self.pin_mode,
                manual_source,
            ))
        }
    }
    fn text_core_ready(&self, window: &Window, cx: &Context<Self>) -> bool {
        window.is_window_active()
            && self.focus.is_focused(window)
            && !self.saving
            && !self.processing
            && !self.pin_handoff_pending
            && (!self.pin_mode || self.pin_editing)
            && !(self.pin_mode
                && self
                    .pin_owner
                    .as_ref()
                    .and_then(|v| v.upgrade())
                    .is_some_and(|v| v.read(cx).gesture.is_some()))
            && (self.pin_mode || self.image_canvas.is_some() || self.control_selection_ready)
            && (self.pin_mode || self.image_canvas.is_some() || self.capture_lock.is_some())
            && self.manual_preview.is_none()
            && self.toolbar_drag.is_none()
            && self.toolbar_panel == ToolbarPanel::None
            && self.screenshot_toolbar_menu.presentation().is_none()
            && self.result_editor.is_none()
            && self.recognition_result.is_none()
            && !self.watermark_input_focused
            && self.input.marked.is_none()
    }
    pub(crate) fn observe_text_core_activation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text_core_activation_observed {
            return;
        }
        self.text_core_activation_observed = true;
        cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() && view.long_clip_cancel_gesture(window, cx) {
                cx.notify();
            }
            if !window.is_window_active() && view.inline_ocr_host() {
                view.invalidate_inline_ocr_projection();
                if let Some(editor) = &mut view.result_editor {
                    editor.cancel_composition();
                }
                cx.notify();
            }
            if !window.is_window_active() && view.cancel_image_pan(window) {
                cx.notify();
            }
            if !window.is_window_active() && view.ocr_overlay_dragging() {
                view.end_ocr_overlay_drag();
                cx.notify();
            }
            if !window.is_window_active() && view.text_core_press.is_some() {
                // 只撤输入归属；已进入的文字草稿和文档历史保留。
                view.clear_pointer_tracking();
                cx.notify();
                window.defer(cx, |window, _| {
                    if !window.is_window_active() && unsafe { GetCapture() } == hwnd(window) {
                        unsafe {
                            let _ = ReleaseCapture();
                        }
                    }
                });
            }
        })
        .detach();
    }
    fn text_press_carets(
        &self,
        press: &crate::capture_text_selection::TextCorePress,
        point: crate::annotation::Point,
        window: &Window,
    ) -> Result<(usize, usize)> {
        let scale = self.canvas_scale(window);
        let geometry = crate::text_geometry::TextGeometry::new(&press.original, scale)?;
        let b = press.original.local_bounds();
        let hit = |point| {
            let local = press.original.to_local(point);
            geometry.hit(gpui::point(
                gpui::px((local.x - b.x) / scale),
                gpui::px((local.y - b.y) / scale),
            ))
        };
        let anchor = hit(press.point);
        Ok((anchor, if press.dragged { hit(point) } else { anchor }))
    }
    pub fn down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        crate::remote_display::observe_input_source();
        if self.long_clip_down(e, window, cx) {
            return;
        }
        if self.refresh_request.is_some() {
            return;
        }
        if self.text_core_press.is_some() {
            self.clear_pointer_tracking();
        }
        // Down 后 Editor 会清选择/结束旧态，不能据此补造本次 New 的来源。
        // 可信 Native replay 仍沿原事件 Down 建 New；只禁旧按键资格迟到移鼠。
        let new_crop_down_ready = self.preview_cursor_ready() && self.result_editor.is_none();
        let crop_adjust_down_ready = self.crop_adjust_cursor_down_ready();
        // 必须早于新 Down 的清理；只暂存本 View 的实时提交，重放不能读取当下系统状态。
        let idle_candidate = if e.button == gpui::MouseButton::Left {
            let eligible = idle_preview_cursor_snapshot_ready(
                self.replay_canvas_click.is_some() || self.preview_cursor_replay,
                e.click_count,
                e.modifiers,
                self.preview_cursor_ready(),
            );
            let snapshot = self
                .idle_preview_cursor_keys
                .snapshot(&self.preview_cursor_nudge);
            let owner = self.idle_preview_cursor_owner.take();
            if eligible && !snapshot.is_empty() {
                owner.map(|owner| (snapshot, owner))
            } else {
                None
            }
        } else {
            self.clear_idle_preview_cursor();
            None
        };
        if e.button == gpui::MouseButton::Left {
            self.manual_preview_cursor_claimed = false;
            self.new_crop_cursor_origin = None;
            self.crop_adjust_cursor_origin = None;
            self.crop_adjust_cursor_owner = None;
            self.manual_preview_cursor_keys.cancel_gesture();
        }
        // 新画布点击不能接续已丢失抬键的结果选字手势。
        self.result_selection_end(self.result_generation);
        if self.screenshot_toolbar_menu.dismiss() {
            self.invalidate_click_sequence();
            self.wheel_input.reset();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // 每次 Down 消费完成首击；控件/忙碌拒绝后不能遗留给后续手势。
        let replay = self.replay_canvas_click.take();
        let click_at = replay.map_or_else(std::time::Instant::now, |replay| replay.received_at);
        let click_point = replay.map_or_else(
            || self.physical(e.position, window),
            |replay| replay.physical_point,
        );
        let click_seed = self
            .startup_click_seed
            .take()
            .or_else(|| replay.and_then(|replay| replay.seed));
        let click_count = click_seed.map_or(e.click_count, |seed| {
            seed.click_count(e.click_count, click_point, click_at)
        });
        let ordinary_click_count =
            self.ordinary_clicks
                .count(click_seed, e.click_count, click_point, click_at);
        if self.pin_handoff_pending {
            self.invalidate_click_sequence();
            return;
        }
        if self.pin_mode {
            if !self.pin_editing || e.modifiers.control && self.editor.text_draft.is_none() {
                return;
            }
            cx.stop_propagation();
        }
        if self.saving {
            self.invalidate_click_sequence();
            return;
        }
        if self.text_composing() {
            // 组合串由输入法提交；画布点击不能提前把预编辑内容写入文档。
            self.invalidate_click_sequence();
            cx.stop_propagation();
            return;
        }
        let flyout_pinned = self.flyout.pinned();
        if self.flyout.close() {
            cx.notify();
            // 点击打开的下拉像菜单一样吃掉这次点击；悬停展开的只收起，不打断绘制。
            if flyout_pinned {
                self.invalidate_click_sequence();
                return;
            }
        }
        if self.toolbar_panel != ToolbarPanel::None {
            self.toolbar_panel = ToolbarPanel::None;
            self.selection_size_input = None;
            window.focus(&self.focus);
            self.invalidate_click_sequence();
            cx.notify();
            return;
        }
        if self.ocr_overlay_down(e, window, cx) {
            self.invalidate_click_sequence();
            self.wheel_input.reset();
            return;
        }
        if self.result_selection_pointer_ready() {
            // 完成结果的空白点击只属于结果层，不能重新开始截图或标注。
            self.invalidate_click_sequence();
            self.wheel_input.reset();
            cx.stop_propagation();
            return;
        }
        self.pointer_sample = self.world_point(e.position, window);
        self.pointer = self.physical(e.position, window);
        self.invalidate_control_hover();
        // 保存入口当时资格；既有 fill 准备不能让本次点击从草稿状态变成宿主调整。
        let screenshot_crop_outside = self
            .screenshot_crop_outside_target(6. * self.canvas_scale(window))
            .is_some();
        let screenshot_crop_inside = ordinary_click_count == 1
            && self
                .screenshot_empty_crop_inside_target(6. * self.canvas_scale(window), e.modifiers)
                .is_some();
        let screenshot_crop_center = ordinary_click_count == 1
            && self
                .screenshot_empty_crop_center_target(6. * self.canvas_scale(window), e.modifiers);
        if let Err(error) = self.try_finish_local_fill() {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        let double_click_busy =
            self.processing || self.pointer_down_start.is_some() || self.toolbar_drag.is_some();
        let text_blank_tolerance = 6. * self.canvas_scale(window);
        let text_blank_canvas = self.source.full();
        let text_blank_finished = crate::capture_double_click::finish_ordinary_text_blank_down(
            &mut self.editor,
            self.pointer_sample,
            text_blank_tolerance,
            text_blank_canvas,
            crate::capture_double_click::CanvasClickUp {
                screenshot: self.image_canvas.is_none() && !self.pin_mode,
                modified: e.modifiers.modified(),
                busy: double_click_busy,
                input_active: self.watermark_input_focused
                    || self.input.marked.is_some()
                    || self.recognition_result.is_some(),
                owned: self.control_selection_ready
                    && self.manual_preview.is_none()
                    && self.result_editor.is_none()
                    && self.input.drag_anchor.is_none(),
            },
        );
        if text_blank_finished {
            self.reset_input();
        }
        let ordinary_canvas_ready = self.control_selection_ready
            && crate::capture_double_click::ordinary_canvas_click_ready(
                &self.editor,
                self.image_canvas.is_none() && !self.pin_mode,
                e.modifiers.modified(),
                double_click_busy,
                self.watermark_input_focused
                    || self.input.marked.is_some()
                    || self.recognition_result.is_some(),
            );
        if ordinary_click_count == 1 && ordinary_canvas_ready {
            self.ordinary_clicks.begin(
                ordinary_click_count,
                true,
                click_point,
                click_at,
                std::time::Duration::from_millis(unsafe { GetDoubleClickTime() } as u64),
                unsafe {
                    (
                        GetSystemMetrics(SM_CXDOUBLECLK),
                        GetSystemMetrics(SM_CYDOUBLECLK),
                    )
                },
            );
        } else if !ordinary_canvas_ready {
            self.ordinary_clicks.consumed();
        }
        let double_click = crate::capture_double_click::ordinary_double_click(
            &self.editor,
            self.source.full(),
            ordinary_click_count,
            self.image_canvas.is_none() && !self.pin_mode,
            e.modifiers.modified(),
            double_click_busy,
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some(),
        );
        let manual_preview_at_down = self.manual_preview;
        if let Some(rect) = manual_preview_at_down {
            if crate::capture_monitor_snap::hit_preview(rect, self.pointer, window.scale_factor()) {
                if !self.screenshot_preview_confirm_ready(rect) {
                    self.invalidate_click_sequence();
                    cx.stop_propagation();
                    return;
                }
                // 内区按下保持 Preview，抬起才确认；保存当前固定范围，输出另验真实呈现。
                self.selection_click_target = None;
                self.manual_preview_click = Some(rect);
                self.manual_preview_dragged = false;
                self.manual_preview_cursor_claimed = true;
                self.selection_pointer_dragged = false;
                self.pointer_down_start = Some(self.pointer);
                self.suspend_control_tracking();
                self.wheel_input.reset();
                self.status.clear();
                unsafe {
                    SetCapture(hwnd(window));
                }
                if click_count == 1 && ordinary_click_count == 1 {
                    if let Some((snapshot, owner)) = idle_candidate {
                        self.adopt_idle_preview_cursor(snapshot, owner, window, e.modifiers);
                    }
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            // 外侧 Down 是新框选，不允许之后借屏幕范围完成旧 click_selection。
            self.set_manual_preview(None);
        }
        self.selection_click_target = if manual_preview_at_down.is_some() {
            None
        } else {
            self.click_selection()
        };
        self.suspend_control_tracking();
        self.selection_pointer_dragged = false;
        self.pointer_down_start = Some(self.pointer);
        self.pencil_samples.clear();
        self.wheel_input.reset();
        self.status.clear();
        if text_blank_finished && double_click.starts_pointer() {
            // 第一击已结束文字编辑，保留原 Down 给真实 Up，不创建下一只空文本框。
            let owner = hwnd(window);
            unsafe {
                SetCapture(owner);
            }
            if unsafe { GetCapture() } != owner {
                // 捕获失败仍保留已提交文字，但不能发布普通双击的首击证明。
                self.clear_pointer_tracking();
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if double_click.starts_pointer()
            && screenshot_crop_outside
            && self
                .editor
                .begin_screenshot_crop_outside(self.pointer, 6. * self.canvas_scale(window))
        {
            // 截图宿主范围优先于外侧对象、Ctrl 多选与双击编辑；保留当前 Edit 和对象选择。
            unsafe {
                SetCapture(hwnd(window));
            }
            self.accept_crop_adjust_cursor_down(crop_adjust_down_ready, window);
            cx.notify();
            return;
        }
        if double_click.starts_pointer()
            && self.smart_pointer_down(ordinary_canvas_ready, window, cx)
        {
            return;
        }
        if let Some(draft) = &self.editor.text_draft {
            if (draft.annotation.tool != Tool::Text
                && draft.annotation.hit(self.pointer_sample, 0., true))
                || crate::capture_text_selection::text_core_hit(
                    &draft.annotation,
                    self.pointer_sample,
                    6. * self.canvas_scale(window),
                    self.canvas_scale(window),
                )
            {
                if let Ok(geometry) = crate::text_geometry::TextGeometry::new(
                    &draft.annotation,
                    self.canvas_scale(window),
                ) {
                    let b = draft.annotation.local_bounds();
                    let local = draft.annotation.to_local(crate::annotation::Point {
                        x: self.pointer_sample.x,
                        y: self.pointer_sample.y,
                    });
                    let text_point = gpui::point(
                        gpui::px((local.x - b.x) / self.canvas_scale(window)),
                        gpui::px((local.y - b.y) / self.canvas_scale(window)),
                    );
                    let caret = geometry.hit(text_point);
                    let (anchor, caret) = crate::text_input::edit_boundary::selection_to(
                        &draft.annotation.text,
                        self.input.selection.clone(),
                        self.input.reversed,
                        caret,
                        e.modifiers.shift,
                    );
                    if click_count == 2 && draft.annotation.tool == Tool::Text {
                        if let Some(range) = geometry.visual_line_at(text_point) {
                            crate::capture_text_selection::select_visual_line(
                                &mut self.input,
                                &draft.annotation.text,
                                range,
                            );
                            // 行选择消费 Down；随后 Move 不重新塌回单个光标。
                            self.input.drag_anchor = None;
                            self.invalidate_click_sequence();
                            self.pointer_down_start = None;
                            if unsafe { GetCapture() } == hwnd(window) {
                                self.release_capture();
                            }
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                    }
                    self.input.select_to(anchor, caret);
                    if click_count >= 2 {
                        self.input.select_to(0, draft.annotation.text.len());
                    }
                    self.input.drag_anchor = Some(anchor);
                    unsafe {
                        SetCapture(hwnd(window));
                    }
                    cx.notify();
                    return;
                }
            }
        }

        if e.button == gpui::MouseButton::Left
            && !e.modifiers.control
            && !e.modifiers.alt
            && !e.modifiers.platform
            && !e.modifiers.function
        {
            if let Some(mut press) = crate::capture_text_selection::TextCorePress::candidate(
                &self.editor,
                self.pointer_sample,
                6. * self.canvas_scale(window),
                self.canvas_scale(window),
                !self.pin_mode && self.image_canvas.is_none(),
            ) {
                // 正文首击不属于宿主普通双击，也不开始 Object Move。
                self.invalidate_click_sequence();
                self.selection_click_target = None;
                if !self.text_core_ready(window, cx) || double_click_busy {
                    self.clear_pointer_tracking();
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                let owner = hwnd(window);
                unsafe {
                    SetCapture(owner);
                }
                if unsafe { GetCapture() } == owner && press.accept(&mut self.editor) {
                    press.capture_owner = Some(owner.0 as usize);
                    self.text_core_press = Some(press);
                    self.observe_text_core_activation(window, cx);
                } else {
                    self.clear_pointer_tracking();
                    if unsafe { GetCapture() } == owner {
                        self.release_capture();
                    }
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        if click_count == 2
            && self.editor.insert_polyline_node_at_with_shift(
                self.drawing_down_pointer(e.modifiers.shift),
                6. * self.canvas_scale(window),
                e.modifiers.shift,
            )
        {
            self.release_capture();
            cx.notify();
            return;
        }
        if click_count == 2
            && self.editor.text_draft.is_none()
            && self.editor.selected_annotation().is_some_and(|a| {
                a.tool.text()
                    && a.hit(
                        crate::annotation::Point {
                            x: self.pointer.0 as f32,
                            y: self.pointer.1 as f32,
                        },
                        0.,
                        true,
                    )
            })
        {
            self.editor.edit_text();
            self.reset_input();
            cx.notify();
            return;
        }
        if click_count == 2
            && e.button == gpui::MouseButton::Left
            && !e.modifiers.modified()
            && !double_click_busy
            && !self.group_navigation_blocked()
            && self
                .editor
                .enter_group_scope_at(self.pointer_sample, 6. * self.canvas_scale(window))
        {
            self.clear_pointer_tracking();
            self.release_capture();
            self.reset_input();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if double_click != crate::capture_double_click::DoubleClickAction::None {
            // 普通双击不开始新手势；文字选取/编辑和折线落点分支始终先处理。
            self.clear_pointer_tracking();
            if unsafe { GetCapture() } == hwnd(window) {
                self.release_capture();
            }
            cx.stop_propagation();
            self.perform(
                match double_click {
                    crate::capture_double_click::DoubleClickAction::Confirm => Action::Confirm,
                    crate::capture_double_click::DoubleClickAction::Edit => {
                        Action::Tool(Tool::Edit)
                    }
                    crate::capture_double_click::DoubleClickAction::None => unreachable!(),
                },
                window,
                cx,
            );
            // perform 会清输入链；普通 double 最后完成，不能让 raw3/4 再播首击。
            self.ordinary_clicks.after_double();
            return;
        }
        if e.modifiers.control && self.editor.tool == Tool::Edit {
            if self.image_canvas.is_some() {
                self.editor
                    .toggle_selection_at(self.pointer_sample, 6. * self.canvas_scale(window));
            } else {
                self.editor
                    .toggle_selection(self.pointer, 6. * self.canvas_scale(window));
            }
        } else if !((screenshot_crop_inside
            && self
                .editor
                .begin_screenshot_empty_crop_inside(self.pointer, 6. * self.canvas_scale(window)))
            || (screenshot_crop_center
                && self.editor.begin_screenshot_empty_crop_center(
                    self.pointer,
                    6. * self.canvas_scale(window),
                )))
        {
            let p = self.drawing_down_pointer(e.modifiers.shift);
            if self.image_canvas.is_some() {
                self.editor
                    .pointer_down_at(p, 6. * self.canvas_scale(window), e.modifiers.shift);
            } else if e.modifiers.shift {
                self.editor.pointer_down_with_shift(
                    p.rounded(),
                    6. * self.canvas_scale(window),
                    true,
                );
            } else {
                self.editor
                    .pointer_down(p.rounded(), 6. * self.canvas_scale(window));
            }
        }
        if self.editor.text_draft.is_some() {
            self.reset_input();
        }
        if self.editor.drawing_freehand_pen() {
            self.pencil_samples.begin(
                hwnd(window),
                crate::image_editor::client_point(e.position, window),
            );
        }
        if self.editor.is_dragging() {
            unsafe {
                SetCapture(hwnd(window));
            }
        }
        // 使用实际接受 Down 保存的点，绝不从当前 OS 指针补原点。
        self.new_crop_cursor_origin = admitted_new_crop_cursor_origin(
            &self.editor,
            self.pointer_down_start,
            !self.pin_mode && self.image_canvas.is_none(),
            new_crop_down_ready,
        );
        self.accept_crop_adjust_cursor_down(crop_adjust_down_ready, window);
        cx.notify();
    }
    pub fn moved(&mut self, e: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        // GPUI 在窗口过程内同步派发，此处仍能读到本条输入的来源。
        crate::remote_display::observe_input_source();
        if self.long_clip_moved(e, window, cx) {
            return;
        }
        if self.refresh_request.is_some() {
            return;
        }
        // 图片中键平移先于原位选字；只改视口，不改 OCR 的原图身份。
        if self.pan_move(e, window, cx) {
            self.pointer_sample = self.world_point(e.position, window);
            self.pointer = self.physical(e.position, window);
            return;
        }
        if self.ocr_overlay_moved(e, window, cx) {
            return;
        }
        if let Some(mut press) = self.text_core_press.take() {
            self.pointer_sample = self.world_point(e.position, window);
            self.pointer = self.physical(e.position, window);
            let ready = self.text_core_ready(window, cx)
                && press.valid(&self.editor)
                && press.owns_pointer(self.pointer_down_start)
                && e.pressed_button == Some(gpui::MouseButton::Left)
                && !e.modifiers.control
                && !e.modifiers.alt
                && !e.modifiers.platform
                && !e.modifiers.function
                && unsafe { GetCapture() } == hwnd(window);
            press.observe(self.pointer_sample, 3. * self.canvas_scale(window));
            let selected = if ready && press.dragged {
                self.text_press_carets(&press, self.pointer_sample, window)
                    .is_ok_and(|(anchor, caret)| {
                        press.select(&mut self.editor, &mut self.input, anchor, caret)
                    })
            } else {
                ready
            };
            if selected {
                self.text_core_press = Some(press);
            } else {
                self.clear_pointer_tracking();
                if unsafe { GetCapture() } == hwnd(window) {
                    self.release_capture();
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.result_selection_dragging(self.result_generation) {
            // 正文内的 Move 由子元素处理；离开正文后不把同次左拖交给截图画布。
            if e.pressed_button != Some(gpui::MouseButton::Left)
                || !self.result_selection_pointer_ready()
            {
                self.result_selection_end(self.result_generation);
            }
            cx.stop_propagation();
            return;
        }
        if self.toolbar_drag.is_none() && self.result_selection_pointer_ready() {
            // 正文拖选先由子元素/上面的归属门处理；闲置结果不更新宿主悬停。
            return;
        }
        if self.pin_handoff_pending {
            return;
        }
        if self.pin_mode
            && self
                .pin_owner
                .as_ref()
                .and_then(|v| v.upgrade())
                .is_some_and(|v| v.read(cx).gesture.is_some())
        {
            return;
        }
        if self.pin_mode {
            if !self.pin_editing || e.modifiers.control && self.editor.text_draft.is_none() {
                return;
            }
            cx.stop_propagation();
        }
        if self.editor.local_fill_candidate().is_some() && self.pointer_down_start.is_none() {
            // 准备失败后保留意图供重试/取消；悬停不再延长已抬起的笔迹。
            return;
        }
        if (self.editor.drawing_freehand_pen() || self.editor.local_fill_candidate().is_some())
            && e.pressed_button != Some(gpui::MouseButton::Left)
        {
            // 丢失抬键消息后保留已收到的笔画，不把随后无按键的悬停连成跨屏直线。
            self.finish_content_or_status();
            self.invalidate_click_sequence();
            self.pencil_samples.clear();
            self.pointer_down_start = None;
            self.release_capture();
            cx.notify();
            return;
        }
        let previous_hover = self.canvas_hover(6. * self.canvas_scale(window), e.modifiers);
        self.pointer_sample = self.world_point(e.position, window);
        self.pointer = self.physical(e.position, window);
        self.invalidate_control_hover();
        self.ordinary_clicks.moved(
            self.pointer,
            e.pressed_button == Some(gpui::MouseButton::Left) && !e.modifiers.modified(),
        );
        if self.smart_pointer_move(
            e.pressed_button == Some(gpui::MouseButton::Left),
            window,
            cx,
        ) {
            return;
        }
        if self.manual_preview_click.is_some() {
            if e.pressed_button != Some(gpui::MouseButton::Left) {
                self.clear_pointer_tracking();
                self.release_capture();
                cx.notify();
                return;
            }
            let Some(start) = self.pointer_down_start else {
                self.manual_preview_click = None;
                self.manual_preview_cursor_claimed = false;
                self.new_crop_cursor_origin = None;
                self.crop_adjust_cursor_origin = None;
                self.crop_adjust_cursor_owner = None;
                self.manual_preview_cursor_keys.cancel_gesture();
                return;
            };
            if !self.manual_preview_dragged
                && !crate::capture_monitor_snap::preview_dragged(
                    start,
                    self.pointer,
                    window.scale_factor(),
                )
            {
                cx.notify();
                return;
            }
            self.set_manual_preview(None);
            self.selection_click_target = None;
            self.selection_pointer_dragged = true;
            self.editor
                .pointer_down(start, 6. * self.canvas_scale(window));
        }
        if self.pointer_down_start.is_some_and(|start| {
            (start.0 - self.pointer.0).abs() >= 3 || (start.1 - self.pointer.1).abs() >= 3
        }) {
            self.selection_pointer_dragged = true;
        }
        if let (Some(anchor), Some(draft)) = (self.input.drag_anchor, &self.editor.text_draft) {
            if let Ok(geometry) = crate::text_geometry::TextGeometry::new(
                &draft.annotation,
                self.canvas_scale(window),
            ) {
                let b = draft.annotation.local_bounds();
                let local = draft.annotation.to_local(crate::annotation::Point {
                    x: self.pointer_sample.x,
                    y: self.pointer_sample.y,
                });
                let caret = geometry.hit(gpui::point(
                    gpui::px((local.x - b.x) / self.canvas_scale(window)),
                    gpui::px((local.y - b.y) / self.canvas_scale(window)),
                ));
                self.input.select_to(anchor, caret);
                cx.notify();
                return;
            }
        }
        // 光标全部由系统绘制；仅悬停类别变化时重渲染以更新光标样式。
        let hover = self.canvas_hover(6. * self.canvas_scale(window), e.modifiers);
        if hover != previous_hover {
            cx.notify();
        }
        self.show_guides =
            self.guides || ((self.pin_mode || self.image_canvas.is_some()) && e.modifiers.alt);
        if self.editor.crop.is_none() {
            self.hover_window = self
                .window_targets
                .iter()
                .position(|target| target.rect.contains(self.pointer));
            self.request_control_probe(cx);
            if self.magnifier {
                self.refresh_magnifier_sample();
            }
            cx.notify();
        } else if self.show_guides {
            cx.notify();
        }
        if self.toolbar_drag.is_some() {
            self.move_toolbar_drag(e.position, window, false);
            cx.notify();
            return;
        }
        if self.editor.is_dragging() || self.editor.has_pending_drawing() {
            let pointer = self.drawing_pointer(e.modifiers.shift);
            if self.editor.drawing_freehand_pen() {
                self.collect_pencil(e.position, window);
            } else if self.image_canvas.is_some() {
                self.editor.pointer_move_at(pointer);
            } else {
                self.editor.pointer_move(pointer.rounded());
            }
            cx.notify();
        }
    }
    fn collect_pencil(&mut self, position: gpui::Point<gpui::Pixels>, window: &Window) {
        let points = self.pencil_samples.collect(
            hwnd(window),
            crate::image_editor::client_point(position, window),
        );
        for p in points {
            let point = self
                .image_canvas
                .as_ref()
                .map_or(p, |s| s.viewport.image_point(p));
            self.editor.pointer_move_at(self.clip_source_point(point));
        }
    }
    pub fn up(&mut self, e: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.long_clip_up(e, window, cx) {
            return;
        }
        if self.refresh_request.is_some() {
            return;
        }
        if self.ocr_overlay_up(e, window, cx) {
            return;
        }
        if e.button == gpui::MouseButton::Left {
            if let Some(mut press) = self.text_core_press.take() {
                let point = self.world_point(e.position, window);
                press.observe(point, 3. * self.canvas_scale(window));
                // Windows 在分发真实 Up 前释放捕获；这里核原来源，不要求捕获尚在。
                if self.text_core_ready(window, cx)
                    && press.valid(&self.editor)
                    && press.owns_pointer(self.pointer_down_start)
                    && !e.modifiers.control
                    && !e.modifiers.alt
                    && !e.modifiers.platform
                    && !e.modifiers.function
                {
                    if let Ok((anchor, caret)) = self.text_press_carets(&press, point, window) {
                        press.select(&mut self.editor, &mut self.input, anchor, caret);
                    }
                }
                self.clear_pointer_tracking();
                if unsafe { GetCapture() } == hwnd(window) {
                    self.release_capture();
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        // 最终 Up 保留本次来源用于几何；移动资格立即结束，不让 held 键迟到移鼠。
        let manual_crop_source =
            e.button == gpui::MouseButton::Left && self.manual_preview_cursor_claimed;
        if e.button == gpui::MouseButton::Left {
            self.manual_preview_cursor_claimed = false;
            self.new_crop_cursor_origin = None;
            self.crop_adjust_cursor_origin = None;
            self.crop_adjust_cursor_owner = None;
            self.manual_preview_cursor_keys.cancel_gesture();
            self.clear_idle_preview_cursor();
        }
        if e.button == gpui::MouseButton::Left && self.result_selection_end(self.result_generation)
        {
            self.invalidate_click_sequence();
            cx.stop_propagation();
            return;
        }
        // 被控件消费、取消、输入或忙碌路线提前返回时，pending 随本次 Up 丢弃。
        let canvas_click_pending = self.ordinary_clicks.take_pending();
        if self.pin_handoff_pending {
            return;
        }
        if self.pin_mode
            && self
                .pin_owner
                .as_ref()
                .and_then(|v| v.upgrade())
                .is_some_and(|v| v.read(cx).gesture.is_some())
        {
            return;
        }
        if self.pin_mode {
            if !self.pin_editing || e.modifiers.control && self.editor.text_draft.is_none() {
                return;
            }
            cx.stop_propagation();
        }
        if self.input.drag_anchor.take().is_some() {
            self.pointer_down_start = None;
            self.release_capture();
            cx.notify();
            return;
        }
        if self.toolbar_drag.is_some() {
            self.move_toolbar_drag(e.position, window, true);
            self.release_capture();
            cx.notify();
            return;
        }
        if e.button == gpui::MouseButton::Left
            && self.smart_mode()
            && !self.editor.is_changing_crop()
            && self.pointer_down_start.is_some()
        {
            self.pointer_sample = self.world_point(e.position, window);
            self.pointer = self.physical(e.position, window);
            if self.smart_pointer_up(e, canvas_click_pending, window, cx) {
                return;
            }
        }
        if self.editor.local_fill_candidate().is_some() && self.pointer_down_start.is_none() {
            self.release_capture();
            return;
        }
        if let Some(rect) = self.manual_preview_click.take() {
            let point = self.physical(e.position, window);
            let Some(start) = self
                .pointer_down_start
                .filter(|_| self.manual_preview == Some(rect))
            else {
                self.clear_pointer_tracking();
                self.release_capture();
                cx.notify();
                return;
            };
            let dragged = self.manual_preview_dragged
                || crate::capture_monitor_snap::preview_dragged(
                    start,
                    point,
                    window.scale_factor(),
                );
            self.set_manual_preview(None);
            self.selection_click_target = None;
            if dragged {
                // Up 可先于最后一个 Move；仍按实际抬起位置进入新框选。
                self.selection_pointer_dragged = true;
                self.editor
                    .pointer_down(start, 6. * self.canvas_scale(window));
            } else {
                self.editor.set_crop(Some(rect));
                self.toolbar_monitor = self.crop_anchor_monitor(Some(rect), point);
                self.toolbar_visibility
                    .finished_new_crop(&self.editor, None);
            }
            cx.notify();
        }
        let canvas_click_owned = self.pointer_down_start.is_some();
        let canvas_click_dragged = self.selection_pointer_dragged;
        if self.editor.is_dragging() {
            let changing_crop = self.editor.is_changing_crop();
            let crop_creation_before = self.editor.crop_creation_before();
            if self.editor.drawing_freehand_pen() {
                self.collect_pencil(e.position, window);
            }
            self.pencil_samples.clear();
            self.pointer_sample = self.world_point(e.position, window);
            self.pointer = self.physical(e.position, window);
            let p = self.drawing_pointer_with_preview_source(e.modifiers.shift, manual_crop_source);
            self.editor.pointer_move_at(p);
            if let Err(error) = self.try_finish_local_fill() {
                self.status = error.to_string();
                self.pointer_down_start = None;
                self.release_capture();
                cx.notify();
                return;
            }
            if self.image_canvas.is_some() {
                self.editor.pointer_up_at(p);
            } else {
                self.editor.pointer_up(p.rounded());
            }
            if !self.selection_pointer_dragged
                && self.pointer_down_start.is_some_and(|start| {
                    (start.0 - self.pointer.0).abs() < 3 && (start.1 - self.pointer.1).abs() < 3
                })
            {
                if let Some(rect) = self
                    .selection_click_target
                    .take()
                    .filter(|rect| rect.contains(self.pointer))
                {
                    if self.smart_mode() {
                        self.editor.set_smart_crop(Some(rect));
                    } else {
                        self.editor.set_crop(Some(rect));
                    }
                }
            }
            self.pointer_down_start = None;
            self.selection_click_target = None;
            self.selection_pointer_dragged = false;
            if changing_crop {
                self.toolbar_monitor = self.crop_anchor_monitor(self.editor.crop, self.pointer);
            }
            if !self.pin_mode && self.image_canvas.is_none() {
                self.toolbar_visibility
                    .finished_new_crop(&self.editor, crop_creation_before);
            }
            self.release_capture();
            if changing_crop {
                self.smart_candidate_sync(window, cx);
            }
            cx.notify();
        } else if self.pointer_down_start.is_some() {
            // Ctrl 选择、折线落点等不产生连续拖动；Up 后不能残留按下状态。
            // 只清输入跟踪，保留 Editor 中待续的折线和文字草稿。
            self.clear_pointer_tracking();
            if unsafe { GetCapture() } == hwnd(window) {
                self.release_capture();
            }
            cx.notify();
        }
        if let Some(click) = canvas_click_pending {
            self.startup_click_seed = self.ordinary_clicks.finish_ordinary(
                click,
                &self.editor,
                self.source.full(),
                self.physical(e.position, window),
                crate::capture_double_click::CanvasClickUp {
                    screenshot: self.image_canvas.is_none() && !self.pin_mode,
                    modified: e.modifiers.modified(),
                    busy: self.processing
                        || self.saving
                        || self.pin_handoff_pending
                        || self.toolbar_drag.is_some(),
                    input_active: self.watermark_input_focused
                        || self.input.marked.is_some()
                        || self.recognition_result.is_some(),
                    owned: canvas_click_owned
                        && !canvas_click_dragged
                        && self.control_selection_ready,
                },
            );
        }
    }
    pub(crate) fn release_capture(&self) {
        unsafe {
            let _ = ReleaseCapture();
        }
    }
    pub fn perform(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.perform_with_text_roi(action, None, window, cx);
    }
    pub(crate) fn startup_text_output(
        &mut self,
        kind: crate::startup_output::StartupOutputKind,
        rect: Rect,
        original_crop: Option<Rect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing
            || !self.text_copy_completion_is_idle()
            || self.recognition_result.is_some()
            || self.screenshot_toolbar_menu.presentation().is_some()
            || self.editor.crop != original_crop
            || original_crop.is_some_and(|crop| crop != rect)
            || !crate::startup_output::text_editor_ready(&self.editor, self.source.full(), false)
            || !rect.valid(self.source.bounds.width, self.source.bounds.height)
        {
            return;
        }
        let action = match kind {
            crate::startup_output::StartupOutputKind::ControlText => Action::ControlText,
            crate::startup_output::StartupOutputKind::Ocr if original_crop == Some(rect) => {
                Action::Ocr
            }
            _ => return,
        };
        // OCR 的原 finish/apply_lasso/Scene 顺序保留；ControlText 只使用保存的原始像素 ROI。
        self.perform_with_text_roi(action, Some(rect), window, cx);
    }
    fn group_navigation_blocked(&self) -> bool {
        self.processing
            || self.input.marked.is_some()
            || self.text_composing()
            || self.watermark_input_focused
            || self.result_editor.is_some()
            || self.ocr_overlay.is_some()
            || self.editor.text_draft.is_some()
            || self.editor.lasso_draft.is_some()
            || self.editor.watermark.has_preview()
            || self.editor.is_dragging()
            || self.editor.has_pending_drawing()
            || self.editor.local_fill_candidate().is_some()
            || self.toolbar_panel != ToolbarPanel::None && !self.toolbar_panel.more()
    }
    /// 调用者已处理输入、参数、OCR与手势；成功导航消费这一Esc，不交宿主关闭。
    pub(crate) fn exit_group_scope_input(&mut self, cx: &mut Context<Self>) -> bool {
        if self.group_navigation_blocked() || !self.editor.exit_group_scope() {
            return false;
        }
        self.wheel_input.reset();
        self.clear_pointer_tracking();
        self.release_capture();
        self.reset_input();
        self.status.clear();
        cx.notify();
        true
    }
    fn perform_with_text_roi(
        &mut self,
        action: Action,
        text_roi: Option<Rect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.long_clip_ui.is_some() && self.pin_handoff_pending {
            return;
        }
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.drag.is_some() || ui.draft.is_some() || ui.parameters_pending)
        {
            return;
        }
        if matches!(action, Action::Help) {
            if self.saving
                || self.refresh_request.is_some()
                || self.editor.is_dragging()
                || self.pointer_down_start.is_some()
                || self.text_composing()
                || self.result_editor_composing()
            {
                return;
            }
            self.selection_size_input = None;
            self.toolbar_panel = if self.toolbar_panel == ToolbarPanel::Help {
                ToolbarPanel::None
            } else {
                ToolbarPanel::Help
            };
            self.flyout.close();
            cx.notify();
            return;
        }
        if self.handle_refresh_action(action, window, cx) {
            return;
        }
        if matches!(action, Action::FollowToolbar) {
            if self.toolbar_drag.is_some() {
                self.release_capture();
            }
            self.follow_toolbar();
            self.toolbar_panel = ToolbarPanel::None;
            self.selection_size_input = None;
            window.focus(&self.focus);
            cx.notify();
            return;
        }
        if let Action::Recognize(mode) = action {
            self.recognize_mode = mode;
            self.flyout.close();
            if self.apply_recognize_mode_to_result(mode, window, cx) {
                return;
            }
            self.ocr_intent = matches!(
                mode,
                crate::toolbar_groups::RecognizeMode::TextTranslate
                    | crate::toolbar_groups::RecognizeMode::Table
            )
            .then_some(mode);
            let next = match mode {
                crate::toolbar_groups::RecognizeMode::ImageTranslate => Action::TranslateImage,
                crate::toolbar_groups::RecognizeMode::Qr => Action::Qr,
                crate::toolbar_groups::RecognizeMode::Table => Action::TableOcr,
                _ => Action::Ocr,
            };
            let intent = self.ocr_intent;
            let was_processing = self.processing;
            self.perform_with_text_roi(next, text_roi, window, cx);
            // 未真正开始识别（忙、无选区等）时不把意图留给之后的普通识别。
            self.ocr_intent = (!was_processing && self.processing)
                .then_some(intent)
                .flatten();
            return;
        }
        if matches!(action, Action::Ocr) {
            self.ocr_intent = None;
        }
        self.cancel_ocr_panel_click();
        self.cancel_ocr_result_click();
        // 主栏 on_click 同步进入此 Action 分发；这是 Up 后的入口，不是 Down。
        self.cancel_ocr_typed_pointer_clicks();
        let click_trace = matches!(action, Action::Ocr | Action::TableOcr)
            .then(|| ocr_progress::ClickTrace::start("action_dispatch"))
            .flatten();
        if self.text_core_press.is_some() {
            self.clear_pointer_tracking();
        }
        self.preview_cursor_nudge.reset();
        let had_new_cursor = self.new_crop_cursor_origin.take().is_some();
        let had_adjust_cursor = self.crop_adjust_cursor_origin.is_some();
        self.crop_adjust_cursor_owner = None;
        if had_new_cursor || had_adjust_cursor {
            self.manual_preview_cursor_keys.cancel_gesture();
        }
        self.clear_idle_preview_cursor();
        if self.screenshot_toolbar_menu.dismiss() {
            cx.notify();
        }
        if matches!(action, Action::Guides) {
            self.reset_guide_toggle();
        }
        self.invalidate_click_sequence();
        if self.pin_handoff_pending && !matches!(action, Action::Cancel) {
            return;
        }
        if crate::watermark_toolbar_smoke::guard_capture_output(&action, window)
            || crate::watermark_long_memory_smoke::guard_capture_output(&action, window)
        {
            return;
        }
        if self.saving {
            return;
        }
        if self.result_editor_composing()
            && matches!(
                action,
                Action::OpenImage
                    | Action::FitImage
                    | Action::ActualImage
                    | Action::ZoomImage(_)
                    | Action::Save
                    | Action::QuickSave
                    | Action::Confirm
                    | Action::Copy
                    | Action::Pin
                    | Action::Ocr
                    | Action::TableOcr
            )
        {
            self.status = tr!("请先完成输入法组合文字", "Finish the IME composition first").into();
            cx.notify();
            return;
        }
        if self.text_composing()
            && !matches!(
                action,
                Action::Undo
                    | Action::Redo
                    | Action::Cancel
                    | Action::Clear
                    | Action::PickColor
                    | Action::Magnifier
                    | Action::Guides
                    | Action::Cursor
                    | Action::Settings
                    | Action::History
                    | Action::Theme
                    | Action::More
                    | Action::Panel(_)
                    | Action::FitImage
                    | Action::ActualImage
                    | Action::ZoomImage(_)
            )
        {
            self.status = tr!("请先完成输入法组合文字", "Finish the IME composition first").into();
            cx.notify();
            return;
        }
        let group_navigation = matches!(
            action,
            Action::EnterGroup | Action::ExitGroup | Action::RootGroup
        );
        if group_navigation && self.group_navigation_blocked() {
            self.status = tr!(
                "请先完成当前编辑或文字选择",
                "Finish the current edit or text selection first"
            )
            .into();
            cx.notify();
            return;
        }
        let prepared = if group_navigation
            || matches!(
                action,
                Action::Undo
                    | Action::Redo
                    | Action::Cancel
                    | Action::Clear
                    | Action::Panel(ToolbarPanel::SelectionSize | ToolbarPanel::Finish)
            ) {
            Ok(())
        } else {
            self.try_finish_local_fill()
        };
        if let Err(error) = prepared {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        if self.ocr_overlay.is_some()
            && matches!(
                action,
                Action::Tool(_)
                    | Action::Reselect
                    | Action::Current
                    | Action::All
                    | Action::OpenImage
                    | Action::Qr
                    | Action::ControlText
                    | Action::SmartMosaic
            )
        {
            self.dismiss_recognition_result();
        }
        // 属性小窗里的“编辑文字”必须回宿主建立输入焦点，不能绑定到参数窗。
        if self.pin_mode
            && matches!(action, Action::EditText)
            && window
                .window_handle()
                .downcast::<crate::pin_view::PinView>()
                .is_none()
        {
            if let Some(owner) = self.pin_window {
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |v, w, cx| v.annotation(action, w, cx));
                });
            }
            return;
        }
        self.wheel_input.reset();
        if matches!(
            action,
            Action::FitImage | Action::ActualImage | Action::ZoomImage(_)
        ) && (self.editor.is_dragging()
            || self.editor.has_pending_drawing()
            || self.input.drag_anchor.is_some())
        {
            return;
        }
        if matches!(action, Action::OpenImage) && !self.finish_content_or_status() {
            return;
        }
        self.input.drag_anchor = None;
        if let Some(state) = &mut self.image_canvas {
            state.pan = None;
        }
        self.release_capture();
        self.status.clear();
        if matches!(
            action,
            Action::Tool(_)
                | Action::Undo
                | Action::Redo
                | Action::Clear
                | Action::Delete
                | Action::Cancel
                | Action::Current
                | Action::All
                | Action::Reselect
                | Action::Copy
                | Action::Confirm
                | Action::Save
                | Action::Pin
                | Action::OpenImage
                | Action::EnterGroup
                | Action::ExitGroup
                | Action::RootGroup
        ) {
            self.clear_pointer_tracking();
            if matches!(
                action,
                Action::Tool(_) | Action::Cancel | Action::Copy | Action::Pin | Action::OpenImage
            ) {
                self.smart_candidates.core.leave();
            }
            if matches!(action, Action::Cancel) {
                self.smart_candidates.core.terminate();
            }
        }
        if matches!(
            action,
            Action::Tool(_)
                | Action::Current
                | Action::All
                | Action::Reselect
                | Action::Cancel
                | Action::OpenImage
        ) {
            self.set_manual_preview(None);
            self.manual_preview_cursor_claimed = false;
            self.new_crop_cursor_origin = None;
            self.crop_adjust_cursor_origin = None;
            self.crop_adjust_cursor_owner = None;
            self.manual_preview_cursor_keys.cancel_gesture();
        }
        self.flyout.close();
        if !matches!(action, Action::More | Action::Panel(_)) {
            self.toolbar_panel = ToolbarPanel::None;
            self.selection_size_input = None;
        }
        if self.image_canvas.is_some()
            && matches!(
                action,
                Action::Record | Action::LongCapture | Action::Cursor | Action::Magnifier
            )
        {
            self.status = tr!("此操作用于桌面截图", "This action is for desktop captures").into();
            cx.notify();
            return;
        }
        match action {
            Action::OpenImage => self.open_image_prompt(window, cx),
            Action::Refresh => unreachable!("refresh dispatched before editor mutations"),
            Action::FollowToolbar => {
                unreachable!("toolbar placement does not commit editor drafts")
            }
            Action::Help => unreachable!("help dispatched without editor mutations"),
            Action::FitImage => self.fit_image(window),
            Action::ActualImage => self.zoom_image(1., self.zoom_center(window), cx),
            Action::ZoomImage(ratio) => {
                if let Some(state) = &self.image_canvas {
                    self.zoom_image(state.viewport.zoom * ratio, self.zoom_center(window), cx);
                }
            }
            Action::PickColor => {
                self.open_color_picker(
                    crate::toolbar_layout::ColorTarget::Annotation,
                    self.editor.style.color,
                    window,
                    cx,
                );
            }
            Action::Magnifier => {
                self.magnifier = !self.magnifier;
                self.stop_capture_color();
                let (shift, p) = Self::capture_color_keys();
                self.capture_color_display.reset(shift, p);
            }
            Action::Guides => {
                self.guides = !self.guides;
                self.show_guides = self.guides
                    || ((self.pin_mode || self.image_canvas.is_some()) && window.modifiers().alt);
            }
            Action::Cursor => {
                self.include_cursor = !self.include_cursor;
                self.scene = Arc::new(Scene::default());
                self.scene_stamp = None;
                self.status = if self.include_cursor && self.cursor.is_none() {
                    tr!(
                        "开始截图时没有可绘制的鼠标指针",
                        "There was no drawable mouse pointer when the capture started"
                    )
                    .into()
                } else {
                    String::new()
                };
            }
            Action::Tool(tool) => {
                self.tool_groups.remember(tool);
                if tool == Tool::SmartMosaic {
                    self.enter_smart_mosaic(window, cx);
                    cx.notify();
                    return;
                }
                if self.pin_mode {
                    if let Some(preview) = &self.pin_annotation_preview {
                        preview.reveal_for_editing();
                    }
                    self.pin_editing = true;
                }
                self.drop_watermark_field(cx);
                self.editor.select_tool(tool);
                if tool == Tool::Watermark && !self.pin_mode {
                    self.ensure_watermark_field(window, cx);
                }
            }
            Action::ApplyLasso => {
                self.editor.apply_lasso();
            }
            Action::ClearLasso => self.editor.clear_lasso(),
            Action::Undo => {
                self.editor.undo();
                self.reset_input();
                self.refresh_ocr_overlay_after_history();
            }
            Action::Redo => {
                self.editor.redo();
                self.reset_input();
                self.refresh_ocr_overlay_after_history();
            }
            Action::Delete => self.editor.delete_selected(),
            Action::DeleteNode => {
                self.editor.remove_polyline_point();
            }
            Action::StraightenArrow => {
                self.editor.straighten_arrow();
            }
            Action::Clear => {
                self.editor.clear();
                self.reset_input();
            }
            Action::Current => {
                let rect = Some(self.current_capture_rect());
                if self.smart_mode() {
                    self.editor.set_smart_crop(rect);
                } else {
                    self.editor.set_crop(rect);
                }
            }
            Action::All => {
                if self.smart_mode() {
                    self.editor.set_smart_crop(Some(self.source.full()));
                } else {
                    self.editor.set_crop(Some(self.source.full()));
                }
            }
            Action::Reselect => {
                if self.smart_mode() {
                    self.editor.set_smart_crop(None);
                } else {
                    self.editor.set_crop(None);
                }
            }
            Action::Confirm => self.confirm_quick_action(window, cx),
            Action::Copy => self.copy(window, cx),
            Action::Save => self.save(window, cx),
            Action::Cancel => {
                if !self.confirm_image_discard() {
                    return;
                }
                self.long_clip_complete_session(cx);
                self.cancel_capture_ocr_tasks("截图窗口确认退出");
                self.pin_handoff_pending = false;
                self.flush_tool_presets();
                self.capture_lock.take();
                window.remove_window();
            }
            Action::Pin => {
                crate::pin_handoff_smoke::mark("action_pin_entered");
                let started = (std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1"))
                    .then(std::time::Instant::now);
                if self.processing {
                    return;
                }
                if !self.finish_content_or_status() {
                    return;
                }
                self.editor.apply_lasso();
                self.flush_tool_presets();
                match self.output_rect() {
                    Ok(rect) => self.pin_range_started(rect, started, window, cx),
                    Err(error) => self.status = error.to_string(),
                }
            }
            Action::WindowPin => self.window_pin_started(window, cx),
            Action::LongCapture => {
                if self.image_canvas.is_some() || self.pin_mode {
                    self.status = tr!(
                        "长截图需要先框选屏幕上的滚动内容",
                        "For a long capture, first select the scrolling content on screen"
                    )
                    .into();
                    cx.notify();
                    return;
                }
                if self.editor.document.crop_mask.is_some() || self.editor.lasso_draft.is_some() {
                    self.status = tr!("套索仅用于静态截图，请先清除套索再开始长截图", "The lasso is for still captures only; clear it before starting a long capture").into();
                    cx.notify();
                    return;
                }
                let result: Result<()> = (|| {
                    let r = self.selected()?;
                    let global = crate::geometry::ScreenRect {
                        left: self.source.bounds.left + r.x as i32,
                        top: self.source.bounds.top + r.y as i32,
                        width: r.width,
                        height: r.height,
                    };
                    match crate::long_capture_view::LongCaptureView::open(global, cx) {
                        Ok(panel) => {
                            let capture_lock = self.capture_lock.take();
                            panel.update(cx, move |v, _, _| v.capture_lock = capture_lock)?;
                            self.cancel_capture_ocr_tasks("截图成功转入长截图");
                            window.remove_window();
                        }
                        Err(e) => return Err(e),
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    self.status = e.to_string();
                }
            }
            Action::Record => self.record(window, cx),
            Action::Settings => {
                if let Err(e) = crate::settings_view::SettingsView::open(cx) {
                    self.status = e.to_string();
                }
            }
            Action::History => {
                if let Err(e) = crate::settings_view::main_page::launch(
                    crate::settings_view::main_page::MainPage::History,
                ) {
                    self.status = e.to_string();
                }
            }
            Action::Qr => {
                if !self.finish_content_or_status() {
                    return;
                }
                self.editor.apply_lasso();
                if !self.processing {
                    match self.selected_content_image() {
                        Ok(image) => {
                            self.invalidate_ocr_display_request();
                            self.processing = true;
                            self.clear_idle_preview_cursor();
                            let task = cx.background_executor().spawn(async move {
                                crate::recognition::qr(image).map_err(|e| e.to_string())
                            });
                            cx.spawn_in(window, async move |v, cx| {
                                let result = task.await;
                                let _ = v.update(cx, |v, cx| {
                                    v.processing = false;
                                    match result {
                                        Ok(text) => v.show_recognition_result(
                                            text,
                                            text_result::Origin::Qr,
                                            tr!("二维码识别", "QR code"),
                                        ),
                                        Err(e) => v.status = e,
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        }
                        Err(e) => self.status = e.to_string(),
                    }
                }
            }
            Action::Ocr | Action::TableOcr => {
                self.recognize_with_copy_policy(false, text_roi.is_some(), click_trace, window, cx)
            }
            Action::TranslateImage => self.translate_image(window, cx),
            // 已在分发入口转为具体动作。
            Action::Recognize(_) => {}
            Action::ControlText => {
                if text_roi.is_some() {
                    self.capture_control_text_in(text_roi, window, cx);
                } else {
                    self.capture_control_text(window, cx);
                }
            }
            Action::SmartMosaic => {
                if self.editor.effect_context().0 == Tool::LocalFill {
                    self.recognize(true, window, cx);
                } else {
                    self.enter_smart_mosaic(window, cx);
                }
            }
            Action::SmartEffect(effect) => self.change_smart_effect(effect, cx),
            Action::QuickSave => {
                self.quick_save(window, cx);
            }
            Action::Duplicate => self.editor.duplicate_selected(),
            Action::Group => {
                self.editor.group_selection(true);
            }
            Action::Ungroup => {
                self.editor.group_selection(false);
            }
            Action::EnterGroup | Action::ExitGroup | Action::RootGroup => {
                let changed = match action {
                    Action::EnterGroup => self.editor.enter_group_scope(),
                    Action::ExitGroup => self.editor.exit_group_scope(),
                    Action::RootGroup => self.editor.exit_all_group_scopes(),
                    _ => unreachable!(),
                };
                if changed {
                    self.drop_watermark_field(cx);
                    self.reset_input();
                }
            }
            Action::Rotate(degrees) => self.editor.rotate_selection(degrees),
            Action::ResetRotation => self.editor.reset_rotation(),
            Action::Front => self.editor.reorder_selected(true),
            Action::Back => self.editor.reorder_selected(false),
            Action::EditText => {
                self.editor.edit_text();
                self.reset_input();
            }
            Action::Theme => {
                let mut preferences = crate::preferences::Preferences::load();
                preferences.theme = preferences.theme.next();
                if let Err(e) = preferences.save() {
                    self.status = e.to_string();
                } else {
                    crate::theme::set_mode(preferences.theme, cx);
                    self.light_theme = crate::theme::is_light(cx);
                }
            }
            Action::More => {
                self.selection_size_input = None;
                self.toolbar_panel = if self.toolbar_panel.more() {
                    ToolbarPanel::None
                } else {
                    ToolbarPanel::Tools
                }
            }
            Action::Panel(panel) => {
                if panel == ToolbarPanel::SelectionSize && self.toolbar_panel != panel {
                    if !self.open_selection_size_input(window, cx) {
                        return;
                    }
                } else {
                    self.selection_size_input = None;
                }
                self.toolbar_panel = if self.toolbar_panel == panel && !panel.more() {
                    ToolbarPanel::None
                } else {
                    panel
                };
                if self.toolbar_panel != ToolbarPanel::Finish {
                    self.finish_section = Default::default();
                }
            }
        }
        self.screenshot_toolbar_visible();
        cx.notify();
    }
    fn record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.document.crop_mask.is_some() || self.editor.lasso_draft.is_some() {
            self.status = tr!(
                "套索仅用于静态截图，请先清除套索再录屏",
                "The lasso is for still captures only; clear it before recording"
            )
            .into();
            return;
        }
        let rect = match self.selected() {
            Ok(r) => r,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        let global = crate::geometry::ScreenRect {
            left: self.source.bounds.left + rect.x as i32,
            top: self.source.bounds.top + rect.y as i32,
            width: rect.width,
            height: rect.height,
        };
        self.clear_idle_preview_cursor();
        // 先打开录屏宿主（置顶、排除采集）再隐藏冻结截图层，切换期间不露出空白桌面；
        // 录制要等用户点开始才采集，此时截图层早已移除。成品写入录屏历史。
        match crate::recording_view::RecordingView::open(global, cx) {
            Ok(()) => {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                        hwnd(window),
                        windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
                    );
                }
                self.cancel_capture_ocr_tasks("截图成功转入录屏");
                self.capture_lock.take();
                window.remove_window();
            }
            Err(e) => self.status = e.to_string(),
        }
    }
    fn recognize(&mut self, mosaic: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.recognize_with_copy_policy(mosaic, false, None, window, cx);
    }
    pub(crate) fn recognize_with_copy_policy(
        &mut self,
        mosaic: bool,
        startup_copy_intent: bool,
        click_trace: Option<ocr_progress::ClickTrace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recognize_with_profile_and_copy_policy(
            mosaic,
            startup_copy_intent,
            click_trace,
            None,
            window,
            cx,
        );
    }
    pub(crate) fn recognize_with_profile_and_copy_policy(
        &mut self,
        mosaic: bool,
        startup_copy_intent: bool,
        click_trace: Option<ocr_progress::ClickTrace>,
        profile: Option<crate::ppocr::OcrProfile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing
            || self.saving
            || self.pin_handoff_pending
            || (self.inline_ocr_host() && !self.inline_ocr_interaction_idle())
        {
            return;
        }
        if !mosaic && self.result_editor_composing() {
            self.status = tr!("请先完成输入法组合文字", "Finish the IME composition first").into();
            cx.notify();
            return;
        }
        if !self.finish_content_or_status() {
            cx.notify();
            return;
        }
        self.editor.apply_lasso();
        let selected = match self.output_rect() {
            Ok(r) => r,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        let image = match self.recognition_content_image(selected) {
            Ok(i) => i,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        if let Some(trace) = &click_trace {
            trace.event("bgra_ready");
        }
        // 前景文字识别优先；隐藏的智能候选不能继续占用同一推理引擎。
        self.smart_candidates
            .cancel_detection("完整识别请求优先于智能候选检测");
        // 识别期间可继续操作；返回结果只能作用于发起时的源、下层版本和参数。
        let request = self.smart_effect_request(selected);
        let canvas = self.source.bounds;
        let ocr_target = text_result::OcrTarget {
            canvas,
            selected,
            revisions: (request.revision, request.edit_revision),
            stamp: request.stamp,
            identity: request.identity.clone(),
        };
        let ordinary = self.inline_ocr_host();
        let table_mode = !mosaic
            && (self.ocr_intent == Some(crate::toolbar_groups::RecognizeMode::Table)
                || self.quick_action.current() == Some(crate::quick_action::Step::TableCopy));
        // UIA 补救沿用链首固定档位；普通请求在此读取一次供后台和局部重识别共用。
        let preferences = crate::preferences::Preferences::load();
        let profile = profile.unwrap_or(preferences.ocr_profile);
        let provider = crate::recognition::OcrProvider::default();
        let local_input = (!mosaic && ordinary)
            .then(|| {
                crate::capture_ocr_overlay::local_retry_host::FixedInput::copy_of_profile(
                    &image,
                    ocr_target.clone(),
                    provider,
                    profile,
                )
            })
            .flatten();
        self.cancel_local_ocr_intent("新的完整识别请求撤销局部候选");
        let display_request = if mosaic || !ordinary {
            // 贴图/智能效果另有 Owner；旧原位结果回执不能释放其 processing。
            self.invalidate_ocr_display_request();
            None
        } else {
            let content_revision = self.result_content_revision();
            Some(
                self.recognition_session
                    .begin_initial(ocr_target.clone(), content_revision),
            )
        };
        let foreground_request = display_request.is_none().then(|| {
            self.foreground_ocr
                .start(text_result::InputTarget::Content(ocr_target.clone()))
        });
        self.auto_copy_recovery = None;
        let task_token = if let Some(display) = &display_request {
            self.recognition_session.initial_token(display.id).unwrap()
        } else {
            foreground_request.as_ref().unwrap().token.clone()
        };
        let copy_only = !mosaic
            && !table_mode
            && startup_copy_intent
            && !self.pin_mode
            && self.image_canvas.is_none()
            && preferences.text_capture_copy_only;
        self.processing = true;
        self.clear_idle_preview_cursor();
        self.status = if table_mode {
            tr!(
                "正在本机识别表格，首次使用需下载模型…",
                "Recognizing table locally; first use downloads the model…"
            )
        } else {
            tr!("正在本机识别文字…", "Recognizing text locally…")
        }
        .into();
        if let Some(trace) = &click_trace {
            let key = display_request.as_ref().map_or_else(
                || ocr_progress::TaskKey {
                    kind: ocr_progress::TaskKind::Foreground,
                    id: foreground_request.as_ref().unwrap().id,
                },
                |request| ocr_progress::TaskKey {
                    kind: ocr_progress::TaskKind::Initial,
                    id: request.id,
                },
            );
            trace.bind(key, profile);
        }
        self.ocr_click_trace = click_trace.clone();
        let worker_trace = click_trace.clone();
        // 表格线检测只读像素，与 OCR 并行放在后台，结果安装时界面线程只做几何分配。
        let table_image = (!mosaic && ordinary && !table_mode).then(|| crate::capture::Image {
            bgra: image.bgra.clone(),
            ..image
        });
        let task = cx.background_executor().spawn(async move {
            if let Some(trace) = &worker_trace {
                trace.event("worker_started");
            }
            let mut structure = None;
            let (receipt, detected) = std::thread::scope(|scope| {
                let lines = table_image
                    .as_ref()
                    .map(|image| scope.spawn(|| crate::table_lines::detect(image)));
                let result = if table_mode {
                    crate::ppocr::table::recognize_cancellable(&image, profile, &task_token).map(
                        |result| {
                            let (recognized, table) = result.into_parts();
                            structure = Some(table);
                            recognized
                        },
                    )
                } else {
                    crate::recognition::recognize_with_profile_cancellable(
                        image,
                        provider,
                        profile,
                        &task_token,
                    )
                };
                let receipt = ocr_task::Receipt::from_result(result);
                (receipt, lines.and_then(|lines| lines.join().ok().flatten()))
            });
            if let Some(trace) = &worker_trace {
                trace.event("worker_done");
            }
            (receipt, detected, structure)
        });
        cx.spawn_in(window, async move |view, cx| {
            let (result, mut detected, mut structure) = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if let Some(trace) = &click_trace { trace.event("receipt_callback"); }
                let previous_installation = view.ocr_result_installation;
                // 回执在 Worker 保持三态，先认领匹配身份再释放 Owner。
                // 正常释放 Owner 会置取消位，之后绝不再读该位否决已完成结果。
                let target_current = if let Some(display) = &display_request {
                    let Some(owned) = view.recognition_session.finish_initial(display.id) else {
                        return;
                    };
                    view.current_ocr_target().is_some_and(|target| {
                        view.recognition_session.request_is_current(
                            &owned, &target, view.result_content_revision(),
                        )
                    }) && !view.result_editor_composing() && !view.text_composing()
                } else {
                    let foreground = foreground_request.as_ref().unwrap();
                    if !view.foreground_ocr.finish(foreground.id) {
                        return;
                    }
                    view.current_ocr_target().is_some_and(|target| {
                        foreground.key.is_current(&target, true)
                    })
                };
                // 只有本请求认领成功才可清 processing，旧回执直接返回。
                if let Some(trace) = &click_trace { trace.event("receipt_claimed"); }
                view.processing = false;
                let result = match result {
                    ocr_task::Receipt::Completed(recognized) => Ok(recognized),
                    ocr_task::Receipt::Failed(error) => Err(error),
                    ocr_task::Receipt::Cancelled => {
                        if view.quick_ocr_pending() {
                            view.quick_failed(tr!("识别已取消，选区已保留", "Recognition canceled; selection kept"), cx);
                            return;
                        }
                        if table_mode || view.status == tr!("正在本机识别文字…", "Recognizing text locally…") {
                            view.status.clear();
                        }
                        cx.notify();
                        return;
                    }
                };
                if !target_current {
                    if view.quick_ocr_pending() {
                        view.quick_failed(tr!("截图范围或内容已改变，请重试", "Capture changed; retry"), cx);
                        return;
                    }
                    view.status = tr!(
                        "识别期间截图范围、内容或结果会话已改变，请重新识别",
                        "The capture area, content or result session changed; recognize again"
                    ).into();
                    cx.notify();
                    return;
                }
                if mosaic {
                    match result {
                        Ok(result) => {
                            view.status = match view.apply_smart_effect(&request, result.regions) {
                                Ok(0) => tr!("未发现可处理的文字区域", "No text regions found to process").into(),
                                Ok(_) => {
                                    tr!("已处理识别到的文字区域，可一次撤销。请检查识别遗漏。", "Recognized text regions processed; undo reverts them at once. Check for missed text.").into()
                                }
                                Err(error) => error.to_string(),
                            };
                        }
                        Err(error) => view.status = error,
                    }
                } else {
                    if view.quick_ocr_pending() {
                        view.quick_ocr_received(result, structure.take(), window, cx);
                        return;
                    }
                    match text_result::route(
                        result.as_ref().map(|result| result.text.clone()).map_err(Clone::clone),
                        copy_only,
                        target_current
                            && (!copy_only || view.ocr_copy_target_is_current(&request, canvas)),
                    ) {
                        text_result::Completion::CopyAndClose(text) => {
                            match text_result::publish_auto_copy(text, |text| {
                                export::copy_text(text, hwnd(window))
                            }) {
                                text_result::AutoCopyPublication::Close => {
                                    view.perform(Action::Cancel, window, cx);
                                }
                                text_result::AutoCopyPublication::KeepResult { text, error } => {
                                    view.show_recognition_result(
                                        text,
                                        text_result::Origin::Ocr(ocr_target.clone()),
                                        tr!("本机文字识别", "Local text recognition"),
                                    );
                                    if let Ok(recognized) = result {
                                        let note = view.install_table_copy_detected(&recognized, ocr_target.clone(), detected.take(), structure.take());
                                        view.install_ocr_overlay(&recognized, ocr_target.clone());
                                        view.install_local_ocr_input(local_input, recognized, window, cx);
                                        view.apply_ocr_intent(window, cx);
                                        if let Some(note) = note { view.status = note; }
                                    }
                                    view.retain_auto_copy_failure(
                                        &error,
                                        text_result::InputTarget::Content(ocr_target),
                                    );
                                }
                            }
                        }
                        text_result::Completion::ShowResult(text) => {
                            view.status.clear();
                            view.show_recognition_result(
                                text,
                                text_result::Origin::Ocr(ocr_target.clone()),
                                tr!("本机文字识别", "Local text recognition"),
                            );
                            if let Ok(recognized) = result {
                                let note = view.install_table_copy_detected(&recognized, ocr_target.clone(), detected.take(), structure.take());
                                view.install_ocr_overlay(&recognized, ocr_target);
                                view.install_local_ocr_input(local_input, recognized, window, cx);
                                view.apply_ocr_intent(window, cx);
                                if let Some(note) = note { view.status = note; }
                            }
                        }
                        text_result::Completion::KeepOpenWithError(error) => view.status = error,
                    }
                }
                if let Some(trace) = &click_trace {
                    if view.ocr_result_installation != previous_installation {
                        trace.result_installed(view, window, cx);
                    } else {
                        trace.event("result_not_installed");
                    }
                }
                view.resume_image_translation(cx);
                cx.notify();
            });
        })
        .detach();
    }
    pub(crate) fn text_copy_completion_is_idle(&self) -> bool {
        self.inline_ocr_host()
            && self.inline_ocr_interaction_idle()
            && !self.saving
            && !self.pin_handoff_pending
            && self.toolbar_panel == ToolbarPanel::None
            && text_copy_editor_is_idle(&self.editor)
            && self.pointer_down_start.is_none()
            && self.toolbar_drag.is_none()
            && self.input.drag_anchor.is_none()
            && self.input.marked.is_none()
            && !self.watermark_input_focused
            && !self.text_composing()
    }
    fn ocr_copy_target_is_current(
        &self,
        request: &SmartEffectRequest,
        canvas: crate::geometry::ScreenRect,
    ) -> bool {
        self.text_copy_completion_is_idle()
            && self.source.bounds == canvas
            && self.editor.document.revision() == request.revision
            && self.editor.document.edit_revision() == request.edit_revision
            && self.editor.scene_stamp() == request.stamp
            && self.output_rect().ok() == Some(request.selected)
            && crate::scene::SourceIdentity::of(&self.source_pixels()) == request.identity
    }
    pub(crate) fn smart_effect_request(&self, selected: Rect) -> SmartEffectRequest {
        let (effect_tool, style) = self.editor.effect_context();
        SmartEffectRequest {
            effect: match effect_tool {
                Tool::Blur | Tool::BlurBrush => Tool::Blur,
                Tool::LocalFill => Tool::LocalFill,
                _ => Tool::Mosaic,
            },
            style,
            selected,
            revision: self.editor.document.revision(),
            edit_revision: self.editor.document.edit_revision(),
            stamp: self.editor.scene_stamp(),
            identity: crate::scene::SourceIdentity::of(&self.source_pixels()),
        }
    }
    pub(crate) fn apply_smart_effect(
        &mut self,
        request: &SmartEffectRequest,
        regions: Vec<Rect>,
    ) -> Result<usize> {
        if self.editor.document.revision() != request.revision
            || self.editor.document.edit_revision() != request.edit_revision
            || self.editor.scene_stamp() != request.stamp
            || self.output_rect().ok() != Some(request.selected)
            || crate::scene::SourceIdentity::of(&self.source_pixels()) != request.identity
        {
            return Err(tr!(
                "识别期间图片或标注已改变，请重新执行智能效果",
                "The image or annotations changed during recognition; apply the smart effect again"
            )
            .into());
        }
        self.insert_smart_effect(regions, request.selected, request.effect, request.style)
    }
    fn insert_smart_effect(
        &mut self,
        regions: Vec<Rect>,
        selected: Rect,
        effect: Tool,
        style: crate::annotation::Style,
    ) -> Result<usize> {
        use crate::annotation::{Annotation, Bounds, LocalFillGeometry, Point};
        let regions: Vec<_> = regions
            .into_iter()
            .filter_map(|r| r.intersection(Rect::full(selected.width, selected.height)))
            .map(|r| Bounds {
                x: (selected.x + r.x) as f32,
                y: (selected.y + r.y) as f32,
                width: r.width as f32,
                height: r.height as f32,
            })
            .collect();
        let Some(first) = regions.first() else {
            return Ok(0);
        };
        let count = regions.len();
        if effect == Tool::LocalFill {
            let mut annotation = Annotation::new(
                effect,
                Point {
                    x: first.x,
                    y: first.y,
                },
                style,
            );
            annotation.fill_geometry = LocalFillGeometry::Regions(regions.into());
            let scene = {
                let source = self.source_pixels();
                let below = self
                    .scene
                    .updated(&self.editor.document_annotations(), &source);
                if !below.errors.is_empty() {
                    return Err(below.errors.join(";").into());
                }
                let Some(prepared) =
                    crate::local_fill_scene::prepare(&annotation, &source, &below)?
                else {
                    return Ok(0);
                };
                below.with_prepared_fill(&annotation, prepared, &source)?
            };
            self.editor.reset_group_scope();
            self.editor.document.insert(annotation);
            self.scene = Arc::new(scene);
        } else {
            let annotations: Vec<_> = regions
                .into_iter()
                .map(|r| {
                    let mut a = Annotation::new(effect, Point { x: r.x, y: r.y }, style);
                    a.update(Point {
                        x: r.x + r.width,
                        y: r.y + r.height,
                    });
                    a
                })
                .collect();
            let mut all = self.editor.document_annotations();
            all.extend(annotations.iter().cloned());
            let scene = self.scene.updated(&all, &self.source_pixels());
            if !scene.errors.is_empty() {
                return Err(scene.errors.join(";").into());
            }
            self.editor.reset_group_scope();
            self.editor.document.insert_many(annotations);
            self.scene = Arc::new(scene);
        }
        self.scene_stamp = None;
        Ok(count)
    }
    fn copy(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if !self.finish_content_or_status() {
            return;
        }
        self.editor.apply_lasso();
        let image = self.selected_image();
        let origin = self
            .output_rect()
            .ok()
            .and_then(|rect| self.clipboard_image_origin(rect));
        self.copy_image_result(image, origin, window);
    }
    /// 显式预览输出不提交 crop 或草稿，复用复制、历史和关闭结果路线。
    pub(crate) fn copy_range(&mut self, rect: Rect, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.long_clip().is_some() {
            self.copy(window, cx);
            return;
        }
        let finish = self.effective_finish_style();
        let image = self
            .content_image_in(rect)
            .and_then(|image| finish.compose(image));
        self.copy_image_result(image, self.clipboard_image_origin(rect), window);
        cx.notify();
    }
    fn copy_image_result(
        &mut self,
        image: Result<Image>,
        origin: Option<(i32, i32)>,
        window: &mut Window,
    ) {
        match image.and_then(|image| {
            if self.editor.long_clip().is_some() {
                export::copy_image_with(&image, hwnd(window), self.long_clip_placement)?;
            } else {
                export::copy_image_at(&image, hwnd(window), origin)?;
            }
            let warning = if let Err(e) = self.record_session_history(&image) {
                let warning = trf!(
                    "复制已完成，但历史保存失败：{e}",
                    "Copied, but saving history failed: {e}"
                );
                self.notify_output_warning(&warning);
                Some(warning)
            } else {
                None
            };
            Ok(warning)
        }) {
            Ok(warning) => self.finish_output(
                warning.as_deref().unwrap_or(tr!(
                    "已完成，可继续编辑图片",
                    "Done; you can keep editing the image"
                )),
                window,
            ),
            Err(e) => self.status = e.to_string(),
        }
    }
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finish_content_or_status() {
            return;
        }
        self.editor.apply_lasso();
        let image = match self.selected_image() {
            Ok(image) => Arc::new(image),
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        self.saving = true;
        self.clear_idle_preview_cursor();
        self.status = tr!(
            "选择保存格式、名称和位置…",
            "Choose format, name and location…"
        )
        .into();
        let protected = self.image_canvas.as_ref().and_then(|s| s.path.clone());
        let owner = window
            .window_handle()
            .downcast::<Self>()
            .expect("截图窗口类型");
        let history_image = image.clone();
        let record_history = self.quick_action.record_history();
        let opened = crate::save_panel::SavePanel::open(
            image,
            protected,
            tr!("保存图片", "Save image"),
            move |outcome, cx| {
                use crate::save_panel::SaveOutcome;
                match outcome {
                    SaveOutcome::Cancelled => {
                        let _ = owner.update(cx, |v, w, cx| {
                            v.saving = false;
                            v.status.clear();
                            crate::gpui_capture::foreground_auxiliary(w);
                            w.focus(&v.focus);
                            cx.notify();
                        });
                    }
                    SaveOutcome::Saved { path, mut warnings } => {
                        let task = cx.background_executor().spawn(async move {
                            let history_recorded = crate::save_workflow::append_history(
                                &history_image,
                                record_history,
                                &mut warnings,
                            );
                            (path, warnings, history_recorded)
                        });
                        cx.spawn(async move |cx| {
                            let (path, warnings, history_recorded) = task.await;
                            let _ = owner.update(cx, |v, w, cx| {
                                v.saving = false;
                                v.remember_last_region();
                                if history_recorded {
                                    v.quick_action.history_succeeded();
                                }
                                let message = crate::save_workflow::message(&path, &warnings);
                                if !warnings.is_empty() {
                                    v.notify_output_warning(&warnings.join("；"));
                                }
                                v.finish_output(&message, w);
                                w.focus(&v.focus);
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                }
            },
            cx,
        );
        if let Err(e) = opened {
            self.saving = false;
            self.status = e.to_string();
        }
    }

    fn quick_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.finish_content_or_status() {
            return;
        }
        self.editor.apply_lasso();
        let image = match self.selected_image() {
            Ok(image) => image,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        let protected = self.image_canvas.as_ref().and_then(|s| s.path.clone());
        self.saving = true;
        self.clear_idle_preview_cursor();
        self.status = tr!("正在快速保存…", "Quick saving…").into();
        let record_history = self.quick_action.record_history();
        let task = cx.background_executor().spawn(async move {
            crate::save_workflow::quick_save(&image, protected.as_deref(), record_history)
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, w, cx| {
                v.saving = false;
                match result {
                    Ok(report) => {
                        v.remember_last_region();
                        if report.history_recorded {
                            v.quick_action.history_succeeded();
                        }
                        let message = crate::save_workflow::message(&report.path, &report.warnings);
                        if !report.warnings.is_empty() {
                            v.notify_output_warning(&report.warnings.join("；"));
                        }
                        v.finish_output(&message, w);
                    }
                    Err(e) => v.status = e,
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// 与结果面板“返回截图”共用；只关闭已展示结果，不修改选区和编辑历史。
    pub(crate) fn dismiss_recognition_result(&mut self) -> bool {
        self.image_translation.cancel();
        self.local_ocr = None;
        self.result_body_identity = Arc::new(());
        self.clear_ocr_overlay();
        if self.recognition_session.dismiss() {
            self.processing = false;
        }
        if self.foreground_ocr.cancel("返回截图，撤销自己的 OCR 请求") {
            self.processing = false;
        }
        self.reset_result_session();
        self.recognition_result.take().is_some()
    }
    /// 新结果即使全文相同也开始独立选择会话；旧 frame 的命中布局随代次失效。
    fn show_recognition_result(&mut self, text: String, origin: text_result::Origin, title: &str) {
        self.local_ocr = None;
        self.ocr_result_installation = self
            .ocr_result_installation
            .checked_add(1)
            .expect("OCR 安装身份计数耗尽");
        self.result_body_identity = Arc::new(());
        self.clear_ocr_overlay();
        self.clear_idle_preview_cursor();
        if self.recognition_session.show(origin) {
            self.processing = false;
        }
        if self.foreground_ocr.cancel("显示新结果，撤销旧 OCR 请求") {
            self.processing = false;
        }
        self.reset_result_session();
        self.result_title = title.into();
        self.recognition_result = Some(text);
    }
    pub(crate) fn show_control_text_result(&mut self, text: String, title: &str) {
        self.show_recognition_result(text, text_result::Origin::ControlText, title);
    }
    pub(crate) fn show_qr_text_result(&mut self, text: String) {
        self.show_recognition_result(text, text_result::Origin::Qr, tr!("二维码识别", "QR code"));
    }
    pub(crate) fn retain_auto_copy_failure(
        &mut self,
        error: &dyn std::fmt::Display,
        target: text_result::InputTarget,
    ) {
        eprintln!("自动文字复制失败，保留结果供重试：{error}");
        self.auto_copy_recovery = Some((self.result_generation, target));
        self.status = tr!("文字复制失败，结果已保留", "Copy failed; result kept").into();
    }
    pub(crate) fn auto_copy_recovery_is_current(&self) -> bool {
        let Some((generation, target)) = &self.auto_copy_recovery else {
            return false;
        };
        let current = match target {
            text_result::InputTarget::Content(_) => self.current_ocr_target(),
            text_result::InputTarget::Frozen(_) => self.current_frozen_ocr_target(),
        };
        *generation == self.result_generation
            && self.recognition_result.is_some()
            && self.result_editor.is_none()
            && !self.pin_mode
            && self.image_canvas.is_none()
            && current.is_some_and(|current| target.is_current(&current, true))
    }
    pub(crate) fn invalidate_ocr_display_request(&mut self) {
        self.auto_copy_recovery = None;
        let initial = self.recognition_session.invalidate_requests();
        let foreground = self.foreground_ocr.cancel("新的识别请求替换旧请求");
        if initial || foreground {
            self.processing = false;
        }
    }

    pub(crate) fn invalidate_ocr_result_projection(&mut self) {
        self.cancel_ocr_result_click();
        self.invalidate_ocr_result_body_paint();
        self.result_selection_end(self.result_generation);
        self.cancel_ocr_typed_pointer_clicks();
        self.painted_result_output.set(None);
        self.painted_result_projection.borrow_mut().take();
        self.result_input_layout.borrow_mut().take();
        self.ocr_projection_epoch = Arc::new(());
    }

    fn cancel_capture_ocr_tasks(&mut self, reason: &str) {
        if self.quick_ocr_pending() {
            self.quick_action.fail();
        }
        self.local_ocr = None;
        self.auto_copy_recovery = None;
        let initial = self.recognition_session.invalidate_requests();
        let foreground = self.foreground_ocr.cancel(reason);
        if initial || foreground {
            self.processing = false;
        }
        self.smart_candidates.cancel_detection(reason);
    }

    /// Render 的 smart sync 总会调用本门，图片编辑及离开智能工具也观察失效。
    /// Resize 中间态不使 ROI 失效；源身份及已绘制内容仍立即核验。
    pub(crate) fn sync_ocr_task_targets(&mut self) {
        if self.auto_copy_recovery.is_some() && !self.auto_copy_recovery_is_current() {
            self.auto_copy_recovery = None;
            self.status = tr!(
                "截图已改变，自动复制已取消；旧文字仍保留",
                "Capture changed; auto copy cancelled, text kept"
            )
            .into();
        }
        if !self.recognition_session.has_pending() && self.foreground_ocr.key().is_none() {
            return;
        }
        let stable_roi = !self.editor.is_changing_crop();
        let current = self.current_ocr_target().or_else(|| {
            (!stable_roi).then(|| text_result::OcrTarget {
                canvas: self.source.bounds,
                selected: self.source.full(),
                revisions: (
                    self.editor.document.revision(),
                    self.editor.document.edit_revision(),
                ),
                stamp: self.editor.scene_stamp(),
                identity: crate::scene::SourceIdentity::of(&self.source_pixels()),
            })
        });
        let revision = self.result_content_revision();
        let retry_before = self.recognition_session.retry_busy();
        let release_initial =
            self.recognition_session
                .invalidate_stale(current.as_ref(), revision, stable_roi);
        let stale_foreground = self.foreground_ocr.key().is_some_and(|target| {
            let input_current = match target {
                text_result::InputTarget::Content(_) => current.clone(),
                text_result::InputTarget::Frozen(_) => {
                    self.current_frozen_ocr_target().or_else(|| {
                        (!stable_roi).then(|| self.frozen_ocr_target_in(self.source.full()))
                    })
                }
            };
            input_current
                .as_ref()
                .is_none_or(|current| !target.is_current(current, stable_roi))
        });
        let release_foreground = stale_foreground
            && self
                .foreground_ocr
                .cancel("冻结源、稳定选区或读取内容已改变");
        if release_initial || release_foreground {
            if self.quick_ocr_pending() {
                self.quick_action.fail();
                show_screenshot_toolbar(&mut self.toolbar_visibility, &self.editor);
            }
            self.processing = false;
        }
        if retry_before
            && !self.recognition_session.retry_busy()
            && !self.processing
            && self.status == tr!("正在本机重新识别文字…", "Recognizing text again locally…")
        {
            self.status = tr!(
                "当前截图或结果文字已改变，保留当前结果",
                "The capture or result text changed; keeping the current result"
            )
            .into();
        }
    }
    fn result_content_revision(&self) -> u64 {
        self.result_editor
            .as_ref()
            .map_or(self.recognition_session.content_revision, |editor| {
                editor.revision
            })
    }
    pub(crate) fn current_result_body_identity(&self) -> Arc<()> {
        self.result_editor.as_ref().map_or_else(
            || Arc::clone(&self.result_body_identity),
            |editor| Arc::clone(&editor.body_identity),
        )
    }
    pub(crate) fn ocr_find_result_identity(&self) -> (u64, u64) {
        (self.result_generation, self.result_content_revision())
    }
    pub(crate) fn current_ocr_target(&self) -> Option<text_result::OcrTarget> {
        Some(text_result::OcrTarget {
            canvas: self.source.bounds,
            selected: self.output_rect().ok()?,
            revisions: (
                self.editor.document.revision(),
                self.editor.document.edit_revision(),
            ),
            stamp: self.editor.scene_stamp(),
            identity: crate::scene::SourceIdentity::of(&self.source_pixels()),
        })
    }
    /// UIA 与 OCR 补救使用确认 crop 的原始底图 ROI，套索不改变其查询范围。
    pub(crate) fn current_frozen_ocr_target(&self) -> Option<text_result::OcrTarget> {
        Some(self.frozen_ocr_target_in(self.selected().ok()?))
    }
    pub(crate) fn frozen_ocr_target_in(&self, selected: Rect) -> text_result::OcrTarget {
        text_result::OcrTarget {
            canvas: self.source.bounds,
            selected,
            revisions: (
                self.editor.document.revision(),
                self.editor.document.edit_revision(),
            ),
            stamp: self.editor.scene_stamp(),
            identity: self.frozen_source_identity(),
        }
    }
    pub(crate) fn frozen_source_identity(&self) -> crate::scene::SourceIdentity {
        text_result::frozen_source_identity(self.source_pixels())
    }
    pub(crate) fn has_ocr_result(&self) -> bool {
        self.inline_ocr_host()
            && self.recognition_result.is_some()
            && self.recognition_session.ocr_target().is_some()
    }
    pub(crate) fn result_retry_busy(&self) -> bool {
        self.recognition_session.retry_busy()
    }
    pub(crate) fn result_retry_available(&self) -> bool {
        self.has_ocr_result()
            && self.result_selection_pointer_ready()
            && !self.result_selection_dragging(self.result_generation)
            && self.toolbar_drag.is_none()
            && !self.result_editor_composing()
            && !self.result_retry_busy()
            && self.current_ocr_target().as_ref() == self.recognition_session.ocr_target()
    }
    pub(crate) fn retry_recognition_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let click_trace = ocr_progress::ClickTrace::start("retry_dispatch");
        if !self.result_retry_available() {
            return;
        }
        let target = self.current_ocr_target().unwrap();
        let image = match self.recognition_content_image(target.selected) {
            Ok(image) => image,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        if let Some(trace) = &click_trace {
            trace.event("bgra_ready");
        }
        let content_revision = self.result_content_revision();
        // 完整复识别是新请求，可选择新设置；已有行候选仍绑定其原请求档位。
        let profile = crate::preferences::Preferences::load().ocr_profile;
        let provider = crate::recognition::OcrProvider::default();
        let local_input = crate::capture_ocr_overlay::local_retry_host::FixedInput::copy_of_profile(
            &image,
            target.clone(),
            provider,
            profile,
        );
        let Some(request) = self
            .recognition_session
            .begin_retry(target, content_revision)
        else {
            return;
        };
        self.auto_copy_recovery = None;
        self.cancel_local_ocr_intent("完整结果复识别撤销局部候选");
        let task_token = self.recognition_session.retry_token(request.id).unwrap();
        // 独立 pending 不占用 processing：旧全文仍可选字、编辑、复制和返回。
        self.status = tr!("正在本机重新识别文字…", "Recognizing text again locally…").into();
        cx.notify();
        if let Some(trace) = &click_trace {
            trace.bind(
                ocr_progress::TaskKey {
                    kind: ocr_progress::TaskKind::Retry,
                    id: request.id,
                },
                profile,
            );
        }
        self.ocr_click_trace = click_trace.clone();
        let worker_trace = click_trace.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(trace) = &worker_trace {
                trace.event("worker_started");
            }
            let receipt = ocr_task::Receipt::from_result(
                crate::recognition::recognize_with_profile_cancellable(
                    image,
                    provider,
                    profile,
                    &task_token,
                ),
            );
            if let Some(trace) = &worker_trace {
                trace.event("worker_done");
            }
            receipt
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if let Some(trace) = &click_trace {
                    trace.event("receipt_callback");
                }
                let Some(owned) = view.recognition_session.finish_retry(request.id) else {
                    return;
                };
                if let Some(trace) = &click_trace {
                    trace.event("receipt_claimed");
                }
                let previous_installation = view.ocr_result_installation;
                let result = match result {
                    ocr_task::Receipt::Completed(recognized) => Ok(recognized),
                    ocr_task::Receipt::Failed(error) => Err(error),
                    ocr_task::Receipt::Cancelled => {
                        // Cancelled 不替换旧正文/编辑/标记，也不走 copy-only 路由。
                        if !view.processing
                            && view.status
                                == tr!("正在本机重新识别文字…", "Recognizing text again locally…")
                        {
                            view.status.clear();
                        }
                        cx.notify();
                        return;
                    }
                };
                let current = !view.processing
                    && view.result_selection_pointer_ready()
                    && !view.result_selection_dragging(view.result_generation)
                    && view.toolbar_drag.is_none()
                    && !view.result_editor_composing()
                    && view.current_ocr_target().is_some_and(|target| {
                        view.recognition_session.request_is_current(
                            &owned,
                            &target,
                            view.result_content_revision(),
                        )
                    });
                if !current {
                    if !view.processing
                        && view.status
                            == tr!("正在本机重新识别文字…", "Recognizing text again locally…")
                    {
                        view.status = tr!(
                            "当前截图或结果文字已改变，保留当前结果",
                            "The capture or result text changed; keeping the current result"
                        )
                        .into();
                    }
                    cx.notify();
                    return;
                }
                // Retry 永远展示结果，不重读初次捕获的 copy-only 偏好。
                match text_result::route(
                    result
                        .as_ref()
                        .map(|result| result.text.clone())
                        .map_err(Clone::clone),
                    false,
                    true,
                ) {
                    text_result::Completion::ShowResult(text) => {
                        view.status.clear();
                        view.show_recognition_result(
                            text,
                            text_result::Origin::Ocr(owned.target.clone()),
                            tr!("本机文字识别", "Local text recognition"),
                        );
                        if let Ok(recognized) = result {
                            view.install_table_copy(&recognized, owned.target.clone());
                            view.install_ocr_overlay(&recognized, owned.target);
                            view.install_local_ocr_input(local_input, recognized, window, cx);
                        }
                    }
                    text_result::Completion::KeepOpenWithError(error) => view.status = error,
                    text_result::Completion::CopyAndClose(_) => unreachable!("Retry 不复制关闭"),
                }
                if let Some(trace) = &click_trace {
                    if view.ocr_result_installation != previous_installation {
                        trace.result_installed(view, window, cx);
                    } else {
                        trace.event("result_not_installed");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn reset_result_session(&mut self) {
        self.cancel_ocr_result_click();
        self.invalidate_ocr_result_body_paint();
        self.cancel_ocr_typed_pointer_clicks();
        self.auto_copy_recovery = None;
        self.table_copy = None;
        self.table_copy_click = None;
        self.translation = None;
        self.translation_copy_click = None;
        self.translation_close_click = None;
        self.painted_result_output.set(None);
        self.painted_result_projection.borrow_mut().take();
        self.result_editor = None;
        self.result_input_handler = None;
        self.result_input_layout.borrow_mut().take();
        self.result_reveal_caret.set(false);
        self.result_editor_scroll = gpui::ScrollHandle::new();
        self.result_selection.reset();
        self.result_generation = self.result_generation.wrapping_add(1);
    }
    pub(crate) fn current_result_text(&self) -> Option<&str> {
        self.result_editor
            .as_ref()
            .map(|editor| editor.text.as_str())
            .or(self.recognition_result.as_deref())
    }
    pub(crate) fn refresh_ocr_result_target(&mut self, target: text_result::OcrTarget) {
        self.auto_copy_recovery = None;
        let revision = self.result_content_revision();
        let before = self
            .recognition_session
            .ocr_target()
            .map(|source| (source.clone(), revision));
        let body = self.current_result_text().map(str::to_owned);
        if self.table_copy.as_mut().is_some_and(|cache| {
            body.as_deref().is_none_or(|body| {
                !cache.rebase(
                    before.as_ref(),
                    (target.clone(), revision),
                    self.result_generation,
                    body,
                )
            })
        }) {
            self.table_copy = None;
        }
        if self
            .recognition_session
            .show(text_result::Origin::Ocr(target))
        {
            self.processing = false;
        }
        self.recognition_session.content_revision = revision;
    }
    pub(crate) fn ocr_overlay_host_blocked(&self) -> bool {
        self.screenshot_toolbar_menu.presentation().is_some()
            || self.flyout.open().is_some()
            || self.input.marked.is_some()
            || self.text_composing()
    }
    pub(crate) fn result_editor_ready(&self) -> bool {
        self.result_editor.is_some() && self.result_selection_pointer_ready()
    }
    pub(crate) fn result_editor_composing(&self) -> bool {
        self.result_editor_ready()
            && self
                .result_editor
                .as_ref()
                .is_some_and(|editor| editor.composing())
    }
    pub(crate) fn toggle_result_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.result_selection_pointer_ready() {
            return;
        }
        let table_generation = self.result_generation;
        let table_copy = self.table_copy.take();
        if let Some(mut editor) = self.result_editor.take() {
            // 收起只保存已提交的结果文字；预编辑取消不触及截图文档。
            editor.cancel_composition();
            self.result_body_identity = Arc::clone(&editor.body_identity);
            self.recognition_session.content_revision = editor.revision;
            self.recognition_result = Some(editor.text);
            self.reset_result_session();
        } else if let Some(text) = self.recognition_result.clone() {
            let selection = self.result_selection.range();
            self.reset_result_session();
            let mut editor = text_result::ResultEditor::new(text, selection);
            editor.body_identity = Arc::clone(&self.result_body_identity);
            editor.revision = self.recognition_session.content_revision;
            self.result_editor = Some(editor);
            let owner = cx.entity().downgrade();
            let generation = self.result_generation;
            self.result_input_handler = Some(
                cx.new(move |_| crate::text_input::ResultInputHandler::new(owner, generation)),
            );
            self.result_reveal_caret.set(true);
        }
        // 仅打开/收起未变正文会换 UI frame；同源同正文才重绑缓存代次。
        if let (Some(mut cache), Some(target), Some(body)) = (
            table_copy,
            self.current_ocr_target(),
            self.current_result_text(),
        ) {
            if self.recognition_session.ocr_target() == Some(&target)
                && cache.advance_generation(
                    &(target.clone(), self.result_content_revision()),
                    table_generation,
                    self.result_generation,
                    body,
                )
            {
                self.table_copy = Some(cache);
            }
        }
        self.invalidate_click_sequence();
        window.focus(&self.focus);
        cx.notify();
    }
    pub(crate) fn result_selection_range(&self) -> std::ops::Range<usize> {
        self.result_editor.as_ref().map_or_else(
            || self.result_selection.range(),
            |editor| editor.input.selection.clone(),
        )
    }
    pub(crate) fn result_selection_generation(&self) -> u64 {
        self.result_generation
    }
    pub(crate) fn result_selection_dragging(&self, generation: u64) -> bool {
        generation == self.result_generation
            && self.result_editor.as_ref().map_or_else(
                || self.result_selection.dragging(),
                |editor| editor.dragging(),
            )
    }
    fn result_frame_is_current(&self, generation: u64, revision: Option<u64>) -> bool {
        generation == self.result_generation
            && self.result_editor.as_ref().map(|editor| editor.revision) == revision
    }
    fn result_selection_is_idle(&self) -> bool {
        self.inline_ocr_host()
            && self.inline_ocr_interaction_idle()
            && !self.processing
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && self.editor.lasso_draft.is_none()
            && self.recognition_result.is_some()
    }
    pub(crate) fn result_selection_pointer_ready(&self) -> bool {
        self.result_selection_is_idle()
            && !self.pin_handoff_pending
            && !self.saving
            && !self.watermark_input_focused
            && self.input.marked.is_none()
            && self.editor.text_draft.is_none()
            && self.toolbar_panel == ToolbarPanel::None
            && self.screenshot_toolbar_menu.presentation().is_none()
    }
    pub(crate) fn result_selection_down(
        &mut self,
        generation: u64,
        revision: Option<u64>,
        layout: &gpui::TextLayout,
        position: gpui::Point<gpui::Pixels>,
    ) -> bool {
        if !self.result_frame_is_current(generation, revision)
            || !self.result_selection_pointer_ready()
            || self.result_editor_composing()
        {
            return false;
        }
        self.clear_find_preview();
        let index = layout
            .index_for_position(position)
            .unwrap_or_else(|index| index);
        if let Some(editor) = &mut self.result_editor {
            editor.begin_drag(index.min(editor.text.len()));
        } else {
            self.result_selection
                .begin(self.recognition_result.as_deref().unwrap(), index);
        }
        true
    }
    pub(crate) fn result_selection_move(
        &mut self,
        generation: u64,
        revision: Option<u64>,
        layout: &gpui::TextLayout,
        event: &MouseMoveEvent,
    ) -> bool {
        if !self.result_frame_is_current(generation, revision) {
            return false;
        }
        if event.pressed_button != Some(gpui::MouseButton::Left)
            || !self.result_selection_pointer_ready()
        {
            return self.result_selection_end(generation);
        }
        if self.result_selection_dragging(generation) {
            let before = self.result_selection_range();
            let index = layout
                .index_for_position(event.position)
                .unwrap_or_else(|index| index);
            if let Some(editor) = &mut self.result_editor {
                editor.extend_drag(index.min(editor.text.len()));
            } else {
                self.result_selection
                    .extend(self.recognition_result.as_deref().unwrap(), index);
            }
            return before != self.result_selection_range();
        }
        false
    }
    pub(crate) fn result_selection_end(&mut self, generation: u64) -> bool {
        if generation != self.result_generation {
            return false;
        }
        let readonly_drag = self.result_selection.end_drag();
        self.result_editor
            .as_mut()
            .is_some_and(|editor| editor.end_drag())
            || readonly_drag
    }
    pub(crate) fn result_selection_up(
        &mut self,
        generation: u64,
        revision: Option<u64>,
        layout: &gpui::TextLayout,
        position: gpui::Point<gpui::Pixels>,
    ) -> bool {
        if generation == self.result_generation && self.result_selection_dragging(generation) {
            if self.result_frame_is_current(generation, revision)
                && self.result_selection_pointer_ready()
            {
                let index = layout
                    .index_for_position(position)
                    .unwrap_or_else(|index| index);
                if let Some(editor) = &mut self.result_editor {
                    editor.extend_drag(index.min(editor.text.len()));
                } else {
                    self.result_selection
                        .extend(self.recognition_result.as_deref().unwrap(), index);
                }
            }
            return self.result_selection_end(generation);
        }
        false
    }
    pub(crate) fn result_output_frame(&self) -> text_result::ResultFrame {
        text_result::ResultFrame {
            generation: self.result_generation,
            revision: self.result_editor.as_ref().map(|editor| editor.revision),
        }
    }
    fn result_output_gate(
        &self,
        frame: text_result::ResultFrame,
        window: &Window,
    ) -> text_result::OutputGate {
        text_result::OutputGate {
            owns_result: self.inline_ocr_host()
                && self.recognition_result.is_some()
                && (self.ocr_overlay.is_none() || self.result_editor.is_some()),
            editing: self.result_editor.is_some(),
            input_ready: self.result_selection_pointer_ready()
                && !self.result_editor_composing()
                && !self.result_selection_dragging(self.result_generation)
                && self.pointer_down_start.is_none()
                && self.toolbar_drag.is_none()
                && self.input.drag_anchor.is_none()
                && self.flyout.open().is_none()
                && !self.preview_text_replay,
            frame_current: text_result::result_frame_current(
                self.painted_result_output.get(),
                frame,
                self.result_output_frame(),
            ) && self
                .painted_result_projection
                .borrow()
                .as_ref()
                .is_some_and(|paint| {
                    paint.current(&self.ocr_projection_epoch, self.ocr_projection(window))
                }),
            active: window.is_window_active(),
            focused: self.focus.is_focused(window),
        }
    }
    fn result_editor_owns_keys(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.result_writer_key_current(window, cx)
    }
    fn install_table_copy(
        &mut self,
        recognized: &crate::recognition::RecognizedText,
        target: text_result::OcrTarget,
    ) {
        self.table_forced = false;
        self.table_hover = None;
        let image = self.recognition_content_image(target.selected).ok();
        self.table_copy = Some(crate::pin_view::table_copy::TableCopy::with_image(
            recognized,
            (target, self.result_content_revision()),
            self.result_generation,
            image.as_ref(),
        ));
    }
    /// 后台已按同一识别图片算好线检测；保留输入以便“表格”页强制按表格识别。
    fn install_table_copy_detected(
        &mut self,
        recognized: &crate::recognition::RecognizedText,
        target: text_result::OcrTarget,
        detected: Option<crate::table_lines::DetectedLines>,
        structure: Option<crate::ppocr::table::TableResult>,
    ) -> Option<String> {
        let key = (target.clone(), self.result_content_revision());
        let forced = self.ocr_intent == Some(crate::toolbar_groups::RecognizeMode::Table);
        let note = structure.as_ref().and_then(|table| table.warning.clone());
        self.table_copy = Some(if let Some(table) = structure {
            crate::pin_view::table_copy::TableCopy::with_structure(
                recognized,
                key,
                self.result_generation,
                table.grid,
                table.region,
            )
        } else if forced {
            crate::pin_view::table_copy::TableCopy::forced(
                recognized,
                key,
                self.result_generation,
                detected.as_ref(),
            )
        } else {
            crate::pin_view::table_copy::TableCopy::with_lines(
                recognized,
                key,
                self.result_generation,
                detected.as_ref(),
            )
        });
        self.table_forced = forced;
        self.table_hover = None;
        self.table_input = Some(TableInput {
            target,
            recognized: recognized.clone(),
        });
        note
    }
    /// 显式表格任务复用截图请求的取消与来源守卫；网络和模型不在 UI 线程运行。
    pub(crate) fn force_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table_force_available() {
            return;
        }
        self.perform(
            Action::Recognize(crate::toolbar_groups::RecognizeMode::Table),
            window,
            cx,
        );
    }
    pub(crate) fn table_forced(&self) -> bool {
        self.table_forced
    }
    pub(crate) fn table_force_available(&self) -> bool {
        !self.processing
            && self
                .cached_table_grid()
                .is_none_or(|grid| grid.source != crate::table_grid::TableSource::Model)
            && self.table_input.as_ref().is_some_and(|input| {
                self.current_ocr_target().as_ref() == Some(&input.target)
                    && self.current_result_text() == Some(input.recognized.text.as_str())
            })
    }
    /// OCR 结果刚安装：按“识别并翻译／表格识别”打开对应页。
    fn apply_ocr_intent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mode) = self.ocr_intent.take() {
            if mode == crate::toolbar_groups::RecognizeMode::Table {
                self.set_ocr_panel_view(crate::ocr_panel::PanelView::Table, cx);
            } else {
                self.apply_recognize_mode_to_result(mode, window, cx);
            }
        }
    }
    /// 已有原位结果时翻译直接切页；表格仍需发起独立结构识别任务。
    fn apply_recognize_mode_to_result(
        &mut self,
        mode: crate::toolbar_groups::RecognizeMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::{ocr_panel::PanelView, toolbar_groups::RecognizeMode};
        if !self.ocr_overlay_current() || self.processing {
            return false;
        }
        match mode {
            RecognizeMode::TextTranslate => {
                self.set_ocr_panel_view(PanelView::Translation, cx);
                true
            }
            RecognizeMode::Table => false,
            _ => false,
        }
    }
    pub(crate) fn cached_table_text(&self) -> Option<&str> {
        if !self.has_ocr_result() {
            return None;
        }
        let target = self.recognition_session.ocr_target()?;
        if self.current_ocr_target().as_ref() != Some(target) {
            return None;
        }
        self.table_copy.as_ref()?.text(
            &(target.clone(), self.result_content_revision()),
            self.result_generation,
            self.current_result_text()?,
        )
    }
    pub(crate) fn cached_table_grid(&self) -> Option<&crate::table_grid::TableGrid> {
        self.cached_table_text()?;
        let target = self.recognition_session.ocr_target()?;
        self.table_copy.as_ref()?.grid(
            &(target.clone(), self.result_content_revision()),
            self.result_generation,
            self.current_result_text()?,
        )
    }
    /// 表格只占部分识别行时的来源行号；独立表格或无表格为 None。
    pub(crate) fn cached_table_region(&self) -> Option<&[usize]> {
        self.cached_table_grid()?;
        let target = self.recognition_session.ocr_target()?;
        self.table_copy.as_ref()?.region(
            &(target.clone(), self.result_content_revision()),
            self.result_generation,
            self.current_result_text()?,
        )
    }
    fn table_copy_click_stamp(
        &self,
        frame: text_result::ResultFrame,
        inline: bool,
    ) -> Option<text_result::TableClickStamp<text_result::OcrTarget>> {
        (frame == self.result_output_frame() && self.has_ocr_result()).then_some(())?;
        Some(text_result::TableClickStamp {
            source: self.current_ocr_target()?,
            frame,
            body_identity: self.current_result_body_identity(),
            inline,
        })
    }
    pub(crate) fn render_table_copy_button(
        &self,
        frame: text_result::ResultFrame,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use gpui::{div, prelude::*, MouseButton};
        // 渲染只借用已缓存结果；真实来源、paint、焦点和输入闲置在点击时再核。
        let enabled = self.cached_table_text().is_some() && !self.processing && !self.saving;
        let down_stamp = enabled
            .then(|| self.table_copy_click_stamp(frame, inline))
            .flatten();
        div()
            .id(if inline {
                "ocr-inline-copy-table"
            } else {
                "ocr-copy-table"
            })
            .flex_shrink_0()
            .opacity(if enabled { 1. } else { 0.45 })
            .when(enabled, |button| button.cursor_pointer())
            .child(tr!("复制表格", "Copy table"))
            .tooltip(move |_, cx| {
                let hint = if enabled {
                    tr!(
                        "将完整识别结果复制为表格",
                        "Copy the complete OCR result as a table"
                    )
                } else {
                    tr!(
                        "没有识别出完整表格，或正文已修改；仍可复制原文",
                        "No complete table was recognized, or the text was edited; you can still copy the text"
                    )
                };
                cx.new(move |_| crate::pin_view::TextToolHint(hint)).into()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    // Down 的来源不能由释放时的新渲染闭包重新采集。
                    view.cancel_ocr_typed_pointer_clicks();
                    view.table_copy_click = down_stamp.clone().filter(|pressed| {
                        !event.first_mouse
                            && !view.processing
                            && !view.saving
                            && view.text_copy_completion_is_idle()
                            && view.ocr_typed_pointer_capture_current(window, true)
                            && if inline {
                                view.ocr_overlay_text_output_down_ready(window)
                            } else {
                                view.result_output_gate(frame, window).ready()
                            }
                            && view.cached_table_text().is_some()
                            && view
                                .table_copy_click_stamp(frame, inline)
                                .is_some_and(|current| pressed.matches(&current))
                    });
                    view.table_copy_down = view
                        .table_copy_click
                        .as_ref()
                        .map(|_| crate::capture_ocr_overlay::pointer_down_proof(event));
                    if view.table_copy_down.is_some() {
                        view.observe_ocr_typed_pointer_click(window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |view, event, window, cx| {
                cx.stop_propagation();
                let (actual, left_pair) = crate::capture_ocr_overlay::pointer_click_proof(event);
                let saved = view.table_copy_down.take();
                let proof_current =
                    left_pair && saved.is_some_and(|saved| actual.as_ref() == Some(&saved));
                if !proof_current {
                    view.table_copy_click = None;
                    return;
                }
                // 先消费：一次真实点击最多写一次，失效不能变成后续重试。
                if let Some(pressed) = view.table_copy_click.take() {
                    if view
                        .table_copy_click_stamp(frame, inline)
                        .is_some_and(|current| pressed.matches(&current))
                    {
                        view.copy_table_result(frame, inline, window, cx);
                    }
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }
    fn copy_table_result(
        &mut self,
        frame: text_result::ResultFrame,
        inline: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.text_copy_completion_is_idle()
            || !self.ocr_typed_pointer_capture_current(window, false)
            || frame != self.result_output_frame()
            || if inline {
                self.result_editor.is_some() || !self.ocr_overlay_text_output_ready(window)
            } else {
                !self.result_output_gate(frame, window).ready()
            }
        {
            return;
        }
        let Some(text) = self.cached_table_text() else {
            self.status = tr!(
                "没有识别出完整表格，或正文已修改",
                "No complete table, or the text was edited"
            )
            .into();
            cx.notify();
            return;
        };
        let result: Result<bool> = match self.cached_table_grid() {
            Some(grid) => crate::table_clipboard::copy_table(grid, hwnd(window)),
            None => export::copy_text(text, hwnd(window)),
        }
        .map(|_| !text.is_empty());
        self.status = match result {
            Ok(true) => tr!("表格已复制", "Table copied").into(),
            Ok(false) => tr!("没有可复制的表格", "No table to copy").into(),
            Err(error) => {
                eprintln!("表格复制失败：{error}");
                tr!(
                    "表格复制失败；结果已保留，请重试",
                    "Table copy failed; result kept, retry"
                )
                .into()
            }
        };
        // TSV 来自完整识别矩阵，不读取选字范围；不关闭、不切焦点、不写标记或历史。
        cx.notify();
    }
    fn copy_result_output(
        &mut self,
        scope: text_result::CopyScope,
        frame: text_result::ResultFrame,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.result_output_gate(frame, window).ready()
            || !self.text_copy_completion_is_idle()
            || !self.ocr_typed_pointer_capture_current(window, false)
        {
            return;
        }
        let result = text_result::publish_text(
            self.current_result_text().unwrap(),
            self.result_selection_range(),
            scope,
            |text| export::copy_text(text, hwnd(window)),
        );
        self.status = match result {
            Ok(true) => tr!("文字已复制", "Text copied").into(),
            Ok(false) => tr!("未识别到文字", "No text recognized").into(),
            Err(error) => {
                eprintln!("结果文字复制失败：{error}");
                tr!("文字复制失败，请重试", "Copy failed; retry").into()
            }
        };
        // 不重置结果、选择、输入、编辑历史或截图会话；失败也可原地重试。
        cx.notify();
    }
    /// 恢复态 Enter 重试自动全文；有原位选择的 Ctrl+C 保留选字/保持打开合同。
    fn retry_auto_copy_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((generation, _)) = &self.auto_copy_recovery else {
            return false;
        };
        if *generation != self.result_generation || self.result_editor.is_some() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        let bare = !m.modified();
        let control_only = m.control && !m.shift && !m.alt && !m.platform && !m.function;
        if !((key == "enter" && bare) || (key == "c" && control_only)) {
            return false;
        }
        if !self.auto_copy_recovery_is_current() {
            // 原自动输出目标已失效：消耗这次恢复键，不能关闭后来改变的截图。
            self.auto_copy_recovery = None;
            self.status = tr!(
                "截图已改变，自动复制已取消；旧文字仍保留",
                "Capture changed; auto copy cancelled, text kept"
            )
            .into();
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        self.validate_ocr_overlay();
        let frame = self.result_output_frame();
        let gate = self.result_output_gate(frame, window);
        if event.is_held
            || !gate.input_ready
            || !gate.frame_current
            || !gate.active
            || !gate.focused
            || !self.text_copy_completion_is_idle()
            || self.ocr_overlay_dragging()
        {
            // 即使失焦/旧paint/拖选也不能穿透到原位 Enter 图片输出。
            cx.stop_propagation();
            return true;
        }
        let panel_selected = self
            .recognition_result
            .as_deref()
            .and_then(|text| text.get(self.result_selection_range()))
            .is_some_and(|text| !text.is_empty());
        if key == "c" && (self.ocr_overlay_has_selection() || panel_selected) {
            // 原位/结果面板继续复制所选并保持打开；未完成的自动全文仍有独立提示。
            return false;
        }
        cx.stop_propagation();
        let text = self
            .recognition_result
            .clone()
            .expect("当前自动复制失败结果");
        match text_result::publish_auto_copy(text, |text| export::copy_text(text, hwnd(window))) {
            text_result::AutoCopyPublication::Close => {
                self.auto_copy_recovery = None;
                self.perform(Action::Cancel, window, cx);
            }
            text_result::AutoCopyPublication::KeepResult { text, error } => {
                self.recognition_result = Some(text);
                let target = self.auto_copy_recovery.as_ref().unwrap().1.clone();
                self.retain_auto_copy_failure(&error, target);
                cx.notify();
            }
        }
        true
    }
    /// 全文按钮保留全文语义；只读 Enter / Ctrl+C 共用选择或全文语义。
    pub(crate) fn copy_recognition_result(
        &mut self,
        frame: text_result::ResultFrame,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.result_frame_is_current(frame.generation, frame.revision)
            || self
                .result_editor
                .as_ref()
                .is_some_and(|editor| editor.composing())
        {
            return;
        }
        if self.pin_mode {
            // 贴图仍由专用宿主负责文字输出；图片沿可失败、保留结果的发布门。
            if let Some(text) = self.current_result_text() {
                if let Some(text) =
                    text_result::copy_payload(text, 0..0, text_result::CopyScope::Full)
                {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_owned()));
                }
            }
            return;
        }
        self.copy_result_output(text_result::CopyScope::Full, frame, window, cx);
    }
    /// 仅在用户点击时把所选（无选择时为全部）文字发往已配置的翻译服务。
    pub(crate) fn translate_recognition_result(
        &mut self,
        frame: text_result::ResultFrame,
        cx: &mut Context<Self>,
    ) {
        if !self.result_frame_is_current(frame.generation, frame.revision)
            || self.result_editor_composing()
            || self.translation_pending()
        {
            return;
        }
        let text = match (&self.result_editor, self.recognition_result.as_deref()) {
            (Some(editor), _) => editor.selected_or_full().to_owned(),
            (None, Some(text)) => text
                .get(self.result_selection.range())
                .filter(|selected| !selected.is_empty())
                .unwrap_or(text)
                .to_owned(),
            (None, None) => return,
        };
        self.start_translation(text, cx);
    }
    fn translation_source(&self) -> Option<crate::translate::TranslationSource> {
        self.recognition_result.as_ref()?;
        Some(crate::translate::TranslationSource {
            generation: self.result_generation,
            body_identity: self.current_result_body_identity(),
        })
    }
    pub(crate) fn cancel_ocr_typed_pointer_clicks(&mut self) {
        self.table_copy_click = None;
        self.table_copy_down = None;
        self.cancel_translation_clicks();
        self.typed_pointer_epoch = Arc::new(());
        self.typed_pointer_subscriptions.clear();
    }
    fn observe_ocr_typed_pointer_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(focus) = window.focused(cx) else {
            self.cancel_ocr_typed_pointer_clicks();
            return;
        };
        let token = self.typed_pointer_epoch.clone();
        let blur = cx.on_blur(&focus, window, move |view, _, _| {
            if Arc::ptr_eq(&view.typed_pointer_epoch, &token) {
                view.cancel_ocr_typed_pointer_clicks();
            }
        });
        let token = self.typed_pointer_epoch.clone();
        let activation = cx.observe_window_activation(window, move |view, window, _| {
            if !window.is_window_active() && Arc::ptr_eq(&view.typed_pointer_epoch, &token) {
                view.cancel_ocr_typed_pointer_clicks();
            }
        });
        self.typed_pointer_subscriptions = vec![blur, activation];
    }
    pub(crate) fn ocr_typed_pointer_capture_current(&self, window: &Window, held: bool) -> bool {
        let capture = unsafe { GetCapture() };
        capture.0.is_null()
            || held && capture == hwnd(window) && crate::pin_view::find_ui::primary_button_down()
    }
    fn translation_pointer_access(&self, window: &Window, cx: &gpui::App, held: bool) -> bool {
        window.is_window_active()
            && !self.result_editor_composing()
            && (!(self.image_canvas.is_some() && self.inline_ocr_host())
                || (!self.processing
                    && self.text_copy_completion_is_idle()
                    && self.ocr_find_focus_owned(window, cx)
                    && !self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx))
                    && !self.ocr_overlay_host_blocked()
                    && self.ocr_typed_pointer_capture_current(window, held)
                    && self.ocr_projection(window).is_some_and(|projection| {
                        self.ocr_overlay
                            .as_ref()
                            .is_some_and(|_| self.ocr_inline_projection_ready(window))
                            || self
                                .painted_result_projection
                                .borrow()
                                .as_ref()
                                .is_some_and(|paint| {
                                    paint.current(&self.ocr_projection_epoch, Some(projection))
                                })
                    })))
    }
    fn cancel_translation_clicks(&mut self) -> bool {
        self.translation_copy_down = None;
        self.translation_close_down = None;
        let copy = self.translation_copy_click.take().is_some();
        let close = self.translation_close_click.take().is_some();
        copy || close
    }
    pub(crate) fn current_translation(&self) -> Option<&crate::translate::Translation> {
        let current = self.translation_source();
        self.translation
            .as_ref()
            .filter(|translation| translation.current(current.as_ref()))
    }
    /// 原生输入/按键/失活组合回滚均沿实际正文 Arc 撤销；选择变化不撤销。
    pub(crate) fn invalidate_translation_source(&mut self) -> bool {
        let current = self.translation_source();
        if self
            .translation
            .as_ref()
            .is_some_and(|translation| !translation.current(current.as_ref()))
        {
            self.translation = None;
            self.translation_copy_click = None;
            self.translation_close_click = None;
            return true;
        }
        false
    }
    pub(crate) fn translation_copy_ticket(&self) -> Option<crate::translate::TranslationTicket> {
        self.current_translation()
            .filter(|translation| {
                matches!(
                    translation.state,
                    crate::translate::TranslationState::Done(_)
                )
            })
            .map(crate::translate::Translation::ticket)
    }
    /// 复制按钮保存真实 Down 的译文身份；新译文不能借旧按下发布。
    pub(crate) fn begin_translation_copy(
        &mut self,
        ticket: Option<crate::translate::TranslationTicket>,
        window: &Window,
    ) {
        self.translation_copy_down = None;
        self.translation_copy_click = None;
        if window.is_window_active() && !self.result_editor_composing() {
            self.translation_copy_click = ticket;
        }
    }
    pub(crate) fn begin_translation_copy_pointer(
        &mut self,
        ticket: Option<crate::translate::TranslationTicket>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_ocr_typed_pointer_clicks();
        if event.first_mouse
            || event.button != gpui::MouseButton::Left
            || !self.translation_pointer_access(window, cx, true)
        {
            return;
        }
        self.begin_translation_copy(ticket, window);
        self.translation_copy_down = self
            .translation_copy_click
            .as_ref()
            .map(|_| crate::capture_ocr_overlay::pointer_down_proof(event));
        if self.translation_copy_down.is_some() {
            self.observe_ocr_typed_pointer_click(window, cx);
        }
    }
    pub(crate) fn copy_translation_pointer(
        &mut self,
        event: &gpui::ClickEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let saved = self.translation_copy_down.take();
        let (actual, pair) = crate::capture_ocr_overlay::pointer_click_proof(event);
        if !pair || saved.is_none() || saved != actual {
            self.translation_copy_click = None;
            return;
        }
        self.copy_translation(window, cx);
    }
    /// 原位选字与结果面板共用；网络协议/所选或全文语义保持。
    pub(crate) fn start_translation(&mut self, text: String, cx: &mut Context<Self>) {
        self.invalidate_translation_source();
        if self.translation_pending() || text.trim().is_empty() {
            return;
        }
        let Some(source) = self.translation_source() else {
            return;
        };
        let Some(service) =
            crate::translate::Service::from_preferences(&crate::preferences::Preferences::load())
        else {
            self.status = tr!("翻译服务未配置", "Translation service is not configured").into();
            cx.notify();
            return;
        };
        self.translation_request = self.translation_request.wrapping_add(1);
        self.translation_by_paragraph = false;
        let translation = crate::translate::Translation::new(self.translation_request, source);
        let ticket = translation.ticket();
        self.translation = Some(translation);
        self.translation_copy_click = None;
        self.translation_close_click = None;
        cx.notify();
        if let Some(notice) = service.first_use_notice() {
            self.notify_output_warning(notice);
        }
        let mut receiver = crate::translate::spawn_stream(service, text);
        cx.spawn(async move |view, cx| {
            use crate::translate::StreamEvent;
            use futures::StreamExt;
            let mut result = None;
            while let Some(event) = receiver.next().await {
                match event {
                    StreamEvent::Partial(text) => {
                        // 请求已过期或视图已关闭：丢弃接收端，后台线程随即停止读取。
                        let current = view.update(cx, |view, cx| {
                            let source = view.translation_source();
                            let accepted = view.translation.as_mut().is_some_and(|translation| {
                                translation.progress(&ticket, source.as_ref(), text)
                            });
                            if accepted {
                                cx.notify();
                            }
                            accepted
                        });
                        if !matches!(current, Ok(true)) {
                            return;
                        }
                    }
                    StreamEvent::Done(done) => {
                        result = Some(done);
                        break;
                    }
                }
            }
            let result = result.unwrap_or_else(|| {
                Err(tr!("翻译任务意外结束", "Translation task ended unexpectedly").into())
            });
            let _ = view.update(cx, |view, cx| {
                let current = view.translation_source();
                let Some(translation) = &mut view.translation else {
                    return;
                };
                // 此回执拒绝时不清理任何后来安装的新请求/复制意图。
                if translation.complete(&ticket, current.as_ref(), result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }
    /// T 键在截图有选区时触发识别并翻译，其余场景保留贴图。
    pub(crate) fn t_translates(&self) -> bool {
        !self.pin_mode && self.image_canvas.is_none() && self.editor.crop.is_some()
    }
    pub(crate) fn translation_pending(&self) -> bool {
        self.current_translation()
            .is_some_and(|t| matches!(t.state, crate::translate::TranslationState::Pending))
    }
    pub(crate) fn copy_translation(&mut self, window: &Window, cx: &mut Context<Self>) {
        // 先 take，任何拒绝/失败均不自动重放；用户新 Down 才能显式重试。
        let Some(ticket) = self.translation_copy_click.take() else {
            return;
        };
        if !self.translation_pointer_access(window, cx, false) {
            return;
        }
        let current = self.translation_source();
        let Some(translation) = &self.translation else {
            return;
        };
        self.status = match translation.publish_copy(&ticket, current.as_ref(), |text| {
            export::copy_text(text, hwnd(window))
        }) {
            Ok(true) => tr!("译文已复制", "Translation copied").into(),
            Ok(false) => return,
            Err(error) => trf!("复制译文失败：{}", "Failed to copy translation: {}", error),
        };
        cx.notify();
    }
    pub fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.trim_inputs.as_ref())
            .is_some_and(|input| input.read(cx).focused(window, cx))
        {
            return;
        }
        if self.long_clip_key(event, window, cx) {
            return;
        }
        // 按键可能收起选区条而收不到离开事件；样式悬停预览随按键结束，画面回到实际外观。
        if self.finish_hover.take().is_some() {
            cx.notify();
        }
        if self.watermark_input_focused {
            return;
        }
        if !self.pin_mode && (self.text_composing() || self.result_editor_composing()) {
            cx.stop_propagation();
            return;
        }
        if !self.pin_mode
            && crate::text_contract::terminal_held(&event.keystroke.key, event.is_held)
        {
            cx.stop_propagation();
            return;
        }
        if !self.pin_mode && event.keystroke.key == "escape" {
            let modifiers = event.keystroke.modifiers;
            if modifiers.shift
                || modifiers.control
                || modifiers.alt
                || modifiers.platform
                || modifiers.function
            {
                cx.stop_propagation();
                return;
            }
        }
        if self.refresh_request.is_some() {
            if event.keystroke.key == "escape" && !event.is_held {
                self.cancel_refresh(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.toolbar_drag.is_some() && event.keystroke.key == "escape" {
            self.cancel_toolbar_drag();
            self.release_capture();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        self.preview_cursor_key_down(event);
        self.invalidate_click_sequence();
        if self.pin_handoff_pending {
            if event.keystroke.key == "escape" {
                self.perform(Action::Cancel, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        // 字符先给实际输入器；未消费字符仍由原生 TranslateMessage 送达。
        if self.watermark_input_focused {
            return;
        }
        if self.pin_mode {
            if self.pin_editing && self.pin_key(event, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.text_composing() {
            // 原生 backend 通常已绕过应用键处理；直接转发也不能落到宿主快捷键。
            cx.stop_propagation();
            return;
        }
        if self.result_editor_composing() {
            // 结果输入法只由独立输入器提交/取消，不进入标注或截图快捷键。
            cx.stop_propagation();
            return;
        }
        if crate::text_contract::terminal_held(&event.keystroke.key, event.is_held) {
            cx.stop_propagation();
            return;
        }
        if self.saving {
            if event.keystroke.key == "escape" && self.quick_save_cancel.is_some() {
                self.cancel_quick_action(cx);
                cx.stop_propagation();
            }
            return;
        }
        if event.keystroke.key == "escape" && self.toolbar_panel != ToolbarPanel::None {
            self.toolbar_panel = self.toolbar_panel.parent();
            self.selection_size_input = None;
            window.focus(&self.focus);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if event.keystroke.key == "escape" && self.flyout.close() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.toolbar_panel == ToolbarPanel::Help && event.keystroke.key == "f1" {
            if !event.is_held {
                self.perform(Action::Help, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.toolbar_panel != ToolbarPanel::None && !self.toolbar_panel.more() {
            // 参数输入由子控件处理；保留未消费的字符键给 Windows TranslateMessage，
            // 不在这一层触发绘图、输出或历史快捷键。
            return;
        }
        if event.keystroke.key == "escape" && self.qr_escape() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // 原位导航不能先撤掉菜单再修改其下的选字；正文/IME/参数仍在此前处理。
        if self.ocr_overlay.is_some()
            && self.screenshot_toolbar_menu.presentation().is_some()
            && (crate::capture_ocr_overlay::navigation_key(
                event.keystroke.key.as_str(),
                event.keystroke.modifiers,
            )
            .is_some()
                || crate::pin_view::find_ui::shortcut(
                    event.keystroke.key.as_str(),
                    event.keystroke.modifiers,
                    self.ocr_find_input_focused(window, cx),
                )
                .is_some_and(|action| action != crate::pin_view::find_ui::FindAction::Close))
        {
            cx.stop_propagation();
            return;
        }
        if self.range_shortcut_blocked_by_menu(event) {
            cx.stop_propagation();
            return;
        }
        if self.screenshot_toolbar_menu_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.result_editor_owns_keys(window, cx)
            && !self.preview_text_replay
            && self.result_editor_key(event, window, cx)
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if self.ocr_find_capture_key(event, window, cx) {
            return;
        }
        if self.ocr_find_input_focused(window, cx) {
            return;
        }
        if self.local_ocr_candidate_key(event, window, cx) {
            return;
        }
        if self.retry_auto_copy_key(event, window, cx) {
            return;
        }
        // F1 显隐左下提示卡（没有提示卡的场景仍开帮助），Shift+F1 打开完整帮助。
        if key == "f1" && !m.control && !m.alt && !m.platform && !m.function {
            if !event.is_held {
                if !m.shift && self.f1_toggles_key_hints() {
                    self.toggle_key_hints(cx);
                } else {
                    self.perform(Action::Help, window, cx);
                }
            }
            cx.stop_propagation();
            return;
        }
        if key == "f5" && !m.shift && !m.control && !m.alt && !m.platform && !m.function {
            if !event.is_held {
                self.perform(Action::Refresh, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        // 原位层拥有文字键，但图片的打开/视口快捷键仍由图片宿主处理。
        // IME、正文、Find 和候选已在上方优先；旧持键或未就绪输入只消费。
        if self.image_canvas.is_some()
            && self.recognition_result.is_some()
            && m.control
            && !m.alt
            && !m.platform
            && !m.function
        {
            let image_action = match key {
                "o" if !m.shift => Some(Action::OpenImage),
                "s" => Some(if m.shift {
                    Action::QuickSave
                } else {
                    Action::Save
                }),
                "0" if !m.shift => Some(Action::FitImage),
                "1" if !m.shift => Some(Action::ActualImage),
                "=" | "+" => Some(Action::ZoomImage(1.1)),
                "-" => Some(Action::ZoomImage(1. / 1.1)),
                _ => None,
            };
            if let Some(action) = image_action {
                cx.stop_propagation();
                if !event.is_held
                    && self.result_editor.is_none()
                    && !self.result_selection_dragging(self.result_selection_generation())
                    && !self.ocr_overlay_dragging()
                    && self.text_copy_completion_is_idle()
                    && !self.processing
                    && self.flyout.open().is_none()
                    && window.is_window_active()
                    && self.focus.is_focused(window)
                {
                    self.perform(action, window, cx);
                }
                return;
            }
        }
        if self.ocr_overlay_key(event, window, cx) {
            return;
        }
        // 结果层拥有文字键；忙、失焦和旧帧只拒绝副作用，不穿透到图片复制。
        // 草稿与 Native 重放继续走原有优先路由；未知键只返回，不停止传播。
        let result_frame = self.result_output_frame();
        let result_gate = self.result_output_gate(result_frame, window);
        if result_gate.owns_result && self.editor.text_draft.is_none() && !self.preview_text_replay
        {
            match text_result::output_key(key, m, event.is_held, result_gate) {
                text_result::OutputKey::CopySelection => {
                    self.copy_result_output(
                        text_result::CopyScope::Selection,
                        result_frame,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
                text_result::OutputKey::SelectAll => {
                    self.clear_find_preview();
                    self.result_selection
                        .select_all(self.recognition_result.as_deref().unwrap());
                    cx.notify();
                    cx.stop_propagation();
                }
                text_result::OutputKey::Consume => cx.stop_propagation(),
                text_result::OutputKey::Pass => {
                    if key == "escape" && self.result_selection_is_idle() {
                        self.wheel_input.reset();
                        self.dismiss_recognition_result();
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }
            return;
        }
        let image_control = crate::host_shortcuts::control(m);
        let image_action = if image_control && !m.shift && key == "o" {
            Some(Action::OpenImage)
        } else if image_control && key == "s" {
            Some(if m.shift {
                Action::QuickSave
            } else {
                Action::Save
            })
        } else if image_control && self.image_canvas.is_some() {
            match key {
                "0" if !m.shift => Some(Action::FitImage),
                "1" if !m.shift => Some(Action::ActualImage),
                "=" | "+" => Some(Action::ZoomImage(1.1)),
                "-" => Some(Action::ZoomImage(1. / 1.1)),
                _ => None,
            }
        } else {
            None
        };
        if let Some(action) = image_action {
            self.perform(action, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.text_key(event, window, cx) {
            return;
        }
        if self.editor.text_draft.is_some() {
            return;
        }
        if self.preview_text_replay && self.image_canvas.is_none() {
            // Native 未接受的识别快捷键不能在之后的 crop/hover 上重解释；
            // 文字、IME、参数和菜单仍由上面的原分支优先处理。
            cx.stop_propagation();
            return;
        }
        if self.toggle_screenshot_toolbar(event) {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.move_preview_cursor(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.range_shortcut_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.smart_key(event, window, cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let control = crate::host_shortcuts::control(modifiers);
        let plain = !modifiers.modified();
        if key == "escape" {
            // 直接取消/套索退出不经过 perform；新范围不得继承旧中心半刻度。
            self.wheel_input.reset();
        }
        if self.image_canvas.is_none()
            && !self.pin_mode
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.function
            && ((key == "c" && modifiers.shift)
                || (key == "r" && !modifiers.shift && self.editor.crop.is_none()))
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
        {
            if event.is_held && !modifiers.function {
                cx.stop_propagation();
                return;
            }
            self.perform(Action::ControlText, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.capture_color_available(window, true) {
            if key == "p" && (plain || crate::host_shortcuts::shift(modifiers)) {
                if !event.is_held {
                    // 真实 KeyDown 保留短按，不能仅依赖轮询时按键仍处于按下状态。
                    self.capture_color_p_down = true;
                    self.update_capture_color_keys(modifiers.shift, true, window, cx);
                }
                cx.stop_propagation();
                return;
            }
            if key == "c" && (plain || crate::host_shortcuts::shift(modifiers)) {
                self.observe_capture_color_keys(window, cx);
                if !event.is_held {
                    self.copy_capture_color(window, cx);
                }
                cx.stop_propagation();
                return;
            }
        }
        match lasso_outside_key_action(
            &self.editor,
            key,
            !self.pin_mode && self.image_canvas.is_none(),
        ) {
            LassoOutsideKeyAction::ConsumeEnter => {
                // 闭合套索仍属于调整前范围，原 Resize 未结束时不能应用或复制。
                cx.stop_propagation();
                return;
            }
            LassoOutsideKeyAction::CancelResize => {
                self.editor.cancel_gesture();
                self.clear_pointer_tracking();
                self.preview_cursor_nudge.reset();
                self.release_capture();
                cx.stop_propagation();
                cx.notify();
                return;
            }
            LassoOutsideKeyAction::Pass => {}
        }
        if let Some(tool) = crate::host_shortcuts::drawing_tool(key, modifiers, true) {
            self.perform(Action::Tool(tool), window, cx);
            return;
        }
        let action = match key {
            "escape" if self.editor.tool == Tool::Lasso => {
                if !self.editor.cancel_gesture() {
                    self.editor.select_tool(Tool::Edit);
                }
                self.release_capture();
                cx.notify();
                return;
            }
            "enter" if plain && self.editor.drawing_lasso() => {
                cx.notify();
                return;
            }
            "enter"
                if plain
                    && self.editor.tool == Tool::Lasso
                    && self.editor.lasso_draft.is_some() =>
            {
                self.editor.apply_lasso();
                cx.notify();
                return;
            }
            "escape" => {
                if self.cancel_quick_action(cx) {
                    cx.stop_propagation();
                    return;
                }
                if self.editor.cancel_gesture() {
                    self.manual_preview_cursor_claimed = false;
                    self.new_crop_cursor_origin = None;
                    self.crop_adjust_cursor_origin = None;
                    self.crop_adjust_cursor_owner = None;
                    self.manual_preview_cursor_keys.cancel_gesture();
                    self.release_capture();
                    cx.notify();
                    return;
                }
                if self.exit_group_scope_input(cx) {
                    cx.stop_propagation();
                    return;
                }
                if self.image_translation.cancel_whole_image() {
                    self.status = tr!("已取消图片翻译", "Image translation canceled").into();
                    cx.notify();
                    return;
                }
                Some(Action::Cancel)
            }
            "backspace" if plain && self.editor.remove_polyline_point() => {
                cx.notify();
                return;
            }
            "enter" if plain && self.editor.has_pending_drawing() => {
                self.finish_content_or_status();
                cx.notify();
                return;
            }
            "enter"
                if self.editor.crop.is_none()
                    && self.image_canvas.is_none()
                    && !modifiers.modified()
                    && !self.preview_enter_replay
                    && crate::preview_middle_pin::physical_size_matches(
                        self.source.full(),
                        (
                            f32::from(window.viewport_size().width),
                            f32::from(window.viewport_size().height),
                        ),
                        window.scale_factor(),
                    ) =>
            {
                let h = hwnd(window);
                let serial = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetPropW(
                        h,
                        windows::core::w!("RustShot.GpuiPresentSerial"),
                    )
                }
                .0 as usize;
                let size = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetPropW(
                        h,
                        windows::core::w!("RustShot.GpuiPresentSize"),
                    )
                }
                .0 as usize as u32;
                if let Some(rect) = self
                    .painted_preview
                    .get()
                    .and_then(|painted| painted.presented(self.source.full(), serial, size))
                    .filter(|rect| self.screenshot_preview_confirm_ready(*rect))
                {
                    match self.output_rect_for(rect) {
                        Ok(_output) => self.confirm_range(rect, window, cx),
                        Err(error) => {
                            self.status = error.to_string();
                            cx.notify();
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                if self.manual_preview.is_some() {
                    // 新范围尚无 successful Present 时不能退回整屏复制。
                    cx.stop_propagation();
                    return;
                }
                Some(Action::Copy)
            }
            "enter" if plain => Some(Action::Confirm),
            "d" if control && !modifiers.shift => Some(Action::Duplicate),
            "]" if crate::host_shortcuts::alt(modifiers) => Some(Action::Rotate(15.)),
            "[" if crate::host_shortcuts::alt(modifiers) => Some(Action::Rotate(-15.)),
            "]" if control && !modifiers.shift => Some(Action::Front),
            "[" if control && !modifiers.shift => Some(Action::Back),
            "a" if control
                && !modifiers.shift
                && self.editor.tool == Tool::Edit
                && (self.editor.crop.is_some() || self.pin_mode || self.image_canvas.is_some()) =>
            {
                self.wheel_input.reset();
                self.editor.select_all_objects();
                cx.notify();
                return;
            }
            "c" if plain && self.editor.crop.is_none() => {
                if self.image_canvas.is_none() && self.magnifier && !self.capture_color_replay {
                    self.copy_capture_color(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if let Ok(color) =
                    crate::selection_assist::sample(&self.source_pixels(), self.pointer)
                {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(format!("#{color:06X}")));
                    self.status = trf!("已复制颜色 #{color:06X}", "Color copied #{color:06X}");
                    cx.notify();
                }
                return;
            }
            "c" if control && !modifiers.shift => Some(Action::Copy),
            "s" if control && modifiers.shift => Some(Action::QuickSave),
            "s" if control => Some(Action::Save),
            "a" if control => {
                if !self.pin_mode && self.image_canvas.is_none() {
                    // 普通截图暂拒 Ctrl+A 时也不走旧 Current/All 提交范围。
                    let preview_handled = self.snap_screen_preview(event, window, cx);
                    if !preview_handled
                        && drawing_select_all_feedback(
                            &self.editor,
                            event,
                            self.text_core_ready(window, cx)
                                && self.pointer_down_start.is_none()
                                && self.input.drag_anchor.is_none()
                                && self.flyout.open().is_none()
                                && !self.preview_cursor_replay
                                && !self.capture_color_replay,
                        )
                    {
                        self.status = tr!(
                            "绘制模式：V 切换编辑 · Ctrl+A 全选标注",
                            "Drawing: V for Edit · Ctrl+A selects objects"
                        )
                        .into();
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                let current = self.current_capture_rect();
                Some(if modifiers.shift || self.editor.crop == Some(current) {
                    Action::All
                } else {
                    Action::Current
                })
            }
            "a" if plain && (self.pin_mode || self.image_canvas.is_some()) => Some(Action::Current),
            "z" if control => Some(if modifiers.shift {
                Action::Redo
            } else {
                Action::Undo
            }),
            "y" if control && !modifiers.shift => Some(Action::Redo),
            "delete" if crate::host_shortcuts::shift(modifiers) => Some(Action::Clear),
            "delete" if plain => Some(Action::Delete),
            // 0 与主栏鼠标按钮相同；1–9 按主栏绘制槽位，再按轮换组内工具或回到鼠标。
            "0" if !modifiers.modified() => Some(Action::Tool(
                crate::selection_action_layout::mouse_mode_tool(self.editor.tool),
            )),
            digit if !modifiers.modified() && toolbar_groups::shortcut_digit(digit).is_some() => {
                let slots = self.toolbar_config.slots();
                let smart_mosaic = self.smart_mosaic_available();
                toolbar_groups::shortcut_digit(digit)
                    .and_then(|digit| slots.get(usize::from(digit) - 1).copied())
                    .map(|slot| {
                        Action::Tool(toolbar_groups::cycle(
                            slot,
                            self.editor.tool,
                            &mut self.tool_groups,
                            smart_mosaic,
                            Tool::Edit,
                        ))
                    })
            }
            "f2" if plain => Some(Action::EditText),
            "`" | "backtick" if plain => Some(Action::Cursor),
            "t" if modifiers.shift
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.platform =>
            {
                Some(Action::WindowPin)
            }
            // 截图有选区时 T＝识别并翻译（已有结果只切到“译文”页），贴图用中键；
            // 框选前、图片编辑与贴图宿主 T 仍是贴图。
            "f1" if plain => Some(Action::Help),
            "t" if plain && self.t_translates() => {
                if event.is_held {
                    cx.stop_propagation();
                    return;
                }
                Some(Action::Recognize(
                    crate::toolbar_groups::RecognizeMode::TextTranslate,
                ))
            }
            "t" if plain => Some(Action::Pin),
            "r" if event.is_held
                && self.image_canvas.is_none()
                && self.editor.crop.is_some()
                && !modifiers.modified()
                && !self.editor.is_dragging()
                && !self.editor.has_pending_drawing() =>
            {
                cx.stop_propagation();
                return;
            }
            "r" if plain => Some(Action::Ocr),
            "y" if plain => Some(Action::TranslateImage),
            "g" if control => Some(if modifiers.shift {
                Action::Ungroup
            } else {
                Action::Group
            }),
            "g" if plain => Some(Action::Record),
            "l" if plain => Some(Action::LongCapture),
            _ => None,
        };
        if let Some(action) = action {
            self.perform(action, window, cx);
        } else if !modifiers.control && !modifiers.platform && !modifiers.function {
            let step = if modifiers.shift { 10. } else { 1. };
            let delta = match key {
                "left" => Some((-step, 0.)),
                "right" => Some((step, 0.)),
                "up" => Some((0., -step)),
                "down" => Some((0., step)),
                _ => None,
            };
            if let Some((dx, dy)) = delta {
                if edit_arrow::move_empty_edit_crop(
                    &mut self.editor,
                    (dx as i32, dy as i32),
                    modifiers,
                    !self.pin_mode && self.image_canvas.is_none(),
                    self.saving
                        || self.processing
                        || self.pin_handoff_pending
                        || self.pointer_down_start.is_some()
                        || self.toolbar_drag.is_some()
                        || self.recognition_result.is_some(),
                    self.watermark_input_focused
                        || self.input.marked.is_some()
                        || (self.toolbar_panel != ToolbarPanel::None && !self.toolbar_panel.more()),
                ) {
                    self.wheel_input.reset();
                } else if self.editor.tool == Tool::Crop {
                    if let Some(mut r) = self.editor.crop {
                        if modifiers.alt {
                            r.width = (r.width as i32 + dx as i32)
                                .clamp(1, (self.source.bounds.width - r.x) as i32)
                                as u32;
                            r.height = (r.height as i32 + dy as i32)
                                .clamp(1, (self.source.bounds.height - r.y) as i32)
                                as u32;
                        } else {
                            r.x = (r.x as i32 + dx as i32)
                                .clamp(0, (self.source.bounds.width - r.width) as i32)
                                as u32;
                            r.y = (r.y as i32 + dy as i32)
                                .clamp(0, (self.source.bounds.height - r.height) as i32)
                                as u32;
                        }
                        self.wheel_input.reset();
                        self.editor.set_crop(Some(r));
                    }
                } else {
                    self.editor.nudge(dx, dy);
                }
                cx.notify();
            }
        }
    }
    /// 结束点所在显示器；选区在那块屏只占一条窄边时改用重叠最多的显示器。
    fn crop_anchor_monitor(&self, crop: Option<Rect>, point: (i32, i32)) -> usize {
        let end = self.source.monitor_at(point);
        let Some(crop) = crop else {
            return end;
        };
        let screens: Vec<Rect> = (0..self.source.screen_count())
            .map(|index| self.source.screen_rect(index))
            .collect();
        crate::toolbar_layout::anchor_monitor(crop, end, &screens)
    }
    pub fn toolbar_layout(&self, selected: Rect, scale: f32) -> ToolbarLayout {
        let monitor = self
            .toolbar_manual_monitor
            .filter(|_| self.toolbar_position.is_some())
            .unwrap_or(self.toolbar_monitor);
        self.toolbar_layout_at(selected, scale, self.toolbar_position, monitor)
    }
    pub(crate) fn toolbar_layout_at(
        &self,
        selected: Rect,
        scale: f32,
        position: Option<(f32, f32)>,
        monitor_index: usize,
    ) -> ToolbarLayout {
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        let count = self.editor.selected_ids().len();
        let monitor = self.ui_monitor(monitor_index);
        let (selected, position) = if self.image_canvas.is_some() {
            (
                monitor,
                position.or(Some((
                    (monitor.width as f32 / scale
                        - ToolbarLayout::primary_width_with_tools(&self.toolbar_config.slots()))
                        / 2.,
                    monitor.height as f32 / scale - 160.,
                ))),
            )
        } else {
            (selected, position)
        };
        let constructor = if self.image_canvas.is_none() && !self.pin_mode {
            ToolbarLayout::new_capture_with_tools
        } else {
            ToolbarLayout::new_with_tools
        };
        let layout = constructor(
            monitor,
            selected,
            scale,
            position,
            if count > 1 { Tool::Rectangle } else { tool },
            &self.toolbar_config.slots(),
        );
        if self.editor.text_draft.is_none() {
            layout.with_transform_context(count)
        } else {
            layout
        }
    }
}

impl Drop for CaptureView {
    fn drop(&mut self) {
        if let Some(cancel) = self.refresh_request.take() {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        // 关闭被拒时 View 尚存，不走这里。只向本 View 所有任务发出取消请求；
        // HTTP、模型加载与 UIA 查询只能在其边界检查，不表示此刻已退出。
        self.cancel_capture_ocr_tasks("CaptureView 释放兜底");
        self.flush_tool_presets();
    }
}

#[cfg(test)]
mod ordinary_activation_cleanup_tests {
    use super::cleanup_ordinary_activation;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        crop_mask::Polygon,
        editor::Editor,
        geometry::Rect,
        interaction::WheelAccumulator,
    };
    use std::time::Instant;

    fn editor() -> Editor {
        let canvas = Rect::full(400, 300);
        Editor::new(canvas, Some(canvas))
    }
    fn rectangle(x: f32, y: f32) -> Annotation {
        let mut annotation = Annotation::new(Tool::Rectangle, Point { x, y }, Style::default());
        annotation.update(Point {
            x: x + 40.,
            y: y + 30.,
        });
        annotation
    }
    fn polygon() -> Polygon {
        Polygon::new(&[
            Point { x: 100., y: 80. },
            Point { x: 220., y: 80. },
            Point { x: 220., y: 180. },
            Point { x: 100., y: 180. },
        ])
        .unwrap()
    }
    fn deactivate(editor: &mut Editor, wheel: &mut WheelAccumulator, accepted_down: bool) {
        assert!(cleanup_ordinary_activation(
            editor,
            wheel,
            true,
            true,
            true,
            false,
            accepted_down,
        ));
    }

    #[test]
    fn inactive_crop_restores_mask_history_and_preserves_existing_finished_lasso() {
        let mut editor = editor();
        editor.crop = Some(Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        });
        editor.select_tool(Tool::Edit);
        let id = editor.document.insert(rectangle(140., 100.));
        editor.selected = Some(id);
        editor.selection = vec![id];
        editor.document.set_crop_mask(Some(polygon()));
        let crop = editor.crop;
        let mask = editor.document.crop_mask.clone();
        let annotations = editor.document_annotations();
        let revision = (editor.document.revision(), editor.document.edit_revision());
        assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
        editor.pointer_move((20, 130));
        // 保守组合边界：不依赖「完成套索与连续手势绝不共存」的可达性假设。
        editor.lasso_draft = Some(polygon());
        let finished_lasso = editor.lasso_draft.clone();
        deactivate(&mut editor, &mut WheelAccumulator::default(), true);
        assert!(!editor.is_dragging());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, mask);
        assert_eq!(editor.lasso_draft, finished_lasso);
        assert_eq!(editor.selected_ids(), vec![id]);
        assert_eq!(editor.tool, Tool::Edit);
        assert_eq!(editor.document_annotations(), annotations);
        assert_eq!(
            (editor.document.revision(), editor.document.edit_revision()),
            revision
        );
        editor.pointer_up((20, 130));
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document_annotations(), annotations);
        editor.document.undo();
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document_annotations(), annotations);
        editor.document.undo();
        assert!(editor.document.items().is_empty());
        editor.document.redo();
        assert_eq!(editor.document_annotations(), annotations);
    }

    #[test]
    fn inactive_draw_and_object_previews_do_not_commit_or_destroy_redo() {
        for tool in [Tool::Pen, Tool::Edit] {
            let mut editor = editor();
            let id = editor.document.insert(rectangle(100., 100.));
            editor.document.insert(rectangle(250., 200.));
            editor.document.undo();
            editor.select_tool(tool);
            if tool == Tool::Edit {
                editor.selected = Some(id);
                editor.selection = vec![id];
            }
            let annotations = editor.document_annotations();
            let revision = (editor.document.revision(), editor.document.edit_revision());
            editor.pointer_down((120, 115), 4.);
            editor.pointer_move((150, 140));
            assert!(editor.is_dragging());
            deactivate(&mut editor, &mut WheelAccumulator::default(), true);
            assert!(!editor.is_dragging());
            assert_eq!(editor.document_annotations(), annotations);
            assert_eq!(
                (editor.document.revision(), editor.document.edit_revision()),
                revision
            );
            assert!(editor.document.can_redo());
            editor.pointer_up((170, 150));
            assert_eq!(editor.document_annotations(), annotations);
            assert!(editor.document.can_redo());
        }
    }

    #[test]
    fn inactive_cleanup_preserves_pending_polyline_arrow_and_no_down_fill() {
        for tool in [Tool::Polyline, Tool::Arrow] {
            let mut editor = editor();
            editor.select_tool(tool);
            editor.pointer_down((120, 100), 4.);
            editor.pointer_up((120, 100));
            assert!(!editor.is_dragging());
            assert!(editor.has_pending_drawing());
            let stamp = editor.scene_stamp();
            let previews = editor.document_annotations();
            deactivate(&mut editor, &mut WheelAccumulator::default(), true);
            assert!(editor.has_pending_drawing());
            assert_eq!(editor.scene_stamp(), stamp);
            assert_eq!(editor.document_annotations(), previews);
            assert!(!editor.document.can_undo());
        }
        let mut editor = editor();
        editor.document.insert(rectangle(10., 10.));
        editor.document.undo();
        editor.select_tool(Tool::MosaicBrush);
        editor.select_tool(Tool::LocalFill);
        editor.pointer_down((60, 60), 4.);
        editor.pointer_move((80, 60));
        editor.pointer_up((90, 60));
        let candidate = editor.local_fill_candidate().unwrap();
        let stamp = editor.scene_stamp();
        deactivate(&mut editor, &mut WheelAccumulator::default(), false);
        assert_eq!(editor.local_fill_candidate(), Some(candidate));
        assert_eq!(editor.scene_stamp(), stamp);
        assert!(editor.document.items().is_empty());
        assert!(editor.document.can_redo());
    }

    #[test]
    fn inactive_idle_cleanup_preserves_text_and_finished_lasso() {
        let mut text = editor();
        text.select_tool(Tool::Text);
        text.begin_text(None, Point { x: 120., y: 100. });
        text.set_text("尚未提交".into());
        let annotation = text.text_draft.as_ref().unwrap().annotation.clone();
        let stamp = text.scene_stamp();
        deactivate(&mut text, &mut WheelAccumulator::default(), true);
        assert_eq!(text.text_draft.as_ref().unwrap().annotation, annotation);
        assert_eq!(text.scene_stamp(), stamp);
        assert!(!text.document.can_undo());

        let mut lasso = editor();
        lasso.select_tool(Tool::Lasso);
        lasso.pointer_down((100, 80), 2.);
        lasso.pointer_move((220, 80));
        lasso.pointer_move((220, 180));
        lasso.pointer_up((100, 180));
        assert!(!lasso.is_dragging());
        let draft = lasso.lasso_draft.clone();
        assert!(draft.is_some());
        let stamp = lasso.scene_stamp();
        deactivate(&mut lasso, &mut WheelAccumulator::default(), true);
        assert_eq!(lasso.lasso_draft, draft);
        assert_eq!(lasso.scene_stamp(), stamp);
        assert!(lasso.document.crop_mask.is_none());
        assert!(!lasso.document.can_undo());
    }

    #[test]
    fn cleanup_skips_other_hosts_unready_handoff_and_renewed_foreground() {
        for (ordinary, ready, inactive, foreground_owned) in [
            (false, true, true, false),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, true),
        ] {
            let mut editor = editor();
            editor.select_tool(Tool::Pen);
            editor.pointer_down((120, 100), 4.);
            editor.pointer_move((150, 120));
            let previews = editor.document_annotations();
            let stamp = editor.scene_stamp();
            let mut wheel = WheelAccumulator::default();
            let now = Instant::now();
            assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
            assert!(!cleanup_ordinary_activation(
                &mut editor,
                &mut wheel,
                ordinary,
                ready,
                inactive,
                foreground_owned,
                true,
            ));
            assert!(editor.is_dragging());
            assert_eq!(editor.document_annotations(), previews);
            assert_eq!(editor.scene_stamp(), stamp);
            assert_eq!(wheel.push_selection_center(0.5, now).0, 1);
        }
    }

    #[test]
    fn idle_cleanup_resets_wheel_and_late_up_preserves_committed_content() {
        let mut editor = editor();
        editor.select_tool(Tool::Pen);
        editor.pointer_down((120, 100), 4.);
        editor.pointer_up((160, 130));
        assert!(!editor.is_dragging());
        let annotations = editor.document_annotations();
        assert_eq!(annotations.len(), 1);
        let revision = (editor.document.revision(), editor.document.edit_revision());
        let mut wheel = WheelAccumulator::default();
        let now = Instant::now();
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        deactivate(&mut editor, &mut wheel, false);
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        editor.pointer_up((180, 160));
        assert_eq!(editor.document_annotations(), annotations);
        assert_eq!(
            (editor.document.revision(), editor.document.edit_revision()),
            revision
        );
        assert!(editor.document.can_undo());
        editor.document.undo();
        assert!(editor.document.items().is_empty());
    }
}

#[cfg(test)]
mod table_copy_publication_tests {
    use super::text_result::{publish_text, CopyScope, ResultEditor};
    use crate::pin_view::table_copy::{tests::recognized, TableCopy};

    #[test]
    fn capture_full_table_publication_uses_current_revision_and_full_payload() {
        let result = recognized();
        let mut editor = ResultEditor::new(result.text.clone(), 0..0);
        let key = (1, editor.revision);
        let mut cache = TableCopy::new(&result, key, 3);
        let payload = cache.text(&key, 3, &result.text).unwrap();
        let calls = std::cell::Cell::new(0);
        let first = publish_text(payload, 0..payload.len(), CopyScope::Full, |text| {
            calls.set(calls.get() + 1);
            assert_eq!(text.as_bytes(), payload.as_bytes());
            Err::<(), &str>("失败")
        });
        assert_eq!(first, Err("失败"));
        assert_eq!(calls.get(), 1);
        let retry = publish_text(payload, 0..payload.len(), CopyScope::Full, |text| {
            calls.set(calls.get() + 1);
            assert_eq!(text.as_bytes(), payload.as_bytes());
            Ok::<(), &str>(())
        });
        assert_eq!(retry, Ok(true));
        assert_eq!(calls.get(), 2);
        assert_eq!(cache.text(&key, 3, &result.text), Some(payload));
        assert!(cache.advance_generation(&key, 3, 4, &editor.text));
        assert!(cache.text(&key, 4, &editor.text).is_some());
        editor.select_all();
        assert!(editor.replace(None, "改过全文", false, None));
        editor.undo();
        assert_eq!(editor.text.as_bytes(), result.text.as_bytes());
        assert_ne!(editor.revision, key.1);
        let changed_key = (1, editor.revision);
        assert!(cache.text(&changed_key, 4, &editor.text).is_none());
        assert!(!cache.advance_generation(&changed_key, 4, 5, &editor.text));
    }
}

#[cfg(test)]
mod shortcut_feedback_tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn select_all(m: Modifiers, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: "a".into(),
                key_char: None,
                modifiers: m,
            },
            is_held: held,
        }
    }

    fn control() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    #[test]
    fn drawing_feedback_preserves_content_and_edit_guidance_selects_real_objects() {
        let crop = Rect {
            x: 80,
            y: 60,
            width: 160,
            height: 120,
        };
        let mut editor = Editor::new(Rect::full(400, 300), Some(crop));
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((100, 90), 3.);
        editor.pointer_up((180, 140));
        editor.select_tool(Tool::Pen);
        let objects = editor.document.items().to_vec();
        assert_eq!(objects.len(), 1);
        let selection = editor.selected_ids();
        let stamp = editor.scene_stamp();
        assert!(drawing_select_all_feedback(
            &editor,
            &select_all(control(), false),
            true
        ));
        assert_eq!(editor.crop, Some(crop));
        assert_eq!(editor.tool, Tool::Pen);
        assert_eq!(editor.document.items(), objects);
        assert_eq!(editor.selected_ids(), selection);
        assert_eq!(editor.scene_stamp(), stamp);

        // 文案给出的 V → Ctrl+A 使用当前真实 Editor 接口，不假造宿主。
        editor.select_tool(Tool::Edit);
        editor.select_all_objects();
        assert_eq!(editor.selected_ids(), vec![objects[0].id]);
        assert_eq!(editor.document.items(), objects);
        assert_eq!(editor.crop, Some(crop));
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), objects);
    }

    #[test]
    fn feedback_rejects_other_all_routes_repeat_and_busy_drawing() {
        let mut editor = Editor::new(Rect::full(400, 300), Some(Rect::full(400, 300)));
        let event = select_all(control(), false);
        for tool in [
            Tool::Crop,
            Tool::Edit,
            Tool::SmartMosaic,
            Tool::Eraser,
            Tool::Lasso,
        ] {
            editor.select_tool(tool);
            assert!(!drawing_select_all_feedback(&editor, &event, true));
        }
        editor.select_tool(Tool::Pen);
        assert!(!drawing_select_all_feedback(&editor, &event, false));
        assert!(!drawing_select_all_feedback(
            &editor,
            &select_all(control(), true),
            true
        ));
        for m in [
            Modifiers {
                shift: true,
                ..control()
            },
            Modifiers {
                alt: true,
                ..control()
            },
            Modifiers {
                platform: true,
                ..control()
            },
            Modifiers {
                function: true,
                ..control()
            },
            Modifiers::default(),
        ] {
            assert!(!drawing_select_all_feedback(
                &editor,
                &select_all(m, false),
                true
            ));
        }
        editor.crop = None;
        assert!(!drawing_select_all_feedback(&editor, &event, true));
        editor.crop = Some(Rect::full(400, 300));
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((100, 100), 3.);
        editor.pointer_up((100, 100));
        assert!(editor.has_pending_drawing());
        assert!(!drawing_select_all_feedback(&editor, &event, true));
    }
}
