//! 逐项对应 SeamAlignmentHelper 的全宽16带行特征、粗/精评分与全像素 SAD。
pub(super) fn compute_spatial_row_profile(
    gray: &[u8],
    width: usize,
    height: usize,
    bands: usize,
    profile: &mut [f32],
    spatial: &mut [f32],
) {
    for y in 0..height {
        let mut total = 0u64;
        for band in 0..bands {
            let start = band * width / bands;
            let end = (band + 1) * width / bands;
            let row = &gray[y * width + start..y * width + end];
            // 带宽不超过 65536 时 u32 不会溢出，和值相同而累加更容易向量化。
            let sum: u64 = if row.len() <= 1 << 16 {
                row.iter().map(|&v| v as u32).sum::<u32>() as u64
            } else {
                row.iter().map(|&v| v as u64).sum()
            };
            spatial[y * bands + band] = sum as f32 / (end - start).max(1) as f32;
            total += sum;
        }
        profile[y] = total as f32 / width as f32;
    }
}

pub(super) struct Profiles<'a> {
    pub prev: &'a [f32],
    pub curr: &'a [f32],
    pub prev_spatial: &'a [f32],
    pub curr_spatial: &'a [f32],
    pub height: usize,
    pub band_count: usize,
    /// 16 带且每带不超过 65536 列：可走 `row_difference16`，结果与逐项累加逐位相同。
    pub exact_bands: bool,
}
/// `f32::total_cmp` 的整数键：按 i32 排序与按 total_cmp 排序次序相同，相等键即同一位模式。
/// 原生整数排序比带比较闭包的浮点排序快一倍；变换自逆，取回的值逐位不变。
#[inline(always)]
fn sort_key(value: f32) -> i32 {
    let bits = value.to_bits() as i32;
    bits ^ (((bits >> 31) as u32) >> 1) as i32
}
#[inline(always)]
fn from_sort_key(key: i32) -> f32 {
    f32::from_bits(sort_key(f32::from_bits(key as u32)) as u32)
}
/// 16 带行差的快速路径。带均值是“整数和 ÷ 带宽”的 f32，非零时不小于 1/65536，
/// 因此每个差值都是 2^-39 的整数倍且小于 256；16 项及其任意部分和都在 f64 中精确，
/// 分组累加与逐项累加逐位相同。差值非负且不含 NaN，树形取最大与逐项比较结果相同；
/// 最大值出现两次以上时第二大即最大值，与逐项比较的并列规则一致。
#[inline(always)]
fn row_difference16(prev: &[f32], curr: &[f32]) -> (f64, f32, usize) {
    let prev: &[f32; 16] = prev.try_into().expect("16 带行特征");
    let curr: &[f32; 16] = curr.try_into().expect("16 带行特征");
    let mut diff = [0f32; 16];
    for band in 0..16 {
        diff[band] = (prev[band] - curr[band]).abs();
    }
    let mut lanes = [0f64; 4];
    for band in 0..16 {
        lanes[band % 4] += diff[band] as f64;
    }
    let sum = (lanes[0] + lanes[1]) + (lanes[2] + lanes[3]);
    let mismatches = diff.iter().filter(|&&value| value >= 6.).count();
    let largest = max16(&diff);
    let ties = diff.iter().filter(|&&value| value == largest).count();
    let second_largest = if ties > 1 {
        largest
    } else {
        max16(&diff.map(|value| if value < largest { value } else { 0. }))
    };
    let dropped = (largest + second_largest) as f64;
    (sum / 16., ((sum - dropped) / 14.) as f32, mismatches)
}
#[inline(always)]
fn max16(values: &[f32; 16]) -> f32 {
    let pick = |a: f32, b: f32| if a > b { a } else { b };
    let mut half = [0f32; 8];
    for i in 0..8 {
        half[i] = pick(values[i], values[i + 8]);
    }
    let mut quarter = [0f32; 4];
    for i in 0..4 {
        quarter[i] = pick(half[i], half[i + 4]);
    }
    pick(pick(quarter[0], quarter[2]), pick(quarter[1], quarter[3]))
}
pub(super) struct FineScore {
    pub profile: f64,
    pub selection: f64,
    pub matched_ratio: f64,
    pub mismatch_ratio: f64,
    pub mismatch_blocks: u16,
}
impl Profiles<'_> {
    pub(super) fn select_salient_rows(&self, current: bool) -> Vec<usize> {
        let (profile, spatial) = if current {
            (self.curr, self.curr_spatial)
        } else {
            (self.prev, self.prev_spatial)
        };
        let count = 16.min(self.height);
        (0..count)
            .map(|bucket| {
                let start = bucket * self.height / count;
                let end = (start + 1).max((bucket + 1) * self.height / count);
                let mut best_row = start;
                let mut best_salience = f64::NEG_INFINITY;
                for row in start..end {
                    let bands = &spatial[row * self.band_count..(row + 1) * self.band_count];
                    let minimum = bands.iter().copied().fold(f32::MAX, f32::min);
                    let maximum = bands.iter().copied().fold(f32::MIN, f32::max);
                    let mut vertical = 0f64;
                    if row > 0 {
                        vertical = vertical.max((profile[row] - profile[row - 1]).abs() as f64);
                    }
                    if row + 1 < self.height {
                        vertical = vertical.max((profile[row + 1] - profile[row]).abs() as f64);
                    }
                    let salience = (maximum - minimum) as f64 + vertical;
                    if salience > best_salience {
                        best_salience = salience;
                        best_row = row;
                    }
                }
                best_row
            })
            .collect()
    }
    #[inline(always)]
    fn row_difference(&self, prev_row: usize, curr_row: usize) -> (f64, f32, usize) {
        if self.exact_bands {
            return row_difference16(
                &self.prev_spatial[prev_row * 16..prev_row * 16 + 16],
                &self.curr_spatial[curr_row * 16..curr_row * 16 + 16],
            );
        }
        let mut sum = 0.;
        let mut largest = 0f32;
        let mut second_largest = 0f32;
        let mut mismatches = 0;
        for band in 0..self.band_count {
            let diff = (self.prev_spatial[prev_row * self.band_count + band]
                - self.curr_spatial[curr_row * self.band_count + band])
                .abs();
            sum += diff as f64;
            if diff >= 6. {
                mismatches += 1;
            }
            if diff >= largest {
                second_largest = largest;
                largest = diff;
            } else if diff > second_largest {
                second_largest = diff;
            }
        }
        let dropped = if self.band_count > 4 { 2 } else { 0 };
        let trimmed = ((sum
            - if dropped > 0 {
                (largest + second_largest) as f64
            } else {
                0.
            })
            / (self.band_count - dropped) as f64) as f32;
        (sum / self.band_count as f64, trimmed, mismatches)
    }
    pub(super) fn coarse_score(
        &self,
        offset: i32,
        preferred_overlap: usize,
        prev_anchors: &[usize],
        curr_anchors: &[usize],
    ) -> f64 {
        let start_prev = offset.max(0) as usize;
        let start_curr = (-offset).max(0) as usize;
        let overlap = self.height - offset.unsigned_abs() as usize;
        let samples = 32.min(overlap);
        let mut keys = [0i32; 32];
        let mut full_sum = 0.;
        // 采样行 sample * (overlap - 1) / (samples - 1) 按商和余数递推，免去逐行整除。
        let span = (samples - 1).max(1);
        let (step, carry) = if samples > 1 {
            ((overlap - 1) / span, (overlap - 1) % span)
        } else {
            (0, 0)
        };
        let (mut row, mut remainder) = (0, 0);
        for key in &mut keys[..samples] {
            let (full, trimmed, _) = self.row_difference(start_prev + row, start_curr + row);
            *key = sort_key(trimmed);
            full_sum += full;
            row += step;
            remainder += carry;
            if remainder >= span {
                row += 1;
                remainder -= span;
            }
        }
        let mut anchor_sum = 0.;
        let mut anchor_count = 0;
        for (anchors, start) in [(prev_anchors, start_prev), (curr_anchors, start_curr)] {
            for &anchor in anchors {
                let Some(row) = anchor.checked_sub(start).filter(|&row| row < overlap) else {
                    continue;
                };
                anchor_sum += self.row_difference(start_prev + row, start_curr + row).0;
                anchor_count += 1;
            }
        }
        keys[..samples].sort_unstable();
        let keep = ((samples as f64 * 0.70).ceil() as usize).max(1);
        let trimmed: f64 = keys[..keep].iter().map(|&k| from_sort_key(k) as f64).sum();
        let penalty = preferred_overlap.saturating_sub(overlap) as f64 / preferred_overlap as f64;
        let anchor_score = if anchor_count > 0 {
            anchor_sum / anchor_count as f64
        } else {
            0.
        };
        trimmed / keep as f64 + full_sum / samples as f64 * 0.05 + anchor_score * 0.20 + penalty
    }
    pub(super) fn fine_score(
        &self,
        offset: i32,
        preferred_overlap: usize,
        keys: &mut [i32],
    ) -> FineScore {
        let start_prev = offset.max(0) as usize;
        let start_curr = (-offset).max(0) as usize;
        let overlap = self.height - offset.unsigned_abs() as usize;
        let mut profile_sum = 0.;
        let mut full_sum = 0.;
        let mut matched = 0;
        let mut mismatch_cells = 0;
        let mut mismatch_blocks = 0u16;
        for (row, key) in keys[..overlap].iter_mut().enumerate() {
            profile_sum += (self.prev[start_prev + row] - self.curr[start_curr + row]).abs() as f64;
            let (full, trimmed, mismatches) =
                self.row_difference(start_prev + row, start_curr + row);
            *key = sort_key(trimmed);
            full_sum += full;
            if full <= 15. {
                matched += 1;
            }
            mismatch_cells += mismatches;
            if mismatches > 0 {
                mismatch_blocks |= 1 << (row * 16 / overlap).min(15);
            }
        }
        keys[..overlap].sort_unstable();
        let keep = ((overlap as f64 * 0.80).ceil() as usize).max(1);
        let trimmed: f64 = keys[..keep].iter().map(|&k| from_sort_key(k) as f64).sum();
        let profile = profile_sum / overlap as f64;
        let penalty = preferred_overlap.saturating_sub(overlap) as f64 / preferred_overlap as f64;
        FineScore {
            profile,
            selection: trimmed / keep as f64
                + full_sum / overlap as f64 * 0.10
                + profile * 0.05
                + penalty,
            matched_ratio: matched as f64 / overlap as f64,
            mismatch_ratio: mismatch_cells as f64 / (overlap * self.band_count) as f64,
            mismatch_blocks,
        }
    }
}

pub(super) fn compute_row_diffs(
    prev: &[u8],
    curr: &[u8],
    width: usize,
    start_prev: usize,
    start_curr: usize,
    output: &mut [f32],
) {
    for (row, value) in output.iter_mut().enumerate() {
        let a = &prev[(start_prev + row) * width..(start_prev + row + 1) * width];
        let b = &curr[(start_curr + row) * width..(start_curr + row + 1) * width];
        if a == b {
            *value = 0.;
            continue;
        }
        let sum: u64 = a.iter().zip(b).map(|(&a, &b)| a.abs_diff(b) as u64).sum();
        *value = sum as f32 / width as f32;
    }
}
