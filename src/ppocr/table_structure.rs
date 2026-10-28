//! SLANet_plus 的有界输入和结构解码；文字由现有 OCR 分配到返回的空网格。
use super::Tensor;
use crate::{
    capture::Image,
    table_grid::{
        TableCell, TableGrid, TableSource, MAX_CELLS, MAX_COLUMNS, MAX_COORDINATE, MAX_ROWS,
    },
    Result,
};
use std::collections::HashSet;

pub(super) const INPUT_SIZE: usize = 488;
const MAX_SEQUENCE: usize = 16_384;
const MAX_VOCABULARY: usize = 4096;
const MAX_OUTPUT_VALUES: usize = 16_000_000;
const MAX_DESCRIPTOR: usize = 1024;

/// 模型 metadata 的原始字典不含起止标记；与导出模型一致地合并空单元格 token。
pub(super) fn read_characters(raw: &str) -> Result<Vec<String>> {
    if raw.is_empty() || raw.len() > 1_048_576 || raw.contains('\0') {
        return Err("表格模型字符表缺失或超过预算".into());
    }
    let mut characters: Vec<String> = raw
        .split(['\r', '\n'])
        .filter(|token| !token.is_empty() && *token != "<td>")
        .map(str::to_owned)
        .collect();
    if !characters.iter().any(|token| token == "<td></td>") {
        characters.push("<td></td>".into());
    }
    if characters.len() > MAX_VOCABULARY - 2
        || characters
            .iter()
            .any(|token| token.len() > MAX_DESCRIPTOR || token == "sos" || token == "eos")
        || characters.iter().collect::<HashSet<_>>().len() != characters.len()
        || !characters.iter().any(|token| token == "<tr>")
        || !characters.iter().any(|token| token == "</tr>")
    {
        return Err("表格模型字符表无效".into());
    }
    characters.insert(0, "sos".into());
    characters.push("eos".into());
    Ok(characters)
}

/// 等比缩到 488，BGR / ImageNet 归一化，再在右边和下边填归一化后的零。
pub(super) fn preprocess(image: &Image) -> Result<Tensor> {
    let width = image.width as usize;
    let height = image.height as usize;
    let count = width.checked_mul(height);
    if width == 0
        || height == 0
        || image.width > MAX_COORDINATE
        || image.height > MAX_COORDINATE
        || count.is_none_or(|count| count > 32_000_000)
        || count.and_then(|count| count.checked_mul(4)) != Some(image.bgra.len())
    {
        return Err("表格识别图像尺寸或 BGRA 长度无效".into());
    }
    let longest = width.max(height);
    let resized_width = (width * INPUT_SIZE / longest).max(1);
    let resized_height = (height * INPUT_SIZE / longest).max(1);
    let plane = INPUT_SIZE * INPUT_SIZE;
    let mut values = vec![0.; 3 * plane];
    let mean = [0.485, 0.456, 0.406];
    let std = [0.229, 0.224, 0.225];
    for y in 0..resized_height {
        let source_y = ((y as f64 + 0.5) * height as f64 / resized_height as f64 - 0.5)
            .clamp(0., (height - 1) as f64);
        let y0 = source_y.floor() as usize;
        let y1 = (y0 + 1).min(height - 1);
        let dy = (source_y - y0 as f64) as f32;
        for x in 0..resized_width {
            let source_x = ((x as f64 + 0.5) * width as f64 / resized_width as f64 - 0.5)
                .clamp(0., (width - 1) as f64);
            let x0 = source_x.floor() as usize;
            let x1 = (x0 + 1).min(width - 1);
            let dx = (source_x - x0 as f64) as f32;
            let pixel = |x, y, channel| {
                let offset = (y * width + x) * 4;
                let alpha = f32::from(image.bgra[offset + 3]) / 255.;
                f32::from(image.bgra[offset + channel]) * alpha + 255. * (1. - alpha)
            };
            for channel in 0..3 {
                let top = pixel(x0, y0, channel) * (1. - dx) + pixel(x1, y0, channel) * dx;
                let bottom = pixel(x0, y1, channel) * (1. - dx) + pixel(x1, y1, channel) * dx;
                let value = top * (1. - dy) + bottom * dy;
                values[channel * plane + y * INPUT_SIZE + x] =
                    (value / 255. - mean[channel]) / std[channel];
            }
        }
    }
    Ok(Tensor {
        shape: vec![1, 3, INPUT_SIZE, INPUT_SIZE],
        values,
    })
}

