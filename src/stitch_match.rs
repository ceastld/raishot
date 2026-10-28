//! 匹配先用稀疏行指纹提出位置，再完整验证重叠；小范围动态像素有严格预算，等价位置仍拒绝。
use super::{Image, ScrollAxis};
use std::collections::{BTreeSet, HashMap};

pub(super) struct Line<'a> {
    bytes: &'a [u8],
    offset: usize,
    stride: usize,
}
impl Line<'_> {
    fn pixel(&self, x: u32) -> &[u8] {
        let offset = self.offset + x as usize * self.stride;
        &self.bytes[offset..offset + 4]
    }
}
pub(super) fn image_line(image: &Image, axis: ScrollAxis, along: u32) -> Line<'_> {
    let (offset, stride) = match axis {
        ScrollAxis::Vertical => (along as usize * image.width as usize * 4, 4),
        ScrollAxis::Horizontal => (along as usize * 4, image.width as usize * 4),
    };
    Line {
        bytes: &image.bgra,
        offset,
        stride,
    }
}
pub(super) trait Pixels {
    fn length(&self) -> u32;
    fn cross(&self) -> u32;
    fn line(&self, along: u32) -> Line<'_>;
    fn hash(&self, along: u32) -> u64;
}
pub(super) struct Frame<'a> {
    image: &'a Image,
    axis: ScrollAxis,
    pub start: u32,
    length: u32,
    cross: u32,
    pub hashes: Vec<u64>,
}
impl<'a> Frame<'a> {
    pub fn new(image: &'a Image, axis: ScrollAxis, start: u32, length: u32) -> Self {
        let mut frame = Self::view(image, axis, start, length);
        frame.hashes = (0..length)
            .map(|i| line_hash(frame.line(i), frame.cross))
            .collect();
        frame
    }
    /// 相等性/固定边带比较无需创建指纹；只有待定位正文才建立指纹。
    pub fn view(image: &'a Image, axis: ScrollAxis, start: u32, length: u32) -> Self {
        let (cross, _) = axis.size(image);
        Self {
            image,
            axis,
            start,
            length,
            cross,
            hashes: Vec::new(),
        }
    }
}
impl Pixels for Frame<'_> {
    fn length(&self) -> u32 {
        self.length
    }
    fn cross(&self) -> u32 {
        self.cross
    }
    fn line(&self, along: u32) -> Line<'_> {
        image_line(self.image, self.axis, self.start + along)
    }
    fn hash(&self, along: u32) -> u64 {
        self.hashes
            .get(along as usize)
            .copied()
            .unwrap_or_else(|| line_hash(self.line(along), self.cross))
    }
}
fn line_hash(line: Line<'_>, cross: u32) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    // 覆盖完整行；固定的 48 个采样点会把落在格子之间的窄字列当作白行。
    // 每像素只混合一次打包颜色，保留 alpha，RGB 去掉低三位以容忍小量采集噪声。
    for x in 0..cross {
        let pixel = u32::from_le_bytes(line.pixel(x).try_into().expect("像素宽度"));
        hash = (hash ^ u64::from(pixel & 0xfff8f8f8)).wrapping_mul(0x100000001b3);
    }
    hash
}
pub(super) fn line_equal(a: Line<'_>, b: Line<'_>, cross: u32) -> bool {
    if a.stride == 4 && b.stride == 4 {
        a.bytes[a.offset..a.offset + cross as usize * 4]
            == b.bytes[b.offset..b.offset + cross as usize * 4]
    } else {
        (0..cross).all(|x| a.pixel(x) == b.pixel(x))
    }
}
pub(super) fn equal(a: &impl Pixels, b: &impl Pixels) -> bool {
    a.length() == b.length()
        && a.cross() == b.cross()
        && (0..a.length()).all(|i| line_equal(a.line(i), b.line(i), a.cross()))
}
pub(super) fn textured(source: &impl Pixels, start: u32, length: u32) -> bool {
    let mut low = [255_u8; 3];
    let mut high = [0_u8; 3];
    for y in 0..length.min(32) {
        let line = source.line(start + y * (length - 1) / (length.min(32) - 1).max(1));
        for x in 0..source.cross().min(48) {
            let pixel = line.pixel(x * (source.cross() - 1) / (source.cross().min(48) - 1).max(1));
            for c in 0..3 {
                low[c] = low[c].min(pixel[c]);
                high[c] = high[c].max(pixel[c]);
            }
        }
    }
    if (0..3).any(|c| high[c].saturating_sub(low[c]) >= 12) {
        return true;
    }
    // 快速网格不能证明没有纹理；稀疏文字可能同时落在横纵采样间隙。
    // 最坏只扫描本次范围一次（采集帧已有 3200 万像素上限），发现纹理立即返回。
    for y in 0..length {
        let line = source.line(start + y);
        for x in 0..source.cross() {
            let pixel = line.pixel(x);
            for c in 0..3 {
                low[c] = low[c].min(pixel[c]);
                high[c] = high[c].max(pixel[c]);
                if high[c].saturating_sub(low[c]) >= 12 {
                    return true;
                }
            }
        }
    }
    false
}
pub(super) enum Match {
    Unique(i64),
    Missing,
    Ambiguous,
}
fn overlap(source: u32, incoming: u32, position: i64) -> Option<(u32, u32, u32)> {
    let from = position.max(0);
    let to = (position + incoming as i64).min(source as i64);
    (to > from).then_some((from as u32, (from - position) as u32, (to - from) as u32))
}
pub(super) fn guard_edges(
    source: &impl Pixels,
    incoming: &impl Pixels,
    prior: &impl Pixels,
    position: i64,
    start: u32,
    end: u32,
) -> bool {
    if start == 0 && end == 0 {
        return true;
    }
    let Some((from, to, length)) = overlap(source.length(), incoming.length(), position) else {
        return false;
    };
    // 仅两帧均存在连续轨道/滑块的同色窄条时豁免滚动条，不固定忽略靠边文字。
    let gutter = scrollbar_tail(prior).min(scrollbar_tail(incoming));
    for row in 0..length {
        if row >= start.min(length) && row < length.saturating_sub(end) {
            continue;
        }
        let a = source.line(from + row);
        let b = incoming.line(to + row);
        for x in 0..source.cross() - gutter {
            let a = a.pixel(x);
            let b = b.pixel(x);
            if a[3] != b[3] || (0..3).any(|c| a[c].abs_diff(b[c]) > 8) {
                return false;
            }
        }
    }
    true
}
fn scrollbar_tail(frame: &impl Pixels) -> u32 {
    let mut width = frame.cross().saturating_sub(1).min(23);
    let mut previous: Option<[u8; 4]> = None;
    let mut transitions = 0;
    let mut contrast = false;
    for row in 0..frame.length() {
        let line = frame.line(row);
        let edge = line.pixel(frame.cross() - 1);
        if let Some(last) = previous {
            if edge != last {
                transitions += 1;
                if transitions > 2 {
                    return 0;
                }
                contrast |= (0..3).any(|c| edge[c].abs_diff(last[c]) >= 12);
            }
        }
        previous = Some([edge[0], edge[1], edge[2], edge[3]]);
        for offset in 1..=width {
            if line.pixel(frame.cross() - 1 - offset) != edge {
                width = offset - 1;
                break;
            }
        }
        if width == 0 {
            return 0;
        }
    }
    // 一条滑块最多带来两次颜色转换；整列纯色背景不能证明有滚动条。
    if contrast {
        width + 1
    } else {
        0
    }
}
pub(super) fn locate(source: &impl Pixels, incoming: &impl Pixels, preferred: i64) -> Match {
    let minimum = 48.min(incoming.length() / 2);
    // 纯色轻微变化没有定位证据；在搜索前拒绝，避免对每个位移完整扫白页。
    if !textured(incoming, 0, incoming.length()) {
        return Match::Missing;
    }
    // 近似指纹只负责补选，不允许候选数量把一轮采集放大为近二次全图比较。
    // 超大合法 ROI 至少可验证两个完整候选；通常一轮最多比较约 1600 万像素。
    let mut remaining_work =
        (incoming.length() as u64 * incoming.cross() as u64 * 2).max(16_000_000);
    let probes = content_probes(incoming);
    // 常见字型的单行可重复数百次；相邻三行的组合仍能定位其不同排列。
    // 复用条带缓存指纹，不重新读取原图像素，周期正文仍由完整多位置验证拒绝。
    let anchor_hash = |pixels: &dyn Pixels, row: u32| {
        pixels.hash(row).wrapping_mul(0x9e3779b185ebca87)
            ^ pixels.hash(row + 1).rotate_left(21)
            ^ pixels.hash(row + 2).rotate_left(43)
    };
    // 少量稀有指纹优先于白色行，避免白边产生海量候选；指纹不作为最终匹配证据。
    let mut index: HashMap<u64, Vec<u32>> = HashMap::new();
    for i in 0..source.length().saturating_sub(2) {
        index.entry(anchor_hash(source, i)).or_default().push(i);
    }
    let mut anchors: Vec<_> = (0..incoming.length().saturating_sub(2))
        .filter_map(|i| {
            index
                .get(&anchor_hash(incoming, i))
                // 重复白行不能成为定位锚点；真正重复的纹理由最后的多位置验证拒绝。
                .filter(|positions| positions.len() <= 64)
                .map(|positions| (positions.len(), i))
        })
        .collect();
    anchors.sort_unstable();
    let mut selected = Vec::new();
    let separation = (incoming.length() / 24).max(1);
    for (_, i) in anchors {
        if selected
            .iter()
            .all(|&other: &u32| other.abs_diff(i) >= separation)
        {
            selected.push(i);
        }
        if selected.len() == 16 {
            break;
        }
    }
    let mut candidates = BTreeSet::new();
    for i in selected {
        for &line in &index[&anchor_hash(incoming, i)] {
            let position = line as i64 - i as i64;
            if overlap(source.length(), incoming.length(), position)
                .is_some_and(|(_, _, n)| n >= minimum)
            {
                candidates.insert(position);
            }
        }
    }
    let mut verified = Vec::new();
    for &position in &candidates {
        if !verify(
            source,
            incoming,
            position,
            &mut verified,
            &mut remaining_work,
            &probes,
        ) {
            return Match::Ambiguous;
        }
    }
    // 白行可能只提出错误候选，字形抗锯齿又可能破坏真实位置的精确指纹。
    // 因此在所有精确候选未通过时补搜，而不是仅在没有候选时补搜。
    if verified.is_empty() {
        let range = incoming.length() - minimum;
        // 先检查原视口附近的小位移；从最远负位移开始会被白色假重叠耗尽工作预算。
        for distance in 0..=range as i64 {
            for delta in [distance, -distance]
                .into_iter()
                .take(if distance == 0 { 1 } else { 2 })
            {
                let position = preferred + delta;
                if candidates.contains(&position) {
                    continue;
                }
                if !verify(
                    source,
                    incoming,
                    position,
                    &mut verified,
                    &mut remaining_work,
                    &probes,
                ) {
                    return Match::Ambiguous;
                }
                // 两个近乎无误差的位置已足以证明歧义，避免周期纹理扫完全部位移。
                if verified.iter().filter(|&&(value, _)| value <= 0.05).count() >= 2 {
                    return Match::Ambiguous;
                }
            }
        }
    }
    verified.sort_by(|a, b| a.0.total_cmp(&b.0));
    let Some(&(best, position)) = verified.first() else {
        return Match::Missing;
    };
    if verified
        .iter()
        .skip(1)
        .any(|&(other, candidate)| candidate != position && other <= best + 0.05)
    {
        Match::Ambiguous
    } else {
        Match::Unique(position)
    }
}
fn verify(
    source: &impl Pixels,
    incoming: &impl Pixels,
    position: i64,
    verified: &mut Vec<(f64, i64)>,
    remaining_work: &mut u64,
    probes: &[ContentProbe],
) -> bool {
    if !probe_match(source, incoming, position, probes) {
        return true;
    }
    if score(source, incoming, position, false).is_some() {
        let Some((_, _, length)) = overlap(source.length(), incoming.length(), position) else {
            return true;
        };
        let pixels = length as u64 * source.cross() as u64;
        if pixels > *remaining_work {
            return false;
        }
        *remaining_work -= pixels;
        if let Some(value) = score(source, incoming, position, true) {
            verified.push((value, position));
        }
    }
    true
}

