//! BGRA 面积平均缩放与 RGB555 色盘映射，坐标区间只在创建时计算一次。
use crate::{Histogram, Palette};

/// 磁盘缓存读回后验证完整 RGB555 范围，避免损坏缓存越界查表。
pub fn valid_keys(keys: &[u16]) -> bool {
    keys.iter().all(|key| *key < 1 << 15)
}

#[derive(Clone, Copy)]
struct Span {
    start: u32,
    end: u32,
}

/// 保持宿主原有 floor/ceil 覆盖和整数面积均值，不改缩放采样结果。
pub struct Scaler {
    size: (u32, u32),
    output: (u32, u32),
    columns: Vec<Span>,
    rows: Vec<Span>,
}

impl Scaler {
    pub fn new(size: (u32, u32), output: (u32, u32)) -> Self {
        assert!(size.0 != 0 && size.1 != 0 && output.0 != 0 && output.1 != 0);
        Self {
            size,
            output,
            columns: spans(size.0, output.0),
            rows: spans(size.1, output.1),
        }
    }

    /// 输入为宿主解码后的紧密 top-down BGRA，x/y 位于输出尺寸内。
    pub fn color(&self, bgra: &[u8], x: u32, y: u32) -> [u8; 3] {
        if self.size == self.output {
            let i = (y as usize * self.size.0 as usize + x as usize) * 4;
            return [bgra[i + 2], bgra[i + 1], bgra[i]];
        }
        let xs = self.columns[x as usize];
        let ys = self.rows[y as usize];
        let mut sum = [0_u32; 3];
        for sy in ys.start..ys.end {
            let row = sy as usize * self.size.0 as usize * 4;
            for sx in xs.start..xs.end {
                let i = row + sx as usize * 4;
                sum[0] += u32::from(bgra[i + 2]);
                sum[1] += u32::from(bgra[i + 1]);
                sum[2] += u32::from(bgra[i]);
            }
        }
        let count = (ys.end - ys.start) * (xs.end - xs.start);
        sum.map(|channel| (channel / count) as u8)
    }

    /// 第一遍的步长4与额外末列/末行/末角采样都在本 crate 内执行。
    pub fn accumulate(&self, histogram: &mut Histogram, bgra: &[u8]) {
        let (width, height) = self.output;
        for y in (0..height).step_by(4) {
            for x in (0..width).step_by(4) {
                histogram.sample_rgb(self.color(bgra, x, y));
            }
            histogram.sample_rgb(self.color(bgra, width - 1, y));
        }
        for x in (0..width).step_by(4) {
            histogram.sample_rgb(self.color(bgra, x, height - 1));
        }
        histogram.sample_rgb(self.color(bgra, width - 1, height - 1));
    }

    /// 第一遍只缩放一次，缓存原索引映射所需的 RGB555 桶。
    /// 直方图仍累计完整 8 位 RGB；末行/末列与网格重合时保留重复权重。
    pub fn accumulate_keys(&self, histogram: &mut Histogram, bgra: &[u8]) -> Vec<u16> {
        self.accumulate_keys_with(histogram, bgra, &mut || true)
            .expect("固定继续的像素缓存不会取消")
    }

