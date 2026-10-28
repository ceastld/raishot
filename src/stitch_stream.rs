//! 长截图拼接流：可信相邻帧与 seam 替换流程。
//! 长图保存共享条带；Clone 不复制像素和灰度，只有最终导出才物化整图。
use super::{
    ScrollAxis, StitchMatchQuality, StitchOptions, StitchSegment, StitchSnapshot, StitchUpdate,
    UpdateKind,
};
use crate::{
    capture::Image,
    geometry::Rect,
    seam_alignment::{self, AlignResult, AlignmentOptions, AlignmentWorkspace, OffsetHistoryItem},
    stitch_content::{self, StableContentBorderTracker},
    Result,
};
use std::sync::{Arc, Mutex};

const MAX_PIXELS: u64 = 32_000_000;
const MAX_SIDE: u32 = 65_535;
const MIN_OVERLAP: u32 = 30;

#[path = "stitch_floating.rs"]
mod floating;

#[derive(Clone)]
struct State {
    last: Arc<Image>,
    gray: Arc<[u8]>,
    borders: StableContentBorderTracker,
    viewport_top: u32,
    length: u32,
    segments: Vec<StitchSegment>,
    /// 与条带一一对应的原始采集视口原点；裁出小条带后仍能定位历史悬浮候选。
    source_viewports: Vec<i64>,
    history: Vec<OffsetHistoryItem>,
    accepted_count: i32,
}
#[derive(Default)]
struct Workspace {
    full: AlignmentWorkspace,
    content: AlignmentWorkspace,
}
#[derive(Clone)]
pub struct Stitcher {
    pub width: u32,
    pub height: u32,
    options: StitchOptions,
    cross: u32,
    frame_length: u32,
    state: State,
    previous: Option<State>,
    /// 只共享计算临时空间，不参与会话提交、可信历史或撤销。
    workspace: Arc<Mutex<Workspace>>,
}
impl Stitcher {
    pub fn new(image: Image) -> Self {
        Self::with_options(image, StitchOptions::default()).expect("invalid initial stitch frame")
    }
    pub fn with_options(image: Image, options: StitchOptions) -> Result<Self> {
        Self::with_options_shared(Arc::new(image), options)
    }
    /// 管线移交同一份像素；初始条带与可信比较基线共同持有该帧。
    pub fn with_options_shared(image: Arc<Image>, options: StitchOptions) -> Result<Self> {
        validate_image(&image)?;
        let (cross, frame_length) = options.axis.size(&image);
        if cross < 32 || frame_length < 64 {
            return Err(tr!(
                "长截图正文轴至少 64 像素，另一边至少 32 像素",
                "A long capture needs at least 64 pixels along the content axis and 32 on the other side"
            )
            .into());
        }
        if options.fixed_start.unwrap_or(0) as u64 + options.fixed_end.unwrap_or(0) as u64 + 64
            > frame_length as u64
        {
            return Err(tr!(
                "固定边带之后至少要保留 64 像素可滚动内容",
                "At least 64 pixels of scrollable content must remain after the fixed bands"
            )
            .into());
        }
        let gray = grayscale(&image, options.axis)?.into();
        let mut borders = StableContentBorderTracker::default();
        borders.reset();
        Ok(Self {
            width: image.width,
            height: image.height,
            state: State {
                segments: vec![StitchSegment {
                    image: image.clone(),
                    source: Rect::full(image.width, image.height),
                    origin: (0, 0),
                }],
                source_viewports: vec![0],
                last: image,
                gray,
                borders,
                viewport_top: 0,
                length: frame_length,
                history: Vec::new(),
                accepted_count: 0,
            },
            options,
            cross,
            frame_length,
            previous: None,
            workspace: Arc::new(Mutex::new(Workspace::default())),
        })
    }
    pub fn append(&mut self, image: Image) -> Result<u32> {
        let update = self.process(image)?;
        match update.kind {
            UpdateKind::Added | UpdateKind::ViewportOnly | UpdateKind::Unchanged => {
                Ok(update.added)
            }
            UpdateKind::Rejected | UpdateKind::Limit => {
                Err(update.reason.unwrap_or_default().into())
            }
        }
    }
    pub fn process(&mut self, image: Image) -> Result<StitchUpdate> {
        self.process_shared(Arc::new(image))
    }
    /// 选区迁移的零位移证明必须比较整帧，不能把匹配器的 delta=0 当作像素相同。
    pub(crate) fn matches_last_frame(&self, image: &Image) -> bool {
        image.width == self.state.last.width
            && image.height == self.state.last.height
            && image.bgra == self.state.last.bgra
    }
    /// 临时证明与队列仍持有帧时也不复制输入；匹配和条带裁剪合同保持不变。
    pub fn process_shared(&mut self, image: Arc<Image>) -> Result<StitchUpdate> {
        validate_image(&image)?;
        if image.width != self.state.last.width || image.height != self.state.last.height {
            return Err(tr!(
                "采集尺寸变化，请重新开始长截图",
                "The capture size changed; start the long capture again"
            )
            .into());
        }
        // 精确重复帧可跳过；稀疏文字不能被 16×16 的重复采样提前吞掉。
        if image.bgra == self.state.last.bgra {
            return Ok(self.update(UpdateKind::Unchanged, 0, 0, None, None));
        }
        let old = self.state.clone();
        let mut next = old.clone();
        if self.options.detect_fixed_borders {
            next.borders
                .observe_frame_pair(&old.last, &image, self.options.axis);
        }
        let content = self.content_rect(&next);
        let gray = grayscale(&image, self.options.axis)?;
        let (align, mapping) = {
            let mut scratch = self.workspace.lock().unwrap_or_else(|e| e.into_inner());
            choose_alignment(
                &old.gray,
                &gray,
                self.cross,
                self.frame_length,
                content,
                &old.history,
                &mut scratch,
            )?
        };
        let matching_length = content
            .filter(|r| r.valid(self.cross, self.frame_length) && r.height >= MIN_OVERLAP)
            .map_or(self.frame_length, |r| r.height);
        let quality = Some(StitchMatchQuality {
            error: align.selection_score,
            matched_ratio: align.matched_row_ratio,
            unique: align.is_visually_unique,
            overlap_ratio: matching_length.saturating_sub(align.offset.unsigned_abs()) as f64
                / matching_length as f64,
        });
        // 检测器可累计四边最小值，拒绝不能推进比较基线或历史。
        self.state.borders = next.borders.clone();
        if !align.is_acceptable(8., 15.) {
            return Ok(self.update(
                UpdateKind::Rejected,
                0,
                0,
                Some(trf!(
                    "接缝尚不可靠：位移={}，分数={:.2}，匹配行={:.0}%{}",
                    "Seam not reliable: offset={}, score={:.2}, matched rows={:.0}%{}",
                    align.offset,
                    align.selection_score,
                    align.matched_row_ratio * 100.,
                    if align.is_ambiguous {
                        tr!("，存在歧义", ", ambiguous")
                    } else {
                        ""
                    }
                )),
                quality,
            ));
        }
        let delta = align.offset;
        if delta == 0 {
            next.last = image;
            next.gray = gray.into();
            self.state = next;
            return Ok(self.update(UpdateKind::Unchanged, 0, 0, None, quality));
        }
        let offset_y = mapping.map_or(0, |r| r.y as i32);
        let mut seam_curr =
            (offset_y + align.seam_row_in_curr).clamp(0, self.frame_length as i32 - 1) as u32;
        let new_top = old.viewport_top as i64 + delta as i64;
        let fits = new_top >= 0 && new_top + self.frame_length as i64 <= old.length as i64;
        let up_inside = delta < 0 && old.viewport_top > 0 && new_top >= 0;
        let viewport_only = fits || up_inside;
        let insets = next.borders.fixed_borders().unwrap_or_default();
        let fixed_borders = (
            self.options.fixed_start.unwrap_or(insets.top),
            self.options.fixed_end.unwrap_or(insets.bottom),
        );
        let mut seam_prev = if !viewport_only {
            let Some((prev, curr)) = moving_seam(
                &old.gray,
                &gray,
                (self.cross, self.frame_length),
                fixed_borders,
                delta,
                seam_curr,
            ) else {
                return Ok(self.update(
                    UpdateKind::Rejected,
                    0,
                    0,
                    Some(
                        tr!(
                            "共有正文没有可靠接缝；保留最后成功帧",
                            "No reliable seam in the shared content; keeping the last good frame"
                        )
                        .into(),
                    ),
                    quality,
                ));
            };
            seam_curr = curr;
            prev
        } else {
            (offset_y + align.seam_row_in_prev).clamp(0, self.frame_length as i32 - 1) as u32
        };
        if !viewport_only && delta > 0 {
            let geo_height = mapping.map_or(self.frame_length, |r| r.height) as i32;
            if !paste_consistent(
                geo_height,
                delta,
                seam_curr as i32 - offset_y,
                align.matched_row_ratio,
            ) {
                return Ok(self.update(
                    UpdateKind::Rejected,
                    0,
                    0,
                    Some(
                        tr!(
                            "接缝追加范围不可靠；保留最后成功帧，等待下一次重叠",
                            "Seam append range not reliable; keeping the last good frame and waiting for the next overlap"
                        )
                        .into(),
                    ),
                    quality,
                ));
            }
        }
        if !viewport_only && align.is_visually_unique {
            if let Some((previous, current)) = prefer_floating_seam(
                &old.gray,
                &gray,
                (self.cross, self.frame_length),
                fixed_borders,
                delta,
                seam_curr,
                (
                    &old.segments,
                    &old.source_viewports,
                    new_top,
                    self.options.axis,
                ),
            ) {
                let geo_height = mapping.map_or(self.frame_length, |r| r.height) as i32;
                if delta < 0
                    || paste_consistent(
                        geo_height,
                        delta,
                        current as i32 - offset_y,
                        align.matched_row_ratio,
                    )
                {
                    seam_prev = previous;
                    seam_curr = current;
                }
            }
        }
        let old_length = old.length;
        if viewport_only {
            next.viewport_top = new_top as u32;
        } else {
            let seam_out = old.viewport_top + seam_prev;
            if delta > 0 {
                let keep = (seam_out + 1).min(old.length);
                let source_top = seam_curr + 1;
                let append = self.frame_length - source_top;
                next.length = keep + append;
                if !valid_size(self.cross, next.length) {
                    return Ok(self.limit());
                }
                (next.segments, next.source_viewports) = trim_segments(
                    &old.segments,
                    &old.source_viewports,
                    0,
                    keep,
                    self.options.axis,
                )?;
                if append > 0 {
                    next.segments.push(new_segment(
                        &image,
                        self.options.axis,
                        source_top,
                        append,
                        keep,
                    )?);
                    next.source_viewports.push(new_top);
                }
                next.viewport_top = new_top as u32;
            } else {
                let cut = seam_out.min(old.length);
                next.length = old.length - cut + seam_curr;
                if !valid_size(self.cross, next.length) {
                    return Ok(self.limit());
                }
                (next.segments, next.source_viewports) = trim_segments(
                    &old.segments,
                    &old.source_viewports,
                    cut,
                    old.length,
                    self.options.axis,
                )?;
                for segment in &mut next.segments {
                    let start = along_origin(segment.origin, self.options.axis) + seam_curr;
                    segment.origin = self.options.axis.origin(start);
                }
                for viewport in &mut next.source_viewports {
                    *viewport += seam_curr as i64;
                }
                if seam_curr > 0 {
                    next.segments
                        .insert(0, new_segment(&image, self.options.axis, 0, seam_curr, 0)?);
                    next.source_viewports.insert(0, 0);
                }
                next.viewport_top = 0;
            }
        }
        next.accepted_count += 1;
        if next
            .history
            .last()
            .is_some_and(|h| h.offset.signum() != delta.signum())
        {
            next.history.clear();
        }
        next.history.push(OffsetHistoryItem {
            frame_index: next.accepted_count,
            offset: delta,
            diff: align.score,
            seam_row_in_curr: seam_curr as i32 - offset_y,
            is_trusted: align.is_visually_unique,
        });
        if next.history.len() > 16 {
            next.history.remove(0);
        }
        next.last = image;
        next.gray = gray.into();
        self.previous = Some(old);
        self.state = next;
        (self.width, self.height) = self.options.axis.dimensions(self.cross, self.state.length);
        Ok(self.update(
            if viewport_only {
                UpdateKind::ViewportOnly
            } else {
                UpdateKind::Added
            },
            delta,
            self.state.length.saturating_sub(old_length),
            None,
            quality,
        ))
    }
    fn content_rect(&self, state: &State) -> Option<Rect> {
        let detected = state.borders.content_rect();
        if self.options.fixed_start.is_none() && self.options.fixed_end.is_none() {
            return detected;
        }
        let top = self
            .options
            .fixed_start
            .unwrap_or_else(|| detected.map_or(0, |r| r.y));
        let bottom = self
            .options
            .fixed_end
            .unwrap_or_else(|| detected.map_or(0, |r| self.frame_length - r.y - r.height));
        Some(Rect {
            x: detected.map_or(0, |r| r.x),
            y: top,
            width: detected.map_or(self.cross, |r| r.width),
            height: self.frame_length.checked_sub(top)?.checked_sub(bottom)?,
        })
    }
    fn viewport(&self) -> Rect {
        // 回到已有捕获带内不重拼接；绿框必须映射到有效图像范围。
        self.options.axis.rect(
            self.state.viewport_top,
            self.frame_length
                .min(self.state.length.saturating_sub(self.state.viewport_top)),
            self.cross,
        )
    }
    fn update(
        &self,
        kind: UpdateKind,
        delta: i32,
        added: u32,
        reason: Option<String>,
        quality: Option<StitchMatchQuality>,
    ) -> StitchUpdate {
        StitchUpdate {
            kind,
            delta,
            viewport: self.viewport(),
            added,
            reason,
            quality,
        }
    }
    fn limit(&self) -> StitchUpdate {
        self.update(
            UpdateKind::Limit,
            0,
            0,
            Some(
                tr!(
                    "长截图达到 3200 万像素或 65535 单边上限，请保存当前结果",
                    "The long capture reached 32 megapixels or 65535 px on one side; save the current result"
                )
                .into(),
            ),
            None,
        )
    }
    pub fn can_undo(&self) -> bool {
        self.previous.is_some()
    }
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.previous.take() else {
            return false;
        };
        self.state = previous;
        (self.width, self.height) = self.options.axis.dimensions(self.cross, self.state.length);
        true
    }
    pub fn snapshot(&self) -> StitchSnapshot {
        StitchSnapshot {
            width: self.width,
            height: self.height,
            axis: self.options.axis,
            viewport: self.viewport(),
            segments: self.state.segments.clone(),
        }
    }
    pub fn image(&self) -> Image {
        let mut output = Image {
            width: self.width,
            height: self.height,
            bgra: vec![0; self.width as usize * self.height as usize * 4],
            readback_ms: 0.,
        };
        for segment in &self.state.segments {
            copy_into(&segment.image, segment.source, &mut output, segment.origin);
        }
        output
    }
    #[cfg(test)]
    pub fn pixel_bytes(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut bytes = 0;
        for state in std::iter::once(&self.state).chain(self.previous.iter()) {
            for image in std::iter::once(&state.last).chain(state.segments.iter().map(|s| &s.image))
            {
                if seen.insert(Arc::as_ptr(image)) {
                    bytes += image.bgra.len();
                }
            }
        }
        bytes
    }
}

