//! 设备音频时钟的纯模型；设备读取、输出队列与线程调度由调用者负责。
use std::{error::Error, fmt, time::Duration};

const QPC_FREQUENCY: u64 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClockPhase {
    Priming,
    Playing,
    Paused,
    Underrun,
    /// 解码结束但设备仍有已排队声音；只有设备确认播完才转 Ended。
    Draining,
    Ended,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockAnchor {
    /// 每次清空设备队列、seek 或变速都换代；可跳过被合并的控制代次。
    pub generation: u64,
    pub media_origin: Duration,
    pub device_origin: u64,
    pub qpc_100ns: u64,
    /// IAudioClock 的频率，不假定设备计数使用采样帧作为单位。
    pub frequency: u64,
    pub rate: f64,
    /// 本代已成功提交到设备的媒体绝对终点，不能使用尚未入队的解码游标。
    pub queued_until: Duration,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockReading {
    pub generation: u64,
    pub device_position: u64,
    /// 与 device_position 同次读取的 QPC，单位为 100ns。
    pub qpc_100ns: u64,
    pub queued_until: Duration,
    /// 由输出循环报告真实状态；设备欠载后先 Underrun，再以新代重锚恢复。
    pub phase: ClockPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockSnapshot {
    pub generation: u64,
    pub media_position: Duration,
    /// 快照所对应的时刻；snapshot_at 返回估计时刻，observe 返回设备观测时刻。
    pub qpc_100ns: u64,
    pub phase: ClockPhase,
    pub advancing: bool,
    pub queued_media: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClockError {
    InvalidFrequency,
    InvalidRate,
    QueueRegressed,
    StaleGeneration,
    DeviceRegressed,
    QpcRegressed,
    NeedsReanchor,
    NotDrained,
}
impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFrequency => "音频设备时钟频率必须大于零",
            Self::InvalidRate => "音频播放倍率必须为有限正数",
            Self::QueueRegressed => "音频已入队终点不能在同代回退或早于媒体锚点",
            Self::StaleGeneration => "音频时钟代次已失效",
            Self::DeviceRegressed => "音频设备位置在同代回退",
            Self::QpcRegressed => "音频观测时刻回退",
            Self::NeedsReanchor => "恢复音频时钟前需要新代次和设备锚点",
            Self::NotDrained => "设备尚未确认播放完音频队列",
        })
    }
}
impl Error for ClockError {}

/// 外推只读取最后一笔硬件基线，不把估计位置写回已确认位置。
#[derive(Clone, Debug)]
pub(crate) struct AudioClockModel {
    anchor: ClockAnchor,
    device_position: u64,
    observed_qpc: u64,
    observed_media: Duration,
    queued_until: Duration,
    phase: ClockPhase,
    progressed: bool,
}

type ClockResult<T> = std::result::Result<T, ClockError>;

impl AudioClockModel {
    pub(crate) fn new(anchor: ClockAnchor) -> ClockResult<Self> {
        if anchor.frequency == 0 {
            return Err(ClockError::InvalidFrequency);
        }
        if !anchor.rate.is_finite() || anchor.rate <= 0. {
            return Err(ClockError::InvalidRate);
        }
        if anchor.queued_until < anchor.media_origin {
            return Err(ClockError::QueueRegressed);
        }
        Ok(Self {
            device_position: anchor.device_origin,
            observed_qpc: anchor.qpc_100ns,
            observed_media: anchor.media_origin,
            queued_until: anchor.queued_until,
            anchor,
            phase: ClockPhase::Priming,
            progressed: false,
        })
    }

