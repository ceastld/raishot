//! 仅合并正文、方向与真实裁行投影都高度一致的重复检测候选。
//! 词投影来自 CTC 估算带，不能据此声称精确墨迹或修整字框。
use crate::{
    annotation::Point,
    geometry::Rect,
    recognition::{
        geometry::{Projection, ReadingDirection},
        HorizontalDirection, RecognizedText,
    },
    Result,
};

// 与现有 det 候选预算一致；超出预算时保留原对象。
const MAX_LINES: usize = 1_000;
const MIN_AXIS_COSINE: f64 = 0.99985;
const MAX_LOCAL_U_ERROR: f64 = 0.025;
const MAX_LOCAL_V_ERROR: f64 = 0.075;
const MAX_HEIGHT_RATIO: f64 = 1.05;
const MAX_ALONG_HEIGHT: f64 = 0.10;
const MAX_NORMAL_HEIGHT: f64 = 0.05;
const BAND_EPSILON: f64 = 0.0001;

#[derive(Clone, Copy, Debug)]
pub(super) struct LineQuality {
    pub(super) recognition: f32,
    pub(super) detection: f32,
}

impl LineQuality {
    fn valid(self) -> bool {
        [self.recognition, self.detection]
            .into_iter()
            .all(|value| value.is_finite() && (0. ..=1.).contains(&value))
    }

    fn better_than(self, other: Self) -> bool {
        self.recognition > other.recognition
            || (self.recognition == other.recognition && self.detection > other.detection)
    }
}

#[derive(Clone, Copy)]
struct Band {
    projection: Projection,
    direction: ReadingDirection,
    center: [f64; 2],
    axis: [f64; 2],
    height: f64,
}

impl Band {
    fn new(projection: Projection) -> Option<Self> {
        let start = coordinates(projection.point(0., 0.5));
        let end = coordinates(projection.point(1., 0.5));
        let delta = subtract(end, start);
        let length = delta[0].hypot(delta[1]);
        if !length.is_finite() || length <= 0.0001 {
            return None;
        }
        let axis = [delta[0] / length, delta[1] / length];
        let normal = [-axis[1], axis[0]];
        let mut height = f64::INFINITY;
        for u in [0., 0.5, 1.] {
            let top = coordinates(projection.point(u, 0.));
            let bottom = coordinates(projection.point(u, 1.));
            let transverse = dot(subtract(bottom, top), normal);
            if !transverse.is_finite() || transverse <= 0.0001 {
                return None;
            }
            height = height.min(transverse);
        }
        let center = coordinates(projection.point(0.5, 0.5));
        center
            .iter()
            .all(|value| value.is_finite())
            .then_some(Self {
                projection,
                direction: projection.direction(),
                center,
                axis,
                height,
            })
    }

    fn horizontal_direction(self) -> HorizontalDirection {
        if self.axis[0] < 0. {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        }
    }
}

struct LineGeometry {
    line: Band,
    words: Vec<Band>,
}

struct Group {
    // 保留所有成员，禁止只对赢家比较产生传递式桥接。
    members: Vec<usize>,
    winner: usize,
}

