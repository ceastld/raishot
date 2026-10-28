//! 有界 BGRA 缩放。两个采样器均映射像素中心，并将超出来源的坐标钳制到边界。
use crate::{
    capture::Image,
    image_tools::{
        allocate_pixels, check_cancel, validate_input, validate_output, ToolError, ToolLimits,
        ToolResult,
    },
};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResizeFilter {
    Nearest,
    /// 四邻居双线性；按 alpha 预乘加权，不是面积平均下采样。
    Bilinear,
}

/// 借用不可变源，只验证一次尺寸与预算；调用方可直接向最终图中的一行写入。
/// 不分配整帧或缓存源图，采样与 resize 使用相同的像素中心和 alpha 规则。
pub(crate) struct RowResizer<'a> {
    source: &'a Image,
    width: u32,
    height: u32,
    filter: ResizeFilter,
    source_stride: usize,
    output_stride: usize,
    byte_len: usize,
}

impl<'a> RowResizer<'a> {
    pub(crate) fn new(
        source: &'a Image,
        width: u32,
        height: u32,
        filter: ResizeFilter,
        limits: ToolLimits,
    ) -> ToolResult<Self> {
        validate_input(source, limits)?;
        let byte_len = validate_output(width, height, limits)?;
        Ok(Self {
            source,
            width,
            height,
            filter,
            source_stride: source.width as usize * 4,
            output_stride: width as usize * 4,
            byte_len,
        })
    }

    /// 行号和目标长度必须精确匹配；调用前已取消或参数无效时不修改目标。
    /// 行内不逐像素轮询取消；执行中的取消在本行写完后返回，调用方不应提交该结果。
    pub(crate) fn render_row(
        &self,
        y: u32,
        destination: &mut [u8],
        cancel: &AtomicBool,
    ) -> ToolResult<()> {
        check_cancel(cancel)?;
        if y >= self.height || destination.len() != self.output_stride {
            return Err(ToolError::Invalid(
                tr!(
                    "缩放行号或目标行长度无效",
                    "Invalid resize row index or destination length"
                )
                .into(),
            ));
        }
        if (self.source.width, self.source.height) == (self.width, self.height) {
            let offset = y as usize * self.source_stride;
            destination.copy_from_slice(&self.source.bgra[offset..offset + self.source_stride]);
        } else {
            match self.filter {
                ResizeFilter::Nearest => {
                    let source_y = nearest_index(y, self.source.height, self.height);
                    for (x, destination) in destination.chunks_exact_mut(4).enumerate() {
                        let source_x = nearest_index(x as u32, self.source.width, self.width);
                        let offset = source_y * self.source_stride + source_x * 4;
                        destination.copy_from_slice(&self.source.bgra[offset..offset + 4]);
                    }
                }
                ResizeFilter::Bilinear => {
                    let (top, bottom, y_weight) = linear_axis(y, self.source.height, self.height);
                    for (x, destination) in destination.chunks_exact_mut(4).enumerate() {
                        let (left, right, x_weight) =
                            linear_axis(x as u32, self.source.width, self.width);
                        let offsets = [
                            top * self.source_stride + left * 4,
                            top * self.source_stride + right * 4,
                            bottom * self.source_stride + left * 4,
                            bottom * self.source_stride + right * 4,
                        ];
                        let weights = [
                            (1. - x_weight) * (1. - y_weight),
                            x_weight * (1. - y_weight),
                            (1. - x_weight) * y_weight,
                            x_weight * y_weight,
                        ];
                        interpolate(destination, &self.source.bgra, offsets, weights);
                    }
                }
            }
        }
        check_cancel(cancel)
    }
}

