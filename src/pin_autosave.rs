//! 内容快照只合并元数据；保存像素、重试与关闭仍由宿主负责。
use std::time::{Duration, Instant};

pub(crate) const QUIET: Duration = Duration::from_millis(750);
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotDecision {
    Clean,
    Initial,
    Deferred,
    Quiet,
    MaximumDelay,
}
impl SnapshotDecision {
    pub(crate) fn due(self) -> bool {
        matches!(self, Self::Initial | Self::Quiet | Self::MaximumDelay)
    }
}

/// K 必须是内容身份等小元数据，不能持有图片或编码闭包。
pub(crate) struct ContentSchedule<K> {
    observed: Option<K>,
    last_change: Option<Instant>,
    first_dirty: Option<Instant>,
}
impl<K> Default for ContentSchedule<K> {
    fn default() -> Self {
        Self {
            observed: None,
            last_change: None,
            first_dirty: None,
        }
    }
}
impl<K: Clone + PartialEq> ContentSchedule<K> {
    /// 只清除本次已保存的时钟；编码期间已经观察到的新内容继续计龄。
    pub(crate) fn acknowledge(&mut self, content: &K) {
        if self.observed.as_ref() == Some(content) {
            self.last_change = None;
            self.first_dirty = None;
        }
    }
    /// 活动期间不观察被处理占位图；静置期限从最后一次活动重新计算。
    pub(crate) fn activity(&mut self, now: Instant) {
        self.last_change = Some(now);
    }
    pub(crate) fn observe(
        &mut self,
        content: &K,
        saved: Option<&K>,
        now: Instant,
    ) -> SnapshotDecision {
        if saved == Some(content) {
            self.observed = Some(content.clone());
            self.last_change = None;
            self.first_dirty = None;
            return SnapshotDecision::Clean;
        }
        if self.observed.as_ref() != Some(content) {
            self.observed = Some(content.clone());
            self.last_change = Some(now);
        }
        let first = *self.first_dirty.get_or_insert(now);
        let last = *self.last_change.get_or_insert(now);
        if saved.is_none() {
            SnapshotDecision::Initial
        } else if now.saturating_duration_since(last) >= QUIET {
            SnapshotDecision::Quiet
        } else if now.saturating_duration_since(first) >= MAX_DELAY {
            // 长时间连续调整仍定期保存；只有宿主已停止处理/草稿时才调用本方法。
            SnapshotDecision::MaximumDelay
        } else {
            SnapshotDecision::Deferred
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_autosave_idle_quiet_requires_latest_content() {
        let start = Instant::now();
        let mut schedule = ContentSchedule::default();
        assert_eq!(
            schedule.observe(&2, Some(&1), start),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&2, Some(&1), start + QUIET - Duration::from_millis(1)),
            SnapshotDecision::Deferred
        );
        let changed = start + QUIET;
        assert_eq!(
            schedule.observe(&3, Some(&1), changed),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&3, Some(&1), changed + Duration::from_millis(500)),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&3, Some(&1), changed + QUIET),
            SnapshotDecision::Quiet
        );
        assert_eq!(
            schedule.observe(&3, Some(&3), changed + QUIET),
            SnapshotDecision::Clean
        );
    }

    #[test]
    fn pin_autosave_idle_maximum_wait_bounds_continuous_changes() {
        let start = Instant::now();
        let mut schedule = ContentSchedule::default();
        for n in 0..6 {
            assert_eq!(
                schedule.observe(&(n + 2), Some(&1), start + Duration::from_millis(n * 500)),
                SnapshotDecision::Deferred
            );
        }
        assert_eq!(
            schedule.observe(&8, Some(&1), start + MAX_DELAY),
            SnapshotDecision::MaximumDelay
        );
        assert_eq!(
            schedule.observe(&8, Some(&8), start + MAX_DELAY),
            SnapshotDecision::Clean
        );
        assert_eq!(
            schedule.observe(&9, Some(&8), start + MAX_DELAY),
            SnapshotDecision::Deferred
        );
    }

    #[test]
    fn pin_autosave_idle_saved_ack_and_retry_preserve_pending_clock() {
        let start = Instant::now();
        let mut schedule = ContentSchedule::default();
        assert_eq!(
            schedule.observe(&2, Some(&1), start),
            SnapshotDecision::Deferred
        );
        let latest = start + Duration::from_millis(400);
        assert_eq!(
            schedule.observe(&3, Some(&1), latest),
            SnapshotDecision::Deferred
        );
        // 旧快照确认/重试冷却不得将新内容时钟归零；重试是否许可由宿主单独决定。
        schedule.acknowledge(&2);
        assert_eq!(
            schedule.observe(&3, Some(&2), latest + QUIET - Duration::from_millis(1)),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&3, Some(&2), latest + QUIET),
            SnapshotDecision::Quiet
        );
        assert_eq!(
            schedule.observe(&3, Some(&2), latest + QUIET + Duration::from_secs(1)),
            SnapshotDecision::Quiet
        );
        // 慢编码成功后立刻开始下一批修改，不得继承已保存内容的 MAX 时钟。
        let acknowledged = start + Duration::from_secs(5);
        schedule.acknowledge(&3);
        assert_eq!(
            schedule.observe(&4, Some(&3), acknowledged),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(
                &4,
                Some(&3),
                acknowledged + QUIET - Duration::from_millis(1)
            ),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&4, Some(&3), acknowledged + QUIET),
            SnapshotDecision::Quiet
        );
    }

    #[test]
    fn pin_autosave_idle_initial_save_and_active_gesture_defer_content() {
        let start = Instant::now();
        let mut schedule = ContentSchedule::default();
        assert_eq!(schedule.observe(&1, None, start), SnapshotDecision::Initial);
        assert_eq!(
            schedule.observe(&1, Some(&1), start),
            SnapshotDecision::Clean
        );
        assert_eq!(
            schedule.observe(&2, Some(&1), start),
            SnapshotDecision::Deferred
        );
        schedule.activity(start + Duration::from_millis(700));
        assert_eq!(
            schedule.observe(&2, Some(&1), start + QUIET),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&2, Some(&1), start + Duration::from_millis(700) + QUIET),
            SnapshotDecision::Quiet
        );
    }
}
