use crate::{
    hotkey::{HotkeySpec, CTRL},
    quick_action::{
        builtin_action, builtin_actions, hotkey_conflicts, ActionError, HistoryPolicy, HotkeyOwner,
        QuickRecipe, RecipeError, Source, Step,
    },
    quick_action_store::QuickActionStore,
};

#[test]
fn quick_action_packed_round_trip_all_fields() {
    for source in Source::ALL {
        for first in Step::ALL {
            for edit in [false, true] {
                for history in [HistoryPolicy::Inherit, HistoryPolicy::Never] {
                    let recipe = QuickRecipe {
                        source,
                        edit,
                        steps: [Some(first), None, None],
                        history,
                    };
                    assert_eq!(
                        QuickRecipe::unpack(recipe.pack().unwrap()),
                        Ok(Some(recipe))
                    );
                }
            }
        }
    }
    let recipe = QuickRecipe {
        source: Source::ActiveWindow,
        edit: false,
        steps: [
            Some(Step::QuickSave),
            Some(Step::CopyImage),
            Some(Step::Pin),
        ],
        history: HistoryPolicy::Never,
    };
    assert_eq!(recipe.pack().unwrap(), 0xa501_2134);
    assert_eq!(QuickRecipe::parse_code("0xA5012134"), Ok(Some(recipe)));
    assert_eq!(
        QuickRecipe::parse_code(&recipe.pack().unwrap().to_string()),
        Ok(Some(recipe))
    );
    assert_eq!(QuickRecipe::unpack(0), Ok(None));
}

#[test]
fn quick_action_rejects_malformed_and_incompatible_recipes() {
    assert_eq!(
        QuickRecipe::unpack(0x1100_0010),
        Err(RecipeError::InvalidMagic)
    );
    assert_eq!(
        QuickRecipe::unpack(0xa502_0010),
        Err(RecipeError::ReservedBits)
    );
    assert_eq!(
        QuickRecipe::unpack(0xa500_0017),
        Err(RecipeError::UnknownSource)
    );
    assert_eq!(
        QuickRecipe::unpack(0xa500_0090),
        Err(RecipeError::UnknownStep)
    );
    assert_eq!(QuickRecipe::unpack(0xa500_0000), Err(RecipeError::Empty));
    assert_eq!(QuickRecipe::unpack(0xa500_0100), Err(RecipeError::StepGap));
    assert_eq!(
        QuickRecipe::unpack(0xa500_0410),
        Err(RecipeError::ClipboardConflict)
    );
    assert_eq!(
        QuickRecipe::unpack(0xa500_0320),
        Err(RecipeError::TerminalNotLast)
    );
    assert_eq!(
        QuickRecipe::parse_code("0x100000000"),
        Err(RecipeError::InvalidNumber)
    );
    for first in Step::ALL.into_iter().filter(|step| step.writes_clipboard()) {
        for second in Step::ALL.into_iter().filter(|step| step.writes_clipboard()) {
            let recipe = QuickRecipe {
                steps: [Some(first), Some(second), None],
                ..QuickRecipe::default()
            };
            assert_eq!(recipe.validate(), Err(RecipeError::ClipboardConflict));
        }
    }
}

#[test]
fn quick_action_builtin_names_and_text_history() {
    let store = QuickActionStore::default();
    assert_eq!(store.actions.len(), 13);
    for action in &store.actions {
        action.validate().unwrap();
        assert!(action.hotkey.is_none());
    }
    assert!(store.resolve("capture").unwrap().edit);
    assert!(!store.resolve("copy").unwrap().edit);
    assert_eq!(
        store.resolve("copy-pin").unwrap().steps,
        [Some(Step::CopyImage), Some(Step::Pin), None]
    );
    for id in ["ocr-copy", "table-copy", "qr-copy", "qr-open"] {
        assert_eq!(store.resolve(id).unwrap().history, HistoryPolicy::Never);
    }
    assert_eq!(
        store.resolve("typo"),
        Err(ActionError::Unknown("typo".into()))
    );
    assert_eq!(store.resolve("../../copy"), Err(ActionError::InvalidId));
    let store = QuickActionStore::parse_legacy_ini("[action.copy]\nenabled=false\n");
    assert_eq!(
        store.resolve("copy"),
        Err(ActionError::Disabled("copy".into()))
    );
}

