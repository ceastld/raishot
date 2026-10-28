//! 把原始 QPC 包锚映射到录制活动区段；不声称重采样后的 PCM 具有相同的精确时间。
use std::time::{Duration, Instant};

const CLOSED_SPAN_LIMIT: usize = 32;
const QPC_TICKS_PER_SECOND: u64 = 10_000_000;

/// 包围一次 Instant 读取的 QPC 区间，单位均为 100ns。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QpcWindow {
    pub earliest: u64,
    pub latest: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockPoint {
    pub instant: Instant,
    pub qpc: Option<QpcWindow>,
}
impl ClockPoint {
    pub(crate) fn now() -> Self {
        let before = crate::media_playback_sync::qpc_now_100ns();
        let instant = Instant::now();
        let after = crate::media_playback_sync::qpc_now_100ns();
        let qpc = match (before, after) {
            // QPC 换算会向下取整，上界多留一个 100ns 刻度以包含量化误差。
            (Some(earliest), Some(after)) if after >= earliest => after
                .checked_add(1)
                .map(|latest| QpcWindow { earliest, latest }),
            _ => None,
        };
        Self { instant, qpc }
    }
}

/// 活动时间的不确定范围；不是对某个重采样输出帧的时间估计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveRange {
    pub earliest: Duration,
    pub latest: Duration,
}

/// 可表示的活动区段；key 是原始控制锚的 Instant，不是重采样输出帧身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ActiveSpan {
    pub key: Instant,
    pub start: Duration,
    /// None 仅用于真正仍开放的当前区段。
    pub end: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockSummary {
    pub retained_spans: usize,
    pub active_span: bool,
    pub retired_spans: u64,
    pub missing_anchors: u64,
    pub invalid_anchors: u64,
}

#[derive(Clone, Copy, Debug)]
struct OpenSpan {
    start: ClockPoint,
    active_start: Duration,
    active_origin_valid: bool,
}
impl OpenSpan {
    fn active_at(&self, instant: Instant) -> Option<Duration> {
        if !self.active_origin_valid {
            return None;
        }
        self.active_start
            .checked_add(instant.checked_duration_since(self.start.instant)?)
    }

    fn active_span(&self) -> Option<ActiveSpan> {
        self.active_origin_valid.then_some(ActiveSpan {
            key: self.start.instant,
            start: self.active_start,
            end: None,
        })
    }

