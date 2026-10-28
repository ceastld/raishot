//! 局部悬浮元素候选只辅助可靠接缝排序，不确认固定元素，也不改写像素。

const MIN_RUN: u32 = 8;
const MIN_ROWS: u32 = 4;
const MAX_CANDIDATES: usize = 64;
const MARGIN: u32 = 4;

struct Candidate {
    left: u32,
    right: u32,
    top: u32,
    bottom: u32,
    last_row: u32,
    rows: u32,
}

/// 输入为逻辑帧灰度：x 为跨轴、y 为滚动轴；返回沿滚动轴的半开区间。
/// 仅由调用方已证明可信的非零位移产生候选；无状态候选不能作为擦除依据。
pub(super) fn spans(
    prev: &[u8],
    curr: &[u8],
    size: (u32, u32),
    borders: (u32, u32),
    offset: i32,
) -> Vec<(i32, i32)> {
    let (width, height) = size;
    let (top, bottom) = borders;
    let Some(end) = height.checked_sub(bottom) else {
        return Vec::new();
    };
    let Some(pixels) = (width as usize).checked_mul(height as usize) else {
        return Vec::new();
    };
    let max_width = width / 3;
    let max_height = height / 3;
    if offset == 0
        || top >= end
        || height > i32::MAX as u32
        || offset.unsigned_abs() >= end - top
        || max_width < MIN_RUN
        || max_height < MIN_ROWS
        || prev.len() != pixels
        || curr.len() != pixels
    {
        return Vec::new();
    }
    // 小步滚动只暴露纯色按钮的两端，允许有限空行将它们连成完整局部框。
    let max_gap = (max_height / 2).clamp(4, 96);
    let limits = (max_width, max_height, max_gap);
    let mut candidates = Vec::with_capacity(16);
    let stride = width as usize;
    for y in top..end {
        let row = y as usize * stride;
        let previous_y = y as i64 + offset as i64;
        let current_y = y as i64 - offset as i64;
        let previous_row = (top as i64..end as i64)
            .contains(&previous_y)
            .then(|| previous_y as usize * stride);
        let current_row = (top as i64..end as i64)
            .contains(&current_y)
            .then(|| current_y as usize * stride);
        let mut start = None;
        for x in 0..width {
            let index = row + x as usize;
            let stable = prev[index].abs_diff(curr[index]) <= 2;
            let displaced = stable
                && (previous_row
                    .is_some_and(|base| curr[index].abs_diff(prev[base + x as usize]) >= 16)
                    || current_row
                        .is_some_and(|base| prev[index].abs_diff(curr[base + x as usize]) >= 16));
            if displaced {
                start.get_or_insert(x);
            } else if let Some(left) = start.take() {
                if !add_run(&mut candidates, left, x, y, limits) {
                    return Vec::new();
                }
            }
        }
        if let Some(left) = start {
            if !add_run(&mut candidates, left, width, y, limits) {
                return Vec::new();
            }
        }
    }
    let mut output: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| candidate.rows >= MIN_ROWS)
        .map(|candidate| {
            (
                candidate.top.saturating_sub(MARGIN).max(top) as i32,
                candidate.bottom.saturating_add(MARGIN).min(end) as i32,
            )
        })
        .collect();
    output.sort_unstable();
    let mut count = 0;
    for index in 0..output.len() {
        let span = output[index];
        if count > 0 && span.0 <= output[count - 1].1 {
            output[count - 1].1 = output[count - 1].1.max(span.1);
        } else {
            output[count] = span;
            count += 1;
        }
    }
    output.truncate(count);
    output
}

fn add_run(
    candidates: &mut Vec<Candidate>,
    left: u32,
    right: u32,
    y: u32,
    limits: (u32, u32, u32),
) -> bool {
    let (max_width, max_height, max_gap) = limits;
    if right - left < MIN_RUN || right - left > max_width {
        return true;
    }
    let matching = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            y - candidate.last_row <= max_gap + 1
                && left < candidate.right
                && right > candidate.left
                && right.max(candidate.right) - left.min(candidate.left) <= max_width
                && y + 1 - candidate.top <= max_height
        })
        .max_by_key(|(_, candidate)| {
            (
                candidate.last_row,
                right.min(candidate.right) - left.max(candidate.left),
            )
        })
        .map(|(index, _)| index);
    if let Some(index) = matching {
        let candidate = &mut candidates[index];
        candidate.left = candidate.left.min(left);
        candidate.right = candidate.right.max(right);
        candidate.bottom = y + 1;
        candidate.rows += u32::from(candidate.last_row != y);
        candidate.last_row = y;
    } else {
        if candidates.len() == MAX_CANDIDATES {
            return false;
        }
        candidates.push(Candidate {
            left,
            right,
            top: y,
            bottom: y + 1,
            last_row: y,
            rows: 1,
        });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::spans;

    fn button_frame(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = vec![240; width as usize * height as usize];
        for y in 56..96 {
            for x in 80..104 {
                pixels[(y * width + x) as usize] = 40;
            }
        }
        pixels
    }

    #[test]
    fn small_scroll_connects_both_edges_of_a_solid_button() {
        let frame = button_frame(144, 192);
        for offset in [4, -4] {
            let candidates = spans(&frame, &frame, (144, 192), (0, 0), offset);
            assert!(
                candidates
                    .iter()
                    .any(|&(top, bottom)| top <= 56 && bottom >= 96),
                "按钮中部不能落在上下边缘候选之间：{candidates:?}"
            );
        }
    }

    #[test]
    fn zero_motion_and_plain_background_do_not_make_candidates() {
        let frame = button_frame(144, 192);
        assert!(spans(&frame, &frame, (144, 192), (0, 0), 0).is_empty());
        let plain = vec![240; 144 * 192];
        assert!(spans(&plain, &plain, (144, 192), (0, 0), 12).is_empty());
        assert!(spans(&frame, &frame, (144, 192), (100, 0), 4).is_empty());
    }
}
