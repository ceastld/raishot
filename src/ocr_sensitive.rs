//! OCR 敏感格式候选：显式扫描一次；预览不修改 Scene、图片或剪贴板。
//! 格式通过不证明真实邮箱、电话号码或身份。几何只采用原词投影或完整 CTC 采样证据。
use crate::{
    annotation::{Bounds, Point},
    geometry::Rect,
    recognition::{
        geometry::Projection, graphemes, HorizontalDirection, RecognizedLine, RecognizedText,
        RecognizedWord, TextGeometry,
    },
};
use std::{collections::BTreeSet, ops::Range, sync::Arc};

const MAX_LINES: usize = 4096;
const MAX_WORDS: usize = 32768;
const MAX_CANDIDATES: usize = 128;
const MAX_QUADS: usize = 4096;
const MAX_COORDINATE: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SensitiveKind {
    Email,
    Phone,
    NationalId,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) id: usize,
    pub(crate) kind: SensitiveKind,
    /// 完整 OCR 正文中的半开 UTF-8 范围，不跨行。
    pub(crate) range: Range<usize>,
    pub(crate) line: usize,
    /// OCR 原图局部坐标。宿主统一映射到其 Document，不在这里平移。
    pub(crate) quads: Vec<[Point; 4]>,
}

/// 安装 OCR 时保留的未分类只读快照；不会再次识别或枚举候选。
pub(crate) struct Snapshot {
    recognized: RecognizedText,
}
impl Snapshot {
    pub(crate) fn from_recognized(source: &RecognizedText) -> Option<Self> {
        source_ready(source).then(|| Self {
            recognized: RecognizedText {
                text: source.text.clone(),
                regions: Vec::new(),
                words: source
                    .words
                    .iter()
                    .map(|word| RecognizedWord {
                        text: word.text.clone(),
                        rect: word.rect,
                        line: word.line,
                        horizontal_direction: word.horizontal_direction,
                        geometry: word.geometry,
                    })
                    .collect(),
                lines: source
                    .lines
                    .iter()
                    .map(|line| RecognizedLine {
                        text: line.text.clone(),
                        rect: line.rect,
                        horizontal_direction: line.horizontal_direction,
                        geometry: line.geometry,
                        graphemes: line.graphemes.clone(),
                    })
                    .collect(),
            },
        })
    }
    pub(crate) fn text(&self) -> &str {
        &self.recognized.text
    }
}

