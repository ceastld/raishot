//! 有界 BGRA 降采样；源区域的物理原点与裁边由调用方保留。
//!
//! 四个通道在编码后的数值空间中独立双线性插值，不做色彩转换、gamma
//! 线性化或 alpha 预乘。这不证明物理线性色中的插值或最终编码色准。

use crate::{recording_options::validate_region, Result};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FrameResizeGeometry {
    pub source_width: u32,
    pub source_height: u32,
    pub target_width: u32,
    pub target_height: u32,
}

impl FrameResizeGeometry {
    pub(crate) fn new(
        source_width: u32,
        source_height: u32,
        target_width: u32,
        target_height: u32,
    ) -> Result<Self> {
        validate_region(source_width, source_height)?;
        validate_region(target_width, target_height)?;
        if [source_width, source_height, target_width, target_height]
            .into_iter()
            .any(|size| !size.is_multiple_of(2))
        {
            return Err(tr!(
                "缩放源与目标宽高必须为已裁偶数，未自动裁剪",
                "Resize source and target dimensions must already be even; they were not cropped"
            )
            .into());
        }
        if target_width > source_width || target_height > source_height {
            return Err(tr!(
                "录屏缩放暂只支持逐轴降采样，未自动调整目标尺寸",
                "Recording resize supports downsampling on each axis only; the target was not adjusted"
            )
            .into());
        }
        Ok(Self {
            source_width,
            source_height,
            target_width,
            target_height,
        })
    }

    pub(crate) fn is_identity(self) -> bool {
        self.source_width == self.target_width && self.source_height == self.target_height
    }
}

#[derive(Clone, Copy)]
struct AxisSample {
    first: usize,
    second: usize,
    second_weight: u64,
}

fn axis_samples(source: u32, target: u32) -> Result<(Vec<AxisSample>, u64)> {
    let denominator = u64::from(target).checked_mul(2).ok_or(tr!(
        "缩放采样分母溢出",
        "Resize sampling denominator overflow"
    ))?;
    let mut samples = Vec::new();
    samples.try_reserve_exact(usize::try_from(target)?)?;
    for index in 0..target {
        // 像素中心 ((dst + 1/2) * source / target - 1/2)，保留精确有理数。
        // 只允许降采样，所以分子非负；不经浮点取整或累加步长。
        let numerator = u64::from(index)
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .and_then(|value| value.checked_mul(u64::from(source)))
            .and_then(|value| value.checked_sub(u64::from(target)))
            .ok_or(tr!(
                "缩放采样坐标溢出",
                "Resize sampling coordinate overflow"
            ))?;
        let first = numerator / denominator;
        if first >= u64::from(source) {
            return Err(tr!(
                "缩放采样坐标超出源尺寸",
                "Resize sampling coordinate exceeds the source size"
            )
            .into());
        }
        samples.push(AxisSample {
            first: usize::try_from(first)?,
            second: usize::try_from((first + 1).min(u64::from(source) - 1))?,
            second_weight: numerator % denominator,
        });
    }
    Ok((samples, denominator))
}

pub(crate) struct BgraDownscaler {
    geometry: FrameResizeGeometry,
    output: Vec<u8>,
    horizontal: Vec<AxisSample>,
    vertical: Vec<AxisSample>,
    horizontal_denominator: u64,
    vertical_denominator: u64,
    denominator: u64,
    source_row_bytes: usize,
    target_row_bytes: usize,
}

