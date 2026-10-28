use super::*;
use crate::table_grid::{TableCell, TableSource};

fn grid() -> TableGrid {
    let mut grid = TableGrid {
        row_edges: vec![0, 20, 40],
        column_edges: vec![0, 30, 60],
        cells: Vec::new(),
        confidence: 1.,
        source: TableSource::Lines,
    };
    for (row, column, rowspan, colspan, text) in [
        (0, 0, 1, 2, "表头<&>"),
        (1, 0, 1, 1, "0012"),
        (1, 1, 1, 1, "a\"b\t中\n文"),
    ] {
        grid.cells.push(TableCell {
            row,
            column,
            rowspan,
            colspan,
            text: text.to_owned(),
            rect: grid.cell_rect(row, column, rowspan, colspan).unwrap(),
            confidence: 1.,
        });
    }
    grid
}

#[test]
fn table_export_tsv_escapes_and_expands_merged_cells() {
    assert_eq!(
        tsv(&grid()).unwrap(),
        "表头<&>\t\r\n0012\t\"a\"\"b\t中\n文\""
    );
}

#[test]
fn table_export_html_preserves_spans_and_text_format() {
    let html = html(&grid()).unwrap();
    assert!(html.contains("colspan=\"2\""));
    assert!(html.contains("表头&lt;&amp;&gt;"));
    assert!(html.contains("mso-number-format:'\\@'"));
    assert!(html.contains("中<br>文"));
    assert_eq!(html.matches("<td").count(), 3);
}

#[test]
fn table_export_cf_html_offsets_count_utf8_bytes() {
    let grid = grid();
    let encoded = cf_html(&grid).unwrap();
    let offset = |key: &str| -> usize {
        encoded
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .parse()
            .unwrap()
    };
    let start = offset("StartHTML:");
    let end = offset("EndHTML:");
    let start_fragment = offset("StartFragment:");
    let end_fragment = offset("EndFragment:");
    assert_eq!(end, encoded.len());
    assert!(encoded[start..end].starts_with("<html>"));
    assert_eq!(&encoded[start_fragment..end_fragment], html(&grid).unwrap());
    assert!(encoded[..start_fragment].ends_with("<!--StartFragment-->"));
    assert!(encoded[end_fragment..].starts_with("<!--EndFragment-->"));
}

#[test]
fn table_export_validation_rejects_holes_overlap_and_overflow() {
    let valid = grid();
    assert!(valid.validate());
    let mut hole = valid.clone();
    hole.cells.pop();
    assert!(!hole.validate());
    let mut overlap = valid.clone();
    overlap.cells.push(overlap.cells[0].clone());
    assert!(!overlap.validate());
    let mut overflow = valid.clone();
    overflow.cells[0].rowspan = usize::MAX;
    assert!(!overflow.validate());
    let mut bounds = valid.clone();
    bounds.column_edges[1] = bounds.column_edges[0];
    assert!(!bounds.validate());
    let mut confidence = valid.clone();
    confidence.confidence = f32::NAN;
    assert!(!confidence.validate());
    let mut rect = valid.clone();
    rect.cells[0].rect.width += 1;
    assert!(!rect.validate());
    assert!(tsv(&hole).is_none());
    assert!(html(&overlap).is_none());
}

#[test]
fn table_export_rowspan_skips_covered_html_cells() {
    let mut grid = grid();
    grid.cells.clear();
    for (row, column, rowspan, colspan) in [(0, 0, 2, 1), (0, 1, 1, 1), (1, 1, 1, 1)] {
        grid.cells.push(TableCell {
            row,
            column,
            rowspan,
            colspan,
            text: "x".to_owned(),
            rect: grid.cell_rect(row, column, rowspan, colspan).unwrap(),
            confidence: 1.,
        });
    }
    assert!(html(&grid).unwrap().contains("rowspan=\"2\""));
    assert_eq!(tsv(&grid).unwrap(), "x\tx\r\n\tx");
}
