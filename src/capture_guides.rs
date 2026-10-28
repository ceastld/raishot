//! 普通截图裸 Alt 松开开关；快照、组合输入和失活不能伪造一次按键。
use gpui::{ModifierKey, ModifierKeyPhase, ModifiersChangedEvent};

#[derive(Default)]
pub(crate) struct GuideToggleGesture {
    pending: Option<ModifierKey>,
    alt_held: bool,
}

impl GuideToggleGesture {
    pub(crate) fn reset(&mut self) {
        self.pending = None;
    }

    pub(crate) fn reset_tracking(&mut self) {
        self.reset();
        self.alt_held = false;
    }

    pub(crate) fn observe(&mut self, event: &ModifiersChangedEvent, allowed: bool) -> bool {
        let previously_held = self.alt_held;
        self.alt_held = event.modifiers.alt;
        let Some(key) = event.physical_key else {
            self.reset();
            return false;
        };
        if !allowed || key.key == ModifierKey::Other {
            self.reset();
            return false;
        }
        match key.phase {
            ModifierKeyPhase::Down => {
                if !event.modifiers.alt || event.modifiers.number_of_modifiers() != 1 {
                    self.reset();
                } else if key.is_repeat {
                    if self.pending != Some(key.key) {
                        self.reset();
                    }
                } else {
                    // 同侧重复 Down、双 Alt 和进入截图前已 held 的快照都不能重武装。
                    self.pending = (!previously_held).then_some(key.key);
                }
                false
            }
            ModifierKeyPhase::Up => {
                let toggle = self.pending == Some(key.key) && !event.modifiers.modified();
                self.reset();
                toggle
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ModifierKeyEvent, Modifiers};

    fn event(
        key: ModifierKey,
        phase: ModifierKeyPhase,
        repeat: bool,
        modifiers: Modifiers,
    ) -> ModifiersChangedEvent {
        ModifiersChangedEvent {
            modifiers,
            physical_key: Some(ModifierKeyEvent {
                key,
                phase,
                is_repeat: repeat,
            }),
            ..Default::default()
        }
    }

    fn down(key: ModifierKey, repeat: bool) -> ModifiersChangedEvent {
        event(
            key,
            ModifierKeyPhase::Down,
            repeat,
            Modifiers {
                alt: true,
                ..Default::default()
            },
        )
    }

    fn up(key: ModifierKey) -> ModifiersChangedEvent {
        event(key, ModifierKeyPhase::Up, false, Modifiers::default())
    }

    #[test]
    fn both_alt_sides_toggle_session_only_once_after_repeats_and_again_on_a_fresh_tap() {
        for side in [ModifierKey::LeftAlt, ModifierKey::RightAlt] {
            let mut gesture = GuideToggleGesture::default();
            let mut guides = false;
            for expected in [true, false] {
                assert!(!gesture.observe(&down(side, false), true));
                assert!(!gesture.observe(&down(side, true), true));
                assert!(!gesture.observe(&down(side, true), true));
                if gesture.observe(&up(side), true) {
                    guides = !guides;
                }
                assert_eq!(guides, expected);
                assert!(!gesture.observe(&up(side), true));
            }
        }
    }

    #[test]
    fn chords_and_second_alt_cannot_rearm_when_the_other_key_is_released_first() {
        let mut gesture = GuideToggleGesture::default();
        for other in [ModifierKey::Other, ModifierKey::RightAlt] {
            assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
            let modifiers = Modifiers {
                alt: true,
                shift: other == ModifierKey::Other,
                ..Default::default()
            };
            assert!(!gesture.observe(
                &event(other, ModifierKeyPhase::Down, false, modifiers),
                true
            ));
            assert!(!gesture.observe(
                &event(
                    other,
                    ModifierKeyPhase::Up,
                    false,
                    Modifiers {
                        alt: true,
                        ..Default::default()
                    }
                ),
                true
            ));
            assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        }
        for modifier in [
            Modifiers {
                alt: true,
                control: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                platform: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                function: true,
                ..Default::default()
            },
        ] {
            assert!(!gesture.observe(
                &event(
                    ModifierKey::LeftAlt,
                    ModifierKeyPhase::Down,
                    false,
                    modifier
                ),
                true
            ));
            assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        }
    }

    #[test]
    fn snapshots_and_preheld_alt_never_arm_and_cannot_continue_an_older_tap() {
        let mut gesture = GuideToggleGesture::default();
        let snapshot = ModifiersChangedEvent {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!gesture.observe(&snapshot, true));
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, true), true));
        assert!(!gesture.observe(&up(ModifierKey::RightAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        assert!(!gesture.observe(&snapshot, true));
        assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        assert!(gesture.observe(&up(ModifierKey::LeftAlt), true));
    }

    #[test]
    fn occupied_down_or_up_cancels_the_cycle_without_toggling_after_input_finishes() {
        let mut gesture = GuideToggleGesture::default();
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), false));
        assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, false), true));
        assert!(!gesture.observe(&up(ModifierKey::RightAlt), false));
        assert!(!gesture.observe(&up(ModifierKey::RightAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, false), true));
        assert!(gesture.observe(&up(ModifierKey::RightAlt), true));
    }

    #[test]
    fn reset_after_child_key_toolbar_or_deactivation_does_not_rearm_a_held_cycle() {
        let mut gesture = GuideToggleGesture::default();
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        gesture.reset();
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, true), true));
        assert!(!gesture.observe(&up(ModifierKey::LeftAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, false), true));
        gesture.reset();
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, false), true));
        assert!(!gesture.observe(&up(ModifierKey::RightAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, false), true));
        assert!(gesture.observe(&up(ModifierKey::RightAlt), true));
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        gesture.reset_tracking();
        // 失焦期间的 Up 可以未送达；重新聚焦后的实际新 Down 必须正常开始。
        assert!(!gesture.observe(&down(ModifierKey::LeftAlt, false), true));
        assert!(gesture.observe(&up(ModifierKey::LeftAlt), true));
        gesture.reset_tracking();
        assert!(!gesture.observe(&down(ModifierKey::RightAlt, true), true));
        assert!(!gesture.observe(&up(ModifierKey::RightAlt), true));
    }
}
