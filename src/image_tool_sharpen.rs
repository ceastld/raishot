//! 有界分块锐化；复用固定高斯核，保留原 alpha 和全透明像素的隐藏 RGB。
use crate::{
    capture::Image,
    image_tools::{
        allocate_pixels, check_cancel, validate_input, validate_output, ToolError, ToolLimits,
        ToolResult,
    },
};
use image::{imageops::GaussianBlurParameters, GenericImageView, Rgba};
use std::sync::atomic::AtomicBool;

const TILE_SIDE: u32 = 256;
const HALO: u32 = 2;
const MAX_SCRATCH_BYTES: usize = 160 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SharpenOptions {
    /// 0 不处理，100 使用一倍原色与模糊色之差；范围为 0..=200。
    pub amount_percent: u16,
    /// 每个 RGB 通道的差值超过此阈值才增强，单位为 8 位通道值。
    pub threshold: u8,
}

impl SharpenOptions {
    pub(crate) fn validate(self) -> ToolResult<()> {
        if self.amount_percent > 200 {
            return Err(ToolError::Invalid(
                tr!("锐化强度须为 0–200%", "Sharpen amount must be 0–200%").into(),
            ));
        }
        Ok(())
    }
}

fn scratch_bytes(width: u32, height: u32, output_bytes: usize) -> ToolResult<usize> {
    let tile_width = width.min(TILE_SIDE + HALO * 2) as usize;
    let tile_height = height.min(TILE_SIDE + HALO * 2) as usize;
    // 锁定的固定小核路径持有两张 f32 瞬态、一张 f32 成品和少量行缓冲。
    // 每像素保守预留四张 RGBA f32 缓冲，另留 64 KiB 给核、环形行缓冲和元数据。
    let bytes = tile_width
        .checked_mul(tile_height)
        .and_then(|pixels| pixels.checked_mul(64))
        .and_then(|tiles| tiles.checked_add(64 * 1024))
        .and_then(|tiles| tiles.checked_add(output_bytes))
        .filter(|bytes| *bytes <= MAX_SCRATCH_BYTES)
        .ok_or_else(|| {
            ToolError::Invalid(
                tr!(
                    "图片超过锐化临时内存预算（160 MiB）",
                    "Image exceeds the 160 MiB sharpening scratch limit"
                )
                .into(),
            )
        })?;
    Ok(bytes)
}

/// 原帧只读到任务成功；新像素全部生成并检查取消后才替换。
/// 在编码 RGB 上锐化，不进行线性光转换。高斯模糊使用归一化预乘 RGBA，
/// 消除隐藏 RGB 对相邻像素的影响；最终只增强 RGB，alpha 永远取原值。
pub(crate) fn apply(
    image: &mut Image,
    options: SharpenOptions,
    limits: ToolLimits,
    cancel: &AtomicBool,
) -> ToolResult<()> {
    check_cancel(cancel)?;
    options.validate()?;
    // 此处输入已经是变换后的最终尺寸，不能再受变换前的输入像素预算限制。
    validate_input(
        image,
        ToolLimits {
            max_input_pixels: limits.max_output_pixels,
            ..limits
        },
    )?;
    let bytes = validate_output(image.width, image.height, limits)?;
    if limits.max_items == 0 {
        return Err(ToolError::Invalid(
            tr!(
                "图片处理项数预算为空",
                "The image processing item limit is zero"
            )
            .into(),
        ));
    }
    if options.amount_percent == 0 {
        return check_cancel(cancel);
    }
    scratch_bytes(image.width, image.height, bytes)?;
    let mut output = allocate_pixels(bytes)?;
    check_cancel(cancel)?;
    let stride = image.width as usize * 4;
    for top in (0..image.height).step_by(TILE_SIDE as usize) {
        let bottom = top.saturating_add(TILE_SIDE).min(image.height);
        for left in (0..image.width).step_by(TILE_SIDE as usize) {
            check_cancel(cancel)?;
            let right = left.saturating_add(TILE_SIDE).min(image.width);
            let view = PremultTile {
                source: image,
                left: left.saturating_sub(HALO),
                top: top.saturating_sub(HALO),
                right: right.saturating_add(HALO).min(image.width),
                bottom: bottom.saturating_add(HALO).min(image.height),
            };
            // 单个有界小块的调库过程不可中断；外围每块及合成每行均检查取消。
            let blurred =
                image::imageops::blur_advanced(&view, GaussianBlurParameters::SMOOTHING_5);
            check_cancel(cancel)?;
            for y in top..bottom {
                check_cancel(cancel)?;
                for x in left..right {
                    let offset = y as usize * stride + x as usize * 4;
                    let source = &image.bgra[offset..offset + 4];
                    let blurred = blurred.get_pixel(x - view.left, y - view.top);
                    sharpen_pixel(source, blurred.0, options, &mut output[offset..offset + 4])?;
                }
            }
        }
    }
    check_cancel(cancel)?;
    std::mem::swap(&mut image.bgra, &mut output);
    Ok(())
}