/// S 必须包含真实图像来源、OCR 安装身份和正文修订；仅图像坐标不足以防编辑后改回。
/// identity 是本次预览/选择意图身份；它不能代替宿主的正文安装和修订门。
pub(crate) struct Preview<S> {
    source: S,
    body: String,
    identity: Arc<()>,
    candidates: Vec<Candidate>,
    selected: Vec<bool>,
    active: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewNavigation {
    Previous,
    Next,
    First,
    Last,
}
impl<S: Clone + PartialEq> Preview<S> {
    /// 仅由用户显式点击调用一次；渲染、悬停及每帧绘制不调用此方法。
    pub(crate) fn new(source: S, snapshot: &Snapshot) -> Self {
        let candidates = candidates(&snapshot.recognized);
        let selected = vec![false; candidates.len()];
        let active = (!candidates.is_empty()).then_some(0);
        Self {
            source,
            body: snapshot.text().to_owned(),
            identity: Arc::new(()),
            candidates,
            selected,
            active,
        }
    }
    pub(crate) fn text(&self) -> &str {
        &self.body
    }
    pub(crate) fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    pub(crate) fn identity(&self) -> &Arc<()> {
        &self.identity
    }
    pub(crate) fn current(&self, source: &S, body: &str, identity: &Arc<()>) -> bool {
        self.source == *source && self.body == body && Arc::ptr_eq(&self.identity, identity)
    }
    pub(crate) fn selected(&self, id: usize) -> bool {
        self.selected.get(id).copied().unwrap_or(false)
    }
    pub(crate) fn selected_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| self.selected(c.id))
            .count()
    }
    /// 列表项文案：勾选状态、格式类别、行号与原文。
    pub(crate) fn item_label(&self, candidate: &Candidate) -> String {
        let check = if self.selected(candidate.id) {
            "☑"
        } else {
            "☐"
        };
        let kind = match candidate.kind {
            SensitiveKind::Email => tr!("邮箱", "Email"),
            SensitiveKind::Phone => tr!("电话", "Phone"),
            SensitiveKind::NationalId => tr!("身份证", "ID number"),
        };
        let line = candidate.line + 1;
        let value = &self.text()[candidate.range.clone()];
        trf!(
            "{check} {kind} · 第 {line} 行：{value}",
            "{check} {kind} · line {line}: {value}"
        )
    }
    pub(crate) fn active(&self) -> Option<usize> {
        self.active
    }
    /// 活动项也是预览意图，变化后不能提交已按下的旧确认。
    pub(crate) fn set_active(&mut self, id: usize) -> bool {
        if id >= self.candidates.len() || self.active == Some(id) {
            return false;
        }
        self.active = Some(id);
        self.identity = Arc::new(());
        true
    }
    pub(crate) fn navigate(&mut self, navigation: PreviewNavigation) -> bool {
        let Some(active) = self.active else {
            return false;
        };
        let next = match navigation {
            PreviewNavigation::Previous => active.saturating_sub(1),
            PreviewNavigation::Next => (active + 1).min(self.candidates.len() - 1),
            PreviewNavigation::First => 0,
            PreviewNavigation::Last => self.candidates.len() - 1,
        };
        self.set_active(next)
    }
    pub(crate) fn toggle_active(&mut self) -> bool {
        let Some(active) = self.active else {
            return false;
        };
        self.set_selected(active, !self.selected(active))
    }
    pub(crate) fn select_all(&mut self) -> bool {
        if self.selected.iter().all(|selected| *selected) {
            return false;
        }
        self.selected.fill(true);
        self.identity = Arc::new(());
        true
    }
    /// 实际选择变化使已经按下的旧确认意图失效；相同值和无效 id 为 noop。
    pub(crate) fn set_selected(&mut self, id: usize, selected: bool) -> bool {
        let Some(old) = self.selected.get_mut(id) else {
            return false;
        };
        if *old == selected {
            return false;
        }
        *old = selected;
        self.identity = Arc::new(());
        true
    }
    /// 只准备合法局部区域；空选择为明确 noop，不创建 Document 历史。
    pub(crate) fn selected_regions(
        &self,
        source: &S,
        body: &str,
        identity: &Arc<()>,
    ) -> Option<Vec<Bounds>> {
        if !self.current(source, body, identity) {
            return None;
        }
        let mut regions = Vec::new();
        for candidate in &self.candidates {
            if !self.selected(candidate.id) {
                continue;
            }
            for &quad in &candidate.quads {
                let bounds = quad_bounds(quad)?;
                if !regions.contains(&bounds) {
                    regions.push(bounds);
                }
            }
        }
        Some(regions)
    }
}

