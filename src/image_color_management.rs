//! 元数据证据和颜色变换分开记录；失败时保留完整原像素，不提交部分变换结果。
use crate::{
    capture::Image,
    image_color_profile::{self, ProfileSummary},
    image_container_metadata::{ColorModel, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnappliedReason {
    TooLarge,
    MalformedMetadata,
    InvalidProfile,
    UnsupportedSource,
    UnsupportedProfile,
    TransformFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorConversion {
    NotInspected,
    /// 已读取来源信号，但没有把这份像素按该信号映射到sRGB。
    SignalUnmapped,
    AssumedSrgb,
    EmbeddedUnapplied(UnappliedReason),
    ConvertedSrgb,
    /// 在原16位RGB设备域完成ICC转换，然后量化为8位显示副本。
    ConvertedSrgb16,
    RenderedSrgb,
    /// 从已知编码域高精度输入按固定显示策略生成的SDR副本。
    HdrMappedSrgb,
}

impl ColorConversion {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NotInspected => tr!("未检查颜色配置", "Color profile not inspected"),
            Self::SignalUnmapped => tr!(
                "来源颜色信号尚未映射到sRGB",
                "Source color signal has not been mapped to sRGB"
            ),
            Self::AssumedSrgb => tr!("无ICC配置，按sRGB显示", "No ICC profile; display as sRGB"),
            Self::ConvertedSrgb => tr!("已按ICC转换为sRGB", "Converted to sRGB using ICC"),
            Self::ConvertedSrgb16 => tr!(
                "已按16位ICC转换为sRGB",
                "Converted to sRGB using 16-bit ICC input"
            ),
            Self::RenderedSrgb => tr!("已栅格化为sRGB", "Rasterized to sRGB"),
            Self::HdrMappedSrgb => tr!(
                "HDR已映射到sRGB预览（100nit，75nit膝点）",
                "HDR mapped to sRGB preview (100 nits, 75-nit knee)"
            ),
            Self::EmbeddedUnapplied(reason) => match reason {
                UnappliedReason::TooLarge => tr!("ICC超限，未应用", "ICC too large; not applied"),
                UnappliedReason::MalformedMetadata => {
                    tr!(
                        "ICC元数据损坏，未应用",
                        "Malformed ICC metadata; not applied"
                    )
                }
                UnappliedReason::InvalidProfile => {
                    tr!("ICC配置无效，未应用", "Invalid ICC profile; not applied")
                }
                UnappliedReason::UnsupportedSource => tr!(
                    "来源颜色模型暂不支持ICC转换",
                    "ICC conversion does not support this source color model"
                ),
                UnappliedReason::UnsupportedProfile => {
                    tr!("此ICC类型尚未应用", "This ICC type is not applied")
                }
                UnappliedReason::TransformFailed => {
                    tr!("ICC转换失败，未应用", "ICC conversion failed; not applied")
                }
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManagedMetadata {
    pub source: Summary,
    pub profile: Option<ProfileSummary>,
    pub color_conversion: ColorConversion,
}

pub(crate) struct ManagedImage {
    pub image: Image,
    pub metadata: ManagedMetadata,
    pub warning: Option<String>,
}

pub(crate) fn manage(
    image: Image,
    source: Summary,
    profile: Option<&[u8]>,
    cancel: &AtomicBool,
) -> Result<ManagedImage, String> {
    manage_with_transform(image, source, profile, cancel, |image, profile, cancel| {
        crate::image_color_transform::to_srgb(image, profile, || cancel.load(Ordering::Relaxed))
    })
}

/// 原16位直alpha RGB/RGBA先经过ICC，再量化为唯一BGRA8交付图。
/// resident_bytes须包含调用方仍持有的编码来源、16位输入实际capacity及其它目录/配置。
/// 本层不扩成整帧RGBA16，也不更改来源位深、方向或ICC提取事实。
pub(crate) fn manage_rgb16(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    source: Summary,
    profile: Option<&[u8]>,
    resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ManagedImage, String> {
    manage_rgb16_with_transform(
        data,
        dimensions,
        channels,
        source,
        profile,
        resident_bytes,
        max_output_bytes,
        cancel,
        |profile, cancel| {
            crate::image_color_transform::to_srgb16(
                data,
                dimensions,
                channels,
                profile,
                resident_bytes,
                max_output_bytes,
                || cancel.load(Ordering::Relaxed),
            )
        },
    )
}

fn manage_rgb16_with_transform(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    source: Summary,
    profile: Option<&[u8]>,
    resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    transform: impl FnOnce(&[u8], &AtomicBool) -> Result<Image, String>,
) -> Result<ManagedImage, String> {
    check_cancel(cancel)?;
    // 必须先为整图、两条WORD RGB行和配置摘要留足空间，再解析配置或进入CMM。
    crate::image_color_transform::validate_rgb16(
        data,
        dimensions,
        channels,
        profile.map_or(0, <[u8]>::len),
        resident_bytes,
        max_output_bytes,
    )?;
    let mut plan = color_plan(source, profile, cancel)?;
    check_cancel(cancel)?;
    if plan.transform {
        let converted = transform(profile.unwrap(), cancel);
        check_cancel(cancel)?;
        match converted {
            Ok(image) => {
                plan.metadata.color_conversion = ColorConversion::ConvertedSrgb16;
                return Ok(ManagedImage {
                    image,
                    metadata: plan.metadata,
                    warning: None,
                });
            }
            Err(error) => plan.failed(error),
        }
    }
    // 转换失败的部分目标已经释放；仅从完整原16位输入生成降级图，取消不能走降级。
    let image = crate::image_color_transform::quantize_rgb16(
        data,
        dimensions,
        channels,
        profile.map_or(0, <[u8]>::len),
        resident_bytes,
        max_output_bytes,
        || cancel.load(Ordering::Relaxed),
    )?;
    check_cancel(cancel)?;
    Ok(ManagedImage {
        image,
        metadata: plan.metadata,
        warning: plan.warning,
    })
}

fn manage_with_transform(
    image: Image,
    source: Summary,
    profile: Option<&[u8]>,
    cancel: &AtomicBool,
    transform: impl FnOnce(&Image, &[u8], &AtomicBool) -> Result<Image, String>,
) -> Result<ManagedImage, String> {
    let mut plan = color_plan(source, profile, cancel)?;
    if plan.transform {
        let converted = transform(&image, profile.unwrap(), cancel);
        check_cancel(cancel)?;
        match converted {
            Ok(image) => {
                plan.metadata.color_conversion = ColorConversion::ConvertedSrgb;
                return Ok(ManagedImage {
                    image,
                    metadata: plan.metadata,
                    warning: None,
                });
            }
            Err(error) => plan.failed(error),
        }
    }
    Ok(ManagedImage {
        image,
        metadata: plan.metadata,
        warning: plan.warning,
    })
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片颜色处理已取消", "Image color processing canceled").to_string())
    } else {
        Ok(())
    }
}

struct ColorPlan {
    metadata: ManagedMetadata,
    warning: Option<String>,
    transform: bool,
}

impl ColorPlan {
    fn failed(&mut self, error: String) {
        self.metadata.color_conversion =
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed);
        self.warning = Some(trf!(
            "ICC转换失败，保留解码颜色：{error}",
            "ICC conversion failed; keeping decoded colors: {error}"
        ));
        self.transform = false;
    }
}

fn color_plan(
    source: Summary,
    profile: Option<&[u8]>,
    cancel: &AtomicBool,
) -> Result<ColorPlan, String> {
    check_cancel(cancel)?;
    let mut metadata = ManagedMetadata {
        source,
        profile: None,
        color_conversion: ColorConversion::NotInspected,
    };
    let skipped = match source.icc.status {
        MetadataStatus::NotInspected => Some(ColorConversion::NotInspected),
        MetadataStatus::Absent => Some(ColorConversion::AssumedSrgb),
        MetadataStatus::TooLarge => Some(ColorConversion::EmbeddedUnapplied(
            UnappliedReason::TooLarge,
        )),
        MetadataStatus::Malformed => Some(ColorConversion::EmbeddedUnapplied(
            UnappliedReason::MalformedMetadata,
        )),
        MetadataStatus::Unsupported => Some(ColorConversion::EmbeddedUnapplied(
            UnappliedReason::UnsupportedProfile,
        )),
        MetadataStatus::Present => None,
    };
    if let Some(conversion) = skipped {
        metadata.color_conversion = conversion;
        let warning = matches!(conversion, ColorConversion::EmbeddedUnapplied(_))
            .then(|| conversion.label().to_string());
        return Ok(ColorPlan {
            metadata,
            warning,
            transform: false,
        });
    }
    let parsed = profile
        .ok_or_else(|| {
            tr!("ICC元数据缺少配置字节", "ICC metadata has no profile bytes").to_string()
        })
        .and_then(image_color_profile::inspect);
    let summary = match parsed {
        Ok(summary) => summary,
        Err(error) => {
            metadata.color_conversion =
                ColorConversion::EmbeddedUnapplied(UnappliedReason::InvalidProfile);
            return Ok(ColorPlan {
                metadata,
                warning: Some(trf!(
                    "ICC配置未应用：{error}",
                    "ICC profile not applied: {error}"
                )),
                transform: false,
            });
        }
    };
    metadata.profile = Some(summary);
    // CMYK/YCCK已被普通解码器转成RGB，不能再对这份像素套原CMYK配置。
    if !matches!(
        source.color_model,
        ColorModel::Rgb | ColorModel::Bgr | ColorModel::Palette
    ) {
        metadata.color_conversion =
            ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource);
    } else if summary.color_space != *b"RGB "
        || !matches!(&summary.connection_space, b"XYZ " | b"Lab ")
        || !matches!(&summary.device_class, b"scnr" | b"mntr" | b"prtr" | b"spac")
    {
        metadata.color_conversion =
            ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedProfile);
    } else {
        check_cancel(cancel)?;
        return Ok(ColorPlan {
            metadata,
            warning: None,
            transform: true,
        });
    }
    Ok(ColorPlan {
        metadata,
        warning: Some(metadata.color_conversion.label().to_string()),
        transform: false,
    })
}

trait FrameConverter {
    fn convert(
        &mut self,
        dimensions: (u32, u32),
        bgra: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Image, String>;
}

impl FrameConverter for crate::image_color_transform::SrgbTransform {
    fn convert(
        &mut self,
        dimensions: (u32, u32),
        bgra: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Image, String> {
        self.convert_pixels(dimensions, bgra, || cancel.load(Ordering::Relaxed))
    }
}

pub(crate) struct ManagedFrame {
    pub image: Image,
    pub metadata_source: Arc<SourceMetadata>,
    pub warning: Option<String>,
}

/// 仅由动画worker持有；画布保持来源颜色，完整合成后才生成一幅交付图。
pub(crate) struct FrameColorManager {
    transform: Option<Box<dyn FrameConverter>>,
    profile: Option<Arc<[u8]>>,
    metadata_source: Arc<SourceMetadata>,
    warning: Option<String>,
}

impl FrameColorManager {
    pub(crate) fn new(
        source: Summary,
        profile: Option<Vec<u8>>,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        Self::with_factory(source, profile, cancel, |profile, cancel| {
            crate::image_color_transform::SrgbTransform::new(profile, || {
                cancel.load(Ordering::Relaxed)
            })
            .map(|transform| Box::new(transform) as Box<dyn FrameConverter>)
        })
    }

    fn with_factory(
        source: Summary,
        profile: Option<Vec<u8>>,
        cancel: &AtomicBool,
        factory: impl FnOnce(Arc<[u8]>, &AtomicBool) -> Result<Box<dyn FrameConverter>, String>,
    ) -> Result<Self, String> {
        let mut plan = color_plan(source, profile.as_deref(), cancel)?;
        let mut retained = None;
        let mut transform = None;
        if plan.transform {
            let profile: Arc<[u8]> = Arc::from(profile.unwrap());
            let initialized = factory(profile.clone(), cancel);
            check_cancel(cancel)?;
            match initialized {
                Ok(initialized) => {
                    transform = Some(initialized);
                    retained = Some(profile);
                    // 这些事实只随第一幅成功转换的完整帧发布。
                    plan.metadata.color_conversion = ColorConversion::ConvertedSrgb;
                }
                Err(error) => {
                    plan.failed(error);
                    let metadata_source =
                        Arc::new(SourceMetadata::new(plan.metadata, Some(&profile))?);
                    return Ok(Self {
                        transform: None,
                        profile: None,
                        metadata_source,
                        warning: plan.warning,
                    });
                }
            }
        }
        check_cancel(cancel)?;
        Ok(Self {
            transform,
            profile: retained,
            metadata_source: Arc::new(SourceMetadata::new(plan.metadata, None)?),
            warning: plan.warning,
        })
    }

    /// 失败降级短时同时有原Arc、复制Vec和新Arc；系统CMM内部缓存不在此数内。
    pub(crate) fn retained_bytes(&self) -> usize {
        self.profile
            .as_ref()
            .map_or(0, |profile| profile.len().saturating_mul(3))
            .saturating_add(self.metadata_source.retained_bytes())
            .saturating_add(self.warning_capacity())
    }

    /// 来源可把可变警告及交付clone放入独立的元数据池，不挪用ICC转换余量。
    pub(crate) fn warning_capacity(&self) -> usize {
        self.warning.as_ref().map_or(0, String::capacity)
    }

    pub(crate) fn row_buffer_bytes(width: u32) -> Result<usize, String> {
        (width as usize)
            .checked_mul(6)
            .ok_or_else(|| tr!("颜色行缓冲超限", "Color row buffer exceeds limits").into())
    }

    pub(crate) fn render(
        &mut self,
        dimensions: (u32, u32),
        bgra: &[u8],
        cancel: &AtomicBool,
    ) -> Result<ManagedFrame, String> {
        check_cancel(cancel)?;
        let expected_bytes = frame_byte_length(dimensions, bgra.len())?;
        if let Some(transform) = self.transform.as_mut() {
            let converted = transform.convert(dimensions, bgra, cancel);
            // 取消不永久降级，也不发布原图；Restart可沿用同一已冻结配置。
            check_cancel(cancel)?;
            let converted = converted.and_then(|image| {
                if (image.width, image.height) != dimensions
                    || image.bgra.len() != expected_bytes
                    || image.bgra.capacity() > expected_bytes
                {
                    return Err(tr!(
                        "动画颜色输出无效或超过容量预算",
                        "Animation color output is invalid or exceeds its capacity budget"
                    )
                    .into());
                }
                Ok(image)
            });
            match converted {
                Ok(image) => {
                    return Ok(ManagedFrame {
                        image,
                        metadata_source: self.metadata_source.clone(),
                        warning: None,
                    });
                }
                Err(error) => {
                    // 原生转换器已释放部分输出，先关句柄，再构建原ICC降级上下文。
                    self.transform = None;
                    let mut plan = ColorPlan {
                        metadata: self.metadata_source.facts(),
                        warning: None,
                        transform: false,
                    };
                    plan.failed(error);
                    let profile = self.profile.take();
                    self.metadata_source =
                        Arc::new(SourceMetadata::new(plan.metadata, profile.as_deref())?);
                    self.warning = plan.warning;
                }
            }
        }
        let image = copy_frame(dimensions, bgra, cancel)?;
        Ok(ManagedFrame {
            image,
            metadata_source: self.metadata_source.clone(),
            warning: self.warning.clone(),
        })
    }
}

fn frame_byte_length((width, height): (u32, u32), source_len: usize) -> Result<usize, String> {
    let pixels = u64::from(width) * u64::from(height);
    let bytes = pixels
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok());
    if width == 0
        || height == 0
        || width > 65_535
        || height > 65_535
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
        || bytes != Some(source_len)
        || source_len as u64 > crate::image_decode::MAX_DECODE_BYTES
    {
        return Err(tr!(
            "动画颜色帧尺寸无效或超限",
            "Animation color frame is invalid or too large"
        )
        .into());
    }
    Ok(source_len)
}

fn copy_frame(dimensions: (u32, u32), source: &[u8], cancel: &AtomicBool) -> Result<Image, String> {
    check_cancel(cancel)?;
    let expected_bytes = frame_byte_length(dimensions, source.len())?;
    let (width, height) = dimensions;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(source.len()).map_err(|_| {
        tr!(
            "动画颜色帧分配失败",
            "Animation color frame allocation failed"
        )
        .to_string()
    })?;
    if bgra.capacity() > expected_bytes {
        return Err(tr!(
            "动画颜色帧实际容量超过预留",
            "Animation color frame capacity exceeds its reservation"
        )
        .into());
    }
    for row in source.chunks(width as usize * 4) {
        check_cancel(cancel)?;
        bgra.extend_from_slice(row);
    }
    check_cancel(cancel)?;
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    struct TestConverter<F>(F);

    impl<F> FrameConverter for TestConverter<F>
    where
        F: FnMut((u32, u32), &[u8], &AtomicBool) -> Result<Image, String>,
    {
        fn convert(
            &mut self,
            dimensions: (u32, u32),
            bgra: &[u8],
            cancel: &AtomicBool,
        ) -> Result<Image, String> {
            (self.0)(dimensions, bgra, cancel)
        }
    }

    fn rgb_source() -> Summary {
        let bytes = jpeg_with_profile(
            include_bytes!("../tests/data/image_decode/progressive_rgb.jpg"),
            include_bytes!("../assets/color/srgb.icc"),
        );
        crate::image_container_metadata::inspect(&bytes)
            .unwrap()
            .summary
    }

    fn jpeg_with_profile(jpeg: &[u8], profile: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe2];
        bytes.extend_from_slice(&((profile.len() + 16) as u16).to_be_bytes());
        bytes.extend_from_slice(b"ICC_PROFILE\0\x01\x01");
        bytes.extend_from_slice(profile);
        bytes.extend_from_slice(&jpeg[2..]);
        bytes
    }

    fn pixel() -> Image {
        Image {
            width: 1,
            height: 1,
            bgra: vec![29, 61, 113, 128],
            readback_ms: 0.,
        }
    }

    fn rgb16_source() -> Summary {
        let mut source = rgb_source();
        source.bits_per_channel = Some(16);
        source.orientation = Some(image::metadata::Orientation::Rotate90);
        source
    }

    #[test]
    fn image_color_management_word_success_keeps_source_facts_and_names_the_precision_stage() {
        let profile = include_bytes!("../assets/color/srgb.icc");
        let source = rgb16_source();
        let data = [0x1234, 0x5678, 0x9abc, 32_768];
        // 固定sRGB同配置分支无需任何系统调用，也仍须生成独立的8位交付图。
        let managed = manage_rgb16(
            &data,
            (1, 1),
            4,
            source,
            Some(profile),
            8,
            4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(managed.image.bgra, [154, 86, 18, 128]);
        assert_eq!(managed.metadata.source, source);
        assert_eq!(
            managed.metadata.color_conversion,
            ColorConversion::ConvertedSrgb16
        );
        assert_eq!(
            managed.metadata.profile,
            Some(image_color_profile::inspect(profile).unwrap())
        );
        assert!(managed.warning.is_none());
        assert_eq!(data, [0x1234, 0x5678, 0x9abc, 32_768]);
        let rgb = manage_rgb16(
            &data[..3],
            (1, 1),
            3,
            source,
            Some(profile),
            6,
            4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(rgb.image.bgra, [154, 86, 18, 255]);
    }

    #[test]
    fn image_color_management_word_failed_partial_transform_uses_the_complete_original_frame() {
        let data = [65_535, 32_768, 256, 0, 0x1234, 0x5678, 0x9abc, 32_768];
        let source = rgb16_source();
        let result = manage_rgb16_with_transform(
            &data,
            (1, 2),
            4,
            source,
            Some(include_bytes!("../assets/color/srgb.icc")),
            16,
            8,
            &AtomicBool::new(false),
            |_, _| {
                // 模拟第一行已经改变、第二行失败；错误返回后该部分输出先释放。
                let mut partial = vec![255; 8];
                partial[..4].copy_from_slice(&[9, 8, 7, 6]);
                drop(partial);
                Err("second WORD row failed".into())
            },
        )
        .unwrap();
        assert_eq!(result.image.bgra, [1, 128, 255, 0, 154, 86, 18, 128]);
        assert_eq!(result.metadata.source, source);
        assert!(result.metadata.profile.is_some());
        assert_eq!(
            result.metadata.color_conversion,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
        );
        assert!(result.warning.is_some());
        assert_eq!(
            data,
            [65_535, 32_768, 256, 0, 0x1234, 0x5678, 0x9abc, 32_768]
        );
    }

    #[test]
    fn image_color_management_word_skips_preserve_distinct_fallback_facts() {
        let profile = include_bytes!("../assets/color/srgb.icc").as_slice();
        let invalid = [0; 132];
        for (status, model, bytes, expected) in [
            (
                MetadataStatus::Absent,
                ColorModel::Rgb,
                None,
                ColorConversion::AssumedSrgb,
            ),
            (
                MetadataStatus::NotInspected,
                ColorModel::Rgb,
                None,
                ColorConversion::NotInspected,
            ),
            (
                MetadataStatus::Present,
                ColorModel::Gray,
                Some(profile),
                ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource),
            ),
            (
                MetadataStatus::Present,
                ColorModel::Cmyk,
                Some(profile),
                ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource),
            ),
            (
                MetadataStatus::Present,
                ColorModel::Rgb,
                Some(invalid.as_slice()),
                ColorConversion::EmbeddedUnapplied(UnappliedReason::InvalidProfile),
            ),
            (
                MetadataStatus::TooLarge,
                ColorModel::Rgb,
                None,
                ColorConversion::EmbeddedUnapplied(UnappliedReason::TooLarge),
            ),
        ] {
            let mut source = rgb16_source();
            source.icc.status = status;
            source.color_model = model;
            let result = manage_rgb16_with_transform(
                &[128, 32_768, 65_535, 0],
                (1, 1),
                4,
                source,
                bytes,
                8,
                4,
                &AtomicBool::new(false),
                |_, _| panic!("unqualified source must not enter CMM"),
            )
            .unwrap();
            assert_eq!(result.metadata.source, source);
            assert_eq!(result.metadata.color_conversion, expected);
            assert_eq!(result.image.bgra, [255, 128, 0, 0]);
        }
    }

    #[test]
    fn image_color_management_word_cancel_has_priority_over_success_error_and_invalid_layout() {
        for fail in [false, true] {
            let cancel = AtomicBool::new(false);
            let result = manage_rgb16_with_transform(
                &[128, 128, 128, 32_768],
                (1, 1),
                4,
                rgb16_source(),
                Some(include_bytes!("../assets/color/srgb.icc")),
                8,
                4,
                &cancel,
                |_, cancel| {
                    cancel.store(true, Ordering::Relaxed);
                    if fail {
                        Err("CMM failed".into())
                    } else {
                        Ok(pixel())
                    }
                },
            );
            assert_eq!(
                result.err().unwrap(),
                tr!("图片颜色处理已取消", "Image color processing canceled")
            );
        }
        let result = manage_rgb16(
            &[],
            (0, 0),
            0,
            rgb16_source(),
            None,
            0,
            0,
            &AtomicBool::new(true),
        );
        assert_eq!(
            result.err().unwrap(),
            tr!("图片颜色处理已取消", "Image color processing canceled")
        );
    }

    #[test]
    fn image_color_management_word_checks_caller_budget_before_profile_and_transform() {
        for (resident, output) in [(7, 4), (8, 3), (crate::image_decode::MAX_DECODE_BYTES, 4)] {
            let result = manage_rgb16_with_transform(
                &[0; 4],
                (1, 1),
                4,
                rgb16_source(),
                Some(&[0; 132]),
                resident,
                output,
                &AtomicBool::new(false),
                |_, _| panic!("budget rejection must precede CMM"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn image_color_management_cmyk_evidence_prevents_a_second_rgb_profile_transform() {
        let bytes = jpeg_with_profile(
            include_bytes!("../tests/data/image_decode/adobe_cmyk.jpg"),
            include_bytes!("../assets/color/srgb.icc"),
        );
        let inspected = crate::image_container_metadata::inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.color_model, ColorModel::Cmyk);
        let result = manage_with_transform(
            pixel(),
            inspected.summary,
            inspected.icc_profile.as_deref(),
            &AtomicBool::new(false),
            |_, _, _| panic!("CMYK-derived RGB pixels must not enter an RGB ICC transform"),
        )
        .unwrap();
        assert_eq!(result.image.bgra, [29, 61, 113, 128]);
        assert_eq!(
            result.metadata.color_conversion,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource)
        );
        assert!(result.metadata.profile.is_some());
        assert!(result.warning.is_some());
    }

    #[test]
    fn image_color_management_failed_transform_keeps_the_entire_input_and_reports_unapplied() {
        let bytes = jpeg_with_profile(
            include_bytes!("../tests/data/image_decode/progressive_rgb.jpg"),
            include_bytes!("../assets/color/srgb.icc"),
        );
        let inspected = crate::image_container_metadata::inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.color_model, ColorModel::Rgb);
        let result = manage_with_transform(
            pixel(),
            inspected.summary,
            inspected.icc_profile.as_deref(),
            &AtomicBool::new(false),
            |input, _, _| {
                assert_eq!(input.bgra, [29, 61, 113, 128]);
                Err("CMM unavailable".into())
            },
        )
        .unwrap();
        assert_eq!(result.image.bgra, [29, 61, 113, 128]);
        assert_eq!(
            result.metadata.color_conversion,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
        );
        assert!(result.warning.is_some());
    }

    #[test]
    fn image_color_management_cancellation_after_transform_cannot_publish_success_or_fallback() {
        let bytes = jpeg_with_profile(
            include_bytes!("../tests/data/image_decode/progressive_rgb.jpg"),
            include_bytes!("../assets/color/srgb.icc"),
        );
        let inspected = crate::image_container_metadata::inspect(&bytes).unwrap();
        for fail in [false, true] {
            let cancel = AtomicBool::new(false);
            let result = manage_with_transform(
                pixel(),
                inspected.summary,
                inspected.icc_profile.as_deref(),
                &cancel,
                |_, _, flag| {
                    flag.store(true, Ordering::Relaxed);
                    if fail {
                        Err("CMM interrupted".into())
                    } else {
                        Ok(pixel())
                    }
                },
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn image_color_management_frames_cache_context_and_keep_failed_canvas_complete() {
        let profile = include_bytes!("../assets/color/srgb.icc");
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        let mut manager = FrameColorManager::with_factory(
            rgb_source(),
            Some(profile.to_vec()),
            &AtomicBool::new(false),
            move |_, _| {
                Ok(Box::new(TestConverter(
                    move |dimensions, bgra: &[u8], cancel: &AtomicBool| {
                        observed.set(observed.get() + 1);
                        let mut output = copy_frame(dimensions, bgra, cancel)?;
                        output.bgra[0] = 211;
                        if observed.get() == 2 {
                            // 模拟原生逐行转换已生成部分像素后失败，部分图不能交付。
                            return Err("second frame failed after a partial row".into());
                        }
                        Ok(output)
                    },
                )))
            },
        )
        .unwrap();
        assert_eq!(manager.retained_bytes(), profile.len() * 3);
        assert_eq!(FrameColorManager::row_buffer_bytes(7).unwrap(), 42);
        let input = [29, 61, 113, 128, 17, 52, 93, 0];
        let cancel = AtomicBool::new(false);
        let first = manager.render((2, 1), &input, &cancel).unwrap();
        assert_eq!(first.image.bgra, [211, 61, 113, 128, 17, 52, 93, 0]);
        assert_eq!(
            first.metadata_source.facts().color_conversion,
            ColorConversion::ConvertedSrgb
        );
        assert!(first.warning.is_none());
        let second = manager.render((2, 1), &input, &cancel).unwrap();
        assert_eq!(second.image.bgra, input);
        assert_eq!(
            second.metadata_source.facts().color_conversion,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
        );
        assert!(second.warning.is_some());
        assert_eq!(
            manager.retained_bytes(),
            profile.len() + manager.warning.as_ref().unwrap().capacity()
        );
        let third = manager.render((2, 1), &input, &cancel).unwrap();
        assert_eq!(third.image.bgra, input);
        assert_eq!(calls.get(), 2);
        assert!(Arc::ptr_eq(&second.metadata_source, &third.metadata_source));
        // 已交付成功帧的事实不可被后帧失败改写。
        assert_eq!(
            first.metadata_source.facts().color_conversion,
            ColorConversion::ConvertedSrgb
        );
        assert_eq!(input, [29, 61, 113, 128, 17, 52, 93, 0]);
    }

    #[test]
    fn image_color_management_frame_budget_rejects_oversized_or_mismatched_converter_output() {
        for wrong_dimensions in [false, true] {
            let calls = Rc::new(Cell::new(0));
            let observed = calls.clone();
            let cancel = AtomicBool::new(false);
            let mut manager = FrameColorManager::with_factory(
                rgb_source(),
                Some(include_bytes!("../assets/color/srgb.icc").to_vec()),
                &cancel,
                move |_, _| {
                    Ok(Box::new(TestConverter(
                        move |dimensions, bgra: &[u8], cancel: &AtomicBool| {
                            observed.set(observed.get() + 1);
                            let mut image = copy_frame(dimensions, bgra, cancel)?;
                            image.bgra[0] = 211;
                            if wrong_dimensions {
                                image.width += 1;
                            } else {
                                image.bgra.reserve(64);
                            }
                            Ok(image)
                        },
                    )))
                },
            )
            .unwrap();
            let input = [29, 61, 113, 128, 17, 52, 93, 0];
            let first = manager.render((2, 1), &input, &cancel).unwrap();
            assert_eq!(first.image.bgra, input);
            assert_eq!(first.image.bgra.capacity(), input.len());
            assert_eq!(
                first.metadata_source.facts().color_conversion,
                ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
            );
            assert!(first.warning.is_some());
            let second = manager.render((2, 1), &input, &cancel).unwrap();
            assert_eq!(second.image.bgra, input);
            assert_eq!(calls.get(), 1);
            assert!(Arc::ptr_eq(&first.metadata_source, &second.metadata_source));
        }
    }

    #[test]
    fn image_color_management_frame_cancellation_keeps_cached_context_for_restart() {
        for fail in [false, true] {
            let factories = Rc::new(Cell::new(0));
            let factory_count = factories.clone();
            let calls = Rc::new(Cell::new(0));
            let observed = calls.clone();
            let cancel = AtomicBool::new(false);
            let mut manager = FrameColorManager::with_factory(
                rgb_source(),
                Some(include_bytes!("../assets/color/srgb.icc").to_vec()),
                &cancel,
                move |_, _| {
                    factory_count.set(factory_count.get() + 1);
                    Ok(Box::new(TestConverter(
                        move |dimensions, bgra: &[u8], cancel: &AtomicBool| {
                            observed.set(observed.get() + 1);
                            let image = copy_frame(dimensions, bgra, cancel)?;
                            if observed.get() == 1 {
                                cancel.store(true, Ordering::Relaxed);
                                if fail {
                                    return Err("canceled CMM operation".into());
                                }
                            }
                            Ok(image)
                        },
                    )))
                },
            )
            .unwrap();
            assert!(manager
                .render((1, 1), &[29, 61, 113, 128], &cancel)
                .is_err());
            cancel.store(false, Ordering::Relaxed);
            let restarted = manager
                .render((1, 1), &[29, 61, 113, 128], &cancel)
                .unwrap();
            assert_eq!(factories.get(), 1);
            assert_eq!(calls.get(), 2);
            assert_eq!(
                restarted.metadata_source.facts().color_conversion,
                ColorConversion::ConvertedSrgb
            );
            assert!(restarted.warning.is_none());
        }
    }

    #[test]
    fn image_color_management_frame_initialization_failure_and_skips_remain_explicit() {
        let cancel = AtomicBool::new(false);
        let mut failed = FrameColorManager::with_factory(
            rgb_source(),
            Some(include_bytes!("../assets/color/srgb.icc").to_vec()),
            &cancel,
            |_, _| Err("CMM unavailable".into()),
        )
        .unwrap();
        let image = failed.render((1, 1), &[29, 61, 113, 128], &cancel).unwrap();
        assert_eq!(image.image.bgra, [29, 61, 113, 128]);
        assert_eq!(
            image.metadata_source.facts().color_conversion,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
        );
        assert_eq!(
            image.metadata_source.retained_bytes(),
            include_bytes!("../assets/color/srgb.icc").len()
        );
        assert!(image.warning.is_some());
        let mut absent = rgb_source();
        absent.icc = crate::image_container_metadata::FieldSummary {
            status: MetadataStatus::Absent,
            encoded_bytes: 0,
            decoded_bytes: None,
        };
        let mut manager = FrameColorManager::with_factory(absent, None, &cancel, |_, _| {
            panic!("absent ICC must not create a transform")
        })
        .unwrap();
        let output = manager
            .render((1, 1), &[29, 61, 113, 128], &cancel)
            .unwrap();
        assert_eq!(
            output.metadata_source.facts().color_conversion,
            ColorConversion::AssumedSrgb
        );
        assert!(output.warning.is_none());
        assert!(manager.render((0, 1), &[], &cancel).is_err());
        assert!(manager.render((1, 2), &[0; 4], &cancel).is_err());
        assert!(FrameColorManager::with_factory(
            rgb_source(),
            Some(include_bytes!("../assets/color/srgb.icc").to_vec()),
            &cancel,
            |_, cancel| {
                cancel.store(true, Ordering::Relaxed);
                Err("initialization canceled".into())
            },
        )
        .is_err());
    }
}