/// 先验证输入和目标预算，再只分配一张输出图。同尺寸逐行复制，保留透明像素隐藏 RGB。
/// 双线性在编码的颜色通道上插值；像素中心为 (index + 0.5)，边缘向最近来源像素延伸。
pub(crate) fn resize(
    source: &Image,
    width: u32,
    height: u32,
    filter: ResizeFilter,
    limits: ToolLimits,
    cancel: &AtomicBool,
) -> ToolResult<Image> {
    check_cancel(cancel)?;
    let resizer = RowResizer::new(source, width, height, filter, limits)?;
    check_cancel(cancel)?;
    let mut bgra = allocate_pixels(resizer.byte_len)?;
    for (y, row) in bgra.chunks_exact_mut(resizer.output_stride).enumerate() {
        resizer.render_row(y as u32, row, cancel)?;
    }
    check_cancel(cancel)?;
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

fn nearest_index(index: u32, source: u32, target: u32) -> usize {
    // floor((index + 0.5) * source / target)，u128 避免任意合法 u32 尺寸乘积溢出。
    let numerator = (u128::from(index) * 2 + 1) * u128::from(source);
    let denominator = u128::from(target) * 2;
    ((numerator / denominator) as u32).min(source - 1) as usize
}

fn linear_axis(index: u32, source: u32, target: u32) -> (usize, usize, f64) {
    let coordinate = ((f64::from(index) + 0.5) * f64::from(source) / f64::from(target) - 0.5)
        .clamp(0., f64::from(source - 1));
    let low = coordinate.floor() as u32;
    (
        low as usize,
        low.saturating_add(1).min(source - 1) as usize,
        coordinate - f64::from(low),
    )
}

fn interpolate(destination: &mut [u8], source: &[u8], offsets: [usize; 4], weights: [f64; 4]) {
    let mut alpha = 0.;
    let mut premultiplied = [0.; 3];
    for (offset, weight) in offsets.into_iter().zip(weights) {
        let weighted_alpha = f64::from(source[offset + 3]) * weight;
        alpha += weighted_alpha;
        for channel in 0..3 {
            premultiplied[channel] += f64::from(source[offset + channel]) * weighted_alpha;
        }
    }
    let rounded_alpha = alpha.round().clamp(0., 255.) as u8;
    if rounded_alpha == 0 {
        destination.fill(0);
        return;
    }
    for channel in 0..3 {
        destination[channel] = (premultiplied[channel] / alpha).round().clamp(0., 255.) as u8;
    }
    destination[3] = rounded_alpha;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_tools::ToolError;

    fn image(width: u32, height: u32, pixels: &[[u8; 4]]) -> Image {
        Image {
            width,
            height,
            bgra: pixels.iter().flatten().copied().collect(),
            readback_ms: 0.,
        }
    }

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * image.width as usize + x as usize) * 4;
        image.bgra[offset..offset + 4].try_into().unwrap()
    }

    fn run(source: &Image, width: u32, height: u32, filter: ResizeFilter) -> Image {
        resize(
            source,
            width,
            height,
            filter,
            ToolLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    #[test]
    fn image_resize_nearest_maps_pixel_centers_exactly() {
        let colors = [
            [1, 2, 3, 255],
            [11, 12, 13, 128],
            [21, 22, 23, 0],
            [31, 32, 33, 255],
            [41, 42, 43, 128],
            [51, 52, 53, 0],
        ];
        let source = image(3, 2, &colors);
        let output = run(&source, 5, 3, ResizeFilter::Nearest);
        let source_x = [0, 0, 1, 2, 2];
        let source_y = [0, 1, 1];
        for y in 0..3 {
            for x in 0..5 {
                assert_eq!(
                    pixel(&output, x, y),
                    colors[source_y[y as usize] * 3 + source_x[x as usize]]
                );
            }
        }
        assert_eq!(
            pixel(&run(&source, 1, 1, ResizeFilter::Nearest), 0, 0),
            colors[4]
        );
    }

    #[test]
    fn image_resize_bilinear_interpolates_four_opaque_neighbors() {
        let source = image(
            2,
            2,
            &[
                [0, 0, 0, 255],
                [100, 0, 0, 255],
                [0, 100, 0, 255],
                [100, 100, 200, 255],
            ],
        );
        let output = run(&source, 3, 3, ResizeFilter::Bilinear);
        assert_eq!(pixel(&output, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&output, 1, 0), [50, 0, 0, 255]);
        assert_eq!(pixel(&output, 1, 1), [50, 50, 50, 255]);
        assert_eq!(pixel(&output, 2, 2), [100, 100, 200, 255]);
        assert_eq!(
            pixel(&run(&source, 1, 1, ResizeFilter::Bilinear), 0, 0),
            [50, 50, 50, 255]
        );
    }

    #[test]
    fn image_resize_bilinear_premultiplies_alpha_and_ignores_hidden_rgb() {
        let source = image(2, 1, &[[255, 0, 0, 0], [0, 0, 200, 128]]);
        let output = run(&source, 3, 1, ResizeFilter::Bilinear);
        assert_eq!(pixel(&output, 0, 0), [0; 4]);
        assert_eq!(pixel(&output, 1, 0), [0, 0, 200, 64]);
        assert_eq!(pixel(&output, 2, 0), [0, 0, 200, 128]);
        let source = image(2, 1, &[[0, 100, 0, 64], [0, 0, 200, 192]]);
        let output = run(&source, 3, 1, ResizeFilter::Bilinear);
        assert_eq!(pixel(&output, 1, 0), [0, 25, 150, 128]);
    }

    #[test]
    fn image_resize_single_pixel_edges_extend_without_dimming() {
        let color = [10, 20, 30, 77];
        let source = image(1, 1, &[color]);
        for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear] {
            let output = run(&source, 4, 3, filter);
            assert!(output.bgra.chunks_exact(4).all(|pixel| pixel == color));
        }
        let source = image(1, 2, &[color, [30, 40, 50, 77]]);
        let output = run(&source, 3, 3, ResizeFilter::Bilinear);
        assert_eq!(pixel(&output, 0, 0), color);
        assert_eq!(pixel(&output, 1, 1), [20, 30, 40, 77]);
        assert_eq!(pixel(&output, 2, 2), [30, 40, 50, 77]);
    }

    #[test]
    fn image_resize_same_dimensions_preserve_all_bgra_bytes() {
        let source = image(2, 1, &[[251, 61, 17, 0], [10, 20, 30, 1]]);
        for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear] {
            let output = run(&source, 2, 1, filter);
            assert_eq!(output.bgra, source.bgra);
        }
    }

    #[test]
    fn image_resize_rejects_invalid_data_budget_and_cancelled_requests() {
        let source = image(1, 1, &[[1, 2, 3, 255]]);
        let cancel = AtomicBool::new(false);
        for (width, height) in [(0, 1), (1, 0), (u32::MAX, u32::MAX)] {
            assert!(resize(
                &source,
                width,
                height,
                ResizeFilter::Nearest,
                ToolLimits::default(),
                &cancel,
            )
            .is_err());
        }
        assert!(resize(
            &source,
            2,
            2,
            ResizeFilter::Bilinear,
            ToolLimits {
                max_output_pixels: 3,
                ..ToolLimits::default()
            },
            &cancel,
        )
        .is_err());
        let malformed = image(2, 1, &[[1, 2, 3, 255]]);
        assert!(resize(
            &malformed,
            1,
            1,
            ResizeFilter::Nearest,
            ToolLimits::default(),
            &cancel,
        )
        .is_err());
        assert!(matches!(
            resize(
                &source,
                2,
                2,
                ResizeFilter::Bilinear,
                ToolLimits::default(),
                &AtomicBool::new(true),
            ),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn image_resize_row_rejects_bounds_length_and_prior_cancellation_without_writing() {
        let source = image(2, 1, &[[251, 61, 17, 0], [10, 20, 30, 1]]);
        let cancel = AtomicBool::new(false);
        for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear] {
            let resizer = RowResizer::new(&source, 2, 1, filter, ToolLimits::default()).unwrap();
            let mut row = [91; 8];
            assert!(resizer.render_row(1, &mut row, &cancel).is_err());
            assert_eq!(row, [91; 8]);
            assert!(resizer.render_row(0, &mut row[..7], &cancel).is_err());
            assert_eq!(row, [91; 8]);
            let cancelled = AtomicBool::new(true);
            assert!(matches!(
                resizer.render_row(0, &mut row, &cancelled),
                Err(ToolError::Cancelled)
            ));
            assert_eq!(row, [91; 8]);
            resizer.render_row(0, &mut row, &cancel).unwrap();
            assert_eq!(row.as_slice(), source.bgra.as_slice());
        }
    }
}
