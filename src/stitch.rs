//! 双向接缝拼接接口；实现见 stitch_stream，替代旧的累计整图指纹定位。
use crate::{capture::Image, geometry::Rect};

use std::{fmt, sync::Arc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollAxis {
    #[default]
    Vertical,
    Horizontal,
}
impl ScrollAxis {
    pub(crate) fn size(self, image: &Image) -> (u32, u32) {
        match self {
            Self::Vertical => (image.width, image.height),
            Self::Horizontal => (image.height, image.width),
        }
    }
    pub(crate) fn rect(self, start: u32, length: u32, cross: u32) -> Rect {
        match self {
            Self::Vertical => Rect {
                x: 0,
                y: start,
                width: cross,
                height: length,
            },
            Self::Horizontal => Rect {
                x: start,
                y: 0,
                width: length,
                height: cross,
            },
        }
    }
    pub(crate) fn origin(self, start: u32) -> (u32, u32) {
        match self {
            Self::Vertical => (0, start),
            Self::Horizontal => (start, 0),
        }
    }
    pub(crate) fn dimensions(self, cross: u32, length: u32) -> (u32, u32) {
        match self {
            Self::Vertical => (cross, length),
            Self::Horizontal => (length, cross),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct StitchOptions {
    pub axis: ScrollAxis,
    pub detect_fixed_borders: bool,
    /// Some(0) 明确保留该边；None 可由自动固定栏检测提供辅助匹配范围。
    pub fixed_start: Option<u32>,
    pub fixed_end: Option<u32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    Unchanged,
    ViewportOnly,
    Added,
    Rejected,
    Limit,
}
#[derive(Clone, Copy, Debug)]
pub struct StitchMatchQuality {
    /// 本帧最终对齐候选的综合误差，越低越好；不是概率。
    pub error: f64,
    /// 所选对齐范围中通过逐行差异检查的比例。
    pub matched_ratio: f64,
    /// 本帧有视觉上唯一的位移证据，而非仅靠运动预测消歧。
    pub unique: bool,
    /// 共有正文占可匹配正文的比例，不把固定栏算入正文长度。
    pub overlap_ratio: f64,
}
#[derive(Clone, Debug)]
pub struct StitchUpdate {
    pub kind: UpdateKind,
    pub delta: i32,
    /// 当前完整采集帧在结果中的投影（预览里的绿框）。
    pub viewport: Rect,
    pub added: u32,
    pub reason: Option<String>,
    /// 精确重复帧跳过匹配，故没有本帧质量证据。
    pub quality: Option<StitchMatchQuality>,
}
#[derive(Clone)]
pub struct StitchSegment {
    pub image: Arc<Image>,
    pub source: Rect,
    pub origin: (u32, u32),
}
impl fmt::Debug for StitchSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StitchSegment")
            .field("image_size", &(self.image.width, self.image.height))
            .field("source", &self.source)
            .field("origin", &self.origin)
            .finish()
    }
}
#[derive(Clone, Debug)]
pub struct StitchSnapshot {
    pub width: u32,
    pub height: u32,
    pub axis: ScrollAxis,
    pub viewport: Rect,
    pub segments: Vec<StitchSegment>,
}

#[path = "stitch_stream.rs"]
mod stream;
pub use stream::Stitcher;
#[cfg(test)]
#[path = "stitch_regression_tests.rs"]
mod regression_tests;
#[cfg(test)]
#[path = "stitch_shared_tests.rs"]
mod shared_contract_tests;
#[cfg(test)]
#[path = "stitch_stream_tests.rs"]
mod stream_contract_tests;
