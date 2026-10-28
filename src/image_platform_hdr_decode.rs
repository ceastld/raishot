//! 已核独立HEVC Main10图项直接解为P010，再从编码域生成明确的SDR副本。
//! 不把系统RGB16布局当成原始PQ/HLG域，也不复用已经降为8位的像素。
use super::{
    coded::CodedInfo, hdr, hevc_native, p010, preflight, ColorFacts, PlatformDecoded,
    PlatformError, PlatformFormat,
};
use crate::{
    capture::Image,
    image_color_management::{ColorConversion, ManagedMetadata},
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
};
use std::sync::{atomic::AtomicBool, Arc};

fn geometry(
    physical: (u32, u32),
    coded: CodedInfo,
    aperture: Option<&hevc_native::Aperture>,
) -> Result<(u32, u32), PlatformError> {
    let origin = if physical == coded.coded_dimensions {
        coded.display_origin
    } else if physical == coded.display_dimensions {
        (0, 0)
    } else {
        return Err(super::invalid(
            "HDR解码尺寸不匹配编码画布或显示裁剪",
            "HDR decode dimensions match neither the coded canvas nor the display crop",
        ));
    };
    if let Some(aperture) = aperture {
        if (aperture.x, aperture.y) != origin
            || (aperture.width, aperture.height) != coded.display_dimensions
        {
            return Err(super::invalid(
                "HDR解码孔径与已核码流裁剪不一致",
                "HDR decode aperture does not match the checked coded crop",
            ));
        }
    }
    if origin
        .0
        .checked_add(coded.display_dimensions.0)
        .is_none_or(|end| end > physical.0)
        || origin
            .1
            .checked_add(coded.display_dimensions.1)
            .is_none_or(|end| end > physical.1)
    {
        return Err(super::invalid(
            "HDR显示裁剪超出解码平面",
            "HDR display crop exceeds the decoded planes",
        ));
    }
    Ok(origin)
}