#[test]
fn quick_action_store_only_saves_builtin_differences() {
    let mut store = QuickActionStore::default();
    assert_eq!(store.encode_legacy_ini().unwrap(), "version=1\n");
    store.find_mut("ocr-copy").unwrap().hotkey = Some(HotkeySpec {
        modifiers: CTRL,
        key: b'3'.into(),
    });
    let mut custom = builtin_action("copy-pin").unwrap();
    custom.id = "save-copy-pin".into();
    custom.name = "保存复制贴图".into();
    custom.recipe.steps = [
        Some(Step::QuickSave),
        Some(Step::CopyImage),
        Some(Step::Pin),
    ];
    store.actions.push(custom);
    let text = store.encode_legacy_ini().unwrap();
    assert!(text.contains("[action.ocr-copy]\nhotkey=1,51\n"));
    assert!(!text.contains("[action.capture]"));
    assert_eq!(QuickActionStore::parse_legacy_ini(&text), store);
}

#[test]
fn quick_action_store_skips_damaged_fields_without_losing_valid_overrides() {
    let store = QuickActionStore::parse_legacy_ini("version=1\n[action.copy]\nname=自定义复制\nrecipe=0xA5000090\nhotkey=17,51\ntray=false\nenabled=wrong\n[action.bad/path]\nname=污染名称\n[action.custom]\nname=自定义\nrecipe=broken\n[action.custom-ok]\nrecipe=0xA5000030\n[unrelated]\nenabled=false\n");
    let copy = store.find("copy").unwrap();
    assert_eq!(copy.name, "自定义复制");
    assert!(!copy.in_tray);
    assert!(copy.enabled);
    assert_eq!(copy.recipe, builtin_action("copy").unwrap().recipe);
    assert_eq!(copy.hotkey, None);
    assert!(store.find("custom").is_none());
    assert_eq!(
        store.find("custom-ok").unwrap().recipe.steps[0],
        Some(Step::QuickSave)
    );
    assert_eq!(
        QuickActionStore::parse_legacy_ini("version=2\n[action.copy]\nenabled=false"),
        QuickActionStore::default()
    );
    assert_eq!(
        QuickActionStore::parse_legacy_ini("settings-from-before-quick-actions=true"),
        QuickActionStore::default()
    );
    assert_eq!(
        QuickActionStore::parse_legacy_ini(&" ".repeat(65 * 1024)),
        QuickActionStore::default()
    );
}

#[test]
fn quick_action_hotkey_conflict_set_includes_capture_pin_and_actions() {
    let spec = HotkeySpec {
        modifiers: CTRL,
        key: b'1'.into(),
    };
    let mut actions = builtin_actions();
    actions[1].hotkey = Some(spec);
    actions[2].hotkey = Some(spec);
    actions[2].enabled = false;
    let conflicts = hotkey_conflicts(&actions, Some(spec), Some(spec));
    assert_eq!(conflicts.len(), 3);
    assert_eq!(conflicts[0].first, HotkeyOwner::Capture);
    assert_eq!(conflicts[0].second, HotkeyOwner::ClipboardPin);
    assert_eq!(conflicts[1].second, HotkeyOwner::Action("copy".into()));
}

#[test]
fn quick_action_store_rejects_configuration_injection_and_duplicate_ids() {
    let mut store = QuickActionStore::default();
    store.actions[0].name = "截图\n[action.copy]\nenabled=false".into();
    assert_eq!(store.encode_legacy_ini(), Err(ActionError::InvalidName));
    store = QuickActionStore::default();
    store.actions.push(store.actions[0].clone());
    assert_eq!(
        store.encode_legacy_ini(),
        Err(ActionError::DuplicateId("capture".into()))
    );
}
