//! 快捷动作的有界会话状态；副作用成功后才前进，失败重试不重复已完成步骤。
use crate::quick_action::{HistoryPolicy, QuickRecipe, Step};

#[derive(Default)]
pub(crate) struct QuickActionFlow {
    pub recipe: Option<QuickRecipe>,
    index: usize,
    automatic_started: bool,
    running: bool,
    history_written: bool,
    complete: bool,
}

impl QuickActionFlow {
    pub fn install(&mut self, recipe: QuickRecipe) {
        *self = Self {
            recipe: Some(recipe),
            ..Default::default()
        };
    }
    pub fn start(&mut self, automatic: bool, ready: bool) -> bool {
        let Some(recipe) = &self.recipe else {
            return false;
        };
        if !ready
            || self.running
            || self.complete
            || (automatic && (recipe.edit || self.automatic_started))
        {
            return false;
        }
        self.automatic_started = true;
        self.running = true;
        true
    }
    pub fn current(&self) -> Option<Step> {
        self.running
            .then(|| {
                self.recipe
                    .as_ref()?
                    .steps
                    .get(self.index)
                    .copied()
                    .flatten()
            })
            .flatten()
    }
    pub fn expected(&self) -> Option<Step> {
        self.recipe
            .as_ref()?
            .steps
            .get(self.index)
            .copied()
            .flatten()
    }
    pub fn succeeded(&mut self) {
        if self.expected().is_some() {
            self.index += 1;
        }
    }
    pub fn complete(&mut self) {
        self.running = false;
        self.complete = true;
    }
    pub fn fail(&mut self) {
        self.running = false;
    }
    pub fn running(&self) -> bool {
        self.running
    }
    pub fn completed_steps(&self) -> usize {
        self.index
    }
    pub fn record_history(&self) -> bool {
        !self.history_written
            && self
                .recipe
                .as_ref()
                .is_none_or(|r| r.history == HistoryPolicy::Inherit)
    }
    pub fn history_succeeded(&mut self) {
        self.history_written = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quick_action::Source;
    fn recipe(edit: bool) -> QuickRecipe {
        QuickRecipe {
            source: Source::Region,
            edit,
            steps: [Some(Step::QuickSave), Some(Step::OcrCopy), None],
            history: HistoryPolicy::Inherit,
        }
    }
    #[test]
    fn quick_action_flow_waits_for_selection_and_automatic_execution_is_once() {
        let mut flow = QuickActionFlow::default();
        flow.install(recipe(false));
        assert!(!flow.start(true, false));
        assert!(flow.start(true, true));
        assert!(!flow.start(true, true));
        flow.fail();
        assert!(!flow.start(true, true));
        assert!(flow.start(false, true));
    }
    #[test]
    fn quick_action_flow_retry_retains_successful_side_effects_and_history() {
        let mut flow = QuickActionFlow::default();
        flow.install(recipe(false));
        flow.start(true, true);
        flow.history_succeeded();
        flow.succeeded();
        flow.fail();
        assert!(flow.start(false, true));
        assert_eq!(flow.current(), Some(Step::OcrCopy));
        assert!(!flow.record_history());
        flow.succeeded();
        assert!(flow.current().is_none());
        flow.fail();
        assert!(flow.start(false, true));
        flow.complete();
        assert!(!flow.start(false, true));
    }
    #[test]
    fn quick_action_flow_edit_requires_confirmation_and_never_skips_history_policy() {
        let mut flow = QuickActionFlow::default();
        let mut r = recipe(true);
        r.history = HistoryPolicy::Never;
        flow.install(r);
        assert!(!flow.start(true, true));
        assert!(flow.start(false, true));
        assert!(!flow.record_history());
        flow.fail();
        assert_eq!(flow.completed_steps(), 0);
    }
}