    fn map_window(&self, earliest: u64, latest: u64) -> Option<ActiveRange> {
        if latest < earliest {
            return None;
        }
        Some(ActiveRange {
            earliest: self.map(earliest)?.earliest,
            latest: self.map(latest)?.latest,
        })
    }
    fn map(&self, qpc: u64) -> Option<ActiveRange> {
        let start = self.start.qpc?;
        // 开始窗口内仍可能早于 Instant；latest 本身已不早于活动区段。
        if qpc < start.latest {
            return None;
        }
        Some(ActiveRange {
            earliest: self
                .active_start
                .checked_add(tick_duration(qpc.checked_sub(start.latest)?))?,
            latest: self
                .active_start
                .checked_add(tick_duration(qpc.checked_sub(start.earliest)?))?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ClosedSpan {
    span: OpenSpan,
    end: ClockPoint,
}
impl ClosedSpan {
    fn active_end(&self) -> Option<Duration> {
        self.span.active_at(self.end.instant)
    }

    fn active_span(&self) -> Option<ActiveSpan> {
        let end = self.active_end()?;
        // 同 Instant 的零长区段没有 PCM 归属，不能复用它的 key 认领后续区段。
        (end > self.span.active_start).then_some(ActiveSpan {
            key: self.span.start.instant,
            start: self.span.active_start,
            end: Some(end),
        })
    }

    fn map_window(&self, earliest: u64, latest: u64) -> Option<ActiveRange> {
        // 关闭锚缺失时整段拒绝，不能把已结束的区段当成无限活动。
        let end = self.end.qpc?;
        if latest >= end.earliest {
            return None;
        }
        self.span.map_window(earliest, latest)
    }
}

/// 最多保留 32 个已关闭区段及一个当前区段；暂停次数不增加内存占用。
#[derive(Clone, Debug)]
pub(crate) struct RecordingClock {
    closed: [Option<ClosedSpan>; CLOSED_SPAN_LIMIT],
    oldest: usize,
    closed_len: usize,
    current: Option<OpenSpan>,
    last_instant: Instant,
    qpc_floor: Option<u64>,
    active_floor: Duration,
    retired_spans: u64,
    missing_anchors: u64,
    invalid_anchors: u64,
}
impl RecordingClock {
    pub(crate) fn start(point: ClockPoint, active: Duration) -> Self {
        let mut clock = Self {
            closed: [None; CLOSED_SPAN_LIMIT],
            oldest: 0,
            closed_len: 0,
            current: None,
            last_instant: point.instant,
            qpc_floor: None,
            active_floor: active,
            retired_spans: 0,
            missing_anchors: 0,
            invalid_anchors: 0,
        };
        let point = clock.accept_point(point);
        clock.current = Some(OpenSpan {
            start: point,
            active_start: active,
            active_origin_valid: true,
        });
        clock
    }

    pub(crate) fn pause(&mut self, point: ClockPoint) {
        let Some(span) = self.current.take() else {
            return;
        };
        let active_end = (point.instant >= self.last_instant)
            .then(|| span.active_at(point.instant))
            .flatten();
        let mut end = self.accept_point(point);
        if let Some(active_end) = active_end {
            self.active_floor = self.active_floor.max(active_end);
        } else {
            // 无效起点、反向 Instant 或溢出的活动末尾不能建立关闭锚或改变已有下界。
            if end.qpc.take().is_some() {
                self.invalid_anchors = self.invalid_anchors.saturating_add(1);
            }
        }
        let index = (self.oldest + self.closed_len) % CLOSED_SPAN_LIMIT;
        self.closed[index] = Some(ClosedSpan { span, end });
        if self.closed_len < CLOSED_SPAN_LIMIT {
            self.closed_len += 1;
        } else {
            self.oldest = (self.oldest + 1) % CLOSED_SPAN_LIMIT;
            self.retired_spans = self.retired_spans.saturating_add(1);
        }
    }

    pub(crate) fn resume(&mut self, point: ClockPoint, active: Duration) {
        if self.current.is_some() {
            return;
        }
        let instant_valid = point.instant >= self.last_instant;
        let active_valid = active >= self.active_floor;
        let mut point = self.accept_point(point);
        if !active_valid {
            point.qpc = None;
            self.invalid_anchors = self.invalid_anchors.saturating_add(1);
        } else if instant_valid {
            self.active_floor = active;
        }
        self.current = Some(OpenSpan {
            start: point,
            active_start: active,
            active_origin_valid: instant_valid && active_valid,
        });
    }

    /// 只查询当前控制前缀之后的时刻；反向 Instant、无效原点或溢出返回 None。
    pub(crate) fn active_at(&self, instant: Instant) -> Option<Duration> {
        if instant < self.last_instant {
            return None;
        }
        if let Some(span) = &self.current {
            return span.active_at(instant);
        }
        // active_floor 只是下界；关闭失败时不能把它当作精确的停止时间。
        let index = (self.oldest + self.closed_len.checked_sub(1)?) % CLOSED_SPAN_LIMIT;
        self.closed[index]?.active_end()
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.current.is_none()
    }

    /// 顺序返回至多 32 个可表示的非零关闭段及一个当前段，不从 QPC 补造 Instant 端点。
    pub(super) fn active_spans(&self) -> Vec<ActiveSpan> {
        let mut spans = Vec::with_capacity(CLOSED_SPAN_LIMIT + 1);
        for offset in 0..self.closed_len {
            let index = (self.oldest + offset) % CLOSED_SPAN_LIMIT;
            if let Some(span) = self.closed[index].and_then(|span| span.active_span()) {
                spans.push(span);
            }
        }
        if let Some(span) = self.current.and_then(|span| span.active_span()) {
            spans.push(span);
        }
        spans
    }

    /// 将同一段的身份与原有 QPC 不确定范围一起返回；关闭段仍采用半开边界。
    pub(super) fn map_control_window(
        &self,
        earliest: u64,
        latest: u64,
    ) -> Option<(ActiveSpan, ActiveRange)> {
        if let Some(span) = &self.current {
            if let Some(mapped) = span.active_span().zip(span.map_window(earliest, latest)) {
                return Some(mapped);
            }
        }
        for offset in (0..self.closed_len).rev() {
            let index = (self.oldest + offset) % CLOSED_SPAN_LIMIT;
            if let Some(span) = self.closed[index] {
                if let Some(mapped) = span.active_span().zip(span.map_window(earliest, latest)) {
                    return Some(mapped);
                }
            }
        }
        None
    }

    pub(crate) fn map(&self, qpc: u64) -> Option<ActiveRange> {
        self.map_window(qpc, qpc)
    }

    /// 放置只使用最新区段；恢复后迟到的旧区段包不能重新进入输出。
    pub(crate) fn map_placement_window(&self, earliest: u64, latest: u64) -> Option<ActiveRange> {
        if let Some(span) = &self.current {
            return span.map_window(earliest, latest);
        }
        let index = (self.oldest + self.closed_len.checked_sub(1)?) % CLOSED_SPAN_LIMIT;
        self.closed[index]?.map_window(earliest, latest)
    }

    /// 整个 QPC 不确定窗口必须落在同一活动区段，不能跨越暂停边界。
    pub(crate) fn map_window(&self, earliest: u64, latest: u64) -> Option<ActiveRange> {
        if let Some(mapped) = self
            .current
            .as_ref()
            .and_then(|span| span.map_window(earliest, latest))
        {
            return Some(mapped);
        }
        for offset in (0..self.closed_len).rev() {
            let index = (self.oldest + offset) % CLOSED_SPAN_LIMIT;
            if let Some(mapped) =
                self.closed[index].and_then(|span| span.map_window(earliest, latest))
            {
                return Some(mapped);
            }
        }
        None
    }

    pub(crate) fn summary(&self) -> ClockSummary {
        ClockSummary {
            retained_spans: self.closed_len + usize::from(self.current.is_some()),
            active_span: self.current.is_some(),
            retired_spans: self.retired_spans,
            missing_anchors: self.missing_anchors,
            invalid_anchors: self.invalid_anchors,
        }
    }

    fn accept_point(&mut self, mut point: ClockPoint) -> ClockPoint {
        if point.qpc.is_none() {
            self.missing_anchors = self.missing_anchors.saturating_add(1);
        }
        let instant_valid = point.instant >= self.last_instant;
        self.last_instant = self.last_instant.max(point.instant);
        if let Some(window) = point.qpc {
            if !instant_valid
                || window.latest < window.earliest
                || self.qpc_floor.is_some_and(|floor| window.earliest < floor)
            {
                point.qpc = None;
                self.invalid_anchors = self.invalid_anchors.saturating_add(1);
            } else {
                self.qpc_floor = Some(window.latest);
            }
        } else if !instant_valid {
            self.invalid_anchors = self.invalid_anchors.saturating_add(1);
        }
        point
    }
}

fn tick_duration(ticks: u64) -> Duration {
    Duration::new(
        ticks / QPC_TICKS_PER_SECOND,
        (ticks % QPC_TICKS_PER_SECOND * 100) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(base: Instant, offset: u64, earliest: u64, latest: u64) -> ClockPoint {
        ClockPoint {
            instant: base + Duration::from_micros(offset),
            qpc: Some(QpcWindow { earliest, latest }),
        }
    }

    fn control_point(base: Instant, millis: u64) -> ClockPoint {
        let qpc = 10_000_000 + millis * 10_000;
        point(base, millis * 1_000, qpc, qpc)
    }

    #[test]
    fn recording_clock_delayed_control_receipts_keep_the_requested_stop_boundary() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(control_point(base, 0), Duration::ZERO);
        let running_copy = clock.clone();
        let pause = control_point(base, 200);
        assert_eq!(
            clock.active_at(pause.instant),
            Some(Duration::from_millis(200))
        );
        clock.pause(pause);
        assert!(clock.is_paused());
        let resume = control_point(base, 300);
        let active = clock.active_at(resume.instant).unwrap();
        assert_eq!(active, Duration::from_millis(200));
        clock.resume(resume, active);
        assert!(!clock.is_paused());
        let stop = control_point(base, 400);
        assert_eq!(
            clock.active_at(stop.instant),
            Some(Duration::from_millis(300))
        );
        clock.pause(stop);
        // 即便工作者700ms才收到并消费这批收据，尾点也不能扩到工作者当前时间。
        assert_eq!(
            clock.active_at(control_point(base, 700).instant),
            Some(Duration::from_millis(300))
        );
        assert_eq!(clock.active_at(control_point(base, 399).instant), None);
        assert_eq!(
            running_copy.active_at(control_point(base, 700).instant),
            Some(Duration::from_millis(700))
        );
    }

    #[test]
    fn recording_clock_control_windows_return_the_owning_historical_span() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(control_point(base, 0), Duration::ZERO);
        clock.pause(control_point(base, 200));
        clock.resume(control_point(base, 300), Duration::from_millis(200));
        clock.pause(control_point(base, 400));
        let spans = clock.active_spans();
        assert_eq!(
            spans,
            [
                ActiveSpan {
                    key: base,
                    start: Duration::ZERO,
                    end: Some(Duration::from_millis(200))
                },
                ActiveSpan {
                    key: base + Duration::from_millis(300),
                    start: Duration::from_millis(200),
                    end: Some(Duration::from_millis(300))
                },
            ]
        );
        let qpc = |millis| control_point(base, millis).qpc.unwrap().earliest;
        for (millis, span, active) in [
            (100, spans[0], 100),
            (300, spans[1], 200),
            (350, spans[1], 250),
        ] {
            assert_eq!(
                clock.map_control_window(qpc(millis), qpc(millis)),
                Some((
                    span,
                    ActiveRange {
                        earliest: Duration::from_millis(active),
                        latest: Duration::from_millis(active),
                    }
                ))
            );
        }
        for (earliest, latest) in [
            (199, 200),
            (200, 200),
            (250, 250),
            (399, 400),
            (400, 400),
            (350, 349),
        ] {
            assert_eq!(clock.map_control_window(qpc(earliest), qpc(latest)), None);
        }
        // 同一闭合边界的不确定区间不能越过暂停或恢复锚。
        assert!(clock.map_control_window(qpc(199), qpc(301)).is_none());
    }

    #[test]
    fn recording_clock_zero_instant_span_cannot_own_pcm_or_duplicate_a_resumed_key() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 100, 100), Duration::ZERO);
        clock.pause(point(base, 0, 200, 200));
        assert_eq!(clock.active_at(base), Some(Duration::ZERO));
        assert!(clock.active_spans().is_empty());
        assert!(clock.map_control_window(150, 150).is_none());
        // 保留旧映射合同；只有新增的控制归属接口排除零 Instant 段。
        assert!(clock.map_window(150, 150).is_some());
        clock.resume(point(base, 0, 300, 300), Duration::ZERO);
        let open = ActiveSpan {
            key: base,
            start: Duration::ZERO,
            end: None,
        };
        assert_eq!(clock.active_spans(), [open]);
        assert_eq!(clock.map_control_window(310, 310).unwrap().0, open);
        assert!(clock.map_control_window(150, 150).is_none());
    }

