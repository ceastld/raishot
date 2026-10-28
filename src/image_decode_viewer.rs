//! 查看器专用选择：同次冻结输入进入完整解码或明确标记的采样预览。
use super::*;
use crate::image_preview::{
    png::{PreviewDecoded, PreviewLimits},
    source::FrozenPreviewSource,
};

pub(crate) enum ViewerDecoded {
    Full(DecodedImage),
    Preview {
        decoded: PreviewDecoded,
        encoded_bytes: u64,
        source: Arc<FrozenPreviewSource>,
    },
}

pub(crate) fn decode_path(
    path: &Path,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ViewerDecoded> {
    let result = (|| {
        let bytes = input::read_path_input_with_resident(
            path,
            cancel,
            external_resident_bytes,
            MAX_DECODE_BYTES,
        )?;
        decode_owned(bytes, max_output_bytes, external_resident_bytes, cancel)
    })();
    check_cancel(cancel)?;
    result
}

fn decode_owned(
    bytes: Vec<u8>,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ViewerDecoded> {
    check_cancel(cancel)?;
    let result = (|| {
        if oversized_png(&bytes) {
            let encoded_bytes = bytes.len() as u64;
            let mut limits = PreviewLimits::default();
            limits.max_pixels = limits.max_pixels.min(max_output_bytes / 4);
            let (source, decoded) =
                FrozenPreviewSource::open(bytes, external_resident_bytes, limits, cancel)?;
            Ok(ViewerDecoded::Preview {
                decoded,
                encoded_bytes,
                source,
            })
        } else {
            decode_bytes_with_foreground_budget_cancellable(
                &bytes,
                bytes.capacity() as u64,
                max_output_bytes,
                external_resident_bytes,
                cancel,
            )
            .map(ViewerDecoded::Full)
        }
    })();
    check_cancel(cancel)?;
    result
}

// 这里只选择能力，不代替PNG验证；非法头部、CRC、交错或动画由专用预检拒绝。
// 普通尺寸仍沿原Foreground路径，不用错误文案判定失败后再打开文件。
fn oversized_png(bytes: &[u8]) -> bool {
    if bytes.get(..16) != Some(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice()) {
        return false;
    }
    let Some(size) = bytes.get(16..24) else {
        return false;
    };
    let width = u32::from_be_bytes(size[..4].try_into().unwrap());
    let height = u32::from_be_bytes(size[4..].try_into().unwrap());
    width > crate::export::MAX_DECODE_SIDE
        || height > crate::export::MAX_DECODE_SIDE
        || u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png(height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, height);
            encoder.set_color(png::ColorType::Rgba);
            let mut writer = encoder.write_header().unwrap();
            {
                let mut stream = writer.stream_writer().unwrap();
                for _ in 0..height {
                    stream.write_all(&[42, 77, 88, 128]).unwrap();
                }
                stream.finish().unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn image_decode_viewer_routes_frozen_source_without_changing_full_decode() {
        let cancel = AtomicBool::new(false);
        for height in [2, 65_536] {
            let bytes = png(height);
            let original_pointer = bytes.as_ptr();
            let original_length = bytes.len() as u64;
            let original_capacity = bytes.capacity() as u64;
            let result = decode_owned(bytes, MAX_DECODE_BYTES, 0, &cancel).unwrap();
            match result {
                ViewerDecoded::Full(decoded) => {
                    assert_eq!(height, 2);
                    assert_eq!((decoded.image.width, decoded.image.height), (1, 2));
                    assert_eq!(&decoded.image.bgra[..4], &[88, 77, 42, 128]);
                }
                ViewerDecoded::Preview {
                    decoded,
                    encoded_bytes,
                    source,
                } => {
                    assert_eq!(height, 65_536);
                    assert_eq!(encoded_bytes, original_length);
                    assert_eq!(source.input().as_ptr(), original_pointer);
                    assert_eq!(source.input_reserved_bytes(), original_capacity);
                    assert_eq!(source.geometry(), decoded.geometry);
                    assert_eq!(source.source(), decoded.source);
                    assert_eq!(Arc::strong_count(&source), 1);
                    assert_eq!(decoded.geometry.encoded_dimensions(), (1, 65_536));
                    assert_eq!((decoded.image.width, decoded.image.height), (1, 2048));
                    assert!(decoded
                        .image
                        .bgra
                        .chunks_exact(4)
                        .all(|p| p == [88, 77, 42, 128]));
                }
            }
        }
    }

    #[test]
    fn image_decode_viewer_preview_budget_and_cancel_do_not_fall_back_to_full() {
        let bytes = png(65_536);
        assert!(oversized_png(&bytes));
        assert!(decode_owned(bytes, 3, 0, &AtomicBool::new(false)).is_err());
        assert!(decode_owned(
            png(65_536),
            MAX_DECODE_BYTES,
            MAX_DECODE_BYTES,
            &AtomicBool::new(false)
        )
        .is_err());
        let error = decode_owned(png(65_536), MAX_DECODE_BYTES, 0, &AtomicBool::new(true))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("取消") || error.contains("canceled"));
    }
}