struct Token<'a> {
    value: &'a str,
    score: f32,
    sequence: usize,
}

struct Cell {
    row: usize,
    column: usize,
    rowspan: usize,
    colspan: usize,
    bounds: [f32; 4],
    confidence: f32,
}

/// probabilities 为 [1,T,V]，V 必须等于含 sos/eos 的字符表长度；boxes 为 [1,T,8]。
/// 没有 EOS 的输出直接回退，不将被模型 token 上限裁掉的尾部当作完整表格。
pub(super) fn decode(
    probabilities: &Tensor,
    boxes: &Tensor,
    characters: &[String],
    width: u32,
    height: u32,
) -> Result<TableGrid> {
    if width == 0 || height == 0 || width > MAX_COORDINATE || height > MAX_COORDINATE {
        return Err("表格结构图像尺寸无效".into());
    }
    validate_tensor(probabilities)?;
    validate_tensor(boxes)?;
    if probabilities.shape.len() != 3
        || boxes.shape.len() != 3
        || probabilities.shape[0] != 1
        || boxes.shape[0] != 1
        || boxes.shape[2] != 8
        || probabilities.shape[1] != boxes.shape[1]
        || probabilities.shape[1] > MAX_SEQUENCE
        || characters.len() < 3
        || characters.len() > MAX_VOCABULARY
        || probabilities.shape[2] != characters.len()
        || characters.first().map(String::as_str) != Some("sos")
        || characters.last().map(String::as_str) != Some("eos")
        || characters
            .iter()
            .any(|token| token.len() > MAX_DESCRIPTOR || token.contains('\0'))
    {
        return Err("表格模型输出形状与字符表不匹配".into());
    }
    let vocabulary = characters.len();
    let mut tokens = Vec::new();
    let mut complete = false;
    for (sequence, scores) in probabilities.values.chunks_exact(vocabulary).enumerate() {
        if scores.iter().any(|score| !(0.0..=1.0).contains(score)) {
            return Err("表格模型输出不是有效概率".into());
        }
        let (index, score) = scores
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1).then_with(|| right.0.cmp(&left.0)))
            .unwrap();
        if index == vocabulary - 1 {
            complete = true;
            break;
        }
        if index != 0 {
            tokens.push(Token {
                value: &characters[index],
                score: *score,
                sequence,
            });
        }
    }
    if !complete {
        return Err("表格结构输出被截断，已回退到几何识别".into());
    }
    parse(&tokens, boxes, width, height)
}

fn validate_tensor(tensor: &Tensor) -> Result<()> {
    let count = tensor
        .shape
        .iter()
        .try_fold(1usize, |count, side| count.checked_mul(*side));
    if count.is_none_or(|count| count == 0 || count > MAX_OUTPUT_VALUES)
        || count != Some(tensor.values.len())
        || tensor.values.iter().any(|value| !value.is_finite())
    {
        return Err("表格模型输出尺寸或数值无效".into());
    }
    Ok(())
}

