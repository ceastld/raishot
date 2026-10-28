//! 自动选区展示与逻辑路径分开；中键确认另读取已经实际 Present 的短动画范围。
use crate::{control_selection::ControlTarget, geometry::Rect};
use std::time::{Duration, Instant};

pub(crate) const SETTLE_TIME: Duration = Duration::from_millis(150);
const TRANSITION_TIME: Duration = Duration::from_millis(32);
/// 自动吸附：小范围切换与大范围（窗口↔整页）切换的过渡时长。
const AUTO_SMALL_TRANSITION: Duration = Duration::from_millis(100);
const AUTO_LARGE_TRANSITION: Duration = Duration::from_millis(140);
/// 同窗口查询未到时短暂保留旧的真实范围，避免细元素之间反复闪回窗口。
const AUTO_HOLD: Duration = Duration::from_millis(1000);
/// 面积放大到这一倍数以上算尺度跳变。
const AUTO_EXPAND_RATIO: u64 = 4;
/// 旧点查询得到的放大候选：二次确认、鼠标停住或等满上限即采用，不屏蔽大范围。
const AUTO_EXPAND_STILL: Duration = Duration::from_millis(32);
const AUTO_EXPAND_MAX_WAIT: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct FeedbackCandidate {
    pub(crate) target: ControlTarget,
    pub(crate) window: usize,
    /// 冻结桌面已知的窗口几何，可先于 UIA 细化展示，不代表查询完成。
    pub(crate) window_fallback: bool,
    pub(crate) refined: bool,
    pub(crate) terminal: bool,
    pub(crate) complete: bool,
    /// 当前点完整查询到的最深元素；容器空白处的最深元素也是真实范围，不要求叶控件类型。
    pub(crate) leaf: bool,
    pub(crate) navigated: bool,
    /// 已知路径在新点的临时范围；只改变展示，不把它冒称用户导航或完成查询。
    pub(crate) retained_for_query: bool,
    /// 结果查询时的画布坐标；None 表示就是当前点（缓存命中等）。
    pub(crate) sample: Option<(i32, i32)>,
}

struct Transition {
    from: Rect,
    to: Rect,
    started: Instant,
    duration: Duration,
}

/// 旧点结果带来的放大候选，等待二次确认、鼠标停住或等待上限。
struct PendingExpansion {
    window: usize,
    rect: Rect,
    sample: Option<(i32, i32)>,
    since: Instant,
}

impl Transition {
    fn sample(&self, now: Instant) -> Rect {
        let t = (now.saturating_duration_since(self.started).as_secs_f64()
            / self.duration.as_secs_f64().max(f64::EPSILON))
        .clamp(0., 1.);
        let eased = 1. - (1. - t).powi(3);
        let interpolate =
            |a: u32, b: u32| (a as f64 + (b as f64 - a as f64) * eased).round() as u32;
        let x = interpolate(self.from.x, self.to.x);
        let y = interpolate(self.from.y, self.to.y);
        let right = interpolate(self.from.x + self.from.width, self.to.x + self.to.width);
        let bottom = interpolate(self.from.y + self.from.height, self.to.y + self.to.height);
        Rect {
            x,
            y,
            width: right.saturating_sub(x).max(1),
            height: bottom.saturating_sub(y).max(1),
        }
    }

    fn active(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.duration
    }
}

pub(crate) struct ControlFeedback {
    pointer: Option<((i32, i32), Option<usize>)>,
    last_motion: Instant,
    shown: Option<FeedbackCandidate>,
    transition: Option<Transition>,
    pending: Option<PendingExpansion>,
    hold_since: Option<Instant>,
}

impl Default for ControlFeedback {
    fn default() -> Self {
        Self {
            pointer: None,
            last_motion: Instant::now(),
            shown: None,
            transition: None,
            pending: None,
            hold_since: None,
        }
    }
}

fn area(rect: Rect) -> u64 {
    rect.width as u64 * rect.height as u64
}

/// 大跳判定（物理像素²）：任一端很大，或并集远大于较小者。
fn large_jump(from: Rect, to: Rect) -> bool {
    let (a, b) = (area(from), area(to));
    let smaller = a.min(b);
    if smaller == 0 || a.max(b) >= 1_000_000 {
        return true;
    }
    let left = from.x.min(to.x) as u64;
    let top = from.y.min(to.y) as u64;
    let right = (from.x as u64 + from.width as u64).max(to.x as u64 + to.width as u64);
    let bottom = (from.y as u64 + from.height as u64).max(to.y as u64 + to.height as u64);
    let union = (right - left) * (bottom - top);
    union >= 640_000 && union * 2 >= smaller * 5
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x as u64 + outer.width as u64 >= inner.x as u64 + inner.width as u64
        && outer.y as u64 + outer.height as u64 >= inner.y as u64 + inner.height as u64
}