pub(super) fn deduplicate(
    result: RecognizedText,
    quality: &[LineQuality],
    image: Rect,
    mut checkpoint: impl FnMut() -> Result<()>,
) -> Result<RecognizedText> {
    checkpoint()?;
    if result.lines.len() < 2
        || result.lines.len() > MAX_LINES
        || quality.len() != result.lines.len()
    {
        return Ok(result);
    }
    let Some(word_indices) = validate_contract(&result, image, &mut checkpoint)? else {
        return Ok(result);
    };
    let mut geometry = Vec::with_capacity(result.lines.len());
    for (index, score) in quality.iter().enumerate() {
        checkpoint()?;
        geometry.push(if score.valid() {
            prepare_geometry(&result, index, &word_indices[index], image, &mut checkpoint)?
        } else {
            None
        });
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut changed = false;
    for index in 0..result.lines.len() {
        checkpoint()?;
        let mut matching_group = None;
        let mut ambiguous = false;
        if geometry[index].is_some() {
            for (group_index, group) in groups.iter().enumerate() {
                let mut matches = true;
                for &member in &group.members {
                    checkpoint()?;
                    if !same_physical_line(
                        &result,
                        &word_indices,
                        &geometry,
                        index,
                        member,
                        &mut checkpoint,
                    )? {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    if matching_group.is_some() {
                        // 新行可同时连到两个组时，宁可保留为独立候选。
                        ambiguous = true;
                        break;
                    }
                    matching_group = Some(group_index);
                }
            }
        }
        if let Some(group_index) = matching_group.filter(|_| !ambiguous) {
            let group = &mut groups[group_index];
            group.members.push(index);
            if quality[index].better_than(quality[group.winner]) {
                group.winner = index;
            }
            changed = true;
        } else {
            groups.push(Group {
                members: vec![index],
                winner: index,
            });
        }
    }
    checkpoint()?;
    if !changed {
        return Ok(result);
    }

    // 组按首次出现顺序生成；赢家完整行落在组的首个槽位，不重新排序阅读顺序。
    let RecognizedText { words, lines, .. } = result;
    let mut source_lines: Vec<_> = lines.into_iter().map(Some).collect();
    let mut source_words: Vec<Vec<_>> = (0..source_lines.len()).map(|_| Vec::new()).collect();
    for word in words {
        checkpoint()?;
        source_words[word.line].push(word);
    }
    let mut deduplicated = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::with_capacity(groups.len()),
    };
    for (new_index, group) in groups.into_iter().enumerate() {
        checkpoint()?;
        let line = source_lines[group.winner]
            .take()
            .expect("重复行分组赢家必须唯一且有效");
        if new_index != 0 {
            deduplicated.text.push('\n');
        }
        deduplicated.text.push_str(&line.text);
        deduplicated.lines.push(line);
        for mut word in std::mem::take(&mut source_words[group.winner]) {
            checkpoint()?;
            word.line = new_index;
            deduplicated.regions.push(word.rect);
            deduplicated.words.push(word);
        }
    }
    checkpoint()?;
    Ok(deduplicated)
}

fn validate_contract(
    result: &RecognizedText,
    image: Rect,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Vec<Vec<usize>>>> {
    checkpoint()?;
    if image.width == 0
        || image.height == 0
        || image.x.checked_add(image.width).is_none()
        || image.y.checked_add(image.height).is_none()
        || result.words.len() != result.regions.len()
    {
        return Ok(None);
    }
    // 与 lines.join("\n") 逐字节等价，避免再分配整份原文。
    let mut position = 0;
    for (index, line) in result.lines.iter().enumerate() {
        checkpoint()?;
        if line.text.trim().is_empty() {
            return Ok(None);
        }
        if index != 0 {
            if result.text.as_bytes().get(position) != Some(&b'\n') {
                return Ok(None);
            }
            position += 1;
        }
        let Some(end) = position.checked_add(line.text.len()) else {
            return Ok(None);
        };
        if result.text.as_bytes().get(position..end) != Some(line.text.as_bytes()) {
            return Ok(None);
        }
        position = end;
    }
    if position != result.text.len() {
        return Ok(None);
    }
    let mut indices: Vec<Vec<usize>> = (0..result.lines.len()).map(|_| Vec::new()).collect();
    let mut unions = vec![None; result.lines.len()];
    for (index, (word, region)) in result.words.iter().zip(&result.regions).enumerate() {
        checkpoint()?;
        if word.line >= result.lines.len()
            || word.text.trim().is_empty()
            || word.rect != *region
            || !rect_within(word.rect, image)
            || word.horizontal_direction != result.lines[word.line].horizontal_direction
        {
            return Ok(None);
        }
        indices[word.line].push(index);
        unions[word.line] = Some(match unions[word.line] {
            Some(rect) => union(rect, word.rect),
            None => word.rect,
        });
    }
    for (index, line) in result.lines.iter().enumerate() {
        checkpoint()?;
        if line.rect.is_none_or(|rect| !rect_within(rect, image)) || line.rect != unions[index] {
            return Ok(None);
        }
    }
    Ok(Some(indices))
}

fn prepare_geometry(
    result: &RecognizedText,
    index: usize,
    word_indices: &[usize],
    image: Rect,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<Option<LineGeometry>> {
    checkpoint()?;
    let source = &result.lines[index];
    let Some(line) = source
        .geometry
        .and_then(|geometry| geometry.validated_in(image))
        .and_then(Band::new)
    else {
        return Ok(None);
    };
    if line.horizontal_direction() != source.horizontal_direction {
        return Ok(None);
    }
    let mut words = Vec::with_capacity(word_indices.len());
    let mut previous_start = 0.;
    let mut previous_end = 0.;
    for &word_index in word_indices {
        checkpoint()?;
        let source_word = &result.words[word_index];
        let Some(word) = source_word
            .geometry
            .and_then(|geometry| geometry.validated_in(source_word.rect))
            .and_then(Band::new)
        else {
            return Ok(None);
        };
        if word.direction != line.direction
            || word.horizontal_direction() != source_word.horizontal_direction
        {
            return Ok(None);
        }
        let Some([start, end]) = word_interval(line.projection, word.projection) else {
            return Ok(None);
        };
        // CTC Voronoi 边界分别 floor／ceil，相邻估算词带可正常重叠；
        // 只核对起点、终点各自单调，不把这种重叠当作坏几何。
        if start + BAND_EPSILON < previous_start || end + BAND_EPSILON < previous_end {
            return Ok(None);
        }
        previous_start = start;
        previous_end = end;
        words.push(word);
    }
    Ok(Some(LineGeometry { line, words }))
}

// 词必须仍是完整 crop 内的正向 CTC 子带；这里只核对映射，不推断墨迹。
fn word_interval(line: Projection, word: Projection) -> Option<[f64; 2]> {
    let mut local = [[0.; 2]; 4];
    for (index, point) in word.quad().into_iter().enumerate() {
        local[index] = line.inverse(point)?;
        let expected_v = if index < 2 { 0. } else { 1. };
        if !(-BAND_EPSILON..=1. + BAND_EPSILON).contains(&local[index][0])
            || (local[index][1] - expected_v).abs() > BAND_EPSILON
        {
            return None;
        }
    }
    if (local[0][0] - local[3][0]).abs() > BAND_EPSILON
        || (local[1][0] - local[2][0]).abs() > BAND_EPSILON
        || local[1][0] <= local[0][0]
        || local[2][0] <= local[3][0]
    {
        return None;
    }
    Some([
        (local[0][0] + local[3][0]) / 2.,
        (local[1][0] + local[2][0]) / 2.,
    ])
}

fn same_physical_line(
    result: &RecognizedText,
    word_indices: &[Vec<usize>],
    geometry: &[Option<LineGeometry>],
    left: usize,
    right: usize,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<bool> {
    let (Some(a), Some(b)) = (&geometry[left], &geometry[right]) else {
        return Ok(false);
    };
    if result.lines[left].text != result.lines[right].text
        || result.lines[left].horizontal_direction != result.lines[right].horizontal_direction
        || a.words.len() != b.words.len()
        || !bands_match(a.line, b.line)
    {
        return Ok(false);
    }
    for (index, (a_word, b_word)) in a.words.iter().zip(&b.words).enumerate() {
        checkpoint()?;
        let a_source = &result.words[word_indices[left][index]];
        let b_source = &result.words[word_indices[right][index]];
        if a_source.text != b_source.text
            || a_source.horizontal_direction != b_source.horizontal_direction
            || !bands_match(*a_word, *b_word)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bands_match(a: Band, b: Band) -> bool {
    let height = a.height.min(b.height);
    if a.direction != b.direction
        || dot(a.axis, b.axis) < MIN_AXIS_COSINE
        || a.height.max(b.height) > height * MAX_HEIGHT_RATIO
    {
        return false;
    }
    let displacement = subtract(a.center, b.center);
    for axis in [a.axis, b.axis] {
        if dot(displacement, axis).abs() > height * MAX_ALONG_HEIGHT
            || dot(displacement, [-axis[1], axis[0]]).abs() > height * MAX_NORMAL_HEIGHT
        {
            return false;
        }
    }
    // 双向检查四角、各边中点与中心，宽行不能靠归一化 u 容差放宽平移。
    const SAMPLES: [[f64; 2]; 9] = [
        [0., 0.],
        [1., 0.],
        [1., 1.],
        [0., 1.],
        [0.5, 0.],
        [1., 0.5],
        [0.5, 1.],
        [0., 0.5],
        [0.5, 0.5],
    ];
    for (from, into) in [(a.projection, b.projection), (b.projection, a.projection)] {
        for [u, v] in SAMPLES {
            let Some(local) = into.inverse(from.point(u, v)) else {
                return false;
            };
            if (local[0] - u).abs() > MAX_LOCAL_U_ERROR || (local[1] - v).abs() > MAX_LOCAL_V_ERROR
            {
                return false;
            }
        }
    }
    true
}

fn coordinates(point: Point) -> [f64; 2] {
    [f64::from(point.x), f64::from(point.y)]
}

fn subtract(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn dot(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

fn rect_within(rect: Rect, image: Rect) -> bool {
    rect.width > 0
        && rect.height > 0
        && rect.x >= image.x
        && rect.y >= image.y
        && rect
            .x
            .checked_add(rect.width)
            .zip(image.x.checked_add(image.width))
            .is_some_and(|(right, image_right)| right <= image_right)
        && rect
            .y
            .checked_add(rect.height)
            .zip(image.y.checked_add(image.height))
            .is_some_and(|(bottom, image_bottom)| bottom <= image_bottom)
}

fn union(a: Rect, b: Rect) -> Rect {
    // 两个 rect 已在同一非溢出原图内，边界加法安全。
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

#[cfg(test)]
#[path = "duplicate_lines_tests.rs"]
mod tests;