/// 只转换实际裁块内的源像素；通道顺序为 RGBA，所有通道归一到 0..=1。
struct PremultTile<'a> {
    source: &'a Image,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl GenericImageView for PremultTile<'_> {
    type Pixel = Rgba<f32>;

    fn dimensions(&self) -> (u32, u32) {
        (self.right - self.left, self.bottom - self.top)
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        assert!(x < self.width() && y < self.height());
        let offset =
            ((self.top + y) as usize * self.source.width as usize + (self.left + x) as usize) * 4;
        let pixel = &self.source.bgra[offset..offset + 4];
        let alpha = f32::from(pixel[3]) / 255.;
        Rgba([
            f32::from(pixel[2]) / 255. * alpha,
            f32::from(pixel[1]) / 255. * alpha,
            f32::from(pixel[0]) / 255. * alpha,
            alpha,
        ])
    }
}

fn sharpen_pixel(
    source: &[u8],
    blurred: [f32; 4],
    options: SharpenOptions,
    output: &mut [u8],
) -> ToolResult<()> {
    output.copy_from_slice(source);
    if source[3] == 0 {
        return Ok(());
    }
    // 正核包含当前非透明像素，因此分母必为正；检查异常库结果后才继续计算。
    if !blurred.iter().all(|value| value.is_finite()) || blurred[3] <= 0. {
        return Err(ToolError::Failed(
            tr!("锐化模糊结果无效", "Invalid sharpening blur result").into(),
        ));
    }
    let amount = f32::from(options.amount_percent) / 100.;
    for (destination, blurred_channel) in [2, 1, 0].into_iter().enumerate() {
        let original = f32::from(source[destination]);
        let soft = (blurred[blurred_channel] / blurred[3]).clamp(0., 1.) * 255.;
        let difference = original - soft;
        if difference.abs() > f32::from(options.threshold) {
            output[destination] = (original + amount * difference).round().clamp(0., 255.) as u8;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 4]) -> Image {
        let mut bgra = Vec::new();
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&pixel(x, y));
            }
        }
        Image {
            width,
            height,
            bgra,
            readback_ms: 17.,
        }
    }

    fn options(amount_percent: u16, threshold: u8) -> SharpenOptions {
        SharpenOptions {
            amount_percent,
            threshold,
        }
    }

    #[test]
    fn image_tool_sharpen_noop_preflight_and_cancel_preserve_working_frame() {
        let mut image = image(2, 1, |x, _| [30, x as u8, 90, (x * 128) as u8]);
        let original = image.bgra.clone();
        let limits = ToolLimits {
            max_input_pixels: 1,
            max_output_pixels: 2,
            ..ToolLimits::default()
        };
        let cancel = AtomicBool::new(false);
        apply(&mut image, SharpenOptions::default(), limits, &cancel).unwrap();
        assert_eq!(image.bgra, original);
        assert!(apply(&mut image, options(201, 0), limits, &cancel).is_err());
        assert_eq!(image.bgra, original);
        assert!(apply(
            &mut image,
            options(100, 0),
            ToolLimits {
                max_output_pixels: 1,
                ..limits
            },
            &cancel
        )
        .is_err());
        assert_eq!(image.bgra, original);
        assert!(matches!(
            apply(&mut image, options(100, 0), limits, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert_eq!(image.bgra, original);
        image.bgra.pop();
        assert!(apply(&mut image, SharpenOptions::default(), limits, &cancel).is_err());

        let expanded = ToolLimits {
            max_side: u32::MAX,
            max_output_pixels: u64::MAX,
            ..ToolLimits::default()
        };
        let bytes = validate_output(65_535, 1_000, expanded).unwrap();
        assert!(scratch_bytes(65_535, 1_000, bytes).is_err());
        assert!(scratch_bytes(8_000, 4_000, 128_000_000).unwrap() <= MAX_SCRATCH_BYTES);
    }

    #[test]
    fn image_tool_sharpen_preserves_alpha_hidden_rgb_and_constant_visible_color() {
        let mut first = image(9, 7, |x, y| {
            let alpha = [0, 1, 64, 128, 255][((x + y) % 5) as usize];
            if alpha == 0 {
                [255, 0, 255, 0]
            } else {
                [40, 90, 180, alpha]
            }
        });
        let mut second = image(9, 7, |x, y| {
            let alpha = [0, 1, 64, 128, 255][((x + y) % 5) as usize];
            if alpha == 0 {
                [0, 255, 0, 0]
            } else {
                [40, 90, 180, alpha]
            }
        });
        let original_first = first.bgra.clone();
        let original_second = second.bgra.clone();
        let cancel = AtomicBool::new(false);
        apply(&mut first, options(200, 0), ToolLimits::default(), &cancel).unwrap();
        apply(&mut second, options(200, 0), ToolLimits::default(), &cancel).unwrap();
        assert_eq!(first.bgra, original_first);
        assert_eq!(second.bgra, original_second);
        assert_eq!(first.readback_ms, 17.);
    }

    #[test]
    fn image_tool_sharpen_enhances_edges_respects_threshold_and_handles_tiny_images() {
        let mut edge = image(10, 1, |x, _| {
            let value = if x < 5 { 64 } else { 192 };
            [value, value, value, 255]
        });
        let cancel = AtomicBool::new(false);
        apply(&mut edge, options(100, 0), ToolLimits::default(), &cancel).unwrap();
        assert!(edge.bgra[4 * 4] < 64);
        assert!(edge.bgra[5 * 4] > 192);
        assert_eq!(&edge.bgra[..4], &[64, 64, 64, 255]);
        assert_eq!(&edge.bgra[9 * 4..], &[192, 192, 192, 255]);

        let mut threshold = image(10, 1, |x, _| [x as u8 * 25, 23, 180, 128]);
        let original = threshold.bgra.clone();
        apply(
            &mut threshold,
            options(200, 255),
            ToolLimits::default(),
            &cancel,
        )
        .unwrap();
        assert_eq!(threshold.bgra, original);
        for (width, height) in [(1, 1), (1, 257), (257, 1)] {
            let mut tiny = image(width, height, |_, _| [200, 90, 20, 64]);
            let original = tiny.bgra.clone();
            apply(&mut tiny, options(100, 0), ToolLimits::default(), &cancel).unwrap();
            assert_eq!(tiny.bgra, original);
        }
    }

    #[test]
    fn image_tool_sharpen_tiles_match_whole_image_kernel_without_seams() {
        let mut tiled = image(267, 271, |x, y| {
            [
                ((x * 13 + y * 17) % 256) as u8,
                ((x * 3 + y * 19) % 256) as u8,
                ((x * 23 + y * 7) % 256) as u8,
                [0, 1, 71, 128, 255][((x + y * 3) % 5) as usize],
            ]
        });
        let original = tiled.bgra.clone();
        let setting = options(150, 3);
        // 参考整图调库只验证分块坐标和余量；颜色与透明规则由其它测试独立验证。
        let view = PremultTile {
            source: &tiled,
            left: 0,
            top: 0,
            right: tiled.width,
            bottom: tiled.height,
        };
        let blurred = image::imageops::blur_advanced(&view, GaussianBlurParameters::SMOOTHING_5);
        let mut expected = original.clone();
        for y in 0..tiled.height {
            for x in 0..tiled.width {
                let offset = (y as usize * tiled.width as usize + x as usize) * 4;
                sharpen_pixel(
                    &original[offset..offset + 4],
                    blurred.get_pixel(x, y).0,
                    setting,
                    &mut expected[offset..offset + 4],
                )
                .unwrap();
            }
        }
        apply(
            &mut tiled,
            setting,
            ToolLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(tiled.bgra, expected);
    }
}
