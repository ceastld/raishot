use super::*;
use crate::config_json::TestDir;

#[test]
fn command_hotkeys_default_to_unbound_without_changing_legacy_bindings() {
    let original = Preferences::default();
    for target in HotkeyTarget::ALL {
        assert_eq!(
            original.trigger_for(target),
            target.default_binding().map(Trigger::Hotkey)
        );
    }
    let old = Preferences::parse_legacy_ini(
        "hotkey=4\npinHotkey=3\npinHotkeyModifiers=2\npinHotkeyKey=81\n",
    );
    assert!(old.trigger_for(HotkeyTarget::Capture).is_none());
    assert_eq!(old.hotkey_label_for(HotkeyTarget::ClipboardPin), "Alt+Q");
    assert!(old.command_hotkeys.is_empty());
}

#[test]
fn command_hotkeys_round_trip_system_input_and_retained_disabled_binding() {
    let dir = TestDir::new("command-hotkeys");
    let path = dir.join("settings.json");
    let spec = HotkeySpec::from_key_name("F9", ALT).unwrap();
    let input = InputTrigger::from_wire("key:14 + key:48").unwrap();
    let preferences = Preferences::default()
        .with_trigger_change(HotkeyTarget::PinToggleGroup, Trigger::Hotkey(spec))
        .with_trigger_change(HotkeyTarget::PinNextGroup, Trigger::Input(input));
    preferences.save_at(&path).unwrap();
    let loaded = Preferences::load_at(&path);
    for target in HotkeyTarget::ALL {
        assert!(
            loaded.hotkey_matches(target, &preferences),
            "{}",
            target.id()
        );
    }
    let disabled = loaded.with_hotkey_change(HotkeyTarget::PinToggleGroup, 4, None);
    disabled.save_at(&path).unwrap();
    let loaded = Preferences::load_at(&path);
    assert!(loaded.trigger_for(HotkeyTarget::PinToggleGroup).is_none());
    assert_eq!(loaded.custom_hotkey_for(HotkeyTarget::PinToggleGroup), spec);
    assert!(loaded.hotkey_matches(HotkeyTarget::PinNextGroup, &preferences));
}

#[test]
fn command_hotkeys_single_target_delta_and_rollback_keep_other_preferences() {
    let original = Preferences::default();
    let mut latest = original.with_trigger_change(
        HotkeyTarget::PinToggleGroup,
        Trigger::Input(InputTrigger::from_wire("key:14 + key:48").unwrap()),
    );
    latest.theme = ThemeMode::Dark;
    latest.history = true;
    latest = latest.with_trigger_change(
        HotkeyTarget::Settings,
        Trigger::Hotkey(HotkeySpec::from_key_name("F10", ALT).unwrap()),
    );
    let other = latest.clone();
    latest.restore_hotkey_from(HotkeyTarget::PinToggleGroup, &original);
    assert!(latest.hotkey_matches(HotkeyTarget::PinToggleGroup, &original));
    assert!(latest.hotkey_matches(HotkeyTarget::Settings, &other));
    assert!(!latest.hotkey_matches(HotkeyTarget::Settings, &original));
    assert_eq!(latest.theme, ThemeMode::Dark);
    assert!(latest.history);
    assert!(!latest
        .command_hotkeys
        .contains_key(HotkeyTarget::PinToggleGroup.id()));
}