    /// 每 16 行及完成时检查取消；返回 Err 时丢弃本次直方图和所有缓存。
    /// 已累计行不会回滚，调用者必须立即结束转换，不能继续 build。
    pub fn accumulate_keys_with(
        &self,
        histogram: &mut Histogram,
        bgra: &[u8],
        proceed: &mut dyn FnMut() -> bool,
    ) -> crate::Result<Vec<u16>> {
        const CANCEL_ROWS: u32 = 16;
        let (width, height) = self.output;
        // 同尺寸连续读取，keys 与稀疏直方图分开，避免逐像素判定采样权重。
        if self.size == self.output {
            let stride = width as usize * 4;
            let mut keys = Vec::with_capacity(width as usize * height as usize);
            for y in 0..height {
                if y.is_multiple_of(CANCEL_ROWS) && !proceed() {
                    return Err("已取消 GIF 转换".into());
                }
                let start = y as usize * stride;
                let row = &bgra[start..start + stride];
                keys.extend(
                    row.chunks_exact(4)
                        .map(|pixel| crate::palette::key([pixel[2], pixel[1], pixel[0]]) as u16),
                );
                let row_weight = u32::from(y.is_multiple_of(4)) + u32::from(y == height - 1);
                for _ in 0..row_weight {
                    for pixel in row.chunks_exact(4).step_by(4) {
                        histogram.sample_rgb([pixel[2], pixel[1], pixel[0]]);
                    }
                    let pixel = &row[stride - 4..];
                    histogram.sample_rgb([pixel[2], pixel[1], pixel[0]]);
                }
            }
            if !proceed() {
                return Err("已取消 GIF 转换".into());
            }
            return Ok(keys);
        }
        // 通用面积缩放仍走下面的原函数正文。
        let mut keys = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            if y.is_multiple_of(CANCEL_ROWS) && !proceed() {
                return Err("已取消 GIF 转换".into());
            }
            let row_weight = u32::from(y.is_multiple_of(4)) + u32::from(y == height - 1);
            for x in 0..width {
                let rgb = self.color(bgra, x, y);
                keys.push(crate::palette::key(rgb) as u16);
                let column_weight = u32::from(x.is_multiple_of(4)) + u32::from(x == width - 1);
                for _ in 0..row_weight * column_weight {
                    histogram.sample_rgb(rgb);
                }
            }
        }
        if !proceed() {
            return Err("已取消 GIF 转换".into());
        }
        Ok(keys)
    }

    /// 第二遍直接生成一字节索引，不创建缩放后的 RGBA 整帧。
    pub fn index(&self, bgra: &[u8], palette: &Palette) -> Vec<u8> {
        let (width, height) = self.output;
        let mut indices = Vec::with_capacity(width as usize * height as usize);
        if self.size == self.output {
            indices.extend(
                bgra.chunks_exact(4)
                    .map(|pixel| palette.index([pixel[2], pixel[1], pixel[0]])),
            );
        } else {
            for y in 0..height {
                for x in 0..width {
                    indices.push(palette.index(self.color(bgra, x, y)));
                }
            }
        }
        indices
    }
}

