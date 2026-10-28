//! 调用方在同一把控制锁内传入时间点；本叶只保存有序收据，不采时或加锁。
use super::clock::ClockPoint;
use std::{error::Error, fmt};

const CAPACITY: usize = 64;
const PAUSE_CAPACITY: usize = CAPACITY - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlKind {
    Pause(bool),
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ControlReceipt {
    pub seq: u64,
    pub point: ClockPoint,
    pub kind: ControlKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlFailure {
    Capacity,
    SequenceOverflow,
    ReversedInstant,
    AlreadyBegun,
    NotBegun,
}

impl fmt::Display for ControlFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Capacity => tr!("录屏控制队列已满", "Recording control queue is full"),
            Self::SequenceOverflow => {
                tr!("录屏控制序号溢出", "Recording control sequence overflowed")
            }
            Self::ReversedInstant => {
                tr!(
                    "录屏控制时间点逆序",
                    "Recording control time moved backwards"
                )
            }
            Self::AlreadyBegun => {
                tr!(
                    "录屏控制队列已经起钟",
                    "Recording control queue has already begun"
                )
            }
            Self::NotBegun => {
                tr!(
                    "录屏控制队列尚未起钟",
                    "Recording control queue has not begun"
                )
            }
        })
    }
}

impl Error for ControlFailure {}

#[derive(Debug)]
pub(super) struct ControlBegin {
    pub point: ClockPoint,
    pub seq_floor: u64,
    pub initial_paused: bool,
    pub stop: Option<ControlReceipt>,
    pub failure: Option<ControlFailure>,
}

#[derive(Debug)]
pub(super) struct ControlBatch {
    pub point: ClockPoint,
    pub events: Vec<ControlReceipt>,
    pub paused: bool,
    pub stop: Option<ControlReceipt>,
    pub failure: Option<ControlFailure>,
}

pub(super) struct ControlQueue {
    begun: bool,
    paused: bool,
    seq: u64,
    last_observed: Option<ClockPoint>,
    stop: Option<ControlReceipt>,
    failure: Option<ControlFailure>,
    events: Vec<ControlReceipt>,
}

impl ControlQueue {
    pub(super) fn new() -> Self {
        Self {
            begun: false,
            paused: false,
            seq: 0,
            last_observed: None,
            stop: None,
            failure: None,
            events: Vec::with_capacity(CAPACITY),
        }
    }

    pub(super) fn request(
        &mut self,
        kind: ControlKind,
        point: ClockPoint,
    ) -> Result<(), ControlFailure> {
        // 首次 Stop 的身份和时间固定；迟到或重复请求也不再改变错误或观察边界。
        if self.stop.is_some() {
            return Ok(());
        }
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        self.observe(point)?;
        if matches!(kind, ControlKind::Pause(paused) if paused == self.paused) {
            return Ok(());
        }
        if self.begun {
            let limit = match kind {
                ControlKind::Pause(_) => PAUSE_CAPACITY,
                ControlKind::Stop => CAPACITY,
            };
            if self.events.len() >= limit {
                return Err(self.fail(ControlFailure::Capacity));
            }
        }
        let Some(seq) = self.seq.checked_add(1) else {
            return Err(self.fail(ControlFailure::SequenceOverflow));
        };
        let receipt = ControlReceipt { seq, point, kind };
        self.seq = seq;
        match kind {
            ControlKind::Pause(paused) => self.paused = paused,
            ControlKind::Stop => self.stop = Some(receipt),
        }
        if self.begun {
            self.events.push(receipt);
        }
        Ok(())
    }

    pub(super) fn paused(&self) -> bool {
        self.paused
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.is_some() || self.failure.is_some()
    }

    pub(super) fn failure(&self) -> Option<ControlFailure> {
        self.failure
    }

    pub(super) fn stop_receipt(&self) -> Option<ControlReceipt> {
        self.stop
    }

    pub(super) fn begin(&mut self, point: ClockPoint) -> ControlBegin {
        if self.begun {
            self.fail(ControlFailure::AlreadyBegun);
        } else {
            self.begun = true;
            let _ = self.observe(point);
            // 起钟前只保留最终暂停意图与序号下界，不回放准备期间的暂停。
            self.events.clear();
        }
        ControlBegin {
            point,
            seq_floor: self.seq,
            initial_paused: self.paused,
            stop: self.stop,
            failure: self.failure,
        }
    }

    /// point 始终原样返回。failure 优先；坏 fence 不能用于关闭活动区间。
    /// 即使 fence 失败，已有事件也交给调用方保留证据，不伪造或替换事件。
    pub(super) fn take(&mut self, point: ClockPoint) -> ControlBatch {
        if !self.begun {
            self.fail(ControlFailure::NotBegun);
        } else {
            let _ = self.observe(point);
        }
        let mut events = Vec::new();
        if !self.events.is_empty() {
            events.reserve_exact(self.events.len());
            events.extend(self.events.drain(..));
        }
        ControlBatch {
            point,
            events,
            paused: self.paused,
            stop: self.stop,
            failure: self.failure,
        }
    }