/// 对齐框的 2px margin 可能落进固定栏；预测接缝也可能比全局最低 SAD 高 2.5。
/// 保留原 offset/质量门，每次输出都复核实际接缝；只在共有正文中找 SAD≤15 的替代行。
fn moving_seam(
    prev: &[u8],
    curr: &[u8],
    size: (u32, u32),
    borders: (u32, u32),
    offset: i32,
    seam: u32,
) -> Option<(u32, u32)> {
    let (width, height) = size;
    let (top, bottom) = borders;
    let lower = (top as i32).max(top as i32 - offset);
    let end = height.checked_sub(bottom)? as i32 - 1;
    let upper = end.min(end - offset);
    if lower > upper {
        return None;
    }
    let difference = |row| seam_difference(prev, curr, width, offset, row);
    if (lower..=upper).contains(&(seam as i32)) && difference(seam as i32) <= 15. {
        return Some(((seam as i32 + offset) as u32, seam));
    }
    let desired = (seam as i32).clamp(lower, upper);
    let mut best = None;
    let mut best_diff = f64::INFINITY;
    for row in lower..=upper {
        let diff = difference(row);
        if diff < best_diff
            || (diff == best_diff
                && best.is_some_and(|value: i32| (row - desired).abs() < (value - desired).abs()))
        {
            best = Some(row);
            best_diff = diff;
        }
    }
    best.filter(|_| best_diff <= 15.)
        .map(|row| ((row + offset) as u32, row as u32))
}

