//! 独占处理帧上的颜色调整；只改可见 RGB，不复制整帧或改动透明度。
use crate::{
    capture::Image,
    image_tools::{
        check_cancel, validate_input, validate_output, ToolError, ToolLimits, ToolResult,
    },
};
use image::Pixel;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ColorAdjustments {
    /// -100 压到中灰，0 原样，100 提高对比度；范围为 -100..=100。
    pub contrast: i16,
    /// -100 灰度，0 原样，100 将相对灰度的通道差加倍；范围为 -100..=100。
    pub saturation: i16,
}

impl ColorAdjustments {
    pub(crate) fn validate(self) -> ToolResult<()> {
        if !(-100..=100).contains(&self.contrast) || !(-100..=100).contains(&self.saturation) {
            return Err(ToolError::Invalid(
                tr!(
                    "对比度与饱和度须为 -100–100",
                    "Contrast and saturation must be -100–100"
                )
                .into(),
            ));
        }
        Ok(())
    }
}

/// 先对比度再饱和度，直接在编码后的 RGB 数值域运算，不进行线性光转换。
///
/// 对比度复用 image 的实现：系数为 ((100+c)/100)^2，以 127.5 为中心映射并夹断。
/// 灰度复用 image::Pixel::to_luma 的 Rec.709 权重；饱和度为
/// gray + (channel-gray)*(100+s)/100，四舍五入后夹到 0..=255。
/// alpha 原样保留，alpha 为零时连隐藏 RGB 也不修改。取消可能留下部分已处理行，
/// 调用者应丢弃该独占工作帧，不能将它作为完成结果发布。
pub(crate) fn apply(
    image: &mut Image,
    options: ColorAdjustments,
    limits: ToolLimits,
    cancel: &AtomicBool,
) -> ToolResult<()> {
    check_cancel(cancel)?;
    options.validate()?;
    validate_input(image, limits)?;
    validate_output(image.width, image.height, limits)?;
    if limits.max_items == 0 {
        return Err(ToolError::Invalid(
            tr!(
                "图片处理项数预算为空",
                "The image processing item limit is zero"
            )
            .into(),
        ));
    }
    if options == ColorAdjustments::default() {
        return check_cancel(cancel);
    }

    // 固定 256 字节查找表复用依赖算法；不将整张 BGRA 转成另一种像素容器。
    let mut contrast = std::array::from_fn::<_, 256, _>(|value| value as u8);
    if options.contrast != 0 {
        let mut ramp =
            image::ImageBuffer::<image::Luma<u8>, _>::from_raw(256, 1, contrast.as_mut_slice())
                .ok_or_else(|| {
                    ToolError::Failed(
                        tr!(
                            "无法建立颜色查找表",
                            "Could not create the color lookup table"
                        )
                        .into(),
                    )
                })?;
        image::imageops::colorops::contrast_in_place(&mut ramp, f32::from(options.contrast));
    }
    check_cancel(cancel)?;
    let saturation = i32::from(options.saturation) + 100;
    // 完整 BGRA 长度已验证，且高度至少为一，因此本行字节数不会溢出 usize。
    let stride = image.width as usize * 4;
    for row in image.bgra.chunks_exact_mut(stride) {
        check_cancel(cancel)?;
        for pixel in row.chunks_exact_mut(4) {
            if pixel[3] == 0 {
                continue;
            }
            let mut channels = [
                contrast[usize::from(pixel[0])],
                contrast[usize::from(pixel[1])],
                contrast[usize::from(pixel[2])],
            ];
            if options.saturation != 0 {
                let gray =
                    i32::from(image::Rgb([channels[2], channels[1], channels[0]]).to_luma()[0]);
                for channel in &mut channels {
                    let scaled = gray * 100 + (i32::from(*channel) - gray) * saturation;
                    *channel = ((scaled.clamp(0, 25_500) + 50) / 100) as u8;
                }
            }
            pixel[..3].copy_from_slice(&channels);
        }
    }
    check_cancel(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: &[[u8; 4]]) -> Image {
        Image {
            width: pixels.len() as u32,
            height: 1,
            bgra: pixels.iter().flatten().copied().collect(),
            readback_ms: 17.,
        }
    }

    fn adjusted(pixels: &[[u8; 4]], contrast: i16, saturation: i16) -> Vec<u8> {
        let mut image = image(pixels);
        apply(
            &mut image,
            ColorAdjustments {
                contrast,
                saturation,
            },
            ToolLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        image.bgra
    }

    #[test]
    fn image_tool_color_neutral_keeps_all_bgra_bytes_and_metadata() {
        let mut image = image(&[[7, 20, 250, 0], [19, 80, 140, 1], [127, 128, 255, 255]]);
        let original = image.bgra.clone();
        let storage = image.bgra.as_ptr();
        apply(
            &mut image,
            ColorAdjustments::default(),
            ToolLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(image.bgra, original);
        assert_eq!(image.bgra.as_ptr(), storage);
        assert_eq!((image.width, image.height, image.readback_ms), (3, 1, 17.));
    }

    #[test]
    fn image_tool_color_saturation_extremes_use_rgb_luma_and_clamp() {
        assert_eq!(
            adjusted(
                &[[0, 0, 255, 255], [0, 255, 0, 80], [255, 0, 0, 1]],
                0,
                -100,
            ),
            [54, 54, 54, 255, 182, 182, 182, 80, 18, 18, 18, 1]
        );
        assert_eq!(
            adjusted(&[[90, 100, 110, 255], [0, 128, 255, 50]], 0, 100),
            [79, 99, 119, 255, 0, 111, 255, 50]
        );
        for saturation in [-100, 100] {
            assert_eq!(
                adjusted(
                    &[[0, 0, 0, 1], [128, 128, 128, 50], [255; 4]],
                    0,
                    saturation
                ),
                [0, 0, 0, 1, 128, 128, 128, 50, 255, 255, 255, 255]
            );
        }
    }

    #[test]
    fn image_tool_color_contrast_extremes_and_processing_order_are_explicit() {
        assert_eq!(
            adjusted(&[[0, 127, 255, 128]], -100, 0),
            [127, 127, 127, 128]
        );
        assert_eq!(adjusted(&[[64, 127, 192, 16]], 100, 0), [0, 125, 255, 16]);
        // 对比度先得到 BGR(0, 0, 255)，再去饱和得到 54；顺序相反不会得到此值。
        assert_eq!(
            adjusted(&[[64, 64, 192, 255]], 100, -100),
            [54, 54, 54, 255]
        );
    }

    #[test]
    fn image_tool_color_alpha_and_fully_transparent_hidden_rgb_are_preserved() {
        let output = adjusted(
            &[
                [13, 29, 91, 0],
                [90, 100, 110, 1],
                [90, 100, 110, 128],
                [90, 100, 110, 255],
            ],
            40,
            60,
        );
        assert_eq!(&output[..4], [13, 29, 91, 0]);
        assert_eq!(
            output.chunks_exact(4).map(|p| p[3]).collect::<Vec<_>>(),
            [0, 1, 128, 255]
        );
        assert_eq!(&output[4..7], &output[8..11]);
        assert_eq!(&output[4..7], &output[12..15]);
        assert_ne!(&output[4..7], [90, 100, 110]);
    }

    #[test]
    fn image_tool_color_invalid_parameters_budgets_and_cancel_precede_mutation() {
        let source = [[20, 40, 60, 128], [80, 100, 120, 255]];
        let cancel = AtomicBool::new(false);
        for options in [
            ColorAdjustments {
                contrast: -101,
                saturation: 0,
            },
            ColorAdjustments {
                contrast: 101,
                saturation: 0,
            },
            ColorAdjustments {
                contrast: 0,
                saturation: i16::MIN,
            },
            ColorAdjustments {
                contrast: 0,
                saturation: i16::MAX,
            },
        ] {
            let mut frame = image(&source);
            let original = frame.bgra.clone();
            assert!(matches!(
                apply(&mut frame, options, ToolLimits::default(), &cancel),
                Err(ToolError::Invalid(_))
            ));
            assert_eq!(frame.bgra, original);
        }
        for limits in [
            ToolLimits {
                max_input_pixels: 1,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_output_pixels: 1,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_side: 1,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_items: 0,
                ..ToolLimits::default()
            },
        ] {
            let mut frame = image(&source);
            let original = frame.bgra.clone();
            assert!(matches!(
                apply(&mut frame, ColorAdjustments::default(), limits, &cancel),
                Err(ToolError::Invalid(_))
            ));
            assert_eq!(frame.bgra, original);
        }
        for zero_width in [false, true] {
            let mut frame = image(&source);
            if zero_width {
                frame.width = 0;
            } else {
                frame.bgra.pop();
            }
            let original = frame.bgra.clone();
            assert!(matches!(
                apply(
                    &mut frame,
                    ColorAdjustments {
                        contrast: 20,
                        saturation: 0
                    },
                    ToolLimits::default(),
                    &cancel
                ),
                Err(ToolError::Invalid(_))
            ));
            assert_eq!(frame.bgra, original);
        }
        let mut frame = image(&source);
        let original = frame.bgra.clone();
        assert_eq!(
            apply(
                &mut frame,
                ColorAdjustments {
                    contrast: 20,
                    saturation: 20
                },
                ToolLimits::default(),
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        );
        assert_eq!(frame.bgra, original);
    }
}
