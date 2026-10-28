//! 最小化的播放意图状态；恢复票据只属于进入最小化时的同一来源。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Pause,
    Resume,
}

#[derive(Debug, Default)]
pub(super) struct MinimizeState {
    minimized: bool,
    resume_generation: Option<u64>,
}

impl MinimizeState {
    /// 只记录最小化切换，不处理失焦；持续最小化的播放请求不能重建恢复票据。
    pub(super) fn update(
        &mut self,
        minimized: bool,
        source_generation: u64,
        playing: bool,
    ) -> Action {
        if self
            .resume_generation
            .is_some_and(|generation| generation != source_generation)
        {
            self.resume_generation = None;
        }

        let entering = minimized && !self.minimized;
        self.minimized = minimized;
        if minimized {
            if entering && playing {
                self.resume_generation = Some(source_generation);
            }
            return if playing { Action::Pause } else { Action::None };
        }

        let resume = self.resume_generation.take() == Some(source_generation);
        if resume && !playing {
            Action::Resume
        } else {
            Action::None
        }
    }

    pub(super) fn is_minimized(&self) -> bool {
        self.minimized
    }

    /// 显式播放意图或切源撤销自动恢复，继续保留已观察到的最小化状态。
    pub(super) fn cancel_resume(&mut self) {
        self.resume_generation = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playing_entry_pauses_and_restores_only_once() {
        let mut state = MinimizeState::default();
        assert!(!state.is_minimized());
        assert_eq!(state.update(false, 7, true), Action::None);
        assert_eq!(state.update(true, 7, true), Action::Pause);
        assert!(state.is_minimized());
        assert_eq!(state.update(true, 7, false), Action::None);
        assert_eq!(state.update(true, 7, false), Action::None);
        assert_eq!(state.update(false, 7, false), Action::Resume);
        assert!(!state.is_minimized());
        assert_eq!(state.update(false, 7, false), Action::None);
    }

    #[test]
    fn paused_entry_never_arms_from_later_minimized_playback() {
        let mut state = MinimizeState::default();
        assert_eq!(state.update(true, 1, false), Action::None);
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(true, 1, false), Action::None);
        assert_eq!(state.update(false, 1, false), Action::None);
    }

    #[test]
    fn source_changes_discard_the_old_ticket_even_when_switching_back() {
        let mut state = MinimizeState::default();
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(true, 2, true), Action::Pause);
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(false, 1, false), Action::None);

        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(false, 2, false), Action::None);
    }

    #[test]
    fn cancel_keeps_minimized_without_rearming_until_a_new_entry() {
        let mut state = MinimizeState::default();
        assert_eq!(state.update(true, 1, true), Action::Pause);
        state.cancel_resume();
        assert!(state.is_minimized());
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(false, 1, false), Action::None);

        assert_eq!(state.update(true, 2, true), Action::Pause);
        assert_eq!(state.update(false, 2, false), Action::Resume);
    }

    #[test]
    fn already_playing_restore_consumes_the_ticket_without_replaying() {
        let mut state = MinimizeState::default();
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(false, 1, true), Action::None);
        assert_eq!(state.update(false, 1, false), Action::None);
        assert_eq!(state.update(true, 1, true), Action::Pause);
        assert_eq!(state.update(false, 1, false), Action::Resume);
    }
}
