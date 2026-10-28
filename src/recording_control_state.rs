//! Worker 的控制收据消费器；整批认可后才提交时钟并逐次通知暂停变化。
use super::{
    clock::{ClockPoint, RecordingClock},
    control_receipt::{ControlBatch, ControlBegin, ControlFailure, ControlKind, ControlReceipt},
};
use crate::Result;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ControlFence {
    pub point: ClockPoint,
    pub active: Duration,
    pub stopped: bool,
}

pub(super) struct ControlState {
    clock: RecordingClock,
    latest: ControlFence,
    observed: ClockPoint,
    seq: u64,
    paused: bool,
    stop: Option<ControlReceipt>,
    begin_failure: Option<ControlFailure>,
    begin_stop_valid: bool,
    finished: bool,
}

impl ControlState {
    /// 构造保持不可失败，让外层即使起钟失败也能进入 writer 的统一收尾。
    pub(super) fn new(begin: ControlBegin) -> Self {
        let mut clock = RecordingClock::start(begin.point, Duration::ZERO);
        if begin.initial_paused || begin.stop.is_some() {
            clock.pause(begin.point);
        }
        let begin_stop_valid = begin.stop.is_none_or(|stop| {
            stop.kind == ControlKind::Stop
                && stop.seq != 0
                && stop.seq == begin.seq_floor
                && stop.point.instant <= begin.point.instant
        });
        Self {
            clock,
            latest: ControlFence {
                point: begin.point,
                active: Duration::ZERO,
                stopped: begin.stop.is_some(),
            },
            observed: begin.point,
            seq: begin.seq_floor,
            paused: begin.initial_paused,
            stop: begin.stop,
            begin_failure: begin.failure,
            begin_stop_valid,
            finished: false,
        }
    }

    pub(super) fn check_begin(&self) -> Result<()> {
        if let Some(failure) = self.begin_failure {
            return Err(failure.into());
        }
        if !self.begin_stop_valid {
            return Err(tr!(
                "录屏起钟前的停止收据无效",
                "Recording control begin has an invalid prestart Stop receipt"
            )
            .into());
        }
        Ok(())
    }

    pub(super) fn clock(&self) -> &RecordingClock {
        &self.clock
    }

    pub(super) fn latest(&self) -> ControlFence {
        self.latest
    }

    /// 逻辑暂停意图；Stop/finish 会关闭时钟，但不会凭空生成 Pause(true)。
    pub(super) fn is_paused(&self) -> bool {
        self.paused
    }

