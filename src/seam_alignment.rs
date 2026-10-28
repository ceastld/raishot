//! 长截图接缝对齐：逐阶段的候选搜索与可信度判定。
//! 保留空间行特征、全域粗搜/精修、SAD 接缝及视觉歧义/可信运动历史门控。
//! 复用 Vec 工作区；没有使用旧的行指纹拼接算法。
#[path = "seam_motion.rs"]
mod motion;
#[path = "seam_scores.rs"]
mod scores;

use scores::{FineScore, Profiles};

const MAX_COARSE_SEED_COUNT: usize = 6;
const FINE_SEED_RADIUS: i32 = 2;
const AMBIGUITY_ABSOLUTE_MARGIN: f64 = 0.35;
const AMBIGUITY_RELATIVE_MARGIN: f64 = 0.12;
const MAX_CANDIDATE_SELECTION_SCORE: f64 = 8.;

#[derive(Clone, Copy, Debug)]
pub(crate) struct OverlapRect {
    pub top: i32,
    pub bottom: i32,
    /// 完整矩形诊断，供数值对照使用。
    #[allow(dead_code)]
    pub left: i32,
    #[allow(dead_code)]
    pub right: i32,
}
impl OverlapRect {
    pub(crate) fn height(self) -> i32 {
        self.bottom - self.top
    }
    /// 矩形诊断接口，供数值对照使用。
    #[allow(dead_code)]
    pub(crate) fn width(self) -> i32 {
        self.right - self.left
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AlignResult {
    pub offset: i32,
    pub rect_in_prev: OverlapRect,
    /// 完整诊断，供数值对照使用。
    #[allow(dead_code)]
    pub rect_in_curr: OverlapRect,
    pub score: f64,
    pub seam_row_in_prev: i32,
    pub seam_row_in_curr: i32,
    pub min_seam_row_diff: f32,
    pub matched_row_ratio: f64,
    pub selection_score: f64,
    pub second_best_selection_score: f64,
    pub is_ambiguous: bool,
    pub is_visually_unique: bool,
    /// 以下诊断字段只供数值对照，不参与额外匹配决策。
    #[allow(dead_code)]
    pub resolved_by_prediction: bool,
    #[allow(dead_code)]
    pub finalist_count: usize,
    pub has_stable_motion_prior: bool,
    #[allow(dead_code)]
    pub second_best_offset: i32,
    #[allow(dead_code)]
    pub visual_mismatch_ratio: f64,
    #[allow(dead_code)]
    pub alternative_mismatch_ratio: f64,
    #[allow(dead_code)]
    pub alternative_evidence_blocks: u32,
    pub has_verified_short_overlap: bool,
}
impl AlignResult {
    pub(crate) const MIN_MATCHED_ROW_RATIO: f64 = 0.55;
    pub(crate) const MIN_BOOTSTRAP_OVERLAP_RATIO: f64 = 0.40;
    pub(crate) const MIN_SHORT_OVERLAP_MATCHED_ROW_RATIO: f64 = 0.72;
    pub(crate) fn is_acceptable(self, max_alignment_score: f64, max_seam_row_diff: f32) -> bool {
        if self.is_ambiguous
            || self.min_seam_row_diff > max_seam_row_diff
            || self.matched_row_ratio < Self::MIN_MATCHED_ROW_RATIO
        {
            return false;
        }
        let frame_height = self.rect_in_prev.height() + self.offset.abs();
        let overlap_ratio = if frame_height > 0 {
            self.rect_in_prev.height() as f64 / frame_height as f64
        } else {
            0.
        };
        if overlap_ratio < Self::MIN_BOOTSTRAP_OVERLAP_RATIO
            && !self.has_verified_short_overlap
            && (!self.has_stable_motion_prior
                || self.matched_row_ratio < Self::MIN_SHORT_OVERLAP_MATCHED_ROW_RATIO)
        {
            return false;
        }
        self.selection_score <= max_alignment_score
    }
    pub(crate) fn as_ambiguous(self, alternative_score: f64) -> Self {
        Self {
            second_best_selection_score: self.second_best_selection_score.min(alternative_score),
            is_ambiguous: true,
            is_visually_unique: false,
            resolved_by_prediction: false,
            has_verified_short_overlap: false,
            ..self
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OffsetHistoryItem {
    /// 历史诊断，供数值对照使用。
    #[allow(dead_code)]
    pub frame_index: i32,
    pub offset: i32,
    #[allow(dead_code)]
    pub diff: f64,
    pub seam_row_in_curr: i32,
    pub is_trusted: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AlignmentOptions {
    pub min_overlap: u32,
    pub predict_offset: i32,
    /// 只为调用兼容保留，视觉唯一性校验取代此早退阈值。
    pub approx_diff: f64,
}
impl Default for AlignmentOptions {
    fn default() -> Self {
        Self {
            min_overlap: 30,
            predict_offset: 0,
            approx_diff: 0.2,
        }
    }
}

#[derive(Default, Debug)]
pub(crate) struct AlignmentWorkspace {
    prev_profile: Vec<f32>,
    curr_profile: Vec<f32>,
    prev_spatial: Vec<f32>,
    curr_spatial: Vec<f32>,
    row_diffs: Vec<f32>,
    sort_keys: Vec<i32>,
    coarse_scores: Vec<f64>,
}

pub(crate) fn predict_next_offset(history: &[OffsetHistoryItem], _expect_offset: i32) -> i32 {
    motion::predict_next_offset(history)
}
pub(crate) fn try_get_stable_prediction(history: &[OffsetHistoryItem]) -> Option<i32> {
    motion::try_get_stable_prediction(history)
}
pub(crate) fn predict_seam_in_overlap(
    history: &[OffsetHistoryItem],
    offset: i32,
    overlap_height: i32,
) -> i32 {
    motion::predict_seam_in_overlap(history, offset, overlap_height)
}

fn better_candidate(
    score: f64,
    offset: i32,
    current_score: f64,
    current_offset: i32,
    preferred: i32,
) -> bool {
    if score < current_score - 1e-9 {
        return true;
    }
    if (score - current_score).abs() > 1e-9 {
        return false;
    }
    let distance = (offset - preferred).abs();
    let current_distance = (current_offset - preferred).abs();
    distance < current_distance
        || (distance == current_distance && offset.abs() < current_offset.abs())
        || (distance == current_distance
            && offset.abs() == current_offset.abs()
            && offset > current_offset)
}

pub(crate) fn find_best_offset(
    prev: &[u8],
    curr: &[u8],
    width: u32,
    height: u32,
    options: AlignmentOptions,
    history: &[OffsetHistoryItem],
    scratch: &mut AlignmentWorkspace,
) -> Result<AlignResult, &'static str> {
    let width_usize = width as usize;
    let height_usize = height as usize;
    let Some(expected) = width_usize.checked_mul(height_usize) else {
        return Err("灰度帧尺寸溢出");
    };
    if width == 0
        || width > i32::MAX as u32
        || height == 0
        || height > i32::MAX as u32
        || options.min_overlap == 0
        || options.min_overlap > height
        || prev.len() != curr.len()
        || prev.len() < expected
    {
        return Err("灰度帧尺寸或最小重叠无效");
    }
    let h = height as i32;
    let min_overlap = options.min_overlap as i32;
    let max_offset = h - min_overlap;
    let band_count = width_usize.min(16);
    scratch.prev_profile.resize(height_usize, 0.);
    scratch.curr_profile.resize(height_usize, 0.);
    scratch.row_diffs.resize(height_usize.max(1), 0.);
    scratch.sort_keys.resize(height_usize.max(1), 0);
    scratch.prev_spatial.resize(height_usize * band_count, 0.);
    scratch.curr_spatial.resize(height_usize * band_count, 0.);
    scratch
        .coarse_scores
        .resize(max_offset as usize * 2 + 1, 0.);
    scores::compute_spatial_row_profile(
        prev,
        width_usize,
        height_usize,
        band_count,
        &mut scratch.prev_profile,
        &mut scratch.prev_spatial,
    );
    scores::compute_spatial_row_profile(
        curr,
        width_usize,
        height_usize,
        band_count,
        &mut scratch.curr_profile,
        &mut scratch.curr_spatial,
    );
    let profiles = Profiles {
        prev: &scratch.prev_profile,
        curr: &scratch.curr_profile,
        prev_spatial: &scratch.prev_spatial,
        curr_spatial: &scratch.curr_spatial,
        height: height_usize,
        band_count,
        exact_bands: band_count == 16 && width_usize <= 16 << 16,
    };
    let prev_anchors = profiles.select_salient_rows(false);
    let curr_anchors = profiles.select_salient_rows(true);
    let stable_prediction = try_get_stable_prediction(history);
    let preferred = stable_prediction
        .unwrap_or(options.predict_offset)
        .clamp(-max_offset, max_offset);
    let preferred_overlap = options.min_overlap.max(height / 8) as usize;
    for offset in -max_offset..=max_offset {
        scratch.coarse_scores[(offset + max_offset) as usize] =
            profiles.coarse_score(offset, preferred_overlap, &prev_anchors, &curr_anchors);
    }
    let distinct_distance = 4.max(min_overlap / 4);
    let mut seeds: Vec<i32> = Vec::with_capacity(MAX_COARSE_SEED_COUNT);
    while seeds.len() < MAX_COARSE_SEED_COUNT {
        let mut seed = None;
        let mut seed_score = f64::INFINITY;
        for offset in -max_offset..=max_offset {
            if seeds
                .iter()
                .any(|&s| (offset - s).abs() < distinct_distance)
            {
                continue;
            }
            let score = scratch.coarse_scores[(offset + max_offset) as usize];
            if better_candidate(score, offset, seed_score, seed.unwrap_or(0), preferred) {
                seed = Some(offset);
                seed_score = score;
            }
        }
        let Some(seed) = seed else {
            break;
        };
        seeds.push(seed);
    }
    let mut finalists: Vec<i32> = Vec::with_capacity(MAX_COARSE_SEED_COUNT * 5 + 4);
    let mut add = |offset: i32| {
        if (-max_offset..=max_offset).contains(&offset) && !finalists.contains(&offset) {
            finalists.push(offset);
        }
    };
    for seed in seeds {
        for delta in -FINE_SEED_RADIUS..=FINE_SEED_RADIUS {
            add(seed + delta);
        }
    }
    add(preferred);
    add(0);
    let mut best_positive = (f64::INFINITY, 0);
    let mut best_negative = (f64::INFINITY, 0);
    for offset in 1..=max_offset {
        let positive = scratch.coarse_scores[(offset + max_offset) as usize];
        let negative = scratch.coarse_scores[(-offset + max_offset) as usize];
        if positive < best_positive.0 {
            best_positive = (positive, offset);
        }
        if negative < best_negative.0 {
            best_negative = (negative, -offset);
        }
    }
    if max_offset > 0 {
        add(best_positive.1);
        add(best_negative.1);
    }
    let fine: Vec<FineScore> = finalists
        .iter()
        .map(|&offset| profiles.fine_score(offset, preferred_overlap, &mut scratch.sort_keys))
        .collect();
    let prediction_tolerance =
        stable_prediction.map_or(0, |p| 4.max((p.abs() as f64 * 0.15).ceil() as i32));
    let near_stable = |index: usize| {
        stable_prediction.is_some_and(|p| {
            p != 0
                && finalists[index] != 0
                && finalists[index].signum() == p.signum()
                && (finalists[index] - p).abs() <= prediction_tolerance
        })
    };
    let visual_quality = |index: usize| {
        let overlap_ratio = (h - finalists[index].abs()) as f64 / h as f64;
        let matched_limit = if overlap_ratio < AlignResult::MIN_BOOTSTRAP_OVERLAP_RATIO {
            AlignResult::MIN_SHORT_OVERLAP_MATCHED_ROW_RATIO
        } else {
            AlignResult::MIN_MATCHED_ROW_RATIO
        };
        fine[index].selection <= MAX_CANDIDATE_SELECTION_SCORE
            && fine[index].matched_ratio >= matched_limit
    };
    let motion_admissible = |index: usize| {
        (h - finalists[index].abs()) as f64 / h as f64 >= AlignResult::MIN_BOOTSTRAP_OVERLAP_RATIO
            || near_stable(index)
    };
    let mut visual_best = None;
    for validated in [true, false] {
        for index in 0..finalists.len() {
            if validated && !visual_quality(index) {
                continue;
            }
            if visual_best.is_none_or(|current: usize| {
                better_candidate(
                    fine[index].selection,
                    finalists[index],
                    fine[current].selection,
                    finalists[current],
                    preferred,
                )
            }) {
                visual_best = Some(index);
            }
        }
        if visual_best.is_some() {
            break;
        }
    }
    let visual_best = visual_best.expect("至少存在零位移候选");
    let best_selection = fine[visual_best].selection;
    let near_equal_limit =
        best_selection + AMBIGUITY_ABSOLUTE_MARGIN.max(best_selection * AMBIGUITY_RELATIVE_MARGIN);
    let visually_defeated = |index: usize| {
        let gap = fine[index].selection - best_selection;
        if gap >= 0.12f64.max(best_selection * 0.10) {
            return true;
        }
        let mismatch_gap = fine[index].mismatch_ratio - fine[visual_best].mismatch_ratio;
        let new_evidence = fine[index].mismatch_blocks & !fine[visual_best].mismatch_blocks;
        gap >= 0.03 && mismatch_gap >= 0.015 && new_evidence.count_ones() >= 3
    };
    let visual_alternative = |index: usize| {
        index != visual_best
            && visual_quality(index)
            && (finalists[index] - finalists[visual_best]).abs() >= distinct_distance
            && fine[index].selection <= near_equal_limit
            && !visually_defeated(index)
    };
    let mut has_visual_alias = false;
    let mut has_near_equal_alternative = false;
    let mut has_direction_conflict = false;
    for index in 0..finalists.len() {
        if !visual_alternative(index) {
            continue;
        }
        has_visual_alias = true;
        if !motion_admissible(index) {
            continue;
        }
        has_near_equal_alternative = true;
        let best = finalists[visual_best];
        let alternative = finalists[index];
        if best == 0 || alternative == 0 || best.signum() != alternative.signum() {
            has_direction_conflict = true;
        }
    }
    let mut predicted = None;
    if has_near_equal_alternative && !has_direction_conflict {
        for index in 0..finalists.len() {
            if index != visual_best && (!visual_alternative(index) || !motion_admissible(index)) {
                continue;
            }
            if !near_stable(index) {
                continue;
            }
            let prediction = stable_prediction.expect("可信预测");
            if predicted.is_none_or(|current: usize| {
                let distance = (finalists[index] - prediction).abs();
                let current_distance = (finalists[current] - prediction).abs();
                distance < current_distance
                    || (distance == current_distance
                        && fine[index].selection < fine[current].selection)
            }) {
                predicted = Some(index);
            }
        }
    }
    let resolved_by_prediction =
        has_near_equal_alternative && !has_direction_conflict && predicted.is_some();
    let selected = if resolved_by_prediction {
        predicted.expect("预测候选")
    } else {
        visual_best
    };
    let mut second_score = f64::INFINITY;
    let mut second_offset = finalists[selected];
    let mut alternative_mismatch = f64::INFINITY;
    let mut alternative_evidence = 0;
    for index in 0..finalists.len() {
        if index == selected
            || !visual_quality(index)
            || (finalists[index] - finalists[selected]).abs() < distinct_distance
        {
            continue;
        }
        if fine[index].selection < second_score {
            second_score = fine[index].selection;
            second_offset = finalists[index];
            alternative_mismatch = fine[index].mismatch_ratio;
            alternative_evidence =
                (fine[index].mismatch_blocks & !fine[visual_best].mismatch_blocks).count_ones();
        }
    }
    let offset = finalists[selected];
    let (start_prev, start_curr) = if offset >= 0 {
        (offset, 0)
    } else {
        (0, -offset)
    };
    let overlap = h - offset.abs();
    let rect_prev = OverlapRect {
        top: start_prev,
        bottom: start_prev + overlap,
        left: 0,
        right: width as i32,
    };
    let rect_curr = OverlapRect {
        top: start_curr,
        bottom: start_curr + overlap,
        left: 0,
        right: width as i32,
    };
    scores::compute_row_diffs(
        prev,
        curr,
        width_usize,
        start_prev as usize,
        start_curr as usize,
        &mut scratch.row_diffs[..overlap as usize],
    );
    let seam_prediction = predict_seam_in_overlap(history, offset, overlap);
    let seam = motion::choose_seam_row(
        &scratch.row_diffs[..overlap as usize],
        offset,
        seam_prediction,
    );
    let diffs = &scratch.row_diffs[..overlap as usize];
    let min_seam_row_diff = diffs.iter().copied().fold(f32::MAX, f32::min);
    let matched_row_ratio =
        diffs.iter().filter(|&&diff| diff <= 15.).count() as f64 / overlap as f64;
    let exact_overlap = diffs.iter().all(|&diff| diff == 0.);
    let is_visually_unique = !has_visual_alias;
    let has_verified_short_overlap = is_visually_unique
        && exact_overlap
        && overlap >= min_overlap.max(64).max((h as f64 * 0.20).ceil() as i32)
        && motion::has_trusted_direction(history, offset);
    let _ = options.approx_diff;
    Ok(AlignResult {
        offset,
        rect_in_prev: rect_prev,
        rect_in_curr: rect_curr,
        score: fine[selected].profile,
        seam_row_in_prev: start_prev + seam,
        seam_row_in_curr: start_curr + seam,
        min_seam_row_diff,
        matched_row_ratio,
        selection_score: fine[selected].selection,
        second_best_selection_score: second_score,
        is_ambiguous: has_near_equal_alternative
            && (stable_prediction.is_none() || predicted.is_none() || has_direction_conflict),
        is_visually_unique,
        resolved_by_prediction,
        finalist_count: finalists.len(),
        has_stable_motion_prior: near_stable(selected),
        second_best_offset: second_offset,
        visual_mismatch_ratio: fine[visual_best].mismatch_ratio,
        alternative_mismatch_ratio: alternative_mismatch,
        alternative_evidence_blocks: alternative_evidence,
        has_verified_short_overlap,
    })
}

#[cfg(test)]
#[path = "seam_alignment_tests.rs"]
mod tests;