#[test]
fn command_hotkeys_preserve_unknown_commands_and_disable_only_invalid_entries() {
    let dir = TestDir::new("command-hotkeys-unknown");
    let path = dir.join("settings.json");
    let future = serde_json::json!({"kind":"future","sequence":[1,2,3]});
    let original = serde_json::json!({
        "hotkey":4,
        "command_hotkeys": {
            "future.command": future,
            "pin.group.toggle_visibility": {"mode":3,"modifiers":0,"key":65},
            "app.settings": {"mode":3,"modifiers":2,"key":120},
            "pin.group.next": {"mode":6,"trigger":"invalid"},
            "pin.group.previous": "invalid"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let preferences = Preferences::load_at(&path);
    assert!(preferences.trigger_for(HotkeyTarget::Capture).is_none());
    for target in [
        HotkeyTarget::PinToggleGroup,
        HotkeyTarget::PinNextGroup,
        HotkeyTarget::PinPreviousGroup,
    ] {
        assert!(preferences.trigger_for(target).is_none());
    }
    assert_eq!(
        preferences.hotkey_label_for(HotkeyTarget::Settings),
        "Alt+F9"
    );
    preferences.save_at(&path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["command_hotkeys"]["future.command"], future);
}

#[test]
fn command_hotkeys_conflict_with_legacy_and_other_new_commands() {
    let preferences = Preferences::default().with_trigger_change(
        HotkeyTarget::PinToggleGroup,
        Trigger::Hotkey(HotkeyTarget::Capture.default_spec()),
    );
    assert_eq!(
        preferences.hotkey_conflict_pair(),
        Some((HotkeyTarget::Capture, HotkeyTarget::PinToggleGroup))
    );
    let dir = TestDir::new("command-hotkeys-conflict");
    assert!(preferences.save_at(&dir.join("settings.json")).is_err());
    let first = InputTrigger::from_wire("key:14 + key:48").unwrap();
    let configured = Preferences::default()
        .with_trigger_change(HotkeyTarget::PinToggleGroup, Trigger::Input(first.clone()))
        .with_trigger_change(HotkeyTarget::PinNextGroup, Trigger::Input(first));
    assert_eq!(
        configured.hotkey_conflict_pair(),
        Some((HotkeyTarget::PinToggleGroup, HotkeyTarget::PinNextGroup))
    );
    let disabled = configured.with_hotkey_change(HotkeyTarget::PinNextGroup, 4, None);
    assert!(!disabled.hotkeys_conflict());
}

#[test]
fn command_hotkeys_loaded_conflict_does_not_disable_an_unrelated_legacy_binding() {
    let file = serde_json::json!({"command_hotkeys": {
        "pin.group.toggle_visibility":{"mode":3,"modifiers":2,"key":120},
        "app.settings":{"mode":3,"modifiers":2,"key":120}
    }});
    let preferences =
        crate::config_json::overlay(SettingsFile::default(), file.as_object().unwrap().clone())
            .preferences();
    assert!(preferences.hotkeys_conflict());
    assert_eq!(
        preferences.hotkey_label_for(HotkeyTarget::ClipboardPin),
        "Ctrl+2"
    );
}

#[test]
fn command_hotkeys_atomic_save_merges_latest_fields_and_preserves_unreadable_secret() {
    let dir = TestDir::new("command-hotkeys-merge");
    let path = dir.join("settings.json");
    let stale = Preferences::default().with_trigger_change(
        HotkeyTarget::PinToggleGroup,
        Trigger::Hotkey(HotkeySpec::from_key_name("F9", ALT).unwrap()),
    );
    let latest = serde_json::json!({
        "theme":"dark", "history":true,
        "translate_api_key_encrypted":"dpapi-v1:00",
        "command_hotkeys": {
            "future.command":{"version":7},
            "app.settings":{"mode":3,"modifiers":2,"key":121}
        }
    });
    std::fs::write(&path, serde_json::to_vec(&latest).unwrap()).unwrap();
    let merged = stale
        .save_hotkey_change_at(HotkeyTarget::PinToggleGroup, &path)
        .unwrap();
    assert!(merged.hotkey_matches(HotkeyTarget::PinToggleGroup, &stale));
    assert_eq!(merged.theme, ThemeMode::Dark);
    assert!(merged.history);
    assert_eq!(merged.hotkey_label_for(HotkeyTarget::Settings), "Alt+F10");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved["translate_api_key_encrypted"],
        latest["translate_api_key_encrypted"]
    );
    assert_eq!(
        saved["command_hotkeys"]["future.command"],
        latest["command_hotkeys"]["future.command"]
    );
}

#[test]
fn command_hotkeys_atomic_rollback_does_not_replace_newer_request_or_other_settings() {
    let dir = TestDir::new("command-hotkeys-cas");
    let path = dir.join("settings.json");
    let original = Preferences::default();
    let requested = original.with_trigger_change(
        HotkeyTarget::PinToggleGroup,
        Trigger::Hotkey(HotkeySpec::from_key_name("F9", ALT).unwrap()),
    );
    requested
        .save_hotkey_change_at(HotkeyTarget::PinToggleGroup, &path)
        .unwrap();
    let newer = original.with_trigger_change(
        HotkeyTarget::PinToggleGroup,
        Trigger::Hotkey(HotkeySpec::from_key_name("F10", ALT).unwrap()),
    );
    newer
        .save_hotkey_change_at(HotkeyTarget::PinToggleGroup, &path)
        .unwrap();
    Preferences::revert_hotkeys_at(
        &requested,
        &original,
        &[HotkeyTarget::PinToggleGroup],
        &path,
    )
    .unwrap();
    let kept = Preferences::load_at(&path);
    assert!(kept.hotkey_matches(HotkeyTarget::PinToggleGroup, &newer));
    Preferences::revert_hotkeys_at(&newer, &original, &[HotkeyTarget::PinToggleGroup], &path)
        .unwrap();
    assert!(Preferences::load_at(&path).hotkey_matches(HotkeyTarget::PinToggleGroup, &original));
}

#[test]
fn command_hotkeys_atomic_save_checks_conflicts_against_latest_binding() {
    let dir = TestDir::new("command-hotkeys-latest-conflict");
    let path = dir.join("settings.json");
    let trigger = Trigger::Hotkey(HotkeySpec::from_key_name("F9", ALT).unwrap());
    let stale =
        Preferences::default().with_trigger_change(HotkeyTarget::PinToggleGroup, trigger.clone());
    let latest = Preferences::default().with_trigger_change(HotkeyTarget::Settings, trigger);
    latest.save_at(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(stale
        .save_hotkey_change_at(HotkeyTarget::PinToggleGroup, &path)
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
