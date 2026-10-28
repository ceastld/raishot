//! 快捷动作设置；编辑保留草稿，应用只合并当前动作到最新配置。
use crate::{
    hotkey::{HotkeySpec, HotkeyTarget},
    preferences::Preferences,
    quick_action::{builtin_action, ActionError, HistoryPolicy, QuickAction, Source, Step},
    quick_action_store::QuickActionStore,
    settings_view::SettingsView,
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, AppContext, Context, Entity, MouseButton, SharedString,
    Subscription, Window,
};

struct Draft {
    original_id: Option<String>,
    action: QuickAction,
}
pub(crate) struct QuickActionSettings {
    store: QuickActionStore,
    draft: Option<Draft>,
    id: Entity<TextField>,
    name: Entity<TextField>,
    picker: Option<usize>,
    pub(crate) recording_action: Option<String>,
    _subscriptions: Vec<Subscription>,
}
impl QuickActionSettings {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        Self::from_store(QuickActionStore::load(), window, cx)
    }

    #[cfg(test)]
    pub(crate) fn new_in_memory(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        Self::from_store(QuickActionStore::default(), window, cx)
    }

    fn from_store(
        store: QuickActionStore,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let id = cx.new(|cx| {
            TextField::new_settings(String::new(), crate::i18n::Text("动作 ID", "Action ID"), cx)
        });
        let name = cx.new(|cx| {
            TextField::new_settings(
                String::new(),
                crate::i18n::Text("动作名称", "Action name"),
                cx,
            )
        });
        let group = vec![id.downgrade(), name.downgrade()];
        let mut subscriptions = Vec::new();
        for field in [&id, &name] {
            field.update(cx, |view, cx| {
                view.configure_focus(group.clone(), window, cx)
            });
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |view: &mut SettingsView, _, event, _, cx| match event {
                    TextFieldEvent::Submit => view.apply_quick_action(cx),
                    TextFieldEvent::Changed => {
                        view.status.clear();
                        cx.notify();
                    }
                },
            ));
            subscriptions
                .push(cx.observe_in(field, window, |_: &mut SettingsView, _, _, cx| cx.notify()));
        }
        Self {
            store,
            draft: None,
            id,
            name,
            picker: None,
            recording_action: None,
            _subscriptions: subscriptions,
        }
    }
    pub(crate) fn reload(&mut self) {
        self.store = QuickActionStore::load();
    }
    fn composing(&self, cx: &gpui::App) -> bool {
        self.id.read(cx).is_composing() || self.name.read(cx).is_composing()
    }
}