fn defer_expansion(old: &FeedbackCandidate, new: &FeedbackCandidate, point: (i32, i32)) -> bool {
    if new.navigated || new.retained_for_query {
        return false;
    }
    // 已完成的按钮/文字等终端在离开旧框时可以立即给出新元素。
    if new.complete && new.terminal && !old.target.rect.contains(point) {
        return false;
    }
    old.refined
        && (!new.refined
            || !new.complete
            || (old.window == new.window
                && old.target.rect != new.target.rect
                && contains_rect(new.target.rect, old.target.rect)))
}

impl ControlFeedback {
    pub(crate) fn observe_pointer(
        &mut self,
        point: (i32, i32),
        window: Option<usize>,
        now: Instant,
    ) {
        let sample = Some((point, window));
        if self.pointer != sample {
            self.last_motion = now;
            self.pointer = sample;
        }
    }

    pub(crate) fn settled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_motion) >= SETTLE_TIME
    }

    pub(crate) fn clear(&mut self) {
        self.shown = None;
        self.transition = None;
        self.pending = None;
        self.hold_since = None;
    }

    pub(crate) fn automatic_window_allowed(&self, window: Option<usize>, now: Instant) -> bool {
        window.is_some()
            && self.shown.as_ref().is_none_or(|shown| {
                Some(shown.window) != window
                    || !shown.refined
                    || self
                        .hold_since
                        .is_some_and(|since| now.saturating_duration_since(since) >= AUTO_HOLD)
            })
    }

    /// 每次进入窗口立即显示冻结范围，随后采用当前点完整查询或用户主动选择的范围。
    /// 同窗口等待新结果时短暂保留旧细范围，到期恢复窗口，避免移动时反复退回窗口。
    /// 旧点查询得到的放大结果先等确认，避免穿过空隙时大小框来回跳。
    pub(crate) fn update_automatic(
        &mut self,
        candidate: Option<FeedbackCandidate>,
        point: (i32, i32),
        window: Option<usize>,
        now: Instant,
    ) -> (Option<ControlTarget>, bool) {
        self.observe_pointer(point, window, now);
        let candidate = candidate.filter(|candidate| {
            window == Some(candidate.window)
                && candidate.target.rect.contains(point)
                && (candidate.window_fallback
                    || candidate.navigated
                    || candidate.complete && candidate.leaf && !candidate.retained_for_query)
        });
        let candidate = candidate.filter(|candidate| {
            !candidate.window_fallback || self.automatic_window_allowed(window, now)
        });
        match candidate {
            None => {
                self.pending = None;
                if self.shown.is_some() {
                    let since = *self.hold_since.get_or_insert(now);
                    if self.shown.as_ref().map(|shown| shown.window) != window
                        || now.saturating_duration_since(since) >= AUTO_HOLD
                    {
                        self.clear();
                    }
                }
            }
            Some(candidate) => {
                self.hold_since = None;
                self.accept_automatic(candidate, point, now);
            }
        }
        if self
            .transition
            .as_ref()
            .is_some_and(|transition| !transition.active(now))
        {
            self.transition = None;
        }
        // 只有过渡需要逐帧绘制；保留与待确认由 wake_at 定时唤醒一次。
        let active = self.transition.is_some();
        let shown = self.shown.as_ref().map(|shown| {
            let mut target = shown.target.clone();
            target.rect = self.visual_rect(now).expect("展示目标有矩形");
            target
        });
        (shown, active)
    }

    /// 窗口细化尚未完成时，点击与展示共用仍可短暂保留的旧细范围。
    pub(crate) fn retained_automatic_target(
        &self,
        point: (i32, i32),
        window: Option<usize>,
        now: Instant,
    ) -> Option<ControlTarget> {
        self.shown
            .as_ref()
            .filter(|shown| {
                !shown.window_fallback
                    && (shown.navigated
                        || shown.refined
                            && shown.complete
                            && shown.leaf
                            && !shown.retained_for_query)
                    && window == Some(shown.window)
                    && shown.target.rect.contains(point)
                    && self
                        .hold_since
                        .is_none_or(|since| now.saturating_duration_since(since) < AUTO_HOLD)
            })
            .map(|shown| shown.target.clone())
    }

    /// 保留旧框或放大待确认时，下一次需要重新判断的时刻。
    pub(crate) fn wake_at(&self) -> Option<Instant> {
        let hold = self.hold_since.map(|since| since + AUTO_HOLD);
        let pending = self.pending.as_ref().map(|pending| {
            (pending.since + AUTO_EXPAND_MAX_WAIT).min(self.last_motion + AUTO_EXPAND_STILL)
        });
        hold.into_iter().chain(pending).min()
    }

    fn accept_automatic(&mut self, candidate: FeedbackCandidate, point: (i32, i32), now: Instant) {
        if candidate.window_fallback
            || self
                .shown
                .as_ref()
                .is_some_and(|shown| shown.window_fallback || shown.window != candidate.window)
        {
            // 快速窗口与后续细结果直接交接，不等待旧点放大确认或残留动画。
            self.pending = None;
            self.transition = None;
            self.shown = Some(candidate);
            return;
        }
        let Some(shown) = &self.shown else {
            self.pending = None;
            self.transition = None;
            self.shown = Some(candidate);
            return;
        };
        if (shown.window, shown.target.rect) == (candidate.window, candidate.target.rect) {
            // 同一目标的重复结果不重启过渡。
            self.pending = None;
            self.shown = Some(candidate);
            return;
        }
        let expand = area(candidate.target.rect)
            >= area(shown.target.rect).saturating_mul(AUTO_EXPAND_RATIO);
        let current_point = candidate.sample.is_none_or(|sample| sample == point);
        if candidate.navigated || !expand || current_point {
            return self.retarget(candidate, now);
        }
        // 旧点结果的大范围：多半是穿过元素间空隙，新点的细结果马上会到。
        let confirmed = self.pending.as_ref().is_some_and(|pending| {
            pending.window == candidate.window
                && pending.rect == candidate.target.rect
                && (pending.sample != candidate.sample
                    || now.saturating_duration_since(pending.since) >= AUTO_EXPAND_MAX_WAIT
                    || now.saturating_duration_since(self.last_motion) >= AUTO_EXPAND_STILL)
        });
        if confirmed {
            self.retarget(candidate, now);
        } else if !self.pending.as_ref().is_some_and(|pending| {
            pending.window == candidate.window && pending.rect == candidate.target.rect
        }) {
            self.pending = Some(PendingExpansion {
                window: candidate.window,
                rect: candidate.target.rect,
                sample: candidate.sample,
                since: now,
            });
        }
    }

    fn retarget(&mut self, candidate: FeedbackCandidate, now: Instant) {
        let to = candidate.target.rect;
        // 自动跟随直接切换，避免整屏逐帧重绘拖慢跟手；只有滚轮层级选择做过渡，
        // 从当前可见矩形出发，在途过渡可被打断。
        self.transition = self
            .visual_rect(now)
            .filter(|from| candidate.navigated && *from != to)
            .map(|from| Transition {
                from,
                to,
                started: now,
                duration: if large_jump(from, to) {
                    AUTO_LARGE_TRANSITION
                } else {
                    AUTO_SMALL_TRANSITION
                },
            });
        self.pending = None;
        self.shown = Some(candidate);
    }

    /// 自动吸附关闭时保留窗口预览及手动导航的展示方式。
    pub(crate) fn update(
        &mut self,
        candidate: Option<FeedbackCandidate>,
        point: (i32, i32),
        window: Option<usize>,
        now: Instant,
    ) -> (Option<ControlTarget>, bool) {
        self.observe_pointer(point, window, now);
        let deferred = match (&self.shown, &candidate) {
            (Some(old), Some(new)) => !self.settled(now) && defer_expansion(old, new, point),
            (Some(old), None) => old.refined && !self.settled(now),
            _ => false,
        };
        if !deferred {
            let changed = self.shown.as_ref().map(|old| (old.window, old.target.rect))
                != candidate.as_ref().map(|new| (new.window, new.target.rect));
            if changed {
                let from = self.visual_rect(now);
                self.transition = from.zip(candidate.as_ref()).map(|(from, new)| Transition {
                    from,
                    to: new.target.rect,
                    started: now,
                    duration: TRANSITION_TIME,
                });
            }
            self.shown = candidate;
        }
        let active = self.transition.as_ref().is_some_and(|t| t.active(now));
        if !active {
            self.transition = None;
        }
        let shown = self.shown.as_ref().map(|shown| {
            let mut target = shown.target.clone();
            target.rect = self.visual_rect(now).expect("展示目标有矩形");
            target
        });
        // 只在32ms动画或150ms释放门期间请求绘制；不做永久帧循环。
        (shown, active || deferred)
    }

    fn visual_rect(&self, now: Instant) -> Option<Rect> {
        self.transition
            .as_ref()
            .map(|transition| transition.sample(now))
            .or_else(|| self.shown.as_ref().map(|shown| shown.target.rect))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(x: u32, width: u32, refined: bool) -> FeedbackCandidate {
        FeedbackCandidate {
            target: ControlTarget {
                rect: Rect {
                    x,
                    y: 10,
                    width,
                    height: 30,
                },
                title: String::new(),
            },
            window: 11,
            window_fallback: false,
            refined,
            terminal: refined,
            complete: true,
            leaf: refined,
            navigated: false,
            retained_for_query: false,
            sample: None,
        }
    }

    fn window_candidate() -> FeedbackCandidate {
        FeedbackCandidate {
            window_fallback: true,
            complete: false,
            sample: Some((1, 1)),
            ..candidate(0, 400, false)
        }
    }

    #[test]
    fn automatic_window_appears_immediately_without_waiting_for_provider() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let window = window_candidate();
        assert!(!window.refined && !window.complete && !window.terminal && !window.leaf);
        // 冷启动、慢查询与失败后的后续帧都保留窗口，不启动等待动画。
        for millis in [0, 80, 300, 1200, 2500] {
            let (shown, active) = feedback.update_automatic(
                Some(window.clone()),
                (20, 20),
                Some(11),
                now + Duration::from_millis(millis),
            );
            assert_eq!(shown.unwrap().rect, window.target.rect);
            assert!(!active && feedback.wake_at().is_none());
            assert!(feedback.automatic_window_allowed(Some(11), now));
            assert!(feedback.pending.is_none());
            assert!(feedback
                .retained_automatic_target((20, 20), Some(11), now)
                .is_none());
        }
        let fine = candidate(10, 30, true);
        let (shown, active) = feedback.update_automatic(
            Some(fine.clone()),
            (20, 20),
            Some(11),
            now + Duration::from_millis(2501),
        );
        assert_eq!(shown.unwrap().rect, fine.target.rect);
        assert!(!active && feedback.pending.is_none());
        assert!(!feedback.automatic_window_allowed(Some(11), now));
    }

    #[test]
    fn automatic_window_waits_only_while_querying_details_in_the_same_window() {
        let now = Instant::now();
        let fine = candidate(10, 30, true);
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(fine.clone()), (20, 20), Some(11), now);
        for (millis, point, window) in [(1, (80, 20), 11), (30, (100, 20), 11)] {
            let mut fallback = window_candidate();
            fallback.window = window;
            let at = now + Duration::from_millis(millis);
            let (shown, active) =
                feedback.update_automatic(Some(fallback), point, Some(window), at);
            assert_eq!(shown.unwrap().rect, fine.target.rect);
            assert!(!active && feedback.pending.is_none());
            assert!(!feedback.automatic_window_allowed(Some(window), at));
            assert_eq!(
                feedback.wake_at(),
                Some(now + Duration::from_millis(1) + AUTO_HOLD)
            );
            assert!(feedback
                .retained_automatic_target(point, Some(window), at)
                .is_none());
        }
        // 两个窗口重叠时，旧细框仍含当前点也不能挡住新窗口；点击在重绘前同样可用。
        let at = now + Duration::from_millis(60);
        let mut fallback = window_candidate();
        fallback.window = 12;
        assert!(feedback.automatic_window_allowed(Some(12), at));
        let (shown, active) =
            feedback.update_automatic(Some(fallback.clone()), (20, 20), Some(12), at);
        assert_eq!(shown.unwrap().rect, fallback.target.rect);
        assert_eq!(feedback.shown.as_ref().unwrap().window, 12);
        assert!(!active && feedback.pending.is_none() && feedback.wake_at().is_none());

        let mut peer = candidate(15, 20, true);
        peer.window = 12;
        let (shown, active) = feedback.update_automatic(
            Some(peer.clone()),
            (20, 20),
            Some(12),
            now + Duration::from_millis(80),
        );
        assert_eq!(shown.unwrap().rect, peer.target.rect);
        assert_eq!(feedback.shown.as_ref().unwrap().window, 12);
        assert!(!active && feedback.pending.is_none() && feedback.wake_at().is_none());

        // 小窗口 → 下层窗口细元素 → 回小窗口，多次往返都无需滚轮恢复。
        for millis in [81, 100, 120] {
            let at = now + Duration::from_millis(millis);
            assert!(feedback.automatic_window_allowed(Some(11), at));
            let (shown, active) =
                feedback.update_automatic(Some(window_candidate()), (20, 20), Some(11), at);
            assert_eq!(shown.unwrap().rect, window_candidate().target.rect);
            assert_eq!(feedback.shown.as_ref().unwrap().window, 11);
            assert!(!active && feedback.wake_at().is_none());
            feedback.update_automatic(Some(peer.clone()), (20, 20), Some(12), at);
        }
    }

    #[test]
    fn automatic_window_returns_after_hold_clear_or_leaving_all_windows() {
        let now = Instant::now();
        let hold_start = now + Duration::from_millis(1);
        let mut fine = candidate(10, 30, true);
        fine.navigated = true;
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(fine.clone()), (20, 20), Some(11), now);
        // 包括显式导航的真实细范围：窗口候选不能覆盖仍在鼠标下的范围。
        for elapsed in [0, 50, 500, 999] {
            let at = hold_start + Duration::from_millis(elapsed);
            let (shown, active) =
                feedback.update_automatic(Some(window_candidate()), (20, 20), Some(11), at);
            assert_eq!(shown.unwrap().rect, fine.target.rect);
            assert_eq!(
                feedback
                    .retained_automatic_target((20, 20), Some(11), at)
                    .unwrap()
                    .rect,
                fine.target.rect
            );
            assert!(!active);
            assert_eq!(feedback.wake_at(), Some(hold_start + AUTO_HOLD));
        }
        let expired = hold_start + AUTO_HOLD;
        assert!(feedback
            .retained_automatic_target((20, 20), Some(11), expired)
            .is_none());
        let (shown, active) =
            feedback.update_automatic(Some(window_candidate()), (20, 20), Some(11), expired);
        assert_eq!(shown.unwrap().rect, window_candidate().target.rect);
        assert!(!active && feedback.wake_at().is_none());
        // 移出所有窗口与宿主清屏之后也要恢复自动窗口。
        for millis in [1, 200] {
            let at = expired + Duration::from_millis(millis);
            feedback.update_automatic(None, (20, 20), None, at);
            feedback.clear();
            let (shown, active) =
                feedback.update_automatic(Some(window_candidate()), (20, 20), Some(11), at);
            assert_eq!(shown.unwrap().rect, window_candidate().target.rect);
            assert!(!active);
            assert!(feedback.automatic_window_allowed(Some(11), at));
        }
        // 用户主动选中窗口仍有效。
        let window = FeedbackCandidate {
            window_fallback: false,
            navigated: true,
            ..window_candidate()
        };
        let resumed = expired + Duration::from_millis(201);
        let (shown, active) =
            feedback.update_automatic(Some(window.clone()), (20, 20), Some(11), resumed);
        assert_eq!(shown.unwrap().rect, window.target.rect);
        assert!(!active && feedback.automatic_window_allowed(Some(11), resumed));

        // 新捕获使用新的状态，重新获得快速首屏窗口资格。
        let mut fresh = ControlFeedback::default();
        assert!(fresh.automatic_window_allowed(Some(11), resumed));
        let (shown, active) =
            fresh.update_automatic(Some(window_candidate()), (20, 20), Some(11), resumed);
        assert_eq!(shown.unwrap().rect, window.target.rect);
        assert!(!active);
    }

    #[test]
    fn automatic_preview_rejects_unmarked_coarse_candidates_and_holds_real_range_briefly() {
        let now = Instant::now();
        let at = |millis| now + Duration::from_millis(millis);
        let mut feedback = ControlFeedback::default();
        let window = candidate(0, 400, false);
        let leaf = candidate(10, 30, true);
        // 未明确标记的粗范围不是冻结窗口，也不为等待启动帧循环。
        for millis in [0, 80, 300, 1000] {
            let (shown, active) =
                feedback.update_automatic(Some(window.clone()), (20, 20), Some(11), at(millis));
            assert!(shown.is_none() && !active);
        }
        // 首次出现直接显示，不从空白插值。
        let (shown, active) =
            feedback.update_automatic(Some(leaf.clone()), (20, 20), Some(11), at(1001));
        assert_eq!(shown.unwrap().rect, leaf.target.rect);
        assert!(!active && feedback.transition.is_none());
        // 等待新结果：只保留旧的真实范围，不改画已知父级；超时后清空。
        let mut parent = candidate(0, 400, true);
        parent.terminal = false;
        parent.retained_for_query = true;
        let mut incomplete = leaf.clone();
        incomplete.complete = false;
        for (millis, waiting) in [(1010, None), (1100, Some(parent)), (1200, Some(incomplete))] {
            let (shown, active) =
                feedback.update_automatic(waiting, (80, 20), Some(11), at(millis));
            assert_eq!(shown.unwrap().rect, leaf.target.rect);
            // 保留期间不逐帧绘制，只在到期时唤醒一次。
            assert!(!active);
            assert_eq!(feedback.wake_at(), Some(at(1010) + AUTO_HOLD));
        }
        let (shown, active) =
            feedback.update_automatic(None, (80, 20), Some(11), at(1010) + AUTO_HOLD);
        assert!(shown.is_none() && !active);
        // 鼠标离开所有窗口立即清空。
        feedback.update_automatic(Some(leaf.clone()), (20, 20), Some(11), at(2000));
        assert!(feedback
            .update_automatic(None, (20, 20), None, at(2001))
            .0
            .is_none());
        // 鼠标不在任何子元素上：完整查询的最深元素是容器，也直接显示。
        let mut container = candidate(0, 200, true);
        container.terminal = false;
        let (shown, _) =
            feedback.update_automatic(Some(container.clone()), (150, 20), Some(11), at(3000));
        assert_eq!(shown.unwrap().rect, container.target.rect);
    }

    #[test]
    fn automatic_follow_snaps_and_only_wheel_navigation_transitions() {
        let now = Instant::now();
        let at = |millis| now + Duration::from_millis(millis);
        let mut feedback = ControlFeedback::default();
        let first = candidate(10, 30, true);
        let peer = candidate(60, 30, true);
        feedback.update_automatic(Some(first.clone()), (20, 20), Some(11), now);
        // 自动跟随直接切换，不启动逐帧过渡。
        let (shown, active) =
            feedback.update_automatic(Some(peer.clone()), (70, 20), Some(11), at(1));
        assert_eq!(shown.unwrap().rect, peer.target.rect);
        assert!(!active && feedback.transition.is_none());
        // 滚轮选择的父级（即使是旧点结果）立即采用，并从当前可见矩形过渡。
        let mut parent = candidate(0, 400, true);
        parent.terminal = false;
        parent.navigated = true;
        parent.sample = Some((1, 1));
        let (shown, active) =
            feedback.update_automatic(Some(parent.clone()), (70, 20), Some(11), at(10));
        assert_eq!(shown.unwrap().rect, peer.target.rect);
        assert!(active && feedback.pending.is_none());
        // 同一目标重复到达不重启过渡。
        let (middle, _) =
            feedback.update_automatic(Some(parent.clone()), (70, 20), Some(11), at(20));
        let middle = middle.unwrap().rect;
        assert!(middle.width > peer.target.rect.width && middle.width < parent.target.rect.width);
        // 过渡中再滚回子级：从当前可见矩形出发。
        let mut child = peer.clone();
        child.navigated = true;
        let (shown, _) = feedback.update_automatic(Some(child.clone()), (70, 20), Some(11), at(20));
        assert_eq!(shown.unwrap().rect, middle);
        let (shown, active) = feedback.update_automatic(
            Some(child.clone()),
            (70, 20),
            Some(11),
            at(20) + AUTO_LARGE_TRANSITION,
        );
        assert_eq!(shown.unwrap().rect, child.target.rect);
        assert!(!active && feedback.transition.is_none());
    }

    #[test]
    fn stale_expansion_waits_for_confirmation_but_is_never_suppressed() {
        let now = Instant::now();
        let at = |millis| now + Duration::from_millis(millis);
        let leaf = |x, point: (i32, i32)| FeedbackCandidate {
            sample: Some(point),
            ..candidate(x, 30, true)
        };
        let container = |point: (i32, i32)| {
            let mut container = candidate(0, 400, true);
            container.terminal = false;
            container.sample = Some(point);
            container
        };
        let target = |feedback: &ControlFeedback| feedback.shown.as_ref().unwrap().target.rect;
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(leaf(10, (20, 20))), (20, 20), Some(11), now);
        // 穿过空隙：旧点得到容器、鼠标已到下一元素；容器不显示，下一元素直接接上。
        let (shown, active) =
            feedback.update_automatic(Some(container((45, 20))), (52, 20), Some(11), at(5));
        assert_eq!(shown.unwrap().rect, leaf(10, (20, 20)).target.rect);
        assert!(!active && feedback.pending.is_some());
        assert_eq!(feedback.wake_at(), Some(at(5) + AUTO_EXPAND_STILL));
        feedback.update_automatic(Some(leaf(50, (52, 20))), (52, 20), Some(11), at(20));
        assert_eq!(target(&feedback), leaf(50, (52, 20)).target.rect);
        assert!(feedback.pending.is_none());
        // 当前点的容器结果立即采用。
        feedback.update_automatic(Some(container((90, 20))), (90, 20), Some(11), at(30));
        assert_eq!(target(&feedback), container((0, 0)).target.rect);

        // 另一点的结果给出同一容器：二次确认后采用。
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(leaf(10, (20, 20))), (20, 20), Some(11), now);
        feedback.update_automatic(Some(container((45, 20))), (50, 20), Some(11), at(5));
        feedback.update_automatic(Some(container((50, 20))), (55, 20), Some(11), at(25));
        assert_eq!(target(&feedback), container((0, 0)).target.rect);

        // 鼠标停住即采用。
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(leaf(10, (20, 20))), (20, 20), Some(11), now);
        feedback.update_automatic(Some(container((45, 20))), (50, 20), Some(11), at(5));
        feedback.update_automatic(Some(container((45, 20))), (50, 20), Some(11), at(20));
        assert!(feedback.pending.is_some());
        feedback.update_automatic(
            Some(container((45, 20))),
            (50, 20),
            Some(11),
            at(5) + AUTO_EXPAND_STILL,
        );
        assert_eq!(target(&feedback), container((0, 0)).target.rect);

        // 持续移动且没有新结果：等满上限也采用，大范围不会被屏蔽。
        let mut feedback = ControlFeedback::default();
        feedback.update_automatic(Some(leaf(10, (20, 20))), (20, 20), Some(11), now);
        let mut x = 50;
        let mut millis = 5;
        feedback.update_automatic(Some(container((45, 20))), (x, 20), Some(11), at(millis));
        while millis < 5 + AUTO_EXPAND_MAX_WAIT.as_millis() as u64 - 10 {
            millis += 10;
            x += 1;
            feedback.update_automatic(Some(container((45, 20))), (x, 20), Some(11), at(millis));
            assert!(feedback.pending.is_some());
        }
        feedback.update_automatic(
            Some(container((45, 20))),
            (x + 1, 20),
            Some(11),
            at(5) + AUTO_EXPAND_MAX_WAIT,
        );
        assert_eq!(target(&feedback), container((0, 0)).target.rect);
        assert!(feedback.transition.is_none());
    }

    #[test]
    fn large_jump_follows_reference_thresholds() {
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert!(!large_jump(rect(0, 0, 30, 30), rect(40, 0, 30, 30)));
        assert!(!large_jump(rect(0, 0, 300, 30), rect(0, 0, 400, 300)));
        assert!(large_jump(rect(0, 0, 30, 30), rect(0, 0, 900, 800)));
        assert!(large_jump(rect(0, 0, 1200, 900), rect(0, 0, 1300, 900)));
        assert!(large_jump(rect(0, 0, 0, 30), rect(0, 0, 30, 30)));
    }

    #[test]
    fn pending_window_does_not_flash_over_completed_element() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let fine = candidate(10, 30, true);
        assert_eq!(
            feedback
                .update(Some(fine.clone()), (20, 20), Some(11), now)
                .0
                .unwrap()
                .rect,
            fine.target.rect
        );
        let coarse = candidate(0, 200, false);
        let (shown, active) = feedback.update(
            Some(coarse.clone()),
            (45, 20),
            Some(11),
            now + Duration::from_millis(20),
        );
        assert_eq!(shown.unwrap().rect, fine.target.rect);
        assert!(active);
        feedback.update(
            Some(coarse.clone()),
            (45, 20),
            Some(11),
            now + Duration::from_millis(171),
        );
        assert_eq!(
            feedback
                .update(
                    Some(coarse.clone()),
                    (45, 20),
                    Some(11),
                    now + Duration::from_millis(204)
                )
                .0
                .unwrap()
                .rect,
            coarse.target.rect
        );
    }

    #[test]
    fn completed_peer_transitions_immediately_and_ends_in_32_ms() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        feedback.update(Some(candidate(10, 30, true)), (20, 20), Some(11), now);
        let peer = candidate(60, 30, true);
        let (_, active) = feedback.update(
            Some(peer.clone()),
            (70, 20),
            Some(11),
            now + Duration::from_millis(1),
        );
        assert!(active);
        let middle = feedback
            .update(
                Some(peer.clone()),
                (70, 20),
                Some(11),
                now + Duration::from_millis(17),
            )
            .0
            .unwrap()
            .rect;
        assert!(middle.x > 10 && middle.x < 60);
        let (shown, active) = feedback.update(
            Some(peer.clone()),
            (70, 20),
            Some(11),
            now + Duration::from_millis(33),
        );
        assert_eq!(shown.unwrap().rect, peer.target.rect);
        assert!(!active);
    }

    #[test]
    fn incomplete_peer_keeps_fine_feedback_and_complete_leaf_can_interrupt_hold() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let fine = candidate(10, 30, true);
        feedback.update(Some(fine.clone()), (20, 20), Some(11), now);
        let mut partial = candidate(60, 600, true);
        partial.complete = false;
        partial.terminal = false;
        let moved = now + Duration::from_millis(1);
        let (shown, active) = feedback.update(Some(partial.clone()), (70, 20), Some(11), moved);
        assert_eq!(shown.unwrap().rect, fine.target.rect);
        assert!(active);
        assert_eq!(
            feedback
                .update(
                    Some(partial.clone()),
                    (70, 20),
                    Some(11),
                    moved + Duration::from_millis(149)
                )
                .0
                .unwrap()
                .rect,
            fine.target.rect
        );
        let peer = candidate(60, 30, true);
        feedback.update(
            Some(peer.clone()),
            (70, 20),
            Some(11),
            moved + Duration::from_millis(149),
        );
        let (shown, active) = feedback.update(
            Some(peer.clone()),
            (70, 20),
            Some(11),
            moved + Duration::from_millis(181),
        );
        assert_eq!(shown.unwrap().rect, peer.target.rect);
        assert!(!active);

        // 没有新细结果时，停稳后仍可接受安全的部分几何，不永久挂在旧元素。
        let mut feedback = ControlFeedback::default();
        feedback.update(Some(fine), (20, 20), Some(11), now);
        feedback.update(Some(partial.clone()), (70, 20), Some(11), moved);
        feedback.update(
            Some(partial.clone()),
            (70, 20),
            Some(11),
            moved + SETTLE_TIME,
        );
        let (shown, active) = feedback.update(
            Some(partial.clone()),
            (70, 20),
            Some(11),
            moved + SETTLE_TIME + TRANSITION_TIME,
        );
        assert_eq!(shown.unwrap().rect, partial.target.rect);
        assert!(!active);
    }

    #[test]
    fn retarget_starts_at_current_visual_and_same_target_does_not_restart() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        feedback.update(Some(candidate(0, 30, true)), (10, 20), Some(11), now);
        feedback.update(Some(candidate(60, 30, true)), (70, 20), Some(11), now);
        let sampled = feedback
            .visual_rect(now + Duration::from_millis(16))
            .unwrap();
        assert_eq!(
            feedback
                .update(
                    Some(candidate(120, 30, true)),
                    (130, 20),
                    Some(11),
                    now + Duration::from_millis(16)
                )
                .0
                .unwrap()
                .rect,
            sampled
        );
        let target = candidate(120, 30, true);
        feedback.update(
            Some(target.clone()),
            (130, 20),
            Some(11),
            now + Duration::from_millis(30),
        );
        assert_eq!(
            feedback
                .update(
                    Some(target.clone()),
                    (130, 20),
                    Some(11),
                    now + Duration::from_millis(48)
                )
                .0
                .unwrap()
                .rect,
            target.target.rect
        );
    }

    #[test]
    fn one_pixel_motion_restarts_expansion_settle_gate() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        feedback.observe_pointer((20, 20), Some(11), now);
        assert!(feedback.settled(now + SETTLE_TIME));
        feedback.observe_pointer((21, 20), Some(11), now + SETTLE_TIME);
        assert!(!feedback.settled(now + SETTLE_TIME + Duration::from_millis(149)));
    }

    #[test]
    fn manual_parent_is_not_delayed_and_clear_drops_old_visual() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        feedback.update(Some(candidate(10, 30, true)), (20, 20), Some(11), now);
        let mut parent = candidate(0, 200, true);
        parent.terminal = false;
        assert!(defer_expansion(&candidate(10, 30, true), &parent, (20, 20)));
        parent.navigated = true;
        assert!(!defer_expansion(
            &candidate(10, 30, true),
            &parent,
            (20, 20)
        ));
        feedback.update(Some(parent.clone()), (20, 20), Some(11), now);
        assert_eq!(
            feedback
                .update(
                    Some(parent.clone()),
                    (20, 20),
                    Some(11),
                    now + TRANSITION_TIME
                )
                .0
                .unwrap()
                .rect,
            parent.target.rect
        );
        feedback.clear();
        assert!(feedback.visual_rect(now).is_none());
    }

    #[test]
    fn anchored_right_bottom_transition_never_leaves_canvas() {
        let now = Instant::now();
        let transition = Transition {
            from: Rect::full(100, 100),
            to: Rect {
                x: 4,
                y: 4,
                width: 96,
                height: 96,
            },
            started: now,
            duration: TRANSITION_TIME,
        };
        for millis in 0..=32 {
            let rect = transition.sample(now + Duration::from_millis(millis));
            assert!(rect.valid(100, 100));
            assert_eq!((rect.x + rect.width, rect.y + rect.height), (100, 100));
        }
    }

    #[test]
    fn completed_moving_feedback_exports_shown_frame_even_outside_event_point() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let fine = candidate(10, 30, true);
        let shown = feedback
            .update(Some(fine), (180, 20), Some(11), now)
            .0
            .unwrap();
        let editor = crate::editor::Editor::new(Rect::full(400, 300), None);
        assert!(crate::preview_middle_pin::candidate(
            &editor,
            editor.canvas,
            (180, 20),
            Some(shown.rect),
            false,
            false
        )
        .is_some());
        assert!(crate::preview_middle_pin::candidate(
            &editor,
            editor.canvas,
            (20, 20),
            Some(shown.rect),
            false,
            false
        )
        .is_some());
    }

    #[test]
    fn retained_range_animates_during_motion_without_waiting_for_settle() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let fine = candidate(10, 30, true);
        feedback.update(Some(fine.clone()), (20, 20), Some(11), now);
        let mut retained = candidate(0, 200, true);
        retained.terminal = false;
        retained.complete = false;
        retained.retained_for_query = true;
        assert!(!fine.target.rect.contains((60, 20)));
        assert!(!retained.navigated);
        let start = now + Duration::from_millis(1);
        let (shown, active) = feedback.update(Some(retained.clone()), (60, 20), Some(11), start);
        assert_eq!(shown.unwrap().rect, fine.target.rect);
        assert!(active);
        for millis in [8, 16, 24] {
            let (shown, active) = feedback.update(
                Some(retained.clone()),
                (60 + millis as i32, 20),
                Some(11),
                start + Duration::from_millis(millis),
            );
            let rect = shown.unwrap().rect;
            assert!(rect.width > fine.target.rect.width && rect.width < retained.target.rect.width);
            assert!(active);
            assert!(!feedback.settled(start + Duration::from_millis(millis)));
        }
        let (shown, active) = feedback.update(
            Some(retained.clone()),
            (100, 20),
            Some(11),
            start + TRANSITION_TIME,
        );
        assert_eq!(shown.unwrap().rect, retained.target.rect);
        assert!(!active);
        assert!(!feedback.settled(start + TRANSITION_TIME));
    }

    #[test]
    fn unretained_fallback_keeps_settle_gate_after_continuous_motion() {
        let now = Instant::now();
        let mut feedback = ControlFeedback::default();
        let fine = candidate(10, 30, true);
        feedback.update(Some(fine.clone()), (20, 20), Some(11), now);
        let fallback = candidate(0, 200, false);
        assert!(!fallback.retained_for_query);
        for millis in [1, 8, 16, 24, 32] {
            let (shown, active) = feedback.update(
                Some(fallback.clone()),
                (60 + millis as i32, 20),
                Some(11),
                now + Duration::from_millis(millis),
            );
            assert_eq!(shown.unwrap().rect, fine.target.rect);
            assert!(active);
        }
        let last_motion = now + Duration::from_millis(32);
        assert_eq!(
            feedback
                .update(
                    Some(fallback.clone()),
                    (92, 20),
                    Some(11),
                    last_motion + SETTLE_TIME - Duration::from_millis(1),
                )
                .0
                .unwrap()
                .rect,
            fine.target.rect
        );
        feedback.update(
            Some(fallback.clone()),
            (92, 20),
            Some(11),
            last_motion + SETTLE_TIME,
        );
        let (shown, active) = feedback.update(
            Some(fallback.clone()),
            (92, 20),
            Some(11),
            last_motion + SETTLE_TIME + TRANSITION_TIME,
        );
        assert_eq!(shown.unwrap().rect, fallback.target.rect);
        assert!(!active);
    }
}
