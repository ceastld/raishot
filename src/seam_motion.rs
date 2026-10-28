//! 逐项对应 SeamAlignmentHelper 的可信历史与接缝预测函数。
use super::OffsetHistoryItem;

fn trusted_values(history: &[OffsetHistoryItem]) -> Vec<i32> {
    let last_sign = history
        .iter()
        .rev()
        .find(|item| item.is_trusted && item.offset != 0)
        .map_or(0, |item| item.offset.signum());
    if last_sign == 0 {
        return Vec::new();
    }
    let mut values = Vec::with_capacity(history.len().min(5));
    for item in history.iter().rev() {
        if !item.is_trusted || item.offset == 0 {
            continue;
        }
        if item.offset.signum() != last_sign {
            break;
        }
        values.push(item.offset);
        if values.len() == history.len().min(5) {
            break;
        }
    }
    values.sort_unstable();
    values
}
pub(super) fn predict_next_offset(history: &[OffsetHistoryItem]) -> i32 {
    let values = trusted_values(history);
    if values.is_empty() {
        0
    } else {
        values[values.len() / 2]
    }
}
pub(super) fn try_get_stable_prediction(history: &[OffsetHistoryItem]) -> Option<i32> {
    if history.len() < 2 {
        return None;
    }
    let values = trusted_values(history);
    if values.len() < 2 {
        return None;
    }
    let median = values[values.len() / 2];
    let tolerance = 2.max((median.abs() as f64 * 0.15).ceil() as i32);
    values
        .iter()
        .all(|&value| (value - median).abs() <= tolerance)
        .then_some(median)
}
pub(super) fn has_trusted_direction(history: &[OffsetHistoryItem], offset: i32) -> bool {
    offset != 0
        && history.len() >= 2
        && history[history.len() - 2..]
            .iter()
            .all(|item| item.is_trusted && item.offset.signum() == offset.signum())
}
pub(super) fn predict_seam_in_overlap(
    history: &[OffsetHistoryItem],
    offset: i32,
    overlap: i32,
) -> i32 {
    let Some(last) = history.last() else {
        return -1;
    };
    if overlap <= 0
        || offset == 0
        || last.seam_row_in_curr < 0
        || last.offset.signum() != offset.signum()
    {
        return -1;
    }
    (last.seam_row_in_curr + last.offset - offset).clamp(0, overlap - 1)
}
pub(super) fn choose_seam_row(diffs: &[f32], offset: i32, prediction: i32) -> i32 {
    const EPSILON: f32 = 1e-6;
    const MAX_REGRESSION: f32 = 2.5;
    if diffs.is_empty() {
        return 0;
    }
    let length = diffs.len() as i32;
    let global_min = diffs.iter().copied().fold(f32::MAX, f32::min);
    if (0..length).contains(&prediction) {
        let radius = 16.max(length / 16);
        let lo = 0.max(prediction - radius);
        let hi = (length - 1).min(prediction + radius);
        let mut best = prediction;
        let mut best_value = f32::MAX;
        for index in lo..=hi {
            let value = diffs[index as usize];
            if value < best_value - EPSILON
                || ((value - best_value).abs() <= EPSILON
                    && (index - prediction).abs() < (best - prediction).abs())
            {
                best_value = value;
                best = index;
            }
        }
        if best_value <= global_min + MAX_REGRESSION {
            return best;
        }
    }
    let mut fallback = if offset > 0 { length - 1 } else { 0 };
    let mut fallback_value = f32::MAX;
    for (index, &value) in diffs.iter().enumerate() {
        if value > global_min + EPSILON {
            continue;
        }
        let index = index as i32;
        let tie_better = if prediction >= 0 {
            (index - prediction).abs() < (fallback - prediction).abs()
        } else if offset > 0 {
            index > fallback
        } else if offset < 0 {
            index < fallback
        } else {
            false
        };
        if value < fallback_value - EPSILON
            || ((value - fallback_value).abs() <= EPSILON && tie_better)
        {
            fallback_value = value;
            fallback = index;
        }
    }
    fallback
}
