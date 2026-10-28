//! 显式录屏缩放请求的纯解析；不改变源物理区域，不探测编码器或开放界面。
use crate::{
    geometry::ScreenRect, media_foundation::frame_resize::FrameResizeGeometry,
    recording_options::Preset, recording_size_layout::RecordingSizeLayout, Result,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResizeRequest {
    pub target_width: u32,
    pub target_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ScalePlan {
    pub source: RecordingSizeLayout,
    pub geometry: FrameResizeGeometry,
}

pub(crate) fn resolve(
    region: ScreenRect,
    preset: Preset,
    request: Option<ResizeRequest>,
    experimental: bool,
) -> Result<Option<ScalePlan>> {
    // None 也校验真实采集区域；不因没有缩放请求绕过原坐标或像素预算。
    let source = RecordingSizeLayout::new(region)?;
    let Some(request) = request else {
        return Ok(None);
    };
    if !experimental {
        return Err(tr!(
            "录屏缩放请求仅在实验模式下可用",
            "Recording resize requests require experimental mode"
        )
        .into());
    }
    if preset.is_gif() {
        return Err(tr!(
            "显式录屏缩放请求暂不支持 GIF",
            "Explicit recording resize requests do not support GIF yet"
        )
        .into());
    }
    let geometry = FrameResizeGeometry::new(
        source.encoded_width,
        source.encoded_height,
        request.target_width,
        request.target_height,
    )?;
    // 同尺寸仍是显式请求；源布局的裁边和物理原点独立保留。
    Ok(Some(ScalePlan { source, geometry }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> ScreenRect {
        ScreenRect {
            left: -100,
            top: -50,
            width: 385,
            height: 257,
        }
    }

    #[test]
    fn recording_scale_plan_default_none_preserves_old_modes_and_validates_source() {
        let region = region();
        for preset in Preset::ALL {
            for experimental in [false, true] {
                assert_eq!(resolve(region, preset, None, experimental).unwrap(), None);
            }
        }
        assert!(resolve(
            ScreenRect { width: 0, ..region },
            Preset::CompactMp4,
            None,
            false,
        )
        .is_err());
        assert_eq!(
            (region.left, region.top, region.width, region.height),
            (-100, -50, 385, 257)
        );
    }

    #[test]
    fn recording_scale_plan_explicit_keeps_source_roi_and_exact_target() {
        let region = region();
        let request = ResizeRequest {
            target_width: 192,
            target_height: 128,
        };
        let original_request = request;
        let plan = resolve(region, Preset::CompactMp4, Some(request), true)
            .unwrap()
            .unwrap();
        assert_eq!(plan.source, RecordingSizeLayout::new(region).unwrap());
        assert_eq!((plan.source.left, plan.source.top), (-100, -50));
        assert_eq!(
            (plan.source.source_width, plan.source.source_height),
            (385, 257)
        );
        assert_eq!(
            (plan.source.encoded_width, plan.source.encoded_height),
            (384, 256)
        );
        assert_eq!(
            plan.geometry,
            FrameResizeGeometry {
                source_width: 384,
                source_height: 256,
                target_width: 192,
                target_height: 128,
            }
        );
        assert_eq!(request, original_request);
        let serialized = serde_json::to_value(plan).unwrap();
        assert_eq!(serialized["source"]["left"], -100);
        assert_eq!(serialized["geometry"]["target_width"], 192);
    }

    #[test]
    fn recording_scale_plan_explicit_identity_remains_a_request() {
        let region = region();
        let request = ResizeRequest {
            target_width: 384,
            target_height: 256,
        };
        let plan = resolve(region, Preset::HighQualityMp4, Some(request), true)
            .unwrap()
            .unwrap();
        assert!(plan.geometry.is_identity());
        assert_eq!(plan.source.cropped_right(), 1);
        assert_eq!(plan.source.cropped_bottom(), 1);
        assert!(resolve(region, Preset::HighQualityMp4, None, true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn recording_scale_plan_rejects_gates_bad_targets_and_invalid_sources() {
        let region = region();
        let request = ResizeRequest {
            target_width: 192,
            target_height: 128,
        };
        assert!(resolve(region, Preset::CompactMp4, Some(request), false).is_err());
        assert!(resolve(region, Preset::Gif, Some(request), true).is_err());
        for (target_width, target_height) in [
            (0, 128),
            (1, 128),
            (193, 128),
            (192, 129),
            (386, 128),
            (192, 258),
            (u32::MAX, 128),
        ] {
            assert!(
                resolve(
                    region,
                    Preset::CompactMp4,
                    Some(ResizeRequest {
                        target_width,
                        target_height,
                    }),
                    true
                )
                .is_err(),
                "target={target_width}x{target_height}"
            );
        }
        for source in [
            ScreenRect {
                height: 1,
                ..region
            },
            ScreenRect {
                left: i32::MAX,
                ..region
            },
            ScreenRect {
                width: 7680,
                height: 4320,
                ..region
            },
        ] {
            assert!(resolve(source, Preset::CompactMp4, Some(request), true).is_err());
        }
        assert_eq!(
            request,
            ResizeRequest {
                target_width: 192,
                target_height: 128
            }
        );
    }
}
