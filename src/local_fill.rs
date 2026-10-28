//! 局部颜色填充：只读 Gray8 掩模确定孔，从相邻 BGRA 像素取主色。
//! 不膨胀掩模、不重建纹理；原掩模外的像素及行间 padding 均保持原样。
use crate::Result;

pub(crate) const MAX_FILL_PIXELS: u64 = 32_000_000;
const COLOR_BUCKETS: usize = 1 << 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FillOptions {
    pub(crate) ring: u8,
    pub(crate) threshold: u8,
}
impl Default for FillOptions {
    fn default() -> Self {
        Self {
            ring: 2,
            threshold: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FillPath {
    Ring,
    Boundary,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FillStats {
    pub(crate) path: FillPath,
    pub(crate) requested: u32,
    pub(crate) filled: u32,
}
impl FillStats {
    fn empty() -> Self {
        Self {
            path: FillPath::Empty,
            requested: 0,
            filled: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    sum: [u64; 3],
    count: u32,
    first: u32,
}

/// 校验和所有可能失败的分配均发生在首次写入之前。
/// 最后一行只要求有效像素长度，不要求该行尾部存在 padding。
pub(crate) fn erase_bgra(
    image: &mut [u8],
    width: u32,
    height: u32,
    stride: usize,
    mask: &[u8],
    mask_stride: usize,
    options: FillOptions,
) -> Result<FillStats> {
    if width == 0 || height == 0 {
        return Ok(FillStats::empty());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("局部填充像素数溢出")?;
    if pixels > MAX_FILL_PIXELS {
        return Err(tr!(
            "局部填充超过 3200 万像素限制，请减小处理范围",
            "Local fill is limited to 32 megapixels; reduce the area"
        )
        .into());
    }
    let width = usize::try_from(width)?;
    let height = usize::try_from(height)?;
    let image_row = width.checked_mul(4).ok_or("局部填充行宽溢出")?;
    validate_buffer(image.len(), stride, image_row, height, "图像")?;
    validate_buffer(mask.len(), mask_stride, width, height, "掩模")?;

    let mut requested = 0_u32;
    for y in 0..height {
        for x in 0..width {
            requested += u32::from(is_hole(mask, mask_stride, x, y, options.threshold));
        }
    }
    if requested == 0 {
        return Ok(FillStats::empty());
    }
    if u64::from(requested) == pixels {
        // 全部都是孔时没有可用样本；保持 Boundary 结果，也避免无谓分配和扫描。
        return Ok(FillStats {
            path: FillPath::Boundary,
            requested,
            filled: 0,
        });
    }

    // 固定 15 bit RGB 桶约 1 MiB；不复制整块 Gray8 掩模或 BGRA 图像。
    let mut buckets = Vec::new();
    buckets.try_reserve_exact(COLOR_BUCKETS)?;
    buckets.resize(COLOR_BUCKETS, Bucket::default());
    let ring = usize::from(options.ring.clamp(1, 3));
    let mut samples = 0_u32;
    for y in 0..height {
        let top = y.saturating_sub(ring);
        let bottom = (y + ring + 1).min(height);
        let mut neighbors = 0_usize;
        for ny in top..bottom {
            for nx in 0..(ring + 1).min(width) {
                neighbors += usize::from(is_hole(mask, mask_stride, nx, ny, options.threshold));
            }
        }
        for x in 0..width {
            // 对非孔像素，包含自身的方形窗口与参考的 TouchesHole 等价。
            // 每个外环像素只计一次，不按与多少孔相邻进行重复加权。
            if neighbors != 0 && !is_hole(mask, mask_stride, x, y, options.threshold) {
                let i = y * stride + x * 4;
                let pixel = &image[i..i + 3];
                let key = (usize::from(pixel[2] >> 3) << 10)
                    | (usize::from(pixel[1] >> 3) << 5)
                    | usize::from(pixel[0] >> 3);
                let bucket = &mut buckets[key];
                if bucket.count == 0 {
                    bucket.first = samples;
                }
                bucket.count += 1;
                for (sum, channel) in bucket.sum.iter_mut().zip(pixel) {
                    *sum += u64::from(*channel);
                }
                samples += 1;
            }
            // 将方形窗口右移一像素，仅更新两列，不为每个候选重复扫描全邻域。
            if x >= ring {
                for ny in top..bottom {
                    neighbors -=
                        usize::from(is_hole(mask, mask_stride, x - ring, ny, options.threshold));
                }
            }
            if x + ring + 1 < width {
                for ny in top..bottom {
                    neighbors += usize::from(is_hole(
                        mask,
                        mask_stride,
                        x + ring + 1,
                        ny,
                        options.threshold,
                    ));
                }
            }
        }
    }
    if samples >= 4 {
        let mut best: Option<&Bucket> = None;
        for bucket in &buckets {
            if bucket.count != 0
                && best.is_none_or(|old| {
                    bucket.count > old.count
                        || (bucket.count == old.count && bucket.first < old.first)
                })
            {
                best = Some(bucket);
            }
        }
        let best = best.expect("四个外环样本必有主色桶");
        let color = best.sum.map(|sum| (sum / u64::from(best.count)) as u8);
        for y in 0..height {
            for x in 0..width {
                if is_hole(mask, mask_stride, x, y, options.threshold) {
                    write_pixel(image, stride, x, y, color);
                }
            }
        }
        return Ok(FillStats {
            path: FillPath::Ring,
            requested,
            filled: requested,
        });
    }

    // 回退只能采原掩模外的像素；已填好的孔不会成为后面孔的样本。
    // 读到的非孔像素从未写入，因此无需额外保存原 BGRA 副本。
    let mut filled = 0_u32;
    for y in 0..height {
        for x in 0..width {
            if !is_hole(mask, mask_stride, x, y, options.threshold) {
                continue;
            }
            for radius in [1_usize, 3, 8] {
                let mut sum = [0_u64; 3];
                let mut count = 0_u64;
                for ny in y.saturating_sub(radius)..(y + radius + 1).min(height) {
                    for nx in x.saturating_sub(radius)..(x + radius + 1).min(width) {
                        if is_hole(mask, mask_stride, nx, ny, options.threshold) {
                            continue;
                        }
                        let i = ny * stride + nx * 4;
                        for (total, channel) in sum.iter_mut().zip(&image[i..i + 3]) {
                            *total += u64::from(*channel);
                        }
                        count += 1;
                    }
                }
                if count != 0 {
                    write_pixel(image, stride, x, y, sum.map(|total| (total / count) as u8));
                    filled += 1;
                    break;
                }
            }
        }
    }
    Ok(FillStats {
        path: FillPath::Boundary,
        requested,
        filled,
    })
}

fn validate_buffer(
    len: usize,
    stride: usize,
    row: usize,
    height: usize,
    label: &str,
) -> Result<()> {
    if stride < row {
        return Err(format!("局部填充{label} stride 小于有效行宽").into());
    }
    let needed = stride
        .checked_mul(height - 1)
        .and_then(|n| n.checked_add(row))
        .ok_or_else(|| format!("局部填充{label}缓冲区长度溢出"))?;
    if len < needed {
        return Err(format!("局部填充{label}缓冲区长度不足").into());
    }
    Ok(())
}

fn is_hole(mask: &[u8], stride: usize, x: usize, y: usize, threshold: u8) -> bool {
    mask[y * stride + x] > threshold
}

fn write_pixel(image: &mut [u8], stride: usize, x: usize, y: usize, color: [u8; 3]) {
    let i = y * stride + x * 4;
    image[i..i + 3].copy_from_slice(&color);
    image[i + 3] = 255;
}
