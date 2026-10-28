//! 重采样输出相对原始采集包的标称相位；保留原 QPC 锚，不修正设备漂移。

const MAX_RATE: u32 = 384_000;
const TICKS_PER_SECOND: i128 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhaseError {
    InvalidRates,
    EmptyOutput,
    InvalidFirstPhase,
    FrameOutOfRange,
    ArithmeticOverflow,
    QpcOutOfRange,
}

/// 首输出点相对包首输入帧的偏移，分母为 `output_rate`。
/// 负偏移可引用保留的上一输入帧；原包时间戳的有效性与 epoch 由调用方判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputPhase {
    pub first_input_offset_num: i64,
    pub input_rate: u32,
    pub output_rate: u32,
    pub output_frames: usize,
}

impl OutputPhase {
    /// 只校验相位自身；输入包长度和 PCM 数量由持有原始包的调用方核对。
    pub(crate) fn is_valid(self) -> bool {
        self.validate().is_ok()
    }

    pub(crate) fn validate(self) -> Result<(), PhaseError> {
        if !(1..=MAX_RATE).contains(&self.input_rate) || !(1..=MAX_RATE).contains(&self.output_rate)
        {
            return Err(PhaseError::InvalidRates);
        }
        if self.output_frames == 0 {
            return Err(PhaseError::EmptyOutput);
        }
        if self.first_input_offset_num < -i64::from(self.output_rate)
            || self.first_input_offset_num >= i64::from(self.input_rate)
        {
            return Err(PhaseError::InvalidFirstPhase);
        }
        Ok(())
    }

    /// 第 j 个输出点的标称 100ns 闭区间；非整数点的上下界只差一个 tick。
    /// 原锚可为零。偏移先按有符号数求 floor/ceil，越出 u64 时间范围则失败。
    pub(crate) fn qpc_bounds(self, raw_qpc: u64, j: usize) -> Option<(u64, u64)> {
        self.qpc_bounds_checked(raw_qpc, j).ok()
    }

