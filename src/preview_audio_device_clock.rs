//! WASAPI 设备时钟适配；只观测已初始化的 client，不负责启动、暂停或重置设备。
use crate::{
    preview_audio_clock::{
        AudioClockModel, ClockAnchor, ClockError, ClockPhase, ClockReading, ClockSnapshot,
    },
    Result,
};
use std::time::Duration;
use windows::Win32::Media::Audio::{IAudioClient, IAudioClock};

/// 先核对界面传输代次，再使用内部设备时钟代次；欠载重置不产生新的界面命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublishedClock {
    pub transport_generation: u64,
    pub clock: ClockSnapshot,
}

/// 可跨线程读取的纯模型副本，不持有 COM 设备；外推仍沿用同一时钟模型。
#[derive(Clone)]
pub(crate) struct ClockProjection {
    transport_generation: u64,
    clock_epoch: u64,
    model: AudioClockModel,
}
impl ClockProjection {
    pub(crate) fn snapshot_at(&self, qpc_100ns: u64) -> ModelResult<PublishedClock> {
        Ok(PublishedClock {
            transport_generation: self.transport_generation,
            clock: self.model.snapshot_at(self.clock_epoch, qpc_100ns)?,
        })
    }
}

pub(crate) struct OutputClock {
    clock: IAudioClock,
    // 保持服务所属 client 的生命周期，释放时先释放时钟服务引用。
    _client: IAudioClient,
    state: OutputClockState,
}

impl OutputClock {
    /// 由音频输出线程在 client.Initialize 成功后创建；不调用 Start/Stop/Reset。
    pub(crate) fn from_client(
        client: &IAudioClient,
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
    ) -> Result<Self> {
        Self::from_client_with_epoch(client, transport_generation, media_origin, rate, 0)
    }

    /// 新 client 沿用输出 actor 分配的代次，不把同一传输代次的设备身份归零。
    pub(crate) fn from_client_with_epoch(
        client: &IAudioClient,
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        clock_epoch: u64,
    ) -> Result<Self> {
        let clock: IAudioClock = unsafe { client.GetService()? };
        let frequency = unsafe { clock.GetFrequency()? };
        let (device_position, qpc_100ns) = read_position(&clock)?;
        let state = OutputClockState::with_epoch(
            transport_generation,
            media_origin,
            rate,
            frequency,
            device_position,
            qpc_100ns,
            clock_epoch,
        )?;
        Ok(Self {
            clock,
            _client: client.clone(),
            state,
        })
    }

    /// 调用者先 Stop/Reset，再用新的同次设备位置/QPC 重锚。
    /// 失败时保留此前的传输代次、设备代次和模型，不发布部分成功的状态。
    pub(crate) fn reanchor(
        &mut self,
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        queued_until: Duration,
    ) -> Result<()> {
        let (device_position, qpc_100ns) = read_position(&self.clock)?;
        self.state.reanchor(
            transport_generation,
            media_origin,
            rate,
            queued_until,
            device_position,
            qpc_100ns,
        )?;
        Ok(())
    }

    /// queued_until 只包含已成功 ReleaseBuffer 的有效媒体；phase 由输出循环提供。
    pub(crate) fn observe(
        &mut self,
        queued_until: Duration,
        phase: ClockPhase,
    ) -> Result<PublishedClock> {
        let (device_position, qpc_100ns) = read_position(&self.clock)?;
        Ok(self
            .state
            .observe(device_position, qpc_100ns, queued_until, phase)?)
    }

    /// 仅对已确认设备基线作有界外推，不访问设备、不修改硬件观测位置。
    pub(crate) fn snapshot_at(&self, qpc_100ns: u64) -> Result<PublishedClock> {
        Ok(self.state.snapshot_at(qpc_100ns)?)
    }

