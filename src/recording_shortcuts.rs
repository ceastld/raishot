//! 录屏键盘入口的纯判定。调用方提供当次事件的输入所有权与工作状态。
//! 此模块不读焦点、不触碰播放器/剪贴板，也不启动原生保存窗口。

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
    pub function: bool,
}

impl Modifiers {
    fn plain(self) -> bool {
        self == Self::default()
    }

    fn only_control(self) -> bool {
        self == Self {
            control: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Idle,
    Countdown,
    Recording,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Context {
    pub stage: Stage,
    /// 仅当录屏工具栏拥有键盘，且当前输入目标不是文本/参数输入时为 true。
    pub keyboard_owned: bool,
    pub ime_composing: bool,
    pub native_dialog: bool,
    pub flyout_open: bool,
    /// 录制中标注工具栏展开；Esc 先收起标注，不停止录制。
    pub drawing: bool,
    /// 预览存在，且没有 GIF 转换、混音、关闭等输出操作占用。
    pub output_ready: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    ToggleRecord,
    TogglePause,
    TogglePlayback,
    DismissFlyout,
    ExitDrawing,
    CopyFile,
    SaveAs,
    Done,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// 此事件由其它输入目标或快捷键入口处理。
    Pass,
    /// 是本入口的快捷键，但本次不能执行；阻止重复事件或忙碌输出落入其它入口。
    Consume,
    /// 执行一次既有实际操作，调用方同时消费当前按键事件。
    Run(Action),
}

pub(super) fn route(key: &str, modifiers: Modifiers, held: bool, context: Context) -> Route {
    if !context.keyboard_owned || context.ime_composing || context.native_dialog {
        return Route::Pass;
    }

    let action = if modifiers.plain() {
        match key {
            "f9" if context.stage != Stage::Preview => Action::ToggleRecord,
            "f10" if context.stage == Stage::Recording => Action::TogglePause,
            "space" if context.stage == Stage::Preview => Action::TogglePlayback,
            "space" if context.stage == Stage::Recording => Action::TogglePause,
            "escape" if context.flyout_open => Action::DismissFlyout,
            "escape" if context.drawing && context.stage == Stage::Recording => Action::ExitDrawing,
            "escape" if matches!(context.stage, Stage::Countdown | Stage::Recording) => {
                Action::ToggleRecord
            }
            "escape" => Action::Close,
            "enter" if context.stage == Stage::Preview => Action::Done,
            _ => return Route::Pass,
        }
    } else if modifiers.only_control() && context.stage == Stage::Preview {
        match key {
            "c" => Action::CopyFile,
            "s" => Action::SaveAs,
            _ => return Route::Pass,
        }
    } else {
        return Route::Pass;
    };

    // 弹层优先接收参数输入；新输出入口不穿透弹层，也不排队等待忙碌任务结束。
    if matches!(action, Action::CopyFile | Action::SaveAs | Action::Done)
        && (context.flyout_open || !context.output_ready)
    {
        return Route::Consume;
    }
    if held {
        Route::Consume
    } else {
        Route::Run(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGES: [Stage; 4] = [
        Stage::Idle,
        Stage::Countdown,
        Stage::Recording,
        Stage::Preview,
    ];
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

    fn context(stage: Stage) -> Context {
        Context {
            stage,
            keyboard_owned: true,
            ime_composing: false,
            native_dialog: false,
            flyout_open: false,
            drawing: false,
            output_ready: true,
        }
    }

    #[test]
    fn recording_shortcuts_escape_closes_markup_before_stopping() {
        let mut ctx = context(Stage::Recording);
        ctx.drawing = true;
        ctx.flyout_open = true;
        assert_eq!(
            route("escape", PLAIN, false, ctx),
            Route::Run(Action::DismissFlyout)
        );
        ctx.flyout_open = false;
        assert_eq!(
            route("escape", PLAIN, false, ctx),
            Route::Run(Action::ExitDrawing)
        );
        ctx.drawing = false;
        assert_eq!(
            route("escape", PLAIN, false, ctx),
            Route::Run(Action::ToggleRecord)
        );
        // 只在录制中生效；F9 停止不受标注影响。
        let mut preview = context(Stage::Preview);
        preview.drawing = true;
        assert_eq!(
            route("escape", PLAIN, false, preview),
            Route::Run(Action::Close)
        );
        ctx.drawing = true;
        assert_eq!(
            route("f9", PLAIN, false, ctx),
            Route::Run(Action::ToggleRecord)
        );
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

    /// 独立的可观察操作记录，不从 route 的内部状态判断是否“成功”。
    #[derive(Default)]
    struct Trace {
        actions: Vec<Action>,
        consumed: usize,
        passed: usize,
    }

    impl Trace {
        fn event(&mut self, key: &str, modifiers: Modifiers, held: bool, context: Context) {
            match route(key, modifiers, held, context) {
                Route::Pass => self.passed += 1,
                Route::Consume => self.consumed += 1,
                Route::Run(action) => {
                    self.actions.push(action);
                    self.consumed += 1;
                }
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_output_contract_executes_existing_operations() {
        let mut trace = Trace::default();
        let preview = context(Stage::Preview);
        trace.event("c", CONTROL, false, preview);
        trace.event("s", CONTROL, false, preview);
        trace.event("enter", PLAIN, false, preview);
        assert_eq!(
            trace.actions,
            [Action::CopyFile, Action::SaveAs, Action::Done]
        );
        assert_eq!(trace.consumed, 3);
        assert_eq!(trace.passed, 0);
    }

    #[test]
    fn recording_preview_shortcuts_enter_finishes_and_never_records_again() {
        let mut trace = Trace::default();
        for held in [false, true, true] {
            trace.event("enter", PLAIN, held, context(Stage::Preview));
        }
        assert_eq!(trace.actions, [Action::Done]);
        assert!(!trace.actions.contains(&Action::ToggleRecord));
        assert_eq!(trace.consumed, 3);
    }

    #[test]
    fn recording_preview_shortcuts_control_outputs_require_exact_modifier_set() {
        let preview = context(Stage::Preview);
        for key in ["c", "s"] {
            let expected = if key == "c" {
                Action::CopyFile
            } else {
                Action::SaveAs
            };
            for mask in 0..32 {
                let actual = route(key, modifiers(mask), false, preview);
                assert_eq!(
                    actual,
                    if mask == 1 {
                        Route::Run(expected)
                    } else {
                        Route::Pass
                    },
                    "键 {key} 的修饰键集合 {mask:05b} 不应串入复制/保存入口"
                );
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_plain_keys_reject_every_modifier_combination() {
        let contracts = [
            ("f9", Stage::Idle, Action::ToggleRecord),
            ("f10", Stage::Recording, Action::TogglePause),
            ("space", Stage::Preview, Action::TogglePlayback),
            ("escape", Stage::Preview, Action::Close),
            ("enter", Stage::Preview, Action::Done),
        ];
        for (key, stage, expected) in contracts {
            for mask in 0..32 {
                assert_eq!(
                    route(key, modifiers(mask), false, context(stage)),
                    if mask == 0 {
                        Route::Run(expected)
                    } else {
                        Route::Pass
                    },
                    "键 {key} 的修饰键集合 {mask:05b} 应保持独立"
                );
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_new_outputs_belong_only_to_preview() {
        for stage in STAGES {
            let mut trace = Trace::default();
            for held in [false, true] {
                trace.event("c", CONTROL, held, context(stage));
                trace.event("s", CONTROL, held, context(stage));
                trace.event("enter", PLAIN, held, context(stage));
            }
            if stage == Stage::Preview {
                assert_eq!(
                    trace.actions,
                    [Action::CopyFile, Action::SaveAs, Action::Done]
                );
                assert_eq!(trace.consumed, 6);
                assert_eq!(trace.passed, 0);
            } else {
                assert!(trace.actions.is_empty());
                assert_eq!(trace.consumed, 0);
                assert_eq!(trace.passed, 6);
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_owner_ime_and_native_dialog_release_all_keys() {
        let keys = [
            ("f9", PLAIN),
            ("f10", PLAIN),
            ("space", PLAIN),
            ("escape", PLAIN),
            ("enter", PLAIN),
            ("c", CONTROL),
            ("s", CONTROL),
        ];
        // 每一种输入所有权阻挡均独立生效；组合不应改变优先级。
        for stage in STAGES {
            for blocked in 1..8 {
                let mut ctx = context(stage);
                ctx.keyboard_owned = blocked & 1 == 0;
                ctx.ime_composing = blocked & 2 != 0;
                ctx.native_dialog = blocked & 4 != 0;
                ctx.flyout_open = true;
                ctx.output_ready = false;
                let mut trace = Trace::default();
                for (key, mods) in keys {
                    for held in [false, true] {
                        trace.event(key, mods, held, ctx);
                    }
                }
                assert!(trace.actions.is_empty());
                assert_eq!(trace.consumed, 0);
                assert_eq!(trace.passed, keys.len() * 2);
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_busy_or_flyout_blocks_outputs_without_queueing() {
        for flyout_open in [false, true] {
            for output_ready in [false, true] {
                if !flyout_open && output_ready {
                    continue;
                }
                let mut ctx = context(Stage::Preview);
                ctx.flyout_open = flyout_open;
                ctx.output_ready = output_ready;
                let mut trace = Trace::default();
                for held in [false, true, true] {
                    trace.event("c", CONTROL, held, ctx);
                    trace.event("s", CONTROL, held, ctx);
                    trace.event("enter", PLAIN, held, ctx);
                }
                assert!(trace.actions.is_empty());
                assert_eq!(trace.consumed, 9);
                assert_eq!(trace.passed, 0);
                // 任务结束后没有自动重放；下一次新的按下才再次执行。
                ctx.flyout_open = false;
                ctx.output_ready = true;
                trace.event("s", CONTROL, true, ctx);
                assert!(trace.actions.is_empty());
                trace.event("s", CONTROL, false, ctx);
                assert_eq!(trace.actions, [Action::SaveAs]);
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_held_events_do_not_repeat_any_matched_action() {
        let contracts = [
            ("f9", PLAIN, context(Stage::Idle), Action::ToggleRecord),
            ("f10", PLAIN, context(Stage::Recording), Action::TogglePause),
            (
                "space",
                PLAIN,
                context(Stage::Preview),
                Action::TogglePlayback,
            ),
            ("escape", PLAIN, context(Stage::Preview), Action::Close),
            ("c", CONTROL, context(Stage::Preview), Action::CopyFile),
            ("s", CONTROL, context(Stage::Preview), Action::SaveAs),
            ("enter", PLAIN, context(Stage::Preview), Action::Done),
        ];
        for (key, mods, ctx, action) in contracts {
            let mut trace = Trace::default();
            trace.event(key, mods, false, ctx);
            for _ in 0..20 {
                trace.event(key, mods, true, ctx);
            }
            assert_eq!(trace.actions, [action], "长按 {key} 不应执行第二次");
            assert_eq!(trace.consumed, 21);
            assert_eq!(trace.passed, 0);
        }
    }

    #[test]
    fn recording_preview_shortcuts_escape_dismisses_before_stop_or_close() {
        for stage in STAGES {
            let mut ctx = context(stage);
            ctx.flyout_open = true;
            ctx.output_ready = false;
            let mut trace = Trace::default();
            trace.event("escape", PLAIN, false, ctx);
            trace.event("escape", PLAIN, true, ctx);
            assert_eq!(trace.actions, [Action::DismissFlyout]);
            assert_eq!(trace.consumed, 2);
            ctx.flyout_open = false;
            trace.event("escape", PLAIN, false, ctx);
            let expected = match stage {
                Stage::Idle | Stage::Preview => Action::Close,
                Stage::Countdown | Stage::Recording => Action::ToggleRecord,
            };
            assert_eq!(trace.actions, [Action::DismissFlyout, expected]);
        }
    }

    #[test]
    fn recording_preview_shortcuts_record_pause_and_playback_preserve_stage_contract() {
        // 产品状态表是独立合同：列顺序对应 Idle、Countdown、Recording、Preview。
        let rows = [
            (
                "f9",
                [
                    Some(Action::ToggleRecord),
                    Some(Action::ToggleRecord),
                    Some(Action::ToggleRecord),
                    None,
                ],
            ),
            ("f10", [None, None, Some(Action::TogglePause), None]),
            (
                "space",
                [
                    None,
                    None,
                    Some(Action::TogglePause),
                    Some(Action::TogglePlayback),
                ],
            ),
        ];
        for (key, expected) in rows {
            for (stage, expected) in STAGES.into_iter().zip(expected) {
                assert_eq!(
                    route(key, PLAIN, false, context(stage)),
                    expected.map(Route::Run).unwrap_or(Route::Pass),
                    "旧键 {key} 在 {stage:?} 的行为应保持"
                );
            }
        }
    }

    #[test]
    fn recording_preview_shortcuts_flyout_does_not_redefine_old_record_or_space_keys() {
        let contracts = [
            ("f9", Stage::Idle, Route::Run(Action::ToggleRecord)),
            ("f9", Stage::Countdown, Route::Run(Action::ToggleRecord)),
            ("f9", Stage::Recording, Route::Run(Action::ToggleRecord)),
            ("f9", Stage::Preview, Route::Pass),
            ("f10", Stage::Recording, Route::Run(Action::TogglePause)),
            ("space", Stage::Recording, Route::Run(Action::TogglePause)),
            ("space", Stage::Preview, Route::Run(Action::TogglePlayback)),
        ];
        for (key, stage, expected) in contracts {
            let mut ctx = context(stage);
            ctx.flyout_open = true;
            ctx.output_ready = false;
            assert_eq!(route(key, PLAIN, false, ctx), expected);
        }
    }

    #[test]
    fn recording_preview_shortcuts_unmatched_and_modified_held_keys_still_pass() {
        let preview = context(Stage::Preview);
        for key in ["a", "tab", "return", "numpadenter", "C", "S", "f8", ""] {
            for mods in [PLAIN, CONTROL] {
                assert_eq!(route(key, mods, false, preview), Route::Pass);
                assert_eq!(route(key, mods, true, preview), Route::Pass);
            }
        }
        for (key, stage) in [
            ("f9", Stage::Preview),
            ("f10", Stage::Idle),
            ("space", Stage::Countdown),
            ("enter", Stage::Recording),
        ] {
            assert_eq!(route(key, PLAIN, true, context(stage)), Route::Pass);
        }
        assert_eq!(route("c", modifiers(3), true, preview), Route::Pass);
        assert_eq!(route("escape", CONTROL, true, preview), Route::Pass);
    }

    #[test]
    fn recording_preview_shortcuts_busy_preview_keeps_playback_and_escape_available() {
        let mut ctx = context(Stage::Preview);
        ctx.output_ready = false;
        let mut trace = Trace::default();
        trace.event("space", PLAIN, false, ctx);
        trace.event("escape", PLAIN, false, ctx);
        assert_eq!(trace.actions, [Action::TogglePlayback, Action::Close]);
        trace.event("enter", PLAIN, false, ctx);
        trace.event("c", CONTROL, false, ctx);
        trace.event("s", CONTROL, false, ctx);
        assert_eq!(trace.actions, [Action::TogglePlayback, Action::Close]);
        assert_eq!(trace.consumed, 5);
    }
}