    fn observe(&mut self, point: ClockPoint) -> Result<(), ControlFailure> {
        // 必须先比较真实 Instant；不能用可能把逆序视为零间隔的时差运算。
        if self
            .last_observed
            .is_some_and(|previous| point.instant < previous.instant)
        {
            return Err(self.fail(ControlFailure::ReversedInstant));
        }
        self.last_observed = Some(point);
        Ok(())
    }

    fn fail(&mut self, failure: ControlFailure) -> ControlFailure {
        *self.failure.get_or_insert(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn point(base: Instant, nanos: u64) -> ClockPoint {
        ClockPoint {
            instant: base + Duration::from_nanos(nanos),
            qpc: None,
        }
    }

    #[test]
    fn recording_control_receipt_keeps_fast_pause_resume_in_order() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        let begin = queue.begin(point(base, 0));
        assert_eq!(begin.point, point(base, 0));
        assert_eq!(begin.seq_floor, 0);
        assert!(!begin.initial_paused);
        assert!(begin.failure.is_none());
        queue
            .request(ControlKind::Pause(true), point(base, 10))
            .unwrap();
        queue
            .request(ControlKind::Pause(true), point(base, 11))
            .unwrap();
        queue
            .request(ControlKind::Pause(false), point(base, 12))
            .unwrap();
        let capacity = queue.events.capacity();
        let batch = queue.take(point(base, 20));
        assert_eq!(batch.point, point(base, 20));
        assert_eq!(
            batch.events,
            [
                ControlReceipt {
                    seq: 1,
                    point: point(base, 10),
                    kind: ControlKind::Pause(true)
                },
                ControlReceipt {
                    seq: 2,
                    point: point(base, 12),
                    kind: ControlKind::Pause(false)
                },
            ]
        );
        assert!(!batch.paused);
        assert!(batch.stop.is_none() && batch.failure.is_none());
        assert_eq!(queue.events.capacity(), capacity);
        let empty = queue.take(point(base, 21));
        assert!(empty.events.is_empty());
        assert_eq!(empty.events.capacity(), 0);
        assert!(!queue.stopped());
    }

    #[test]
    fn recording_control_receipt_folds_prestart_and_preserves_stop_floor() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        for (nanos, paused) in [(1, true), (2, false), (3, true)] {
            queue
                .request(ControlKind::Pause(paused), point(base, nanos))
                .unwrap();
        }
        assert!(queue.events.is_empty());
        let begin = queue.begin(point(base, 4));
        assert_eq!(begin.seq_floor, 3);
        assert!(begin.initial_paused && begin.stop.is_none() && begin.failure.is_none());
        queue
            .request(ControlKind::Pause(false), point(base, 5))
            .unwrap();
        let batch = queue.take(point(base, 6));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].seq, begin.seq_floor + 1);
        assert_eq!(batch.events[0].kind, ControlKind::Pause(false));

        let mut cancelled = ControlQueue::new();
        cancelled
            .request(ControlKind::Pause(true), point(base, 1))
            .unwrap();
        cancelled
            .request(ControlKind::Stop, point(base, 2))
            .unwrap();
        let stop = cancelled.stop_receipt().unwrap();
        cancelled
            .request(ControlKind::Pause(false), point(base, 0))
            .unwrap();
        let begin = cancelled.begin(point(base, 3));
        assert_eq!(begin.seq_floor, 2);
        assert_eq!(begin.stop, Some(stop));
        assert!(begin.initial_paused && begin.failure.is_none());
        let batch = cancelled.take(point(base, 4));
        assert!(batch.events.is_empty());
        assert_eq!(batch.stop, Some(stop));
        assert!(cancelled.stopped());
    }

    #[test]
    fn recording_control_receipt_terminal_requests_preserve_the_first_stop() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        queue.begin(point(base, 0));
        queue
            .request(ControlKind::Pause(true), point(base, 1))
            .unwrap();
        queue.request(ControlKind::Stop, point(base, 2)).unwrap();
        let stop = queue.stop_receipt().unwrap();
        for kind in [
            ControlKind::Stop,
            ControlKind::Pause(false),
            ControlKind::Pause(true),
        ] {
            queue.request(kind, point(base, 0)).unwrap();
        }
        assert_eq!(queue.last_observed, Some(point(base, 2)));
        assert_eq!(queue.stop_receipt(), Some(stop));
        assert_eq!(stop.seq, 2);
        assert!(queue.paused() && queue.stopped());
        assert!(queue.failure().is_none());
        let batch = queue.take(point(base, 3));
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[1], stop);
        queue.request(ControlKind::Stop, point(base, 0)).unwrap();
        assert_eq!(queue.last_observed, Some(point(base, 3)));
        let repeated = queue.take(point(base, 4));
        assert!(repeated.events.is_empty());
        assert_eq!(repeated.stop, Some(stop));
        assert!(repeated.failure.is_none());
    }

    #[test]
    fn recording_control_receipt_distinguishes_equal_and_reversed_instants() {
        let base = Instant::now();
        let mut queue = ControlQueue::new();
        queue.begin(point(base, 1));
        queue
            .request(ControlKind::Pause(true), point(base, 1))
            .unwrap();
        queue
            .request(ControlKind::Pause(false), point(base, 1))
            .unwrap();
        let batch = queue.take(point(base, 2));
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].point, batch.events[1].point);
        assert_eq!((batch.events[0].seq, batch.events[1].seq), (1, 2));
        assert_eq!(
            queue.request(ControlKind::Pause(true), point(base, 1)),
            Err(ControlFailure::ReversedInstant)
        );
        assert!(!queue.paused());
        assert_eq!(queue.last_observed, Some(point(base, 2)));
        assert!(queue.stopped());

        let mut bad_fence = ControlQueue::new();
        bad_fence.begin(point(base, 0));
        bad_fence
            .request(ControlKind::Pause(true), point(base, 2))
            .unwrap();
        let batch = bad_fence.take(point(base, 1));
        assert_eq!(batch.point, point(base, 1));
        assert_eq!(batch.failure, Some(ControlFailure::ReversedInstant));
        assert_eq!(batch.events.len(), 1); // 证据仍保留，坏 fence 不能用作边界。
        assert_eq!(bad_fence.last_observed, Some(point(base, 2)));

        let mut bad_begin = ControlQueue::new();
        bad_begin
            .request(ControlKind::Pause(true), point(base, 2))
            .unwrap();
        assert_eq!(
            bad_begin.begin(point(base, 1)).failure,
            Some(ControlFailure::ReversedInstant)
        );
    }

    #[test]
    fn recording_control_receipt_bounds_and_first_failure_remain_terminal() {
        let base = Instant::now();
        let fill = |queue: &mut ControlQueue| {
            queue.begin(point(base, 0));
            for seq in 1..=PAUSE_CAPACITY {
                queue
                    .request(ControlKind::Pause(seq % 2 != 0), point(base, seq as u64))
                    .unwrap();
            }
        };
        let mut full = ControlQueue::new();
        fill(&mut full);
        full.request(ControlKind::Stop, point(base, 64)).unwrap();
        let batch = full.take(point(base, 65));
        assert_eq!(batch.events.len(), CAPACITY);
        assert_eq!(batch.events.last(), batch.stop.as_ref());
        assert!(batch.failure.is_none());

        let mut overflow = ControlQueue::new();
        fill(&mut overflow);
        assert_eq!(
            overflow.request(ControlKind::Pause(false), point(base, 64)),
            Err(ControlFailure::Capacity)
        );
        assert!(overflow.paused() && overflow.stopped());
        assert_eq!(
            overflow.request(ControlKind::Stop, point(base, 0)),
            Err(ControlFailure::Capacity)
        );
        let batch = overflow.take(point(base, 65));
        assert_eq!(batch.events.len(), PAUSE_CAPACITY);
        assert_eq!(batch.events.last().unwrap().seq, PAUSE_CAPACITY as u64);
        assert_eq!(batch.failure, Some(ControlFailure::Capacity));
        assert!(batch.stop.is_none());

        let mut sequence = ControlQueue::new();
        sequence.begin(point(base, 0));
        sequence.seq = u64::MAX;
        assert_eq!(
            sequence.request(ControlKind::Pause(true), point(base, 1)),
            Err(ControlFailure::SequenceOverflow)
        );
        assert!(!sequence.paused());
        assert!(sequence.events.is_empty() && sequence.stopped());
        assert_eq!(
            sequence.request(ControlKind::Stop, point(base, 0)),
            Err(ControlFailure::SequenceOverflow)
        );

        let mut unbegun = ControlQueue::new();
        assert_eq!(
            unbegun.take(point(base, 0)).failure,
            Some(ControlFailure::NotBegun)
        );
        assert_eq!(
            unbegun.request(ControlKind::Pause(true), point(base, 1)),
            Err(ControlFailure::NotBegun)
        );
        unbegun.begin(point(base, 2));
        assert_eq!(
            unbegun.begin(point(base, 3)).failure,
            Some(ControlFailure::NotBegun)
        );

        let mut begun = ControlQueue::new();
        begun.begin(point(base, 0));
        begun
            .request(ControlKind::Pause(true), point(base, 1))
            .unwrap();
        assert_eq!(
            begun.begin(point(base, 2)).failure,
            Some(ControlFailure::AlreadyBegun)
        );
        let batch = begun.take(point(base, 3));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.failure, Some(ControlFailure::AlreadyBegun));
    }
}
