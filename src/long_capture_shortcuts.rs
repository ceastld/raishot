//! 长截图操作条的键盘路由；实际输出仍由宿主现有 Control::Output 完成。
//! output_ready / pause_ready 由宿主根据鼠标按钮与控制提交合同计算。

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
    pub function: bool,
}

impl Modifiers {
    fn only_control(self) -> bool {
        self.control && !self.alt && !self.shift && !self.platform && !self.function
    }

    fn plain(self) -> bool {
        !self.control && !self.alt && !self.shift && !self.platform && !self.function
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Copy,
    Save,
    Finish,
    Pause,
    CancelToolbar,
    CancelRegion,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Pass,
    Consume,
    Action(Action),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Context {
    pub owns_keyboard: bool,
    pub ime_composing: bool,
    pub field_focused: bool,
    pub closing: bool,
    pub editor_open: bool,
    pub pending_editor: bool,
    pub toolbar_dragging: bool,
    pub region_adjusting: bool,
    pub output_ready: bool,
    pub pause_ready: bool,
}

pub(super) fn route(key: &str, modifiers: Modifiers, is_held: bool, context: Context) -> Route {
    // 字段及 IME 拥有全部按键，包括重复键和区域调整期间的 Esc。
    if !context.owns_keyboard || context.ime_composing || context.field_focused {
        return Route::Pass;
    }
    let action = match key {
        "c" if modifiers.only_control() => Action::Copy,
        "s" if modifiers.only_control() => Action::Save,
        "enter" if modifiers.plain() => Action::Finish,
        "space" if modifiers.plain() => Action::Pause,
        "escape" if modifiers.plain() => Action::Close,
        _ => return Route::Pass,
    };
    if is_held {
        return Route::Consume;
    }
    if context.closing || context.editor_open || context.pending_editor {
        return Route::Consume;
    }
    if context.toolbar_dragging {
        return if action == Action::Close {
            Route::Action(Action::CancelToolbar)
        } else {
            Route::Consume
        };
    }
    if context.region_adjusting {
        return if action == Action::Close {
            Route::Action(Action::CancelRegion)
        } else {
            Route::Consume
        };
    }
    match action {
        Action::Copy | Action::Save | Action::Finish if !context.output_ready => Route::Consume,
        Action::Pause if !context.pause_ready => Route::Consume,
        _ => Route::Action(action),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Modifiers = Modifiers {
        control: false,
        alt: false,
        shift: false,
        platform: false,
        function: false,
    };
    const CONTROL: Modifiers = Modifiers {
        control: true,
        ..PLAIN
    };
    const KEYS: [(&str, Modifiers, Action); 5] = [
        ("c", CONTROL, Action::Copy),
        ("s", CONTROL, Action::Save),
        ("enter", PLAIN, Action::Finish),
        ("space", PLAIN, Action::Pause),
        ("escape", PLAIN, Action::Close),
    ];

    fn ready() -> Context {
        Context {
            owns_keyboard: true,
            output_ready: true,
            pause_ready: true,
            ..Context::default()
        }
    }

    fn modifiers(mask: u8) -> Modifiers {
        Modifiers {
            control: mask & 1 != 0,
            alt: mask & 2 != 0,
            shift: mask & 4 != 0,
            platform: mask & 8 != 0,
            function: mask & 16 != 0,
        }
    }

    #[test]
    fn long_capture_shortcuts_control_outputs_match_exact_modifier_contract() {
        for (key, expected) in [("c", Action::Copy), ("s", Action::Save)] {
            for mask in 0..32 {
                let expected = if mask == 1 {
                    Route::Action(expected)
                } else {
                    Route::Pass
                };
                assert_eq!(route(key, modifiers(mask), false, ready()), expected);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_terminal_keys_require_no_modifier() {
        for (key, expected) in [
            ("enter", Action::Finish),
            ("space", Action::Pause),
            ("escape", Action::Close),
        ] {
            for mask in 0..32 {
                let expected = if mask == 0 {
                    Route::Action(expected)
                } else {
                    Route::Pass
                };
                assert_eq!(route(key, modifiers(mask), false, ready()), expected);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_each_additional_control_modifier_is_rejected() {
        for extra in [2, 4, 8, 16] {
            for key in ["c", "s"] {
                for held in [false, true] {
                    assert_eq!(route(key, modifiers(1 | extra), held, ready()), Route::Pass);
                }
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_unknown_or_non_control_character_passes() {
        for key in ["c", "s", "a", "C", "S", "f9", "tab", ""] {
            for held in [false, true] {
                assert_eq!(route(key, PLAIN, held, ready()), Route::Pass);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_held_events_consume_without_any_action() {
        for (key, mods, action) in KEYS {
            assert_eq!(route(key, mods, false, ready()), Route::Action(action));
            for _ in 0..5 {
                assert_eq!(route(key, mods, true, ready()), Route::Consume);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_no_keyboard_owner_passes_even_held_terminal_keys() {
        let context = Context {
            owns_keyboard: false,
            closing: true,
            region_adjusting: true,
            ..ready()
        };
        for (key, mods, _) in KEYS {
            for held in [false, true] {
                assert_eq!(route(key, mods, held, context), Route::Pass);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_ime_owns_keys_before_held_and_region_escape() {
        let context = Context {
            ime_composing: true,
            region_adjusting: true,
            ..ready()
        };
        for (key, mods, _) in KEYS {
            for held in [false, true] {
                assert_eq!(route(key, mods, held, context), Route::Pass);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_fixed_field_owns_copy_save_and_terminal_keys() {
        let context = Context {
            field_focused: true,
            toolbar_dragging: true,
            region_adjusting: true,
            ..ready()
        };
        for (key, mods, _) in KEYS {
            for held in [false, true] {
                assert_eq!(route(key, mods, held, context), Route::Pass);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_all_busy_owner_combinations_block_recognized_actions() {
        for mask in 1..8 {
            let context = Context {
                closing: mask & 1 != 0,
                editor_open: mask & 2 != 0,
                pending_editor: mask & 4 != 0,
                toolbar_dragging: true,
                region_adjusting: true,
                ..ready()
            };
            for (key, mods, _) in KEYS {
                for held in [false, true] {
                    assert_eq!(route(key, mods, held, context), Route::Consume);
                }
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_unmatched_keys_pass_during_pending_editor() {
        let context = Context {
            pending_editor: true,
            ..ready()
        };
        for key in ["a", "tab", "f9", ""] {
            for held in [false, true] {
                assert_eq!(route(key, CONTROL, held, context), Route::Pass);
            }
        }
        assert_eq!(route("escape", CONTROL, false, context), Route::Pass);
    }

    #[test]
    fn long_capture_shortcuts_toolbar_escape_cancels_before_region_or_close_once() {
        let context = Context {
            toolbar_dragging: true,
            region_adjusting: true,
            output_ready: false,
            pause_ready: false,
            ..ready()
        };
        let mut requests = Vec::new();
        for held in [false, true, true, true] {
            if let Route::Action(action) = route("escape", PLAIN, held, context) {
                requests.push(dispatch_intent(action));
            }
        }
        assert_eq!(requests, [ExistingRequest::CancelToolbar]);
        for (key, mods, action) in KEYS {
            if action != Action::Close {
                assert_eq!(route(key, mods, false, context), Route::Consume);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_toolbar_cancel_respects_held_ime_field_and_modifiers() {
        let context = Context {
            toolbar_dragging: true,
            ..ready()
        };
        assert_eq!(route("escape", PLAIN, true, context), Route::Consume);
        for mask in 1..32 {
            for held in [false, true] {
                assert_eq!(route("escape", modifiers(mask), held, context), Route::Pass);
            }
        }
        for blocked in 0..3 {
            let context = Context {
                owns_keyboard: blocked != 0,
                ime_composing: blocked == 1,
                field_focused: blocked == 2,
                ..context
            };
            for held in [false, true] {
                assert_eq!(route("escape", PLAIN, held, context), Route::Pass);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_region_owner_blocks_outputs_and_pause() {
        let context = Context {
            region_adjusting: true,
            ..ready()
        };
        for (key, mods, action) in KEYS {
            if action != Action::Close {
                assert_eq!(route(key, mods, false, context), Route::Consume);
            }
        }
    }

    #[test]
    fn long_capture_shortcuts_plain_escape_cancels_region_before_close() {
        let context = Context {
            region_adjusting: true,
            output_ready: false,
            pause_ready: false,
            ..ready()
        };
        assert_eq!(
            route("escape", PLAIN, false, context),
            Route::Action(Action::CancelRegion)
        );
        assert_eq!(route("escape", PLAIN, true, context), Route::Consume);
        for mask in 1..32 {
            assert_eq!(
                route("escape", modifiers(mask), false, context),
                Route::Pass
            );
        }
    }

    #[test]
    fn long_capture_shortcuts_output_unready_consumes_without_disabling_pause_or_close() {
        let context = Context {
            output_ready: false,
            ..ready()
        };
        for (key, mods, _) in KEYS[..3].iter().copied() {
            assert_eq!(route(key, mods, false, context), Route::Consume);
        }
        assert_eq!(
            route("space", PLAIN, false, context),
            Route::Action(Action::Pause)
        );
        assert_eq!(
            route("escape", PLAIN, false, context),
            Route::Action(Action::Close)
        );
    }

    #[test]
    fn long_capture_shortcuts_pause_unready_does_not_block_existing_output_paths() {
        let context = Context {
            pause_ready: false,
            ..ready()
        };
        assert_eq!(route("space", PLAIN, false, context), Route::Consume);
        for (key, mods, action) in KEYS[..3].iter().copied() {
            assert_eq!(route(key, mods, false, context), Route::Action(action));
        }
    }

    #[test]
    fn long_capture_shortcuts_capture_error_with_retained_ready_result_still_allows_output() {
        // ready 是宿主对失败后保留结果的判断，路由不按采集 phase 另行禁用。
        for (key, mods, action) in KEYS[..3].iter().copied() {
            assert_eq!(route(key, mods, false, ready()), Route::Action(action));
        }
    }

    #[test]
    fn long_capture_shortcuts_field_and_ime_owners_do_not_turn_into_repeat_suppression() {
        for (field_focused, ime_composing) in [(true, false), (false, true), (true, true)] {
            let context = Context {
                field_focused,
                ime_composing,
                closing: true,
                editor_open: true,
                pending_editor: true,
                toolbar_dragging: true,
                region_adjusting: true,
                output_ready: false,
                pause_ready: false,
                ..ready()
            };
            for (key, mods, _) in KEYS {
                assert_eq!(route(key, mods, true, context), Route::Pass);
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExistingRequest {
        Edit(Option<ExistingOutput>),
        Pause,
        CancelToolbar,
        CancelRegion,
        Close,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExistingOutput {
        Copy,
        Save,
    }

    // 仅记录宿主的既有请求意图，不执行导出、文件或剪贴板操作。
    fn dispatch_intent(action: Action) -> ExistingRequest {
        match action {
            Action::Copy => ExistingRequest::Edit(Some(ExistingOutput::Copy)),
            Action::Save => ExistingRequest::Edit(Some(ExistingOutput::Save)),
            Action::Finish => ExistingRequest::Edit(Some(ExistingOutput::Copy)),
            Action::Pause => ExistingRequest::Pause,
            Action::CancelToolbar => ExistingRequest::CancelToolbar,
            Action::CancelRegion => ExistingRequest::CancelRegion,
            Action::Close => ExistingRequest::Close,
        }
    }

    #[test]
    fn long_capture_shortcuts_one_press_records_one_existing_edit_output_intent() {
        for (key, mods, expected) in [
            (
                "c",
                CONTROL,
                ExistingRequest::Edit(Some(ExistingOutput::Copy)),
            ),
            (
                "s",
                CONTROL,
                ExistingRequest::Edit(Some(ExistingOutput::Save)),
            ),
            (
                "enter",
                PLAIN,
                ExistingRequest::Edit(Some(ExistingOutput::Copy)),
            ),
        ] {
            let mut requests = Vec::new();
            let mut context = ready();
            for held in [false, true, true, true] {
                if let Route::Action(action) = route(key, mods, held, context) {
                    requests.push(dispatch_intent(action));
                    context.pending_editor = true;
                }
            }
            assert_eq!(requests, [expected]);
        }
    }

    #[test]
    fn long_capture_shortcuts_failed_output_can_retry_on_new_press_after_owner_restores() {
        let mut requests = Vec::new();
        let mut context = ready();
        if let Route::Action(action) = route("c", CONTROL, false, context) {
            requests.push(dispatch_intent(action));
            context.pending_editor = true;
        }
        assert_eq!(route("s", CONTROL, false, context), Route::Consume);
        // 宿主处理错误后保留采集结果，解除 pending 并恢复现有输出可用性。
        context.pending_editor = false;
        assert_eq!(route("c", CONTROL, true, context), Route::Consume);
        if let Route::Action(action) = route("c", CONTROL, false, context) {
            requests.push(dispatch_intent(action));
        }
        assert_eq!(
            requests,
            [
                ExistingRequest::Edit(Some(ExistingOutput::Copy)),
                ExistingRequest::Edit(Some(ExistingOutput::Copy)),
            ]
        );
    }

    #[test]
    fn long_capture_shortcuts_region_cancel_then_restore_never_replays_held_output() {
        let mut context = Context {
            region_adjusting: true,
            ..ready()
        };
        assert_eq!(route("c", CONTROL, false, context), Route::Consume);
        assert_eq!(
            route("escape", PLAIN, false, context),
            Route::Action(Action::CancelRegion)
        );
        // 恢复是宿主独立事务；释放区域所有权也不能把已按住的键变成新命令。
        context.region_adjusting = false;
        assert_eq!(route("c", CONTROL, true, context), Route::Consume);
        assert_eq!(route("escape", PLAIN, true, context), Route::Consume);
        assert_eq!(
            route("c", CONTROL, false, context),
            Route::Action(Action::Copy)
        );
    }

    #[test]
    fn long_capture_shortcuts_cancel_and_close_held_sequences_only_act_once() {
        for adjusting in [false, true] {
            let context = Context {
                region_adjusting: adjusting,
                ..ready()
            };
            let mut requests = Vec::new();
            for held in [false, true, true, true] {
                if let Route::Action(action) = route("escape", PLAIN, held, context) {
                    requests.push(dispatch_intent(action));
                }
            }
            assert_eq!(
                requests,
                [if adjusting {
                    ExistingRequest::CancelRegion
                } else {
                    ExistingRequest::Close
                }]
            );
        }
    }
}