/// 两帧的局部稳定性只用于选缝，不作为擦除正文或确认固定元素的依据。
fn prefer_floating_seam(
    prev: &[u8],
    curr: &[u8],
    size: (u32, u32),
    borders: (u32, u32),
    offset: i32,
    seam: u32,
    sources: (&[StitchSegment], &[i64], i64, ScrollAxis),
) -> Option<(u32, u32)> {
    let spans = floating::spans(prev, curr, size, borders, offset);
    if spans.is_empty() {
        return None;
    }
    let (segments, viewports, new_top, axis) = sources;
    // 最后采集帧未必是实际保留的像素来源。把候选投影到每条历史条带，
    // 小步后改大步时才能覆盖真正留在成品中的旧按钮，而非只覆盖 last 的投影。
    let mut footprints = Vec::new();
    for (segment, &viewport) in segments.iter().zip(viewports) {
        let start = along_origin(segment.origin, axis);
        let length = match axis {
            ScrollAxis::Vertical => segment.source.height,
            ScrollAxis::Horizontal => segment.source.width,
        };
        let end = start as i64 + length as i64;
        for &(top, bottom) in &spans {
            let lo = (viewport + top as i64).max(start as i64);
            let hi = (viewport + bottom as i64).min(end);
            if lo < hi {
                footprints.push((lo, hi));
            }
        }
    }
    let retained = |row: i32| -> Option<usize> {
        let boundary = row as i64 + i64::from(offset > 0);
        let cut = new_top + boundary;
        let mut coverage = 0;
        for &(top, bottom) in &footprints {
            if top < cut && cut < bottom {
                return None;
            }
            let keep = if offset > 0 {
                bottom <= cut
            } else {
                top >= cut
            };
            if keep {
                coverage += (bottom - top) as usize;
            }
        }
        for &(top, bottom) in &spans {
            let (top, bottom) = (top as i64, bottom as i64);
            if top < boundary && boundary < bottom {
                return None;
            }
            let keep = if offset > 0 {
                top >= boundary
            } else {
                bottom <= boundary
            };
            if keep {
                coverage += (bottom - top) as usize;
            }
        }
        Some(coverage)
    };
    let mut best_coverage = retained(seam as i32).unwrap_or(usize::MAX);
    if best_coverage == 0 {
        return None;
    }
    let (width, height) = size;
    let (top, bottom) = borders;
    let lower = (top as i32).max(top as i32 - offset);
    let end = height.checked_sub(bottom)? as i32 - 1;
    let upper = end.min(end - offset);
    let difference = |row| seam_difference(prev, curr, width, offset, row);
    let limit = 15f64.min(difference(seam as i32) + 2.5);
    let mut best = None;
    // 按实际保留的长度比较，候选外扩后仅剩的几像素不能与整枚按钮等权。
    // 先避免残片、减少疑似遮挡，再取最近的等价可靠行；没有改善便保留原缝。
    for distance in 1..=(upper - lower) {
        for row in [
            seam as i32 - distance * offset.signum(),
            seam as i32 + distance * offset.signum(),
        ] {
            if !(lower..=upper).contains(&row) {
                continue;
            }
            if let Some(coverage) = retained(row) {
                if coverage < best_coverage && difference(row) <= limit {
                    best_coverage = coverage;
                    best = Some(((row + offset) as u32, row as u32));
                    if coverage == 0 {
                        return best;
                    }
                }
            }
        }
    }
    best
}