    #[test]
    fn recording_clock_control_spans_reject_invalid_instant_origins_and_overflow() {
        let base = Instant::now();
        let mut reversed = RecordingClock::start(point(base, 10, 100, 100), Duration::ZERO);
        assert_eq!(reversed.active_at(base), None);
        reversed.pause(point(base, 0, 200, 200));
        assert!(reversed.active_spans().is_empty());
        assert_eq!(reversed.active_at(base + Duration::from_micros(20)), None);
        assert!(reversed.map_control_window(150, 150).is_none());

        let mut invalid_origin = RecordingClock::start(control_point(base, 0), Duration::ZERO);
        invalid_origin.pause(control_point(base, 20));
        invalid_origin.resume(control_point(base, 10), Duration::from_millis(20));
        assert_eq!(
            invalid_origin.active_at(control_point(base, 30).instant),
            None
        );
        assert_eq!(invalid_origin.active_spans().len(), 1);
        invalid_origin.pause(control_point(base, 40));
        assert_eq!(
            invalid_origin.active_at(control_point(base, 50).instant),
            None
        );
        assert_eq!(invalid_origin.active_spans().len(), 1);

        let mut overflow = RecordingClock::start(point(base, 0, 100, 100), Duration::MAX);
        assert_eq!(overflow.active_at(base), Some(Duration::MAX));
        assert_eq!(overflow.active_at(base + Duration::from_micros(1)), None);
        overflow.pause(point(base, 10, 200, 200));
        assert!(overflow.active_spans().is_empty());
        assert_eq!(overflow.active_at(base + Duration::from_micros(20)), None);
        assert!(overflow.map_control_window(150, 150).is_none());

        // QPC缺失不否认有效Instant边界，但也不能补造可映射的设备锚。
        let mut no_qpc = RecordingClock::start(
            ClockPoint {
                instant: base,
                qpc: None,
            },
            Duration::ZERO,
        );
        no_qpc.pause(ClockPoint {
            instant: base + Duration::from_millis(10),
            qpc: None,
        });
        assert_eq!(
            no_qpc.active_at(base + Duration::from_millis(20)),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            no_qpc.active_spans()[0].end,
            Some(Duration::from_millis(10))
        );
        assert!(no_qpc.map_control_window(0, u64::MAX).is_none());
    }

