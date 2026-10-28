//! GIF 导出的纯参数模型；范围按来源时长严格核验，不自动截断输入。
use crate::Result;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GifOptions {
    pub start: Duration,
    pub end: Option<Duration>,
    pub fps: u32,
    pub max_side: u32,
    pub colors: usize,
}

impl Default for GifOptions {
    fn default() -> Self {
        Self {
            start: Duration::ZERO,
            end: None,
            fps: 15,
            max_side: 1280,
            colors: 256,
        }
    }
}

impl GifOptions {
    /// 核验与来源时长无关的参数，保留调用方传入的值。
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=15).contains(&self.fps) {
            return Err(tr!(
                "GIF 帧率必须在 1 到 15 之间",
                "GIF frame rate must be between 1 and 15"
            )
            .into());
        }
        if !(128..=1280).contains(&self.max_side) {
            return Err(tr!(
                "GIF 最长边必须在 128 到 1280 像素之间",
                "GIF maximum side must be between 128 and 1280 pixels"
            )
            .into());
        }
        if !matches!(self.colors, 64 | 128 | 256) {
            return Err(tr!(
                "GIF 颜色数必须为 64、128 或 256",
                "GIF color count must be 64, 128 or 256"
            )
            .into());
        }
        if self.end.is_some_and(|end| end <= self.start) {
            return Err(tr!(
                "GIF 结束时间必须晚于开始时间",
                "GIF end time must be after its start time"
            )
            .into());
        }
        Ok(())
    }

    /// 返回严格位于来源时长内的非空范围；未指定结束时间时使用来源结尾。
    pub(crate) fn range(&self, source_duration: Duration) -> Result<(Duration, Duration)> {
        self.validate()?;
        if source_duration.is_zero() {
            return Err(tr!(
                "视频时长必须大于零才能导出 GIF",
                "The video duration must be greater than zero to export a GIF"
            )
            .into());
        }
        if self.start >= source_duration {
            return Err(tr!(
                "GIF 开始时间必须早于视频结尾",
                "GIF start time must be before the end of the video"
            )
            .into());
        }
        let end = self.end.unwrap_or(source_duration);
        if end > source_duration {
            return Err(tr!(
                "GIF 结束时间不能超过视频时长",
                "GIF end time must not exceed the video duration"
            )
            .into());
        }
        if end <= self.start {
            return Err(tr!(
                "GIF 结束时间必须晚于开始时间",
                "GIF end time must be after its start time"
            )
            .into());
        }
        Ok((self.start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_the_whole_source_without_changing_options() {
        let options = GifOptions::default();
        assert_eq!(
            options,
            GifOptions {
                start: Duration::ZERO,
                end: None,
                fps: 15,
                max_side: 1280,
                colors: 256,
            }
        );
        assert!(options.validate().is_ok());
        assert_eq!(
            options.range(Duration::from_secs(3)).unwrap(),
            (Duration::ZERO, Duration::from_secs(3))
        );
        assert_eq!(options.end, None);
    }

    #[test]
    fn numeric_options_accept_boundaries_and_reject_outside_values() {
        for fps in [1, 15] {
            for max_side in [128, 1280] {
                for colors in [64, 128, 256] {
                    assert!(GifOptions {
                        fps,
                        max_side,
                        colors,
                        ..GifOptions::default()
                    }
                    .validate()
                    .is_ok());
                }
            }
        }
        for fps in [0, 16, u32::MAX] {
            assert!(GifOptions {
                fps,
                ..GifOptions::default()
            }
            .validate()
            .is_err());
        }
        for max_side in [0, 127, 1281, u32::MAX] {
            assert!(GifOptions {
                max_side,
                ..GifOptions::default()
            }
            .validate()
            .is_err());
        }
        for colors in [0, 63, 65, 127, 129, 255, 257, usize::MAX] {
            assert!(GifOptions {
                colors,
                ..GifOptions::default()
            }
            .validate()
            .is_err());
        }
    }

    #[test]
    fn ranges_reject_empty_reversed_and_out_of_source_inputs() {
        let source = Duration::from_secs(10);
        assert!(GifOptions::default().range(Duration::ZERO).is_err());
        for end in [
            Duration::ZERO,
            Duration::from_secs(4),
            Duration::from_secs(5),
        ] {
            let options = GifOptions {
                start: Duration::from_secs(5),
                end: Some(end),
                ..GifOptions::default()
            };
            assert!(options.validate().is_err());
            assert!(options.range(source).is_err());
        }
        for start in [source, Duration::from_secs(11)] {
            assert!(GifOptions {
                start,
                ..GifOptions::default()
            }
            .range(source)
            .is_err());
        }
        let beyond = GifOptions {
            start: Duration::from_secs(5),
            end: Some(Duration::from_secs(11)),
            ..GifOptions::default()
        };
        assert!(beyond.validate().is_ok());
        assert!(beyond.range(source).is_err());
        let invalid_rate = GifOptions {
            fps: 0,
            ..GifOptions::default()
        };
        assert!(invalid_rate.range(source).is_err());
    }

    #[test]
    fn explicit_range_keeps_input_and_allows_the_source_end() {
        let options = GifOptions {
            start: Duration::from_nanos(1),
            end: Some(Duration::from_secs(10)),
            fps: 1,
            max_side: 128,
            colors: 64,
        };
        let before = options;
        assert_eq!(
            options.range(Duration::from_secs(10)).unwrap(),
            (options.start, options.end.unwrap())
        );
        assert_eq!(options, before);
        assert!(options.range(Duration::from_secs(9)).is_err());
        assert_eq!(options, before);
    }

    #[test]
    fn duration_max_and_nanosecond_ranges_need_no_arithmetic_or_clamping() {
        let start = Duration::MAX - Duration::from_nanos(1);
        for end in [None, Some(Duration::MAX)] {
            let options = GifOptions {
                start,
                end,
                ..GifOptions::default()
            };
            assert!(options.validate().is_ok());
            assert_eq!(
                options.range(Duration::MAX).unwrap(),
                (start, Duration::MAX)
            );
            assert!(options.range(start).is_err());
        }
        assert!(GifOptions {
            start: Duration::MAX,
            ..GifOptions::default()
        }
        .range(Duration::MAX)
        .is_err());
        assert_eq!(
            GifOptions::default()
                .range(Duration::from_nanos(1))
                .unwrap(),
            (Duration::ZERO, Duration::from_nanos(1))
        );
    }
}
