//! PSD合并图进入统一显示与另存链路；保留来源事实，不授予图层或色管资格。
use super::{DecodedImage, ImageMetadata};
use crate::{
    image_color_management::{ColorConversion, ManagedMetadata},
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
    image_metadata::SourceFormat,
};
use std::sync::{atomic::AtomicBool, Arc};

#[path = "image_psd.rs"]
mod reader;

pub(super) fn decode(
    bytes: &[u8],
    source_reserved_bytes: u64,
    external_resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> crate::Result<DecodedImage> {
    let decoded = reader::decode(
        bytes,
        source_reserved_bytes,
        external_resident_bytes,
        max_output_bytes,
        cancel,
    )?;
    // 读取器已拒绝ICC、EXIF及可声明方向的XMP资源，不能据此声称已转换为sRGB。
    let absent = FieldSummary {
        status: MetadataStatus::Absent,
        encoded_bytes: 0,
        decoded_bytes: None,
    };
    let details = ManagedMetadata {
        source: Summary {
            icc: absent,
            exif: absent,
            color_model: ColorModel::Rgb,
            bits_per_channel: Some(8),
            orientation: None,
        },
        profile: None,
        color_conversion: match decoded.color {
            reader::ColorStatus::Unmanaged => ColorConversion::SignalUnmapped,
        },
    };
    let warning = tr!(
        "仅查看PSD的RGB8不透明合并图；未映射颜色，不保留图层，另存为普通图片",
        "Viewing the opaque RGB8 PSD composite; color is unmapped. Save a regular image without layers."
    )
    .to_string();
    let metadata_source = SourceMetadata::new(details, None)?;
    let controls = (warning.capacity() as u64)
        .checked_add(std::mem::size_of::<SourceMetadata>() as u64)
        .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>() as u64));
    if controls.is_none_or(|bytes| bytes > reader::CONTROL_BYTES) {
        return Err(reader::PsdError::Budget.into());
    }
    // 像素不复制；短提示与Arc侧车使用读取前已计入的64KiB控制预留。
    let dimensions = (decoded.image.width, decoded.image.height);
    super::check_cancel(cancel)?;
    Ok(DecodedImage {
        image: decoded.image,
        metadata: ImageMetadata {
            format: SourceFormat::Psd,
            encoded_dimensions: Some(dimensions),
            display_dimensions: dimensions,
            orientation: None,
            encoded_bytes: Some(bytes.len() as u64),
            decoded_color: Some(image::ColorType::Rgb8),
            details: Some(details),
        },
        warning: Some(warning),
        metadata_source: Some(Arc::new(metadata_source)),
    })
}