impl BgraDownscaler {
    pub(crate) fn new(geometry: FrameResizeGeometry) -> Result<Self> {
        // 字段公开供报告消费，仍拒绝绕过构造器手工拼出的非法几何。
        let geometry = FrameResizeGeometry::new(
            geometry.source_width,
            geometry.source_height,
            geometry.target_width,
            geometry.target_height,
        )?;
        let source_row_bytes = usize::try_from(geometry.source_width)?
            .checked_mul(4)
            .ok_or(tr!(
                "缩放源行字节数溢出",
                "Resize source row byte count overflow"
            ))?;
        let target_row_bytes = usize::try_from(geometry.target_width)?
            .checked_mul(4)
            .ok_or(tr!(
                "缩放目标行字节数溢出",
                "Resize target row byte count overflow"
            ))?;
        let target_bytes = target_row_bytes
            .checked_mul(usize::try_from(geometry.target_height)?)
            .ok_or(tr!(
                "缩放目标字节数溢出",
                "Resize target byte count overflow"
            ))?;
        let (horizontal, horizontal_denominator) =
            axis_samples(geometry.source_width, geometry.target_width)?;
        let (vertical, vertical_denominator) =
            axis_samples(geometry.source_height, geometry.target_height)?;
        let denominator = horizontal_denominator
            .checked_mul(vertical_denominator)
            .ok_or(tr!("缩放权重乘积溢出", "Resize weight product overflow"))?;
        // 四个非负权重和等于 denominator；任何通道的加权和不超过 255 倍。
        // 同时证明随后加半个分母取整也不会溢出，而不在逐像素循环里补救。
        denominator
            .checked_mul(255)
            .and_then(|maximum| maximum.checked_add(denominator / 2))
            .ok_or(tr!(
                "缩放像素加权数值溢出",
                "Resize weighted pixel value overflow"
            ))?;
        let mut output = Vec::new();
        output.try_reserve_exact(target_bytes)?;
        output.resize(target_bytes, 0);
        Ok(Self {
            geometry,
            output,
            horizontal,
            vertical,
            horizontal_denominator,
            vertical_denominator,
            denominator,
            source_row_bytes,
            target_row_bytes,
        })
    }