    pub(super) fn apply(
        &mut self,
        batch: ControlBatch,
        mut on_pause: impl FnMut(bool, ClockPoint),
    ) -> Result<ControlFence> {
        self.check_begin()?;
        if let Some(failure) = batch.failure {
            return Err(failure.into());
        }
        if self.finished {
            return Err(tr!(
                "录屏控制已结束，不能继续应用收据",
                "Recording controls cannot be applied after finish"
            )
            .into());
        }
        if batch.point.instant < self.observed.instant {
            return Err(tr!(
                "录屏控制批次的时间边界逆序",
                "Recording control batch fence moved backwards"
            )
            .into());
        }
        if batch.events.len() > 64 {
            return Err(tr!(
                "录屏控制批次超过 64 条收据上限",
                "Recording control batch exceeds its 64-receipt bound"
            )
            .into());
        }
        if batch.events.is_empty() {
            if batch.paused != self.paused || batch.stop != self.stop {
                return Err(tr!(
                    "录屏控制批次摘要与已接纳的状态不一致",
                    "Recording control batch summary contradicts its accepted prefix"
                )
                .into());
            }
            let latest = if self.stop.is_some() {
                self.latest
            } else {
                fence_at(&self.clock, batch.point, self.latest.active)?
            };
            // 空批不克隆时钟或分配临时事件；停止后只推进观察 fence，不延长 tail。
            self.observed = batch.point;
            self.latest = latest;
            return Ok(latest);
        }
        if self.stop.is_some() {
            return Err(tr!(
                "录屏首次停止之后出现了新的控制事件",
                "Recording control events appeared after the first Stop"
            )
            .into());
        }

        let mut clock = self.clock.clone();
        let mut seq = self.seq;
        let mut paused = self.paused;
        let mut stop = self.stop;
        let mut previous = self.observed;
        let mut active = self.latest.active;
        let mut stopped_fence = None;
        for receipt in &batch.events {
            if stop.is_some() {
                return Err(tr!(
                    "录屏控制批次在停止之后仍有事件",
                    "Recording control batch has events after Stop"
                )
                .into());
            }
            let expected = seq.checked_add(1).ok_or(ControlFailure::SequenceOverflow)?;
            if receipt.seq != expected {
                return Err(tr!(
                    "录屏控制收据序号不连续",
                    "Recording control receipt sequence is not contiguous"
                )
                .into());
            }
            if receipt.point.instant < previous.instant
                || receipt.point.instant > batch.point.instant
            {
                return Err(tr!(
                    "录屏控制收据时间逆序或超出批次边界",
                    "Recording control receipt is reversed or beyond its batch fence"
                )
                .into());
            }
            let event_active = clock
                .active_at(receipt.point.instant)
                .filter(|&value| value >= active)
                .ok_or(tr!(
                    "录屏控制收据没有有效的活动时间",
                    "Recording control receipt has no valid active time"
                ))?;
            match receipt.kind {
                ControlKind::Pause(next) => {
                    if next == paused {
                        return Err(tr!(
                            "录屏控制收据重复了未变化的暂停状态",
                            "Recording control receipt repeats an unchanged pause state"
                        )
                        .into());
                    }
                    if next {
                        clock.pause(receipt.point);
                    } else {
                        clock.resume(receipt.point, event_active);
                    }
                    if clock.is_paused() != next
                        || clock.active_at(receipt.point.instant) != Some(event_active)
                    {
                        return Err(tr!(
                            "录屏时钟拒绝了暂停或恢复边界",
                            "Recording clock rejected a pause/resume transition"
                        )
                        .into());
                    }
                    paused = next;
                }
                ControlKind::Stop => {
                    clock.pause(receipt.point);
                    if !clock.is_paused()
                        || clock.active_at(receipt.point.instant) != Some(event_active)
                    {
                        return Err(tr!(
                            "录屏时钟拒绝了首次停止边界",
                            "Recording clock rejected the first Stop boundary"
                        )
                        .into());
                    }
                    stop = Some(*receipt);
                    stopped_fence = Some(ControlFence {
                        point: receipt.point,
                        active: event_active,
                        stopped: true,
                    });
                }
            }
            active = event_active;
            seq = receipt.seq;
            previous = receipt.point;
        }
        if batch.paused != paused || batch.stop != stop {
            return Err(tr!(
                "录屏控制批次摘要与事件收据不一致",
                "Recording control batch summary contradicts its receipts"
            )
            .into());
        }
        let latest = match stopped_fence {
            Some(fence) => fence,
            None => fence_at(&clock, batch.point, active)?,
        };

        self.clock = clock;
        self.seq = seq;
        self.paused = paused;
        self.stop = stop;
        self.latest = latest;
        self.observed = batch.point;
        // 完整批次已经认可，才通知每个真实 Pause；快速暂停/恢复不能合并。
        for receipt in batch.events {
            if let ControlKind::Pause(paused) = receipt.kind {
                on_pause(paused, receipt.point);
            }
        }
        Ok(latest)
    }

    /// 只封住最后认可的 fence；不重新采时，也不伪造 Stop 收据。
    pub(super) fn finish(&mut self) -> Result<ControlFence> {
        self.check_begin()?;
        if self.finished {
            return Ok(self.latest);
        }
        let mut clock = self.clock.clone();
        if clock.active_at(self.latest.point.instant) != Some(self.latest.active) {
            return Err(tr!(
                "录屏时钟无法在最后接纳的边界封口",
                "Recording clock cannot close at its last accepted fence"
            )
            .into());
        }
        clock.pause(self.latest.point);
        if !clock.is_paused()
            || clock.active_at(self.latest.point.instant) != Some(self.latest.active)
        {
            return Err(tr!(
                "录屏时钟拒绝了最后接纳的封口边界",
                "Recording clock rejected its final accepted fence"
            )
            .into());
        }
        self.clock = clock;
        self.latest.stopped = true;
        self.finished = true;
        Ok(self.latest)
    }
}