    /// 新代允许媒体向后 seek、设备计数归零和变速。只接受向前的代次，
    /// u64 回绕仍按相邻代次处理；半个取值范围的跳跃视为失效控制。
    pub(crate) fn reanchor(&mut self, anchor: ClockAnchor) -> ClockResult<()> {
        let distance = anchor.generation.wrapping_sub(self.anchor.generation);
        if distance == 0 || distance >= 1_u64 << 63 {
            return Err(ClockError::StaleGeneration);
        }
        if anchor.qpc_100ns < self.observed_qpc {
            return Err(ClockError::QpcRegressed);
        }
        let replacement = Self::new(anchor)?;
        *self = replacement;
        Ok(())
    }

    /// 错误观测不修改模型；暂停/欠载的最后一笔设备位置仍可以先结算。
    /// 已冻结的同代不能自行恢复，调用者先重锚再启动设备。
    pub(crate) fn observe(&mut self, reading: ClockReading) -> ClockResult<ClockSnapshot> {
        self.check_generation(reading.generation)?;
        if reading.qpc_100ns < self.observed_qpc {
            return Err(ClockError::QpcRegressed);
        }
        if reading.device_position < self.device_position {
            return Err(ClockError::DeviceRegressed);
        }
        if reading.queued_until < self.queued_until {
            return Err(ClockError::QueueRegressed);
        }
        if self.phase == ClockPhase::Ended && reading.queued_until != self.queued_until {
            return Err(ClockError::NeedsReanchor);
        }
        let frozen = matches!(
            self.phase,
            ClockPhase::Paused | ClockPhase::Underrun | ClockPhase::Ended
        );
        let repeated_eof = self.phase == ClockPhase::Ended && reading.phase == ClockPhase::Draining;
        if frozen && reading.phase != self.phase && !repeated_eof {
            return Err(ClockError::NeedsReanchor);
        }
        let progressed = self.progressed || reading.device_position > self.anchor.device_origin;
        let position = if frozen {
            self.observed_media
        } else {
            let delta = reading.device_position - self.anchor.device_origin;
            let capacity = reading.queued_until - self.anchor.media_origin;
            self.anchor
                .media_origin
                .saturating_add(scale(
                    delta,
                    self.anchor.frequency,
                    self.anchor.rate,
                    capacity,
                ))
                .min(reading.queued_until)
        };
        if reading.phase == ClockPhase::Ended && position < reading.queued_until {
            return Err(ClockError::NotDrained);
        }
        let phase = if self.phase == ClockPhase::Ended
            || reading.phase == ClockPhase::Draining && position == reading.queued_until
        {
            ClockPhase::Ended
        } else if reading.phase == ClockPhase::Playing && !progressed {
            ClockPhase::Priming
        } else {
            reading.phase
        };
        self.device_position = reading.device_position;
        self.observed_qpc = reading.qpc_100ns;
        self.observed_media = position;
        self.queued_until = reading.queued_until;
        self.phase = phase;
        self.progressed = progressed;
        self.snapshot_at(reading.generation, reading.qpc_100ns)
    }

    /// QPC 外推受已提交媒体终点限制；暂停、欠载、初始设备零位置与 Ended 均冻结。
    /// 到达外推上限不代表硬件已播完，Draining 要等待下一笔设备观测确认。
    pub(crate) fn snapshot_at(
        &self,
        generation: u64,
        qpc_100ns: u64,
    ) -> ClockResult<ClockSnapshot> {
        self.check_generation(generation)?;
        let delta = qpc_100ns
            .checked_sub(self.observed_qpc)
            .ok_or(ClockError::QpcRegressed)?;
        let can_advance =
            self.progressed && matches!(self.phase, ClockPhase::Playing | ClockPhase::Draining);
        let remaining = self.queued_until.saturating_sub(self.observed_media);
        let position = if can_advance {
            self.observed_media
                .saturating_add(scale(delta, QPC_FREQUENCY, self.anchor.rate, remaining))
                .min(self.queued_until)
        } else {
            self.observed_media
        };
        Ok(ClockSnapshot {
            generation,
            media_position: position,
            qpc_100ns,
            phase: self.phase,
            advancing: can_advance && position < self.queued_until,
            queued_media: self.queued_until.saturating_sub(position),
        })
    }

