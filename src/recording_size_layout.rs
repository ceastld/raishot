//! 录屏物理区域、裁偶数边与 BGRA 行布局。只调用既有尺寸规则，不改变选区或标注原点。
use crate::{
    geometry::ScreenRect,
    recording_options::{encoded_size, validate_region},
    Result,
};
use serde::Serialize;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RecordingSizeLayout {
    pub left: i32,
    pub top: i32,
    pub source_width: u32,
    pub source_height: u32,
    pub encoded_width: u32,
    pub encoded_height: u32,
    pub source_stride: usize,
    pub encoded_stride: usize,
    pub source_bytes: usize,
    pub encoded_bytes: usize,
}

impl RecordingSizeLayout {
    pub(crate) fn new(region: ScreenRect) -> Result<Self> {
        validate_region(region.width, region.height)?;
        if i64::from(region.left) + i64::from(region.width) > i64::from(i32::MAX)
            || i64::from(region.top) + i64::from(region.height) > i64::from(i32::MAX)
        {
            return Err(tr!(
                "录屏区域超出物理坐标范围",
                "Recording area exceeds the physical coordinate range"
            )
            .into());
        }
        let (encoded_width, encoded_height) = encoded_size(region.width, region.height);
        let source_stride = usize::try_from(region.width)
            .ok()
            .and_then(|width| width.checked_mul(4));
        let encoded_stride = usize::try_from(encoded_width)
            .ok()
            .and_then(|width| width.checked_mul(4));
        let source_bytes =
            source_stride.and_then(|stride| stride.checked_mul(region.height as usize));
        let encoded_bytes =
            encoded_stride.and_then(|stride| stride.checked_mul(encoded_height as usize));
        let (Some(source_stride), Some(encoded_stride), Some(source_bytes), Some(encoded_bytes)) =
            (source_stride, encoded_stride, source_bytes, encoded_bytes)
        else {
            return Err(tr!("录屏帧布局过大", "Recording frame layout is too large").into());
        };
        Ok(Self {
            left: region.left,
            top: region.top,
            source_width: region.width,
            source_height: region.height,
            encoded_width,
            encoded_height,
            source_stride,
            encoded_stride,
            source_bytes,
            encoded_bytes,
        })
    }

    pub(crate) fn cropped_right(&self) -> u32 {
        self.source_width - self.encoded_width
    }

    pub(crate) fn cropped_bottom(&self) -> u32 {
        self.source_height - self.encoded_height
    }

    pub(crate) fn encoded_region(&self) -> ScreenRect {
        ScreenRect {
            left: self.left,
            top: self.top,
            width: self.encoded_width,
            height: self.encoded_height,
        }
    }

    /// 输入是区域内 top-down BGRA；不接收整屏尺寸冒充区域尺寸或缺失底行的数据。
    pub(crate) fn validate_frame(&self, width: u32, height: u32, bytes: usize) -> Result<()> {
        if (width, height) != (self.source_width, self.source_height) || bytes < self.source_bytes {
            return Err(tr!(
                "录屏帧与采集区域不一致",
                "Recording frame does not match the capture area"
            )
            .into());
        }
        Ok(())
    }

    /// 源缓冲中第 row 行的编码范围；额外右列与底行不进入 H.264，行序和物理原点不变。
    pub(crate) fn source_row(&self, row: u32) -> Option<Range<usize>> {
        if row >= self.encoded_height {
            return None;
        }
        let start = row as usize * self.source_stride;
        Some(start..start + self.encoded_stride)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_size_layout_odd_edges_preserve_negative_origin_and_rows() {
        let layout = RecordingSizeLayout::new(ScreenRect {
            left: -1920,
            top: -257,
            width: 385,
            height: 257,
        })
        .unwrap();
        assert_eq!((layout.cropped_right(), layout.cropped_bottom()), (1, 1));
        assert_eq!(
            layout.encoded_region(),
            ScreenRect {
                left: -1920,
                top: -257,
                width: 384,
                height: 256
            }
        );
        assert_eq!(layout.source_row(0), Some(0..384 * 4));
        assert_eq!(
            layout.source_row(255),
            Some(255 * 385 * 4..255 * 385 * 4 + 384 * 4)
        );
        assert_eq!(layout.source_row(256), None);
        assert!(layout.validate_frame(385, 257, layout.source_bytes).is_ok());
        assert!(layout
            .validate_frame(384, 256, layout.encoded_bytes)
            .is_err());
        assert!(layout
            .validate_frame(385, 257, layout.source_bytes - 1)
            .is_err());
    }

    #[test]
    fn recording_size_layout_coordinate_and_dimension_boundaries() {
        assert!(RecordingSizeLayout::new(ScreenRect {
            left: i32::MAX - 1,
            top: 0,
            width: 2,
            height: 2
        })
        .is_err());
        assert!(RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: i32::MAX - 1,
            width: 2,
            height: 2
        })
        .is_err());
        assert!(RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: 1,
            height: 2
        })
        .is_err());
        assert!(RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: 7681,
            height: 2
        })
        .is_err());
        let layout = RecordingSizeLayout::new(ScreenRect {
            left: i32::MIN,
            top: 0,
            width: 2,
            height: 2,
        })
        .unwrap();
        assert_eq!(layout.encoded_bytes, 16);
        assert_eq!((layout.cropped_right(), layout.cropped_bottom()), (0, 0));
    }
}