/// 完整正文与行、词证据不一致时拒绝；无可信定位的格式不成为可遮盖候选。
pub(crate) fn candidates(source: &RecognizedText) -> Vec<Candidate> {
    if !source_ready(source) {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut offset = 0;
    let mut total_quads = 0;
    let mut classified = 0;
    let mut indexed_words = vec![Vec::new(); source.lines.len()];
    for word in &source.words {
        indexed_words[word.line].push(word);
    }
    for (index, line) in source.lines.iter().enumerate() {
        let Some(formats) = classify_line(&line.text) else {
            return Vec::new();
        };
        classified += formats.len();
        if classified > MAX_CANDIDATES {
            return Vec::new();
        }
        let words = &indexed_words[index];
        if let Some(ranges) = word_ranges(index, line, words) {
            for (kind, range) in formats {
                let Some(quads) = locate(line, index, words, &ranges, &range) else {
                    continue;
                };
                total_quads += quads.len();
                if total_quads > MAX_QUADS {
                    return Vec::new();
                }
                result.push(Candidate {
                    id: result.len(),
                    kind,
                    range: offset + range.start..offset + range.end,
                    line: index,
                    quads,
                });
            }
        }
        offset += line.text.len() + 1;
    }
    result
}

fn source_ready(source: &RecognizedText) -> bool {
    if source.text.is_empty()
        || source.text.len() > graphemes::MAX_SOURCE_BYTES
        || source.lines.is_empty()
        || source.lines.len() > MAX_LINES
        || source.words.len() > MAX_WORDS
    {
        return false;
    }
    let mut content = source.text.split('\n');
    let (mut words_bytes, mut groups, mut runs) = (0usize, 0usize, 0usize);
    for word in &source.words {
        if word.line >= source.lines.len() {
            return false;
        }
        words_bytes = words_bytes.saturating_add(word.text.len());
        if words_bytes > graphemes::MAX_SOURCE_BYTES {
            return false;
        }
    }
    for line in &source.lines {
        if content.next() != Some(line.text.as_str())
            || line.text.len() > graphemes::MAX_LINE_BYTES
            || line.text.chars().any(char::is_control)
        {
            return false;
        }
        if let Some(sampling) = &line.graphemes {
            groups = groups.saturating_add(sampling.grapheme_count());
            runs = runs.saturating_add(sampling.run_count());
            if groups > graphemes::MAX_TOTAL_GRAPHEMES || runs > graphemes::MAX_TOTAL_RUNS {
                return false;
            }
        }
    }
    content.next().is_none()
}

fn word_ranges(
    index: usize,
    line: &RecognizedLine,
    words: &[&RecognizedWord],
) -> Option<Vec<Range<usize>>> {
    let rect = line
        .rect
        .filter(|rect| rect.valid(MAX_COORDINATE, MAX_COORDINATE))?;
    if words.is_empty() {
        return None;
    }
    let (mut cursor, mut union) = (0, None);
    let mut ranges = Vec::with_capacity(words.len());
    let mut positions = BTreeSet::new();
    for word in words {
        if word.line != index
            || word.text.trim().is_empty()
            || word.text.chars().any(char::is_control)
            || !word.rect.valid(MAX_COORDINATE, MAX_COORDINATE)
            || word.horizontal_direction != line.horizontal_direction
            || !positions.insert((word.rect.x, word.rect.y, word.rect.width, word.rect.height))
        {
            return None;
        }
        let start = cursor + line.text.get(cursor..)?.find(&word.text)?;
        if !line
            .text
            .get(cursor..start)?
            .chars()
            .all(char::is_whitespace)
        {
            return None;
        }
        let end = start.checked_add(word.text.len())?;
        ranges.push(start..end);
        cursor = end;
        union = Some(match union {
            None => word.rect,
            Some(old) => union_rect(old, word.rect),
        });
    }
    (union == Some(rect) && line.text.get(cursor..)?.chars().all(char::is_whitespace))
        .then_some(ranges)
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn locate(
    line: &RecognizedLine,
    index: usize,
    words: &[&RecognizedWord],
    ranges: &[Range<usize>],
    wanted: &Range<usize>,
) -> Option<Vec<[Point; 4]>> {
    let mut quads = Vec::new();
    let mut cursor = wanted.start;
    // 只在词内部分需要真实采样，不让可用词级定位依赖不存在的 CTC 字素。
    let mut sampling = None;
    for (word, range) in words.iter().zip(ranges) {
        let start = wanted.start.max(range.start);
        let end = wanted.end.min(range.end);
        if start >= end {
            continue;
        }
        if !line
            .text
            .get(cursor..start)?
            .chars()
            .all(char::is_whitespace)
        {
            return None;
        }
        if start == range.start && end == range.end {
            let projection = word.geometry?.validated_in(word.rect)?;
            let direction = if projection.point(1., 0.5).x < projection.point(0., 0.5).x {
                HorizontalDirection::RightToLeft
            } else {
                HorizontalDirection::LeftToRight
            };
            if direction != word.horizontal_direction {
                return None;
            }
            quad_bounds(projection.quad())?;
            quads.push(projection.quad());
        } else {
            if sampling.is_none() {
                let proof = line.graphemes.as_ref()?;
                sampling = Some((
                    Projection::new(proof.source_geometry())?,
                    proof.validated_bands(index, line, words)?,
                ));
            }
            let (projection, bands) = sampling.as_ref()?;
            let mut covered = start;
            for band in bands {
                if band.source_range.end <= start || band.source_range.start >= end {
                    continue;
                }
                if band.source_range.start != covered || band.source_range.end > end {
                    return None;
                }
                covered = band.source_range.end;
                let quad = [
                    projection.point(band.start, 0.),
                    projection.point(band.end, 0.),
                    projection.point(band.end, 1.),
                    projection.point(band.start, 1.),
                ];
                quad_bounds(quad)?;
                if !line
                    .text
                    .get(band.source_range.clone())?
                    .chars()
                    .all(char::is_whitespace)
                {
                    quads.push(quad);
                }
                if quads.len() > MAX_QUADS {
                    return None;
                }
            }
            if covered != end {
                return None;
            }
        }
        cursor = end;
    }
    if quads.is_empty()
        || !line
            .text
            .get(cursor..wanted.end)?
            .chars()
            .all(char::is_whitespace)
    {
        return None;
    }
    Some(quads)
}

fn quad_bounds(quad: [Point; 4]) -> Option<Bounds> {
    Projection::new(TextGeometry { quad })?;
    let (mut x, mut y, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for point in quad {
        if point.x < 0.
            || point.y < 0.
            || point.x > MAX_COORDINATE as f32
            || point.y > MAX_COORDINATE as f32
        {
            return None;
        }
        (x, y, right, bottom) = (
            x.min(point.x),
            y.min(point.y),
            right.max(point.x),
            bottom.max(point.y),
        );
    }
    (right > x && bottom > y).then_some(Bounds {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

fn classify_line(text: &str) -> Option<Vec<(SensitiveKind, Range<usize>)>> {
    let bytes = text.as_bytes();
    let mut formats = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if email_byte(bytes[cursor]) {
            let start = cursor;
            while cursor < bytes.len() && email_byte(bytes[cursor]) {
                cursor += 1;
            }
            if valid_email(&text[start..cursor]) {
                formats.push((SensitiveKind::Email, start..cursor));
            }
        } else {
            cursor += 1;
        }
        if formats.len() > MAX_CANDIDATES {
            return None;
        }
    }
    cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_digit() || bytes[cursor] == b'+' || bytes[cursor] == b'(' {
            let start = cursor;
            while cursor < bytes.len() && phone_byte(bytes[cursor]) {
                cursor += 1;
            }
            let end = text[start..cursor].trim_end_matches(' ').len() + start;
            let value = &text[start..end];
            let isolated = (start == 0 || !token_neighbor(bytes[start - 1]))
                && (end == bytes.len() || !token_neighbor(bytes[end]));
            if isolated && valid_phone(value, phone_label(&text[..start])) {
                formats.push((SensitiveKind::Phone, start..end));
            }
        } else {
            cursor += 1;
        }
        if formats.len() > MAX_CANDIDATES {
            return None;
        }
    }
    cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_alphanumeric() {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_alphanumeric() {
                cursor += 1;
            }
            if valid_national_id(&text[start..cursor]) {
                formats.push((SensitiveKind::NationalId, start..cursor));
            }
        } else {
            cursor += 1;
        }
        if formats.len() > MAX_CANDIDATES {
            return None;
        }
    }
    formats.sort_by_key(|(_, range)| (range.start, range.end));
    // 任何重叠格式都拒绝，而不是把一个数字片段重复作为不同类型遮盖。
    let overlapping: Vec<_> = formats
        .windows(2)
        .filter(|pair| pair[0].1.end > pair[1].1.start)
        .flat_map(|pair| [pair[0].1.clone(), pair[1].1.clone()])
        .collect();
    formats.retain(|(_, range)| !overlapping.contains(range));
    Some(formats)
}

fn email_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._%+-@".contains(&byte)
}
fn valid_email(value: &str) -> bool {
    if value.len() > 254 {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._%+-".contains(&byte))
    {
        return false;
    }
    let labels: Vec<_> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        && labels.last().is_some_and(|label| {
            label.len() >= 2 && label.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
}
fn phone_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || b" +-()".contains(&byte)
}
fn token_neighbor(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._@+".contains(&byte)
}
fn phone_label(prefix: &str) -> bool {
    let trimmed =
        prefix.trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ':' | '：'));
    if prefix.len() - trimmed.len() > 24 {
        return false;
    }
    if trimmed.ends_with("电话") || trimmed.ends_with("手机") {
        return true;
    }
    let ascii = trimmed.as_bytes();
    [b"phone".as_slice(), b"tel".as_slice()]
        .iter()
        .any(|label| {
            ascii.len() >= label.len()
                && ascii[ascii.len() - label.len()..].eq_ignore_ascii_case(label)
                && (ascii.len() == label.len()
                    || !ascii[ascii.len() - label.len() - 1].is_ascii_alphanumeric())
        })
}
fn valid_phone(value: &str, labeled: bool) -> bool {
    if value.is_empty()
        || value.len() > 40
        || !value.as_bytes().last().is_some_and(u8::is_ascii_digit)
    {
        return false;
    }
    if let Some(local) = value.strip_prefix("+86") {
        // 国家前缀后的分隔只消费一次，不以 filter(digit) 掩盖错误分隔。
        let local = local.strip_prefix([' ', '-']).unwrap_or(local);
        return valid_mobile_format(local);
    }
    labeled && (valid_mobile_format(value) || valid_landline_format(value))
}
fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn valid_mobile_format(value: &str) -> bool {
    let compact = if digits(value) {
        value.to_owned()
    } else {
        let separator = if value.contains('-') { '-' } else { ' ' };
        let groups: Vec<_> = value.split(separator).collect();
        if groups.len() != 3
            || groups.iter().map(|group| group.len()).collect::<Vec<_>>() != [3, 4, 4]
            || !groups.iter().all(|group| digits(group))
        {
            return false;
        }
        groups.concat()
    };
    let bytes = compact.as_bytes();
    bytes.len() == 11 && bytes[0] == b'1' && (b'3'..=b'9').contains(&bytes[1])
}
fn valid_area_code(value: &str) -> bool {
    digits(value)
        && ((value.len() == 3 && (value == "010" || value.starts_with("02")))
            || (value.len() == 4
                && value.as_bytes()[0] == b'0'
                && (b'3'..=b'9').contains(&value.as_bytes()[1])))
}
fn valid_landline_format(value: &str) -> bool {
    let (area, subscriber) = if let Some(parenthesized) = value.strip_prefix('(') {
        let Some((area, subscriber)) = parenthesized.split_once(')') else {
            return false;
        };
        (
            area,
            subscriber.strip_prefix([' ', '-']).unwrap_or(subscriber),
        )
    } else if digits(value) {
        let area_bytes = if value.starts_with("010") || value.starts_with("02") {
            3
        } else {
            4
        };
        if value.len() < area_bytes {
            return false;
        }
        value.split_at(area_bytes)
    } else {
        let Some((area, subscriber)) = value.split_once([' ', '-']) else {
            return false;
        };
        (area, subscriber)
    };
    valid_area_code(area) && (7..=8).contains(&subscriber.len()) && digits(subscriber)
}
fn valid_national_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 18
        || !bytes[..17].iter().all(u8::is_ascii_digit)
        || !(bytes[17].is_ascii_digit() || matches!(bytes[17], b'x' | b'X'))
    {
        return false;
    }
    let province = (bytes[0] - b'0') * 10 + bytes[1] - b'0';
    if !matches!(province, 11..=15 | 21..=23 | 31..=37 | 41..=46 | 50..=54 | 61..=65)
        || &bytes[2..4] == b"00"
        || &bytes[4..6] == b"00"
        || &bytes[14..17] == b"000"
    {
        return false;
    }
    let number = |range: Range<usize>| {
        bytes[range]
            .iter()
            .fold(0u32, |number, byte| number * 10 + u32::from(byte - b'0'))
    };
    let (year, month, day) = (number(6..10), number(10..12), number(12..14));
    if !(1900..=2099).contains(&year) || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days {
        return false;
    }
    let weights = [7u32, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    let sum: u32 = bytes[..17]
        .iter()
        .zip(weights)
        .map(|(digit, weight)| u32::from(digit - b'0') * weight)
        .sum();
    bytes[17].to_ascii_uppercase() == b"10X98765432"[(sum % 11) as usize]
}

#[cfg(test)]
#[path = "ocr_sensitive_tests.rs"]
mod tests;

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    fn preview(values: &[&str]) -> Preview<u64> {
        let mut source = RecognizedText {
            text: values.join("\n"),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        };
        for (line, value) in values.iter().enumerate() {
            let rect = Rect {
                x: 10,
                y: 20 + line as u32 * 30,
                width: 240,
                height: 20,
            };
            let geometry = TextGeometry {
                quad: [
                    Point {
                        x: 10.,
                        y: rect.y as f32,
                    },
                    Point {
                        x: 250.,
                        y: rect.y as f32,
                    },
                    Point {
                        x: 250.,
                        y: (rect.y + 20) as f32,
                    },
                    Point {
                        x: 10.,
                        y: (rect.y + 20) as f32,
                    },
                ],
            };
            source.words.push(RecognizedWord {
                text: (*value).into(),
                rect,
                line,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            });
            source.lines.push(RecognizedLine {
                text: (*value).into(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
                graphemes: None,
            });
        }
        let snapshot = Snapshot::from_recognized(&source).unwrap();
        Preview::new(7, &snapshot)
    }

    #[test]
    fn sensitive_preview_keyboard_starts_unselected_and_navigates_bounded_candidates() {
        let mut preview = preview(&[
            "alice@example.test",
            "bob@example.test",
            "carol@example.test",
        ]);
        assert_eq!(preview.candidates().len(), 3);
        assert_eq!(preview.active(), Some(0));
        assert!(preview
            .candidates()
            .iter()
            .all(|candidate| !preview.selected(candidate.id)));
        assert!(!preview.navigate(PreviewNavigation::Previous));
        assert!(preview.navigate(PreviewNavigation::Next));
        assert_eq!(preview.active(), Some(1));
        assert!(preview.navigate(PreviewNavigation::Last));
        assert_eq!(preview.active(), Some(2));
        assert!(!preview.navigate(PreviewNavigation::Next));
        assert!(preview.navigate(PreviewNavigation::First));
        assert_eq!(preview.active(), Some(0));
        assert!(!preview.set_active(3));
    }

    #[test]
    fn sensitive_preview_keyboard_changes_invalidate_confirmation_and_preserve_text_geometry() {
        let mut preview = preview(&["alice@example.test", "bob@example.test"]);
        let text = preview.text().to_owned();
        let candidates = preview.candidates().to_vec();
        let pressed = Arc::clone(preview.identity());
        assert!(preview.navigate(PreviewNavigation::Last));
        assert!(!preview.current(&7, &text, &pressed));
        let active = Arc::clone(preview.identity());
        assert!(preview.toggle_active());
        assert!(!preview.selected(0));
        assert!(preview.selected(1));
        assert!(!preview.current(&7, &text, &active));
        let selected = Arc::clone(preview.identity());
        assert!(preview.select_all());
        assert!(!preview.current(&7, &text, &selected));
        let all = Arc::clone(preview.identity());
        assert!(!preview.select_all());
        assert!(preview.current(&7, &text, &all));
        assert!(!preview.current(&8, &text, &all));
        assert!(!preview.current(&7, "edited", &all));
        assert_eq!(preview.text(), text);
        assert_eq!(preview.candidates(), candidates);
    }

    #[test]
    fn sensitive_preview_keyboard_empty_batch_is_a_complete_noop() {
        let mut preview = preview(&["ordinary text"]);
        let identity = Arc::clone(preview.identity());
        assert!(preview.candidates().is_empty());
        assert_eq!(preview.active(), None);
        for navigation in [
            PreviewNavigation::Previous,
            PreviewNavigation::Next,
            PreviewNavigation::First,
            PreviewNavigation::Last,
        ] {
            assert!(!preview.navigate(navigation));
        }
        assert!(!preview.toggle_active());
        assert!(!preview.select_all());
        assert!(!preview.set_active(0));
        assert!(preview.current(&7, "ordinary text", &identity));
        assert_eq!(
            preview.selected_regions(&7, "ordinary text", &identity),
            Some(Vec::new())
        );
    }
}