    /// stride 以 BGRA 像素为单位；只要求末行有效像素存在，不读取末行 padding。
    /// 返回紧密排列的 top-down 目标 BGRA；后续调用复用同一缓冲和采样表。
    pub(crate) fn resize(&mut self, source: &[u8], stride: u32) -> Result<&[u8]> {
        if stride < self.geometry.source_width {
            return Err(tr!(
                "缩放源行跨度小于源宽度",
                "Resize source stride is smaller than the source width"
            )
            .into());
        }
        let source_stride = usize::try_from(stride)?
            .checked_mul(4)
            .ok_or(tr!("缩放源行跨度溢出", "Resize source stride overflow"))?;
        let last_row = usize::try_from(self.geometry.source_height - 1)?;
        let required = source_stride
            .checked_mul(last_row)
            .and_then(|offset| offset.checked_add(self.source_row_bytes))
            .ok_or(tr!(
                "缩放源缓冲范围溢出",
                "Resize source buffer range overflow"
            ))?;
        if source.len() < required {
            return Err(tr!(
                "缩放源 BGRA 缓冲不完整",
                "Resize source BGRA buffer is incomplete"
            )
            .into());
        }
        // 在接触输出前完成全部范围检查；采样表已证明索引位于有效源尺寸内。
        if self.geometry.is_identity() {
            for row in 0..usize::try_from(self.geometry.target_height)? {
                let start = row * source_stride;
                let destination = row * self.target_row_bytes;
                self.output[destination..destination + self.target_row_bytes]
                    .copy_from_slice(&source[start..start + self.source_row_bytes]);
            }
            return Ok(&self.output);
        }
        for (row, y) in self.vertical.iter().enumerate() {
            let top = y.first * source_stride;
            let bottom = y.second * source_stride;
            let first_y_weight = self.vertical_denominator - y.second_weight;
            let destination = row * self.target_row_bytes;
            for (column, x) in self.horizontal.iter().enumerate() {
                let first_x = x.first * 4;
                let second_x = x.second * 4;
                let first_x_weight = self.horizontal_denominator - x.second_weight;
                for channel in 0..4 {
                    let upper = u64::from(source[top + first_x + channel]) * first_x_weight
                        + u64::from(source[top + second_x + channel]) * x.second_weight;
                    let lower = u64::from(source[bottom + first_x + channel]) * first_x_weight
                        + u64::from(source[bottom + second_x + channel]) * x.second_weight;
                    let weighted = upper * first_y_weight + lower * y.second_weight;
                    self.output[destination + column * 4 + channel] =
                        ((weighted + self.denominator / 2) / self.denominator) as u8;
                }
            }
        }
        Ok(&self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_frame_resize_identity_preserves_channels_and_padded_rows() {
        let geometry = FrameResizeGeometry::new(4, 2, 4, 2).unwrap();
        assert!(geometry.is_identity());
        let mut scaler = BgraDownscaler::new(geometry).unwrap();
        let packed: Vec<u8> = (0..32).collect();
        assert_eq!(scaler.resize(&packed, 4).unwrap(), packed.as_slice());
        let mut padded = vec![0xee; 40]; // 两个有效行，末行不提供额外 padding。
        padded[..16].copy_from_slice(&packed[..16]);
        padded[24..40].copy_from_slice(&packed[16..]);
        assert_eq!(scaler.resize(&padded, 6).unwrap(), packed.as_slice());
        let different = [37; 32];
        assert_eq!(scaler.resize(&different, 4).unwrap(), &different);
    }

    #[test]
    fn recording_frame_resize_half_scale_uses_centres_and_rounds_all_channels() {
        let geometry = FrameResizeGeometry::new(4, 4, 2, 2).unwrap();
        assert!(!geometry.is_identity());
        let mut source = Vec::new();
        for index in 0_u8..16 {
            source.extend([index, 100 + 2 * index, 255 - index, 40 + 3 * index]);
        }
        // 四个 2×2 块的 B 均值分别为 2.5、4.5、10.5、12.5；只在最终取整。
        let expected = [
            3, 105, 253, 48, 5, 109, 251, 54, 11, 121, 245, 72, 13, 125, 243, 78,
        ];
        let mut scaler = BgraDownscaler::new(geometry).unwrap();
        assert_eq!(scaler.resize(&source, 4).unwrap(), &expected);
    }

    #[test]
    fn recording_frame_resize_noninteger_centres_do_not_become_a_crop() {
        let mut source = Vec::new();
        for y in 0_u8..4 {
            for x in 0_u8..6 {
                source.extend([
                    40 * x,
                    20 * y,
                    if x % 2 == 0 { 0 } else { 200 },
                    if x == 2 || x == 3 { 255 } else { 0 },
                ]);
            }
        }
        // 源中心 x=.25/1.75/3.25/4.75，y=.5/2.5；覆盖源的左右两半。
        let expected = [
            10, 10, 50, 0, 70, 10, 50, 191, 130, 10, 150, 191, 190, 10, 150, 0, 10, 50, 50, 0, 70,
            50, 50, 191, 130, 50, 150, 191, 190, 50, 150, 0,
        ];
        let mut scaler =
            BgraDownscaler::new(FrameResizeGeometry::new(6, 4, 4, 2).unwrap()).unwrap();
        assert_eq!(scaler.resize(&source, 6).unwrap(), &expected);
    }

    #[test]
    fn recording_frame_resize_rejects_bad_geometry_stride_and_truncated_input() {
        for (sw, sh, tw, th) in [
            (0, 2, 2, 2),
            (4, 3, 2, 2),
            (4, 4, 3, 2),
            (4, 4, 6, 2),
            (4, 4, 2, 6),
            (u32::MAX, 2, 2, 2),
            (7680, 4320, 2, 2), // 单边合法，合计仍超过源像素预算。
        ] {
            assert!(FrameResizeGeometry::new(sw, sh, tw, th).is_err());
        }
        let forged = FrameResizeGeometry {
            source_width: 4,
            source_height: 4,
            target_width: 6,
            target_height: 2,
        };
        assert!(BgraDownscaler::new(forged).is_err());
        let mut scaler =
            BgraDownscaler::new(FrameResizeGeometry::new(4, 4, 2, 2).unwrap()).unwrap();
        assert!(scaler.resize(&[0; 64], 3).is_err());
        assert!(scaler.resize(&[0; 63], 4).is_err());
        assert!(scaler.resize(&[0; 64], u32::MAX).is_err());
        // 有 padding 的最后一行只需有效字节；坏输入后仍可完整生成下一帧。
        assert_eq!(scaler.resize(&[127; 88], 6).unwrap(), &[127; 16]);
    }
}