fn parse(tokens: &[Token<'_>], boxes: &Tensor, width: u32, height: u32) -> Result<TableGrid> {
    let mut cells = Vec::new();
    let mut occupied = vec![false; MAX_ROWS * MAX_COLUMNS];
    let mut wrappers = Vec::new();
    let (mut row, mut column) = (0usize, 0usize);
    let (mut row_open, mut cell_open) = (false, false);
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.value {
            "<tr>" if !row_open && !cell_open && row < MAX_ROWS => {
                row_open = true;
                column = 0;
            }
            "</tr>" if row_open && !cell_open => {
                row_open = false;
                row += 1;
            }
            "</td>" if cell_open => cell_open = false,
            "<html>" | "<body>" | "<table>" | "<thead>" | "<tbody>" | "<tfoot>"
                if !row_open && !cell_open =>
            {
                let name = &token.value[1..token.value.len() - 1];
                if wrappers.len() >= 4 || wrappers.contains(&name) {
                    return Err("表格结构包含嵌套或重复容器".into());
                }
                wrappers.push(name);
            }
            "</html>" | "</body>" | "</table>" | "</thead>" | "</tbody>" | "</tfoot>"
                if !row_open && !cell_open =>
            {
                if wrappers.pop() != Some(&token.value[2..token.value.len() - 1]) {
                    return Err("表格结构容器没有正确闭合".into());
                }
            }
            value if cell_start(value) && row_open && !cell_open => {
                let mut descriptor = value.to_owned();
                while !descriptor.contains('>') {
                    index += 1;
                    let next = tokens.get(index).ok_or("表格单元格描述未闭合")?;
                    if next.value.contains('<')
                        || descriptor.len() + next.value.len() > MAX_DESCRIPTOR
                    {
                        return Err("表格单元格描述无效".into());
                    }
                    descriptor.push_str(next.value);
                }
                let (rowspan, colspan, closed) = descriptor_spans(&descriptor)?;
                while column < MAX_COLUMNS && occupied[row * MAX_COLUMNS + column] {
                    column += 1;
                }
                let row_end = row.checked_add(rowspan).ok_or("表格行跨度溢出")?;
                let column_end = column.checked_add(colspan).ok_or("表格列跨度溢出")?;
                if row_end > MAX_ROWS || column_end > MAX_COLUMNS || cells.len() >= MAX_CELLS {
                    return Err("表格行列或单元格数量超过预算".into());
                }
                for r in row..row_end {
                    for c in column..column_end {
                        if std::mem::replace(&mut occupied[r * MAX_COLUMNS + c], true) {
                            return Err("表格合并单元格互相重叠".into());
                        }
                    }
                }
                cells.push(Cell {
                    row,
                    column,
                    rowspan,
                    colspan,
                    bounds: decode_box(boxes, token.sequence, width, height)?,
                    confidence: token.score,
                });
                column = column_end;
                cell_open = !closed;
            }
            _ => return Err("表格结构 token 顺序无效".into()),
        }
        index += 1;
    }
    if row_open || cell_open || !wrappers.is_empty() || cells.is_empty() || row == 0 {
        return Err("表格结构未完整闭合".into());
    }
    let columns = cells
        .iter()
        .map(|cell| cell.column + cell.colspan)
        .max()
        .unwrap();
    if cells.iter().any(|cell| cell.row + cell.rowspan > row)
        || (0..row).any(|r| (0..columns).any(|c| !occupied[r * MAX_COLUMNS + c]))
    {
        return Err("表格结构存在缺格或越界合并格".into());
    }
    let mut grid = TableGrid {
        row_edges: resolve_edges(&cells, row, false)?,
        column_edges: resolve_edges(&cells, columns, true)?,
        confidence: cells.iter().map(|cell| cell.confidence).fold(1., f32::min),
        cells: Vec::with_capacity(cells.len()),
        source: TableSource::Model,
    };
    for cell in cells {
        grid.cells.push(TableCell {
            row: cell.row,
            column: cell.column,
            rowspan: cell.rowspan,
            colspan: cell.colspan,
            text: String::new(),
            rect: grid
                .cell_rect(cell.row, cell.column, cell.rowspan, cell.colspan)
                .ok_or("表格单元格边界无效")?,
            confidence: cell.confidence,
        });
    }
    if !grid.validate() {
        return Err("表格模型候选未通过网格校验".into());
    }
    Ok(grid)
}

fn cell_start(value: &str) -> bool {
    value.strip_prefix("<td").is_some_and(|rest| {
        rest.is_empty() || rest.starts_with('>') || rest.starts_with(char::is_whitespace)
    })
}

