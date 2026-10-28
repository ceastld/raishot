//! 内部单图变换：复用绝对调整参数，先调整再缩放，预览与保存使用同一产物。
#[path = "image_tool_color.rs"]
pub(crate) mod color;
#[path = "image_tool_sharpen.rs"]
pub(crate) mod sharpen;
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tools::{
        check_cancel, validate_input, validate_output, ProcessedImage, ToolError, ToolLimits,
        ToolResult,
    },
    pin_model::Adjustments,
};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResizeOptions {
    /// 最终物理像素尺寸；锁定宽高比时由调用方先计算成明确的宽高。
    pub width: u32,
    pub height: u32,
    pub filter: ResizeFilter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TransformOptions {
    pub adjustments: Adjustments,
    pub resize: Option<ResizeOptions>,
}

/// 不持有输入像素；调用方更换源或参数时递增 request_id 并取消旧请求。
pub(crate) struct TransformPlan {
    source_size: (u32, u32),
    output_size: (u32, u32),
    options: TransformOptions,
    limits: ToolLimits,
    request_id: u64,
}

impl TransformPlan {
    pub(crate) fn new(
        source: &Image,
        options: TransformOptions,
        limits: ToolLimits,
        request_id: u64,
    ) -> ToolResult<Self> {
        validate_input(source, limits)?;
        if limits.max_items == 0 {
            return Err(ToolError::Invalid(
                tr!(
                    "图片处理项数预算为空",
                    "The image processing item limit is zero"
                )
                .into(),
            ));
        }
        if options.adjustments.turns > 3 || !(-100..=100).contains(&options.adjustments.brightness)
        {
            return Err(ToolError::Invalid(
                tr!(
                    "旋转须为 0–3 个直角，亮度须为 -100–100",
                    "Rotation must be 0–3 quarter turns and brightness must be -100–100"
                )
                .into(),
            ));
        }
        // 既有调整算法用 u32 计算字节索引；自定义预算也不能绕过其索引范围。
        if options.adjustments != Adjustments::default()
            && source.bgra.len() as u64 > u64::from(u32::MAX)
        {
            return Err(ToolError::Invalid(
                tr!(
                    "图片超过调整算法的像素索引范围",
                    "Image exceeds the adjustment pixel index range"
                )
                .into(),
            ));
        }
        let adjusted_size = options.adjustments.dimensions(source.width, source.height);
        let output_size = options
            .resize
            .map_or(adjusted_size, |resize| (resize.width, resize.height));
        // 输入预算同时限制既有调整算法的临时结果；缩放前不会创建更大的中间图。
        // 输出在调整前检查，错误尺寸不能先消耗一次完整图像处理。
        validate_output(output_size.0, output_size.1, limits)?;
        Ok(Self {
            source_size: (source.width, source.height),
            output_size,
            options,
            limits,
            request_id,
        })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.output_size
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 调整阶段复用既有算法，在前后响应取消；缩放阶段逐行响应取消。
    /// 调整后的中间结果只在本调用内持有，不改写源图或贴图文档。
    pub(crate) fn render(&self, source: &Image, cancel: &AtomicBool) -> ToolResult<ProcessedImage> {
        self.render_with_color(source, color::ColorAdjustments::default(), cancel)
    }

    /// 在几何和尺寸确定后调整颜色；旧调用保持默认颜色，不改变既有参数结构。
    pub(crate) fn render_with_color(
        &self,
        source: &Image,
        colors: color::ColorAdjustments,
        cancel: &AtomicBool,
    ) -> ToolResult<ProcessedImage> {
        self.render_with_effects(source, colors, sharpen::SharpenOptions::default(), cancel)
    }

    /// 颜色后在最终尺寸上锐化；独立效果参数不改变旧几何参数与调用方式。
    pub(crate) fn render_with_effects(
        &self,
        source: &Image,
        colors: color::ColorAdjustments,
        sharpening: sharpen::SharpenOptions,
        cancel: &AtomicBool,
    ) -> ToolResult<ProcessedImage> {
        check_cancel(cancel)?;
        colors.validate()?;
        sharpening.validate()?;
        validate_input(source, self.limits)?;
        if (source.width, source.height) != self.source_size {
            return Err(ToolError::Invalid(
                tr!(
                    "原图尺寸已变化，请重新建立处理计划",
                    "Source dimensions changed; create a new transform plan"
                )
                .into(),
            ));
        }
        let filter = self
            .options
            .resize
            .map_or(ResizeFilter::Nearest, |resize| resize.filter);
        let mut image = if self.options.adjustments == Adjustments::default() {
            // 同尺寸时缩放器只复制原字节，避免无变化时经过颜色或 alpha 运算。
            resize(
                source,
                self.output_size.0,
                self.output_size.1,
                filter,
                self.limits,
                cancel,
            )?
        } else {
            let adjusted = self
                .options
                .adjustments
                .apply(source)
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            check_cancel(cancel)?;
            if (adjusted.width, adjusted.height) == self.output_size {
                adjusted
            } else {
                resize(
                    &adjusted,
                    self.output_size.0,
                    self.output_size.1,
                    filter,
                    self.limits,
                    cancel,
                )?
            }
        };
        check_cancel(cancel)?;
        color::apply(
            &mut image,
            colors,
            ToolLimits {
                max_input_pixels: self.limits.max_output_pixels,
                ..self.limits
            },
            cancel,
        )?;
        sharpen::apply(&mut image, sharpening, self.limits, cancel)?;
        Ok(ProcessedImage::new(image, self.request_id))
    }
}

pub(crate) fn process(
    source: &Image,
    options: TransformOptions,
    limits: ToolLimits,
    request_id: u64,
    cancel: &AtomicBool,
) -> ToolResult<ProcessedImage> {
    check_cancel(cancel)?;
    TransformPlan::new(source, options, limits, request_id)?.render(source, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_transform_sharpen_uses_final_size_and_keeps_default_call_unchanged() {
        let source = Image {
            width: 2,
            height: 1,
            bgra: vec![64, 64, 64, 128, 192, 192, 192, 128],
            readback_ms: 0.,
        };
        let original = source.bgra.clone();
        let plan = TransformPlan::new(
            &source,
            TransformOptions {
                adjustments: Adjustments::default(),
                resize: Some(ResizeOptions {
                    width: 10,
                    height: 1,
                    filter: ResizeFilter::Nearest,
                }),
            },
            ToolLimits {
                max_input_pixels: 2,
                max_output_pixels: 10,
                ..ToolLimits::default()
            },
            44,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let normal = plan.render(&source, &cancel).unwrap();
        assert_eq!(normal.image().bgra[4 * 4], 64);
        assert_eq!(normal.image().bgra[5 * 4], 192);
        let sharp = plan
            .render_with_effects(
                &source,
                color::ColorAdjustments::default(),
                sharpen::SharpenOptions {
                    amount_percent: 100,
                    threshold: 0,
                },
                &cancel,
            )
            .unwrap();
        assert_eq!(
            (
                sharp.image().width,
                sharp.image().height,
                sharp.request_id()
            ),
            (10, 1, 44)
        );
        assert!(sharp.image().bgra[4 * 4] < 64);
        assert!(sharp.image().bgra[5 * 4] > 192);
        assert!(sharp
            .image()
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 128));
        assert_eq!(source.bgra, original);
    }

    #[test]
    fn image_transform_color_adjustments_accept_valid_enlarged_output_and_preserve_alpha() {
        let source = Image {
            width: 1,
            height: 1,
            bgra: vec![0, 0, 255, 128],
            readback_ms: 0.,
        };
        let limits = ToolLimits {
            max_input_pixels: 1,
            max_output_pixels: 4,
            ..Default::default()
        };
        let plan = TransformPlan::new(
            &source,
            TransformOptions {
                adjustments: Adjustments::default(),
                resize: Some(ResizeOptions {
                    width: 2,
                    height: 2,
                    filter: ResizeFilter::Bilinear,
                }),
            },
            limits,
            17,
        )
        .unwrap();
        let result = plan
            .render_with_color(
                &source,
                color::ColorAdjustments {
                    contrast: 0,
                    saturation: -100,
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(result.request_id(), 17);
        assert_eq!((result.image().width, result.image().height), (2, 2));
        for pixel in result.image().bgra.chunks_exact(4) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 128);
        }
        assert_eq!(source.bgra, [0, 0, 255, 128]);
    }

    fn source() -> Image {
        Image {
            width: 3,
            height: 2,
            bgra: vec![
                1, 10, 100, 0, 2, 20, 110, 40, 3, 30, 120, 80, 4, 40, 130, 120, 5, 50, 140, 160, 6,
                60, 150, 255,
            ],
            readback_ms: 0.,
        }
    }

    #[test]
    fn image_transform_rotation_then_flip_matches_existing_geometry_and_keeps_source() {
        let source = source();
        let before = source.bgra.clone();
        let result = process(
            &source,
            TransformOptions {
                adjustments: Adjustments {
                    turns: 1,
                    flip_x: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            ToolLimits::default(),
            22,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.request_id(), 22);
        assert_eq!((result.image().width, result.image().height), (2, 3));
        // 顺时针转动后水平镜像，把原来每一列变为从上到下的一行。
        let blue: Vec<_> = result.image().bgra.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(blue, [1, 4, 2, 5, 3, 6]);
        let alpha: Vec<_> = result.image().bgra.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(alpha, [0, 120, 40, 160, 80, 255]);
        assert_eq!(source.bgra, before);
    }

    #[test]
    fn image_transform_rotation_is_applied_before_explicit_resize() {
        let source = source();
        let options = TransformOptions {
            adjustments: Adjustments {
                turns: 1,
                ..Default::default()
            },
            resize: Some(ResizeOptions {
                width: 1,
                height: 3,
                filter: ResizeFilter::Nearest,
            }),
        };
        let plan = TransformPlan::new(&source, options, ToolLimits::default(), 23).unwrap();
        assert_eq!(plan.size(), (1, 3));
        assert_eq!(plan.request_id(), 23);
        let result = plan.render(&source, &AtomicBool::new(false)).unwrap();
        let blue: Vec<_> = result.image().bgra.chunks_exact(4).map(|p| p[0]).collect();
        // 最近邻像素中心映射选转后图的右列，即原图上行。
        assert_eq!(blue, [1, 2, 3]);
    }

    #[test]
    fn image_transform_color_adjustments_share_existing_pixel_behavior() {
        let source = Image {
            width: 1,
            height: 1,
            bgra: vec![10, 20, 30, 77],
            readback_ms: 0.,
        };
        let result = process(
            &source,
            TransformOptions {
                adjustments: Adjustments {
                    grayscale: true,
                    invert: true,
                    brightness: -10,
                    ..Default::default()
                },
                ..Default::default()
            },
            ToolLimits::default(),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        // 灰度22，反色233，再减25亮度；alpha始终保持。
        assert_eq!(result.image().bgra, [208, 208, 208, 77]);
        assert_eq!(source.bgra, [10, 20, 30, 77]);
    }

    #[test]
    fn image_transform_no_change_preserves_hidden_rgb_exactly() {
        let source = source();
        let result = process(
            &source,
            TransformOptions::default(),
            ToolLimits::default(),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.image().bgra, source.bgra);
    }

    #[test]
    fn image_transform_rejects_parameters_and_output_budget_before_processing() {
        let source = source();
        for adjustments in [
            Adjustments {
                turns: 4,
                ..Default::default()
            },
            Adjustments {
                brightness: 101,
                ..Default::default()
            },
            Adjustments {
                brightness: i16::MIN,
                ..Default::default()
            },
        ] {
            assert!(TransformPlan::new(
                &source,
                TransformOptions {
                    adjustments,
                    ..Default::default()
                },
                ToolLimits::default(),
                0
            )
            .is_err());
        }
        for size in [(0, 2), (2, 0), (u32::MAX, 2)] {
            assert!(TransformPlan::new(
                &source,
                TransformOptions {
                    resize: Some(ResizeOptions {
                        width: size.0,
                        height: size.1,
                        filter: ResizeFilter::Bilinear
                    }),
                    ..Default::default()
                },
                ToolLimits::default(),
                0
            )
            .is_err());
        }
        let limits = ToolLimits {
            max_output_pixels: 5,
            ..ToolLimits::default()
        };
        assert!(TransformPlan::new(&source, TransformOptions::default(), limits, 0).is_err());
        assert!(TransformPlan::new(
            &source,
            TransformOptions::default(),
            ToolLimits {
                max_items: 0,
                ..ToolLimits::default()
            },
            0
        )
        .is_err());
    }

    #[test]
    fn image_transform_cancellation_and_changed_source_return_no_result() {
        let source = source();
        assert!(matches!(
            process(
                &source,
                TransformOptions::default(),
                ToolLimits::default(),
                0,
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
        let plan = TransformPlan::new(
            &source,
            TransformOptions::default(),
            ToolLimits::default(),
            0,
        )
        .unwrap();
        let mut changed = source;
        changed.width = 2;
        changed.height = 3;
        assert!(plan.render(&changed, &AtomicBool::new(false)).is_err());
        changed.bgra.pop();
        assert!(plan.render(&changed, &AtomicBool::new(false)).is_err());
    }
}