    pub(crate) fn projection(&self) -> ClockProjection {
        ClockProjection {
            transport_generation: self.state.transport_generation,
            clock_epoch: self.state.clock_epoch,
            model: self.state.model.clone(),
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.state.clock_epoch
    }
}

fn read_position(clock: &IAudioClock) -> Result<(u64, u64)> {
    let mut position = 0;
    let mut qpc_100ns = 0;
    // QPC 使用 GetPosition 同次返回的 100ns 值，不另取墙钟与设备位置拼接。
    unsafe { clock.GetPosition(&mut position, Some(&mut qpc_100ns))? };
    Ok((position, qpc_100ns))
}

/// 与 COM 接口分离的发布状态，便于在不连接设备的情况下验证换代事务。
struct OutputClockState {
    transport_generation: u64,
    clock_epoch: u64,
    frequency: u64,
    model: AudioClockModel,
}
type ModelResult<T> = std::result::Result<T, ClockError>;

impl OutputClockState {
    fn new(
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        frequency: u64,
        device_position: u64,
        qpc_100ns: u64,
    ) -> ModelResult<Self> {
        Self::with_epoch(
            transport_generation,
            media_origin,
            rate,
            frequency,
            device_position,
            qpc_100ns,
            0,
        )
    }

    fn with_epoch(
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        frequency: u64,
        device_position: u64,
        qpc_100ns: u64,
        clock_epoch: u64,
    ) -> ModelResult<Self> {
        let model = AudioClockModel::new(ClockAnchor {
            generation: clock_epoch,
            media_origin,
            device_origin: device_position,
            qpc_100ns,
            frequency,
            rate,
            queued_until: media_origin,
        })?;
        Ok(Self {
            transport_generation,
            clock_epoch,
            frequency,
            model,
        })
    }

    fn reanchor(
        &mut self,
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        queued_until: Duration,
        device_position: u64,
        qpc_100ns: u64,
    ) -> ModelResult<()> {
        let clock_epoch = self.clock_epoch.wrapping_add(1);
        self.model.reanchor(ClockAnchor {
            generation: clock_epoch,
            media_origin,
            device_origin: device_position,
            qpc_100ns,
            frequency: self.frequency,
            rate,
            queued_until,
        })?;
        self.clock_epoch = clock_epoch;
        self.transport_generation = transport_generation;
        Ok(())
    }

    fn observe(
        &mut self,
        device_position: u64,
        qpc_100ns: u64,
        queued_until: Duration,
        phase: ClockPhase,
    ) -> ModelResult<PublishedClock> {
        let clock = self.model.observe(ClockReading {
            generation: self.clock_epoch,
            device_position,
            qpc_100ns,
            queued_until,
            phase,
        })?;
        Ok(PublishedClock {
            transport_generation: self.transport_generation,
            clock,
        })
    }

