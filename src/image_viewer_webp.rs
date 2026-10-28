//! WebP完整帧沿用唯一图片worker的呈现、旋转及首帧确认时钟。
use super::{
    super::{
        webp_playback::WebpPlayback,
        webp_source::{WebpFrame, WebpOptions, WebpSource},
    },
    ActiveFrames, LoadedImage, ViewerFrame,
};
use crate::{capture::Image, image_export_metadata::SourceMetadata};
use image::ImageFormat;
use std::{
    fs::File,
    path::Path,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

pub(super) type ActiveWebpAnimation<R = File> = ActiveFrames<WebpSource<R>>;

impl ViewerFrame for WebpFrame {
    fn image(&self) -> &Image {
        WebpFrame::image(self)
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn raw_delay_label(&self) -> String {
        trf!("{} 毫秒", "{} ms", self.timing.raw_delay_ms)
    }

    fn metadata_source(&self) -> Option<&Arc<SourceMetadata>> {
        Some(WebpFrame::metadata_source(self))
    }

    fn warning(&self) -> Option<&str> {
        WebpFrame::warning(self)
    }
}

impl ActiveFrames<WebpSource<File>> {
    pub(super) fn open(
        path: &Path,
        resident_bytes: u64,
        cancel: Arc<AtomicBool>,
        now: Duration,
    ) -> Result<(Self, LoadedImage), String> {
        let options = WebpOptions {
            max_total_bytes: super::animation_budget(resident_bytes, &cancel)?,
            ..WebpOptions::default()
        };
        let playback =
            WebpPlayback::open(path, options, cancel, true).map_err(|error| error.to_string())?;
        let (mut animation, image) = Self::initialize_format(
            playback,
            std::fs::metadata(path).ok().map(|metadata| metadata.len()),
            now,
            ImageFormat::WebP,
        )?;
        animation.await_presentation(now)?;
        Ok((animation, image))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rotation;
    use std::io::Cursor;

    fn animation_bytes() -> Vec<u8> {
        fn chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            if payload.len() & 1 != 0 {
                bytes.push(0);
            }
        }
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        chunk(&mut bytes, b"VP8X", &[0x12, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        chunk(&mut bytes, b"ANIM", &[0, 0, 0, 0, 1, 0]);
        for rgba in [
            [255, 0, 0, 255, 0, 0, 255, 128],
            [23, 61, 107, 80, 19, 83, 149, 255],
        ] {
            let mut frame = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut frame)
                .encode(&rgba, 2, 1, image::ExtendedColorType::Rgba8)
                .unwrap();
            assert_eq!(&frame[12..16], b"VP8L");
            // 全画布、1ms、Source；每帧保留编码器生成的完整静态像素块。
            let mut payload = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 2];
            payload.extend_from_slice(&frame[12..]);
            chunk(&mut bytes, b"ANMF", &payload);
        }
        let length = bytes.len() as u32 - 8;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    #[test]
    fn image_viewer_webp_presentation_rotation_and_snapshot_keep_selected_frame_facts() {
        use super::super::super::{
            presentation_resident_bytes, source_snapshot, SnapshotRequest, SnapshotTarget,
        };
        let encoded = animation_bytes();
        let playback = WebpPlayback::from_reader(
            Cursor::new(encoded.clone()),
            WebpOptions::default(),
            Arc::new(AtomicBool::new(false)),
            true,
        )
        .unwrap();
        let (mut animation, first) = ActiveFrames::initialize_format(
            playback,
            Some(encoded.len() as u64),
            Duration::ZERO,
            ImageFormat::WebP,
        )
        .unwrap();
        animation.await_presentation(Duration::ZERO).unwrap();
        assert_eq!(first.metadata.format, ImageFormat::WebP);
        let source = first.metadata_source.as_ref().unwrap();
        assert_eq!(first.metadata.details, Some(source.facts()));
        assert_eq!(
            source.facts().color_conversion,
            crate::image_color_management::ColorConversion::AssumedSrgb
        );
        let now = Duration::from_millis(100);
        assert!(animation.tick(now, false).image.is_none());
        let rotated = animation
            .control(super::super::Action::Rotate(Rotation::R90), now)
            .image
            .unwrap();
        assert_eq!(
            (rotated.tiles[0].0.width, rotated.tiles[0].0.height),
            (1, 2)
        );
        assert!(Arc::ptr_eq(
            first.raw_frame.as_ref().unwrap(),
            rotated.raw_frame.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            source,
            rotated.metadata_source.as_ref().unwrap()
        ));
        animation.begin(now, 0);
        assert!(animation
            .tick(Duration::from_millis(109), false)
            .image
            .is_none());
        let second = animation
            .tick(Duration::from_millis(110), false)
            .image
            .unwrap();
        let raw = second.raw_frame.as_ref().unwrap();
        assert_eq!(raw.index(), 1);
        assert!(raw.raw_delay_label().contains('1'));
        assert_eq!(raw.timeout(), Some(Duration::from_millis(10)));
        assert_eq!(raw.image().bgra, [107, 61, 23, 80, 149, 83, 19, 255]);
        assert!(Arc::ptr_eq(
            second.metadata_source.as_ref().unwrap(),
            raw.metadata_source().unwrap()
        ));
        let request = SnapshotRequest {
            serial: 1,
            path: "absent-webp-frames-source.webp".into(),
            frame: Some(raw.clone()),
            raw_page: None,
            metadata_source: second.metadata_source.clone(),
            resident_bytes: presentation_resident_bytes(
                &second,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target: SnapshotTarget::Tools,
        };
        drop(animation);
        let snapshot = source_snapshot(&request).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (2, 1));
        assert_eq!(snapshot.bgra, raw.image().bgra);
        assert_ne!(snapshot.bgra.as_ptr(), raw.image().bgra.as_ptr());
    }
}
