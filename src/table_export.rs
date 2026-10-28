//! 表格的纯文本与富文本编码；CF_HTML 偏移均为 UTF-8 字节偏移。
use crate::table_grid::TableGrid;

pub fn tsv(grid: &TableGrid) -> Option<String> {
    let cells = anchors(grid)?;
    let mut output = String::new();
    for row in 0..grid.rows() {
        if row > 0 {
            output.push_str("\r\n");
        }
        for column in 0..grid.columns() {
            if column > 0 {
                output.push('\t');
            }
            if let Some(index) = cells[row * grid.columns() + column] {
                encode_tsv(&grid.cells[index].text, &mut output);
            }
        }
    }
    Some(output)
}

pub fn html(grid: &TableGrid) -> Option<String> {
    let cells = anchors(grid)?;
    let mut output = String::from("<table><tbody>");
    for row in 0..grid.rows() {
        output.push_str("<tr>");
        for column in 0..grid.columns() {
            let Some(index) = cells[row * grid.columns() + column] else {
                continue;
            };
            let cell = &grid.cells[index];
            output.push_str("<td");
            if cell.rowspan > 1 {
                output.push_str(&format!(" rowspan=\"{}\"", cell.rowspan));
            }
            if cell.colspan > 1 {
                output.push_str(&format!(" colspan=\"{}\"", cell.colspan));
            }
            // 使用文本格式保护前导零与超出电子表格精度的数字。
            output.push_str(" style=\"mso-number-format:'\\@'\">");
            let normalized = cell.text.replace("\r\n", "\n").replace('\r', "\n");
            for (line, text) in normalized.split('\n').enumerate() {
                if line > 0 {
                    output.push_str("<br>");
                }
                escape_html(text, &mut output);
            }
            output.push_str("</td>");
        }
        output.push_str("</tr>");
    }
    output.push_str("</tbody></table>");
    Some(output)
}

pub fn cf_html(grid: &TableGrid) -> Option<String> {
    let fragment = html(grid)?;
    let prefix = "<html><head><meta charset=\"utf-8\"></head><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let placeholder = header(0, 0, 0, 0);
    let start_html = placeholder.len();
    let start_fragment = start_html.checked_add(prefix.len())?;
    let end_fragment = start_fragment.checked_add(fragment.len())?;
    let end_html = end_fragment.checked_add(suffix.len())?;
    if end_html > 9_999_999_999usize {
        return None;
    }
    Some(format!(
        "{}{prefix}{fragment}{suffix}",
        header(start_html, end_html, start_fragment, end_fragment)
    ))
}

fn anchors(grid: &TableGrid) -> Option<Vec<Option<usize>>> {
    if !grid.validate() {
        return None;
    }
    let mut anchors = vec![None; grid.rows() * grid.columns()];
    for (index, cell) in grid.cells.iter().enumerate() {
        anchors[cell.row * grid.columns() + cell.column] = Some(index);
    }
    Some(anchors)
}

fn encode_tsv(text: &str, output: &mut String) {
    if text.contains(['"', '\t', '\r', '\n']) {
        output.push('"');
        for character in text.chars() {
            output.push(character);
            if character == '"' {
                output.push('"');
            }
        }
        output.push('"');
    } else {
        output.push_str(text);
    }
}

fn escape_html(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
}

fn header(
    start_html: usize,
    end_html: usize,
    start_fragment: usize,
    end_fragment: usize,
) -> String {
    format!("Version:1.0\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n")
}

#[cfg(test)]
#[path = "table_export_tests.rs"]
mod tests;