    #[test]
    fn recording_clock_window_cannot_bridge_a_pause() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 100, 100), Duration::ZERO);
        clock.pause(point(base, 10, 200, 200));
        clock.resume(point(base, 20, 300, 300), Duration::from_micros(10));
        assert!(clock.map(190).is_some());
        assert!(clock.map(310).is_some());
        assert!(clock.map_placement_window(190, 190).is_none());
        assert!(clock.map_placement_window(310, 320).is_some());
        assert!(clock.map_window(190, 310).is_none());
        assert!(clock.map_window(190, 180).is_none());
        assert_eq!(
            clock.map_window(310, 320),
            Some(ActiveRange {
                earliest: Duration::from_micros(11),
                latest: Duration::from_micros(12),
            })
        );
    }

    #[test]
    fn recording_clock_pause_gap_does_not_advance_active_time() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 1_000_000, 1_000_000), Duration::ZERO);
        clock.pause(point(base, 10_000, 1_100_000, 1_100_000));
        clock.resume(
            point(base, 40_000, 1_400_000, 1_400_000),
            Duration::from_millis(10),
        );
        assert_eq!(
            clock.map(1_050_000),
            Some(ActiveRange {
                earliest: Duration::from_millis(5),
                latest: Duration::from_millis(5),
            })
        );
        assert_eq!(clock.map(1_100_000), None);
        assert_eq!(clock.map(1_300_000), None);
        assert_eq!(
            clock.map(1_450_000),
            Some(ActiveRange {
                earliest: Duration::from_millis(15),
                latest: Duration::from_millis(15),
            })
        );
        assert_eq!(clock.summary().retained_spans, 2);

        let mut backwards_active = RecordingClock::start(point(base, 0, 0, 0), Duration::ZERO);
        backwards_active.pause(point(base, 10_000_000, 100_000_000, 100_000_000));
        backwards_active.resume(
            point(base, 20_000_000, 200_000_000, 200_000_000),
            Duration::from_secs(5),
        );
        assert_eq!(backwards_active.map(205_000_000), None);
        backwards_active.pause(point(base, 21_000_000, 210_000_000, 210_000_000));
        backwards_active.resume(
            point(base, 22_000_000, 220_000_000, 220_000_000),
            Duration::from_secs(10),
        );
        assert_eq!(
            backwards_active.map(221_000_000),
            Some(ActiveRange {
                earliest: Duration::from_millis(10_100),
                latest: Duration::from_millis(10_100),
            })
        );
    }

    #[test]
    fn recording_clock_rejects_brackets_and_keeps_mapping_uncertainty() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 100, 110), Duration::ZERO);
        clock.pause(point(base, 10, 200, 210));
        for qpc in [99, 100, 109, 200, 205, 210, 299] {
            assert_eq!(clock.map(qpc), None, "{qpc}");
        }
        assert_eq!(
            clock.map(110),
            Some(ActiveRange {
                earliest: Duration::ZERO,
                latest: Duration::from_nanos(1_000),
            })
        );
        assert_eq!(
            clock.map(199),
            Some(ActiveRange {
                earliest: Duration::from_nanos(8_900),
                latest: Duration::from_nanos(9_900),
            })
        );
        clock.resume(point(base, 20, 300, 310), Duration::from_micros(10));
        assert_eq!(clock.map(309), None);
        assert_eq!(
            clock.map(310),
            Some(ActiveRange {
                earliest: Duration::from_micros(10),
                latest: Duration::from_micros(11),
            })
        );
    }

    #[test]
    fn recording_clock_retires_old_spans_at_a_fixed_limit() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 0, 0), Duration::ZERO);
        for index in 0..34 {
            let qpc = index * 100 + 50;
            clock.pause(point(base, qpc / 10, qpc, qpc));
            let qpc = (index + 1) * 100;
            clock.resume(
                point(base, qpc / 10, qpc, qpc),
                tick_duration((index + 1) * 50),
            );
        }
        assert_eq!(clock.map(10), None);
        assert_eq!(clock.map(110), None);
        assert!(clock.map(210).is_some());
        let spans = clock.active_spans();
        assert_eq!(spans.len(), CLOSED_SPAN_LIMIT + 1);
        assert_eq!(spans.first().unwrap().key, base + Duration::from_micros(20));
        assert_eq!(spans.last().unwrap().key, base + Duration::from_micros(340));
        assert!(spans[..CLOSED_SPAN_LIMIT]
            .iter()
            .all(|span| span.end.is_some()));
        assert_eq!(spans.last().unwrap().end, None);
        assert_eq!(
            clock.summary(),
            ClockSummary {
                retained_spans: 33,
                active_span: true,
                retired_spans: 2,
                missing_anchors: 0,
                invalid_anchors: 0,
            }
        );
    }

    #[test]
    fn recording_clock_missing_and_reversed_anchors_never_create_open_mappings() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0, 100, 100), Duration::ZERO);
        clock.pause(ClockPoint {
            instant: base + Duration::from_micros(10),
            qpc: None,
        });
        assert_eq!(clock.map(150), None);
        assert_eq!(clock.map(u64::MAX), None);
        clock.resume(point(base, 20, 300, 300), Duration::from_micros(10));
        assert!(clock.map(310).is_some());
        clock.pause(point(base, 30, 400, 400));
        clock.resume(point(base, 40, 450, 440), Duration::from_micros(20));
        assert_eq!(clock.map(460), None);
        clock.pause(point(base, 50, 500, 500));
        clock.resume(point(base, 60, 480, 490), Duration::from_micros(30));
        assert_eq!(clock.map(510), None);
        clock.pause(point(base, 70, 600, 600));
        clock.resume(point(base, 80, 700, 700), Duration::MAX);
        assert_eq!(clock.map(701), None);
        assert_eq!(clock.summary().missing_anchors, 1);
        assert_eq!(clock.summary().invalid_anchors, 2);

        let mut reversed_instant = RecordingClock::start(point(base, 10, 100, 100), Duration::ZERO);
        reversed_instant.pause(point(base, 0, 200, 200));
        assert_eq!(reversed_instant.map(150), None);
        assert_eq!(reversed_instant.summary().invalid_anchors, 1);

        let mut invalid_origin = RecordingClock::start(point(base, 0, 100, 100), Duration::ZERO);
        invalid_origin.pause(point(base, 10, 200, 200));
        invalid_origin.resume(point(base, 5, 300, 300), Duration::from_micros(10));
        invalid_origin.pause(point(base, 20, 400, 400));
        invalid_origin.resume(point(base, 30, 500, 500), Duration::from_micros(10));
        assert_eq!(
            invalid_origin.map(510),
            Some(ActiveRange {
                earliest: Duration::from_micros(11),
                latest: Duration::from_micros(11),
            })
        );

        let mut overflow = RecordingClock::start(point(base, 0, 100, 100), Duration::MAX);
        overflow.pause(point(base, 10, 200, 200));
        assert_eq!(overflow.map(150), None);
        assert_eq!(overflow.summary().invalid_anchors, 1);
    }
}
