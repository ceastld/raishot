//! 与 UI 和 OCR 提供者无关的有界表格结构；所有坐标局部于识别图片。
use crate::geometry::Rect;
use crate::recognition::{table::OcrTableCopy, RecognizedText};

pub const MAX_ROWS: usize = 256;
pub const MAX_COLUMNS: usize = 128;
pub const MAX_CELLS: usize = MAX_ROWS * MAX_COLUMNS;
pub const MAX_TEXT_BYTES: usize = 1_048_576;
pub const MAX_CELL_BYTES: usize = 65_536;
pub const MAX_COORDINATE: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSource {
    Lines,
    Alignment,
    Legacy,
    Model,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableCell {
    pub row: usize,
    pub column: usize,
    pub rowspan: usize,
    pub colspan: usize,
    /// 多行内容保留换行；空格不会用于自动推测数值类型。
    pub text: String,
    pub rect: Rect,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableGrid {
    pub row_edges: Vec<u32>,
    pub column_edges: Vec<u32>,
    pub cells: Vec<TableCell>,
    pub confidence: f32,
    pub source: TableSource,
}

impl TableGrid {
    pub fn rows(&self) -> usize {
        self.row_edges.len().saturating_sub(1)
    }
    pub fn columns(&self) -> usize {
        self.column_edges.len().saturating_sub(1)
    }

    /// 每个基础格恰好覆盖一次；拒绝溢出、缺口、重叠和非矩形跨度。
    pub fn validate(&self) -> bool {
        let rows = self.rows();
        let columns = self.columns();
        if rows == 0
            || rows > MAX_ROWS
            || columns == 0
            || columns > MAX_COLUMNS
            || self.cells.is_empty()
            || self.cells.len() > MAX_CELLS
            || !confidence(self.confidence)
            || !edges(&self.row_edges)
            || !edges(&self.column_edges)
        {
            return false;
        }
        let mut covered = vec![false; rows * columns];
        let mut bytes = 0usize;
        for cell in &self.cells {
            let Some(end_row) = cell.row.checked_add(cell.rowspan) else {
                return false;
            };
            let Some(end_column) = cell.column.checked_add(cell.colspan) else {
                return false;
            };
            if cell.rowspan == 0
                || cell.colspan == 0
                || end_row > rows
                || end_column > columns
                || !confidence(cell.confidence)
                || cell.text.len() > MAX_CELL_BYTES
                || cell.text.contains('\0')
            {
                return false;
            }
            let Some(total) = bytes.checked_add(cell.text.len()) else {
                return false;
            };
            bytes = total;
            if bytes > MAX_TEXT_BYTES
                || cell.rect
                    != self
                        .cell_rect(cell.row, cell.column, cell.rowspan, cell.colspan)
                        .unwrap()
            {
                return false;
            }
            for row in cell.row..end_row {
                for column in cell.column..end_column {
                    if std::mem::replace(&mut covered[row * columns + column], true) {
                        return false;
                    }
                }
            }
        }
        covered.into_iter().all(|value| value)
    }

    pub fn cell_rect(
        &self,
        row: usize,
        column: usize,
        rowspan: usize,
        colspan: usize,
    ) -> Option<Rect> {
        let x = *self.column_edges.get(column)?;
        let y = *self.row_edges.get(row)?;
        let right = *self.column_edges.get(column.checked_add(colspan)?)?;
        let bottom = *self.row_edges.get(row.checked_add(rowspan)?)?;
        Some(Rect {
            x,
            y,
            width: right.checked_sub(x)?,
            height: bottom.checked_sub(y)?,
        })
    }

    /// 将旧的可信矩阵接到同一输出通路；来源原文和旧模型均保持只读。
    pub fn from_legacy(source: &RecognizedText, table: &OcrTableCopy) -> Option<Self> {
        let rows = table.rows.len();
        let columns = table.rows.first()?.len();
        if rows == 0
            || rows > MAX_ROWS
            || columns == 0
            || columns > MAX_COLUMNS
            || table.rows.iter().any(|row| row.len() != columns)
        {
            return None;
        }
        let mut ordered: Vec<_> = table.rows.iter().collect();
        ordered.sort_by_key(|row| row.iter().map(|cell| cell.rect.y).min().unwrap_or(0));
        let mut rects = Vec::with_capacity(rows * columns);
        let mut texts = Vec::with_capacity(rows * columns);
        for row in ordered {
            let mut cells: Vec<_> = row.iter().collect();
            cells.sort_by_key(|cell| cell.rect.x);
            for cell in cells {
                rects.push(cell.rect);
                texts.push(source.text.get(cell.source_range.clone())?.to_owned());
            }
        }
        let column_edges = extent_edges(columns, |column| {
            let left = (0..rows).map(|row| rects[row * columns + column].x).min()?;
            let right = (0..rows)
                .map(|row| {
                    let rect = rects[row * columns + column];
                    rect.x.checked_add(rect.width)
                })
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .max()?;
            Some((left, right))
        })?;
        let row_edges = extent_edges(rows, |row| {
            let cells = &rects[row * columns..(row + 1) * columns];
            let top = cells.iter().map(|rect| rect.y).min()?;
            let bottom = cells
                .iter()
                .map(|rect| rect.y.checked_add(rect.height))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .max()?;
            Some((top, bottom))
        })?;
        let mut grid = Self {
            row_edges,
            column_edges,
            cells: Vec::new(),
            confidence: 0.8,
            source: TableSource::Legacy,
        };
        for (index, text) in texts.into_iter().enumerate() {
            let row = index / columns;
            let column = index % columns;
            grid.cells.push(TableCell {
                row,
                column,
                rowspan: 1,
                colspan: 1,
                text,
                rect: grid.cell_rect(row, column, 1, 1)?,
                confidence: 0.8,
            });
        }
        grid.validate().then_some(grid)
    }
}

pub(crate) fn extent_edges(
    count: usize,
    mut extent: impl FnMut(usize) -> Option<(u32, u32)>,
) -> Option<Vec<u32>> {
    let extents: Vec<_> = (0..count).map(&mut extent).collect::<Option<_>>()?;
    let mut result = vec![extents.first()?.0];
    for pair in extents.windows(2) {
        if pair[0].1 > pair[1].0 {
            return None;
        }
        result.push(pair[0].1 + (pair[1].0 - pair[0].1) / 2);
    }
    result.push(extents.last()?.1);
    edges(&result).then_some(result)
}

fn edges(values: &[u32]) -> bool {
    values.len() >= 2
        && values.last().is_some_and(|value| *value <= MAX_COORDINATE)
        && values.windows(2).all(|pair| pair[0] < pair[1])
}

fn confidence(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}
