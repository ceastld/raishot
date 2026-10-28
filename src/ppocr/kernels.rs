//! 自有 OCR 卷积内核原型。张量为连续 NCHW，权重为 OIHW，不依赖推理运行库。
//!
//! 目前只覆盖 OCR 主干常见的 depthwise 3×3 / 5×5 与 pointwise 1×1。
//! 所有尺寸和缓冲区检查均在写入前完成；Auto 只在 CPU 支持 AVX2 + FMA 时启用 SIMD。

use std::fmt;

/// 单次输出最多 256 MiB，避免损坏的模型尺寸驱动不受限的工作量。
pub const MAX_OUTPUT_ELEMENTS: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Scalar,
    Auto,
    Avx2,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Auto => "auto",
            Self::Avx2 => "avx2+fma",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn avx2_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Conv2d {
    pub batch: usize,
    pub in_channels: usize,
    pub height: usize,
    pub width: usize,
    pub out_channels: usize,
    pub kernel_h: usize,
    pub kernel_w: usize,
    pub stride_h: usize,
    pub stride_w: usize,
    pub pad_top: usize,
    pub pad_left: usize,
    pub pad_bottom: usize,
    pub pad_right: usize,
    pub groups: usize,
}

#[derive(Clone, Copy)]
enum Kind {
    Depthwise,
    Pointwise,
}

#[derive(Clone, Copy)]
struct Layout {
    kind: Kind,
    input_len: usize,
    weights_len: usize,
    output_len: usize,
    output_h: usize,
    output_w: usize,
    input_spatial: usize,
    output_spatial: usize,
}

impl Conv2d {
    pub fn output_shape(&self) -> Result<[usize; 4], String> {
        let layout = self.validate()?;
        Ok([
            self.batch,
            self.out_channels,
            layout.output_h,
            layout.output_w,
        ])
    }

    pub fn output_len(&self) -> Result<usize, String> {
        Ok(self.validate()?.output_len)
    }

    pub fn input_len(&self) -> Result<usize, String> {
        Ok(self.validate()?.input_len)
    }

    pub fn weights_len(&self) -> Result<usize, String> {
        Ok(self.validate()?.weights_len)
    }

    /// 运行卷积并返回实际后端。任何返回的错误都保持 output 原值。
    ///
    /// input / weights / output 长度须精确匹配；bias 可省略，否则每个输出通道一个值。
    pub fn run(
        &self,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
        backend: Backend,
    ) -> Result<Backend, String> {
        let layout = self.validate()?;
        for (name, actual, expected) in [
            ("input", input.len(), layout.input_len),
            ("weights", weights.len(), layout.weights_len),
            ("output", output.len(), layout.output_len),
        ] {
            if actual != expected {
                return Err(format!("{name} 长度错误：需要 {expected}，实际 {actual}"));
            }
        }
        if let Some(bias) = bias {
            if bias.len() != self.out_channels {
                return Err(format!(
                    "bias 长度错误：需要 {}，实际 {}",
                    self.out_channels,
                    bias.len()
                ));
            }
        }
        let actual = match backend {
            Backend::Scalar => Backend::Scalar,
            Backend::Auto if !avx2_available() => Backend::Scalar,
            Backend::Auto | Backend::Avx2 if avx2_available() => Backend::Avx2,
            Backend::Avx2 => return Err("当前 CPU 不支持 AVX2 + FMA".into()),
            Backend::Auto => Backend::Scalar,
        };
        if actual == Backend::Avx2 {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: 上面已检测 AVX2 与 FMA；validate 保证尺寸乘加不溢出，
            // 四个缓冲区已逐一校验，内部向量读写只覆盖完整的 8 个有效元素。
            unsafe {
                match layout.kind {
                    Kind::Depthwise => x86::depthwise(self, layout, input, weights, bias, output),
                    Kind::Pointwise => x86::pointwise(self, layout, input, weights, bias, output),
                }
            }
        } else {
            match layout.kind {
                Kind::Depthwise => self.depthwise_scalar(layout, input, weights, bias, output),
                Kind::Pointwise => self.pointwise_scalar(layout, input, weights, bias, output),
            }
        }
        Ok(actual)
    }

