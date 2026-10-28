//! 按需生成的自有格式小样；编码不可用不等同于解码不支持。
//! 只验证当前构建的单帧、小尺寸、8位像素读回，不代替动画、多页或外部样本验收。
use crate::image_decode::DecodedImage;
use image::{error::UnsupportedErrorKind, DynamicImage, ImageError, ImageFormat, RgbaImage};
use serde::Serialize;
use std::io::Cursor;

const WIDTH: u32 = 3;
const HEIGHT: u32 = 2;
const PIXELS: usize = (WIDTH * HEIGHT) as usize;
const JPEG_TOLERANCE: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SampleFormat {
    Png,
    Jpeg,
    Bmp,
    Gif,
    Webp,
    Tiff,
    Ico,
}

impl SampleFormat {
    pub(crate) const ALL: [Self; 7] = [
        Self::Png,
        Self::Jpeg,
        Self::Bmp,
        Self::Gif,
        Self::Webp,
        Self::Tiff,
        Self::Ico,
    ];

    fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Bmp => ImageFormat::Bmp,
            Self::Gif => ImageFormat::Gif,
            Self::Webp => ImageFormat::WebP,
            Self::Tiff => ImageFormat::Tiff,
            Self::Ico => ImageFormat::Ico,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SampleContract {
    OpaqueRgbLossless,
    OpaqueJpegLossy { max_channel_error: u8 },
    StraightAlphaLossless,
    BinaryAlphaPaletteLossless,
}

impl SampleContract {
    fn channel_tolerance(self) -> u8 {
        match self {
            Self::OpaqueJpegLossy { max_channel_error } => max_channel_error,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SampleStatus {
    SampleEncoderUnavailable,
    SampleEncodeFailed,
    DecodeFailed,
    ValidationFailed,
    Passed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ColorChannel {
    Red,
    Green,
    Blue,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SampleIssue {
    HeaderMismatch {
        expected: SampleFormat,
        detected: Option<String>,
    },
    DecodedFormatMismatch {
        expected: SampleFormat,
        actual: String,
    },
    UnexpectedOrientation {
        exif_value: u8,
    },
    UnknownOrientation,
    UnknownEncodedDimensions,
    DimensionsMismatch {
        expected: [u32; 2],
        pixels: [u32; 2],
        source_metadata: Option<[u32; 2]>,
        display_metadata: [u32; 2],
    },
    PixelBufferLengthMismatch {
        expected: usize,
        actual: usize,
    },
    AlphaMismatch {
        pixel: usize,
        expected: u8,
        actual: u8,
    },
    ColorMismatch {
        pixel: usize,
        channel: ColorChannel,
        expected: u8,
        actual: u8,
        tolerance: u8,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct FormatSampleResult {
    pub format: SampleFormat,
    pub contract: SampleContract,
    pub dimensions: [u32; 2],
    pub encoded_bytes: Option<u64>,
    pub status: SampleStatus,
    pub error: Option<String>,
    pub issues: Vec<SampleIssue>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FormatSampleReport {
    pub schema_version: u8,
    pub samples: Vec<FormatSampleResult>,
}

struct Sample {
    contract: SampleContract,
    rgba: [[u8; 4]; PIXELS],
}

fn sample(format: SampleFormat) -> Sample {
    let rgba = match format {
        // 均色避免把JPEG色度抽样的边缘差异误判为通道顺序错误。
        SampleFormat::Jpeg => [[83, 147, 201, 255]; PIXELS],
        SampleFormat::Bmp => [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [23, 45, 67, 255],
            [17, 89, 201, 255],
            [255, 255, 255, 255],
        ],
        // 颜色总数远少于256，GIF小样可建立精确调色板；透明只使用0/255。
        SampleFormat::Gif => [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [13, 19, 29, 0],
            [0, 0, 255, 255],
            [13, 19, 29, 0],
            [255, 255, 255, 255],
        ],
        _ => [
            [255, 0, 0, 255],
            [0, 255, 0, 128],
            [13, 19, 29, 0],
            [23, 45, 67, 1],
            [17, 89, 201, 254],
            [255, 255, 255, 255],
        ],
    };
    let contract = match format {
        SampleFormat::Jpeg => SampleContract::OpaqueJpegLossy {
            max_channel_error: JPEG_TOLERANCE,
        },
        SampleFormat::Bmp => SampleContract::OpaqueRgbLossless,
        SampleFormat::Gif => SampleContract::BinaryAlphaPaletteLossless,
        _ => SampleContract::StraightAlphaLossless,
    };
    Sample { contract, rgba }
}

fn encoding_status(error: &ImageError) -> SampleStatus {
    if matches!(error, ImageError::Unsupported(error)
        if matches!(error.kind(), UnsupportedErrorKind::Format(_)))
    {
        SampleStatus::SampleEncoderUnavailable
    } else {
        SampleStatus::SampleEncodeFailed
    }
}

fn header_issues(format: SampleFormat, bytes: &[u8]) -> Vec<SampleIssue> {
    let detected = image::guess_format(bytes).ok();
    if detected == Some(format.image_format()) {
        Vec::new()
    } else {
        vec![SampleIssue::HeaderMismatch {
            expected: format,
            detected: detected.map(|format| format!("{format:?}")),
        }]
    }
}

fn decoded_issues(
    format: SampleFormat,
    expected: &Sample,
    decoded: &DecodedImage,
) -> Vec<SampleIssue> {
    let mut issues = Vec::new();
    if decoded.metadata.format != format.image_format() {
        issues.push(SampleIssue::DecodedFormatMismatch {
            expected: format,
            actual: format!("{:?}", decoded.metadata.format),
        });
    }
    // 生成器不写方向元数据；均色JPEG无法仅凭像素发现错误的方向解释。
    match decoded.metadata.orientation {
        Some(image::metadata::Orientation::NoTransforms) => {}
        Some(orientation) => issues.push(SampleIssue::UnexpectedOrientation {
            exif_value: orientation.to_exif(),
        }),
        None => issues.push(SampleIssue::UnknownOrientation),
    }
    let dimensions = [WIDTH, HEIGHT];
    let pixels = [decoded.image.width, decoded.image.height];
    let source_metadata = decoded
        .metadata
        .encoded_dimensions
        .map(|(width, height)| [width, height]);
    if source_metadata.is_none() {
        issues.push(SampleIssue::UnknownEncodedDimensions);
    }
    let display_metadata = [
        decoded.metadata.display_dimensions.0,
        decoded.metadata.display_dimensions.1,
    ];
    if pixels != dimensions
        || source_metadata.is_some_and(|source| source != dimensions)
        || display_metadata != dimensions
    {
        issues.push(SampleIssue::DimensionsMismatch {
            expected: dimensions,
            pixels,
            source_metadata,
            display_metadata,
        });
    }
    let bgra = &decoded.image.bgra;
    if bgra.len() != PIXELS * 4 {
        issues.push(SampleIssue::PixelBufferLengthMismatch {
            expected: PIXELS * 4,
            actual: bgra.len(),
        });
        return issues;
    }
    let tolerance = expected.contract.channel_tolerance();
    for (pixel, (rgba, bgra)) in expected.rgba.iter().zip(bgra.chunks_exact(4)).enumerate() {
        if rgba[3] != bgra[3] {
            issues.push(SampleIssue::AlphaMismatch {
                pixel,
                expected: rgba[3],
                actual: bgra[3],
            });
        }
        // 完全透明像素的隐藏RGB不作为往返契约；其alpha仍严格核对。
        if rgba[3] == 0 {
            continue;
        }
        for (channel, expected, actual) in [
            (ColorChannel::Red, rgba[0], bgra[2]),
            (ColorChannel::Green, rgba[1], bgra[1]),
            (ColorChannel::Blue, rgba[2], bgra[0]),
        ] {
            if expected.abs_diff(actual) > tolerance {
                issues.push(SampleIssue::ColorMismatch {
                    pixel,
                    channel,
                    expected,
                    actual,
                    tolerance,
                });
            }
        }
    }
    issues
}

/// 只在调用时生成一个内存小样并经生产解码入口读回；不访问磁盘或系统组件。
pub(crate) fn run_format(format: SampleFormat) -> FormatSampleResult {
    let sample = sample(format);
    let mut result = FormatSampleResult {
        format,
        contract: sample.contract,
        dimensions: [WIDTH, HEIGHT],
        encoded_bytes: None,
        status: SampleStatus::Passed,
        error: None,
        issues: Vec::new(),
    };
    let rgba = RgbaImage::from_fn(WIDTH, HEIGHT, |x, y| {
        image::Rgba(sample.rgba[(y * WIDTH + x) as usize])
    });
    let source = match format {
        SampleFormat::Jpeg | SampleFormat::Bmp => {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).into_rgb8())
        }
        _ => DynamicImage::ImageRgba8(rgba),
    };
    let mut output = Cursor::new(Vec::new());
    if let Err(error) = source.write_to(&mut output, format.image_format()) {
        result.status = encoding_status(&error);
        result.error = Some(error.to_string());
        return result;
    }
    let bytes = output.into_inner();
    result.encoded_bytes = Some(bytes.len() as u64);
    result.issues = header_issues(format, &bytes);
    match crate::image_decode::decode_bytes(&bytes) {
        Ok(decoded) => {
            result
                .issues
                .extend(decoded_issues(format, &sample, &decoded));
            if !result.issues.is_empty() {
                result.status = SampleStatus::ValidationFailed;
            }
        }
        Err(error) => {
            result.status = SampleStatus::DecodeFailed;
            result.error = Some(error.to_string());
        }
    }
    result
}

/// 此报告只描述本次实际运行结果；编码器不可用时，解码能力仍未验证。
pub(crate) fn run() -> FormatSampleReport {
    FormatSampleReport {
        schema_version: 1,
        samples: SampleFormat::ALL.into_iter().map(run_format).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture::Image, image_metadata::ImageMetadata};
    use image::{
        error::{ImageFormatHint, UnsupportedError},
        metadata::Orientation,
    };

    fn decoded(format: SampleFormat) -> DecodedImage {
        let sample = sample(format);
        DecodedImage {
            metadata_source: None,
            warning: None,
            image: Image {
                width: WIDTH,
                height: HEIGHT,
                bgra: sample
                    .rgba
                    .iter()
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect(),
                readback_ms: 0.,
            },
            metadata: ImageMetadata {
                format: format.image_format().into(),
                encoded_dimensions: Some((WIDTH, HEIGHT)),
                display_dimensions: (WIDTH, HEIGHT),
                orientation: Some(Orientation::NoTransforms),
                decoded_color: Some(image::ColorType::Rgba8),
                encoded_bytes: None,
                details: None,
            },
        }
    }

    #[test]
    fn image_format_samples_baseline_roundtrip_and_json_report() {
        let report = run();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        for format in [SampleFormat::Png, SampleFormat::Jpeg, SampleFormat::Bmp] {
            let result = report
                .samples
                .iter()
                .find(|sample| sample.format == format)
                .unwrap();
            assert_eq!(result.status, SampleStatus::Passed, "{result:?}");
        }
        for result in &report.samples {
            assert!(
                matches!(
                    result.status,
                    SampleStatus::Passed | SampleStatus::SampleEncoderUnavailable
                ),
                "{result:?}"
            );
        }
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["samples"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn image_format_samples_missing_encoder_and_color_failure_are_distinct() {
        let missing = ImageError::Unsupported(UnsupportedError::from_format_and_kind(
            ImageFormatHint::Exact(ImageFormat::WebP),
            UnsupportedErrorKind::Format(ImageFormatHint::Exact(ImageFormat::WebP)),
        ));
        assert_eq!(
            encoding_status(&missing),
            SampleStatus::SampleEncoderUnavailable
        );
        let color = ImageError::Unsupported(UnsupportedError::from_format_and_kind(
            ImageFormatHint::Exact(ImageFormat::Jpeg),
            UnsupportedErrorKind::Color(image::ExtendedColorType::Rgba8),
        ));
        assert_eq!(encoding_status(&color), SampleStatus::SampleEncodeFailed);
    }

    #[test]
    fn image_format_samples_wrong_header_dimensions_channel_and_alpha_are_reported() {
        assert!(matches!(
            header_issues(SampleFormat::Png, b"BM").as_slice(),
            [SampleIssue::HeaderMismatch { .. }]
        ));
        let mut decoded = decoded(SampleFormat::Png);
        decoded.metadata.display_dimensions = (WIDTH + 1, HEIGHT);
        decoded.image.bgra[2] = 0;
        decoded.image.bgra[3] = 127;
        let issues = decoded_issues(SampleFormat::Png, &sample(SampleFormat::Png), &decoded);
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, SampleIssue::DimensionsMismatch { .. })));
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, SampleIssue::AlphaMismatch { pixel: 0, .. })));
        assert!(issues.iter().any(|issue| matches!(
            issue,
            SampleIssue::ColorMismatch {
                pixel: 0,
                channel: ColorChannel::Red,
                ..
            }
        )));
        decoded.image.bgra.pop();
        assert!(
            decoded_issues(SampleFormat::Png, &sample(SampleFormat::Png), &decoded)
                .iter()
                .any(|issue| matches!(issue, SampleIssue::PixelBufferLengthMismatch { .. }))
        );
    }

    #[test]
    fn image_format_samples_alpha_and_jpeg_tolerance_have_separate_contracts() {
        let expected = sample(SampleFormat::Png);
        let mut rgba = decoded(SampleFormat::Png);
        rgba.image.bgra[8..11].fill(255);
        assert!(decoded_issues(SampleFormat::Png, &expected, &rgba).is_empty());
        rgba.image.bgra[4] = 200;
        assert!(decoded_issues(SampleFormat::Png, &expected, &rgba)
            .iter()
            .any(|issue| matches!(issue, SampleIssue::ColorMismatch { pixel: 1, .. })));
        let expected = sample(SampleFormat::Jpeg);
        let mut jpeg = decoded(SampleFormat::Jpeg);
        jpeg.image.bgra[0] += JPEG_TOLERANCE;
        assert!(decoded_issues(SampleFormat::Jpeg, &expected, &jpeg).is_empty());
        jpeg.image.bgra[0] += 1;
        jpeg.image.bgra[3] -= 1;
        let issues = decoded_issues(SampleFormat::Jpeg, &expected, &jpeg);
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, SampleIssue::ColorMismatch { pixel: 0, .. })));
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, SampleIssue::AlphaMismatch { pixel: 0, .. })));
    }

    #[test]
    fn image_format_samples_flat_jpeg_still_requires_unoriented_metadata() {
        let expected = sample(SampleFormat::Jpeg);
        let mut jpeg = decoded(SampleFormat::Jpeg);
        assert!(decoded_issues(SampleFormat::Jpeg, &expected, &jpeg).is_empty());
        jpeg.metadata.orientation = Some(Orientation::FlipHorizontal);
        assert_eq!(
            decoded_issues(SampleFormat::Jpeg, &expected, &jpeg),
            [SampleIssue::UnexpectedOrientation { exif_value: 2 }]
        );
    }

    #[test]
    fn image_format_samples_unknown_encoded_dimensions_are_reported_without_fabrication() {
        let mut decoded = decoded(SampleFormat::Png);
        decoded.metadata.encoded_dimensions = None;
        let issues = decoded_issues(SampleFormat::Png, &sample(SampleFormat::Png), &decoded);
        assert_eq!(issues, [SampleIssue::UnknownEncodedDimensions]);
        decoded.metadata.display_dimensions = (WIDTH + 1, HEIGHT);
        assert!(
            decoded_issues(SampleFormat::Png, &sample(SampleFormat::Png), &decoded)
                .iter()
                .any(|issue| matches!(
                    issue,
                    SampleIssue::DimensionsMismatch {
                        source_metadata: None,
                        ..
                    }
                ))
        );
    }

    #[test]
    fn image_format_samples_unknown_orientation_is_reported_without_assuming_identity() {
        let mut decoded = decoded(SampleFormat::Jpeg);
        decoded.metadata.orientation = None;
        assert_eq!(
            decoded_issues(SampleFormat::Jpeg, &sample(SampleFormat::Jpeg), &decoded),
            [SampleIssue::UnknownOrientation]
        );
    }
}