/// None表示此来源未进入本分支；组件/能力错误由上层明确降级，其它错误不能静默换源。
pub(super) fn decode(
    plan: &preflight::Plan<'_>,
    index: usize,
    format: PlatformFormat,
    source_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Option<PlatformDecoded>, PlatformError> {
    super::cancelled(cancel)?;
    let item = *plan.catalog().get(index).ok_or_else(|| {
        super::invalid("原图项索引越界", "Source image item index is out of range")
    })?;
    if format != PlatformFormat::Heif || item.coded.bits_per_channel != 10 {
        return Ok(None);
    }
    let Some(cicp) = item
        .color
        .container_cicp
        .filter(|cicp| matches!(cicp.transfer, 16 | 18))
    else {
        return Ok(None);
    };
    if plan.icc_profile(index)?.is_some() {
        // ICC覆盖RGB色域/传递；不能绕过它只按nclx映射。
        return Ok(None);
    }
    if !matches!(cicp.matrix, 1 | 5 | 6 | 9) {
        return Err(super::coded::unsupported(
            "此HDR YCC矩阵尚不支持高精度映射",
            "This HDR YCC matrix is not supported for high-precision mapping",
        ));
    }
    let mapping = hdr::Plan::new(cicp, hdr::Policy::Sdr100Knee75, false)?;
    let resident = source_reserved_bytes
        .checked_add(plan.retained_bytes()?)
        .ok_or_else(super::budget)?;
    let raw = hevc_native::decode(plan.hevc_input(index)?, resident, max_output_bytes, cancel)?;
    super::cancelled(cancel)?;
    let origin = geometry((raw.width, raw.height), item.coded, raw.aperture.as_ref())?;
    let source = p010::Source::new(
        &raw.bytes,
        (raw.width, raw.height),
        raw.stride,
        origin,
        item.coded.display_dimensions,
        cicp,
    )?;
    let mapping_resident = resident
        .checked_add(raw.bytes.capacity() as u64)
        .ok_or_else(super::budget)?;
    let image = mapping.render_encoded_rgb16(
        item.coded.display_dimensions,
        item.orientation
            .unwrap_or(image::metadata::Orientation::NoTransforms),
        mapping_resident,
        max_output_bytes,
        cancel,
        |x, y| source.pixel(x, y),
    )?;
    super::cancelled(cancel)?;
    mapped(image, item, index, plan.catalog().len()).map(Some)
}

fn mapped(
    image: Image,
    item: preflight::ItemInfo,
    index: usize,
    count: usize,
) -> Result<PlatformDecoded, PlatformError> {
    let metadata = ManagedMetadata {
        source: Summary {
            icc: FieldSummary {
                status: MetadataStatus::Absent,
                encoded_bytes: 0,
                decoded_bytes: None,
            },
            exif: FieldSummary {
                status: MetadataStatus::NotInspected,
                encoded_bytes: 0,
                decoded_bytes: None,
            },
            color_model: ColorModel::Rgb,
            bits_per_channel: Some(item.coded.bits_per_channel),
            orientation: item.orientation,
        },
        profile: None,
        color_conversion: ColorConversion::HdrMappedSrgb,
    };
    let metadata_source =
        SourceMetadata::new(metadata, None).map_err(|message| PlatformError::Invalid {
            hresult: None,
            message,
        })?;
    Ok(PlatformDecoded {
        image,
        frame_count: u32::try_from(count).map_err(|_| super::budget())?,
        frame_index: u32::try_from(index).map_err(|_| super::budget())?,
        item_id: item.item_id,
        is_primary: index == 0,
        orientation: item.orientation,
        encoded_dimensions: Some(item.coded.coded_dimensions),
        bits_per_channel: Some(item.coded.bits_per_channel),
        decoded_color: None,
        color: Some(ColorFacts {
            container_cicp: item.color.container_cicp,
            bitstream_cicp: item.color.coded_color.cicp,
            sample_model: item.color.coded_color.model,
            hdr_mapped: true,
        }),
        managed: Some(metadata),
        metadata_source: Some(Arc::new(metadata_source)),
        warning: Some(tr!(
            "HDR已按固定100nit策略生成SDR预览；编辑及另存使用此8位副本，不保留原HDR精度。",
            "HDR mapped to a fixed 100-nit SDR preview; editing and saving use this 8-bit copy, without the original HDR precision."
        ).into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> CodedInfo {
        CodedInfo {
            coded_dimensions: (256, 128),
            display_dimensions: (250, 128),
            display_origin: (6, 0),
            bits_per_channel: 10,
            picture_buffers: 1,
            color: super::super::coded::CodedColor::UNKNOWN,
        }
    }

    #[test]
    fn image_platform_hdr_decode_applies_conformance_crop_only_once() {
        let coded = info();
        assert_eq!(geometry((256, 128), coded, None), Ok((6, 0)));
        assert_eq!(geometry((250, 128), coded, None), Ok((0, 0)));
        let aperture = hevc_native::Aperture {
            x: 6,
            y: 0,
            width: 250,
            height: 128,
        };
        assert_eq!(geometry((256, 128), coded, Some(&aperture)), Ok((6, 0)));
        assert!(geometry((250, 128), coded, Some(&aperture)).is_err());
        assert!(geometry((252, 128), coded, None).is_err());
        assert!(geometry((128, 250), coded, None).is_err());
        assert!(geometry(
            (256, 128),
            CodedInfo {
                display_origin: (8, 0),
                ..coded
            },
            None
        )
        .is_err());
    }

    #[test]
    fn image_platform_hdr_decode_delivers_mapped_pixels_with_original_item_facts() {
        let signal = super::super::Cicp {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: false,
        };
        let coded_color = super::super::coded::CodedColor {
            model: super::super::SampleModel::Ycc,
            cicp: Some(signal),
        };
        let coded = CodedInfo {
            coded_dimensions: (4, 2),
            display_dimensions: (2, 2),
            display_origin: (2, 0),
            color: coded_color,
            ..info()
        };
        let item = preflight::ItemInfo {
            item_id: 91,
            hidden: false,
            coded,
            orientation: image::metadata::Orientation::from_exif(6),
            color: preflight::ItemColor {
                container_cicp: Some(signal),
                coded_color,
            },
        };
        let pixels = vec![
            32, 64, 128, 255, 40, 80, 160, 255, 50, 100, 200, 255, 60, 120, 240, 255,
        ];
        let decoded = mapped(
            Image {
                width: 2,
                height: 2,
                bgra: pixels.clone(),
                readback_ms: 0.,
            },
            item,
            1,
            3,
        )
        .unwrap();
        assert_eq!(decoded.image.bgra, pixels);
        assert_eq!(
            (decoded.frame_index, decoded.frame_count, decoded.item_id),
            (1, 3, 91)
        );
        assert!(!decoded.is_primary);
        assert_eq!(decoded.encoded_dimensions, Some((4, 2)));
        assert_eq!(decoded.bits_per_channel, Some(10));
        assert_eq!(decoded.orientation, item.orientation);
        assert_eq!(decoded.decoded_color, None);
        let color = decoded.color.unwrap();
        assert_eq!(color.container_cicp, Some(signal));
        assert_eq!(color.bitstream_cicp, Some(signal));
        assert!(color.hdr_mapped);
        let metadata = decoded.managed.unwrap();
        assert_eq!(metadata.source.bits_per_channel, Some(10));
        assert_eq!(metadata.source.icc.status, MetadataStatus::Absent);
        assert_eq!(metadata.source.exif.status, MetadataStatus::NotInspected);
        assert_eq!(metadata.color_conversion, ColorConversion::HdrMappedSrgb);
        let sidecar = decoded.metadata_source.unwrap();
        assert_eq!(sidecar.facts(), metadata);
        assert_eq!(sidecar.retained_bytes(), 0);
    }

    #[test]
    fn image_platform_hdr_decode_eight_bit_fixture_does_not_start_hdr_backend() {
        let bytes = include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic");
        let cancel = AtomicBool::new(false);
        let plan = preflight::inspect(bytes, super::super::MAX_BYTES, &cancel).unwrap();
        assert!(decode(
            &plan,
            0,
            PlatformFormat::Heif,
            bytes.len() as u64,
            super::super::MAX_BYTES,
            &cancel
        )
        .unwrap()
        .is_none());
        assert!(matches!(
            decode(
                &plan,
                1,
                PlatformFormat::Heif,
                bytes.len() as u64,
                super::super::MAX_BYTES,
                &cancel
            ),
            Err(PlatformError::Invalid { .. })
        ));
        assert!(matches!(
            decode(
                &plan,
                0,
                PlatformFormat::Heif,
                bytes.len() as u64,
                super::super::MAX_BYTES,
                &AtomicBool::new(true)
            ),
            Err(PlatformError::Cancelled)
        ));
    }
}