fn spans(length: u32, output: u32) -> Vec<Span> {
    (0..output)
        .map(|point| {
            let start = (u64::from(point) * u64::from(length) / u64::from(output)) as u32;
            let end = ((u64::from(point + 1) * u64::from(length)).div_ceil(u64::from(output))
                as u32)
                .clamp(start + 1, length);
            Span { start, end }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 原宿主 scaled_color 逐像素坐标计算；与预计算实现分别执行。
    fn old_color(bgra: &[u8], size: (u32, u32), output: (u32, u32), point: (u32, u32)) -> [u8; 3] {
        let (width, height) = size;
        let (out_w, out_h) = output;
        let (x, y) = point;
        if (out_w, out_h) == (width, height) {
            let i = (y as usize * width as usize + x as usize) * 4;
            return [bgra[i + 2], bgra[i + 1], bgra[i]];
        }
        let y0 = (u64::from(y) * u64::from(height) / u64::from(out_h)) as u32;
        let y1 = ((u64::from(y + 1) * u64::from(height)).div_ceil(u64::from(out_h)) as u32)
            .clamp(y0 + 1, height);
        let x0 = (u64::from(x) * u64::from(width) / u64::from(out_w)) as u32;
        let x1 = ((u64::from(x + 1) * u64::from(width)).div_ceil(u64::from(out_w)) as u32)
            .clamp(x0 + 1, width);
        let mut sum = [0_u32; 3];
        for sy in y0..y1 {
            for sx in x0..x1 {
                let i = (sy as usize * width as usize + sx as usize) * 4;
                sum[0] += u32::from(bgra[i + 2]);
                sum[1] += u32::from(bgra[i + 1]);
                sum[2] += u32::from(bgra[i]);
            }
        }
        let count = (y1 - y0) * (x1 - x0);
        sum.map(|channel| (channel / count) as u8)
    }

    fn fixture(size: (u32, u32)) -> Vec<u8> {
        let mut frame = Vec::new();
        for y in 0..size.1 {
            for x in 0..size.0 {
                frame.extend_from_slice(&[
                    (x * 73 + y * 31) as u8,
                    (x * 23 + y * 97) as u8,
                    (x * 117 + y * 11) as u8,
                    if (x + y).is_multiple_of(2) { 0 } else { 255 },
                ]);
            }
        }
        frame
    }

    fn check_color(size: (u32, u32), output: (u32, u32)) {
        let frame = fixture(size);
        let scaler = Scaler::new(size, output);
        for y in 0..output.1 {
            for x in 0..output.0 {
                assert_eq!(
                    scaler.color(&frame, x, y),
                    old_color(&frame, size, output, (x, y)),
                    "源 {size:?}，输出 {output:?}，坐标 ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn gif_scaler_integer_ratio_matches_original_area_average() {
        for (size, output) in [((8, 6), (4, 3)), ((12, 8), (3, 2)), ((2, 1), (1, 1))] {
            check_color(size, output);
        }
        let frame = [0, 0, 200, 255, 0, 0, 100, 0];
        assert_eq!(Scaler::new((2, 1), (1, 1)).color(&frame, 0, 0), [150, 0, 0]);
    }

    #[test]
    fn gif_scaler_noninteger_odd_sizes_match_original_every_pixel() {
        for (size, output) in [
            ((11, 7), (6, 4)),
            ((13, 9), (7, 5)),
            ((3, 5), (1, 2)),
            ((5, 3), (8, 7)),
        ] {
            check_color(size, output);
        }
    }

    #[test]
    fn gif_scaler_unscaled_single_pixel_and_alpha_match_original() {
        for size in [(1, 1), (1, 9), (13, 1), (13, 9)] {
            check_color(size, size);
        }
    }

    #[test]
    fn gif_scaler_indices_match_original_color_mapping() {
        let mut histogram = Histogram::new();
        for color in [[0, 0, 0], [255, 255, 255], [255, 0, 0], [31, 113, 241]] {
            histogram.sample_rgb(color);
        }
        let palette = histogram.build(&mut |_| true).unwrap();
        for (size, output) in [((13, 9), (7, 5)), ((8, 6), (4, 3)), ((11, 7), (11, 7))] {
            let frame = fixture(size);
            let scaler = Scaler::new(size, output);
            let expected: Vec<u8> = (0..output.1)
                .flat_map(|y| {
                    let frame = &frame;
                    let palette = &palette;
                    (0..output.0)
                        .map(move |x| palette.index(old_color(frame, size, output, (x, y))))
                })
                .collect();
            assert_eq!(scaler.index(&frame, &palette), expected);
        }
    }

    #[test]
    fn gif_scaler_global_sampling_keeps_edges_and_weighting() {
        for (size, output) in [((13, 9), (7, 5)), ((8, 6), (4, 3)), ((5, 3), (5, 3))] {
            let frame = fixture(size);
            let scaler = Scaler::new(size, output);
            let mut current = Histogram::new();
            scaler.accumulate(&mut current, &frame);
            let mut old = Histogram::new();
            for y in (0..output.1).step_by(4) {
                for x in (0..output.0).step_by(4) {
                    old.sample_rgb(old_color(&frame, size, output, (x, y)));
                }
                old.sample_rgb(old_color(&frame, size, output, (output.0 - 1, y)));
            }
            for x in (0..output.0).step_by(4) {
                old.sample_rgb(old_color(&frame, size, output, (x, output.1 - 1)));
            }
            old.sample_rgb(old_color(
                &frame,
                size,
                output,
                (output.0 - 1, output.1 - 1),
            ));
            assert_eq!(current.samples(), old.samples());
            let current = current.build(&mut |_| true).unwrap();
            let old = old.build(&mut |_| true).unwrap();
            assert_eq!(current.table, old.table);
            for key in 0_u32..1 << 15 {
                let expand = |value: u32| ((value << 3) | (value >> 2)) as u8;
                let rgb = [expand(key >> 10), expand((key >> 5) & 31), expand(key & 31)];
                assert_eq!(current.index(rgb), old.index(rgb));
            }
        }
    }
}

// 测试 oracle 为本批冻结生产原方法，只有方法名作一处替换。
#[cfg(test)]
impl Scaler {
    fn original_accumulate_keys_with(
        &self,
        histogram: &mut Histogram,
        bgra: &[u8],
        proceed: &mut dyn FnMut() -> bool,
    ) -> crate::Result<Vec<u16>> {
        const CANCEL_ROWS: u32 = 16;
        let (width, height) = self.output;
        let mut keys = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            if y.is_multiple_of(CANCEL_ROWS) && !proceed() {
                return Err("已取消 GIF 转换".into());
            }
            let row_weight = u32::from(y.is_multiple_of(4)) + u32::from(y == height - 1);
            for x in 0..width {
                let rgb = self.color(bgra, x, y);
                keys.push(crate::palette::key(rgb) as u16);
                let column_weight = u32::from(x.is_multiple_of(4)) + u32::from(x == width - 1);
                for _ in 0..row_weight * column_weight {
                    histogram.sample_rgb(rgb);
                }
            }
        }
        if !proceed() {
            return Err("已取消 GIF 转换".into());
        }
        Ok(keys)
    }
}

// 新增热点候选验证；旧测试与旧通用路径不作修改。
#[cfg(test)]
mod identity_key_prepare_tests {
    use super::*;

    fn frame(size: (u32, u32)) -> Vec<u8> {
        let mut bgra = Vec::with_capacity(size.0 as usize * size.1 as usize * 4);
        for y in 0..size.1 {
            for x in 0..size.0 {
                bgra.extend_from_slice(&[
                    (x * 73 + y * 31) as u8,
                    (x * 23 + y * 97) as u8,
                    (x * 117 + y * 11) as u8,
                    (x * 19 + y * 5) as u8,
                ]);
            }
        }
        bgra
    }

    #[test]
    fn gif_identity_keys_continuous_rows_equal_original_all_samples_and_buckets() {
        for size in [
            (1, 1),
            (1, 33),
            (33, 1),
            (4, 4),
            (5, 5),
            (9, 9),
            (17, 33),
            (31, 35),
            (512, 64),
        ] {
            let mut bgra = frame(size);
            if size == (512, 64) {
                // 覆盖全部 RGB555 桶，桶内低位仍不同，避免只比较桶中心色。
                for (key, pixel) in bgra.chunks_exact_mut(4).enumerate() {
                    pixel[0] = (((key & 31) << 3) | ((key >> 3) & 7)) as u8;
                    pixel[1] = ((((key >> 5) & 31) << 3) | ((key >> 6) & 7)) as u8;
                    pixel[2] = (((key >> 10) << 3) | (key & 7)) as u8;
                }
            }
            // 解码器可能给出更长容量／尾部，实际读取仍严格限于该帧尺寸。
            bgra.extend_from_slice(&[3, 7, 11, 13]);
            let scaler = Scaler::new(size, size);
            let mut original = Histogram::new();
            let mut candidate = Histogram::new();
            original.sample_rgb([251, 252, 253]);
            candidate.sample_rgb([251, 252, 253]);
            let original_keys = scaler
                .original_accumulate_keys_with(&mut original, &bgra, &mut || true)
                .unwrap();
            let candidate_keys = scaler
                .accumulate_keys_with(&mut candidate, &bgra, &mut || true)
                .unwrap();
            assert_eq!(candidate_keys, original_keys, "{size:?}");
            assert_eq!(candidate.samples(), original.samples(), "{size:?}");
        }
    }

    #[test]
    fn gif_identity_keys_keep_each_cancellation_checkpoint_and_partial_histogram() {
        for size in [(1, 33), (17, 33), (9, 17), (31, 16)] {
            let bgra = frame(size);
            let scaler = Scaler::new(size, size);
            let checkpoints = (size.1 - 1) / 16 + 2;
            for stop in 1..=checkpoints {
                let mut original = Histogram::new();
                let mut candidate = Histogram::new();
                original.sample_rgb([251, 252, 253]);
                candidate.sample_rgb([251, 252, 253]);
                let mut original_checks = 0;
                let mut candidate_checks = 0;
                let original_result =
                    scaler.original_accumulate_keys_with(&mut original, &bgra, &mut || {
                        original_checks += 1;
                        original_checks != stop
                    });
                let candidate_result =
                    scaler.accumulate_keys_with(&mut candidate, &bgra, &mut || {
                        candidate_checks += 1;
                        candidate_checks != stop
                    });
                assert_eq!(original_checks, stop);
                assert_eq!(candidate_checks, original_checks);
                assert_eq!(
                    candidate_result.unwrap_err().to_string(),
                    original_result.unwrap_err().to_string(),
                );
                assert_eq!(candidate.samples(), original.samples(), "{size:?}, {stop}");
            }
        }
    }
}