    fn check_generation(&self, generation: u64) -> ClockResult<()> {
        if generation == self.anchor.generation {
            Ok(())
        } else {
            Err(ClockError::StaleGeneration)
        }
    }
}

/// 先减绝对设备计数后换算，避免巨大起点的浮点抵消；任何数值溢出只饱和到队列上限。
fn scale(ticks: u64, frequency: u64, rate: f64, limit: Duration) -> Duration {
    let seconds = ticks as f64 / frequency as f64 * rate;
    Duration::try_from_secs_f64(seconds)
        .unwrap_or(limit)
        .min(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }
    fn anchor(rate: f64) -> ClockAnchor {
        ClockAnchor {
            generation: 7,
            media_origin: seconds(10),
            device_origin: 500,
            qpc_100ns: 1_000_000,
            frequency: 1000,
            rate,
            queued_until: seconds(20),
        }
    }
    fn reading(position: u64, qpc: u64, phase: ClockPhase) -> ClockReading {
        ClockReading {
            generation: 7,
            device_position: position,
            qpc_100ns: qpc,
            queued_until: seconds(20),
            phase,
        }
    }

    #[test]
    fn preview_audio_clock_maps_arbitrary_frequency_and_nonzero_origin() {
        for rate in [0.5, 1., 2.] {
            let mut base = anchor(rate);
            base.frequency = 90_000;
            base.device_origin = u64::MAX - 180_000;
            let mut model = AudioClockModel::new(base).unwrap();
            let snapshot = model
                .observe(ClockReading {
                    device_position: base.device_origin + 90_000,
                    ..reading(0, base.qpc_100ns + QPC_FREQUENCY, ClockPhase::Playing)
                })
                .unwrap();
            assert_eq!(
                snapshot.media_position,
                seconds(10) + Duration::from_secs_f64(rate)
            );
            let estimated = model
                .snapshot_at(7, snapshot.qpc_100ns + QPC_FREQUENCY)
                .unwrap();
            assert_eq!(
                estimated.media_position,
                seconds(10) + Duration::from_secs_f64(rate * 2.)
            );
        }
        assert_eq!(
            scale(1, 3, 1., seconds(1)),
            Duration::from_nanos(333_333_333)
        );
    }

    #[test]
    fn preview_audio_clock_priming_does_not_extrapolate_or_advance_when_queue_grows() {
        let mut model = AudioClockModel::new(anchor(1.)).unwrap();
        let mut value = reading(500, 2_000_000, ClockPhase::Playing);
        value.queued_until = seconds(30);
        let snapshot = model.observe(value).unwrap();
        assert_eq!(snapshot.phase, ClockPhase::Priming);
        assert!(!snapshot.advancing);
        assert_eq!(
            model.snapshot_at(7, u64::MAX).unwrap().media_position,
            seconds(10)
        );
    }

    #[test]
    fn preview_audio_clock_estimate_is_bounded_and_never_replaces_hardware_baseline() {
        let mut model = AudioClockModel::new(anchor(2.)).unwrap();
        model
            .observe(reading(1000, 6_000_000, ClockPhase::Playing))
            .unwrap();
        let capped = model.snapshot_at(7, u64::MAX).unwrap();
        assert_eq!(capped.media_position, seconds(20));
        assert_eq!(capped.queued_media, Duration::ZERO);
        assert!(!capped.advancing);
        let next = model
            .observe(reading(1100, 7_000_000, ClockPhase::Playing))
            .unwrap();
        assert_eq!(next.media_position, Duration::from_millis(11_200));
    }

    #[test]
    fn preview_audio_clock_pause_and_underrun_settle_then_freeze() {
        for phase in [ClockPhase::Paused, ClockPhase::Underrun] {
            let mut model = AudioClockModel::new(anchor(1.)).unwrap();
            model
                .observe(reading(1000, 6_000_000, ClockPhase::Playing))
                .unwrap();
            let frozen = model.observe(reading(1250, 8_500_000, phase)).unwrap();
            assert_eq!(frozen.media_position, Duration::from_millis(10_750));
            assert!(!frozen.advancing);
            assert_eq!(
                model.snapshot_at(7, u64::MAX).unwrap().media_position,
                frozen.media_position
            );
            assert_eq!(
                model
                    .observe(reading(1500, 11_000_000, phase))
                    .unwrap()
                    .media_position,
                frozen.media_position
            );
            assert_eq!(
                model.observe(reading(1500, 11_000_001, ClockPhase::Playing)),
                Err(ClockError::NeedsReanchor)
            );
        }
    }

    #[test]
    fn preview_audio_clock_eof_drains_before_ending_and_does_not_fake_confirmation() {
        let mut model = AudioClockModel::new(anchor(1.)).unwrap();
        let pending = model
            .observe(reading(1500, 11_000_000, ClockPhase::Draining))
            .unwrap();
        assert_eq!(pending.media_position, seconds(11));
        assert!(pending.advancing);
        let estimated = model.snapshot_at(7, u64::MAX).unwrap();
        assert_eq!(estimated.media_position, seconds(20));
        assert_eq!(estimated.phase, ClockPhase::Draining);
        assert_eq!(
            model.observe(reading(2000, 16_000_000, ClockPhase::Ended)),
            Err(ClockError::NotDrained)
        );
        let ended = model
            .observe(reading(10_500, 101_000_000, ClockPhase::Draining))
            .unwrap();
        assert_eq!(ended.phase, ClockPhase::Ended);
        assert_eq!(ended.media_position, seconds(20));
        assert_eq!(
            model.snapshot_at(7, u64::MAX).unwrap().media_position,
            seconds(20)
        );
        let repeated = model
            .observe(reading(11_500, 111_000_000, ClockPhase::Draining))
            .unwrap();
        assert_eq!(repeated.phase, ClockPhase::Ended);
        assert_eq!(repeated.media_position, seconds(20));
        assert!(!repeated.advancing);
        assert_eq!(
            model.observe(ClockReading {
                queued_until: seconds(21),
                ..reading(12_000, 116_000_000, ClockPhase::Draining)
            }),
            Err(ClockError::NeedsReanchor)
        );
    }

    #[test]
    fn preview_audio_clock_reset_reanchors_seek_and_rate_and_rejects_old_generation() {
        let mut model = AudioClockModel::new(anchor(1.)).unwrap();
        model
            .observe(reading(1500, 11_000_000, ClockPhase::Playing))
            .unwrap();
        let mut next = anchor(0.5);
        next.generation = 9;
        next.media_origin = seconds(2);
        next.queued_until = seconds(5);
        next.device_origin = 0;
        next.qpc_100ns = 21_000_000;
        model.reanchor(next).unwrap();
        assert_eq!(
            model.observe(reading(2000, 22_000_000, ClockPhase::Playing)),
            Err(ClockError::StaleGeneration)
        );
        let result = model
            .observe(ClockReading {
                generation: 9,
                device_position: 1000,
                qpc_100ns: 31_000_000,
                queued_until: seconds(5),
                phase: ClockPhase::Playing,
            })
            .unwrap();
        assert_eq!(result.media_position, Duration::from_millis(2500));
        assert_eq!(model.reanchor(anchor(1.)), Err(ClockError::StaleGeneration));
    }

    #[test]
    fn preview_audio_clock_generation_wrap_is_new_but_same_or_old_reset_is_rejected() {
        let mut base = anchor(1.);
        base.generation = u64::MAX;
        let mut model = AudioClockModel::new(base).unwrap();
        let mut next = base;
        next.generation = 0;
        model.reanchor(next).unwrap();
        assert_eq!(
            model.snapshot_at(u64::MAX, base.qpc_100ns),
            Err(ClockError::StaleGeneration)
        );
        assert_eq!(model.reanchor(next), Err(ClockError::StaleGeneration));
        assert_eq!(model.reanchor(base), Err(ClockError::StaleGeneration));
    }

    #[test]
    fn preview_audio_clock_rejected_readings_leave_last_good_state_intact() {
        let mut model = AudioClockModel::new(anchor(1.)).unwrap();
        let expected = model
            .observe(reading(1500, 11_000_000, ClockPhase::Playing))
            .unwrap();
        let invalid = [
            (
                reading(1499, 12_000_000, ClockPhase::Playing),
                ClockError::DeviceRegressed,
            ),
            (
                reading(1600, 10_999_999, ClockPhase::Playing),
                ClockError::QpcRegressed,
            ),
            (
                ClockReading {
                    queued_until: seconds(19),
                    ..reading(1600, 12_000_000, ClockPhase::Playing)
                },
                ClockError::QueueRegressed,
            ),
        ];
        for (reading, error) in invalid {
            assert_eq!(model.observe(reading), Err(error));
            assert_eq!(model.snapshot_at(7, expected.qpc_100ns).unwrap(), expected);
        }
        assert_eq!(
            model.snapshot_at(7, expected.qpc_100ns - 1),
            Err(ClockError::QpcRegressed)
        );
    }

    #[test]
    fn preview_audio_clock_invalid_parameters_and_reset_are_transactional() {
        for rate in [0., -1., f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                AudioClockModel::new(anchor(rate)).unwrap_err(),
                ClockError::InvalidRate
            );
        }
        assert_eq!(
            AudioClockModel::new(ClockAnchor {
                frequency: 0,
                ..anchor(1.)
            })
            .unwrap_err(),
            ClockError::InvalidFrequency
        );
        assert_eq!(
            AudioClockModel::new(ClockAnchor {
                queued_until: seconds(9),
                ..anchor(1.)
            })
            .unwrap_err(),
            ClockError::QueueRegressed
        );
        let mut model = AudioClockModel::new(anchor(1.)).unwrap();
        let expected = model.snapshot_at(7, 1_000_000).unwrap();
        assert_eq!(
            model.reanchor(ClockAnchor {
                generation: 8,
                frequency: 0,
                ..anchor(1.)
            }),
            Err(ClockError::InvalidFrequency)
        );
        assert_eq!(
            model.reanchor(ClockAnchor {
                generation: 8,
                qpc_100ns: 0,
                ..anchor(1.)
            }),
            Err(ClockError::QpcRegressed)
        );
        assert_eq!(model.snapshot_at(7, 1_000_000).unwrap(), expected);
    }

    #[test]
    fn preview_audio_clock_extreme_ticks_rates_and_duration_do_not_overflow() {
        let mut base = anchor(f64::MAX);
        base.device_origin = 0;
        base.frequency = 1;
        base.media_origin = Duration::MAX - seconds(1);
        base.queued_until = Duration::MAX;
        let mut model = AudioClockModel::new(base).unwrap();
        let result = model
            .observe(ClockReading {
                device_position: u64::MAX,
                qpc_100ns: u64::MAX,
                queued_until: Duration::MAX,
                ..reading(0, 0, ClockPhase::Playing)
            })
            .unwrap();
        assert_eq!(result.media_position, Duration::MAX);
        assert_eq!(result.queued_media, Duration::ZERO);
        assert_eq!(model.snapshot_at(7, u64::MAX).unwrap(), result);
        assert_eq!(
            model.observe(ClockReading {
                device_position: 0,
                qpc_100ns: u64::MAX,
                queued_until: Duration::MAX,
                ..reading(0, 0, ClockPhase::Playing)
            }),
            Err(ClockError::DeviceRegressed)
        );
        assert_eq!(scale(u64::MAX, 1, f64::MAX, seconds(3)), seconds(3));
        assert_eq!(
            scale(1, u64::MAX, f64::MIN_POSITIVE, seconds(3)),
            Duration::ZERO
        );
    }
}
