//! 不可变编码产物：预览读取实际编码，保存复用相同字节，不重新编码处理帧。
use crate::{
    capture::Image,
    image_export_metadata::{self, MetadataPolicy, SourceMetadata},
    image_tools::{
        check_cancel, require_committed, require_request, validate_input,
        validate_save_destination, ProcessedImage, SavedImage, ToolError, ToolLimits, ToolResult,
    },
    save_options::SaveEncoding,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const MAX_ENCODED_BYTES: usize = 128 * 1024 * 1024;

/// 仅 prepare 系列可创建；不持有处理帧，也不暴露可变字节或接受任意外来编码。
pub(crate) struct EncodedImage {
    bytes: Vec<u8>,
    request_id: u64,
    encoding: SaveEncoding,
    width: u32,
    height: u32,
}

pub(crate) fn prepare(
    processed: &ProcessedImage,
    encoding: SaveEncoding,
    cancel: &AtomicBool,
) -> ToolResult<EncodedImage> {
    prepare_with_metadata(processed, encoding, MetadataPolicy::Strip, None, cancel)
}

/// 元数据在精确大小和不可变字节封存前处理；预览和保存都使用这一次完整编码。
pub(crate) fn prepare_with_metadata(
    processed: &ProcessedImage,
    encoding: SaveEncoding,
    policy: MetadataPolicy,
    source: Option<&SourceMetadata>,
    cancel: &AtomicBool,
) -> ToolResult<EncodedImage> {
    check_cancel(cancel)?;
    validate_input(processed.image(), ToolLimits::default())?;
    encoding
        .validate()
        .map_err(|error| ToolError::Invalid(error.to_string()))?;
    // 有损WebP接同一取消标记；旧编码器返回后仍核对取消和成品上限。
    let encoded = crate::export::encode_image_cancellable(processed.image(), encoding, cancel);
    check_cancel(cancel)?;
    let encoded = encoded.map_err(|error| ToolError::Failed(error.to_string()))?;
    validate_encoded_size(encoded.len())?;
    let dimensions = (processed.image().width, processed.image().height);
    let bytes =
        image_export_metadata::apply(encoded, encoding.format, dimensions, policy, source, cancel);
    // 模块错误使用String；取消必须保留工具层的独立终态，不作为普通失败显示。
    check_cancel(cancel)?;
    let bytes = bytes.map_err(ToolError::Failed)?;
    validate_encoded_size(bytes.len())?;
    Ok(EncodedImage {
        bytes,
        request_id: processed.request_id(),
        encoding,
        width: dimensions.0,
        height: dimensions.1,
    })
}

fn validate_encoded_size(bytes: usize) -> ToolResult<()> {
    if bytes > MAX_ENCODED_BYTES {
        return Err(ToolError::Failed(
            tr!(
                "编码图片超过 128 MiB 上限",
                "Encoded image exceeds the 128 MiB limit"
            )
            .into(),
        ));
    }
    Ok(())
}

impl EncodedImage {
    pub(crate) fn byte_len(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.bytes.capacity() as u64
    }
    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }
    pub(crate) fn encoding(&self) -> SaveEncoding {
        self.encoding
    }
    pub(crate) fn width(&self) -> u32 {
        self.width
    }
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// 解码本产物的实际字节供预览使用；只读内存，不打开来源文件。
    /// 产物中的 EXIF 已按当前像素重建，不重新套用源文件的方向。
    pub(crate) fn decode(&self, cancel: &AtomicBool) -> ToolResult<Image> {
        self.decode_with_resident(0, cancel)
    }

    /// 对照等宿主保留的源像素另计；编码Vec的实际容量仍由解码器单独计入。
    pub(crate) fn decode_with_resident(
        &self,
        external_resident_bytes: u64,
        cancel: &AtomicBool,
    ) -> ToolResult<Image> {
        check_cancel(cancel)?;
        let decoded = crate::image_decode::decode_bytes_with_foreground_budget_cancellable(
            &self.bytes,
            self.bytes.capacity() as u64,
            crate::image_decode::MAX_DECODE_BYTES,
            external_resident_bytes,
            cancel,
        );
        check_cancel(cancel)?;
        let decoded = decoded.map_err(|error| ToolError::Failed(error.to_string()))?;
        if (decoded.image.width, decoded.image.height) != (self.width, self.height) {
            return Err(ToolError::Failed(
                tr!(
                    "编码图片的回读尺寸不一致",
                    "Decoded image dimensions do not match the encoding"
                )
                .into(),
            ));
        }
        Ok(decoded.image)
    }

    /// 先核对代次，再验证格式、原片保护与目录。提交的始终是 prepare 的那一份字节。
    pub(crate) fn save(
        &self,
        expected_request_id: u64,
        path: &Path,
        protected_sources: &[PathBuf],
        cancel: &AtomicBool,
    ) -> ToolResult<SavedImage> {
        require_request(self.request_id, expected_request_id)?;
        validate_save_destination(path, self.encoding, protected_sources, cancel)?;
        check_cancel(cancel)?;
        crate::image_collection::ensure_local_directory(path.parent().expect("validated parent"))
            .map_err(ToolError::Invalid)?;
        require_committed(
            crate::export::save_encoded_cancellable(&self.bytes, path, false, cancel),
            cancel,
        )?;
        // 提交成功后不再查取消：文件已存在，成功必须保持为成功。
        Ok(SavedImage {
            path: path.to_path_buf(),
            width: self.width,
            height: self.height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save_options::SaveFormat;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rustshot-image-compress-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn small_png() -> ProcessedImage {
        ProcessedImage::new(
            Image {
                width: 2,
                height: 1,
                bgra: vec![200, 10, 20, 0, 30, 40, 50, 128],
                readback_ms: 0.,
            },
            42,
        )
    }

    #[test]
    fn image_compress_png_roundtrip_keeps_alpha_and_owns_only_encoded_output() {
        let processed = small_png();
        let encoded =
            prepare(&processed, SaveEncoding::default(), &AtomicBool::new(false)).unwrap();
        assert_eq!((encoded.width(), encoded.height()), (2, 1));
        assert_eq!(encoded.request_id(), 42);
        assert_eq!(encoded.encoding(), SaveEncoding::default());
        assert!(encoded.byte_len() > 8);
        drop(processed);
        let decoded = encoded.decode(&AtomicBool::new(false)).unwrap();
        assert_eq!(decoded.bgra, [200, 10, 20, 0, 30, 40, 50, 128]);
        assert!(matches!(
            encoded.decode(&AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn image_compress_color_metadata_seals_actual_bytes_for_decode_and_save() {
        let directory = TestDirectory::new();
        let cancel = AtomicBool::new(false);
        let processed = small_png();
        let before = processed.image().bgra.clone();
        let plain = prepare(&processed, SaveEncoding::default(), &cancel).unwrap();
        let source = crate::image_decode::decode_bytes(&plain.bytes)
            .unwrap()
            .metadata_source
            .expect("fresh PNG has a verified assumed-sRGB source");
        for (format, filename) in [
            (SaveFormat::Png, "color.png"),
            (SaveFormat::Jpeg, "color.jpg"),
            (SaveFormat::Webp, "color.webp"),
            (SaveFormat::Bmp, "color.bmp"),
        ] {
            let encoding = SaveEncoding {
                format,
                ..Default::default()
            };
            let default = prepare(&processed, encoding, &cancel).unwrap();
            let stripped =
                prepare_with_metadata(&processed, encoding, MetadataPolicy::Strip, None, &cancel)
                    .unwrap();
            assert_eq!(default.bytes, stripped.bytes);
            assert!(matches!(
                prepare_with_metadata(
                    &processed,
                    encoding,
                    MetadataPolicy::ColorOnly,
                    None,
                    &cancel,
                ),
                Err(ToolError::Failed(_))
            ));
            let colored = prepare_with_metadata(
                &processed,
                encoding,
                MetadataPolicy::ColorOnly,
                Some(source.as_ref()),
                &cancel,
            );
            if format == SaveFormat::Bmp {
                assert!(matches!(colored, Err(ToolError::Failed(_))));
                continue;
            }
            let colored = colored.unwrap();
            assert!(colored.byte_len() > stripped.byte_len());
            let decoded = colored.decode(&cancel).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 1));
            let alpha: Vec<_> = decoded.bgra.chunks_exact(4).map(|pixel| pixel[3]).collect();
            assert_eq!(
                alpha,
                if format == SaveFormat::Jpeg {
                    vec![255, 255]
                } else {
                    vec![0, 128]
                }
            );
            let destination = directory.0.join(filename);
            let saved = colored.save(42, &destination, &[], &cancel).unwrap();
            assert_eq!((saved.width, saved.height), (2, 1));
            assert_eq!(std::fs::read(destination).unwrap(), colored.bytes);
        }
        assert_eq!(processed.image().bgra, before);
        assert!(matches!(
            prepare_with_metadata(
                &processed,
                SaveEncoding::default(),
                MetadataPolicy::ColorOnly,
                None,
                &AtomicBool::new(true),
            ),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn image_compress_supported_exif_tracks_transformed_pixels_and_sealed_save_bytes() {
        use crate::{
            image_container_metadata::MetadataStatus,
            image_resize::ResizeFilter,
            image_transform::{ResizeOptions, TransformOptions, TransformPlan},
            pin_model::Adjustments,
        };
        use image::ImageDecoder;
        use std::io::Cursor;

        // 独立小TIFF：旧宽999、高777、Make="TestCam"、Orientation=6，无子目录。
        let source_tiff = b"II*\0\x08\0\0\0\x04\0\
            \x00\x01\x04\0\x01\0\0\0\xe7\x03\0\0\
            \x01\x01\x04\0\x01\0\0\0\x09\x03\0\0\
            \x0f\x01\x02\0\x08\0\0\0\x3e\0\0\0\
            \x12\x01\x03\0\x01\0\0\0\x06\0\0\0\
            \0\0\0\0TestCam\0"
            .to_vec();
        let source_tiff_before = source_tiff.clone();
        let directory = TestDirectory::new();
        let cancel = AtomicBool::new(false);
        let original = small_png();
        let pixels_before = original.image().bgra.clone();
        let plain = prepare(&original, SaveEncoding::default(), &cancel).unwrap();
        let mut facts = crate::image_decode::decode_bytes(&plain.bytes)
            .unwrap()
            .metadata_source
            .unwrap()
            .facts();
        facts.source.exif.status = MetadataStatus::Present;
        facts.source.exif.encoded_bytes = source_tiff.len() as u64;
        facts.source.exif.decoded_bytes = Some(source_tiff.len() as u64);
        facts.source.orientation = Some(image::metadata::Orientation::from_exif(6).unwrap());
        let source = SourceMetadata::new(facts, None)
            .unwrap()
            .with_exif(crate::image_exif::inspect(&source_tiff, &cancel));
        assert!(source.exif_error().is_none());
        assert_eq!(source.exif_summary().unwrap().retained_fields, 1);
        let summary_before = source.exif_summary();
        let retained_before = source.retained_bytes();
        let transformed = TransformPlan::new(
            original.image(),
            TransformOptions {
                adjustments: Adjustments::default().rotated(1),
                resize: Some(ResizeOptions {
                    width: 5,
                    height: 4,
                    filter: ResizeFilter::Nearest,
                }),
            },
            ToolLimits::default(),
            87,
        )
        .unwrap()
        .render(original.image(), &cancel)
        .unwrap();
        assert_eq!(
            (transformed.image().width, transformed.image().height),
            (5, 4)
        );
        for (format, filename) in [
            (SaveFormat::Png, "exif.png"),
            (SaveFormat::Jpeg, "exif.jpg"),
            (SaveFormat::Webp, "exif.webp"),
        ] {
            let encoding = SaveEncoding {
                format,
                ..Default::default()
            };
            let encoded = prepare_with_metadata(
                &transformed,
                encoding,
                MetadataPolicy::ColorAndSupportedExif,
                Some(&source),
                &cancel,
            )
            .unwrap();
            let decoded = encoded.decode(&cancel).unwrap();
            assert_eq!((decoded.width, decoded.height), (5, 4));
            let inspected = crate::image_container_metadata::inspect(&encoded.bytes).unwrap();
            assert_eq!(inspected.summary.exif.status, MetadataStatus::Present);
            assert_eq!(inspected.summary.orientation.unwrap().to_exif(), 1);
            assert_eq!(
                inspected
                    .supported_exif
                    .unwrap()
                    .unwrap()
                    .summary()
                    .retained_fields,
                1
            );
            // 容器提取交给既有解码器；只核对规范化输出的值，不另写块/偏移解析器。
            let mut decoder = image::ImageReader::new(Cursor::new(encoded.bytes.as_slice()))
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap();
            let raw = decoder.exif_metadata().unwrap().expect("encoded EXIF");
            let tiff = raw.strip_prefix(b"Exif\0\0").unwrap_or(raw.as_slice());
            assert!(tiff.windows(8).any(|value| value == b"TestCam\0"));
            for (tag, value) in [(0xa002_u16, 5_u32), (0xa003, 4)] {
                let mut field = [0_u8; 12];
                field[..2].copy_from_slice(&tag.to_le_bytes());
                field[2..4].copy_from_slice(&4_u16.to_le_bytes());
                field[4..8].copy_from_slice(&1_u32.to_le_bytes());
                field[8..].copy_from_slice(&value.to_le_bytes());
                assert!(tiff.windows(12).any(|value| value == field.as_slice()));
            }
            let path = directory.0.join(filename);
            let saved = encoded.save(87, &path, &[], &cancel).unwrap();
            assert_eq!((saved.width, saved.height), (5, 4));
            assert_eq!(std::fs::read(path).unwrap(), encoded.bytes);
            for policy in [MetadataPolicy::Strip, MetadataPolicy::ColorOnly] {
                let compatible =
                    prepare_with_metadata(&transformed, encoding, policy, Some(&source), &cancel)
                        .unwrap();
                let inspected =
                    crate::image_container_metadata::inspect(&compatible.bytes).unwrap();
                assert_eq!(inspected.summary.exif.status, MetadataStatus::Absent);
                if policy == MetadataPolicy::ColorOnly {
                    assert_eq!(compatible.decode(&cancel).unwrap().bgra, decoded.bgra);
                }
            }
        }
        assert_eq!(original.image().bgra, pixels_before);
        assert_eq!(source_tiff, source_tiff_before);
        assert_eq!(source.facts(), facts);
        assert_eq!(source.exif_summary(), summary_before);
        assert_eq!(source.retained_bytes(), retained_before);
        assert!(source.exif_error().is_none());
    }

    #[test]
    fn image_compress_jpeg_preview_decodes_actual_quality_dependent_pixels() {
        let mut bgra = Vec::new();
        for y in 0..24_u32 {
            for x in 0..32_u32 {
                bgra.extend_from_slice(&[
                    (x * 73 + y * 19) as u8,
                    (x * 11 + y * 97) as u8,
                    (x * 53 + y * 41) as u8,
                    255,
                ]);
            }
        }
        let processed = ProcessedImage::new(
            Image {
                width: 32,
                height: 24,
                bgra,
                readback_ms: 0.,
            },
            51,
        );
        let cancel = AtomicBool::new(false);
        let low = prepare(
            &processed,
            SaveEncoding {
                format: SaveFormat::Jpeg,
                jpeg_quality: 10,
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let high = prepare(
            &processed,
            SaveEncoding {
                format: SaveFormat::Jpeg,
                jpeg_quality: 95,
                ..Default::default()
            },
            &cancel,
        )
        .unwrap();
        let low_pixels = low.decode(&cancel).unwrap();
        let high_pixels = high.decode(&cancel).unwrap();
        assert_eq!((low_pixels.width, low_pixels.height), (32, 24));
        assert!(low_pixels.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert_ne!(low_pixels.bgra, processed.image().bgra);
        assert_ne!(low_pixels.bgra, high_pixels.bgra);
        assert!(low.byte_len() < high.byte_len());
        assert_eq!(processed.request_id(), 51);
    }

    #[test]
    fn image_compress_decode_with_resident_counts_frozen_source_and_cancel() {
        let processed = small_png();
        let source = processed.image().bgra.clone();
        let cancel = AtomicBool::new(false);
        let encoded = prepare(&processed, SaveEncoding::default(), &cancel).unwrap();
        let decoded = encoded
            .decode_with_resident(source.capacity() as u64, &cancel)
            .unwrap();
        assert_eq!(decoded.bgra, source);
        assert!(matches!(
            encoded.decode_with_resident(u64::MAX, &cancel),
            Err(ToolError::Failed(_))
        ));
        assert!(matches!(
            encoded.decode_with_resident(u64::MAX, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(processed.image().bgra, source);
        assert_eq!(encoded.request_id(), processed.request_id());
    }

    #[test]
    fn image_compress_webp_modes_seal_real_preview_and_saved_bytes() {
        use crate::image_webp_encode::{WebpEncoding, WebpMode};

        let directory = TestDirectory::new();
        let mut bgra = Vec::new();
        for y in 0..24_u32 {
            for x in 0..32_u32 {
                bgra.extend_from_slice(&[
                    (x * 73 + y * 19) as u8,
                    (x * 11 + y * 97) as u8,
                    (x * 53 + y * 41) as u8,
                    [0, 64, 128, 255][((x + y) % 4) as usize],
                ]);
            }
        }
        let processed = ProcessedImage::new(
            Image {
                width: 32,
                height: 24,
                bgra,
                readback_ms: 0.,
            },
            52,
        );
        let original = processed.image().bgra.clone();
        let cancel = AtomicBool::new(false);
        let mut lossy_outputs = Vec::new();

        for (mode, quality, filename) in [
            (WebpMode::Lossless, 90, "lossless.webp"),
            (WebpMode::Lossy, 10, "lossy-10.webp"),
            (WebpMode::Lossy, 95, "lossy-95.webp"),
        ] {
            let encoding = SaveEncoding {
                format: SaveFormat::Webp,
                webp: WebpEncoding { mode, quality },
                jpeg_quality: 1,
                ..Default::default()
            };
            let encoded = prepare(&processed, encoding, &cancel).unwrap();
            assert_eq!(encoded.encoding(), encoding);
            assert_eq!(encoded.request_id(), 52);
            let decoded = encoded.decode(&cancel).unwrap();
            assert_eq!((decoded.width, decoded.height), (32, 24));
            for (source, preview) in original.chunks_exact(4).zip(decoded.bgra.chunks_exact(4)) {
                assert_eq!(preview[3], source[3]);
                if mode == WebpMode::Lossless && source[3] != 0 {
                    assert_eq!(&preview[..3], &source[..3]);
                }
            }

            let independent = prepare(
                &processed,
                SaveEncoding {
                    jpeg_quality: 100,
                    ..encoding
                },
                &cancel,
            )
            .unwrap();
            assert_eq!(encoded.bytes, independent.bytes);

            let destination = directory.0.join(filename);
            let saved = encoded.save(52, &destination, &[], &cancel).unwrap();
            assert_eq!((saved.width, saved.height), (32, 24));
            assert_eq!(std::fs::read(&saved.path).unwrap(), encoded.bytes);
            let reopened = crate::export::read_image(&saved.path).unwrap();
            assert_eq!(reopened.bgra, decoded.bgra);

            // 批量、分割和拼接共用此直接保存入口，也必须保留相同的WebP参数。
            let direct = processed
                .save(
                    52,
                    &directory.0.join(format!("direct-{filename}")),
                    encoding,
                    &[],
                    &cancel,
                )
                .unwrap();
            assert_eq!(std::fs::read(&direct.path).unwrap(), encoded.bytes);
            assert_eq!(
                crate::export::read_image(&direct.path).unwrap().bgra,
                decoded.bgra
            );

            if mode == WebpMode::Lossy {
                lossy_outputs.push((encoded.bytes, decoded.bgra));
            }
            assert_eq!(processed.image().bgra, original);
        }

        assert_eq!(lossy_outputs.len(), 2);
        assert_ne!(lossy_outputs[0].0, lossy_outputs[1].0);
        assert!(lossy_outputs[0]
            .1
            .chunks_exact(4)
            .zip(lossy_outputs[1].1.chunks_exact(4))
            .any(|(low, high)| low[3] != 0 && low[..3] != high[..3]));

        for quality in [0, 101] {
            assert!(prepare(
                &processed,
                SaveEncoding {
                    format: SaveFormat::Webp,
                    webp: WebpEncoding {
                        mode: WebpMode::Lossy,
                        quality,
                    },
                    ..Default::default()
                },
                &cancel,
            )
            .is_err());
        }
        assert_eq!(processed.image().bgra, original);
        assert_eq!(processed.request_id(), 52);
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 6);
    }

    #[test]
    fn image_compress_prepare_rejects_bad_pixels_quality_and_cancel() {
        let malformed = ProcessedImage::new(
            Image {
                width: 2,
                height: 1,
                bgra: vec![0; 7],
                readback_ms: 0.,
            },
            1,
        );
        assert!(prepare(&malformed, SaveEncoding::default(), &AtomicBool::new(false)).is_err());
        assert!(prepare(
            &small_png(),
            SaveEncoding {
                jpeg_quality: 0,
                ..Default::default()
            },
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(matches!(
            prepare(
                &small_png(),
                SaveEncoding::default(),
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn image_compress_save_writes_exact_encoding_and_rejects_stale_wrong_format_and_overwrite() {
        let directory = TestDirectory::new();
        let cancel = AtomicBool::new(false);
        let encoded = prepare(&small_png(), SaveEncoding::default(), &cancel).unwrap();
        let destination = directory.0.join("result.png");
        assert!(matches!(
            encoded.save(43, &destination, &[], &AtomicBool::new(true)),
            Err(ToolError::Invalid(_))
        ));
        assert!(!destination.exists());
        assert!(encoded
            .save(42, &directory.0.join("wrong.jpg"), &[], &cancel)
            .is_err());
        let original = directory.0.join("original.png");
        std::fs::write(&original, b"keep original").unwrap();
        assert!(encoded
            .save(42, &original, &[original.clone()], &cancel)
            .is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"keep original");
        let moved = directory.0.join("moved.png");
        let saved = encoded.save(42, &destination, &[moved], &cancel).unwrap();
        assert_eq!((saved.width, saved.height), (2, 1));
        assert_eq!(std::fs::read(&destination).unwrap(), encoded.bytes);
        assert!(encoded.save(42, &destination, &[], &cancel).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), encoded.bytes);
        let cancelled = directory.0.join("cancelled.png");
        assert!(matches!(
            encoded.save(42, &cancelled, &[], &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert!(!cancelled.exists());
        assert!(encoded
            .save(42, &directory.0.join("missing/result.png"), &[], &cancel)
            .is_err());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }
}
