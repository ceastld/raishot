use super::*;

fn ready(source: HistorySource) -> SpaceContext {
    SpaceContext {
        source,
        image_preview_enabled: true,
        text_input_focused: false,
        composing: false,
    }
}

#[test]
fn image_quick_preview_space_routes_shots_and_keeps_recording_action() {
    let routes = [
        (HistorySource::Shot, Some(SpaceAction::ViewImage)),
        (HistorySource::Recording, Some(SpaceAction::ToggleRecording)),
        (HistorySource::Other, None),
    ];
    for (source, expected) in routes {
        assert_eq!(
            decide("space", Modifiers::default(), false, ready(source)),
            expected
        );
    }
    assert_eq!(
        decide(
            "space",
            Modifiers::default(),
            false,
            SpaceContext {
                image_preview_enabled: false,
                ..ready(HistorySource::Shot)
            },
        ),
        None
    );
    assert_eq!(
        decide(
            "space",
            Modifiers::default(),
            false,
            SpaceContext {
                image_preview_enabled: false,
                ..ready(HistorySource::Recording)
            },
        ),
        Some(SpaceAction::ToggleRecording)
    );
}

#[test]
fn image_quick_preview_does_not_take_typing_or_ime_input() {
    for source in [HistorySource::Shot, HistorySource::Recording] {
        let contexts = [
            SpaceContext {
                text_input_focused: true,
                ..ready(source)
            },
            // 候选窗口仍活动时，即使文本控件已暂时失焦也不能打开预览。
            SpaceContext {
                composing: true,
                ..ready(source)
            },
        ];
        for context in contexts {
            assert_eq!(decide("space", Modifiers::default(), false, context), None);
        }
    }
}

#[test]
fn image_quick_preview_leaves_every_modified_space_chord_to_its_owner() {
    for mask in 1..32 {
        let modifiers = Modifiers {
            control: mask & 1 != 0,
            alt: mask & 2 != 0,
            shift: mask & 4 != 0,
            platform: mask & 8 != 0,
            function: mask & 16 != 0,
        };
        for source in [HistorySource::Shot, HistorySource::Recording] {
            assert_eq!(
                decide("space", modifiers, false, ready(source)),
                None,
                "modifier mask {mask}"
            );
        }
    }
}

#[test]
fn image_quick_preview_ignores_unrelated_and_held_keys_without_a_release_session() {
    for source in [HistorySource::Shot, HistorySource::Recording] {
        for key in ["escape", "enter", "f11", "o"] {
            assert_eq!(
                decide(key, Modifiers::default(), false, ready(source)),
                None
            );
        }
        let first = decide("space", Modifiers::default(), false, ready(source));
        assert!(first.is_some());
        assert_eq!(
            decide("space", Modifiers::default(), true, ready(source)),
            None
        );
        // 新的非重复按下可以再次请求；纯叶不持有依赖 key-up 的预览会话。
        assert_eq!(
            decide("space", Modifiers::default(), false, ready(source)),
            first
        );
    }
}