fn seam_difference(prev: &[u8], curr: &[u8], width: u32, offset: i32, row: i32) -> f64 {
    let previous = (row + offset) as usize * width as usize;
    let current = row as usize * width as usize;
    let sum: u64 = prev[previous..previous + width as usize]
        .iter()
        .zip(&curr[current..current + width as usize])
        .map(|(&a, &b)| a.abs_diff(b) as u64)
        .sum();
    sum as f64 / width as f64
}

fn choose_alignment(
    prev: &[u8],
    curr: &[u8],
    width: u32,
    height: u32,
    content: Option<Rect>,
    history: &[OffsetHistoryItem],
    scratch: &mut Workspace,
) -> Result<(AlignResult, Option<Rect>)> {
    let options = AlignmentOptions {
        predict_offset: seam_alignment::predict_next_offset(history, 0),
        ..Default::default()
    };
    let full = seam_alignment::find_best_offset(
        prev,
        curr,
        width,
        height,
        options,
        history,
        &mut scratch.full,
    )?;
    let Some(rect) = content.filter(|r| r.valid(width, height) && r.height >= MIN_OVERLAP) else {
        return Ok((full, None));
    };
    let prev_content = crop_gray(prev, width, rect)?;
    let curr_content = crop_gray(curr, width, rect)?;
    let alt = seam_alignment::find_best_offset(
        &prev_content,
        &curr_content,
        rect.width,
        rect.height,
        options,
        history,
        &mut scratch.content,
    )?;
    let full_ok = full.is_acceptable(8., 15.);
    let alt_ok = alt.is_acceptable(8., 15.);
    if !alt_ok {
        return Ok((full, None));
    }
    if !full_ok {
        return Ok((alt, Some(rect)));
    }
    if full.is_visually_unique != alt.is_visually_unique {
        return Ok(if full.is_visually_unique {
            (full, None)
        } else {
            (alt, Some(rect))
        });
    }
    if (full.offset - alt.offset).abs() <= 2 {
        return Ok(if alt.selection_score < full.selection_score {
            (alt, Some(rect))
        } else {
            (full, None)
        });
    }
    let gap = 0.75f64.max(full.selection_score.min(alt.selection_score) * 0.25);
    if full.selection_score + gap < alt.selection_score {
        return Ok((full, None));
    }
    if alt.selection_score + gap < full.selection_score {
        return Ok((alt, Some(rect)));
    }
    Ok(if full.selection_score <= alt.selection_score {
        (full.as_ambiguous(alt.selection_score), None)
    } else {
        (alt.as_ambiguous(full.selection_score), Some(rect))
    })
}
fn paste_consistent(height: i32, offset: i32, seam: i32, matched: f64) -> bool {
    let overlap = height - offset;
    if overlap < MIN_OVERLAP as i32 {
        return false;
    }
    let rows = height - seam - 1;
    let slack = 24.max(offset / 4);
    if matched < 0.72 && rows > 0 {
        let gap = overlap - 1 - seam;
        if gap > slack.max(overlap / 4) && rows > offset + slack.max(overlap / 3) {
            return false;
        }
    }
    rows <= 0 || !(seam <= slack && rows >= height - offset - slack)
}
fn valid_size(cross: u32, length: u32) -> bool {
    cross > 0
        && length > 0
        && cross <= MAX_SIDE
        && length <= MAX_SIDE
        && cross as u64 * length as u64 <= MAX_PIXELS
}
fn validate_image(image: &Image) -> Result<()> {
    if !valid_size(image.width, image.height)
        || image.bgra.len() != image.width as usize * image.height as usize * 4
    {
        return Err(tr!(
            "采集帧尺寸/像素长度无效，或超过 3200 万像素 / 65535 单边限制",
            "Invalid capture frame size or pixel length, or over 32 megapixels / 65535 px per side"
        )
        .into());
    }
    Ok(())
}
fn grayscale(image: &Image, axis: ScrollAxis) -> Result<Vec<u8>> {
    let (width, height) = axis.size(image);
    let mut gray = Vec::new();
    gray.try_reserve_exact(width as usize * height as usize)?;
    let luma = |p: &[u8]| ((p[2] as u16 + 2 * p[1] as u16 + p[0] as u16) >> 2) as u8;
    if axis == ScrollAxis::Vertical {
        // 纵向即原始行序，整块顺序转换，避免逐像素分支与边界检查。
        gray.extend(image.bgra.chunks_exact(4).map(luma));
        return Ok(gray);
    }
    for y in 0..height {
        for x in 0..width {
            let (px, py) = match axis {
                ScrollAxis::Vertical => (x, y),
                ScrollAxis::Horizontal => (y, x),
            };
            let i = (py as usize * image.width as usize + px as usize) * 4;
            gray.push(luma(&image.bgra[i..i + 4]));
        }
    }
    Ok(gray)
}
fn crop_gray(gray: &[u8], width: u32, rect: Rect) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output.try_reserve_exact(rect.width as usize * rect.height as usize)?;
    for y in rect.y..rect.y + rect.height {
        let i = (y as usize * width as usize) + rect.x as usize;
        output.extend_from_slice(&gray[i..i + rect.width as usize]);
    }
    Ok(output)
}
fn along_origin(origin: (u32, u32), axis: ScrollAxis) -> u32 {
    match axis {
        ScrollAxis::Vertical => origin.1,
        ScrollAxis::Horizontal => origin.0,
    }
}
fn trim_segments(
    segments: &[StitchSegment],
    viewports: &[i64],
    start: u32,
    end: u32,
    axis: ScrollAxis,
) -> Result<(Vec<StitchSegment>, Vec<i64>)> {
    let mut output = Vec::with_capacity(segments.len());
    let mut sources = Vec::with_capacity(segments.len());
    debug_assert_eq!(segments.len(), viewports.len());
    for (s, &viewport) in segments.iter().zip(viewports) {
        let origin = along_origin(s.origin, axis);
        let length = match axis {
            ScrollAxis::Vertical => s.source.height,
            ScrollAxis::Horizontal => s.source.width,
        };
        let lo = origin.max(start);
        let hi = (origin + length).min(end);
        if hi <= lo {
            continue;
        }
        let mut segment = s.clone();
        match axis {
            ScrollAxis::Vertical => {
                segment.source.y += lo - origin;
                segment.source.height = hi - lo;
            }
            ScrollAxis::Horizontal => {
                segment.source.x += lo - origin;
                segment.source.width = hi - lo;
            }
        }
        segment.origin = axis.origin(lo - start);
        // 接缝会逐次移除旧尾栏。压缩被裁的边缘块，避免每个 1px 条带永久占用整份尾栏。
        // 其余完整块继续共享，撤销保留原 Arc；分配失败不会提交长图或可信基线。
        if lo != origin || hi != origin + length {
            let image = copy_region(&segment.image, segment.source)?;
            segment.source = Rect::full(image.width, image.height);
            segment.image = Arc::new(image);
        }
        output.push(segment);
        sources.push(viewport - start as i64);
    }
    Ok((output, sources))
}
fn new_segment(
    image: &Image,
    axis: ScrollAxis,
    start: u32,
    length: u32,
    origin: u32,
) -> Result<StitchSegment> {
    let (cross, _) = axis.size(image);
    let rect = axis.rect(start, length, cross);
    // 使用同一物理 Rect 转换入口，纵横向仅改变 source/origin。
    let logical = Rect {
        x: 0,
        y: start,
        width: cross,
        height: length,
    };
    debug_assert_eq!(rect, stitch_content::physical_rect(logical, axis));
    let output = copy_region(image, rect)?;
    Ok(StitchSegment {
        source: Rect::full(output.width, output.height),
        image: Arc::new(output),
        origin: axis.origin(origin),
    })
}
fn copy_region(image: &Image, rect: Rect) -> Result<Image> {
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(rect.width as usize * rect.height as usize * 4)?;
    bgra.resize(rect.width as usize * rect.height as usize * 4, 0);
    let mut output = Image {
        width: rect.width,
        height: rect.height,
        bgra,
        readback_ms: 0.,
    };
    copy_into(image, rect, &mut output, (0, 0));
    Ok(output)
}
fn copy_into(source: &Image, rect: Rect, output: &mut Image, origin: (u32, u32)) {
    let count = rect.width as usize * 4;
    for y in 0..rect.height as usize {
        let from = ((rect.y as usize + y) * source.width as usize + rect.x as usize) * 4;
        let to = ((origin.1 as usize + y) * output.width as usize + origin.0 as usize) * 4;
        output.bgra[to..to + count].copy_from_slice(&source.bgra[from..from + count]);
    }
}

#[cfg(test)]
#[path = "stitch_dynamic_seam_tests.rs"]
mod dynamic_seam_tests;
