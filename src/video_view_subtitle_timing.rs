//! 外部字幕的有界时间偏移；查询保留视频时间戳的原始精度。
use std::time::Duration;

const MAX_OFFSET_MILLIS: i32 = 10 * 60 * 1000;

/// 正值让字幕延后出现，负值让字幕提前出现。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubtitleOffset {
    millis: i32,
}

impl SubtitleOffset {
    pub(crate) fn millis(&self) -> i32 {
        self.millis
    }

    /// 返回偏移是否实际改变；极大增量也只停在正负十分钟边界。
    pub(crate) fn adjust(&mut self, delta_millis: i32) -> bool {
        let next = self
            .millis
            .saturating_add(delta_millis)
            .clamp(-MAX_OFFSET_MILLIS, MAX_OFFSET_MILLIS);
        let changed = next != self.millis;
        self.millis = next;
        changed
    }

    pub(crate) fn reset(&mut self) -> bool {
        let changed = self.millis != 0;
        self.millis = 0;
        changed
    }

    /// 将当前视频PTS映射到字幕时间；零点之前及Duration溢出时没有可查询时间。
    pub(crate) fn query_position(&self, frame_pts: Duration) -> Option<Duration> {
        let offset = Duration::from_millis(u64::from(self.millis.unsigned_abs()));
        if self.millis < 0 {
            frame_pts.checked_add(offset)
        } else {
            frame_pts.checked_sub(offset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_zero_and_reset_preserve_precision_and_report_real_changes() {
        let mut offset = SubtitleOffset::default();
        let pts = Duration::new(12, 345_678_901);
        assert_eq!(offset.millis(), 0);
        assert_eq!(offset.query_position(pts), Some(pts));
        assert_eq!(offset.query_position(Duration::ZERO), Some(Duration::ZERO));
        assert!(!offset.adjust(0));
        assert!(!offset.reset());
        assert!(offset.adjust(250));
        assert!(!offset.adjust(0));
        assert!(offset.reset());
        assert_eq!(offset.millis(), 0);
        assert!(!offset.reset());
    }

    #[test]
    fn delayed_subtitles_have_no_query_before_zero_and_keep_nanoseconds() {
        let mut offset = SubtitleOffset::default();
        assert!(offset.adjust(1500));
        assert_eq!(offset.query_position(Duration::ZERO), None);
        assert_eq!(offset.query_position(Duration::new(1, 499_999_999)), None);
        assert_eq!(
            offset.query_position(Duration::from_millis(1500)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            offset.query_position(Duration::new(1, 500_000_123)),
            Some(Duration::from_nanos(123))
        );
    }

    #[test]
    fn advanced_subtitles_query_later_text_and_can_cross_back_through_zero() {
        let mut offset = SubtitleOffset::default();
        assert!(offset.adjust(-1500));
        assert_eq!(
            offset.query_position(Duration::ZERO),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            offset.query_position(Duration::new(3, 123_456_789)),
            Some(Duration::new(4, 623_456_789))
        );
        assert!(offset.adjust(1500));
        assert_eq!(offset.millis(), 0);
        assert!(!offset.reset());
        assert!(offset.adjust(1));
        assert_eq!(offset.query_position(Duration::ZERO), None);
    }

    #[test]
    fn adjustments_clamp_at_both_limits_without_integer_overflow() {
        let mut offset = SubtitleOffset::default();
        assert!(offset.adjust(MAX_OFFSET_MILLIS));
        assert_eq!(offset.millis(), MAX_OFFSET_MILLIS);
        assert!(!offset.adjust(1));
        assert!(!offset.adjust(i32::MAX));
        assert_eq!(offset.millis(), MAX_OFFSET_MILLIS);
        assert!(offset.adjust(-2 * MAX_OFFSET_MILLIS));
        assert_eq!(offset.millis(), -MAX_OFFSET_MILLIS);
        assert!(!offset.adjust(-1));
        assert!(!offset.adjust(i32::MIN));
        assert_eq!(offset.millis(), -MAX_OFFSET_MILLIS);
        assert!(offset.adjust(i32::MAX));
        assert_eq!(offset.millis(), MAX_OFFSET_MILLIS);
        assert!(offset.adjust(i32::MIN));
        assert_eq!(offset.millis(), -MAX_OFFSET_MILLIS);
        assert_eq!(
            offset.query_position(Duration::ZERO),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn duration_extremes_are_checked_without_saturation() {
        let mut offset = SubtitleOffset::default();
        assert_eq!(offset.query_position(Duration::MAX), Some(Duration::MAX));
        assert!(offset.adjust(-1));
        assert_eq!(offset.query_position(Duration::MAX), None);
        let last_valid = Duration::MAX - Duration::from_millis(1);
        assert_eq!(offset.query_position(last_valid), Some(Duration::MAX));
        assert_eq!(
            offset.query_position(last_valid + Duration::from_nanos(1)),
            None
        );
        assert!(offset.adjust(2));
        assert_eq!(
            offset.query_position(Duration::MAX),
            Some(Duration::MAX - Duration::from_millis(1))
        );
    }
}
