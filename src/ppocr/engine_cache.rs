//! 按明确 profile 隔离引擎；默认空缓存，任何替换仅发布到本次选择的槽。
use super::OcrProfile;
use crate::{recognition::task::TaskToken, Result};

pub(super) struct EngineCache<T> {
    tiny: Option<T>,
    small: Option<T>,
}

impl<T> Default for EngineCache<T> {
    fn default() -> Self {
        Self {
            tiny: None,
            small: None,
        }
    }
}

impl<T> EngineCache<T> {
    pub(super) fn entry(&self, profile: OcrProfile) -> &Option<T> {
        match profile {
            OcrProfile::Tiny => &self.tiny,
            OcrProfile::Small => &self.small,
        }
    }

    pub(super) fn entry_mut(&mut self, profile: OcrProfile) -> &mut Option<T> {
        match profile {
            OcrProfile::Tiny => &mut self.tiny,
            OcrProfile::Small => &mut self.small,
        }
    }

    pub(super) fn publish_cancellable(
        &mut self,
        profile: OcrProfile,
        candidate: T,
        task: &TaskToken,
    ) -> Result<()> {
        // 候选构造后取消，候选由 RAII 释放；已有任一 profile 的成功值保持。
        task.checkpoint()?;
        *self.entry_mut(profile) = Some(candidate);
        Ok(())
    }
}

#[cfg(test)]
#[path = "engine_cache_tests.rs"]
mod tests;
