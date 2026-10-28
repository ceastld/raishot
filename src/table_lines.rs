//! 截图长直线的明暗脊线检测；不依赖绝对底色，也不加载结构模型。
use crate::{
    capture::Image,
    table_grid::{TableCell, TableGrid, TableSource, MAX_CELLS, MAX_COLUMNS, MAX_ROWS},
};

const MAX_PIXELS: usize = 32_000_000;
const MAX_SIDE: usize = 16_384;
const MAX_SEGMENTS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Wired,
    Ruled,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSegment {
    pub coordinate: u32,
    pub start: u32,
    pub end: u32,
}

pub struct DetectedLines {
    pub kind: LineKind,
    pub horizontal: Vec<LineSegment>,
    pub vertical: Vec<LineSegment>,
    pub grid: Option<TableGrid>,
}

pub fn detect(image: &Image) -> Option<DetectedLines> {
    let width = image.width as usize;
    let height = image.height as usize;
    let pixels = width.checked_mul(height)?;
    if width < 8
        || height < 8
        || width > MAX_SIDE
        || height > MAX_SIDE
        || pixels > MAX_PIXELS
        || image.bgra.len() != pixels.checked_mul(4)?
    {
        return None;
    }
    let gray: Vec<u8> = image
        .bgra
        .chunks_exact(4)
        .map(|pixel| {
            ((u32::from(pixel[0]) * 29 + u32::from(pixel[1]) * 150 + u32::from(pixel[2]) * 77) >> 8)
                as u8
        })
        .collect();
    let mut histogram = [0usize; 256];
    for value in &gray {
        histogram[*value as usize] += 1;
    }
    let low = percentile(&histogram, pixels / 200);
    let high = percentile(&histogram, pixels - pixels / 200);
    let contrast = high.saturating_sub(low);
    if contrast < 24 {
        return Some(empty());
    }
    let threshold = (contrast as i16 / 8).max(12);
    // 竖线在转置副本上按行扫描，两轴都顺序访问内存并各占一个线程；全屏约快数倍。
    let mut transposed = vec![0u8; pixels];
    for (y, row) in gray.chunks_exact(width).enumerate() {
        for (x, value) in row.iter().enumerate() {
            transposed[x * height + y] = *value;
        }
    }
    let (horizontal, vertical) = std::thread::scope(|scope| {
        let vertical = scope.spawn(|| scan(&transposed, height, width, threshold));
        let horizontal = scan(&gray, width, height, threshold);
        (horizontal, vertical.join().ok().flatten())
    });
    let (horizontal, vertical) = (merge_double(horizontal?), merge_double(vertical?));
    let grid = build_grid(&horizontal, &vertical);
    let kind = if grid.is_some() {
        LineKind::Wired
    } else if axes(&horizontal).len() >= 3 {
        LineKind::Ruled
    } else {
        LineKind::None
    };
    Some(DetectedLines {
        kind,
        horizontal,
        vertical,
        grid,
    })
}

/// 网页表格的单元格间距、双线外框会给出两条相隔几像素、长度基本重合的平行线；
/// 并成一条（取中线），否则每对双线之间多出一条空窄行，有线网格与横线表都失败。
fn merge_double(mut segments: Vec<LineSegment>) -> Vec<LineSegment> {
    const GAP: u32 = 6;
    segments.sort_by_key(|segment| (segment.coordinate, segment.start));
    let mut merged = vec![false; segments.len()];
    let mut result = Vec::with_capacity(segments.len());
    for index in 0..segments.len() {
        if merged[index] {
            continue;
        }
        let mut segment = segments[index];
        for other in index + 1..segments.len() {
            let candidate = segments[other];
            if candidate.coordinate > segment.coordinate + GAP {
                break;
            }
            if merged[other] || candidate.coordinate <= segment.coordinate {
                continue;
            }
            let overlap = segment
                .end
                .min(candidate.end)
                .saturating_sub(segment.start.max(candidate.start));
            let shorter = (segment.end - segment.start).min(candidate.end - candidate.start);
            if overlap * 5 >= shorter * 4 {
                merged[other] = true;
                segment = LineSegment {
                    coordinate: segment.coordinate
                        + (candidate.coordinate - segment.coordinate) / 2,
                    start: segment.start.min(candidate.start),
                    end: segment.end.max(candidate.end),
                };
                break;
            }
        }
        result.push(segment);
    }
    result.sort_by_key(|segment| (segment.coordinate, segment.start));
    result
}

fn empty() -> DetectedLines {
    DetectedLines {
        kind: LineKind::None,
        horizontal: Vec::new(),
        vertical: Vec::new(),
        grid: None,
    }
}