struct ContentProbe {
    row: u32,
    columns: Vec<u32>,
    background_columns: Vec<u32>,
}
fn content_probes(incoming: &impl Pixels) -> Vec<ContentProbe> {
    // 只弱化贴边的真实轨道，不把最右侧的窄字列一概忽略。
    let mut cross_end = incoming.cross();
    for x in (incoming.cross().saturating_sub(incoming.cross() / 32)..incoming.cross()).rev() {
        let mut run_start = 0;
        let mut longest = (0u32, 0u32);
        let mut last = [0u8; 4];
        for row in 0..incoming.length() {
            let line = incoming.line(row);
            let pixel: [u8; 4] = line.pixel(x).try_into().expect("像素宽度");
            if row == 0 || pixel != last {
                run_start = row;
                last = pixel;
            }
            let length = row - run_start + 1;
            if length > longest.1 {
                longest = (run_start, length);
            }
        }
        if longest.1 < (incoming.length() / 3).max(16) {
            break;
        }
        let line = incoming.line(longest.0 + longest.1 / 2);
        let reference = background(&line, incoming.cross());
        let pixel = line.pixel(x);
        if !(0..3).any(|c| pixel[c].abs_diff(reference[c]) >= 12) {
            break;
        }
        cross_end = x;
    }
    let mut probes: Vec<ContentProbe> = Vec::new();
    let mut clusters: Vec<(u32, u32)> = Vec::new();
    // 每段寻找一行正文；重复字型也有投票权，稀有动态图标不能独占探针。
    for bin in 0..16 {
        let start = incoming.length() * bin / 16;
        let end = incoming.length() * (bin + 1) / 16;
        if start == end {
            continue;
        }
        let middle = (start + end) / 2;
        let rows = (middle..end).chain(start..middle);
        for row in rows {
            let line = incoming.line(row);
            let reference = background(&line, incoming.cross());
            let content: Vec<_> = (0..cross_end)
                .filter(|&x| {
                    let pixel = line.pixel(x);
                    (0..3).any(|c| pixel[c].abs_diff(reference[c]) >= 12)
                })
                .collect();
            if content.is_empty() {
                continue;
            }
            let mut cluster = (content[0], content[0] + 1);
            for &x in &content[1..] {
                if x <= cluster.1 + 8 {
                    cluster.1 = x + 1;
                } else {
                    clusters.push(cluster);
                    cluster = (x, x + 1);
                }
            }
            clusters.push(cluster);
            let samples = content.len().min(16);
            let columns = (0..samples)
                .map(|i| content[i * (content.len() - 1) / (samples - 1).max(1)])
                .collect();
            probes.push(ContentProbe {
                row,
                columns,
                background_columns: Vec::new(),
            });
            break;
        }
    }
    // 不同正文行的前景合成文字簇范围；字型内部空隙也必须参与定位，
    // 单看 incoming 黑点会漏掉 source 多出的笔画，让错误候选消耗完整验证预算。
    clusters.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in clusters {
        if let Some(last) = merged.last_mut().filter(|last| start <= last.1 + 8) {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    for probe in &mut probes {
        let line = incoming.line(probe.row);
        let reference = background(&line, incoming.cross());
        let mut gaps = Vec::new();
        for &(start, end) in &merged {
            // 当前行没有这个簇的正文时，不让其它行的动态图标空间成为采样背景。
            if !probe.columns.iter().any(|&x| x >= start && x < end) {
                continue;
            }
            gaps.extend((start..end).filter(|&x| {
                let pixel = line.pixel(x);
                (0..3).all(|c| pixel[c].abs_diff(reference[c]) < 12)
            }));
        }
        let samples = gaps.len().min(16);
        probe.background_columns = (0..samples)
            .map(|i| gaps[i * (gaps.len() - 1) / (samples - 1).max(1)])
            .collect();
    }
    probes
}
fn probe_match(
    source: &impl Pixels,
    incoming: &impl Pixels,
    position: i64,
    probes: &[ContentProbe],
) -> bool {
    let Some((from, to, length)) = overlap(source.length(), incoming.length(), position) else {
        return false;
    };
    let mut tested = 0u32;
    let mut difference = 0.;
    let mut rows = 0u32;
    for probe in probes {
        if probe.row < to || probe.row >= to + length {
            continue;
        }
        let a = source.line(from + probe.row - to);
        let b = incoming.line(probe.row);
        rows += 1;
        let mut row_difference = 0.;
        let mut groups = 0u32;
        for columns in [&probe.columns, &probe.background_columns] {
            if columns.is_empty() {
                continue;
            }
            groups += 1;
            let mut different = 0u32;
            for &x in columns {
                let a = a.pixel(x);
                let b = b.pixel(x);
                tested += 1;
                different += u32::from(a[3] != b[3] || (0..3).any(|c| a[c].abs_diff(b[c]) > 8));
            }
            row_difference += different as f64 / columns.len() as f64;
        }
        // 每段等权，不能让一个较宽动态块压过大量较窄但稳定的正文。
        difference += row_difference / groups.max(1) as f64;
    }
    // 不足两行正文时不把采样当作拒绝证据，最终仍由完整验证决定。
    rows < 2 || tested < 16 || difference <= rows as f64 / 4.
}
fn background(line: &Line<'_>, cross: u32) -> [u8; 3] {
    let mut color = [0; 3];
    for (channel, color) in color.iter_mut().enumerate() {
        let mut samples = [0; 5];
        for (index, value) in samples.iter_mut().enumerate() {
            *value = line.pixel(index as u32 * (cross - 1) / 4)[channel];
        }
        samples.sort_unstable();
        *color = samples[2];
    }
    color
}
fn score(source: &impl Pixels, incoming: &impl Pixels, position: i64, full: bool) -> Option<f64> {
    let (from, to, length) = overlap(source.length(), incoming.length(), position)?;
    if length < 48.min(incoming.length() / 2) {
        return None;
    }
    let lines = if full { length } else { length.min(25) };
    let columns = if full {
        source.cross()
    } else {
        source.cross().min(33)
    };
    let pixels = lines as u64 * columns as u64;
    let limit = if full { 0.75 } else { 1.25 };
    // 最多 3% 强变化像素，且任何一行最多 1/12。光标、窄滚动条可变化，
    // 但动画块不能凭大面积白色稀释掉。透明度必须一致，不纳入异常预算。
    let budget = (pixels * 3 / 100).max(1);
    let line_budget = (columns / 12).max(2) as u64;
    let mut sum = 0_u64;
    let mut outliers = 0_u64;
    let mut contrast_pixels = 0_u64;
    let mut textured_lines = 0_u32;
    let mut first_texture = None;
    let mut last_texture = 0_u32;
    for row in 0..lines {
        let y = row * (length - 1) / (lines - 1).max(1);
        let a = source.line(from + y);
        let b = incoming.line(to + y);
        // 用分散采样中值估算行背景，左侧黑边不能把整张白页当作有效正文。
        let reference_a = background(&a, source.cross());
        let reference_b = background(&b, source.cross());
        let mut line_outliers = 0_u64;
        let mut low = [255_u8; 3];
        let mut high = [0_u8; 3];
        for column in 0..columns {
            let x = column * (source.cross() - 1) / (columns - 1).max(1);
            let a = a.pixel(x);
            let b = b.pixel(x);
            if (0..3)
                .any(|c| a[c].abs_diff(reference_a[c]) >= 12 || b[c].abs_diff(reference_b[c]) >= 12)
            {
                contrast_pixels += 1;
            }
            if a[3] != b[3] {
                return None;
            }
            if (0..3).any(|c| a[c].abs_diff(b[c]) > 8) {
                line_outliers += 1;
                outliers += 1;
                if line_outliers > line_budget || outliers > budget {
                    return None;
                }
                continue;
            }
            for c in 0..3 {
                sum += a[c].abs_diff(b[c]) as u64;
                low[c] = low[c].min(a[c]);
                high[c] = high[c].max(a[c]);
            }
        }
        if full && (0..3).any(|c| high[c].saturating_sub(low[c]) >= 12) {
            textured_lines += 1;
            first_texture.get_or_insert(y);
            last_texture = y;
        }
        if sum as f64 > pixels as f64 * 4. * limit {
            return None;
        }
    }
    // 纹理证据要来自沿滚动轴分散的正文，而非单个图标/一条分隔线；
    // 不强求横向覆盖，窄聊天文字列和左右留白同样可获得可靠证据。
    if full
        && (textured_lines < 8.min(length / 8)
            || first_texture.is_none_or(|first| last_texture - first < length / 4)
            // 白页上的文字即使只占很小面积，也不能整体被当成异常忽略。
            // 额外按有效对比内容预算，至少 3/4 的内容像素仍须支持该位移。
            || outliers > (contrast_pixels / 4).max(2))
    {
        return None;
    }
    let average = sum as f64 / ((pixels - outliers) * 4) as f64;
    // 误差以正文对比内容归一，不能被白页面积稀释成 ±1 像素也近乎等价。
    (average <= limit).then_some(average + outliers as f64 / contrast_pixels.max(1) as f64)
}

#[cfg(test)]
#[path = "stitch_match_tests.rs"]
mod tests;
