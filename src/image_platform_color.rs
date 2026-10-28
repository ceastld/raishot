//! 系统直alpha BGRA在禁用系统色管后，按选中原图项的RGB ICC转换一次。
//! 编码平面的RGB/YCC事实另由ColorFacts保留；本层Summary的RGB表示ICC设备域。
//! 本批不为HDR或已降为8位的高精度来源授予sRGB映射、原ICC导出资格。
//! 可见缓冲预算不包含系统codec或CMM内部缓存，不能据此保证原生内存峰值。
use super::{preflight::ItemInfo, PlatformError, SampleModel};
use crate::{
    capture::Image,
    image_color_management::{self, ColorConversion, ManagedImage, ManagedMetadata},
    image_color_profile,
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const SRGB_PROFILE: &[u8] = include_bytes!("../assets/color/srgb.icc");
// ICC结构摘要和原生配置打开之前的有界工作区；原生内部缓存不包含在此数中。
const PROFILE_WORKSPACE: u64 = 4 * 1024 * 1024;

pub(super) struct Managed {
    pub image: Image,
    pub metadata: ManagedMetadata,
    pub warning: Option<String>,
    pub metadata_source: Option<Arc<SourceMetadata>>,
}

fn cancelled(cancel: &AtomicBool) -> Result<(), PlatformError> {
    if cancel.load(Ordering::Relaxed) {
        Err(PlatformError::Cancelled)
    } else {
        Ok(())
    }
}

fn budget() -> PlatformError {
    PlatformError::Budget(
        tr!(
            "系统图片颜色处理超过256MiB可见缓冲预算",
            "System image color processing exceeds the 256 MiB visible-buffer budget"
        )
        .into(),
    )
}

/// 原source容量与Plan实际目录容量由调用方提供；COM、Array和虚拟文件须已释放。
/// ICC借自原source，另留一份配置长度，覆盖进入原生转换器时的可见配置副本。
fn check_budget(
    image: &Image,
    profile_bytes: usize,
    resident_bytes: u64,
) -> Result<(), PlatformError> {
    let pixels = super::coded::checked_pixels((image.width, image.height), super::MAX_BYTES)?;
    let output = pixels.checked_mul(4).ok_or_else(budget)?;
    if output != image.bgra.len() as u64 {
        return Err(super::invalid(
            "系统图片颜色缓冲与尺寸不一致",
            "System image color buffer does not match its dimensions",
        ));
    }
    if profile_bytes > image_color_profile::MAX_PROFILE_BYTES {
        return Err(budget());
    }
    resident_bytes
        .checked_add(image.bgra.capacity() as u64)
        .and_then(|bytes| bytes.checked_add(output))
        .and_then(|bytes| bytes.checked_add(u64::from(image.width).checked_mul(6)?))
        .and_then(|bytes| bytes.checked_add(profile_bytes as u64))
        .and_then(|bytes| bytes.checked_add(SRGB_PROFILE.len() as u64))
        .and_then(|bytes| bytes.checked_add(PROFILE_WORKSPACE))
        .filter(|bytes| *bytes <= super::MAX_BYTES)
        .ok_or_else(budget)?;
    Ok(())
}

fn source(item: ItemInfo, profile: Option<&[u8]>) -> Summary {
    let profile_bytes = profile.map_or(0, |bytes| bytes.len() as u64);
    Summary {
        icc: FieldSummary {
            status: if profile.is_some() {
                MetadataStatus::Present
            } else {
                MetadataStatus::Absent
            },
            encoded_bytes: profile_bytes,
            decoded_bytes: profile.map(|_| profile_bytes),
        },
        // 方向已单独检查并应用；没有解析或保留EXIF白名单字段。
        exif: FieldSummary {
            status: MetadataStatus::NotInspected,
            encoded_bytes: 0,
            decoded_bytes: None,
        },
        // 已确认的RGB/YCC编码平面在DoNotColorManage下解成原ICC设备域RGB。
        // 未知或灰度来源不能仅凭请求了BGRA8而取得这项资格。
        color_model: match item.color.coded_color.model {
            SampleModel::Rgb | SampleModel::Ycc => ColorModel::Rgb,
            SampleModel::Gray => ColorModel::Gray,
            SampleModel::Unknown => ColorModel::Unknown,
        },
        bits_per_channel: Some(item.coded.bits_per_channel),
        orientation: item.orientation,
    }
}

fn hdr_signal(item: ItemInfo) -> bool {
    [item.color.container_cicp, item.color.coded_color.cicp]
        .into_iter()
        .flatten()
        .any(|cicp| matches!(cicp.transfer, 16 | 18))
}

/// 仅消费同次预检的选中图项与其ICC；不重新读来源或调用系统元数据getter。
pub(super) fn manage(
    image: Image,
    item: ItemInfo,
    profile: Option<&[u8]>,
    resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Managed, PlatformError> {
    manage_with(
        image,
        item,
        profile,
        resident_bytes,
        cancel,
        image_color_management::manage,
    )
}

fn manage_with(
    image: Image,
    item: ItemInfo,
    profile: Option<&[u8]>,
    resident_bytes: u64,
    cancel: &AtomicBool,
    manager: impl FnOnce(Image, Summary, Option<&[u8]>, &AtomicBool) -> Result<ManagedImage, String>,
) -> Result<Managed, PlatformError> {
    cancelled(cancel)?;
    // 即使最后没有进入CMM，也必须先覆盖配置摘要和最坏像素副本的预算。
    check_budget(&image, profile.map_or(0, <[u8]>::len), resident_bytes)?;
    cancelled(cancel)?;
    let source = source(item, profile);
    let hdr = hdr_signal(item);
    if profile.is_none() || item.coded.bits_per_channel != 8 || hdr {
        let parsed = profile.map(image_color_profile::inspect);
        cancelled(cancel)?;
        let mut warning = if hdr || item.coded.bits_per_channel != 8 {
            tr!(
                "来源HDR或高位深尚未映射到sRGB，未应用ICC。",
                "Source HDR or high bit depth has not been mapped to sRGB; ICC was not applied."
            )
            .to_string()
        } else {
            tr!(
                "无ICC配置，来源颜色信号尚未映射到sRGB。",
                "No ICC profile; source color signal is not mapped to sRGB."
            )
            .to_string()
        };
        let profile = match parsed {
            Some(Ok(profile)) => Some(profile),
            Some(Err(error)) => {
                warning.push(' ');
                warning.push_str(&trf!(
                    "ICC配置无效：{error}",
                    "Invalid ICC profile: {error}"
                ));
                None
            }
            None => None,
        };
        cancelled(cancel)?;
        return Ok(Managed {
            image,
            metadata: ManagedMetadata {
                source,
                profile,
                color_conversion: ColorConversion::SignalUnmapped,
            },
            warning: Some(warning),
            metadata_source: None,
        });
    }

    let result = manager(image, source, profile, cancel);
    cancelled(cancel)?;
    let managed = result.map_err(|message| PlatformError::Invalid {
        hresult: None,
        message,
    })?;
    // 成功转换的像素才可嵌固定sRGB；原生降级像素不获原ICC保留资格。
    let metadata_source = if managed.metadata.color_conversion == ColorConversion::ConvertedSrgb {
        Some(Arc::new(
            SourceMetadata::new(managed.metadata, None).map_err(|message| {
                PlatformError::Invalid {
                    hresult: None,
                    message,
                }
            })?,
        ))
    } else {
        None
    };
    cancelled(cancel)?;
    Ok(Managed {
        image: managed.image,
        metadata: managed.metadata,
        warning: managed.warning,
        metadata_source,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        coded::{Cicp, CodedColor, CodedInfo},
        preflight::ItemColor,
    };
    use super::*;
    use crate::image_color_management::UnappliedReason;
    use image::metadata::Orientation;
    use std::cell::Cell;

    fn pixel() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: vec![1, 2, 3, 0, 4, 5, 6, 128, 7, 8, 9, 255, 10, 11, 12, 64],
            readback_ms: 7.5,
        }
    }

    fn signal(transfer: u16) -> Cicp {
        Cicp {
            primaries: 1,
            transfer,
            matrix: 6,
            full_range: true,
        }
    }

    fn item(model: SampleModel) -> ItemInfo {
        let color = CodedColor {
            model,
            cicp: Some(signal(13)),
        };
        ItemInfo {
            item_id: 19,
            hidden: false,
            coded: CodedInfo {
                coded_dimensions: (2, 2),
                display_dimensions: (2, 2),
                display_origin: (0, 0),
                bits_per_channel: 8,
                picture_buffers: 1,
                color,
            },
            orientation: Some(Orientation::FlipHorizontal),
            color: ItemColor {
                container_cicp: Some(signal(13)),
                coded_color: color,
            },
        }
    }

    fn metadata(source: Summary, conversion: ColorConversion) -> ManagedMetadata {
        ManagedMetadata {
            source,
            profile: Some(image_color_profile::inspect(SRGB_PROFILE).unwrap()),
            color_conversion: conversion,
        }
    }

    #[test]
    fn image_platform_color_fixed_srgb_preserves_straight_alpha_and_gives_only_target_sidecar() {
        let original = pixel();
        let expected = original.bgra.clone();
        // 固定sRGB相等路径在生产转换器内只逐行复制，不调用原生CMM。
        let managed = manage(
            original,
            item(SampleModel::Ycc),
            Some(SRGB_PROFILE),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(managed.image.bgra, expected);
        assert_eq!(managed.image.readback_ms, 7.5);
        assert_eq!(
            managed.metadata.color_conversion,
            ColorConversion::ConvertedSrgb
        );
        assert_eq!(managed.metadata.source.color_model, ColorModel::Rgb);
        assert_eq!(managed.metadata.source.bits_per_channel, Some(8));
        assert_eq!(
            managed.metadata.source.orientation,
            Some(Orientation::FlipHorizontal)
        );
        assert_eq!(
            managed.metadata.source.exif.status,
            MetadataStatus::NotInspected
        );
        let sidecar = managed.metadata_source.unwrap();
        assert_eq!(sidecar.facts(), managed.metadata);
        assert_eq!(sidecar.retained_bytes(), 0);
        assert_eq!(sidecar.exif_summary(), None);
        assert!(managed.warning.is_none());
    }

    #[test]
    fn image_platform_color_rgb_icc_device_domain_is_separate_from_encoded_ycc_and_nclx() {
        for model in [SampleModel::Rgb, SampleModel::Ycc] {
            let mut selected = item(model);
            // ICC覆盖CP/TC，不能因容器与码流CP不同而把合法来源当损坏。
            selected.color.container_cicp.as_mut().unwrap().primaries = 12;
            let calls = Cell::new(0);
            let managed = manage_with(
                pixel(),
                selected,
                Some(SRGB_PROFILE),
                0,
                &AtomicBool::new(false),
                |image, source, profile, cancel| {
                    calls.set(calls.get() + 1);
                    assert_eq!(source.color_model, ColorModel::Rgb);
                    assert_eq!(profile, Some(SRGB_PROFILE));
                    image_color_management::manage(image, source, profile, cancel)
                },
            )
            .unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(selected.color.coded_color.model, model);
            assert_eq!(selected.color.container_cicp.unwrap().primaries, 12);
            assert_eq!(
                managed.metadata.color_conversion,
                ColorConversion::ConvertedSrgb
            );
            assert!(managed.metadata_source.is_some());
        }
    }

    #[test]
    fn image_platform_color_absent_icc_never_assumes_srgb_or_calls_a_transform() {
        for model in [
            SampleModel::Rgb,
            SampleModel::Ycc,
            SampleModel::Gray,
            SampleModel::Unknown,
        ] {
            let mut selected = item(model);
            selected.color.container_cicp = None;
            selected.color.coded_color.cicp = None;
            let original = pixel();
            let expected = original.bgra.clone();
            let managed = manage_with(
                original,
                selected,
                None,
                0,
                &AtomicBool::new(false),
                |_, _, _, _| panic!("absent ICC must not enter the manager"),
            )
            .unwrap();
            assert_eq!(managed.image.bgra, expected);
            assert_eq!(managed.metadata.source.icc.status, MetadataStatus::Absent);
            assert_eq!(managed.metadata.profile, None);
            assert_eq!(
                managed.metadata.color_conversion,
                ColorConversion::SignalUnmapped
            );
            assert!(managed.metadata_source.is_none());
            assert!(managed.warning.is_some());
        }
    }

    #[test]
    fn image_platform_color_hdr_from_either_header_and_precision_loss_do_not_gain_icc_qualification(
    ) {
        for (depth, container, bitstream) in [
            (8, Some(signal(16)), Some(signal(13))),
            (8, Some(signal(13)), Some(signal(18))),
            (8, None, Some(signal(16))),
            (8, Some(signal(18)), None),
            (10, Some(signal(13)), Some(signal(13))),
            (12, None, None),
            (16, Some(signal(13)), None),
        ] {
            let mut selected = item(SampleModel::Ycc);
            selected.coded.bits_per_channel = depth;
            selected.color.container_cicp = container;
            selected.color.coded_color.cicp = bitstream;
            let original = pixel();
            let expected = original.bgra.clone();
            let managed = manage_with(
                original,
                selected,
                Some(SRGB_PROFILE),
                0,
                &AtomicBool::new(false),
                |_, _, _, _| panic!("HDR or reduced precision must not enter the manager"),
            )
            .unwrap();
            assert_eq!(managed.image.bgra, expected);
            assert_eq!(managed.metadata.source.bits_per_channel, Some(depth));
            assert_eq!(managed.metadata.source.icc.status, MetadataStatus::Present);
            assert_eq!(
                managed.metadata.profile,
                Some(image_color_profile::inspect(SRGB_PROFILE).unwrap())
            );
            assert_eq!(
                managed.metadata.color_conversion,
                ColorConversion::SignalUnmapped
            );
            assert!(managed.metadata_source.is_none());
        }
        let mut selected = item(SampleModel::Rgb);
        selected.coded.bits_per_channel = 10;
        let managed = manage(
            pixel(),
            selected,
            Some(&[0; 132]),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(managed.metadata.profile, None);
        assert_eq!(managed.metadata.source.icc.status, MetadataStatus::Present);
        assert_eq!(
            managed.metadata.color_conversion,
            ColorConversion::SignalUnmapped
        );
        assert!(managed.warning.is_some());
    }

    #[test]
    fn image_platform_color_gray_unknown_and_non_rgb_profiles_keep_existing_guards() {
        for (model, expected) in [
            (SampleModel::Gray, ColorModel::Gray),
            (SampleModel::Unknown, ColorModel::Unknown),
        ] {
            let managed = manage(
                pixel(),
                item(model),
                Some(SRGB_PROFILE),
                0,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(managed.image.bgra, pixel().bgra);
            assert_eq!(managed.metadata.source.color_model, expected);
            assert_eq!(
                managed.metadata.color_conversion,
                ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource)
            );
            assert!(managed.metadata_source.is_none());
        }
        let mut cmyk_profile = SRGB_PROFILE.to_vec();
        cmyk_profile[16..20].copy_from_slice(b"CMYK");
        for (profile, reason) in [
            (cmyk_profile.as_slice(), UnappliedReason::UnsupportedProfile),
            (&[0; 132][..], UnappliedReason::InvalidProfile),
        ] {
            let managed = manage(
                pixel(),
                item(SampleModel::Ycc),
                Some(profile),
                0,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(managed.image.bgra, pixel().bgra);
            assert_eq!(
                managed.metadata.color_conversion,
                ColorConversion::EmbeddedUnapplied(reason)
            );
            assert!(managed.metadata_source.is_none());
        }
    }

    #[test]
    fn image_platform_color_manager_fallback_stays_complete_without_original_icc_sidecar() {
        let image = pixel();
        let pointer = image.bgra.as_ptr();
        let expected = image.bgra.clone();
        // 现有manager已负责丢弃失败的部分转换；本适配层不得另授raw ICC导出资格。
        let managed = manage_with(
            image,
            item(SampleModel::Ycc),
            Some(SRGB_PROFILE),
            0,
            &AtomicBool::new(false),
            |image, source, _, _| {
                Ok(ManagedImage {
                    image,
                    metadata: metadata(
                        source,
                        ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed),
                    ),
                    warning: Some("test transform failure".into()),
                })
            },
        )
        .unwrap();
        assert_eq!(managed.image.bgra.as_ptr(), pointer);
        assert_eq!(managed.image.bgra, expected);
        assert!(managed.warning.is_some());
        assert!(managed.metadata_source.is_none());
    }

    #[test]
    fn image_platform_color_cancel_has_priority_before_budget_and_after_manager_outcomes() {
        assert!(matches!(
            manage(
                pixel(),
                item(SampleModel::Ycc),
                Some(SRGB_PROFILE),
                u64::MAX,
                &AtomicBool::new(true)
            ),
            Err(PlatformError::Cancelled)
        ));
        for outcome in 0..3 {
            let cancel = AtomicBool::new(false);
            let result = manage_with(
                pixel(),
                item(SampleModel::Ycc),
                Some(SRGB_PROFILE),
                0,
                &cancel,
                |image, source, _, flag| {
                    flag.store(true, Ordering::Relaxed);
                    if outcome == 0 {
                        Err("canceled manager error".into())
                    } else {
                        Ok(ManagedImage {
                            image,
                            metadata: metadata(
                                source,
                                if outcome == 1 {
                                    ColorConversion::ConvertedSrgb
                                } else {
                                    ColorConversion::EmbeddedUnapplied(
                                        UnappliedReason::TransformFailed,
                                    )
                                },
                            ),
                            warning: None,
                        })
                    }
                },
            );
            assert!(matches!(result, Err(PlatformError::Cancelled)));
        }
    }

    #[test]
    fn image_platform_color_budget_accounts_for_capacity_and_runs_before_profile_processing() {
        let mut image = pixel();
        image.bgra.reserve_exact(1024);
        let work = image.bgra.capacity() as u64
            + 16
            + 12
            + SRGB_PROFILE.len() as u64 * 2
            + PROFILE_WORKSPACE;
        let exact = super::super::MAX_BYTES - work;
        assert!(check_budget(&image, SRGB_PROFILE.len(), exact).is_ok());
        assert!(matches!(
            check_budget(&image, SRGB_PROFILE.len(), exact + 1),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            check_budget(&image, 0, u64::MAX),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            check_budget(&image, image_color_profile::MAX_PROFILE_BYTES + 1, 0),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            manage_with(
                image,
                item(SampleModel::Ycc),
                Some(&[0; 132]),
                super::super::MAX_BYTES,
                &AtomicBool::new(false),
                |_, _, _, _| panic!("budget must precede profile handling")
            ),
            Err(PlatformError::Budget(_))
        ));
        let mut malformed = pixel();
        malformed.bgra.pop();
        assert!(matches!(
            manage(
                malformed,
                item(SampleModel::Ycc),
                Some(SRGB_PROFILE),
                0,
                &AtomicBool::new(false)
            ),
            Err(PlatformError::Invalid { .. })
        ));
    }
}