    fn snapshot_at(&self, qpc_100ns: u64) -> ModelResult<PublishedClock> {
        Ok(PublishedClock {
            transport_generation: self.transport_generation,
            clock: self.model.snapshot_at(self.clock_epoch, qpc_100ns)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn preview_audio_device_clock_new_client_continues_epoch_and_played_origin() {
        let mut old =
            OutputClockState::with_epoch(41, seconds(10), 1., 1000, 0, 1_000_000, 7).unwrap();
        old.reanchor(41, seconds(10), 1., seconds(11), 0, 2_000_000)
            .unwrap();
        let played = old
            .observe(500, 7_000_000, seconds(11), ClockPhase::Playing)
            .unwrap();
        let mut reopened = OutputClockState::with_epoch(
            41,
            played.clock.media_position,
            1.,
            1000,
            0,
            8_000_000,
            old.clock_epoch.wrapping_add(1),
        )
        .unwrap();
        let held = reopened
            .observe(
                0,
                8_000_000,
                played.clock.media_position,
                ClockPhase::Paused,
            )
            .unwrap();
        assert_eq!(held.clock.media_position, Duration::from_millis(10_500));
        assert_eq!(held.transport_generation, played.transport_generation);
        assert_ne!(held.clock.generation, played.clock.generation);
        reopened
            .reanchor(
                41,
                held.clock.media_position,
                1.,
                held.clock.media_position,
                0,
                9_000_000,
            )
            .unwrap();
        assert_eq!(reopened.clock_epoch, 10);

        let mut wrapped =
            OutputClockState::with_epoch(41, seconds(1), 1., 1000, 0, 1_000_000, u64::MAX).unwrap();
        wrapped
            .reanchor(41, seconds(1), 1., seconds(1), 0, 2_000_000)
            .unwrap();
        assert_eq!(wrapped.clock_epoch, 0);
    }

    #[test]
    fn preview_audio_device_clock_keeps_transport_generation_separate_from_reset_epoch() {
        let mut state = OutputClockState::new(41, seconds(10), 1., 1000, 500, 1_000_000).unwrap();
        let first = state
            .observe(1500, 11_000_000, seconds(20), ClockPhase::Playing)
            .unwrap();
        assert_eq!(first.transport_generation, 41);
        assert_eq!(first.clock.generation, 0);
        assert_eq!(first.clock.media_position, seconds(11));
        state
            .reanchor(41, seconds(11), 0.5, seconds(12), 0, 21_000_000)
            .unwrap();
        let recovered = state
            .observe(1000, 31_000_000, seconds(12), ClockPhase::Playing)
            .unwrap();
        assert_eq!(recovered.transport_generation, 41);
        assert_eq!(recovered.clock.generation, 1);
        assert_eq!(
            recovered.clock.media_position,
            Duration::from_millis(11_500)
        );
        state
            .reanchor(42, seconds(2), 1., seconds(3), 0, 41_000_000)
            .unwrap();
        let seek = state.snapshot_at(41_000_000).unwrap();
        assert_eq!(seek.transport_generation, 42);
        assert_eq!(seek.clock.generation, 2);
        assert_eq!(seek.clock.media_position, seconds(2));
    }

    #[test]
    fn preview_audio_device_clock_failed_reanchor_does_not_retag_or_advance_epoch() {
        let mut state = OutputClockState::new(7, seconds(10), 1., 1000, 500, 1_000_000).unwrap();
        let expected = state
            .observe(1500, 11_000_000, seconds(20), ClockPhase::Playing)
            .unwrap();
        assert_eq!(
            state.reanchor(8, seconds(2), f64::NAN, seconds(3), 0, 21_000_000),
            Err(ClockError::InvalidRate)
        );
        assert_eq!(
            state.reanchor(8, seconds(2), 1., seconds(3), 0, 10_000_000),
            Err(ClockError::QpcRegressed)
        );
        assert_eq!(
            state.reanchor(8, seconds(2), 1., seconds(1), 0, 21_000_000),
            Err(ClockError::QueueRegressed)
        );
        assert_eq!(state.snapshot_at(11_000_000).unwrap(), expected);
        state
            .reanchor(8, seconds(2), 1., seconds(3), 0, 21_000_000)
            .unwrap();
        let changed = state.snapshot_at(21_000_000).unwrap();
        assert_eq!(changed.transport_generation, 8);
        assert_eq!(changed.clock.generation, 1);
    }

    #[test]
    fn preview_audio_device_clock_publishes_model_bounds_without_device_access() {
        let mut state = OutputClockState::new(9, seconds(3), 2., 90_000, 9000, 1_000_000).unwrap();
        let observed = state
            .observe(54_000, 6_000_000, seconds(5), ClockPhase::Draining)
            .unwrap();
        assert_eq!(observed.clock.media_position, seconds(4));
        let bounded = state.snapshot_at(u64::MAX).unwrap();
        assert_eq!(bounded.transport_generation, 9);
        assert_eq!(bounded.clock.generation, 0);
        assert_eq!(bounded.clock.media_position, seconds(5));
        assert_eq!(bounded.clock.phase, ClockPhase::Draining);
        assert!(!bounded.clock.advancing);
    }

    #[test]
    fn preview_audio_device_clock_projection_is_a_frozen_copy_of_the_device_baseline() {
        let mut state = OutputClockState::new(9, seconds(3), 2., 90_000, 9000, 1_000_000).unwrap();
        state
            .observe(54_000, 6_000_000, seconds(5), ClockPhase::Playing)
            .unwrap();
        let projection = ClockProjection {
            transport_generation: state.transport_generation,
            clock_epoch: state.clock_epoch,
            model: state.model.clone(),
        };
        let at_frame = projection.snapshot_at(8_500_000).unwrap();
        assert_eq!(at_frame, state.snapshot_at(8_500_000).unwrap());
        assert_eq!(at_frame.clock.media_position, Duration::from_millis(4500));
        assert_eq!(
            projection.snapshot_at(5_000_000),
            Err(ClockError::QpcRegressed)
        );
        state
            .reanchor(10, seconds(1), 1., seconds(1), 0, 10_000_000)
            .unwrap();
        assert_eq!(projection.snapshot_at(8_500_000).unwrap(), at_frame);
        assert_eq!(
            projection
                .snapshot_at(u64::MAX)
                .unwrap()
                .clock
                .media_position,
            seconds(5)
        );
    }
}