pub(crate) fn hotkey_conflict_message(
    store: &QuickActionStore,
    preferences: &Preferences,
) -> Option<String> {
    let mut bindings: Vec<_> = HotkeyTarget::ALL
        .into_iter()
        .filter_map(|target| {
            preferences
                .trigger_for(target)
                .map(|trigger| (target.label().to_owned(), trigger))
        })
        .collect();
    bindings.extend(
        store
            .actions
            .iter()
            .filter(|action| action.enabled)
            .filter_map(|action| {
                action.hotkey.map(|hotkey| {
                    (
                        action.label().to_owned(),
                        crate::trigger::Trigger::Hotkey(hotkey),
                    )
                })
            }),
    );
    for (index, (name, trigger)) in bindings.iter().enumerate() {
        for (other_name, other_trigger) in &bindings[..index] {
            if crate::trigger::triggers_conflict(trigger, other_trigger) {
                return Some(trf!(
                    "{}（{}）与 {}（{}）的全局快捷键冲突，请换一个组合。",
                    "{} ({}) conflicts with {} ({}); choose another shortcut.",
                    other_name,
                    other_trigger.label(),
                    name,
                    trigger.label()
                ));
            }
        }
    }
    None
}
fn merge_action(
    mut latest: QuickActionStore,
    original_id: Option<&str>,
    action: QuickAction,
) -> std::result::Result<QuickActionStore, ActionError> {
    action.validate()?;
    if latest.find(&action.id).is_some() && original_id != Some(action.id.as_str()) {
        return Err(ActionError::DuplicateId(action.id));
    }
    if let Some(original_id) = original_id {
        latest.actions.retain(|item| item.id != original_id);
    }
    // 保持内置列表位置，不把正在编辑的一项挪到末尾。
    let position = QuickActionStore::default()
        .actions
        .iter()
        .position(|item| item.id == action.id);
    if let Some(position) = position {
        latest
            .actions
            .insert(position.min(latest.actions.len()), action);
    } else {
        latest.actions.push(action);
    }
    latest.validate()?;
    Ok(latest)
}
pub(crate) fn notify_tray() {
    if std::env::var_os("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").is_some() {
        return;
    }
    unsafe {
        use windows::{
            core::w,
            Win32::{Foundation::*, UI::WindowsAndMessaging::*},
        };
        if let Ok(window) = FindWindowW(w!("RustShot.MVP.Window"), None) {
            let _ = PostMessageW(
                Some(window),
                crate::ui::RELOAD_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

impl SettingsView {
    fn quick_controls_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.quick_actions.composing(cx) {
            self.status = tr!(
                "请先确认或取消输入法候选，再继续操作。",
                "Confirm or cancel the IME candidate first."
            )
            .into();
            cx.notify();
            false
        } else {
            true
        }
    }
    fn save_quick_store(&mut self, store: QuickActionStore, cx: &mut Context<Self>) -> bool {
        if let Some(error) = hotkey_conflict_message(&store, &Preferences::load()) {
            self.status = error;
            cx.notify();
            return false;
        }
        match store.save() {
            Ok(()) => {
                self.quick_actions.store = store;
                notify_tray();
                self.status = tr!(
                    "快捷动作已保存，下次截图时生效。",
                    "Quick actions saved for the next capture."
                )
                .into();
                cx.notify();
                true
            }
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                false
            }
        }
    }
    fn edit_quick_action(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        self.stop_recording_hotkey();
        self.quick_actions.reload();
        let action = match id {
            Some(id) => match self.quick_actions.store.find(id) {
                Some(action) => action.clone(),
                None => return,
            },
            None => {
                let mut action = builtin_action("copy").unwrap();
                let mut number = 1;
                while self
                    .quick_actions
                    .store
                    .find(&format!("custom-{number}"))
                    .is_some()
                {
                    number += 1;
                }
                action.id = format!("custom-{number}");
                action.name = tr!("新快捷动作", "New quick action").into();
                action.in_tray = true;
                action
            }
        };
        let builtin = action.is_builtin();
        self.quick_actions.id.update(cx, |field, cx| {
            field.set_text(action.id.clone(), cx);
            field.set_enabled(!builtin, cx);
        });
        self.quick_actions
            .name
            .update(cx, |field, cx| field.set_text(action.name.clone(), cx));
        self.quick_actions.draft = Some(Draft {
            original_id: id.map(str::to_owned),
            action,
        });
        self.quick_actions.picker = None;
        self.status.clear();
        cx.notify();
    }
    fn apply_quick_action(&mut self, cx: &mut Context<Self>) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        let Some(draft) = &self.quick_actions.draft else {
            return;
        };
        let mut action = draft.action.clone();
        if !action.is_builtin() {
            action.id = self.quick_actions.id.read(cx).text().trim().into();
        }
        action.name = self.quick_actions.name.read(cx).text().trim().into();
        let candidate = merge_action(
            QuickActionStore::load(),
            draft.original_id.as_deref(),
            action,
        );
        match candidate {
            Ok(store) => {
                if self.save_quick_store(store, cx) {
                    self.quick_actions.draft = None;
                    self.quick_actions.picker = None;
                }
            }
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
            }
        }
    }
    fn quick_action_changed(&mut self, cx: &mut Context<Self>) {
        self.status.clear();
        cx.notify();
    }
    fn toggle_quick_action(&mut self, id: &str, tray: bool, cx: &mut Context<Self>) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        let mut store = QuickActionStore::load();
        let Some(action) = store.find_mut(id) else {
            return;
        };
        if tray {
            action.in_tray = !action.in_tray;
        } else {
            action.enabled = !action.enabled;
        }
        self.save_quick_store(store, cx);
    }
    fn reset_or_delete_quick_action(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        self.stop_recording_hotkey();
        let mut store = QuickActionStore::load();
        if let Some(default) = builtin_action(id) {
            if let Some(action) = store.find_mut(id) {
                *action = default;
            }
        } else {
            store.actions.retain(|action| action.id != id);
        }
        if self.save_quick_store(store, cx)
            && self
                .quick_actions
                .draft
                .as_ref()
                .is_some_and(|draft| draft.original_id.as_deref() == Some(id))
        {
            self.quick_actions.draft = None;
        }
    }
    pub(crate) fn start_recording_quick_action(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        self.start_recording_hotkey(HotkeyTarget::Capture, window, cx);
        self.quick_actions.recording_action = Some(id.into());
        self.status = tr!(
            "正在录制快捷动作热键，Esc 取消，Backspace 禁用。",
            "Recording action hotkey; Esc cancels, Backspace disables."
        )
        .into();
    }
    pub(crate) fn consume_quick_hotkey(&mut self, spec: HotkeySpec, cx: &mut Context<Self>) {
        let Some(id) = self.quick_actions.recording_action.clone() else {
            return;
        };
        if spec.key == 0x1b {
            self.stop_recording_hotkey();
            self.status = tr!(
                "已取消录制，保留原有快捷键。",
                "Recording canceled; previous hotkey kept."
            )
            .into();
            cx.notify();
            return;
        }
        let hotkey = if spec.key == 0x08 && spec.modifiers == 0 {
            None
        } else {
            if let Err(error) = spec.validate() {
                self.status = error.to_string();
                cx.notify();
                return;
            }
            Some(spec)
        };
        let mut store = QuickActionStore::load();
        let Some(action) = store.find_mut(&id) else {
            self.stop_recording_hotkey();
            return;
        };
        action.hotkey = hotkey;
        if self.save_quick_store(store, cx) {
            self.stop_recording_hotkey();
        }
    }
    fn clear_quick_hotkey(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.quick_controls_ready(cx) {
            return;
        }
        self.stop_recording_hotkey();
        let mut store = QuickActionStore::load();
        if let Some(action) = store.find_mut(id) {
            action.hotkey = None;
            self.save_quick_store(store, cx);
        }
        cx.notify();
    }
    fn quick_action_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(draft) = &self.quick_actions.draft else {
            return div().into_any_element();
        };
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let action = &draft.action;
        let mut sources = div().flex().flex_wrap().gap_2();
        for source in Source::ALL {
            sources = sources.child(
                widgets::chip(
                    SharedString::from(format!("quick-source-{}", source as u8)),
                    source.label(),
                    action.recipe.source == source,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    if let Some(draft) = &mut view.quick_actions.draft {
                        draft.action.recipe.source = source;
                    }
                    view.quick_action_changed(cx);
                })),
            );
        }
        let mut rows = vec![
            widgets::field_block(tr!("动作名称", "Action name"), palette)
                .child(self.quick_actions.name.clone())
                .into_any_element(),
            widgets::field_block(tr!("命令行 ID", "Command line ID"), palette)
                .child(self.quick_actions.id.clone())
                .child(widgets::note(
                    tr!(
                        "仅字母、数字、连字符和下划线；内置 ID 固定。",
                        "Letters, digits, hyphens and underscores; built-in IDs are fixed."
                    ),
                    palette,
                ))
                .into_any_element(),
            widgets::field_block(tr!("采集源", "Capture source"), palette)
                .child(sources)
                .into_any_element(),
            widgets::row(tr!("先编辑再执行", "Edit first"), None, palette)
                .id("quick-edit-first")
                .cursor_pointer()
                .child(widgets::switch(action.recipe.edit, palette, surface))
                .on_click(cx.listener(|view, _, _, cx| {
                    if let Some(draft) = &mut view.quick_actions.draft {
                        draft.action.recipe.edit = !draft.action.recipe.edit;
                    }
                    view.quick_action_changed(cx);
                }))
                .into_any_element(),
        ];
        for slot in 0..3 {
            let step = action.recipe.steps[slot];
            let mut row = widgets::row(trf!("步骤 {}", "Step {}", slot + 1), None, palette).child(
                widgets::button(
                    SharedString::from(format!("quick-step-{slot}")),
                    format!("{} ▾", step.map_or(tr!("无", "None"), Step::label)),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.quick_actions.picker = if view.quick_actions.picker == Some(slot) {
                        None
                    } else {
                        Some(slot)
                    };
                    cx.notify();
                })),
            );
            if self.quick_actions.picker == Some(slot) {
                let mut choices = div().flex().flex_wrap().gap_2();
                for option in std::iter::once(None).chain(Step::ALL.into_iter().map(Some)) {
                    choices = choices.child(
                        widgets::chip(
                            SharedString::from(format!(
                                "quick-step-{slot}-{}",
                                option.map_or(0, |step| step as u8)
                            )),
                            option.map_or(tr!("无", "None"), Step::label),
                            option == step,
                            palette,
                            surface,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(draft) = &mut view.quick_actions.draft {
                                draft.action.recipe.steps[slot] = option;
                            }
                            view.quick_actions.picker = None;
                            view.quick_action_changed(cx);
                        })),
                    );
                }
                row = row.flex_col().items_start().child(choices);
            }
            rows.push(row.into_any_element());
        }
        let mut history = widgets::segmented(surface);
        for policy in [HistoryPolicy::Inherit, HistoryPolicy::Never] {
            history = history.child(
                widgets::segment(
                    SharedString::from(format!(
                        "quick-history-{}",
                        u8::from(policy == HistoryPolicy::Never)
                    )),
                    policy.label(),
                    action.recipe.history == policy,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    if let Some(draft) = &mut view.quick_actions.draft {
                        draft.action.recipe.history = policy;
                    }
                    view.quick_action_changed(cx);
                })),
            );
        }
        rows.push(
            widgets::row(tr!("写入历史", "History"), None, palette)
                .child(history)
                .into_any_element(),
        );
        if let Err(error) = action.recipe.validate() {
            rows.push(widgets::note(error.to_string(), palette).into_any_element());
        }
        let buttons = div()
            .flex()
            .gap_2()
            .px_3()
            .py_3()
            .child(
                widgets::button(
                    "quick-apply",
                    tr!("应用", "Apply"),
                    ButtonKind::Primary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(|view, _, _, cx| view.apply_quick_action(cx))),
            )
            .child(
                widgets::button(
                    "quick-revert",
                    tr!("取消修改", "Revert"),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    if !view.quick_controls_ready(cx) {
                        return;
                    }
                    view.quick_actions.draft = None;
                    view.quick_actions.picker = None;
                    cx.notify();
                })),
            );
        rows.push(buttons.into_any_element());
        widgets::section(tr!("编辑快捷动作", "Edit quick action"), palette)
            .child(widgets::card(surface, rows))
            .into_any_element()
    }
    pub(crate) fn quick_action_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut rows = Vec::new();
        for action in &self.quick_actions.store.actions {
            let id = action.id.clone();
            let recording = self.recording_hotkey
                && self.quick_actions.recording_action.as_deref() == Some(id.as_str());
            let toggle_id = id.clone();
            let tray_id = id.clone();
            let edit_id = id.clone();
            let reset_id = id.clone();
            let clear_id = id.clone();
            let label = div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(div().truncate().child(action.label().to_owned()))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .truncate()
                        .child(action.recipe.summary()),
                );
            let row =
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("quick-enabled-{id}")))
                            .cursor_pointer()
                            .child(widgets::switch(action.enabled, palette, surface))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.toggle_quick_action(&toggle_id, false, cx)
                            })),
                    )
                    .child(label)
                    .child(
                        crate::settings_hotkey::hotkey_box(
                            SharedString::from(format!("quick-hotkey-{id}")),
                            if recording {
                                (!self.recording_held.is_empty()).then(|| {
                                    self.recording_held
                                        .iter()
                                        .map(|button| crate::trigger::button_label(*button))
                                        .collect()
                                })
                            } else {
                                action.hotkey.map(|spec| spec.parts())
                            },
                            recording,
                            136.,
                            palette,
                            surface,
                        )
                        .when(action.hotkey.is_some() && !recording, |field| {
                            field.child(
                                crate::settings_hotkey::clear_button(
                                    SharedString::from(format!("quick-hotkey-clear-{clear_id}")),
                                    tr!("清除快捷键", "Clear hotkey"),
                                    palette,
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |view, _, _, cx| {
                                        cx.stop_propagation();
                                        view.clear_quick_hotkey(&clear_id, cx);
                                    }),
                                ),
                            )
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                view.start_recording_quick_action(&id, window, cx);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        widgets::chip(
                            SharedString::from(format!("quick-tray-{tray_id}")),
                            tr!("托盘", "Tray"),
                            action.in_tray,
                            palette,
                            surface,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.toggle_quick_action(&tray_id, true, cx)
                        })),
                    )
                    .child(
                        widgets::button(
                            SharedString::from(format!("quick-edit-{edit_id}")),
                            tr!("编辑", "Edit"),
                            ButtonKind::Secondary,
                            palette,
                            surface,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.edit_quick_action(Some(&edit_id), cx)
                        })),
                    )
                    .child(
                        widgets::button(
                            SharedString::from(format!("quick-reset-{reset_id}")),
                            if action.is_builtin() {
                                tr!("恢复", "Reset")
                            } else {
                                tr!("删除", "Delete")
                            },
                            ButtonKind::Secondary,
                            palette,
                            surface,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.reset_or_delete_quick_action(&reset_id, cx)
                        })),
                    );
            rows.push(row.into_any_element());
        }
        let add = widgets::button(
            "quick-add",
            tr!("添加快捷动作", "Add quick action"),
            ButtonKind::Secondary,
            palette,
            surface,
        )
        .on_click(cx.listener(|view, _, _, cx| view.edit_quick_action(None, cx)));
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                widgets::section(tr!("快捷动作", "Quick actions"), palette)
                    .child(widgets::card(surface, rows))
                    .child(div().pt_3().child(add))
                    .child(widgets::note(
                        tr!(
                            "未绑定热键的动作也可从托盘运行；编辑后点击应用保存。",
                            "Unbound actions can run from the tray; click Apply after editing."
                        ),
                        palette,
                    )),
            )
            .child(self.quick_action_editor(cx))
            .into_any_element()
    }
    pub(crate) fn quick_finish_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let capture = self
            .quick_actions
            .store
            .find("capture")
            .map(|action| action.recipe)
            .unwrap_or_default();
        let mut choices = widgets::segmented(surface);
        for id in ["copy", "pin", "quick-save", "copy-pin"] {
            let recipe = builtin_action(id).unwrap().recipe;
            let label = match id {
                "copy" => Step::CopyImage.label(),
                "pin" => Step::Pin.label(),
                "quick-save" => Step::QuickSave.label(),
                _ => tr!("复制并贴图", "Copy & pin"),
            };
            choices = choices.child(
                widgets::segment(
                    SharedString::from(format!("quick-finish-{id}")),
                    label,
                    capture.steps == recipe.steps,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    let mut store = QuickActionStore::load();
                    if let Some(action) = store.find_mut("capture") {
                        action.recipe.steps = recipe.steps;
                        action.recipe.edit = true;
                        action.recipe.source = Source::Region;
                    }
                    view.save_quick_store(store, cx);
                })),
            );
        }
        widgets::section(tr!("完成动作", "On finish"), palette)
            .child(widgets::card(
                surface,
                [widgets::row(
                    tr!("Enter／双击／✓", "Enter / double-click / ✓"),
                    Some(tr!("Ctrl+C 始终复制图片。", "Ctrl+C always copies the image.").into()),
                    palette,
                )
                .child(choices)
                .into_any_element()],
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_hotkeys_conflict_with_quick_actions_and_name_both_operations() {
        let mut store = QuickActionStore::default();
        let key = HotkeySpec::from_key_name("F8", crate::hotkey::CTRL).unwrap();
        store.find_mut("copy").unwrap().hotkey = Some(key);
        let preferences =
            Preferences::default().with_hotkey_change(HotkeyTarget::PinToggleGroup, 3, Some(key));
        let error = hotkey_conflict_message(&store, &preferences).unwrap();
        assert!(error.contains(HotkeyTarget::PinToggleGroup.label()));
        assert!(error.contains(store.find("copy").unwrap().label()));
        store.find_mut("copy").unwrap().enabled = false;
        assert!(hotkey_conflict_message(&store, &preferences).is_none());
    }

    #[test]
    fn command_hotkeys_check_listener_consumption_against_quick_actions() {
        let mut store = QuickActionStore::default();
        store.find_mut("copy").unwrap().hotkey =
            Some(HotkeySpec::from_key_name("F8", crate::hotkey::CTRL).unwrap());
        let preferences = Preferences::default().with_trigger_change(
            HotkeyTarget::PinToggleGroup,
            crate::trigger::Trigger::Input(
                crate::trigger::InputTrigger::from_wire("mouse:x1 + key:77").unwrap(),
            ),
        );
        assert!(hotkey_conflict_message(&store, &preferences).is_some());
    }

    #[test]
    fn quick_action_editor_merges_one_action_and_preserves_latest_other_settings() {
        let mut latest = QuickActionStore::default();
        latest.find_mut("pin").unwrap().in_tray = false;
        let mut edit = builtin_action("copy").unwrap();
        edit.name = "自定义复制".into();
        let merged = merge_action(latest, Some("copy"), edit).unwrap();
        assert!(!merged.find("pin").unwrap().in_tray);
        assert_eq!(merged.actions[1].name, "自定义复制");
    }
    #[test]
    fn quick_action_editor_cannot_rename_custom_over_a_builtin_id() {
        assert!(matches!(
            merge_action(
                QuickActionStore::default(),
                Some("custom-1"),
                builtin_action("copy").unwrap()
            ),
            Err(ActionError::DuplicateId(_))
        ));
    }
}