fn descriptor_spans(descriptor: &str) -> Result<(usize, usize, bool)> {
    let end = descriptor.find('>').ok_or("表格单元格描述未闭合")?;
    let tail = &descriptor[end + 1..];
    if !tail.is_empty() && tail != "</td>" {
        return Err("表格单元格描述包含额外内容".into());
    }
    let mut attrs = descriptor[3..end].trim();
    let (mut rowspan, mut colspan) = (None, None);
    while !attrs.is_empty() {
        let key_end = attrs
            .find(|character: char| !character.is_ascii_alphabetic())
            .unwrap_or(attrs.len());
        let key = &attrs[..key_end];
        attrs = attrs[key_end..].trim_start();
        attrs = attrs
            .strip_prefix('=')
            .ok_or("表格跨度属性无效")?
            .trim_start();
        let quote = attrs
            .chars()
            .next()
            .filter(|value| matches!(value, '\'' | '"'));
        if quote.is_some() {
            attrs = &attrs[1..];
        }
        let number_end = attrs
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(attrs.len());
        let value: usize = attrs[..number_end]
            .parse()
            .map_err(|_| "表格跨度不是整数")?;
        attrs = &attrs[number_end..];
        if let Some(quote) = quote {
            attrs = attrs.strip_prefix(quote).ok_or("表格跨度引号未闭合")?;
        }
        if !attrs.is_empty() && !attrs.starts_with(char::is_whitespace) {
            return Err("表格跨度属性没有分隔".into());
        }
        attrs = attrs.trim_start();
        let (destination, limit) = match key {
            "rowspan" => (&mut rowspan, MAX_ROWS),
            "colspan" => (&mut colspan, MAX_COLUMNS),
            _ => return Err("表格单元格包含未知属性".into()),
        };
        if value == 0 || value > limit || destination.replace(value).is_some() {
            return Err("表格跨度超过预算或重复定义".into());
        }
    }
    Ok((rowspan.unwrap_or(1), colspan.unwrap_or(1), !tail.is_empty()))
}

fn decode_box(boxes: &Tensor, sequence: usize, width: u32, height: u32) -> Result<[f32; 4]> {
    let points = &boxes.values[sequence * 8..sequence * 8 + 8];
    let scale = width.max(height) as f32;
    let mut bounds = [width as f32, height as f32, 0., 0.];
    for point in points.chunks_exact(2) {
        let x = (point[0] * scale).clamp(0., width as f32);
        let y = (point[1] * scale).clamp(0., height as f32);
        bounds[0] = bounds[0].min(x);
        bounds[1] = bounds[1].min(y);
        bounds[2] = bounds[2].max(x);
        bounds[3] = bounds[3].max(y);
    }
    if bounds[2] - bounds[0] < 1. || bounds[3] - bounds[1] < 1. {
        return Err("表格模型单元格缺少有效坐标".into());
    }
    Ok(bounds)
}

fn resolve_edges(cells: &[Cell], count: usize, horizontal: bool) -> Result<Vec<u32>> {
    let mut starts = vec![Vec::new(); count + 1];
    let mut ends = vec![Vec::new(); count + 1];
    for cell in cells {
        let (start, span, low, high) = if horizontal {
            (cell.column, cell.colspan, cell.bounds[0], cell.bounds[2])
        } else {
            (cell.row, cell.rowspan, cell.bounds[1], cell.bounds[3])
        };
        starts[start].push(low);
        ends[start + span].push(high);
    }
    let mut resolved: Vec<Option<f32>> = starts
        .iter_mut()
        .zip(&mut ends)
        .map(|(starts, ends)| match (median(starts), median(ends)) {
            (Some(start), Some(end)) => Some((start + end) * 0.5),
            (start, end) => start.or(end),
        })
        .collect();
    // 完全被跨格遮住的基础边界没有直接框证据，在最近两条已知边界之间插值。
    for index in 0..resolved.len() {
        if resolved[index].is_some() {
            continue;
        }
        let left = (0..index).rev().find(|left| resolved[*left].is_some());
        let right = (index + 1..resolved.len()).find(|right| resolved[*right].is_some());
        let (left, right) = left.zip(right).ok_or("表格外边界缺少坐标")?;
        resolved[index] = Some(
            resolved[left].unwrap()
                + (resolved[right].unwrap() - resolved[left].unwrap()) * (index - left) as f32
                    / (right - left) as f32,
        );
    }
    let edges: Vec<u32> = resolved
        .into_iter()
        .map(|edge| edge.unwrap().round() as u32)
        .collect();
    if edges.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("表格模型行列边界不是递增序列".into());
    }
    Ok(edges)
}

fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f32::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    })
}

#[cfg(test)]
#[path = "table_structure_tests.rs"]
mod tests;