    fn validate(&self) -> Result<Layout, String> {
        if [
            self.batch,
            self.in_channels,
            self.height,
            self.width,
            self.out_channels,
            self.kernel_h,
            self.kernel_w,
            self.stride_h,
            self.stride_w,
            self.groups,
        ]
        .contains(&0)
        {
            return Err("卷积维度、步长和 groups 必须大于零".into());
        }
        if self.in_channels % self.groups != 0 || self.out_channels % self.groups != 0 {
            return Err("卷积通道数必须能被 groups 整除".into());
        }
        let kind = if self.groups == self.in_channels
            && self.out_channels == self.in_channels
            && self.kernel_h == self.kernel_w
            && matches!(self.kernel_h, 3 | 5)
            && matches!(self.stride_h, 1 | 2)
            && matches!(self.stride_w, 1 | 2)
        {
            Kind::Depthwise
        } else if self.groups == 1
            && self.kernel_h == 1
            && self.kernel_w == 1
            && self.stride_h == 1
            && self.stride_w == 1
            && [self.pad_top, self.pad_left, self.pad_bottom, self.pad_right] == [0; 4]
        {
            Kind::Pointwise
        } else {
            return Err(
                "原型仅支持 depthwise 3×3/5×5（步长 1/2）与无 padding 的 1×1 pointwise".into(),
            );
        };
        let output_h = output_dimension(
            self.height,
            self.pad_top,
            self.pad_bottom,
            self.kernel_h,
            self.stride_h,
        )?;
        let output_w = output_dimension(
            self.width,
            self.pad_left,
            self.pad_right,
            self.kernel_w,
            self.stride_w,
        )?;
        let input_spatial = checked_elements(&[self.height, self.width])?;
        let output_spatial = checked_elements(&[output_h, output_w])?;
        let input_len = checked_elements(&[self.batch, self.in_channels, input_spatial])?;
        let weights_len = checked_elements(&[
            self.out_channels,
            self.in_channels / self.groups,
            self.kernel_h,
            self.kernel_w,
        ])?;
        let output_len = checked_elements(&[self.batch, self.out_channels, output_spatial])?;
        if output_len > MAX_OUTPUT_ELEMENTS {
            return Err(format!(
                "卷积输出超过预算：{output_len} > {MAX_OUTPUT_ELEMENTS} 个元素"
            ));
        }
        Ok(Layout {
            kind,
            input_len,
            weights_len,
            output_len,
            output_h,
            output_w,
            input_spatial,
            output_spatial,
        })
    }

