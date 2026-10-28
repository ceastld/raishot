//! 有界图片拼接；布局与像素生成共用同一计划，输入顺序就是成品顺序。
use crate::{
    capture::Image,
    image_resize::{ResizeFilter, RowResizer},
    image_tools::{
        allocate_pixels, check_cancel, validate_input, validate_output, ProcessedImage, ToolError,
        ToolLimits, ToolResult,
    },
};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollageDirection {
    Horizontal,
    Vertical,
    /// 按输入顺序逐行填充；每列与每行分别使用该列最大宽度和该行最大高度。
    Grid {
        columns: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollageSizing {
    Original,
    EqualWidth(u32),
    EqualHeight(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollageAlignment {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CollageOptions {
    pub direction: CollageDirection,
    pub sizing: CollageSizing,
    pub gap: u32,
    pub background_bgra: [u8; 4],
    pub alignment: CollageAlignment,
}

impl Default for CollageOptions {
    fn default() -> Self {
        Self {
            direction: CollageDirection::Vertical,
            sizing: CollageSizing::Original,
            gap: 0,
            background_bgra: [0; 4],
            alignment: CollageAlignment::Start,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CollagePlacement {
    pub source_index: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 只保存有界布局；预览可读取尺寸和每个来源的位置，不分配结果像素。
pub(crate) struct CollagePlan {
    width: u32,
    height: u32,
    byte_len: usize,
    background_bgra: [u8; 4],
    placements: Vec<CollagePlacement>,
}

impl CollagePlan {
    pub(crate) fn new(
        images: &[&Image],
        options: CollageOptions,
        limits: ToolLimits,
    ) -> ToolResult<Self> {
        if images.is_empty() || images.len() > limits.max_items {
            return Err(ToolError::Invalid(
                tr!(
                    "拼接图片数量为空或超过上限",
                    "Invalid number of collage images"
                )
                .into(),
            ));
        }
        // 先检查全部输入与合计预算，再展开有限的布局项。
        let mut input_pixels = 0_u64;
        for image in images {
            input_pixels = input_pixels
                .checked_add(validate_input(image, limits)?)
                .ok_or_else(size_overflow)?;
            if input_pixels > limits.max_input_pixels {
                return Err(ToolError::Invalid(
                    tr!(
                        "拼接输入图片合计超过像素上限",
                        "Collage inputs exceed the pixel limit"
                    )
                    .into(),
                ));
            }
        }
        let mut placements = Vec::new();
        placements.try_reserve_exact(images.len()).map_err(|_| {
            ToolError::Failed(tr!("无法分配拼接布局", "Unable to allocate collage layout").into())
        })?;
        for (source_index, image) in images.iter().enumerate() {
            let (width, height) = scaled_size(image, options.sizing)?;
            placements.push(CollagePlacement {
                source_index,
                x: 0,
                y: 0,
                width,
                height,
            });
        }
        let (width, height) = match options.direction {
            CollageDirection::Grid { columns } => grid_layout(
                &mut placements,
                columns,
                options.gap,
                options.alignment,
                limits.max_items,
            )?,
            direction => {
                let horizontal = direction == CollageDirection::Horizontal;
                let mut length = 0_u64;
                let mut cross = 0_u32;
                for placement in &mut placements {
                    if placement.source_index > 0 {
                        length = length
                            .checked_add(u64::from(options.gap))
                            .ok_or_else(size_overflow)?;
                    }
                    let origin = u32::try_from(length).map_err(|_| size_overflow())?;
                    let (item_length, item_cross) = if horizontal {
                        placement.x = origin;
                        (placement.width, placement.height)
                    } else {
                        placement.y = origin;
                        (placement.height, placement.width)
                    };
                    length = length
                        .checked_add(u64::from(item_length))
                        .ok_or_else(size_overflow)?;
                    cross = cross.max(item_cross);
                }
                let length = u32::try_from(length).map_err(|_| size_overflow())?;
                for placement in &mut placements {
                    let item_cross = if horizontal {
                        placement.height
                    } else {
                        placement.width
                    };
                    let offset = alignment_offset(cross - item_cross, options.alignment);
                    if horizontal {
                        placement.y = offset;
                    } else {
                        placement.x = offset;
                    }
                }
                if horizontal {
                    (length, cross)
                } else {
                    (cross, length)
                }
            }
        };
        let byte_len = validate_output(width, height, limits)?;
        Ok(Self {
            width,
            height,
            byte_len,
            background_bgra: options.background_bgra,
            placements,
        })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn placements(&self) -> &[CollagePlacement] {
        &self.placements
    }
}

fn alignment_offset(remaining: u32, alignment: CollageAlignment) -> u32 {
    match alignment {
        CollageAlignment::Start => 0,
        CollageAlignment::Center => remaining / 2,
        CollageAlignment::End => remaining,
    }
}

fn layout_sizes(count: usize) -> ToolResult<Vec<u32>> {
    let mut sizes = Vec::new();
    sizes.try_reserve_exact(count).map_err(|_| {
        ToolError::Failed(tr!("无法分配拼接布局", "Unable to allocate collage layout").into())
    })?;
    sizes.resize(count, 0);
    Ok(sizes)
}

/// 将已检查的单元尺寸转换为轴起点，间距只出现在相邻行或列之间。
fn axis_origins(sizes: &[u32], gap: u32) -> ToolResult<(Vec<u32>, u32)> {
    let mut origins = layout_sizes(sizes.len())?;
    let mut total = 0_u32;
    for (index, (&size, origin)) in sizes.iter().zip(&mut origins).enumerate() {
        if index > 0 {
            total = total.checked_add(gap).ok_or_else(size_overflow)?;
        }
        *origin = total;
        total = total.checked_add(size).ok_or_else(size_overflow)?;
    }
    Ok((origins, total))
}

fn grid_layout(
    placements: &mut [CollagePlacement],
    columns: u32,
    gap: u32,
    alignment: CollageAlignment,
    max_items: usize,
) -> ToolResult<(u32, u32)> {
    let columns = usize::try_from(columns).map_err(|_| size_overflow())?;
    if columns == 0 || columns > max_items {
        return Err(ToolError::Invalid(
            tr!(
                "网格列数须大于零且不能超过图片项数上限",
                "Grid columns must be positive and within the image item limit"
            )
            .into(),
        ));
    }
    // 输入非空且已受 max_items 约束；只建立实际使用的列和向上取整后的行。
    let columns = columns.min(placements.len());
    let rows = placements.len().div_ceil(columns);
    let mut widths = layout_sizes(columns)?;
    let mut heights = layout_sizes(rows)?;
    for (index, placement) in placements.iter().enumerate() {
        widths[index % columns] = widths[index % columns].max(placement.width);
        heights[index / columns] = heights[index / columns].max(placement.height);
    }
    let (x_origins, width) = axis_origins(&widths, gap)?;
    let (y_origins, height) = axis_origins(&heights, gap)?;
    for (index, placement) in placements.iter_mut().enumerate() {
        let column = index % columns;
        let row = index / columns;
        placement.x = x_origins[column]
            .checked_add(alignment_offset(
                widths[column] - placement.width,
                alignment,
            ))
            .ok_or_else(size_overflow)?;
        placement.y = y_origins[row]
            .checked_add(alignment_offset(heights[row] - placement.height, alignment))
            .ok_or_else(size_overflow)?;
    }
    Ok((width, height))
}

fn size_overflow() -> ToolError {
    ToolError::Invalid(tr!("拼接图片尺寸溢出", "Collage dimensions overflow").into())
}

/// 比例使用整数四舍五入；极窄一边至少保留一个像素。
fn proportional_side(side: u32, target: u32, original: u32) -> ToolResult<u32> {
    if target == 0 {
        return Err(ToolError::Invalid(
            tr!(
                "拼接目标尺寸必须大于零",
                "Collage target size must be positive"
            )
            .into(),
        ));
    }
    let scaled = u64::from(side)
        .checked_mul(u64::from(target))
        .and_then(|value| value.checked_add(u64::from(original / 2)))
        .ok_or_else(size_overflow)?
        / u64::from(original);
    u32::try_from(scaled.max(1)).map_err(|_| size_overflow())
}

fn scaled_size(image: &Image, sizing: CollageSizing) -> ToolResult<(u32, u32)> {
    match sizing {
        CollageSizing::Original => Ok((image.width, image.height)),
        CollageSizing::EqualWidth(width) => {
            Ok((width, proportional_side(image.height, width, image.width)?))
        }
        CollageSizing::EqualHeight(height) => Ok((
            proportional_side(image.width, height, image.height)?,
            height,
        )),
    }
}

/// 保留按 floor(index * source / target) 采样的既有最近邻合同，不生成中间整图。
/// straight alpha 按 over 合成背景。
/// 调用方在源或参数变化时递增 request_id，保存时使用结果的代次保护。
pub(crate) fn compose(
    images: &[&Image],
    options: CollageOptions,
    limits: ToolLimits,
    request_id: u64,
    cancel: &AtomicBool,
) -> ToolResult<ProcessedImage> {
    compose_impl(images, options, None, limits, request_id, cancel)
}

/// 可选像素中心最近邻或 alpha 感知双线性；只暂存一条最长 placement 的 BGRA 行。
/// 双线性不等同于面积平均下采样，实际采样复用共用缩放器。
pub(crate) fn compose_with_filter(
    images: &[&Image],
    options: CollageOptions,
    filter: ResizeFilter,
    limits: ToolLimits,
    request_id: u64,
    cancel: &AtomicBool,
) -> ToolResult<ProcessedImage> {
    compose_impl(images, options, Some(filter), limits, request_id, cancel)
}

fn compose_impl(
    images: &[&Image],
    options: CollageOptions,
    filter: Option<ResizeFilter>,
    limits: ToolLimits,
    request_id: u64,
    cancel: &AtomicBool,
) -> ToolResult<ProcessedImage> {
    check_cancel(cancel)?;
    let plan = CollagePlan::new(images, options, limits)?;
    check_cancel(cancel)?;
    // 在分配成品前检查行缓冲的尺寸、字节数与预算；各 placement 共用这一条行。
    let mut scratch = if filter.is_some() {
        let width = plan
            .placements
            .iter()
            .map(|placement| placement.width)
            .max()
            .ok_or_else(size_overflow)?;
        allocate_pixels(validate_output(width, 1, limits)?)?
    } else {
        Vec::new()
    };
    check_cancel(cancel)?;
    let mut bgra = allocate_pixels(plan.byte_len)?;
    let stride = plan.width as usize * 4;
    for row in bgra.chunks_exact_mut(stride) {
        check_cancel(cancel)?;
        for pixel in row.chunks_exact_mut(4) {
            pixel.copy_from_slice(&plan.background_bgra);
        }
    }
    for placement in &plan.placements {
        let source = images[placement.source_index];
        let source_stride = source.width as usize * 4;
        let resizer = filter
            .map(|filter| {
                RowResizer::new(source, placement.width, placement.height, filter, limits)
            })
            .transpose()?;
        for y in 0..placement.height {
            check_cancel(cancel)?;
            let dest_row = (placement.y as usize + y as usize) * stride;
            if let Some(resizer) = &resizer {
                // 这一宽度不超过分配前已验证的最长行，乘法与切片均受其约束。
                let row_bytes = placement.width as usize * 4;
                let row = &mut scratch[..row_bytes];
                resizer.render_row(y, row, cancel)?;
                let start = dest_row + placement.x as usize * 4;
                for (destination, source) in bgra[start..start + row_bytes]
                    .chunks_exact_mut(4)
                    .zip(row.chunks_exact(4))
                {
                    over(destination, source);
                }
                continue;
            }
            let source_y =
                (u64::from(y) * u64::from(source.height) / u64::from(placement.height)) as usize;
            for x in 0..placement.width {
                let source_x =
                    (u64::from(x) * u64::from(source.width) / u64::from(placement.width)) as usize;
                let from = source_y * source_stride + source_x * 4;
                let to = dest_row + (placement.x as usize + x as usize) * 4;
                over(&mut bgra[to..to + 4], &source.bgra[from..from + 4]);
            }
        }
    }
    check_cancel(cancel)?;
    Ok(ProcessedImage::new(
        Image {
            width: plan.width,
            height: plan.height,
            bgra,
            readback_ms: 0.,
        },
        request_id,
    ))
}

fn over(destination: &mut [u8], source: &[u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        destination.copy_from_slice(source);
        return;
    }
    let destination_alpha = u32::from(destination[3]);
    let remainder = 255 - source_alpha;
    let alpha = source_alpha * 255 + destination_alpha * remainder;
    for channel in 0..3 {
        let color = u32::from(source[channel]) * source_alpha * 255
            + u32::from(destination[channel]) * destination_alpha * remainder;
        destination[channel] = ((color + alpha / 2) / alpha) as u8;
    }
    destination[3] = ((alpha + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn image_collage_horizontal_preserves_order_alignment_and_gap() {
        let red = [0, 0, 255, 255];
        let blue = [255, 0, 0, 255];
        let background = [7, 11, 19, 255];
        let first = image(1, 1, &[red]);
        let second = image(1, 3, &[blue; 3]);
        let options = CollageOptions {
            direction: CollageDirection::Horizontal,
            gap: 1,
            background_bgra: background,
            alignment: CollageAlignment::Center,
            ..Default::default()
        };
        let plan = CollagePlan::new(&[&first, &second], options, ToolLimits::default()).unwrap();
        assert_eq!(plan.size(), (3, 3));
        assert_eq!((plan.placements()[0].x, plan.placements()[0].y), (0, 1));
        assert_eq!(plan.placements()[1].source_index, 1);
        let result = compose(
            &[&first, &second],
            options,
            ToolLimits::default(),
            41,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.request_id(), 41);
        assert_eq!(pixel(result.image(), 0, 0), background);
        assert_eq!(pixel(result.image(), 0, 1), red);
        assert_eq!(pixel(result.image(), 1, 1), background);
        assert_eq!(pixel(result.image(), 2, 2), blue);
    }

    #[test]
    fn image_collage_equal_width_uses_nearest_pixels_and_proportions() {
        let left = [1, 2, 3, 255];
        let right = [4, 5, 6, 255];
        let first = image(2, 1, &[left, right]);
        let second = image(1, 1, &[right]);
        let result = compose(
            &[&first, &second],
            CollageOptions {
                sizing: CollageSizing::EqualWidth(4),
                ..Default::default()
            },
            ToolLimits::default(),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((result.image().width, result.image().height), (4, 6));
        assert_eq!(pixel(result.image(), 1, 1), left);
        assert_eq!(pixel(result.image(), 2, 1), right);
        assert_eq!(pixel(result.image(), 0, 2), right);
        assert_eq!(first.bgra, [left, right].concat());
    }

    #[test]
    fn image_collage_equal_height_and_end_alignment_are_explicit() {
        let first = image(3, 1, &[[1, 2, 3, 255]; 3]);
        let second = image(1, 2, &[[4, 5, 6, 255]; 2]);
        let plan = CollagePlan::new(
            &[&first, &second],
            CollageOptions {
                sizing: CollageSizing::EqualHeight(2),
                alignment: CollageAlignment::End,
                ..Default::default()
            },
            ToolLimits::default(),
        )
        .unwrap();
        assert_eq!(plan.size(), (6, 4));
        assert_eq!(plan.placements()[1].x, 5);
        assert_eq!(proportional_side(1, 1, 3).unwrap(), 1);
    }

    #[test]
    fn image_collage_straight_alpha_over_keeps_transparent_edges_clean() {
        let source = image(1, 1, &[[200, 100, 0, 128]]);
        let result = compose(
            &[&source],
            CollageOptions {
                background_bgra: [0, 100, 200, 128],
                ..Default::default()
            },
            ToolLimits::default(),
            0,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(pixel(result.image(), 0, 0), [134, 100, 66, 192]);
        let mut transparent = [17, 23, 31, 0];
        over(&mut transparent, &[255, 0, 0, 0]);
        assert_eq!(transparent, [17, 23, 31, 0]);
        over(&mut transparent, &[200, 100, 0, 128]);
        assert_eq!(transparent, [200, 100, 0, 128]);
    }

    #[test]
    fn image_collage_rejects_empty_excessive_overflow_and_budget_inputs() {
        let source = image(1, 1, &[[1, 2, 3, 255]]);
        let options = CollageOptions::default();
        assert!(CollagePlan::new(&[], options, ToolLimits::default()).is_err());
        let mut limits = ToolLimits::default();
        limits.max_items = 1;
        assert!(CollagePlan::new(&[&source, &source], options, limits).is_err());
        limits = ToolLimits::default();
        limits.max_input_pixels = 1;
        assert!(CollagePlan::new(&[&source, &source], options, limits).is_err());
        limits = ToolLimits::default();
        limits.max_output_pixels = 1;
        assert!(CollagePlan::new(&[&source, &source], options, limits).is_err());
        assert!(CollagePlan::new(
            &[&source, &source],
            CollageOptions {
                gap: u32::MAX,
                ..options
            },
            ToolLimits::default(),
        )
        .is_err());
        assert!(CollagePlan::new(
            &[&source],
            CollageOptions {
                sizing: CollageSizing::EqualWidth(0),
                ..options
            },
            ToolLimits::default(),
        )
        .is_err());
        let malformed = image(2, 1, &[[1, 2, 3, 255]]);
        assert!(CollagePlan::new(&[&malformed], options, ToolLimits::default()).is_err());
    }

    #[test]
    fn image_collage_filtered_rows_keep_alpha_edges_clean_and_legacy_nearest_unchanged() {
        let source = image(2, 1, &[[255, 9, 7, 0], [0, 40, 200, 128]]);
        let cancel = AtomicBool::new(false);
        let options = CollageOptions {
            sizing: CollageSizing::EqualWidth(3),
            ..Default::default()
        };
        let smooth = compose_with_filter(
            &[&source],
            options,
            ResizeFilter::Bilinear,
            ToolLimits::default(),
            81,
            &cancel,
        )
        .unwrap();
        assert_eq!((smooth.image().width, smooth.image().height), (3, 2));
        assert_eq!(smooth.request_id(), 81);
        for y in 0..2 {
            assert_eq!(pixel(smooth.image(), 0, y), [0; 4]);
            assert_eq!(pixel(smooth.image(), 1, y), [0, 40, 200, 64]);
            assert_eq!(pixel(smooth.image(), 2, y), [0, 40, 200, 128]);
        }
        let centered = compose_with_filter(
            &[&source],
            options,
            ResizeFilter::Nearest,
            ToolLimits::default(),
            82,
            &cancel,
        )
        .unwrap();
        let legacy = compose(&[&source], options, ToolLimits::default(), 83, &cancel).unwrap();
        assert_eq!(pixel(centered.image(), 1, 0), [0, 40, 200, 128]);
        assert_eq!(pixel(legacy.image(), 1, 0), [0; 4]);

        let smaller = compose_with_filter(
            &[&source],
            CollageOptions {
                sizing: CollageSizing::EqualWidth(1),
                ..options
            },
            ResizeFilter::Bilinear,
            ToolLimits::default(),
            84,
            &cancel,
        )
        .unwrap();
        assert_eq!((smaller.image().width, smaller.image().height), (1, 1));
        assert_eq!(pixel(smaller.image(), 0, 0), [0, 40, 200, 64]);
        assert_eq!(source.bgra, [255, 9, 7, 0, 0, 40, 200, 128]);
    }

    #[test]
    fn image_collage_filtered_rows_preserve_unscaled_composition_and_reject_budget_or_cancel() {
        let source = image(2, 1, &[[200, 100, 0, 128], [255, 0, 0, 0]]);
        let narrow = image(1, 1, &[[10, 20, 30, 255]]);
        let sources = [&source, &narrow, &source];
        let options = CollageOptions {
            direction: CollageDirection::Grid { columns: 2 },
            gap: 1,
            background_bgra: [0, 100, 200, 128],
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let legacy = compose(&sources, options, ToolLimits::default(), 0, &cancel).unwrap();
        for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear] {
            let unscaled =
                compose_with_filter(&sources, options, filter, ToolLimits::default(), 0, &cancel)
                    .unwrap();
            assert_eq!(unscaled.image().bgra, legacy.image().bgra);
            let larger = CollageOptions {
                sizing: CollageSizing::EqualWidth(3),
                ..options
            };
            for limits in [
                ToolLimits {
                    max_input_pixels: 1,
                    ..ToolLimits::default()
                },
                ToolLimits {
                    max_output_pixels: 5,
                    ..ToolLimits::default()
                },
                ToolLimits {
                    max_side: 2,
                    ..ToolLimits::default()
                },
            ] {
                assert!(matches!(
                    compose_with_filter(&[&source], larger, filter, limits, 0, &cancel),
                    Err(ToolError::Invalid(_))
                ));
            }
            assert!(matches!(
                compose_with_filter(
                    &[&source],
                    larger,
                    filter,
                    ToolLimits::default(),
                    0,
                    &AtomicBool::new(true),
                ),
                Err(ToolError::Cancelled)
            ));
        }
    }

    #[test]
    fn image_collage_grid_fills_rows_in_order_and_keeps_only_needed_rows_and_columns() {
        let images: Vec<_> = (1..=5)
            .map(|value| image(1, 1, &[[value, 0, 0, 255]]))
            .collect();
        let sources: Vec<_> = images.iter().collect();
        let background = [9, 7, 5, 255];
        let options = CollageOptions {
            direction: CollageDirection::Grid { columns: 3 },
            gap: 1,
            background_bgra: background,
            ..Default::default()
        };
        let plan = CollagePlan::new(&sources, options, ToolLimits::default()).unwrap();
        assert_eq!(plan.size(), (5, 3));
        assert_eq!(
            plan.placements()
                .iter()
                .map(|p| (p.source_index, p.x, p.y))
                .collect::<Vec<_>>(),
            [(0, 0, 0), (1, 2, 0), (2, 4, 0), (3, 0, 2), (4, 2, 2)]
        );
        let result = compose(
            &sources,
            options,
            ToolLimits::default(),
            73,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.request_id(), 73);
        for placement in plan.placements() {
            assert_eq!(
                pixel(result.image(), placement.x, placement.y),
                [(placement.source_index + 1) as u8, 0, 0, 255]
            );
        }
        assert_eq!(pixel(result.image(), 4, 2), background);
        assert_eq!(pixel(result.image(), 1, 0), background);
        assert_eq!(pixel(result.image(), 0, 1), background);

        let pair = CollagePlan::new(
            &sources[..2],
            CollageOptions {
                direction: CollageDirection::Grid { columns: 4 },
                ..options
            },
            ToolLimits::default(),
        )
        .unwrap();
        assert_eq!(pair.size(), (3, 1));
        let complete_rows = CollagePlan::new(
            &sources[..4],
            CollageOptions {
                direction: CollageDirection::Grid { columns: 2 },
                ..options
            },
            ToolLimits::default(),
        )
        .unwrap();
        assert_eq!(complete_rows.size(), (3, 3));
    }

    #[test]
    fn image_collage_grid_aligns_both_axes_using_scaled_column_and_row_maxima() {
        let first = image(1, 1, &[[1, 0, 0, 255]]);
        let second = image(3, 2, &[[2, 0, 0, 255]; 6]);
        let third = image(5, 4, &[[3, 0, 0, 255]; 20]);
        let fourth = image(1, 1, &[[4, 0, 0, 255]]);
        let sources = [&first, &second, &third, &fourth];
        let options = CollageOptions {
            direction: CollageDirection::Grid { columns: 2 },
            gap: 1,
            ..Default::default()
        };
        for (alignment, expected) in [
            (CollageAlignment::Start, [(0, 0), (6, 0), (0, 3), (6, 3)]),
            (CollageAlignment::Center, [(2, 0), (6, 0), (0, 3), (7, 4)]),
            (CollageAlignment::End, [(4, 1), (6, 0), (0, 3), (8, 6)]),
        ] {
            let plan = CollagePlan::new(
                &sources,
                CollageOptions {
                    alignment,
                    ..options
                },
                ToolLimits::default(),
            )
            .unwrap();
            assert_eq!(plan.size(), (9, 7));
            assert_eq!(
                plan.placements()
                    .iter()
                    .map(|p| (p.x, p.y))
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let scaled = CollagePlan::new(
            &sources,
            CollageOptions {
                sizing: CollageSizing::EqualWidth(2),
                ..options
            },
            ToolLimits::default(),
        )
        .unwrap();
        assert_eq!(scaled.size(), (5, 5));
        assert_eq!(
            scaled
                .placements()
                .iter()
                .map(|p| (p.width, p.height))
                .collect::<Vec<_>>(),
            [(2, 2), (2, 1), (2, 2), (2, 2)]
        );
    }

    #[test]
    fn image_collage_grid_reuses_straight_alpha_and_preserves_gap_background() {
        let semi = image(1, 1, &[[200, 100, 0, 128]]);
        let hidden = image(1, 1, &[[255, 0, 0, 0]]);
        let background = [0, 100, 200, 128];
        let result = compose(
            &[&semi, &hidden, &semi],
            CollageOptions {
                direction: CollageDirection::Grid { columns: 2 },
                gap: 1,
                background_bgra: background,
                ..Default::default()
            },
            ToolLimits::default(),
            74,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((result.image().width, result.image().height), (3, 3));
        assert_eq!(pixel(result.image(), 0, 0), [134, 100, 66, 192]);
        assert_eq!(pixel(result.image(), 0, 2), [134, 100, 66, 192]);
        for (x, y) in [(1, 0), (0, 1), (2, 0), (2, 2)] {
            assert_eq!(pixel(result.image(), x, y), background);
        }
    }

    #[test]
    fn image_collage_grid_rejects_column_dimensions_and_pixel_budget_overflows() {
        let source = image(1, 1, &[[1, 2, 3, 255]]);
        let sources = [&source, &source, &source];
        let options = CollageOptions {
            direction: CollageDirection::Grid { columns: 2 },
            gap: 1,
            ..Default::default()
        };
        for columns in [0, ToolLimits::default().max_items as u32 + 1] {
            assert!(CollagePlan::new(
                &sources,
                CollageOptions {
                    direction: CollageDirection::Grid { columns },
                    ..options
                },
                ToolLimits::default(),
            )
            .is_err());
        }
        for limits in [
            ToolLimits {
                max_input_pixels: 2,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_output_pixels: 8,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_side: 2,
                ..ToolLimits::default()
            },
            ToolLimits {
                max_items: 2,
                ..ToolLimits::default()
            },
        ] {
            assert!(CollagePlan::new(&sources, options, limits).is_err());
        }
        for columns in [1, 2] {
            assert!(CollagePlan::new(
                &sources[..2],
                CollageOptions {
                    direction: CollageDirection::Grid { columns },
                    gap: u32::MAX,
                    ..options
                },
                ToolLimits::default(),
            )
            .is_err());
        }
    }

    #[test]
    fn image_collage_cancelled_request_returns_no_result() {
        let source = image(1, 1, &[[1, 2, 3, 255]]);
        assert!(matches!(
            compose(
                &[&source],
                CollageOptions::default(),
                ToolLimits::default(),
                0,
                &AtomicBool::new(true),
            ),
            Err(ToolError::Cancelled)
        ));
    }
}
