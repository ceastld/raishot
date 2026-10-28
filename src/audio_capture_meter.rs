//! 真实采集输入的来源状态与包电平；混音增益、重采样和补零在此观测之后处理。
use super::AudioSource;
use crate::audio_devices::{EndpointFallbackReason, EndpointInfo, EndpointSelection};
use std::time::{Duration, Instant};

pub(crate) const CAPTURE_LEVEL_FRESHNESS: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureSourceState {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureLevel {
    /// 左右声道绝对峰值，保留超过数字满幅 1.0 的原始有限幅度。
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    pub age: Duration,
    pub input_frames: u64,
    /// 设备提供的静音包标记；零电平本身不表示硬件静音。
    pub silent_packet: bool,
    /// 任一路有限输入采样达到或超过数字满幅（绝对值 >= 1）。
    pub clipped: bool,
}

impl CaptureLevel {
    pub(crate) fn is_fresh(&self) -> bool {
        self.age <= CAPTURE_LEVEL_FRESHNESS
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaptureSourceSnapshot {
    pub source: AudioSource,
    pub requested: EndpointSelection,
    pub endpoint: Option<EndpointInfo>,
    pub fallback: Option<EndpointFallbackReason>,
    pub state: CaptureSourceState,
    pub warning: Option<String>,
    pub error: Option<String>,
    /// 仅 Running 发布距最近一次实际启动的时间；终态保留计数和最后包电平。
    pub running_for: Option<Duration>,
    pub packets: u64,
    pub total_input_frames: u64,
    /// None 表示尚无真实非空输入包；真实零包会得到峰值/RMS 为零的 Some。
    pub level: Option<CaptureLevel>,
}

#[derive(Clone, Copy, Debug)]
struct ObservedLevel {
    peak: [f32; 2],
    rms: [f32; 2],
    observed_at: Instant,
    input_frames: u64,
    silent_packet: bool,
    clipped: bool,
}

pub(crate) struct SourceMonitor {
    requested: EndpointSelection,
    endpoint: Option<EndpointInfo>,
    fallback: Option<EndpointFallbackReason>,
    started_at: Option<Instant>,
    packets: u64,
    total_input_frames: u64,
    level: Option<ObservedLevel>,
}

impl SourceMonitor {
    pub(crate) fn new(requested: EndpointSelection) -> Self {
        Self {
            requested,
            endpoint: None,
            fallback: None,
            started_at: None,
            packets: 0,
            total_input_frames: 0,
            level: None,
        }
    }

    pub(crate) fn resolved(
        &mut self,
        endpoint: EndpointInfo,
        fallback: Option<EndpointFallbackReason>,
    ) {
        self.endpoint = Some(endpoint);
        self.fallback = fallback;
    }

    pub(crate) fn started(&mut self, now: Instant) {
        self.started_at = Some(now);
    }

    /// input 是当前真实输入包转换成的立体声，尚未应用 MixLevels、补零或重采样。
    pub(crate) fn observe(&mut self, input: &[[f32; 2]], silent_packet: bool, now: Instant) {
        if input.is_empty() {
            return;
        }
        let input_frames = input.len() as u64;
        let mut peak = [0_f32; 2];
        let mut energy = [0_f64; 2];
        for frame in input {
            for channel in 0..2 {
                let value = if frame[channel].is_finite() {
                    frame[channel]
                } else {
                    0.
                };
                peak[channel] = peak[channel].max(value.abs());
                let value = f64::from(value);
                energy[channel] += value * value;
            }
        }
        let rms = std::array::from_fn(|channel| {
            // 有限 f32 的平方及任意可分配包的累加均落在 f64 范围内；峰值上界
            // 还避免舍入把最大有限 f32 的 RMS 转回无穷大。
            (energy[channel] / input_frames as f64)
                .sqrt()
                .min(f64::from(peak[channel])) as f32
        });
        self.packets = self.packets.saturating_add(1);
        self.total_input_frames = self.total_input_frames.saturating_add(input_frames);
        self.level = Some(ObservedLevel {
            peak,
            rms,
            observed_at: now,
            input_frames,
            silent_packet,
            clipped: peak.iter().any(|value| *value >= 1.),
        });
    }

    pub(crate) fn snapshot(
        &self,
        source: AudioSource,
        exited: bool,
        error: Option<String>,
        warning: Option<String>,
        now: Instant,
    ) -> CaptureSourceSnapshot {
        let state = if error.is_some() {
            CaptureSourceState::Failed
        } else if exited {
            CaptureSourceState::Stopped
        } else if self.started_at.is_some() {
            CaptureSourceState::Running
        } else {
            CaptureSourceState::Starting
        };
        let running_for = if state == CaptureSourceState::Running {
            self.started_at
                .map(|started| now.saturating_duration_since(started))
        } else {
            None
        };
        CaptureSourceSnapshot {
            source,
            requested: self.requested.clone(),
            endpoint: self.endpoint.clone(),
            fallback: self.fallback,
            state,
            warning,
            error,
            running_for,
            packets: self.packets,
            total_input_frames: self.total_input_frames,
            level: self.level.map(|level| CaptureLevel {
                peak: level.peak,
                rms: level.rms,
                age: now.saturating_duration_since(level.observed_at),
                input_frames: level.input_frames,
                silent_packet: level.silent_packet,
                clipped: level.clipped,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_devices::EndpointKind;

    fn snapshot(monitor: &SourceMonitor, now: Instant) -> CaptureSourceSnapshot {
        monitor.snapshot(AudioSource::Microphone, false, None, None, now)
    }

    #[test]
    fn source_monitor_distinguishes_no_packet_real_zero_and_stale_level() {
        let now = Instant::now();
        let mut monitor = SourceMonitor::new(EndpointSelection::Default);
        let initial = snapshot(&monitor, now);
        assert_eq!(initial.state, CaptureSourceState::Starting);
        assert_eq!((initial.packets, initial.total_input_frames), (0, 0));
        assert!(initial.level.is_none());

        monitor.observe(&[[0., 0.]; 4], false, now);
        let zero = snapshot(&monitor, now);
        assert_eq!((zero.packets, zero.total_input_frames), (1, 4));
        let level = zero.level.unwrap();
        assert_eq!((level.peak, level.rms), ([0.; 2], [0.; 2]));
        assert_eq!(level.input_frames, 4);
        assert!(!level.silent_packet && !level.clipped);
        assert!(level.is_fresh());
        let boundary = snapshot(&monitor, now + CAPTURE_LEVEL_FRESHNESS);
        assert!(boundary.level.unwrap().is_fresh());
        let stale = snapshot(&monitor, now + Duration::from_millis(251));
        assert!(!stale.level.unwrap().is_fresh());

        monitor.observe(&[[0., 0.]; 2], true, now + Duration::from_millis(300));
        let silent = snapshot(&monitor, now + Duration::from_millis(300));
        assert_eq!((silent.packets, silent.total_input_frames), (2, 6));
        assert!(silent.level.unwrap().silent_packet);
        assert_eq!(silent.level.unwrap().peak, [0.; 2]);
    }

    #[test]
    fn source_monitor_measures_stereo_before_clamping_and_sanitizes_nonfinite_values() {
        let now = Instant::now();
        let mut monitor = SourceMonitor::new(EndpointSelection::Default);
        monitor.observe(
            &[[0.5, -0.5], [-1.5, 1.], [f32::NAN, f32::INFINITY]],
            false,
            now,
        );
        let level = snapshot(&monitor, now).level.unwrap();
        assert_eq!(level.peak, [1.5, 1.]);
        assert!((level.rms[0] - (2.5_f64 / 3.).sqrt() as f32).abs() < 1e-6);
        assert!((level.rms[1] - (1.25_f64 / 3.).sqrt() as f32).abs() < 1e-6);
        assert!(level.clipped);

        monitor.observe(&[[-f32::MAX, f32::MAX]], false, now);
        let level = snapshot(&monitor, now).level.unwrap();
        assert_eq!(level.peak, [f32::MAX; 2]);
        assert_eq!(level.rms, [f32::MAX; 2]);
        assert!(level.clipped);
    }

    #[test]
    fn source_monitor_empty_input_does_not_create_or_refresh_a_packet() {
        let now = Instant::now();
        let later = now + Duration::from_millis(100);
        let mut monitor = SourceMonitor::new(EndpointSelection::Default);
        monitor.observe(&[], true, now);
        assert!(snapshot(&monitor, now).level.is_none());
        monitor.observe(&[[0.25, -0.5]; 2], false, now);
        monitor.observe(&[], true, later);
        let result = snapshot(&monitor, later);
        assert_eq!((result.packets, result.total_input_frames), (1, 2));
        let level = result.level.unwrap();
        assert_eq!(level.age, Duration::from_millis(100));
        assert_eq!(level.input_frames, 2);
        assert_eq!(level.peak, [0.25, 0.5]);
        assert!(!level.silent_packet);
    }

    #[test]
    fn source_monitor_reports_start_run_stop_and_error_precedence() {
        let now = Instant::now();
        let later = now + Duration::from_secs(2);
        let mut monitor = SourceMonitor::new(EndpointSelection::Default);
        assert!(snapshot(&monitor, now).running_for.is_none());
        monitor.started(now);
        let running = monitor.snapshot(
            AudioSource::System,
            false,
            None,
            Some("warning".into()),
            later,
        );
        assert_eq!(running.state, CaptureSourceState::Running);
        assert_eq!(running.running_for, Some(Duration::from_secs(2)));
        assert_eq!(running.warning.as_deref(), Some("warning"));
        let stopped = monitor.snapshot(AudioSource::System, true, None, None, later);
        assert_eq!(stopped.state, CaptureSourceState::Stopped);
        assert!(stopped.running_for.is_none());
        for exited in [false, true] {
            let failed = monitor.snapshot(
                AudioSource::System,
                exited,
                Some("failed".into()),
                None,
                later,
            );
            assert_eq!(failed.state, CaptureSourceState::Failed);
            assert_eq!(failed.error.as_deref(), Some("failed"));
            assert!(failed.running_for.is_none());
        }
    }

    #[test]
    fn source_monitor_preserves_requested_identity_and_owns_resolved_metadata() {
        let now = Instant::now();
        let requested = EndpointSelection::Fixed {
            id: "requested-id".into(),
        };
        let endpoint = EndpointInfo {
            kind: EndpointKind::Capture,
            id: "actual-default-id".into(),
            name: "Microphone".into(),
            is_default: true,
        };
        let mut monitor = SourceMonitor::new(requested.clone());
        monitor.resolved(endpoint.clone(), Some(EndpointFallbackReason::NotFound));
        let mut first = snapshot(&monitor, now);
        assert_eq!(first.requested, requested);
        assert_eq!(first.endpoint, Some(endpoint.clone()));
        assert_eq!(first.fallback, Some(EndpointFallbackReason::NotFound));
        first.endpoint.as_mut().unwrap().id = "snapshot-only".into();
        assert_eq!(snapshot(&monitor, now).endpoint, Some(endpoint));
        assert_eq!(snapshot(&monitor, now).requested, requested);
    }

    #[test]
    fn source_monitor_saturates_ages_and_cumulative_counters() {
        let now = Instant::now();
        let later = now + Duration::from_secs(1);
        let mut monitor = SourceMonitor::new(EndpointSelection::Default);
        monitor.started(later);
        monitor.packets = u64::MAX;
        monitor.total_input_frames = u64::MAX - 1;
        monitor.observe(&[[0.; 2]; 2], false, later);
        let result = snapshot(&monitor, now);
        assert_eq!(result.running_for, Some(Duration::ZERO));
        assert_eq!(result.level.unwrap().age, Duration::ZERO);
        assert_eq!(result.packets, u64::MAX);
        assert_eq!(result.total_input_frames, u64::MAX);
    }
}
