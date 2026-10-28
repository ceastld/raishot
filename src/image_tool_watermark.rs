//! 在独占处理帧上复用 Scene 的文字水印；不创建整幅透明叠图或另一套字形算法。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    image_tools::{check_cancel, validate_input, ToolError, ToolLimits, ToolResult},
    output_projection::OutputProjection,
    scene::{PixelPlane, Scene, SourcePixels},
};
use std::sync::atomic::AtomicBool;

const MAX_TEXT_CHARS: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WatermarkOptions {
    pub text: String,
    pub font_size: f32,
    /// 0xRRGGBB；透明度单独沿用既有 Style.opacity。
    pub color: u32,
    pub opacity: f32,
}

impl Default for WatermarkOptions {
    fn default() -> Self {
        Self {
            text: String::new(),
            font_size: 28.,
            color: 0xffffff,
            opacity: 0.3,
        }
    }
}

impl WatermarkOptions {
    /// 长度先检查原输入，再沿用单行规范化；不能让规范化器静默截断过长内容。
    pub(crate) fn validate(&self) -> ToolResult<()> {
        if self.text.chars().take(MAX_TEXT_CHARS + 1).count() > MAX_TEXT_CHARS {
            return Err(ToolError::Invalid(
                tr!(
                    "水印文字不能超过 4096 个字符",
                    "Watermark text must not exceed 4096 characters"
                )
                .into(),
            ));
        }
        if crate::watermark_input::single_line(&self.text)
            .trim()
            .is_empty()
        {
            return Err(ToolError::Invalid(
                tr!(
                    "启用水印时请输入文字",
                    "Enter text when the watermark is enabled"
                )
                .into(),
            ));
        }
        if !self.font_size.is_finite()
            || !(1. ..=256.).contains(&self.font_size)
            || self.color > 0xffffff
            || !self.opacity.is_finite()
            || !(0. ..=1.).contains(&self.opacity)
        {
            return Err(ToolError::Invalid(
                tr!(
                    "水印字号须为 1–256，颜色须为 RGB，不透明度须为 0–100%",
                    "Watermark size must be 1–256, color must be RGB, and opacity must be 0–100%"
                )
                .into(),
            ));
        }
        Ok(())
    }

    fn annotation(&self, canvas: Rect) -> Annotation {
        let mut annotation = Annotation::new(
            Tool::Watermark,
            Point {
                x: canvas.x as f32,
                y: canvas.y as f32,
            },
            Style {
                color: self.color,
                font_size: self.font_size,
                opacity: self.opacity,
                ..Style::default()
            },
        );
        annotation.points.push(Point {
            x: (canvas.x + canvas.width) as f32,
            y: (canvas.y + canvas.height) as f32,
        });
        annotation.text = crate::watermark_input::single_line(&self.text);
        annotation
    }
}