    fn depthwise_scalar(
        &self,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
    ) {
        for batch in 0..self.batch {
            for channel in 0..self.in_channels {
                let input_base = (batch * self.in_channels + channel) * layout.input_spatial;
                let output_base = (batch * self.out_channels + channel) * layout.output_spatial;
                let weights_base = channel * self.kernel_h * self.kernel_w;
                let initial = bias.map_or(0.0, |values| values[channel]);
                for y in 0..layout.output_h {
                    for x in 0..layout.output_w {
                        output[output_base + y * layout.output_w + x] = self.depthwise_pixel(
                            input,
                            weights,
                            input_base,
                            weights_base,
                            y,
                            x,
                            initial,
                        );
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn depthwise_pixel(
        &self,
        input: &[f32],
        weights: &[f32],
        input_base: usize,
        weights_base: usize,
        y: usize,
        x: usize,
        mut value: f32,
    ) -> f32 {
        for ky in 0..self.kernel_h {
            let padded_y = y * self.stride_h + ky;
            if padded_y < self.pad_top || padded_y - self.pad_top >= self.height {
                continue;
            }
            let row = input_base + (padded_y - self.pad_top) * self.width;
            for kx in 0..self.kernel_w {
                let padded_x = x * self.stride_w + kx;
                if padded_x >= self.pad_left && padded_x - self.pad_left < self.width {
                    value += input[row + padded_x - self.pad_left]
                        * weights[weights_base + ky * self.kernel_w + kx];
                }
            }
        }
        value
    }

    fn pointwise_scalar(
        &self,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
    ) {
        for batch in 0..self.batch {
            for out_channel in 0..self.out_channels {
                let output_base = (batch * self.out_channels + out_channel) * layout.output_spatial;
                let initial = bias.map_or(0.0, |values| values[out_channel]);
                for pixel in 0..layout.output_spatial {
                    output[output_base + pixel] = self.pointwise_pixel(
                        layout,
                        input,
                        weights,
                        batch,
                        out_channel,
                        pixel,
                        initial,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pointwise_pixel(
        &self,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        batch: usize,
        out_channel: usize,
        pixel: usize,
        mut value: f32,
    ) -> f32 {
        for in_channel in 0..self.in_channels {
            value += input[(batch * self.in_channels + in_channel) * layout.input_spatial + pixel]
                * weights[out_channel * self.in_channels + in_channel];
        }
        value
    }
}

fn checked_elements(dimensions: &[usize]) -> Result<usize, String> {
    let count = dimensions.iter().try_fold(1usize, |count, &dimension| {
        count.checked_mul(dimension).ok_or("卷积尺寸乘积溢出")
    })?;
    if count > isize::MAX as usize / std::mem::size_of::<f32>() {
        return Err("卷积张量超过可寻址缓冲区大小".into());
    }
    Ok(count)
}

fn output_dimension(
    input: usize,
    before: usize,
    after: usize,
    kernel: usize,
    stride: usize,
) -> Result<usize, String> {
    let padded = input
        .checked_add(before)
        .and_then(|value| value.checked_add(after))
        .ok_or("卷积 padding 后尺寸溢出")?;
    let remaining = padded
        .checked_sub(kernel)
        .ok_or("卷积核大于 padding 后输入")?;
    (remaining / stride)
        .checked_add(1)
        .ok_or_else(|| "卷积输出尺寸溢出".into())
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{Conv2d, Layout};
    use std::arch::x86_64::*;

    /// # Safety
    /// 调用方必须检测 AVX2 + FMA，使用 validate 的 layout，精确校验所有缓冲区长度。
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn depthwise(
        conv: &Conv2d,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
    ) {
        // SAFETY: validate 仅允许这八种组合，具体实现仍使用相同的有效输入范围。
        unsafe {
            match (conv.kernel_h, conv.stride_h, conv.stride_w) {
                (3, 1, 1) => depthwise_fixed::<3, 1, 1>(conv, layout, input, weights, bias, output),
                (3, 1, 2) => depthwise_fixed::<3, 1, 2>(conv, layout, input, weights, bias, output),
                (3, 2, 1) => depthwise_fixed::<3, 2, 1>(conv, layout, input, weights, bias, output),
                (3, 2, 2) => depthwise_fixed::<3, 2, 2>(conv, layout, input, weights, bias, output),
                (5, 1, 1) => depthwise_fixed::<5, 1, 1>(conv, layout, input, weights, bias, output),
                (5, 1, 2) => depthwise_fixed::<5, 1, 2>(conv, layout, input, weights, bias, output),
                (5, 2, 1) => depthwise_fixed::<5, 2, 1>(conv, layout, input, weights, bias, output),
                (5, 2, 2) => depthwise_fixed::<5, 2, 2>(conv, layout, input, weights, bias, output),
                _ => unreachable!("validated depthwise parameters"),
            }
        }
    }

    #[target_feature(enable = "avx2,fma")]
    unsafe fn depthwise_fixed<const K: usize, const SH: usize, const SW: usize>(
        conv: &Conv2d,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
    ) {
        // SAFETY: 各乘积已由入口验证；K/SH/SW 与验证过的参数一致。
        // 每通道只广播一次权重，每行只计算一次纵向补边和输入行地址。
        unsafe {
            let vector_start =
                (conv.pad_left / SW + usize::from(conv.pad_left % SW != 0)).min(layout.output_w);
            let vector_end = (conv.width + conv.pad_left)
                .checked_sub(K)
                .map_or(0, |last| (last / SW + 1).min(layout.output_w));
            for batch in 0..conv.batch {
                for channel in 0..conv.in_channels {
                    let input_base = (batch * conv.in_channels + channel) * layout.input_spatial;
                    let output_base = (batch * conv.out_channels + channel) * layout.output_spatial;
                    let channel_weights = weights.as_ptr().add(channel * K * K);
                    let mut broadcast = [_mm256_setzero_ps(); 25];
                    for tap in 0..K * K {
                        *broadcast.get_unchecked_mut(tap) =
                            _mm256_set1_ps(*channel_weights.add(tap));
                    }
                    let initial = bias.map_or(0.0, |values| values[channel]);
                    for y in 0..layout.output_h {
                        let padded_y = y * SH;
                        let interior =
                            padded_y >= conv.pad_top && padded_y + K <= conv.pad_top + conv.height;
                        let mut rows = [std::ptr::null(); 5];
                        for ky in 0..K {
                            let source_y = padded_y + ky;
                            if source_y >= conv.pad_top && source_y - conv.pad_top < conv.height {
                                *rows.get_unchecked_mut(ky) = input
                                    .as_ptr()
                                    .add(input_base + (source_y - conv.pad_top) * conv.width);
                            }
                        }
                        let output_row = output.as_mut_ptr().add(output_base + y * layout.output_w);
                        if interior {
                            depthwise_row::<K, SW, true>(
                                conv,
                                layout.output_w,
                                vector_start,
                                vector_end,
                                &rows,
                                &broadcast,
                                initial,
                                output_row,
                            );
                        } else {
                            depthwise_row::<K, SW, false>(
                                conv,
                                layout.output_w,
                                vector_start,
                                vector_end,
                                &rows,
                                &broadcast,
                                initial,
                                output_row,
                            );
                        }
                    }
                }
            }
        }
    }

    #[target_feature(enable = "avx2,fma")]
    unsafe fn depthwise_row<const K: usize, const SW: usize, const INTERIOR: bool>(
        conv: &Conv2d,
        output_w: usize,
        vector_start: usize,
        vector_end: usize,
        rows: &[*const f32; 5],
        weights: &[__m256; 25],
        initial: f32,
        output: *mut f32,
    ) {
        // SAFETY: rows 中的非空指针均指向当前通道的有效输入行。内区完整向量
        // 只读有效列；边缘 gather 以有效行首为 base，负数/越界列均被 mask 禁用，
        // 不先构造越界 add 指针。尾部 store 同样屏蔽超过 output_w 的 lane。
        // 输出预算为 2^26 个元素，K<=5、SW<=2，因此 width、pad_left、x*SW
        // 均小于 2^28，带符号 gather 列坐标可无损表示为 i32。
        unsafe {
            let lanes = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
            let steps = _mm256_setr_epi32(
                0,
                SW as i32,
                2 * SW as i32,
                3 * SW as i32,
                4 * SW as i32,
                5 * SW as i32,
                6 * SW as i32,
                7 * SW as i32,
            );
            let mut x = 0;
            while x < output_w {
                let mut sum = _mm256_set1_ps(initial);
                let active =
                    _mm256_cmpgt_epi32(_mm256_set1_epi32((output_w - x).min(8) as i32), lanes);
                if x >= vector_start && x + 8 <= vector_end {
                    let source_x = x * SW - conv.pad_left;
                    for ky in 0..K {
                        let row = *rows.get_unchecked(ky);
                        if !INTERIOR && row.is_null() {
                            continue;
                        }
                        for kx in 0..K {
                            let source = row.add(source_x + kx);
                            let values = if SW == 1 {
                                _mm256_loadu_ps(source)
                            } else {
                                _mm256_i32gather_ps::<4>(source, steps)
                            };
                            sum = _mm256_fmadd_ps(values, *weights.get_unchecked(ky * K + kx), sum);
                        }
                    }
                    _mm256_storeu_ps(output.add(x), sum);
                } else {
                    let origin = (x * SW) as i32 - conv.pad_left as i32;
                    let columns = _mm256_add_epi32(_mm256_set1_epi32(origin), steps);
                    for ky in 0..K {
                        let row = *rows.get_unchecked(ky);
                        if !INTERIOR && row.is_null() {
                            continue;
                        }
                        for kx in 0..K {
                            let indices = _mm256_add_epi32(columns, _mm256_set1_epi32(kx as i32));
                            let valid = _mm256_and_si256(
                                active,
                                _mm256_and_si256(
                                    _mm256_cmpgt_epi32(indices, _mm256_set1_epi32(-1)),
                                    _mm256_cmpgt_epi32(
                                        _mm256_set1_epi32(conv.width as i32),
                                        indices,
                                    ),
                                ),
                            );
                            let mask = _mm256_castsi256_ps(valid);
                            let values = _mm256_mask_i32gather_ps::<4>(
                                _mm256_setzero_ps(),
                                row,
                                indices,
                                mask,
                            );
                            let next =
                                _mm256_fmadd_ps(values, *weights.get_unchecked(ky * K + kx), sum);
                            // 不让 padding 的 0*Inf/NaN 改变原本应跳过该 tap 的输出。
                            sum = _mm256_blendv_ps(sum, next, mask);
                        }
                    }
                    _mm256_maskstore_ps(output.add(x), active, sum);
                }
                x += 8;
            }
        }
    }

    /// # Safety
    /// 调用方必须检测 AVX2 + FMA，使用 validate 的 layout，精确校验所有缓冲区长度。
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn pointwise(
        conv: &Conv2d,
        layout: Layout,
        input: &[f32],
        weights: &[f32],
        bias: Option<&[f32]>,
        output: &mut [f32],
    ) {
        // SAFETY: 调用契约保证特性和长度；每个输入/输出通道均有 spatial
        // 个元素，完整空间块分别满足 pixel+16/8<=spatial。四通道块满足 channel+4<=out_channels，
        // 尾部通道和像素单独处理；无需对齐，所有 load/store 都使用 unaligned。
        // 内层输入指针按完整通道平面递增，最后一次用 wrapping_add，避免构造越过
        // allocation 末尾的普通 add 指针；越界的最终指针不会解引用。
        unsafe {
            let spatial = layout.output_spatial;
            for batch in 0..conv.batch {
                let input_batch = batch * conv.in_channels * spatial;
                let output_batch = batch * conv.out_channels * spatial;
                let inputs = input.as_ptr().add(input_batch);
                let mut channel = 0;
                while channel + 4 <= conv.out_channels {
                    let b0 = bias.map_or(0.0, |values| values[channel]);
                    let b1 = bias.map_or(0.0, |values| values[channel + 1]);
                    let b2 = bias.map_or(0.0, |values| values[channel + 2]);
                    let b3 = bias.map_or(0.0, |values| values[channel + 3]);
                    let w0 = weights.as_ptr().add(channel * conv.in_channels);
                    let w1 = w0.add(conv.in_channels);
                    let w2 = w1.add(conv.in_channels);
                    let w3 = w2.add(conv.in_channels);
                    let o0 = output.as_mut_ptr().add(output_batch + channel * spatial);
                    let o1 = o0.add(spatial);
                    let o2 = o1.add(spatial);
                    let o3 = o2.add(spatial);
                    let mut pixel = 0;
                    while pixel + 16 <= spatial {
                        let mut s00 = _mm256_set1_ps(b0);
                        let mut s01 = s00;
                        let mut s10 = _mm256_set1_ps(b1);
                        let mut s11 = s10;
                        let mut s20 = _mm256_set1_ps(b2);
                        let mut s21 = s20;
                        let mut s30 = _mm256_set1_ps(b3);
                        let mut s31 = s30;
                        let mut values = inputs.add(pixel);
                        for ic in 0..conv.in_channels {
                            let v0 = _mm256_loadu_ps(values);
                            let v1 = _mm256_loadu_ps(values.add(8));
                            let weight = _mm256_set1_ps(*w0.add(ic));
                            s00 = _mm256_fmadd_ps(v0, weight, s00);
                            s01 = _mm256_fmadd_ps(v1, weight, s01);
                            let weight = _mm256_set1_ps(*w1.add(ic));
                            s10 = _mm256_fmadd_ps(v0, weight, s10);
                            s11 = _mm256_fmadd_ps(v1, weight, s11);
                            let weight = _mm256_set1_ps(*w2.add(ic));
                            s20 = _mm256_fmadd_ps(v0, weight, s20);
                            s21 = _mm256_fmadd_ps(v1, weight, s21);
                            let weight = _mm256_set1_ps(*w3.add(ic));
                            s30 = _mm256_fmadd_ps(v0, weight, s30);
                            s31 = _mm256_fmadd_ps(v1, weight, s31);
                            values = values.wrapping_add(spatial);
                        }
                        _mm256_storeu_ps(o0.add(pixel), s00);
                        _mm256_storeu_ps(o0.add(pixel + 8), s01);
                        _mm256_storeu_ps(o1.add(pixel), s10);
                        _mm256_storeu_ps(o1.add(pixel + 8), s11);
                        _mm256_storeu_ps(o2.add(pixel), s20);
                        _mm256_storeu_ps(o2.add(pixel + 8), s21);
                        _mm256_storeu_ps(o3.add(pixel), s30);
                        _mm256_storeu_ps(o3.add(pixel + 8), s31);
                        pixel += 16;
                    }
                    while pixel + 8 <= spatial {
                        let mut s0 = _mm256_set1_ps(b0);
                        let mut s1 = _mm256_set1_ps(b1);
                        let mut s2 = _mm256_set1_ps(b2);
                        let mut s3 = _mm256_set1_ps(b3);
                        let mut values = inputs.add(pixel);
                        for ic in 0..conv.in_channels {
                            let vector = _mm256_loadu_ps(values);
                            s0 = _mm256_fmadd_ps(vector, _mm256_set1_ps(*w0.add(ic)), s0);
                            s1 = _mm256_fmadd_ps(vector, _mm256_set1_ps(*w1.add(ic)), s1);
                            s2 = _mm256_fmadd_ps(vector, _mm256_set1_ps(*w2.add(ic)), s2);
                            s3 = _mm256_fmadd_ps(vector, _mm256_set1_ps(*w3.add(ic)), s3);
                            values = values.wrapping_add(spatial);
                        }
                        _mm256_storeu_ps(o0.add(pixel), s0);
                        _mm256_storeu_ps(o1.add(pixel), s1);
                        _mm256_storeu_ps(o2.add(pixel), s2);
                        _mm256_storeu_ps(o3.add(pixel), s3);
                        pixel += 8;
                    }
                    for oc in channel..channel + 4 {
                        for pixel in pixel..spatial {
                            output[output_batch + oc * spatial + pixel] = conv.pointwise_pixel(
                                layout,
                                input,
                                weights,
                                batch,
                                oc,
                                pixel,
                                bias.map_or(0.0, |values| values[oc]),
                            );
                        }
                    }
                    channel += 4;
                }
                while channel < conv.out_channels {
                    let initial = bias.map_or(0.0, |values| values[channel]);
                    let mut pixel = 0;
                    while pixel + 8 <= spatial {
                        let mut sum = _mm256_set1_ps(initial);
                        for ic in 0..conv.in_channels {
                            let values = _mm256_loadu_ps(
                                input.as_ptr().add(input_batch + ic * spatial + pixel),
                            );
                            sum = _mm256_fmadd_ps(
                                values,
                                _mm256_set1_ps(weights[channel * conv.in_channels + ic]),
                                sum,
                            );
                        }
                        _mm256_storeu_ps(
                            output
                                .as_mut_ptr()
                                .add(output_batch + channel * spatial + pixel),
                            sum,
                        );
                        pixel += 8;
                    }
                    for pixel in pixel..spatial {
                        output[output_batch + channel * spatial + pixel] = conv.pointwise_pixel(
                            layout, input, weights, batch, channel, pixel, initial,
                        );
                    }
                    channel += 1;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "kernels_tests.rs"]
mod tests;
