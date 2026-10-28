//! 原始采集端点的设备计数/QPC 漂移观测；不调整 PCM、重采样或成片时间轴。
//! sample_rate 与 ppm 仅描述此设备时钟相对标称采样率的差异，不代表音视频同步实测。
use super::RawAudioTime;
use std::collections::VecDeque;

const QPC_SECOND: u64 = 10_000_000;
const MIN_SPAN_100NS: u64 = QPC_SECOND;
const MAX_SPAN_100NS: u64 = QPC_SECOND * 5;
const POINT_INTERVAL_100NS: u64 = QPC_SECOND / 10;
const MAX_POINTS: usize = 64;
const MAX_PACKET_GAP_100NS: u64 = QPC_SECOND;
// 单包允许时间戳量化/抖动；完整窗口仍须落在更窄的可信范围内。
const MAX_INTERVAL_PPM: u64 = 100_000;
const MAX_WINDOW_PPM: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureClockReanchor {
    Started,
    TimestampRestored,
    EndpointChanged,
    RateChanged,
    Discontinuity,
    QpcRepeated,
    QpcRegressed,
    DeviceRepeated,
    DeviceRegressed,
    PacketGap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureClockIssue {
    InvalidRate,
    InvalidInputFrames,
    PositionOverflow,
    ObservationGap,
    ImplausibleDelta,
    ImplausibleWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureClockStatus {
    Empty,
    MissingQpc,
    Reanchored(CaptureClockReanchor),
    InsufficientSpan,
    Ready,
    Untrusted(CaptureClockIssue),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureClockSnapshot {
    /// 连续窗口的身份 token，只作等值比较；断开后回绕推进，正常滑窗保持不变。
    pub generation: u64,
    pub status: CaptureClockStatus,
    /// 最新原始包；节流只影响窗口存点，不改写或跳过逐包连续性检查。
    pub latest: Option<RawAudioTime>,
    pub window_start: Option<RawAudioTime>,
    pub window_end: Option<RawAudioTime>,
    pub point_count: usize,
    /// 窗口首末包起始 QPC 的差，不给末包补算持续时间。
    pub span_100ns: u64,
    /// 同一窗口首末包起始设备计数的差，不等同于消费或成片帧数。
    pub device_frames: u64,
    pub sample_rate: Option<f64>,
    /// 正值表示设备计数相对标称率更快；非 Ready 状态不发布数值。
    pub ppm: Option<f64>,
}

impl CaptureClockSnapshot {
    /// 确认原始包起点落在当前已验证连续窗口内；上界用逐包 latest，避免存点节流漏包。
    pub(crate) fn accepts_raw_timestamp(&self, raw: RawAudioTime) -> bool {
        if !matches!(
            self.status,
            CaptureClockStatus::InsufficientSpan | CaptureClockStatus::Ready
        ) {
            return false;
        }
        let (Some(start), Some(latest)) = (self.window_start, self.latest) else {
            return false;
        };
        let (Some(start_qpc), Some(latest_qpc), Some(qpc)) =
            (start.qpc_100ns, latest.qpc_100ns, raw.qpc_100ns)
        else {
            return false;
        };
        // InsufficientSpan 只在连续后继包通过后产生；双计数严格前进再次确认至少两包，
        // 无须等待漂移拟合的一秒窗口，也无须要求节流后的 point_count 达到二。
        start.endpoint_epoch == latest.endpoint_epoch
            && raw.endpoint_epoch == start.endpoint_epoch
            && start.device_rate == latest.device_rate
            && raw.device_rate == start.device_rate
            && start.device_position < latest.device_position
            && start_qpc < latest_qpc
            && (start.device_position..=latest.device_position).contains(&raw.device_position)
            && (start_qpc..=latest_qpc).contains(&qpc)
    }
}

pub(crate) struct CaptureClockObserver {
    generation: u64,
    points: VecDeque<RawAudioTime>,
    previous: Option<RawAudioTime>,
    latest: Option<RawAudioTime>,
    status: CaptureClockStatus,
}
impl Default for CaptureClockObserver {
    fn default() -> Self {
        Self::new()
    }
}
impl CaptureClockObserver {
    pub(crate) fn new() -> Self {
        Self {
            generation: 0,
            points: VecDeque::new(),
            previous: None,
            latest: None,
            status: CaptureClockStatus::Empty,
        }
    }

    /// 每个原始采集包都要调用，包括重采样后没有输出 PCM 的包。
    pub(crate) fn observe(&mut self, raw: RawAudioTime) -> CaptureClockSnapshot {
        self.latest = Some(raw);
        if raw.device_rate == 0 || raw.device_rate > 384_000 {
            return self.clear(CaptureClockStatus::Untrusted(
                CaptureClockIssue::InvalidRate,
            ));
        }
        if raw.input_frames == 0 || raw.input_frames > raw.device_rate {
            return self.clear(CaptureClockStatus::Untrusted(
                CaptureClockIssue::InvalidInputFrames,
            ));
        }
        let Some(qpc) = raw.qpc_100ns else {
            return self.clear(CaptureClockStatus::MissingQpc);
        };
        let Some(previous) = self.previous else {
            let reason = if self.status == CaptureClockStatus::MissingQpc {
                CaptureClockReanchor::TimestampRestored
            } else if raw.discontinuity {
                CaptureClockReanchor::Discontinuity
            } else {
                CaptureClockReanchor::Started
            };
            return self.reanchor(raw, CaptureClockStatus::Reanchored(reason));
        };
        let reason = if raw.endpoint_epoch != previous.endpoint_epoch {
            Some(CaptureClockReanchor::EndpointChanged)
        } else if raw.device_rate != previous.device_rate {
            Some(CaptureClockReanchor::RateChanged)
        } else if raw.discontinuity {
            Some(CaptureClockReanchor::Discontinuity)
        } else {
            None
        };
        if let Some(reason) = reason {
            return self.reanchor(raw, CaptureClockStatus::Reanchored(reason));
        }
        let previous_qpc = previous.qpc_100ns.unwrap();
        let reason = if qpc == previous_qpc {
            Some(CaptureClockReanchor::QpcRepeated)
        } else if qpc < previous_qpc {
            Some(CaptureClockReanchor::QpcRegressed)
        } else if raw.device_position == previous.device_position {
            Some(CaptureClockReanchor::DeviceRepeated)
        } else if raw.device_position < previous.device_position {
            Some(CaptureClockReanchor::DeviceRegressed)
        } else {
            None
        };
        if let Some(reason) = reason {
            return self.reanchor(raw, CaptureClockStatus::Reanchored(reason));
        }

        // 先做整数差，再转为频率；大绝对计数和 QPC 不参与浮点相减。
        let span = qpc - previous_qpc;
        let frames = raw.device_position - previous.device_position;
        let issue = if span > MAX_PACKET_GAP_100NS {
            Some(CaptureClockIssue::ObservationGap)
        } else if !plausible(frames, span, raw.device_rate, MAX_INTERVAL_PPM) {
            Some(CaptureClockIssue::ImplausibleDelta)
        } else {
            None
        };
        if let Some(issue) = issue {
            return self.reanchor(raw, CaptureClockStatus::Untrusted(issue));
        }
        let Some(expected_position) = previous
            .device_position
            .checked_add(u64::from(previous.input_frames))
        else {
            return self.reanchor(
                raw,
                CaptureClockStatus::Untrusted(CaptureClockIssue::PositionOverflow),
            );
        };
        if raw.device_position != expected_position {
            return self.reanchor(
                raw,
                CaptureClockStatus::Reanchored(CaptureClockReanchor::PacketGap),
            );
        }
        self.previous = Some(raw);
        if qpc - self.points.back().unwrap().qpc_100ns.unwrap() >= POINT_INTERVAL_100NS {
            self.points.push_back(raw);
        }
        while self
            .points
            .front()
            .is_some_and(|point| qpc - point.qpc_100ns.unwrap() > MAX_SPAN_100NS)
            || self.points.len() > MAX_POINTS
        {
            self.points.pop_front();
        }
        let (window_frames, window_span) = self.window_delta();
        if window_span < MIN_SPAN_100NS {
            self.status = CaptureClockStatus::InsufficientSpan;
        } else if !plausible(window_frames, window_span, raw.device_rate, MAX_WINDOW_PPM) {
            return self.reanchor(
                raw,
                CaptureClockStatus::Untrusted(CaptureClockIssue::ImplausibleWindow),
            );
        } else {
            self.status = CaptureClockStatus::Ready;
        }
        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> CaptureClockSnapshot {
        let (device_frames, span_100ns) = self.window_delta();
        let estimate = if self.status == CaptureClockStatus::Ready {
            self.points
                .front()
                .map(|point| estimate(device_frames, span_100ns, point.device_rate))
        } else {
            None
        };
        CaptureClockSnapshot {
            generation: self.generation,
            status: self.status,
            latest: self.latest,
            window_start: self.points.front().copied(),
            window_end: self.points.back().copied(),
            point_count: self.points.len(),
            span_100ns,
            device_frames,
            sample_rate: estimate.map(|value| value.0),
            ppm: estimate.map(|value| value.1),
        }
    }

    fn clear(&mut self, status: CaptureClockStatus) -> CaptureClockSnapshot {
        self.generation = self.generation.wrapping_add(1);
        self.points.clear();
        self.previous = None;
        self.status = status;
        self.snapshot()
    }

    fn reanchor(&mut self, raw: RawAudioTime, status: CaptureClockStatus) -> CaptureClockSnapshot {
        self.generation = self.generation.wrapping_add(1);
        self.points.clear();
        self.points.push_back(raw);
        self.previous = Some(raw);
        self.status = status;
        self.snapshot()
    }

    fn window_delta(&self) -> (u64, u64) {
        match (self.points.front(), self.points.back()) {
            (Some(first), Some(last)) => (
                last.device_position - first.device_position,
                last.qpc_100ns.unwrap() - first.qpc_100ns.unwrap(),
            ),
            _ => (0, 0),
        }
    }
}

fn plausible(frames: u64, span: u64, rate: u32, tolerance_ppm: u64) -> bool {
    let actual = u128::from(frames) * u128::from(QPC_SECOND);
    let expected = u128::from(rate) * u128::from(span);
    expected != 0 && actual.abs_diff(expected) * 1_000_000 <= expected * u128::from(tolerance_ppm)
}

fn estimate(frames: u64, span: u64, rate: u32) -> (f64, f64) {
    let actual = u128::from(frames) * u128::from(QPC_SECOND);
    let expected = u128::from(rate) * u128::from(span);
    let sign = if actual >= expected { 1. } else { -1. };
    let ppm = sign * actual.abs_diff(expected) as f64 * 1_000_000. / expected as f64;
    (frames as f64 * QPC_SECOND as f64 / span as f64, ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(index: u64, ppm: i32, position_base: u64, qpc_base: u64) -> RawAudioTime {
        let frames = index * 480;
        let qpc_delta = u128::from(frames) * u128::from(QPC_SECOND) * 1_000_000
            / (48_000 * (1_000_000_i64 + i64::from(ppm)) as u128);
        RawAudioTime {
            endpoint_epoch: 0,
            device_position: position_base + frames,
            qpc_100ns: Some(qpc_base + qpc_delta as u64),
            device_rate: 48_000,
            input_frames: 480,
            discontinuity: false,
        }
    }

    fn steady(ppm: i32, position_base: u64, qpc_base: u64) -> CaptureClockObserver {
        let mut observer = CaptureClockObserver::new();
        for index in 0..=200 {
            observer.observe(packet(index, ppm, position_base, qpc_base));
        }
        observer
    }

    #[test]
    fn capture_clock_observes_positive_and_negative_100ppm_only_after_one_second() {
        for ppm in [-100, 100] {
            let mut observer = CaptureClockObserver::new();
            for index in 0..=90 {
                let snapshot = observer.observe(packet(index, ppm, 0, 10_000_000));
                assert!(snapshot.sample_rate.is_none());
                assert!(snapshot.ppm.is_none());
            }
            for index in 91..=200 {
                observer.observe(packet(index, ppm, 0, 10_000_000));
            }
            let snapshot = observer.snapshot();
            assert_eq!(snapshot.status, CaptureClockStatus::Ready);
            assert!(snapshot.span_100ns >= MIN_SPAN_100NS);
            assert!((snapshot.ppm.unwrap() - f64::from(ppm)).abs() < 0.2);
            assert!(
                (snapshot.sample_rate.unwrap() - 48_000. * (1. + f64::from(ppm) / 1_000_000.))
                    .abs()
                    < 0.01
            );
            assert_eq!(snapshot.latest, Some(packet(200, ppm, 0, 10_000_000)));
        }
    }

    #[test]
    fn capture_clock_large_absolute_counters_do_not_lose_small_drift() {
        for ppm in [-100, 100] {
            let observer = steady(ppm, u64::MAX - 10_000_000, u64::MAX - 1_000_000_000);
            let snapshot = observer.snapshot();
            assert_eq!(snapshot.status, CaptureClockStatus::Ready);
            assert!((snapshot.ppm.unwrap() - f64::from(ppm)).abs() < 0.2);
        }
    }

    #[test]
    fn capture_clock_missing_timestamp_discontinuity_and_holes_break_the_window() {
        let mut observer = steady(0, 0, 10_000_000);
        let missing = RawAudioTime {
            qpc_100ns: None,
            ..packet(201, 0, 0, 10_000_000)
        };
        let snapshot = observer.observe(missing);
        assert_eq!(snapshot.status, CaptureClockStatus::MissingQpc);
        assert_eq!(snapshot.latest, Some(missing));
        assert_eq!(snapshot.point_count, 0);
        let restored = observer.observe(packet(202, 0, 0, 10_000_000));
        assert_eq!(
            restored.status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::TimestampRestored)
        );
        let broken = RawAudioTime {
            discontinuity: true,
            ..packet(203, 0, 0, 10_000_000)
        };
        assert_eq!(
            observer.observe(broken).status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::Discontinuity)
        );
        let hole = observer.observe(packet(205, 0, 0, 10_000_000));
        assert_eq!(
            hole.status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::PacketGap)
        );
        let pause = observer.observe(packet(406, 0, 0, 10_000_000));
        assert_eq!(
            pause.status,
            CaptureClockStatus::Untrusted(CaptureClockIssue::ObservationGap)
        );
        assert_eq!(pause.point_count, 1);
        assert!(pause.sample_rate.is_none() && pause.ppm.is_none());
    }

    #[test]
    fn capture_clock_endpoint_and_rate_changes_start_new_windows() {
        let mut observer = steady(0, 0, 10_000_000);
        let endpoint = RawAudioTime {
            endpoint_epoch: 1,
            ..packet(0, 0, 0, 40_000_000)
        };
        let snapshot = observer.observe(endpoint);
        assert_eq!(
            snapshot.status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::EndpointChanged)
        );
        assert_eq!(snapshot.window_start, Some(endpoint));
        let rate = RawAudioTime {
            device_position: 441,
            qpc_100ns: Some(40_100_000),
            device_rate: 44_100,
            input_frames: 441,
            ..endpoint
        };
        let snapshot = observer.observe(rate);
        assert_eq!(
            snapshot.status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::RateChanged)
        );
        assert_eq!(snapshot.window_start, Some(rate));
        assert_eq!(snapshot.point_count, 1);
        assert!(snapshot.ppm.is_none());
    }

    #[test]
    fn capture_clock_repeated_regressed_and_implausible_observations_never_publish_ppm() {
        let first = packet(1, 0, 0, 10_000_000);
        let next = packet(2, 0, 0, 10_000_000);
        let cases = [
            (
                RawAudioTime {
                    qpc_100ns: first.qpc_100ns,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::QpcRepeated),
            ),
            (
                RawAudioTime {
                    qpc_100ns: Some(first.qpc_100ns.unwrap() - 1),
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::QpcRegressed),
            ),
            (
                RawAudioTime {
                    device_position: first.device_position,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::DeviceRepeated),
            ),
            (
                RawAudioTime {
                    device_position: first.device_position - 1,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::DeviceRegressed),
            ),
            (
                RawAudioTime {
                    device_position: u64::MAX,
                    ..next
                },
                CaptureClockStatus::Untrusted(CaptureClockIssue::ImplausibleDelta),
            ),
            (
                RawAudioTime {
                    input_frames: 0,
                    ..next
                },
                CaptureClockStatus::Untrusted(CaptureClockIssue::InvalidInputFrames),
            ),
            (
                RawAudioTime {
                    device_rate: 0,
                    ..next
                },
                CaptureClockStatus::Untrusted(CaptureClockIssue::InvalidRate),
            ),
        ];
        for (raw, status) in cases {
            let mut observer = CaptureClockObserver::new();
            observer.observe(first);
            let snapshot = observer.observe(raw);
            assert_eq!(snapshot.status, status);
            assert!(snapshot.ppm.is_none() && snapshot.sample_rate.is_none());
        }
    }

    #[test]
    fn capture_clock_window_and_point_storage_remain_bounded() {
        let mut observer = CaptureClockObserver::new();
        for index in 0..=2_000 {
            let snapshot = observer.observe(packet(index, 0, 0, 10_000_000));
            assert!(snapshot.point_count <= MAX_POINTS);
            assert!(snapshot.span_100ns <= MAX_SPAN_100NS);
        }
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.status, CaptureClockStatus::Ready);
        assert_eq!(snapshot.ppm, Some(0.));
        assert!(snapshot.point_count < 60);
        assert!(snapshot.window_start.unwrap().device_position >= 48_000 * 15);
        assert_eq!(snapshot.latest, Some(packet(2_000, 0, 0, 10_000_000)));
    }

    #[test]
    fn capture_clock_timestamp_acceptance_waits_for_two_continuous_packets_not_stored_points() {
        let mut observer = CaptureClockObserver::new();
        let first = packet(0, 0, 0, 10_000_000);
        let next = packet(1, 0, 0, 10_000_000);
        assert!(!observer.snapshot().accepts_raw_timestamp(first));
        assert!(!observer.observe(first).accepts_raw_timestamp(first));
        let snapshot = observer.observe(next);
        assert_eq!(snapshot.status, CaptureClockStatus::InsufficientSpan);
        assert_eq!(snapshot.point_count, 1);
        assert_eq!(snapshot.window_end, Some(first));
        assert_eq!(snapshot.span_100ns, 0);
        assert!(snapshot.accepts_raw_timestamp(first));
        assert!(snapshot.accepts_raw_timestamp(next));
        assert!(snapshot.sample_rate.is_none() && snapshot.ppm.is_none());
    }

    #[test]
    fn capture_clock_timestamp_acceptance_excludes_pcm_from_before_reanchor() {
        let mut observer = steady(0, 0, 10_000_000);
        let old = packet(200, 0, 0, 10_000_000);
        assert!(observer.snapshot().accepts_raw_timestamp(old));
        let first = RawAudioTime {
            discontinuity: true,
            ..packet(500, 0, 0, 10_000_000)
        };
        let snapshot = observer.observe(first);
        assert!(matches!(snapshot.status, CaptureClockStatus::Reanchored(_)));
        assert!(!snapshot.accepts_raw_timestamp(old));
        assert!(!snapshot.accepts_raw_timestamp(first));
        let next = packet(501, 0, 0, 10_000_000);
        let snapshot = observer.observe(next);
        assert_eq!(snapshot.status, CaptureClockStatus::InsufficientSpan);
        assert!(!snapshot.accepts_raw_timestamp(old));
        assert!(snapshot.accepts_raw_timestamp(first));
        assert!(snapshot.accepts_raw_timestamp(next));
    }

    #[test]
    fn capture_clock_timestamp_acceptance_rejects_missing_and_untrusted_observations() {
        let mut observer = steady(0, 0, 10_000_000);
        let old = packet(200, 0, 0, 10_000_000);
        let missing = RawAudioTime {
            qpc_100ns: None,
            ..packet(201, 0, 0, 10_000_000)
        };
        let snapshot = observer.observe(missing);
        assert_eq!(snapshot.status, CaptureClockStatus::MissingQpc);
        assert!(!snapshot.accepts_raw_timestamp(old));
        assert!(!snapshot.accepts_raw_timestamp(missing));
        let restored = packet(202, 0, 0, 10_000_000);
        assert!(!observer.observe(restored).accepts_raw_timestamp(restored));
        let untrusted = packet(403, 0, 0, 10_000_000);
        let snapshot = observer.observe(untrusted);
        assert!(matches!(snapshot.status, CaptureClockStatus::Untrusted(_)));
        assert!(!snapshot.accepts_raw_timestamp(old));
        assert!(!snapshot.accepts_raw_timestamp(untrusted));
        let next = packet(404, 0, 0, 10_000_000);
        let snapshot = observer.observe(next);
        assert!(snapshot.accepts_raw_timestamp(next));
        assert!(!snapshot.accepts_raw_timestamp(old));
    }

    #[test]
    fn capture_clock_timestamp_acceptance_checks_identity_and_both_start_ranges() {
        let snapshot = steady(0, 0, 10_000_000).snapshot();
        let first = snapshot.window_start.unwrap();
        let last = snapshot.latest.unwrap();
        assert_eq!(snapshot.status, CaptureClockStatus::Ready);
        assert!(snapshot.accepts_raw_timestamp(first));
        assert!(snapshot.accepts_raw_timestamp(last));
        for raw in [
            RawAudioTime {
                endpoint_epoch: last.endpoint_epoch + 1,
                ..last
            },
            RawAudioTime {
                device_rate: 44_100,
                ..last
            },
            RawAudioTime {
                qpc_100ns: None,
                ..last
            },
            RawAudioTime {
                device_position: first.device_position,
                qpc_100ns: Some(first.qpc_100ns.unwrap() - 1),
                ..last
            },
            RawAudioTime {
                device_position: last.device_position + 1,
                ..last
            },
            RawAudioTime {
                qpc_100ns: Some(last.qpc_100ns.unwrap() + 1),
                ..last
            },
        ] {
            assert!(!snapshot.accepts_raw_timestamp(raw));
        }
        let snapshot = steady(0, 10_000, 10_000_000).snapshot();
        let first = snapshot.window_start.unwrap();
        assert!(!snapshot.accepts_raw_timestamp(RawAudioTime {
            device_position: first.device_position - 1,
            ..first
        }));
    }

    #[test]
    fn capture_clock_timestamp_acceptance_does_not_resurrect_an_expired_window() {
        let mut observer = CaptureClockObserver::new();
        let first = packet(0, 0, 0, 10_000_000);
        for index in 0..=700 {
            observer.observe(packet(index, 0, 0, 10_000_000));
        }
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.status, CaptureClockStatus::Ready);
        assert!(!snapshot.accepts_raw_timestamp(first));
        assert!(snapshot.accepts_raw_timestamp(snapshot.window_start.unwrap()));
        assert!(snapshot.accepts_raw_timestamp(snapshot.latest.unwrap()));
    }

    #[test]
    fn capture_clock_generation_survives_status_changes_and_rolling_window() {
        let mut observer = CaptureClockObserver::new();
        assert_eq!(observer.snapshot().generation, 0);
        let first = packet(0, 0, 0, 10_000_000);
        let started = observer.observe(first);
        assert_eq!(started.generation, 1);
        assert_eq!(started, observer.snapshot());
        for index in 1..=1_000 {
            let snapshot = observer.observe(packet(index, 0, 0, 10_000_000));
            assert_eq!(snapshot.generation, started.generation);
            assert_eq!(snapshot, observer.snapshot());
            if index == 1 {
                assert_eq!(snapshot.status, CaptureClockStatus::InsufficientSpan);
                assert_eq!(snapshot.point_count, 1);
            }
        }
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.status, CaptureClockStatus::Ready);
        assert!(snapshot.window_start.unwrap().device_position > first.device_position);
        assert!(!snapshot.accepts_raw_timestamp(first));
    }

    #[test]
    fn capture_clock_generation_changes_for_each_continuity_break() {
        let next = packet(201, 0, 0, 10_000_000);
        for (raw, status) in [
            (
                RawAudioTime {
                    discontinuity: true,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::Discontinuity),
            ),
            (
                RawAudioTime {
                    endpoint_epoch: 1,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::EndpointChanged),
            ),
            (
                RawAudioTime {
                    device_rate: 44_100,
                    input_frames: 441,
                    ..next
                },
                CaptureClockStatus::Reanchored(CaptureClockReanchor::RateChanged),
            ),
            (
                RawAudioTime {
                    qpc_100ns: None,
                    ..next
                },
                CaptureClockStatus::MissingQpc,
            ),
            (
                RawAudioTime {
                    device_rate: 0,
                    ..next
                },
                CaptureClockStatus::Untrusted(CaptureClockIssue::InvalidRate),
            ),
            (
                packet(401, 0, 0, 10_000_000),
                CaptureClockStatus::Untrusted(CaptureClockIssue::ObservationGap),
            ),
        ] {
            let mut observer = steady(0, 0, 10_000_000);
            let before = observer.snapshot();
            let snapshot = observer.observe(raw);
            assert_eq!(snapshot.generation, before.generation.wrapping_add(1));
            assert_eq!(snapshot.status, status);
            assert_eq!(snapshot, observer.snapshot());
            assert!(!snapshot.accepts_raw_timestamp(before.latest.unwrap()));
        }
    }

    #[test]
    fn capture_clock_generation_wraps_and_changes_again_when_timestamp_is_restored() {
        let mut observer = CaptureClockObserver::new();
        observer.observe(packet(0, 0, 0, 10_000_000));
        observer.generation = u64::MAX;
        let missing = observer.observe(RawAudioTime {
            qpc_100ns: None,
            ..packet(1, 0, 0, 10_000_000)
        });
        assert_eq!(missing.generation, 0);
        assert_eq!(missing, observer.snapshot());
        let restored = observer.observe(packet(2, 0, 0, 10_000_000));
        assert_eq!(restored.generation, 1);
        assert_eq!(
            restored.status,
            CaptureClockStatus::Reanchored(CaptureClockReanchor::TimestampRestored)
        );
        assert_eq!(restored, observer.snapshot());
        let continuous = observer.observe(packet(3, 0, 0, 10_000_000));
        assert_eq!(continuous.generation, restored.generation);
        assert!(continuous.accepts_raw_timestamp(continuous.latest.unwrap()));

        let invalid = observer.observe(RawAudioTime {
            device_rate: 0,
            ..packet(4, 0, 0, 10_000_000)
        });
        assert_eq!(invalid.generation, 2);
        let restored = observer.observe(packet(5, 0, 0, 10_000_000));
        assert_eq!(restored.generation, 3);
        assert_eq!(restored, observer.snapshot());
        assert!(matches!(restored.status, CaptureClockStatus::Reanchored(_)));
        assert_eq!(
            observer.observe(packet(6, 0, 0, 10_000_000)).generation,
            restored.generation
        );
    }
}