fn fence_at(clock: &RecordingClock, point: ClockPoint, minimum: Duration) -> Result<ControlFence> {
    let active = clock
        .active_at(point.instant)
        .filter(|&value| value >= minimum)
        .ok_or(tr!(
            "录屏控制边界没有有效的活动时间",
            "Recording control fence has no valid active time"
        ))?;
    Ok(ControlFence {
        point,
        active,
        stopped: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::control_receipt::ControlQueue;
    use std::time::Instant;

    fn point(base: Instant, millis: u64) -> ClockPoint {
        ClockPoint {
            instant: base + Duration::from_millis(millis),
            qpc: None,
        }
    }

    fn copy_batch(batch: &ControlBatch) -> ControlBatch {
        ControlBatch {
            point: batch.point,
            events: batch.events.clone(),
            paused: batch.paused,
            stop: batch.stop,
            failure: batch.failure,
        }
    }

    #[test]
    fn recording_control_state_replays_fast_pulses_on_the_real_clock() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        let mut state = ControlState::new(queue.begin(point(base, 0)));
        state.check_begin().unwrap();
        for (millis, paused) in [(10, true), (20, false), (30, true), (40, false)] {
            queue
                .request(ControlKind::Pause(paused), point(base, millis))
                .unwrap();
        }
        let mut callbacks = Vec::new();
        let fence = state
            .apply(queue.take(point(base, 100)), |paused, point| {
                callbacks.push((paused, point));
            })
            .unwrap();
        assert_eq!(
            callbacks,
            [
                (true, point(base, 10)),
                (false, point(base, 20)),
                (true, point(base, 30)),
                (false, point(base, 40)),
            ]
        );
        assert_eq!(fence.active, Duration::from_millis(80));
        assert_eq!(fence.point, point(base, 100));
        assert!(!fence.stopped && !state.is_paused());
        let spans = state.clock().active_spans();
        assert_eq!(spans.len(), 3);
        assert_eq!(
            (spans[0].key, spans[0].start, spans[0].end),
            (
                point(base, 0).instant,
                Duration::ZERO,
                Some(Duration::from_millis(10))
            )
        );
        assert_eq!(
            (spans[1].key, spans[1].start, spans[1].end),
            (
                point(base, 20).instant,
                Duration::from_millis(10),
                Some(Duration::from_millis(20))
            )
        );
        assert_eq!(
            (spans[2].key, spans[2].start, spans[2].end),
            (point(base, 40).instant, Duration::from_millis(20), None)
        );

        let summary = state.clock().summary();
        let fence = state
            .apply(queue.take(point(base, 120)), |_, _| {
                panic!("空批不能通知暂停")
            })
            .unwrap();
        assert_eq!(fence.active, Duration::from_millis(100));
        assert_eq!(state.clock().summary(), summary);
        let finished = state.finish().unwrap();
        assert_eq!(finished.point, point(base, 120));
        assert_eq!(finished.active, fence.active);
        assert!(finished.stopped && state.clock().is_paused());
        assert_eq!(
            state.clock().active_at(point(base, 300).instant),
            Some(finished.active)
        );
        let summary = state.clock().summary();
        assert_eq!(state.finish().unwrap(), finished);
        assert_eq!(state.clock().summary(), summary);
        assert!(state
            .apply(queue.take(point(base, 130)), |_, _| panic!(
                "finish后不能通知"
            ))
            .is_err());
        assert_eq!(state.latest(), finished);
    }

    #[test]
    fn recording_control_state_first_stop_precedes_a_delayed_take() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        let mut state = ControlState::new(queue.begin(point(base, 0)));
        queue
            .request(ControlKind::Pause(true), point(base, 10))
            .unwrap();
        queue
            .request(ControlKind::Pause(false), point(base, 30))
            .unwrap();
        queue.request(ControlKind::Stop, point(base, 50)).unwrap();
        let mut callbacks = Vec::new();
        let fence = state
            .apply(queue.take(point(base, 150)), |paused, point| {
                callbacks.push((paused, point));
            })
            .unwrap();
        assert_eq!(
            callbacks,
            [(true, point(base, 10)), (false, point(base, 30))]
        );
        assert_eq!(fence.point, point(base, 50));
        assert_eq!(fence.active, Duration::from_millis(30));
        assert!(fence.stopped && !state.is_paused());
        assert_eq!(
            state.clock().active_at(point(base, 50).instant),
            Some(fence.active)
        );
        assert_eq!(
            state.clock().active_at(point(base, 300).instant),
            Some(fence.active)
        );
        let spans = state.clock().active_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[1].key, spans[1].start, spans[1].end),
            (
                point(base, 30).instant,
                Duration::from_millis(10),
                Some(Duration::from_millis(30))
            )
        );

        queue.request(ControlKind::Stop, point(base, 0)).unwrap();
        queue
            .request(ControlKind::Pause(true), point(base, 0))
            .unwrap();
        assert_eq!(
            state
                .apply(queue.take(point(base, 300)), |_, _| panic!(
                    "停止后不能通知"
                ))
                .unwrap(),
            fence
        );
        let summary = state.clock().summary();
        assert_eq!(state.finish().unwrap(), fence);
        assert_eq!(state.finish().unwrap(), fence);
        assert_eq!(state.clock().summary(), summary);
        assert_eq!(
            state.clock().active_at(point(base, 400).instant),
            Some(fence.active)
        );
    }

    #[test]
    fn recording_control_state_prestart_intent_and_failure_do_not_start_time_early() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        for (millis, paused) in [(1, true), (2, false), (3, true)] {
            queue
                .request(ControlKind::Pause(paused), point(base, millis))
                .unwrap();
        }
        let mut state = ControlState::new(queue.begin(point(base, 10)));
        state.check_begin().unwrap();
        assert!(state.is_paused() && state.clock().is_paused());
        assert_eq!(state.latest().active, Duration::ZERO);
        queue
            .request(ControlKind::Pause(false), point(base, 20))
            .unwrap();
        let mut callbacks = Vec::new();
        let fence = state
            .apply(queue.take(point(base, 30)), |paused, point| {
                callbacks.push((paused, point));
            })
            .unwrap();
        assert_eq!(callbacks, [(false, point(base, 20))]);
        assert_eq!(fence.active, Duration::from_millis(10));
        assert_eq!(state.clock().active_spans()[0].key, point(base, 20).instant);

        let mut cancelled = ControlQueue::new();
        cancelled
            .request(ControlKind::Stop, point(base, 1))
            .unwrap();
        let mut state = ControlState::new(cancelled.begin(point(base, 10)));
        state.check_begin().unwrap();
        let zero = state.latest();
        assert_eq!(zero.point, point(base, 10));
        assert_eq!(zero.active, Duration::ZERO);
        assert!(zero.stopped && state.clock().is_paused());
        assert_eq!(
            state
                .apply(cancelled.take(point(base, 20)), |_, _| panic!(
                    "录前取消无Pause"
                ))
                .unwrap(),
            zero
        );
        assert_eq!(state.finish().unwrap(), zero);
        assert!(state.clock().active_spans().is_empty());

        let mut failed = ControlQueue::new();
        assert_eq!(
            failed.take(point(base, 0)).failure,
            Some(ControlFailure::NotBegun)
        );
        let mut state = ControlState::new(failed.begin(point(base, 10)));
        let before = state.latest();
        let summary = state.clock().summary();
        assert!(state.check_begin().is_err());
        assert!(state
            .apply(failed.take(point(base, 20)), |_, _| panic!(
                "坏begin不能通知"
            ))
            .is_err());
        assert!(state.finish().is_err());
        assert_eq!(state.latest(), before);
        assert_eq!(state.clock().summary(), summary);
    }

    #[test]
    fn recording_control_state_rejects_entire_invalid_batches_without_callbacks() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        let mut state = ControlState::new(queue.begin(point(base, 0)));
        state
            .apply(queue.take(point(base, 5)), |_, _| panic!("空批无通知"))
            .unwrap();
        queue
            .request(ControlKind::Pause(true), point(base, 10))
            .unwrap();
        queue
            .request(ControlKind::Pause(false), point(base, 20))
            .unwrap();
        let batch = queue.take(point(base, 30));
        let before = state.latest();
        let summary = state.clock().summary();
        for variant in 0..8 {
            let mut invalid = copy_batch(&batch);
            match variant {
                0 => invalid.events[1].seq += 1,
                1 => invalid.events[1].point = point(base, 9),
                2 => invalid.events[1].point = point(base, 31),
                3 => invalid.point = point(base, 4),
                4 => invalid.paused = true,
                5 => invalid.stop = Some(invalid.events[0]),
                6 => {
                    invalid.events[0].kind = ControlKind::Stop;
                    invalid.stop = Some(invalid.events[0]);
                }
                _ => {
                    invalid.failure = Some(ControlFailure::Capacity);
                    invalid.events[1].seq += 1;
                }
            }
            let error = state
                .apply(invalid, |_, _| panic!("非法批次不能部分通知"))
                .unwrap_err();
            if variant == 7 {
                assert_eq!(
                    error.downcast_ref::<ControlFailure>(),
                    Some(&ControlFailure::Capacity)
                );
            }
            assert_eq!(state.latest(), before);
            assert_eq!(state.clock().summary(), summary);
            assert_eq!(
                state.clock().active_at(point(base, 30).instant),
                Some(Duration::from_millis(30))
            );
            assert!(state.clock().active_spans()[0].end.is_none());
            assert!(!state.is_paused());
        }
        let mut callbacks = Vec::new();
        let fence = state
            .apply(batch, |paused, point| callbacks.push((paused, point)))
            .unwrap();
        assert_eq!(
            callbacks,
            [(true, point(base, 10)), (false, point(base, 20))]
        );
        assert_eq!(fence.active, Duration::from_millis(20));

        let mut exhausted = ControlState::new(ControlBegin {
            point: point(base, 0),
            seq_floor: u64::MAX,
            initial_paused: false,
            stop: None,
            failure: None,
        });
        let error = exhausted
            .apply(
                ControlBatch {
                    point: point(base, 2),
                    events: vec![ControlReceipt {
                        seq: 0,
                        point: point(base, 1),
                        kind: ControlKind::Pause(true),
                    }],
                    paused: true,
                    stop: None,
                    failure: None,
                },
                |_, _| panic!("序号溢出不能通知"),
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ControlFailure>(),
            Some(&ControlFailure::SequenceOverflow)
        );
        assert_eq!(exhausted.latest().active, Duration::ZERO);
        assert!(!exhausted.is_paused());
        assert!(exhausted.clock().active_spans()[0].end.is_none());
    }
}
