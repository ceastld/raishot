//! OCR 原位查找：只保存原文范围与定位缓存，不修改普通选字、剪贴板或 Scene。
//! 作为 pin_text_selection 的子模块注册，两个宿主共用此模型。
use super::{cell_quad, SelectionText};
use crate::{
    annotation::{Bounds, Point},
    recognition::{geometry::Projection, TextGeometry},
};
use std::{collections::BTreeMap, ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const MAX_FIND_TEXT_BYTES: usize = 1_048_576;
pub(crate) const MAX_FIND_QUERY_BYTES: usize = 4_096;
pub(crate) const MAX_FIND_HITS: usize = 512;
const MAX_FIND_SOURCE_CELLS: usize = 32_768;
const MAX_FIND_LOCATION_QUADS: usize = 512;
pub(crate) const MAX_FIND_BATCH_QUADS: usize = 4_096;

/// 宿主必须使用真实来源身份，并在重识别、正文编辑或几何重建时更换快照。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OcrFindStamp<S> {
    pub(crate) source: S,
    pub(crate) result_generation: u64,
    pub(crate) content_revision: u64,
}
/// 不可变、共享正文；new 每次产生新身份，即使 stamp 数值和文本相同。
pub(crate) struct OcrFindDocument<S> {
    stamp: Arc<OcrFindStamp<S>>,
    text: Arc<str>,
}
impl<S> Clone for OcrFindDocument<S> {
    fn clone(&self) -> Self {
        Self {
            stamp: Arc::clone(&self.stamp),
            text: Arc::clone(&self.text),
        }
    }
}
impl<S> OcrFindDocument<S> {
    pub(crate) fn new(stamp: OcrFindStamp<S>, text: Arc<str>) -> Result<Self, OcrFindError> {
        if text.len() > MAX_FIND_TEXT_BYTES {
            return Err(OcrFindError::TextTooLarge);
        }
        Ok(Self {
            stamp: Arc::new(stamp),
            text,
        })
    }
    pub(crate) fn stamp(&self) -> &OcrFindStamp<S> {
        &self.stamp
    }
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stamp, &other.stamp) && Arc::ptr_eq(&self.text, &other.text)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OcrFindAccess {
    pub(crate) active: bool,
    pub(crate) idle: bool,
    pub(crate) source_current: bool,
    pub(crate) composing: bool,
}
impl OcrFindAccess {
    fn allowed(self) -> bool {
        self.active && self.idle && self.source_current && !self.composing
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OcrFindError {
    TextTooLarge,
    QueryTooLarge,
    AccessDenied,
    StaleIntent,
    TextMismatch,
    InvalidClip,
    GeometryTooLarge,
    HitsTruncated,
    MissingGlyphGeometry,
    AmbiguousGlyphGeometry,
    InvalidGlyphGeometry,
    OutsideVisibleBounds,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OcrFindDirection {
    Next,
    Previous,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OcrFindSummary {
    pub(crate) stored: usize,
    pub(crate) total: usize,
    pub(crate) truncated: bool,
    /// 已保存命中的从 1 开始序号；未激活命中时为 None。
    pub(crate) current: Option<usize>,
}
pub(crate) struct OcrFindTicket<S> {
    document: OcrFindDocument<S>,
    position: Arc<()>,
    index: usize,
    range: Range<usize>,
}
impl<S> Clone for OcrFindTicket<S> {
    fn clone(&self) -> Self {
        Self {
            document: self.document.clone(),
            position: Arc::clone(&self.position),
            index: self.index,
            range: self.range.clone(),
        }
    }
}
pub(crate) struct OcrFindBatchTicket<S> {
    document: OcrFindDocument<S>,
    position: Arc<()>,
    count: usize,
}
impl<S> Clone for OcrFindBatchTicket<S> {
    fn clone(&self) -> Self {
        Self {
            document: self.document.clone(),
            position: Arc::clone(&self.position),
            count: self.count,
        }
    }
}
pub(crate) struct OcrFindLocation {
    pub(crate) range: Range<usize>,
    pub(crate) quads: Vec<[Point; 4]>,
    #[cfg(test)]
    pub(crate) bounds: Bounds,
    clip: Bounds,
    position: Arc<()>,
}
pub(crate) struct OcrFind<S> {
    document: Option<OcrFindDocument<S>>,
    query: String,
    hits: Vec<Range<usize>>,
    total: usize,
    current: Option<usize>,
    position: Arc<()>,
    location: Option<OcrFindLocation>,
}
impl<S> Default for OcrFind<S> {
    fn default() -> Self {
        Self {
            document: None,
            query: String::new(),
            hits: Vec::new(),
            total: 0,
            current: None,
            position: Arc::new(()),
            location: None,
        }
    }
}
impl<S> OcrFind<S> {
    pub(crate) fn invalidate(&mut self) {
        self.document = None;
        self.query.clear();
        self.hits.clear();
        self.total = 0;
        self.clear_current();
    }
    /// 普通拖选、键盘选字、结果内选择或查找关闭时调用，不改变普通选择本身。
    pub(crate) fn clear_current(&mut self) {
        self.current = None;
        self.position = Arc::new(());
        self.location = None;
    }
    /// 仅在事件中同步；同来源临时忙碌保留查询，但所有读取及动作仍受 gates 约束。
    pub(crate) fn synchronize(&mut self, document: &OcrFindDocument<S>) {
        if !self.document.as_ref().is_some_and(|old| old.same(document)) {
            self.invalidate();
            self.document = Some(document.clone());
        }
    }
    fn ready(&self, document: &OcrFindDocument<S>, access: OcrFindAccess) -> bool {
        access.allowed() && self.document.as_ref().is_some_and(|old| old.same(document))
    }
    pub(crate) fn update(
        &mut self,
        document: &OcrFindDocument<S>,
        query: &str,
        access: OcrFindAccess,
    ) -> Result<OcrFindSummary, OcrFindError> {
        self.synchronize(document);
        if !access.allowed() {
            return Err(OcrFindError::AccessDenied);
        }
        self.query.clear();
        self.hits.clear();
        self.total = 0;
        self.clear_current();
        if query.len() > MAX_FIND_QUERY_BYTES {
            return Err(OcrFindError::QueryTooLarge);
        }
        self.query.push_str(query);
        if !query.is_empty() {
            let (hits, total) = literal_hits(document.text(), query);
            self.hits = hits;
            self.total = total;
            self.current = (!self.hits.is_empty()).then_some(0);
        }
        Ok(self.summary(document, access).expect("刚绑定的来源有效"))
    }
    #[cfg(test)]
    pub(crate) fn query(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<&str> {
        self.ready(document, access).then_some(self.query.as_str())
    }
    pub(crate) fn summary(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<OcrFindSummary> {
        self.ready(document, access).then_some(OcrFindSummary {
            stored: self.hits.len(),
            total: self.total,
            truncated: self.total > self.hits.len(),
            current: self.current.map(|index| index + 1),
        })
    }
    #[cfg(test)]
    pub(crate) fn ranges(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<&[Range<usize>]> {
        self.ready(document, access).then_some(self.hits.as_slice())
    }
    /// 将来的显式“全部命中”动作不能把截断的前 512 项当成全部。
    #[cfg(test)]
    pub(crate) fn complete_ranges(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<&[Range<usize>]> {
        (self.ready(document, access) && self.total == self.hits.len())
            .then_some(self.hits.as_slice())
    }
    pub(crate) fn cycle(
        &mut self,
        document: &OcrFindDocument<S>,
        direction: OcrFindDirection,
        access: OcrFindAccess,
    ) -> Option<OcrFindTicket<S>> {
        self.synchronize(document);
        if !self.ready(document, access) || self.hits.is_empty() {
            return None;
        }
        let length = self.hits.len();
        self.current = Some(match (self.current, direction) {
            (Some(index), OcrFindDirection::Next) => (index + 1) % length,
            (Some(index), OcrFindDirection::Previous) => (index + length - 1) % length,
            (None, OcrFindDirection::Next) => 0,
            (None, OcrFindDirection::Previous) => length - 1,
        });
        self.position = Arc::new(());
        self.location = None;
        self.current_ticket(document, access)
    }
    pub(crate) fn current_ticket(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<OcrFindTicket<S>> {
        if !self.ready(document, access) {
            return None;
        }
        let index = self.current?;
        Some(OcrFindTicket {
            document: document.clone(),
            position: Arc::clone(&self.position),
            index,
            range: self.hits.get(index)?.clone(),
        })
    }
    /// 宿主选中／复制／标记的最后一步重新核验；不会产生这些副作用。
    pub(crate) fn resolve_ticket(
        &self,
        ticket: &OcrFindTicket<S>,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Result<Range<usize>, OcrFindError> {
        if !access.allowed() {
            return Err(OcrFindError::AccessDenied);
        }
        if !self.ready(document, access)
            || !ticket.document.same(document)
            || !Arc::ptr_eq(&ticket.position, &self.position)
            || self.current != Some(ticket.index)
            || self.hits.get(ticket.index) != Some(&ticket.range)
        {
            return Err(OcrFindError::StaleIntent);
        }
        Ok(ticket.range.clone())
    }
    /// 显式批量动作使用同一意图票据；返回借用，提交前仍需再验证一次。
    #[cfg(test)]
    pub(crate) fn resolve_complete_ranges(
        &self,
        ticket: &OcrFindTicket<S>,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Result<&[Range<usize>], OcrFindError> {
        self.resolve_ticket(ticket, document, access)?;
        if self.total != self.hits.len() {
            return Err(OcrFindError::HitsTruncated);
        }
        Ok(&self.hits)
    }
    /// 只对非空、未截断查询生成整批意图；不要求覆盖用户当前手动选择。
    pub(crate) fn all_ticket(
        &self,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Option<OcrFindBatchTicket<S>> {
        if !self.ready(document, access) || self.hits.is_empty() || self.total != self.hits.len() {
            return None;
        }
        Some(OcrFindBatchTicket {
            document: document.clone(),
            position: Arc::clone(&self.position),
            count: self.hits.len(),
        })
    }
    pub(crate) fn resolve_all_ticket(
        &self,
        ticket: &OcrFindBatchTicket<S>,
        document: &OcrFindDocument<S>,
        access: OcrFindAccess,
    ) -> Result<&[Range<usize>], OcrFindError> {
        if !access.allowed() {
            return Err(OcrFindError::AccessDenied);
        }
        if !self.ready(document, access)
            || !ticket.document.same(document)
            || !Arc::ptr_eq(&ticket.position, &self.position)
            || ticket.count == 0
            || ticket.count != self.hits.len()
        {
            return Err(OcrFindError::StaleIntent);
        }
        if self.total != self.hits.len() {
            return Err(OcrFindError::HitsTruncated);
        }
        Ok(&self.hits)
    }
    /// 所有命中先核完整字形资格，重叠 byte 区间合并后只扫描一次来源 cells。
    /// 任意范围没有字形／缺框／坏投影都整批失败，不返回部分可提交结果。
    pub(crate) fn resolve_all_quads(
        &self,
        ticket: &OcrFindBatchTicket<S>,
        document: &OcrFindDocument<S>,
        selection: &SelectionText,
        clip: Bounds,
        access: OcrFindAccess,
    ) -> Result<Vec<[Point; 4]>, OcrFindError> {
        let ranges = self.resolve_all_ticket(ticket, document, access)?;
        if selection.text.as_bytes() != document.text().as_bytes() {
            return Err(OcrFindError::TextMismatch);
        }
        if !valid_bounds(clip) {
            return Err(OcrFindError::InvalidClip);
        }
        selection.find_ranges_quads(ranges, clip, MAX_FIND_BATCH_QUADS)
    }
    /// 仅在查询、换命中或可见裁剪改变的事件中建立缓存，渲染只借用结果。
    pub(crate) fn prepare_location(
        &mut self,
        ticket: &OcrFindTicket<S>,
        document: &OcrFindDocument<S>,
        selection: &SelectionText,
        clip: Bounds,
        access: OcrFindAccess,
    ) -> Result<(), OcrFindError> {
        self.location = None;
        let range = self.resolve_ticket(ticket, document, access)?;
        if selection.text.as_bytes() != document.text().as_bytes() {
            return Err(OcrFindError::TextMismatch);
        }
        if !valid_bounds(clip) {
            return Err(OcrFindError::InvalidClip);
        }
        let quads = selection.find_location_quads(range.clone(), clip)?;
        #[cfg(test)]
        let bounds = crate::transform::bounds(quads.iter().flatten().copied());
        self.location = Some(OcrFindLocation {
            range,
            quads,
            #[cfg(test)]
            bounds,
            clip,
            position: Arc::clone(&self.position),
        });
        Ok(())
    }
    pub(crate) fn location(
        &self,
        document: &OcrFindDocument<S>,
        clip: Bounds,
        access: OcrFindAccess,
    ) -> Option<&OcrFindLocation> {
        if !self.ready(document, access) {
            return None;
        }
        self.location.as_ref().filter(|location| {
            location.clip == clip
                && Arc::ptr_eq(&location.position, &self.position)
                && self.current.and_then(|index| self.hits.get(index)) == Some(&location.range)
        })
    }
}

/// 单次查询 O(正文 bytes + 查询 bytes)，允许重叠，但只接受原文完整字素边界。
fn literal_hits(text: &str, query: &str) -> (Vec<Range<usize>>, usize) {
    let mut boundaries = vec![false; text.len() + 1];
    for (byte, _) in text.grapheme_indices(true) {
        boundaries[byte] = true;
    }
    boundaries[text.len()] = true;
    let pattern = query.as_bytes();
    let mut prefix = vec![0; pattern.len()];
    let mut matched = 0;
    for index in 1..pattern.len() {
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    let mut hits = Vec::new();
    let mut total = 0;
    matched = 0;
    for (index, byte) in text.bytes().enumerate() {
        while matched > 0 && byte != pattern[matched] {
            matched = prefix[matched - 1];
        }
        if byte == pattern[matched] {
            matched += 1;
        }
        if matched == pattern.len() {
            let end = index + 1;
            let start = end - pattern.len();
            if boundaries[start] && boundaries[end] {
                total += 1;
                if hits.len() < MAX_FIND_HITS {
                    hits.push(start..end);
                }
            }
            matched = prefix[matched - 1];
        }
    }
    (hits, total)
}
fn valid_bounds(bounds: Bounds) -> bool {
    [bounds.x, bounds.y, bounds.width, bounds.height]
        .iter()
        .all(|value| value.is_finite())
        && bounds.x.abs() <= 1_000_000.
        && bounds.y.abs() <= 1_000_000.
        && (0.0001..=65_535.).contains(&bounds.width)
        && (0.0001..=65_535.).contains(&bounds.height)
        && (bounds.x + bounds.width).abs() <= 1_000_000.
        && (bounds.y + bounds.height).abs() <= 1_000_000.
}
impl SelectionText {
    fn find_location_quads(
        &self,
        range: Range<usize>,
        clip: Bounds,
    ) -> Result<Vec<[Point; 4]>, OcrFindError> {
        self.find_ranges_quads(&[range], clip, MAX_FIND_LOCATION_QUADS)
    }
    fn find_ranges_quads(
        &self,
        ranges: &[Range<usize>],
        clip: Bounds,
        budget: usize,
    ) -> Result<Vec<[Point; 4]>, OcrFindError> {
        let cells = self
            .lines
            .iter()
            .try_fold(0usize, |total, line| total.checked_add(line.cells.len()));
        if cells.is_none_or(|count| count > MAX_FIND_SOURCE_CELLS) {
            return Err(OcrFindError::GeometryTooLarge);
        }
        let mut expected = BTreeMap::new();
        let mut intervals: Vec<Range<usize>> = Vec::new();
        // 范围已经经过查询模型字素验证，按 byte 顺序；不扫描或复制整份正文。
        for range in ranges {
            let mut has_glyph = false;
            for (offset, glyph) in self.text[range.clone()].grapheme_indices(true) {
                if !glyph.chars().all(char::is_whitespace) {
                    has_glyph = true;
                    let byte = range.start + offset;
                    if !expected.contains_key(&byte) {
                        if expected.len() == budget {
                            return Err(OcrFindError::GeometryTooLarge);
                        }
                        expected.insert(byte, (glyph.len(), None));
                    }
                }
            }
            if !has_glyph {
                return Err(OcrFindError::MissingGlyphGeometry);
            }
            if let Some(last) = intervals.last_mut().filter(|last| last.end >= range.start) {
                last.end = last.end.max(range.end);
            } else {
                intervals.push(range.clone());
            }
        }
        for line in &self.lines {
            for cell in &line.cells {
                let index = intervals.partition_point(|range| range.end <= cell.range.start);
                if intervals
                    .get(index)
                    .is_none_or(|range| range.start >= cell.range.end)
                {
                    continue;
                }
                let Some(glyph) = self.text.get(cell.range.clone()) else {
                    return Err(OcrFindError::InvalidGlyphGeometry);
                };
                if glyph.chars().all(char::is_whitespace) {
                    continue;
                }
                let Some((length, quad_slot)) = expected.get_mut(&cell.range.start) else {
                    return Err(OcrFindError::AmbiguousGlyphGeometry);
                };
                if cell.range.end != cell.range.start + *length
                    || cell.range.start < line.range.start
                    || cell.range.end > line.range.end
                {
                    return Err(OcrFindError::AmbiguousGlyphGeometry);
                }
                if quad_slot.is_some() {
                    return Err(OcrFindError::AmbiguousGlyphGeometry);
                }
                if let Some(projected) = cell.projected {
                    if projected.source_id != cell.source_id
                        || Projection::new(projected.source).is_none()
                        || !projected.start.is_finite()
                        || !projected.end.is_finite()
                        || projected.start < 0.
                        || projected.end > 1.
                        || projected.start >= projected.end
                    {
                        return Err(OcrFindError::InvalidGlyphGeometry);
                    }
                } else if !valid_bounds(cell.bounds) {
                    return Err(OcrFindError::InvalidGlyphGeometry);
                }
                let quad = cell_quad(cell).map(|point| line.rotation.map(point));
                if Projection::new(TextGeometry { quad }).is_none() {
                    return Err(OcrFindError::InvalidGlyphGeometry);
                }
                if quad.iter().any(|point| !clip.contains(*point, 0.)) {
                    return Err(OcrFindError::OutsideVisibleBounds);
                }
                *quad_slot = Some(quad);
            }
        }
        if expected.is_empty() {
            return Err(OcrFindError::MissingGlyphGeometry);
        }
        expected
            .into_values()
            .map(|(_, quad)| quad.ok_or(OcrFindError::MissingGlyphGeometry))
            .collect()
    }
}

#[cfg(test)]
#[path = "ocr_find_selection_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ocr_find_bulk_mark_tests.rs"]
mod bulk_mark_tests;
