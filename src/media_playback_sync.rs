//! 声画同步的有界观测：只比较同一时刻的帧 PTS 与现有音频时钟快照。
//! 解码消费、场景提交和实际呈现分别统计，不能用前两者替代屏幕呈现证据。
use crate::preview_audio_clock::{ClockPhase, ClockSnapshot};
use crate::preview_audio_device_clock::PublishedClock;
use serde_json::{json, Value};
use std::{collections::VecDeque, sync::OnceLock, time::Duration};

#[path = "media_worker_exit.rs"]
mod worker_exit;
pub(crate) use worker_exit::WorkerExit;

const MAX_SAMPLES: usize = 2048;
const STAGES: usize = 3;
const SCENARIOS: usize = 6;
const REJECTIONS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameStage {
    Taken,
    Submitted,
    Presented,
}
impl FrameStage {
    fn index(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Self::Taken => "taken",
            Self::Submitted => "submitted",
            Self::Presented => "presented",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncScenario {
    Start,
    Steady,
    Seek,
    Resume,
    RateChange,
    Recovery,
}
impl Default for SyncScenario {
    fn default() -> Self {
        Self::Start
    }
}
impl SyncScenario {
    fn index(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Steady => "steady",
            Self::Seek => "seek",
            Self::Resume => "resume",
            Self::RateChange => "rate_change",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameObservation {
    /// Player 的控制代次，与音频内部 Reset 使用的 clock epoch 分开。
    pub transport_generation: u64,
    pub pts: Duration,
    pub qpc_100ns: u64,
    pub stage: FrameStage,
    pub scenario: SyncScenario,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncRejection {
    StaleTransport,
    StaleClock,
    UnalignedClock,
    ClockNotAdvancing,
    TimeRegressed,
    DuplicateTime,
}
impl SyncRejection {
    fn index(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Self::StaleTransport => "stale_transport",
            Self::StaleClock => "stale_clock",
            Self::UnalignedClock => "unaligned_clock",
            Self::ClockNotAdvancing => "clock_not_advancing",
            Self::TimeRegressed => "time_regressed",
            Self::DuplicateTime => "duplicate_time",
        }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    stage: FrameStage,
    scenario: SyncScenario,
    skew_ms: f64,
}

pub(crate) struct SyncObserver {
    transport_generation: u64,
    clock_epoch: u64,
    last_qpc: [Option<u64>; STAGES],
    samples: VecDeque<Sample>,
    accepted: [[u64; SCENARIOS]; STAGES],
    rejected: [u64; REJECTIONS],
}

/// 仅保留最近已取出的帧标识，供场景提交回执核对；不保存像素。
pub(crate) struct PlaybackObserver {
    observer: SyncObserver,
    frames: VecDeque<(u64, FrameObservation)>,
    without_clock: u64,
    has_timeline: bool,
}
impl Default for PlaybackObserver {
    fn default() -> Self {
        Self {
            observer: SyncObserver::new(0, 0),
            frames: VecDeque::new(),
            without_clock: 0,
            has_timeline: false,
        }
    }
}
impl PlaybackObserver {
    pub(crate) fn taken(
        &mut self,
        serial: u64,
        frame: FrameObservation,
        current_transport: u64,
        clock: Option<PublishedClock>,
    ) {
        if self.frames.len() == 16 {
            self.frames.pop_front();
        }
        self.frames.push_back((serial, frame));
        self.record(frame, current_transport, clock);
    }
    pub(crate) fn submitted(
        &mut self,
        serial: u64,
        qpc_100ns: u64,
        current_transport: u64,
        clock: Option<PublishedClock>,
    ) {
        if let Some((_, frame)) = self.frames.iter().find(|(id, _)| *id == serial) {
            let frame = FrameObservation {
                qpc_100ns,
                stage: FrameStage::Submitted,
                ..*frame
            };
            self.record(frame, current_transport, clock);
        }
    }
    fn record(
        &mut self,
        mut frame: FrameObservation,
        current_transport: u64,
        clock: Option<PublishedClock>,
    ) {
        let Some(clock) = clock.filter(|clock| clock.transport_generation == current_transport)
        else {
            self.without_clock = self.without_clock.saturating_add(1);
            return;
        };
        if self.has_timeline
            && current_transport == self.observer.transport_generation
            && clock.clock.generation != self.observer.clock_epoch
        {
            frame.scenario = SyncScenario::Recovery;
        }
        // 最新 Player/输出状态有权推进时间线；帧回执自身没有换代权限。
        if self
            .observer
            .begin_timeline(current_transport, clock.clock.generation)
            .is_ok()
        {
            self.has_timeline = true;
            let _ = self.observer.record_published(frame, clock);
        }
    }
    pub(crate) fn report(&self) -> Value {
        let mut report = self.observer.report();
        report["observations_without_aligned_audio_clock"] = self.without_clock.into();
        report
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}

/// 使用与 IAudioClock 相同的 QPC 域，转换成 100ns 后再交给现有时钟模型。
pub(crate) fn qpc_now_100ns() -> Option<u64> {
    static FREQUENCY: OnceLock<Option<u64>> = OnceLock::new();
    let frequency = (*FREQUENCY.get_or_init(|| {
        let mut value = 0;
        (unsafe { QueryPerformanceFrequency(&mut value) } != 0 && value > 0).then_some(value as u64)
    }))?;
    let mut value = 0;
    if unsafe { QueryPerformanceCounter(&mut value) } == 0 || value < 0 {
        return None;
    }
    u64::try_from(value as u128 * 10_000_000 / u128::from(frequency)).ok()
}

fn newer(next: u64, previous: u64) -> bool {
    let delta = next.wrapping_sub(previous);
    delta != 0 && delta < 1_u64 << 63
}

impl SyncObserver {
    pub(crate) fn new(transport_generation: u64, clock_epoch: u64) -> Self {
        Self {
            transport_generation,
            clock_epoch,
            last_qpc: [None; STAGES],
            samples: VecDeque::new(),
            accepted: [[0; SCENARIOS]; STAGES],
            rejected: [0; REJECTIONS],
        }
    }

    /// 由持有最新控制/设备状态的调用者推进时间线，旧后台回执不能自行换代。
    /// 欠载恢复只推进 clock epoch；新的播放控制可以同时换 transport 和 clock epoch。
    pub(crate) fn begin_timeline(
        &mut self,
        transport_generation: u64,
        clock_epoch: u64,
    ) -> Result<(), SyncRejection> {
        if transport_generation == self.transport_generation {
            if clock_epoch != self.clock_epoch && !newer(clock_epoch, self.clock_epoch) {
                return Err(SyncRejection::StaleClock);
            }
        } else if !newer(transport_generation, self.transport_generation) {
            return Err(SyncRejection::StaleTransport);
        }
        self.transport_generation = transport_generation;
        self.clock_epoch = clock_epoch;
        Ok(())
    }

    /// 音频与画面必须属于同一 Player 控制代次；设备内部 epoch 单独核对。
    pub(crate) fn record_published(
        &mut self,
        frame: FrameObservation,
        published: PublishedClock,
    ) -> Result<f64, SyncRejection> {
        if published.transport_generation != self.transport_generation {
            let count = &mut self.rejected[SyncRejection::StaleTransport.index()];
            *count = count.saturating_add(1);
            return Err(SyncRejection::StaleTransport);
        }
        self.record(frame, published.clock)
    }

    /// clock 应由 AudioClockModel::snapshot_at(frame.qpc_100ns) 取得。
    /// 此处不再实现第二套时钟外推，也不把负偏差自动当成解码或设备故障。
    fn record(
        &mut self,
        frame: FrameObservation,
        clock: ClockSnapshot,
    ) -> Result<f64, SyncRejection> {
        let result = self.validate(frame, clock);
        if let Err(reason) = result {
            self.rejected[reason.index()] = self.rejected[reason.index()].saturating_add(1);
            return Err(reason);
        }
        // Duration 的最大纳秒数可放入 i128；先作有符号差再转浮点，避免巨大绝对时间抵消。
        let skew_ms = (frame.pts.as_nanos() as i128 - clock.media_position.as_nanos() as i128)
            as f64
            / 1_000_000.;
        self.last_qpc[frame.stage.index()] = Some(frame.qpc_100ns);
        let count = &mut self.accepted[frame.stage.index()][frame.scenario.index()];
        *count = count.saturating_add(1);
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            stage: frame.stage,
            scenario: frame.scenario,
            skew_ms,
        });
        Ok(skew_ms)
    }

    fn validate(&self, frame: FrameObservation, clock: ClockSnapshot) -> Result<(), SyncRejection> {
        if frame.transport_generation != self.transport_generation {
            return Err(SyncRejection::StaleTransport);
        }
        if clock.generation != self.clock_epoch {
            return Err(SyncRejection::StaleClock);
        }
        if clock.qpc_100ns != frame.qpc_100ns {
            return Err(SyncRejection::UnalignedClock);
        }
        if !clock.advancing || !matches!(clock.phase, ClockPhase::Playing | ClockPhase::Draining) {
            return Err(SyncRejection::ClockNotAdvancing);
        }
        if let Some(previous) = self.last_qpc[frame.stage.index()] {
            if frame.qpc_100ns < previous {
                return Err(SyncRejection::TimeRegressed);
            }
            if frame.qpc_100ns == previous {
                return Err(SyncRejection::DuplicateTime);
            }
        }
        Ok(())
    }

    /// 固定长度最近窗口；不同证据级别和交互场景不会混成一个“实播通过”数字。
    pub(crate) fn report(&self) -> Value {
        let mut groups = Vec::new();
        for stage in [
            FrameStage::Taken,
            FrameStage::Submitted,
            FrameStage::Presented,
        ] {
            for scenario in [
                SyncScenario::Start,
                SyncScenario::Steady,
                SyncScenario::Seek,
                SyncScenario::Resume,
                SyncScenario::RateChange,
                SyncScenario::Recovery,
            ] {
                let lifetime = self.accepted[stage.index()][scenario.index()];
                if lifetime == 0 {
                    continue;
                }
                let signed: Vec<_> = self
                    .samples
                    .iter()
                    .filter(|v| v.stage == stage && v.scenario == scenario)
                    .map(|v| v.skew_ms)
                    .collect();
                let mut row = json!({"stage": stage.label(), "scenario": scenario.label(), "accepted_total": lifetime, "window_samples": signed.len()});
                if !signed.is_empty() {
                    let mut absolute: Vec<_> = signed.iter().map(|value| value.abs()).collect();
                    let mut ordered = signed.clone();
                    ordered.sort_by(f64::total_cmp);
                    absolute.sort_by(f64::total_cmp);
                    row["mean_video_minus_audio_ms"] =
                        (signed.iter().sum::<f64>() / signed.len() as f64).into();
                    row["median_video_minus_audio_ms"] = percentile(&ordered, 50).into();
                    row["p95_abs_ms"] = percentile(&absolute, 95).into();
                    row["max_abs_ms"] = absolute.last().copied().unwrap().into();
                }
                groups.push(row);
            }
        }
        let rejected: Vec<_> = [
            SyncRejection::StaleTransport,
            SyncRejection::StaleClock,
            SyncRejection::UnalignedClock,
            SyncRejection::ClockNotAdvancing,
            SyncRejection::TimeRegressed,
            SyncRejection::DuplicateTime,
        ]
        .into_iter()
        .filter_map(|reason| {
            let count = self.rejected[reason.index()];
            (count > 0).then(|| json!({"reason": reason.label(), "count": count}))
        })
        .collect();
        json!({"window_capacity": MAX_SAMPLES, "window_samples": self.samples.len(), "groups": groups, "rejected": rejected})
    }
}

/// 最近秩百分位；统计口径保持固定，空组不会调用。
fn percentile(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview_audio_clock::{AudioClockModel, ClockAnchor, ClockReading};

    fn clock(qpc: u64) -> ClockSnapshot {
        ClockSnapshot {
            generation: 4,
            media_position: Duration::from_secs(2),
            qpc_100ns: qpc,
            phase: ClockPhase::Playing,
            advancing: true,
            queued_media: Duration::from_millis(50),
        }
    }
    fn frame(qpc: u64, offset_ms: i64) -> FrameObservation {
        FrameObservation {
            transport_generation: 9,
            pts: Duration::from_millis((2000 + offset_ms) as u64),
            qpc_100ns: qpc,
            stage: FrameStage::Submitted,
            scenario: SyncScenario::Steady,
        }
    }

    #[test]
    fn media_playback_sync_signed_offsets_and_statistics() {
        let mut observer = SyncObserver::new(9, 4);
        for (index, offset) in [-20, -10, 0, 10, 20].into_iter().enumerate() {
            assert_eq!(
                observer
                    .record(frame(index as u64, offset), clock(index as u64))
                    .unwrap(),
                offset as f64
            );
        }
        let report = observer.report();
        let row = &report["groups"][0];
        assert_eq!(row["median_video_minus_audio_ms"], 0.);
        assert_eq!(row["mean_video_minus_audio_ms"], 0.);
        assert_eq!(row["p95_abs_ms"], 20.);
        assert_eq!(row["max_abs_ms"], 20.);
    }

    #[test]
    fn media_playback_sync_player_callbacks_keep_stage_and_transport_identity() {
        let mut observer = PlaybackObserver::default();
        let observed = FrameObservation {
            stage: FrameStage::Taken,
            scenario: SyncScenario::Start,
            ..frame(10, 0)
        };
        let published = |qpc| {
            Some(PublishedClock {
                transport_generation: 9,
                clock: clock(qpc),
            })
        };
        observer.taken(17, observed, 9, published(10));
        observer.submitted(17, 20, 9, published(20));
        let report = observer.report();
        let groups = report["groups"].as_array().unwrap();
        assert!(groups
            .iter()
            .any(|group| group["stage"] == "taken" && group["scenario"] == "start"));
        assert!(groups.iter().any(|group| group["stage"] == "submitted"));
        assert!(!groups.iter().any(|group| group["stage"] == "presented"));
        observer.submitted(
            17,
            30,
            10,
            Some(PublishedClock {
                transport_generation: 10,
                clock: clock(30),
            }),
        );
        assert!(observer.report()["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rejection| rejection["reason"] == "stale_transport" && rejection["count"] == 1));
        observer.submitted(17, 40, 10, None);
        assert_eq!(
            observer.report()["observations_without_aligned_audio_clock"],
            1
        );
        assert_eq!(observer.report()["window_samples"], 2);
    }

    #[test]
    fn media_playback_sync_rejects_stale_unaligned_and_frozen_observations() {
        let mut observer = SyncObserver::new(9, 4);
        assert_eq!(
            observer.record(
                FrameObservation {
                    transport_generation: 8,
                    ..frame(1, 0)
                },
                clock(1)
            ),
            Err(SyncRejection::StaleTransport)
        );
        assert_eq!(
            observer.record(
                frame(1, 0),
                ClockSnapshot {
                    generation: 3,
                    ..clock(1)
                }
            ),
            Err(SyncRejection::StaleClock)
        );
        assert_eq!(
            observer.record(frame(1, 0), clock(2)),
            Err(SyncRejection::UnalignedClock)
        );
        for phase in [
            ClockPhase::Priming,
            ClockPhase::Paused,
            ClockPhase::Underrun,
            ClockPhase::Ended,
        ] {
            assert_eq!(
                observer.record(frame(1, 0), ClockSnapshot { phase, ..clock(1) }),
                Err(SyncRejection::ClockNotAdvancing)
            );
        }
        assert_eq!(
            observer.record(
                frame(1, 0),
                ClockSnapshot {
                    advancing: false,
                    ..clock(1)
                }
            ),
            Err(SyncRejection::ClockNotAdvancing)
        );
        observer.record(frame(10, 0), clock(10)).unwrap();
        assert_eq!(
            observer.record(frame(10, 0), clock(10)),
            Err(SyncRejection::DuplicateTime)
        );
        assert_eq!(
            observer.record(frame(9, 0), clock(9)),
            Err(SyncRejection::TimeRegressed)
        );
        assert_eq!(observer.report()["window_samples"], 1);
    }

    #[test]
    fn media_playback_sync_keeps_presentation_evidence_and_scenarios_separate() {
        let mut observer = SyncObserver::new(9, 4);
        observer.record(frame(1, 10), clock(1)).unwrap();
        observer
            .record(
                FrameObservation {
                    stage: FrameStage::Presented,
                    ..frame(1, 40)
                },
                clock(1),
            )
            .unwrap();
        observer
            .record(
                FrameObservation {
                    stage: FrameStage::Taken,
                    scenario: SyncScenario::Seek,
                    ..frame(1, -20)
                },
                clock(1),
            )
            .unwrap();
        let report = observer.report();
        assert_eq!(report["groups"].as_array().unwrap().len(), 3);
        assert_eq!(report["groups"][2]["stage"], "presented");
        assert_eq!(report["groups"][2]["p95_abs_ms"], 40.);
    }

    #[test]
    fn media_playback_sync_epoch_changes_do_not_impersonate_transport_changes() {
        let mut observer = SyncObserver::new(9, 4);
        observer.record(frame(1, 0), clock(1)).unwrap();
        observer.begin_timeline(9, 5).unwrap();
        assert_eq!(
            observer.record(frame(2, 0), clock(2)),
            Err(SyncRejection::StaleClock)
        );
        observer
            .record(
                frame(2, 0),
                ClockSnapshot {
                    generation: 5,
                    ..clock(2)
                },
            )
            .unwrap();
        assert_eq!(
            observer.begin_timeline(9, 4),
            Err(SyncRejection::StaleClock)
        );
        assert_eq!(
            observer.begin_timeline(8, 6),
            Err(SyncRejection::StaleTransport)
        );
        observer.begin_timeline(10, 0).unwrap();
        assert_eq!(
            observer.record(
                frame(3, 0),
                ClockSnapshot {
                    generation: 0,
                    ..clock(3)
                }
            ),
            Err(SyncRejection::StaleTransport)
        );
        observer
            .record(
                FrameObservation {
                    transport_generation: 10,
                    ..frame(3, 0)
                },
                ClockSnapshot {
                    generation: 0,
                    ..clock(3)
                },
            )
            .unwrap();
    }

    #[test]
    fn media_playback_sync_checks_audio_transport_even_when_clock_epoch_matches() {
        let mut observer = SyncObserver::new(9, 4);
        assert_eq!(
            observer.record_published(
                frame(1, 0),
                PublishedClock {
                    transport_generation: 8,
                    clock: clock(1),
                }
            ),
            Err(SyncRejection::StaleTransport)
        );
        observer
            .record_published(
                frame(1, 0),
                PublishedClock {
                    transport_generation: 9,
                    clock: clock(1),
                },
            )
            .unwrap();
        assert_eq!(observer.report()["window_samples"], 1);
    }

    #[test]
    fn media_playback_sync_window_is_bounded_and_lifetime_count_is_retained() {
        let mut observer = SyncObserver::new(9, 4);
        for qpc in 0..MAX_SAMPLES + 7 {
            observer
                .record(frame(qpc as u64, 1), clock(qpc as u64))
                .unwrap();
        }
        let report = observer.report();
        assert_eq!(report["window_samples"], MAX_SAMPLES);
        assert_eq!(report["groups"][0]["accepted_total"], MAX_SAMPLES + 7);
    }

    #[test]
    fn media_playback_sync_reuses_clock_projection_at_frame_qpc() {
        let mut model = AudioClockModel::new(ClockAnchor {
            generation: 4,
            media_origin: Duration::from_secs(2),
            device_origin: 100,
            qpc_100ns: 0,
            frequency: 1000,
            rate: 1.,
            queued_until: Duration::from_secs(3),
        })
        .unwrap();
        model
            .observe(ClockReading {
                generation: 4,
                device_position: 200,
                qpc_100ns: 1_000_000,
                queued_until: Duration::from_secs(3),
                phase: ClockPhase::Playing,
            })
            .unwrap();
        let mut observer = SyncObserver::new(9, 4);
        let qpc = 1_500_000;
        let observation = FrameObservation {
            pts: Duration::from_millis(2140),
            ..frame(qpc, 0)
        };
        assert_eq!(
            observer
                .record(observation, model.snapshot_at(4, qpc).unwrap())
                .unwrap(),
            -10.
        );
    }
}
