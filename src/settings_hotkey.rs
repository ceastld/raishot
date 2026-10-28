//! 设置页热键录制；只在独立输入区域获取焦点时处理按键。
//! 每个动作一行：点击快捷键框即录制，框内清除，非默认时可恢复默认，错误就近显示。
//! 物理输入由全局组合录制接管，可录键盘与鼠标任意组合；注入的按键仍走窗口内录制。
use crate::{
    capture_toolbar::Palette,
    hotkey::{HotkeySpec, HotkeyTarget, ALT, CTRL, SHIFT, WIN},
    preferences::Preferences,
    settings_trigger::{RecorderEvent, TriggerRecorder},
    settings_view::SettingsView,
    settings_widgets::{self as widgets, Surface},
    toolbar_icons::Icon,
    trigger::{Activation, Button, MouseButton as InputMouseButton, Trigger},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, ElementId, KeyDownEvent, MouseButton,
    SharedString, Stateful, Window,
};
use std::time::Duration;

impl SettingsView {
    pub(crate) fn stop_recording_hotkey(&mut self) {
        self.recording_hotkey = false;
        self.quick_actions.recording_action = None;
        self.recording_generation = self.recording_generation.wrapping_add(1);
        self.recording_held.clear();
        if let Some(native) = &self.hotkey_native {
            native.deactivate();
        }
        if let Some(recorder) = &self.trigger_recorder {
            recorder.cancel();
        }
        if let Some(owner) = self.recording_native.take() {
            crate::ui::set_hotkey_recording(false, owner);
        }
    }
    pub(crate) fn start_recording_hotkey(
        &mut self,
        target: HotkeyTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording_hotkey();
        self.hotkey_error = None;
        self.recording_hotkey_target = target;
        window.focus(&self.hotkey_focus);
        let owner = crate::gpui_capture::hwnd(window).0 as usize;
        crate::ui::set_hotkey_recording(true, owner);
        self.recording_native = Some(owner);
        self.recording_hotkey = true;
        let global = self.begin_trigger_recording();
        self.status = if global {
            trf!(
                "正在录制{}：按住键盘或鼠标按键，再按目标键或滚动滚轮，全部松开后保存；Esc 取消，Backspace 禁用。",
                "Recording {}: hold keys or mouse buttons, then press a key or scroll; release all to save. Esc cancels, Backspace disables.",
                target.label()
            )
        } else {
            trf!(
                "正在录制{}热键：请按 Ctrl / Alt / Shift / Win 与主键组合，Esc 取消，Backspace 禁用。",
                "Recording the {} hotkey: press Ctrl / Alt / Shift / Win with a key; Esc cancels, Backspace disables.",
                target.label()
            )
        };
        match &self.hotkey_native {
            Some(native) => native.activate(),
            None if global => {}
            None => {
                self.status = tr!(
                    "原生录制适配不可用，当前仅可录制 GPUI 支持的按键。",
                    "Native key recording is unavailable; only keys supported by GPUI can be recorded."
                )
                .into();
                return;
            }
        }
        let generation = self.recording_generation;
        cx.spawn_in(window, async move |view, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let keep_recording = view.update_in(cx, |v, window, cx| {
                if !v.recording_hotkey || v.recording_generation != generation {
                    return false;
                }
                if !window.is_window_active()
                    || !v.hotkey_focus.is_focused(window)
                    || v.hotkey_native
                        .as_ref()
                        .is_some_and(|native| !native.is_active())
                {
                    v.stop_recording_hotkey();
                    v.status = tr!(
                        "已取消录制，保留原有快捷键。",
                        "Recording canceled; the previous hotkey is kept."
                    )
                    .into();
                    cx.notify();
                    return false;
                }
                let keys = v
                    .hotkey_native
                    .as_ref()
                    .map_or_else(Vec::new, |native| native.take_keys());
                for spec in keys {
                    v.consume_hotkey_spec(spec, window, cx);
                    if !v.recording_hotkey {
                        return false;
                    }
                }
                let events = v
                    .trigger_recorder
                    .as_ref()
                    .map_or_else(Vec::new, TriggerRecorder::poll);
                for event in events {
                    v.consume_recorder_event(event, window, cx);
                    if !v.recording_hotkey || v.recording_generation != generation {
                        return false;
                    }
                }
                true
            });
            if !matches!(keep_recording, Ok(true)) {
                break;
            }
        })
        .detach();
    }
    fn consume_hotkey_spec(
        &mut self,
        spec: HotkeySpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quick_actions.recording_action.is_some() {
            self.consume_quick_hotkey(spec, cx);
            return;
        }
        if spec.key == 0x1b {
            self.stop_recording_hotkey();
            self.status = tr!(
                "已取消录制，保留原有快捷键。",
                "Recording canceled; the previous hotkey is kept."
            )
            .into();
            cx.notify();
            return;
        }
        let target = self.recording_hotkey_target;
        if spec.key == 0x08 && spec.modifiers == 0 {
            self.change_hotkey(target, 4, None, window, cx);
            return;
        }
        if let Err(error) = spec.validate() {
            // 继续录制，错误就近显示在该行。
            let message = trf!(
                "{error}；请换一个组合，Esc 取消。",
                "{error}; try another combination, or Esc to cancel."
            );
            self.status = message.clone();
            self.hotkey_error = Some((target, message));
            cx.notify();
            return;
        }
        self.change_hotkey(target, 3, Some(spec), window, cx);
    }
    /// 开始一轮全局组合录制；监听不可用时返回 false，由窗口内按键录制兜底。
    fn begin_trigger_recording(&mut self) -> bool {
        self.recording_held.clear();
        if self.trigger_recorder.is_none() {
            match TriggerRecorder::start() {
                Ok(recorder) => self.trigger_recorder = Some(recorder),
                Err(error) => {
                    eprintln!("全局组合录制不可用：{error}");
                    return false;
                }
            }
        }
        self.trigger_recorder
            .as_ref()
            .is_some_and(|recorder| recorder.begin().is_ok())
    }
    fn consume_recorder_event(
        &mut self,
        event: RecorderEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (held, activation) = match event {
            RecorderEvent::Progress(held) => {
                self.recording_held = held;
                cx.notify();
                return;
            }
            RecorderEvent::Cancelled => {
                self.stop_recording_hotkey();
                self.status = tr!(
                    "已取消录制，保留原有快捷键。",
                    "Recording canceled; the previous hotkey is kept."
                )
                .into();
                cx.notify();
                return;
            }
            RecorderEvent::Error(error) => {
                self.stop_recording_hotkey();
                self.status = trf!(
                    "录制失败：{error}；保留原有快捷键。",
                    "Recording failed: {error}; the previous hotkey is kept."
                );
                cx.notify();
                return;
            }
            RecorderEvent::Recorded { held, activation } => (held, activation),
        };
        self.recording_held.clear();
        // 录制期间单击鼠标左／右键表示想离开录制，按取消处理。
        if held.is_empty()
            && matches!(
                activation,
                Activation::Press(Button::Mouse(
                    InputMouseButton::Left | InputMouseButton::Right
                ))
            )
        {
            self.stop_recording_hotkey();
            self.status = tr!(
                "已取消录制，保留原有快捷键。",
                "Recording canceled; the previous hotkey is kept."
            )
            .into();
            cx.notify();
            return;
        }
        let target = self.recording_hotkey_target;
        match Trigger::from_recorded(&held, activation) {
            Ok(Trigger::Hotkey(spec)) => self.consume_hotkey_spec(spec, window, cx),
            Ok(Trigger::Input(_)) if self.quick_actions.recording_action.is_some() => {
                self.status = tr!(
                    "快捷动作目前只支持 Ctrl / Alt / Shift / Win 与主键的组合；请重新按键，Esc 取消。",
                    "Quick actions only support Ctrl / Alt / Shift / Win with a key for now; press again, or Esc to cancel."
                )
                .into();
                cx.notify();
            }
            Ok(trigger) => self.change_trigger(target, trigger, window, cx),
            Err(error) => {
                let message = trf!(
                    "{error}；请换一个组合，Esc 取消。",
                    "{error}; try another combination, or Esc to cancel."
                );
                self.status = message.clone();
                if self.quick_actions.recording_action.is_none() {
                    self.hotkey_error = Some((target, message));
                }
                cx.notify();
            }
        }
        // 组合无效时继续录制；单轮录制已结束，需要重新开始。
        if self.recording_hotkey && !self.begin_trigger_recording() {
            self.stop_recording_hotkey();
        }
    }
    fn change_trigger(
        &mut self,
        target: HotkeyTarget,
        trigger: Trigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = Preferences::load();
        self.preferences = previous.with_trigger_change(target, trigger);
        self.stop_recording_hotkey();
        self.apply_hotkey(target, previous, window, cx);
        cx.notify();
    }
    /// 在最新配置上只改这一个动作，结束录制并交给托盘注册。
    fn change_hotkey(
        &mut self,
        target: HotkeyTarget,
        mode: u8,
        custom: Option<HotkeySpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = Preferences::load();
        self.preferences = previous.with_hotkey_change(target, mode, custom);
        self.stop_recording_hotkey();
        self.apply_hotkey(target, previous, window, cx);
        cx.notify();
    }
    fn apply_hotkey(
        &mut self,
        target: HotkeyTarget,
        previous: Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.hotkey_error = None;
        if let Some(error) = crate::settings_quick_actions::hotkey_conflict_message(
            &crate::quick_action_store::QuickActionStore::load(),
            &self.preferences,
        ) {
            self.preferences = previous;
            self.status = error.clone();
            self.hotkey_error = Some((target, error));
            cx.notify();
            return false;
        }
        // 起点必须先于通知，否则可能漏掉已经写完的新回执。
        let requested_after = crate::hotkey_status::request_stamp();
        match self.preferences.save_hotkey_change(target) {
            Ok(saved) => self.preferences = saved,
            Err(error) => {
                self.preferences = previous;
                self.status = error.to_string();
                self.hotkey_error = Some((target, self.status.clone()));
                cx.notify();
                return false;
            }
        }
        self.hotkey_request = Some(self.preferences.clone());
        self.hotkey_request_started = requested_after;
        crate::settings_quick_actions::notify_tray();
        self.status = tr!(
            "已保存，正在等待托盘注册快捷键…",
            "Saved; waiting for the tray to register the hotkey…"
        )
        .into();
        // 托盘回执通常在百毫秒内写出；自动轮询约 3 秒，不再让用户手动检查。
        cx.spawn_in(window, async move |view, cx| {
            for _ in 0..15 {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let settled = view.update(cx, |v, cx| {
                    if v.hotkey_request_started != requested_after || v.hotkey_request.is_none() {
                        return true;
                    }
                    let found = v.refresh_hotkey_status(target);
                    if found {
                        cx.notify();
                    }
                    found
                });
                if !matches!(settled, Ok(false)) {
                    return;
                }
            }
            let _ = view.update(cx, |v, cx| {
                if v.hotkey_request_started == requested_after && v.hotkey_request.is_some() {
                    v.hotkey_request = None;
                    v.hotkey_request_started = 0;
                    v.status = tr!(
                        "已保存；托盘没有回应，可能未在运行，启动托盘后生效。",
                        "Saved; the tray did not respond. It may not be running; the hotkey applies once it starts."
                    )
                    .into();
                    cx.notify();
                }
            });
        })
        .detach();
        true
    }

    /// 读取托盘回执；收到时返回 true，失败原因记到对应行。
    fn refresh_hotkey_status(&mut self, target: HotkeyTarget) -> bool {
        let Some(requested) = &self.hotkey_request else {
            return true;
        };
        let Some((ok, message)) =
            crate::ui::read_hotkey_status(requested, self.hotkey_request_started)
        else {
            return false;
        };
        if !ok {
            self.hotkey_error = Some((target, message.clone()));
        }
        self.status = message;
        // 原生托盘在冲突时只回退热键字段；重新读取真正生效的配置。
        let active = Preferences::load();
        for target in HotkeyTarget::ALL {
            self.preferences.restore_hotkey_from(target, &active);
        }
        self.hotkey_request = None;
        self.hotkey_request_started = 0;
        true
    }

    pub(crate) fn record_hotkey(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.recording_hotkey || !self.hotkey_focus.is_focused(window) {
            return;
        }
        cx.stop_propagation();
        if event.is_held
            || self
                .hotkey_native
                .as_ref()
                .is_some_and(|native| native.is_active())
        {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        // 修饰键独立按下不能完成录制。
        if matches!(
            key,
            "control" | "ctrl" | "alt" | "shift" | "win" | "super" | "cmd"
        ) {
            return;
        }
        let bits = (u32::from(modifiers.control) * CTRL)
            | (u32::from(modifiers.alt) * ALT)
            | (u32::from(modifiers.shift) * SHIFT)
            | (u32::from(modifiers.platform) * WIN);
        match HotkeySpec::from_key_name(key, bits) {
            Ok(spec) => {
                self.consume_hotkey_spec(spec, window, cx);
            }
            Err(error) => {
                self.status = trf!(
                    "{error}；请重新按组合键，Esc 取消。",
                    "{error}; press another combination, or Esc to cancel."
                );
                cx.notify();
            }
        }
    }

    pub(crate) fn hotkey_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut categories = Vec::new();
        for target in HotkeyTarget::ALL {
            if !categories.contains(&target.category()) {
                categories.push(target.category());
            }
        }
        div()
            .flex()
            .flex_col()
            .gap_6()
            .flex_shrink_0()
            .track_focus(&self.hotkey_focus)
            .on_key_down(cx.listener(Self::record_hotkey))
            .child(widgets::note(
                        tr!(
                            "点击快捷键框，按下新组合并全部松开即保存；Esc 取消，Backspace 停用，单击鼠标退出录制。除 Ctrl/Alt/Shift/Win 组合外，也可录 CapsLock + 键、鼠标侧键、按住鼠标键 + 滚轮等；这类组合由托盘监听输入，按住的前缀键在未触发时会稍后补发。组合被占用时保留原快捷键。",
                            "Click a hotkey box, press the new combination and release to save it. Esc cancels, Backspace disables, a click leaves recording. Besides Ctrl/Alt/Shift/Win combinations you can record CapsLock + key, mouse side buttons, or a held mouse button + wheel; the tray listens for these and replays held prefix keys a moment later when nothing triggers. If a combination is taken, the previous hotkey is kept."
                        ),
                        palette,
                    ))
            .children(categories.into_iter().map(|category| {
                widgets::section(category, palette).child(widgets::card(
                    surface,
                    HotkeyTarget::ALL.into_iter().filter(|target| target.category() == category)
                        .map(|target| self.hotkey_row(target, cx)),
                ))
            }))
            .child(self.quick_action_settings(cx))
    }

    /// 一个动作一行：名称与说明在左，快捷键框（点击录制、框内清除）与恢复默认在右。
    fn hotkey_row(&self, target: HotkeyTarget, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let (record_id, clear_id, reset_id): (SharedString, SharedString, SharedString) =
            match target {
                HotkeyTarget::Capture => (
                    "hotkey-record".into(),
                    "hotkey-clear".into(),
                    "hotkey-reset".into(),
                ),
                HotkeyTarget::ClipboardPin => (
                    "pin-hotkey-record".into(),
                    "pin-hotkey-clear".into(),
                    "pin-hotkey-reset".into(),
                ),
                _ => (
                    format!("hotkey-{}-record", target.id()).into(),
                    format!("hotkey-{}-clear", target.id()).into(),
                    format!("hotkey-{}-reset", target.id()).into(),
                ),
            };
        let recording = self.recording_hotkey
            && self.quick_actions.recording_action.is_none()
            && self.recording_hotkey_target == target;
        let trigger = self.preferences.trigger_for(target);
        let error = self
            .hotkey_error
            .as_ref()
            .filter(|(failed, _)| *failed == target)
            .map(|(_, message)| SharedString::from(message.clone()));
        let (description, description_color): (SharedString, u32) = if let Some(error) = error {
            (error, palette.danger)
        } else if recording {
            (
                tr!(
                    "按下新组合并全部松开；可用鼠标按键、滚轮。Esc 取消，Backspace 停用。",
                    "Press the new combination and release; mouse buttons and the wheel work. Esc cancels, Backspace disables."
                )
                .into(),
                palette.accent,
            )
        } else {
            (target.description().into(), palette.muted)
        };
        let text = div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(palette.foreground))
                    .child(target.label()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(rgb(description_color))
                    .child(description),
            );

        let keys = if recording {
            (!self.recording_held.is_empty()).then(|| {
                self.recording_held
                    .iter()
                    .map(|button| crate::trigger::button_label(*button))
                    .collect()
            })
        } else {
            trigger.as_ref().map(Trigger::parts)
        };
        let mut field = hotkey_box(record_id.clone(), keys, recording, 200., palette, surface)
            .child(self.settings_probe(record_id.to_string()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, window, cx| {
                    cx.stop_propagation();
                    v.start_recording_hotkey(target, window, cx);
                    cx.notify();
                }),
            );
        if trigger.is_some() && !recording {
            field = field.child(
                clear_button(
                    clear_id.clone(),
                    tr!("清除（停用此快捷键）", "Clear (disable this hotkey)"),
                    palette,
                )
                .child(self.settings_probe(clear_id.to_string()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, window, cx| {
                        cx.stop_propagation();
                        v.change_hotkey(target, 4, None, window, cx);
                    }),
                ),
            );
        }

        // 已是默认值时只占位，避免按钮出现／消失让快捷键框左右跳动。
        let reset = if trigger == target.default_binding().map(Trigger::Hotkey) {
            div().size(px(28.)).flex_shrink_0().into_any_element()
        } else {
            let default_mode = Preferences::default().hotkey_mode(target);
            let tip: &'static str = match target {
                HotkeyTarget::Capture => tr!("恢复默认 Ctrl+1", "Reset to Ctrl+1"),
                HotkeyTarget::ClipboardPin => tr!("恢复默认 Ctrl+2", "Reset to Ctrl+2"),
                _ => tr!("恢复默认（未设置）", "Reset (not set)"),
            };
            widgets::icon_button(reset_id.clone(), Icon::Undo, true, palette)
                .child(self.settings_probe(reset_id.to_string()))
                .tooltip(move |_, cx| cx.new(move |_| crate::pin_view::TextToolHint(tip)).into())
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.change_hotkey(target, default_mode, None, window, cx);
                }))
                .into_any_element()
        };

        div()
            .relative()
            .flex()
            .items_center()
            .gap_3()
            .min_h(px(60.))
            .px_3()
            .py_2()
            .rounded_md()
            .child(text)
            .child(field)
            .child(reset)
            .into_any_element()
    }
}