/// 目标已经是最终尺寸：使用输出预算验证，不能套用原图的较小输入预算。
/// 既有 GDI 绘制与全局字体锁等待不可中断；前后检查取消，错误时调用方须丢弃工作帧。
pub(crate) fn apply(
    target: &mut Image,
    options: &WatermarkOptions,
    limits: ToolLimits,
    cancel: &AtomicBool,
) -> ToolResult<()> {
    check_cancel(cancel)?;
    options.validate()?;
    validate_input(
        target,
        ToolLimits {
            max_input_pixels: limits.max_output_pixels,
            ..limits
        },
    )?;
    let canvas = Rect::full(target.width, target.height);
    // 同用户导出保持 32MP/65535 单边硬上限；Scene 的恒等投影本身不替调用者做预算检查。
    OutputProjection::new(canvas, 1.).map_err(|error| ToolError::Invalid(error.to_string()))?;
    if options.opacity == 0. {
        return check_cancel(cancel);
    }
    let annotation = options.annotation(canvas);
    let scene = {
        let source = SourcePixels {
            canvas,
            fill: None,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &target.bgra,
            }],
            overlay: None,
        };
        Scene::default().updated(std::slice::from_ref(&annotation), &source)
    };
    check_cancel(cancel)?;
    scene
        .try_apply(target, canvas)
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    check_cancel(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: [91, 53, 27, 96].repeat(4),
            readback_ms: 0.,
        }
    }

    #[test]
    fn image_tool_watermark_invalid_and_cancelled_requests_do_not_touch_pixels() {
        let mut target = frame();
        let original = target.bgra.clone();
        let valid = WatermarkOptions {
            text: "Mark".into(),
            ..WatermarkOptions::default()
        };
        for options in [
            WatermarkOptions {
                text: "字".repeat(4097),
                ..valid.clone()
            },
            WatermarkOptions {
                text: " \r\n\t".into(),
                ..valid.clone()
            },
            WatermarkOptions {
                font_size: f32::NAN,
                ..valid.clone()
            },
            WatermarkOptions {
                opacity: 1.01,
                ..valid.clone()
            },
            WatermarkOptions {
                color: 0x1000000,
                ..valid.clone()
            },
        ] {
            assert!(apply(
                &mut target,
                &options,
                ToolLimits::default(),
                &AtomicBool::new(false)
            )
            .is_err());
            assert_eq!(target.bgra, original);
        }
        assert!(matches!(
            apply(
                &mut target,
                &valid,
                ToolLimits::default(),
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(target.bgra, original);
    }

    #[test]
    fn image_tool_watermark_zero_opacity_checks_output_budget_without_font_resources() {
        let mut target = frame();
        let original = target.bgra.clone();
        let options = WatermarkOptions {
            text: "字".repeat(4096),
            opacity: 0.,
            ..WatermarkOptions::default()
        };
        let limits = ToolLimits {
            max_input_pixels: 1,
            max_output_pixels: 4,
            ..ToolLimits::default()
        };
        apply(&mut target, &options, limits, &AtomicBool::new(false)).unwrap();
        assert_eq!(target.bgra, original);
        assert!(apply(
            &mut target,
            &options,
            ToolLimits {
                max_output_pixels: 3,
                ..limits
            },
            &AtomicBool::new(false)
        )
        .is_err());
        assert_eq!(target.bgra, original);
    }

    #[test]
    #[ignore = "需要 Windows GDI 字体，集成时显式验证"]
    fn image_tool_watermark_pixels_match_existing_scene_and_preserve_source_alpha_contract() {
        let (width, height) = (320, 192);
        let original = [37, 71, 109, 96].repeat(width as usize * height as usize);
        let mut actual = Image {
            width,
            height,
            bgra: original.clone(),
            readback_ms: 0.,
        };
        let mut expected = Image {
            width,
            height,
            bgra: original.clone(),
            readback_ms: 0.,
        };
        let options = WatermarkOptions {
            text: "Mark\r\nA".into(),
            font_size: 20.,
            color: 0xc7357c,
            opacity: 0.5,
        };
        apply(
            &mut actual,
            &options,
            ToolLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();

        let canvas = Rect::full(width, height);
        let mut annotation = Annotation::new(
            Tool::Watermark,
            Point { x: 0., y: 0. },
            Style {
                font_size: 20.,
                color: 0xc7357c,
                opacity: 0.5,
                ..Style::default()
            },
        );
        annotation.points.push(Point {
            x: width as f32,
            y: height as f32,
        });
        annotation.text = "Mark A".into();
        let scene = {
            let source = SourcePixels {
                canvas,
                fill: None,
                planes: vec![PixelPlane {
                    rect: canvas,
                    bgra: &original,
                }],
                overlay: None,
            };
            Scene::default().updated(&[annotation], &source)
        };
        scene.try_apply(&mut expected, canvas).unwrap();
        assert_eq!(actual.bgra, expected.bgra);
        assert_ne!(actual.bgra, original);
        assert!(actual.bgra.chunks_exact(4).all(|pixel| pixel[3] >= 96));
        assert!(actual
            .bgra
            .chunks_exact(4)
            .any(|pixel| pixel == [37, 71, 109, 96]));
        assert!(original
            .chunks_exact(4)
            .all(|pixel| pixel == [37, 71, 109, 96]));
    }
}
