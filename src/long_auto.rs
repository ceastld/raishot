//! 自动滚动的纯决策模型：按匹配质量自适应调速，半格细步，最多预发一整格。
use crate::stitch::{StitchMatchQuality, UpdateKind};
use std::time::{Duration, Instant};

pub(crate) const AUTO_WHEEL_DELTA: i16 = 60;
const WHEEL_NOTCH: u16 = 120;
const NO_MOTION: Duration = Duration::from_millis(2500);
const RECOVERY: Duration = Duration::from_millis(500);
const BASELINE_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AutoDirection {
    #[default]
    Forward,
    Reverse,
}
impl AutoDirection {
    pub(crate) fn forward(self) -> bool {
        self == Self::Forward
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutoScrollOptions {
    pub direction: AutoDirection,
}
/// 质量是匹配器的启发式证据，并非校准后的概率。一次滚轮响应只奖励一次；
/// 平滑滚动的中间帧累计最弱证据，重复帧不能制造加速依据。
struct AdaptiveSpeed {
    speed: f64,
    good_responses: u8,
    response_good: Option<bool>,
    response_slowed: bool,
}
impl Default for AdaptiveSpeed {
    fn default() -> Self {
        Self {
            speed: 1.,
            good_responses: 0,
            response_good: None,
            response_slowed: false,
        }
    }
}
impl AdaptiveSpeed {
    fn slow_down(&mut self) {
        if !self.response_slowed {
            self.speed = (self.speed * 0.6).max(1.);
        }
        self.good_responses = 0;
        self.response_good = Some(false);
        self.response_slowed = true;
    }
    fn observe(&mut self, quality: Option<StitchMatchQuality>, moved: bool, response: Duration) {
        // 留出采集帧的粒度；页面响应持续落后于请求节奏时回退，而非积压输入。
        if response > self.interval().max(Duration::from_millis(30)) * 2 {
            self.slow_down();
        }
        if let Some(q) = quality {
            let finite =
                q.error.is_finite() && q.matched_ratio.is_finite() && q.overlap_ratio.is_finite();
            if !finite
                || !q.unique
                || !(0.0..=4.0).contains(&q.error)
                || !(0.80..=1.0).contains(&q.matched_ratio)
                || !(0.45..=1.0).contains(&q.overlap_ratio)
            {
                self.slow_down();
            }
            if moved {
                let strong = finite
                    && q.unique
                    && (0.0..=2.0).contains(&q.error)
                    && (0.90..=1.0).contains(&q.matched_ratio)
                    && (0.65..=1.0).contains(&q.overlap_ratio);
                self.response_good = Some(self.response_good.unwrap_or(true) && strong);
            }
        } else if moved {
            self.response_good = Some(false);
        }
    }
    fn sent(&mut self) {
        if self.response_good == Some(true) && !self.response_slowed {
            self.good_responses += 1;
            if self.good_responses >= 2 {
                self.speed = (self.speed + 0.5).min(10.);
                self.good_responses = 0;
            }
        } else if self.response_good == Some(false) {
            self.good_responses = 0;
        }
        self.response_good = None;
        self.response_slowed = false;
    }
    fn interval(&self) -> Duration {
        Duration::from_secs_f64(0.1 / self.speed)
    }
    fn quiet_interval(&self) -> Duration {
        // 静止等待比发送间隔下降得慢，给平滑滚动保留停稳观察时间。
        Duration::from_secs_f64(0.03 / self.speed.sqrt())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AutoPhase {
    #[default]
    Off,
    Settling,
    AwaitingFrame,
    Recovering,
    NoMotion,
    Unstable,
    Blocked,
}
#[derive(Clone, Debug)]
pub(crate) struct AutoSnapshot {
    pub active: bool,
    pub phase: AutoPhase,
    pub speed: f64,
    pub steps: u64,
    pub target: String,
    pub status: String,
}
impl Default for AutoSnapshot {
    fn default() -> Self {
        Self {
            active: false,
            phase: AutoPhase::Off,
            speed: 1.,
            steps: 0,
            target: String::new(),
            status: String::new(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Wait,
    Post,
    Stop {
        phase: AutoPhase,
        reason: &'static str,
    },
}

pub(crate) struct AutoPolicy {
    pub(crate) options: AutoScrollOptions,
    pacing: AdaptiveSpeed,
    generation: u64,
    phase: AutoPhase,
    started: Instant,
    last_step: Option<Instant>,
    step_poll: u64,
    last_poll: u64,
    has_baseline: bool,
    last_reliable_position: Option<i64>,
    movement_version: u64,
    movement_at_step: u64,
    last_movement: Option<Instant>,
    unacknowledged_delta: u16,
    rejected_since: Option<Instant>,
    steps: u64,
}
impl AutoPolicy {
    pub(crate) fn new(options: AutoScrollOptions, generation: u64, now: Instant) -> Self {
        Self {
            options,
            pacing: AdaptiveSpeed::default(),
            generation,
            phase: AutoPhase::Settling,
            started: now,
            last_step: None,
            step_poll: 0,
            last_poll: 0,
            has_baseline: false,
            last_reliable_position: None,
            movement_version: 0,
            movement_at_step: 0,
            last_movement: None,
            unacknowledged_delta: 0,
            rejected_since: None,
            steps: 0,
        }
    }
    pub(crate) fn snapshot(&self, target: String) -> AutoSnapshot {
        AutoSnapshot {
            active: true,
            phase: self.phase,
            speed: self.pacing.speed,
            steps: self.steps,
            target,
            status: match self.phase {
                AutoPhase::Settling => tr!(
                    "等待可靠页面停止移动后继续细步滚动…",
                    "Waiting for the page to settle before the next small step…"
                ),
                AutoPhase::AwaitingFrame => tr!(
                    "已发送半格，等待页面响应和可靠匹配…",
                    "Half step sent; waiting for the page to respond and match…"
                ),
                AutoPhase::Recovering => tr!(
                    "页面尚未可靠匹配，暂不继续滚动…",
                    "No reliable match yet; paused scrolling…"
                ),
                _ => "",
            }
            .into(),
        }
    }
    fn stale_generation(&self, generation: u64) -> Option<Decision> {
        (generation != self.generation).then_some(Decision::Stop {
            phase: AutoPhase::Off,
            reason: tr!(
                "采集配置已变更，自动滚动已停止",
                "Capture settings changed; auto-scroll stopped"
            ),
        })
    }
    fn timeout(&self, now: Instant) -> Option<Decision> {
        if self
            .rejected_since
            .is_some_and(|since| now.saturating_duration_since(since) >= RECOVERY)
        {
            return Some(Decision::Stop {
                phase: AutoPhase::Unstable,
                reason: tr!(
                    "未找到可靠重叠，已停止自动滚动；请回到上一段再慢慢滚动",
                    "No reliable overlap found; auto-scroll stopped. Go back a bit and scroll slowly"
                ),
            });
        }
        if !self.has_baseline && now.saturating_duration_since(self.started) >= BASELINE_DEADLINE {
            return Some(Decision::Stop {
                phase: AutoPhase::NoMotion,
                reason: tr!(
                    "页面尚未形成可靠基线，自动滚动已停止",
                    "No reliable baseline from the page; auto-scroll stopped"
                ),
            });
        }
        // 已有基线但当前帧待匹配时先背压；不把暂时拒绝误判为末尾。
        if self.rejected_since.is_some() {
            return None;
        }
        if self.movement_version <= self.movement_at_step
            && self.unacknowledged_delta >= WHEEL_NOTCH
            && self
                .last_step
                .is_some_and(|sent| now.saturating_duration_since(sent) >= NO_MOTION)
        {
            return Some(Decision::Stop {
                phase: AutoPhase::NoMotion,
                reason: tr!(
                    "页面未继续移动，可能到达边界或不响应；自动滚动已停止并保留结果",
                    "The page stopped moving (end reached or unresponsive); auto-scroll stopped, result kept"
                ),
            });
        }
        None
    }
    /// 等待新帧期间只能检查停止时限，绝不利用旧反馈继续投递滚轮。
    pub(crate) fn idle(&self, generation: u64, now: Instant) -> Decision {
        self.stale_generation(generation)
            .or_else(|| self.timeout(now))
            .unwrap_or(Decision::Wait)
    }
    #[cfg(test)]
    pub(crate) fn observe(
        &mut self,
        generation: u64,
        poll: u64,
        kind: UpdateKind,
        delta: i32,
        position: i64,
        now: Instant,
    ) -> Decision {
        self.observe_match(generation, poll, kind, delta, position, None, now)
    }
    /// poll 必须来自当前代成功读取的新帧；发送前的帧不能确认响应或影响速度。
    pub(crate) fn observe_match(
        &mut self,
        generation: u64,
        poll: u64,
        kind: UpdateKind,
        delta: i32,
        position: i64,
        quality: Option<StitchMatchQuality>,
        now: Instant,
    ) -> Decision {
        if let Some(stop) = self.stale_generation(generation) {
            return stop;
        }
        if poll <= self.last_poll || (self.last_step.is_some() && poll <= self.step_poll) {
            return Decision::Wait;
        }
        self.last_poll = poll;
        if kind == UpdateKind::Limit {
            return Decision::Stop {
                phase: AutoPhase::Off,
                reason: tr!(
                    "已达到长截图上限，自动滚动已停止",
                    "Long capture limit reached; auto-scroll stopped"
                ),
            };
        }
        if kind == UpdateKind::Rejected {
            self.pacing.slow_down();
            self.phase = AutoPhase::Recovering;
            self.rejected_since.get_or_insert(now);
            return self.timeout(now).unwrap_or(Decision::Wait);
        }
        // 只有匹配器认可的比较帧可以恢复反馈，拒绝帧从不加强运动先验。
        self.rejected_since = None;
        let position_changed = self
            .last_reliable_position
            .is_some_and(|previous| previous != position);
        // 旧许可可能已经拼接了位移，但不能确认本次滚轮；后续新帧即使零 delta，
        // 仍须比较真实累计位置。直接比较端点，避免长会话 i64 差值溢出。
        let position_reversed = self.last_reliable_position.is_some_and(|previous| {
            position != previous && (position > previous) != self.options.direction.forward()
        });
        if position_reversed || (delta != 0 && (delta > 0) != self.options.direction.forward()) {
            return Decision::Stop {
                phase: AutoPhase::Blocked,
                reason: tr!(
                    "检测到与设置相反的滚动，自动滚动已停止；仍可手动继续采集",
                    "Scrolling opposite to the setting detected; auto-scroll stopped. You can keep scrolling manually"
                ),
            };
        }
        if let Some(sent) = self.last_step.filter(|_| self.unacknowledged_delta > 0) {
            self.pacing.observe(
                quality,
                delta != 0 && position_changed,
                now.saturating_duration_since(sent),
            );
        }
        if !self.has_baseline || kind == UpdateKind::Added || position_changed {
            self.movement_version = self.movement_version.saturating_add(1);
            self.last_movement = Some(now);
        }
        self.last_reliable_position = Some(position);
        self.has_baseline = true;
        if let Some(stop) = self.timeout(now) {
            return stop;
        }
        if self.movement_version <= self.movement_at_step
            && self.unacknowledged_delta >= WHEEL_NOTCH
        {
            self.phase = AutoPhase::AwaitingFrame;
            return Decision::Wait;
        }
        self.phase = AutoPhase::Settling;
        if now.saturating_duration_since(self.last_step.unwrap_or(self.started))
            < self.pacing.interval()
            || self.last_movement.is_some_and(|movement| {
                now.saturating_duration_since(movement) < self.pacing.quiet_interval()
            })
        {
            return Decision::Wait;
        }
        Decision::Post
    }
    /// 只在 Post 被原生目标实际接受后调用；发送失败不能推进步数或半格累计。
    pub(crate) fn sent(&mut self, poll: u64, now: Instant) {
        self.pacing.sent();
        if self.movement_version > self.movement_at_step {
            self.unacknowledged_delta = 0;
        }
        debug_assert!(self.unacknowledged_delta < WHEEL_NOTCH);
        self.unacknowledged_delta += AUTO_WHEEL_DELTA as u16;
        self.movement_at_step = self.movement_version;
        self.steps = self.steps.saturating_add(1);
        self.last_step = Some(now);
        self.step_poll = poll;
        self.last_poll = poll;
        self.phase = AutoPhase::AwaitingFrame;
    }
}

#[cfg(test)]
#[path = "long_auto_pacer_tests.rs"]
mod tests;