/// 快捷键框：显示键帽或“未设置”，录制时高亮并显示正在按住的键；点击录制由调用者挂接。
pub(crate) fn hotkey_box(
    id: impl Into<ElementId>,
    keys: Option<Vec<String>>,
    recording: bool,
    min_width: f32,
    palette: Palette,
    surface: Surface,
) -> Stateful<Div> {
    let content = if let Some(keys) = &keys {
        widgets::keycaps(keys, palette, surface).into_any_element()
    } else if recording {
        div()
            .text_color(rgb(palette.accent))
            .child(tr!("请按下组合键…", "Press keys…"))
            .into_any_element()
    } else {
        div()
            .text_color(rgb(palette.muted))
            .child(tr!("未设置", "Not set"))
            .into_any_element()
    };
    div()
        .id(id)
        .relative()
        .min_w(px(min_width))
        .h(px(34.))
        .pl(px(if keys.is_some() { 4. } else { 10. }))
        .pr(px(4.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
        .border_1()
        .border_color(rgb(if recording {
            palette.accent
        } else {
            surface.line
        }))
        .bg(rgb(if recording {
            palette.selected
        } else {
            surface.track
        }))
        .when(!recording, |field| {
            field.hover(move |style| style.border_color(rgb(palette.accent)))
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .child(content),
        )
}

/// 快捷键框内的清除按钮；调用者负责阻止冒泡到框本身的录制。
pub(crate) fn clear_button(
    id: impl Into<ElementId>,
    tip: &'static str,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(px(24.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(palette.hover)))
        .child(crate::toolbar_icons::action(Icon::Close, palette.muted).size(px(12.)))
        .tooltip(move |_, cx| cx.new(move |_| crate::pin_view::TextToolHint(tip)).into())
}

impl Drop for SettingsView {
    fn drop(&mut self) {
        self.stop_recording_hotkey();
    }
}