fn percentile(histogram: &[usize; 256], rank: usize) -> u8 {
    let mut count = 0;
    for (value, frequency) in histogram.iter().enumerate() {
        count += frequency;
        if count > rank {
            return value as u8;
        }
    }
    255
}

/// 逐行找水平脊线；`gray` 按行存储，竖线由调用方传入转置副本。
fn scan(gray: &[u8], along: usize, across: usize, threshold: i16) -> Option<Vec<LineSegment>> {
    let minimum_length = (along / 8).max(24);
    let at = |a: usize, b: usize| -> i16 { gray[b * along + a] as i16 };
    let row = |index: usize| &gray[index * along..(index + 1) * along];
    let mut raw = Vec::new();
    let mut ridge = vec![false; along];
    for coordinate in 1..across.saturating_sub(1) {
        // 先按半径整行求掩码（编译器可向量化），再在掩码上找连续段。
        ridge.fill(false);
        let center = row(coordinate);
        for radius in [1usize, 3, 6, 12] {
            if coordinate < radius || coordinate + radius >= across {
                continue;
            }
            let before = row(coordinate - radius);
            let after = row(coordinate + radius);
            for (((hit, &c), &a), &b) in ridge.iter_mut().zip(center).zip(before).zip(after) {
                let a = a as i16 - c as i16;
                let b = b as i16 - c as i16;
                *hit |= (a >= threshold && b >= threshold) || (a <= -threshold && b <= -threshold);
            }
        }
        let mut begin = None;
        let mut last = 0;
        let mut hits = 0usize;
        for position in 0..=along {
            if position < along && ridge[position] {
                begin.get_or_insert(position);
                last = position;
                hits += 1;
            } else if begin.is_some() && (position == along || position > last + 3) {
                let start = begin.take().unwrap();
                let length = last + 1 - start;
                if length >= minimum_length && hits * 100 >= length * 65 {
                    raw.push(LineSegment {
                        coordinate: coordinate as u32,
                        start: start as u32,
                        end: (last + 1) as u32,
                    });
                    if raw.len() > MAX_SEGMENTS * 12 {
                        return None;
                    }
                }
                hits = 0;
            }
        }
    }
    // 同一粗线的脊线在相邻像素重复；统一取中心，沿线方向只合并真正相交的段。
    let mut groups: Vec<Vec<LineSegment>> = Vec::new();
    for segment in raw {
        if groups
            .last()
            .is_some_and(|group| segment.coordinate <= group.last().unwrap().coordinate + 2)
        {
            groups.last_mut().unwrap().push(segment);
        } else {
            groups.push(vec![segment]);
        }
    }
    let mut result = Vec::new();
    for mut group in groups {
        let first = group.first()?.coordinate;
        let last = group.last()?.coordinate;
        // 大块色带不是细网格线。
        if last - first > 12 {
            continue;
        }
        let coordinate = first + (last - first) / 2;
        group.sort_by_key(|segment| segment.start);
        let mut merged: Vec<LineSegment> = Vec::new();
        for segment in group {
            if let Some(previous) = merged
                .last_mut()
                .filter(|previous| segment.start <= previous.end + 3)
            {
                previous.end = previous.end.max(segment.end);
            } else {
                merged.push(LineSegment {
                    coordinate,
                    ..segment
                });
            }
        }
        // 可用采样半径在图片边缘不对称；候选像素的中点不一定是粗线中心。
        // 在多个非交点位置还原连续色带，取中心中位数，避免固定向左/上取整。
        let mut centers = Vec::new();
        for segment in &merged {
            for part in [1u32, 3, 5, 7] {
                let position = (segment.start + (segment.end - segment.start) * part / 8) as usize;
                let seed = coordinate as usize;
                let center = at(position, seed);
                let mut low = seed;
                let mut high = seed;
                let tolerance = (threshold / 2).max(3);
                while low > 0
                    && seed - low < 16
                    && (at(position, low - 1) - center).abs() <= tolerance
                {
                    low -= 1;
                }
                while high + 1 < across
                    && high - seed < 16
                    && (at(position, high + 1) - center).abs() <= tolerance
                {
                    high += 1;
                }
                if high - low <= 12 {
                    centers.push((low + (high - low) / 2) as u32);
                }
            }
        }
        centers.sort_unstable();
        if let Some(center) = centers.get(centers.len() / 2) {
            for segment in &mut merged {
                segment.coordinate = *center;
            }
        }
        result.extend(merged);
        if result.len() > MAX_SEGMENTS {
            return None;
        }
    }
    Some(result)
}