    pub(crate) fn qpc_bounds_checked(
        self,
        raw_qpc: u64,
        j: usize,
    ) -> Result<(u64, u64), PhaseError> {
        self.validate()?;
        if j >= self.output_frames {
            return Err(PhaseError::FrameOutOfRange);
        }
        let input_rate = i128::from(self.input_rate);
        let output_rate = i128::from(self.output_rate);
        let input_offset_num = i128::try_from(j)
            .map_err(|_| PhaseError::ArithmeticOverflow)?
            .checked_mul(input_rate)
            .and_then(|offset| offset.checked_add(i128::from(self.first_input_offset_num)))
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let numerator = input_offset_num
            .checked_mul(TICKS_PER_SECOND)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let denominator = output_rate
            .checked_mul(input_rate)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        // denominator 为正；欧几里得除法对负相位同样给出向下取整。
        let lower = numerator
            .checked_div_euclid(denominator)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let remainder = numerator
            .checked_rem_euclid(denominator)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let upper = lower
            .checked_add(i128::from(remainder != 0))
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let anchor = i128::from(raw_qpc);
        let lower = anchor
            .checked_add(lower)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        let upper = anchor
            .checked_add(upper)
            .ok_or(PhaseError::ArithmeticOverflow)?;
        Ok((
            u64::try_from(lower).map_err(|_| PhaseError::QpcOutOfRange)?,
            u64::try_from(upper).map_err(|_| PhaseError::QpcOutOfRange)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputPhase, PhaseError};

    fn phase(input_rate: u32, output_rate: u32, first: i64, frames: usize) -> OutputPhase {
        OutputPhase {
            first_input_offset_num: first,
            input_rate,
            output_rate,
            output_frames: frames,
        }
    }

    #[test]
    fn capture_phase_44100_to_48000_keeps_cross_packet_negative_phase() {
        // 441 个输入帧后，下一输出点仍在上包末帧与新包首帧之间。
        let previous = phase(44_100, 48_000, 0, 479);
        let next = phase(44_100, 48_000, -44_100, 480);
        assert!(next.is_valid());
        assert_eq!(
            previous.qpc_bounds(1_000_000, 478),
            Some((1_099_583, 1_099_584))
        );
        assert_eq!(next.qpc_bounds(1_100_000, 0), Some((1_099_791, 1_099_792)));
        assert_eq!(next.qpc_bounds(1_100_000, 1), Some((1_100_000, 1_100_000)));
        assert_eq!(next.qpc_bounds(1_100_000, 2), Some((1_100_208, 1_100_209)));
    }

    #[test]
    fn capture_phase_48000_to_48000_preserves_anchor_and_tick_bounds() {
        let phase = phase(48_000, 48_000, 0, 4);
        assert_eq!(phase.qpc_bounds(0, 0), Some((0, 0)));
        assert_eq!(phase.qpc_bounds(1_000, 0), Some((1_000, 1_000)));
        assert_eq!(phase.qpc_bounds(1_000, 1), Some((1_208, 1_209)));
        assert_eq!(phase.qpc_bounds(1_000, 2), Some((1_416, 1_417)));
        assert_eq!(phase.qpc_bounds(1_000, 3), Some((1_625, 1_625)));
    }

    #[test]
    fn capture_phase_192000_to_48000_keeps_input_phase_before_dividing() {
        // 首点是包首之后第 3 个输入帧，输出间隔仍为 1/48000 秒。
        let phase = phase(192_000, 48_000, 144_000, 2);
        assert!(phase.is_valid());
        assert_eq!(phase.qpc_bounds(1_000_000, 0), Some((1_000_156, 1_000_157)));
        assert_eq!(phase.qpc_bounds(1_000_000, 1), Some((1_000_364, 1_000_365)));
    }

    #[test]
    fn capture_phase_negative_offset_checks_signed_result_without_saturation() {
        let previous = phase(48_000, 48_000, -48_000, 1);
        assert!(previous.is_valid());
        assert_eq!(previous.qpc_bounds(1_000, 0), Some((791, 792)));
        assert_eq!(previous.qpc_bounds(0, 0), None);
        assert_eq!(
            previous.qpc_bounds_checked(0, 0),
            Err(PhaseError::QpcOutOfRange)
        );
        // 上界已到零，下界仍为负，不能伪造一个被截断的闭区间。
        let fraction = phase(384_000, 384_000, -1, 1);
        assert_eq!(fraction.qpc_bounds(0, 0), None);
    }

    #[test]
    fn capture_phase_rejects_timestamp_overflow_and_huge_indices() {
        let ordinary = phase(48_000, 48_000, 0, 2);
        assert_eq!(ordinary.qpc_bounds(u64::MAX, 0), Some((u64::MAX, u64::MAX)));
        assert_eq!(ordinary.qpc_bounds(u64::MAX, 1), None);
        let huge = phase(1, 384_000, 0, usize::MAX);
        assert_eq!(huge.qpc_bounds(u64::MAX, usize::MAX - 1), None);
        assert_eq!(huge.qpc_bounds(0, usize::MAX), None);
    }

    #[test]
    fn capture_phase_rejects_invalid_rates_empty_output_and_illegal_first_offset() {
        for invalid in [
            phase(0, 48_000, 0, 1),
            phase(48_000, 0, 0, 1),
            phase(384_001, 48_000, 0, 1),
            phase(48_000, 384_001, 0, 1),
            phase(48_000, 48_000, 0, 0),
            phase(44_100, 48_000, -48_001, 1),
            phase(44_100, 48_000, 44_100, 1),
            phase(48_000, 48_000, i64::MIN, 1),
            phase(48_000, 48_000, i64::MAX, 1),
        ] {
            assert!(!invalid.is_valid());
            assert_eq!(invalid.qpc_bounds(1_000_000, 0), None);
        }
        let valid = phase(384_000, 384_000, 0, 1);
        assert!(valid.is_valid());
        assert_eq!(valid.qpc_bounds(0, 0), Some((0, 0)));
        assert_eq!(valid.qpc_bounds(0, 1), None);
    }

    #[test]
    fn capture_phase_checked_errors_distinguish_invalid_input_and_time_range() {
        for (invalid, error) in [
            (phase(0, 48_000, 0, 1), PhaseError::InvalidRates),
            (phase(48_000, 48_000, 0, 0), PhaseError::EmptyOutput),
            (
                phase(48_000, 48_000, i64::MAX, 1),
                PhaseError::InvalidFirstPhase,
            ),
        ] {
            assert_eq!(invalid.validate(), Err(error));
            assert_eq!(invalid.qpc_bounds_checked(1_000, 0), Err(error));
        }
        let valid = phase(48_000, 48_000, 0, 2);
        assert_eq!(
            valid.qpc_bounds_checked(0, 2),
            Err(PhaseError::FrameOutOfRange)
        );
        assert_eq!(
            valid.qpc_bounds_checked(u64::MAX, 1),
            Err(PhaseError::QpcOutOfRange)
        );
    }
}
