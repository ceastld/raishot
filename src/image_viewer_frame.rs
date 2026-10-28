//! 查看器共享完整动画帧；类型擦除只共享同一Arc，不复制像素或拆开交付租约。
use crate::{
    capture::Image, image_animation::GifFrame, image_animation_playback::PlaybackFrame,
    image_apng::ApngFrame, image_export_metadata::SourceMetadata,
};
use std::sync::Arc;

pub(super) trait ViewerFrame: PlaybackFrame + Send + Sync {
    fn image(&self) -> &Image;
    fn generation(&self) -> u64;
    fn raw_delay_label(&self) -> String;
    fn metadata_source(&self) -> Option<&Arc<SourceMetadata>> {
        None
    }
    fn warning(&self) -> Option<&str> {
        None
    }
}

pub(super) type FrameRef = Arc<dyn ViewerFrame>;

impl ViewerFrame for GifFrame {
    fn image(&self) -> &Image {
        GifFrame::image(self)
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn raw_delay_label(&self) -> String {
        trf!("{} 百分秒", "{} cs", self.timing.raw_delay_centis)
    }
}

impl ViewerFrame for ApngFrame {
    fn metadata_source(&self) -> Option<&Arc<SourceMetadata>> {
        Some(ApngFrame::metadata_source(self))
    }
    fn warning(&self) -> Option<&str> {
        ApngFrame::warning(self)
    }
    fn image(&self) -> &Image {
        ApngFrame::image(self)
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn raw_delay_label(&self) -> String {
        if self.timing.denominator == 0 {
            trf!(
                "{}/0 秒（分母100）",
                "{}/0 s (denom=100)",
                self.timing.numerator
            )
        } else {
            trf!(
                "{}/{} 秒",
                "{}/{} s",
                self.timing.numerator,
                self.timing.denominator
            )
        }
    }
}