fn axes(lines: &[LineSegment]) -> Vec<u32> {
    let mut values: Vec<_> = lines.iter().map(|line| line.coordinate).collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn coverage(lines: &[LineSegment], coordinate: u32, start: u32, end: u32) -> f32 {
    if end <= start {
        return 0.;
    }
    let mut covered = 0u32;
    let mut previous_end = start;
    for line in lines.iter().filter(|line| line.coordinate == coordinate) {
        let from = line.start.max(start).max(previous_end);
        let to = line.end.min(end);
        if to > from {
            covered += to - from;
            previous_end = to;
        }
    }
    covered as f32 / (end - start) as f32
}

fn build_grid(horizontal: &[LineSegment], vertical: &[LineSegment]) -> Option<TableGrid> {
    // 长表头字形亦可能形成水平脊线；只有两端连接正交线的线段才能建立网格轴。
    // 交点处的同色粗线会让脊线检测短暂断开，因此留出有界的粗线容差。
    let horizontal = connected_lines(horizontal, vertical);
    let vertical = connected_lines(vertical, &horizontal);
    let horizontal = connected_lines(&horizontal, &vertical);
    let horizontal = horizontal.as_slice();
    let vertical = vertical.as_slice();
    let row_edges = axes(horizontal);
    let column_edges = axes(vertical);
    let rows = row_edges.len().checked_sub(1)?;
    let columns = column_edges.len().checked_sub(1)?;
    if rows < 2
        || rows > MAX_ROWS
        || columns < 2
        || columns > MAX_COLUMNS
        || rows * columns > MAX_CELLS
    {
        return None;
    }
    let left = column_edges[0];
    let right = column_edges[columns];
    let top = row_edges[0];
    let bottom = row_edges[rows];
    let outer = [
        coverage(horizontal, top, left, right),
        coverage(horizontal, bottom, left, right),
        coverage(vertical, left, top, bottom),
        coverage(vertical, right, top, bottom),
    ];
    if outer.iter().any(|coverage| *coverage < 0.65)
        || row_edges.windows(2).any(|pair| pair[1] - pair[0] < 4)
        || column_edges.windows(2).any(|pair| pair[1] - pair[0] < 4)
    {
        return None;
    }
    let mut parent: Vec<_> = (0..rows * columns).collect();
    for row in 0..rows {
        for column in 0..columns {
            if column > 0
                && coverage(
                    vertical,
                    column_edges[column],
                    row_edges[row] + 1,
                    row_edges[row + 1],
                ) < 0.65
            {
                union(
                    &mut parent,
                    row * columns + column - 1,
                    row * columns + column,
                );
            }
            if row > 0
                && coverage(
                    horizontal,
                    row_edges[row],
                    column_edges[column] + 1,
                    column_edges[column + 1],
                ) < 0.65
            {
                union(
                    &mut parent,
                    (row - 1) * columns + column,
                    row * columns + column,
                );
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); rows * columns];
    for index in 0..parent.len() {
        let root = root(&mut parent, index);
        groups[root].push(index);
    }
    let mut grid = TableGrid {
        row_edges,
        column_edges,
        cells: Vec::new(),
        confidence: outer.into_iter().fold(1., f32::min),
        source: TableSource::Lines,
    };
    for group in groups.into_iter().filter(|group| !group.is_empty()) {
        let row = group.iter().map(|index| index / columns).min()?;
        let column = group.iter().map(|index| index % columns).min()?;
        let rowspan = group.iter().map(|index| index / columns).max()? - row + 1;
        let colspan = group.iter().map(|index| index % columns).max()? - column + 1;
        // 缺边形成 L 形区域时不能冒充矩形合并格。
        if group.len() != rowspan * colspan {
            return None;
        }
        grid.cells.push(TableCell {
            row,
            column,
            rowspan,
            colspan,
            text: String::new(),
            rect: grid.cell_rect(row, column, rowspan, colspan)?,
            confidence: grid.confidence,
        });
    }
    grid.cells.sort_by_key(|cell| (cell.row, cell.column));
    grid.validate().then_some(grid)
}

fn connected_lines(lines: &[LineSegment], orthogonal: &[LineSegment]) -> Vec<LineSegment> {
    const TOLERANCE: u32 = 12;
    lines
        .iter()
        .copied()
        .filter(|line| {
            let near = |other: &&LineSegment, endpoint: u32| {
                other.coordinate.abs_diff(endpoint) <= TOLERANCE
                    && other.start <= line.coordinate + TOLERANCE
                    && other.end + TOLERANCE >= line.coordinate
            };
            let starts: Vec<_> = orthogonal
                .iter()
                .filter(|other| near(other, line.start))
                .collect();
            orthogonal
                .iter()
                .filter(|other| near(other, line.end.saturating_sub(1)))
                .any(|end| {
                    starts
                        .iter()
                        .any(|start| start.coordinate != end.coordinate)
                })
        })
        .collect()
}

fn root(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let a = root(parent, a);
    let b = root(parent, b);
    parent[b] = a;
}

#[cfg(test)]
#[path = "table_lines_tests.rs"]
mod tests;
