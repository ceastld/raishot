//! 翻译只修改文字；代码、数字与表格结构由本机持有并在返回后核验。
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

fn invalid() -> String {
    tr!(
        "译文未完整保留代码、数字或表格结构，请重试；原文已保留",
        "Translation changed protected content or table structure; retry. The source is preserved"
    )
    .into()
}

#[derive(Clone)]
struct ProtectedText {
    original: String,
    prefix: String,
    masked: String,
    values: Vec<(String, String)>,
    has_work: bool,
}
impl ProtectedText {
    fn new(source: &str) -> Self {
        let mut prefix = "__RS_KEEP_".to_owned();
        while source
            .to_ascii_lowercase()
            .contains(&prefix.to_ascii_lowercase())
        {
            prefix.push('_');
        }
        let mut masked = String::new();
        let mut values = Vec::new();
        let mut has_work = false;
        let body = source.trim();
        let mut offset = 0;
        while offset < body.len() {
            let rest = &body[offset..];
            if let Some(length) = protected_length(rest) {
                let token = format!("{prefix}{}__", values.len());
                masked.push_str(&token);
                values.push((token, rest[..length].to_owned()));
                offset += length;
            } else {
                let character = rest.chars().next().unwrap();
                has_work |= character.is_alphabetic();
                masked.push(character);
                offset += character.len_utf8();
            }
        }
        Self {
            original: source.into(),
            prefix,
            masked,
            values,
            has_work,
        }
    }

    fn preview(&self, raw: &str) -> String {
        let mut result = String::new();
        let mut remaining = raw;
        while let Some(start) = remaining
            .to_ascii_lowercase()
            .find(&self.prefix.to_ascii_lowercase())
        {
            result.push_str(&remaining[..start]);
            remaining = &remaining[start..];
            let Some((token, value)) = self
                .values
                .iter()
                .find(|(token, _)| remaining.starts_with(token))
            else {
                return result;
            };
            result.push_str(value);
            remaining = &remaining[token.len()..];
        }
        // A marker may be split at any byte boundary in the stream. Hold its unfinished prefix.
        let held = (1..=self.prefix.len().min(remaining.len()))
            .rev()
            .find(|&length| {
                remaining
                    .to_ascii_lowercase()
                    .ends_with(&self.prefix[..length].to_ascii_lowercase())
            })
            .unwrap_or(0);
        result.push_str(&remaining[..remaining.len() - held]);
        result
    }

    fn finish(&self, raw: &str) -> Result<String, String> {
        if !self.has_work {
            return Ok(self.original.clone());
        }
        let mut residue = raw.trim().to_owned();
        for (token, value) in &self.values {
            let _ = value;
            if residue.matches(token).count() != 1 {
                return Err(invalid());
            }
            residue = residue.replacen(token, "", 1);
        }
        if residue
            .to_ascii_lowercase()
            .contains(&self.prefix.to_ascii_lowercase())
            || residue.chars().any(char::is_numeric)
        {
            return Err(invalid());
        }
        let mut output = raw.trim().to_owned();
        for (token, value) in &self.values {
            output = output.replacen(token, value, 1);
        }
        if output.is_empty() {
            return Err(invalid());
        }
        // Keep source newline style and outer whitespace locally, while rejecting added/lost lines.
        let source_body = self.original.trim();
        let source_breaks = breaks(source_body);
        let output_breaks = breaks(&output);
        if source_breaks.len() != output_breaks.len()
            || source_body.matches('\t').count() != output.matches('\t').count()
        {
            return Err(invalid());
        }
        let mut normalized = String::new();
        let mut position = 0;
        for ((start, end), (source_start, source_end)) in output_breaks.iter().zip(&source_breaks) {
            normalized.push_str(&output[position..*start]);
            normalized.push_str(&source_body[*source_start..*source_end]);
            position = *end;
        }
        normalized.push_str(&output[position..]);
        let leading = self.original.len() - self.original.trim_start().len();
        let trailing = self.original.trim_end().len();
        Ok(format!(
            "{}{}{}",
            &self.original[..leading],
            normalized,
            &self.original[trailing..]
        ))
    }
}

fn breaks(text: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'\r' || bytes[offset] == b'\n' {
            let end = offset
                + if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            result.push((offset, end));
            offset = end;
        } else {
            offset += 1;
        }
    }
    result
}

fn paired(text: &str, open: &str, close: &str) -> Option<usize> {
    text.strip_prefix(open)?
        .find(close)
        .map(|index| open.len() + index + close.len())
}

fn protected_length(text: &str) -> Option<usize> {
    if text.starts_with('`') {
        let count = text.bytes().take_while(|&byte| byte == b'`').count();
        let delimiter = "`".repeat(count);
        return Some(paired(text, &delimiter, &delimiter).unwrap_or(text.len()));
    }
    for (open, close) in [("$$", "$$"), ("\\(", "\\)"), ("\\[", "\\]")] {
        if let Some(length) = paired(text, open, close) {
            return Some(length);
        }
    }
    if text.starts_with('$') && !text.starts_with("${") {
        if let Some(length) = paired(text, "$", "$") {
            let currency_pair = text[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
                && text[length..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit());
            if !currency_pair && !text[..length].contains(['\r', '\n']) {
                return Some(length);
            }
        }
    }
    if text.starts_with("\\begin{") {
        if let Some(end) = text.find('}') {
            let close = format!("\\end{{{}}}", &text[7..end]);
            if let Some(index) = text[end + 1..].find(&close) {
                return Some(end + 1 + index + close.len());
            }
        }
    }
    if text.starts_with('\\') {
        let command = text[1..]
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .count();
        if command > 0 {
            let mut end = command + 1;
            while matches!(text.as_bytes().get(end), Some(b'{') | Some(b'[')) {
                let open = text.as_bytes()[end];
                let close = if open == b'{' { b'}' } else { b']' };
                let mut depth = 0usize;
                let mut found = None;
                for (index, byte) in text.as_bytes()[end..].iter().enumerate() {
                    if *byte == open {
                        depth += 1;
                    }
                    if *byte == close {
                        depth -= 1;
                        if depth == 0 {
                            found = Some(end + index + 1);
                            break;
                        }
                    }
                }
                match found {
                    Some(value) => end = value,
                    None => break,
                }
            }
            return Some(end);
        }
    }
    if ["https://", "http://", "www."]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        let end = text
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '，' | '。' | '；')
            })
            .unwrap_or(text.len());
        let mut url = text[..end].trim_end_matches(['.', ',', '!', ';']);
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            while url.ends_with(close) && url.matches(close).count() > url.matches(open).count() {
                url = &url[..url.len() - close.len_utf8()];
            }
        }
        return (!url.is_empty()).then_some(url.len());
    }
    for (open, close) in [("${", "}"), ("{{", "}}"), ("{", "}")] {
        if let Some(length) = paired(text, open, close) {
            let body = &text[open.len()..length - close.len()];
            if !body.trim().is_empty()
                && body
                    .trim()
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_.:-[]".contains(c))
            {
                return Some(length);
            }
        }
    }
    if text.starts_with('%') {
        let mut end = 1;
        for character in text[1..].chars() {
            if "sdifugxX".contains(character) {
                return Some(end + character.len_utf8());
            }
            if character.is_ascii_digit() || "$.-+".contains(character) {
                end += character.len_utf8();
            } else {
                break;
            }
        }
    }
    let first = text.chars().next()?;
    let numeric_start = first.is_numeric()
        || ("+-−￥¥$€£".contains(first)
            && text[first.len_utf8()..]
                .chars()
                .next()
                .is_some_and(char::is_numeric));
    if numeric_start {
        let mut end = first.len_utf8();
        for (index, character) in text[end..].char_indices() {
            let absolute = first.len_utf8() + index;
            let exponent = matches!(character, 'e' | 'E')
                && text[absolute + 1..]
                    .trim_start_matches(['+', '-'])
                    .chars()
                    .next()
                    .is_some_and(char::is_numeric);
            if character.is_numeric() || ".,/%+-−".contains(character) || exponent {
                end = first.len_utf8() + index + character.len_utf8();
            } else {
                break;
            }
        }
        return Some(end);
    }
    None
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellReply {
    cells: Vec<CellText>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellText {
    id: String,
    text: String,
}

/// Cell identities and source content are local. The provider cannot rebuild a grid.
pub(crate) struct CellBatch {
    cells: Vec<(String, ProtectedText)>,
}
impl CellBatch {
    pub(crate) fn new(cells: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            cells: cells
                .into_iter()
                .map(|(id, text)| (id, ProtectedText::new(&text)))
                .collect(),
        }
    }
    pub(crate) fn has_work(&self) -> bool {
        self.cells.iter().any(|(_, text)| text.has_work)
    }
    pub(crate) fn model_text(&self) -> String {
        serde_json::json!({"cells": self.cells.iter().filter(|(_, text)| text.has_work)
            .map(|(id,text)| serde_json::json!({"id":id,"text":text.masked})).collect::<Vec<_>>()})
        .to_string()
    }
    pub(crate) fn finish(&self, raw: &str) -> Result<Vec<(String, String)>, String> {
        if !self.has_work() {
            return Ok(self
                .cells
                .iter()
                .map(|(id, text)| (id.clone(), text.original.clone()))
                .collect());
        }
        let reply: CellReply = serde_json::from_str(raw.trim()).map_err(|_| invalid())?;
        let requested: HashSet<&str> = self
            .cells
            .iter()
            .filter(|(_, text)| text.has_work)
            .map(|(id, _)| id.as_str())
            .collect();
        if requested.len() != self.cells.iter().filter(|(_, text)| text.has_work).count()
            || reply.cells.len() != requested.len()
        {
            return Err(invalid());
        }
        let mut returned = HashMap::new();
        for cell in reply.cells {
            if !requested.contains(cell.id.as_str())
                || returned.insert(cell.id, cell.text).is_some()
            {
                return Err(invalid());
            }
        }
        self.cells
            .iter()
            .map(|(id, text)| {
                let translated = if text.has_work {
                    text.finish(returned.get(id).ok_or_else(invalid)?)?
                } else {
                    text.original.clone()
                };
                Ok((id.clone(), translated))
            })
            .collect()
    }
}

enum Piece {
    Literal(String),
    Cell { id: String, quoted: bool },
}
enum Shape {
    Invalid,
    Plain(ProtectedText),
    Tsv {
        pieces: Vec<Piece>,
        cells: CellBatch,
    },
}
pub(crate) struct Plan {
    shape: Shape,
}
fn has_unprotected_tab(source: &str) -> bool {
    let mut offset = 0;
    while offset < source.len() {
        let rest = &source[offset..];
        if let Some(length) = protected_length(rest) {
            offset += length;
        } else {
            let character = rest.chars().next().unwrap();
            if character == '\t' {
                return true;
            }
            offset += character.len_utf8();
        }
    }
    false
}
impl Plan {
    pub(crate) fn new(source: &str) -> Self {
        if !has_unprotected_tab(source) {
            return Self {
                shape: Shape::Plain(ProtectedText::new(source)),
            };
        }
        let mut pieces = Vec::new();
        let mut cells = Vec::new();
        let mut offset = 0;
        let bytes = source.as_bytes();
        loop {
            let quoted = bytes.get(offset) == Some(&b'"');
            let mut value = String::new();
            if quoted {
                offset += 1;
                let mut closed = false;
                while offset < bytes.len() {
                    if bytes[offset] == b'"' {
                        if bytes.get(offset + 1) == Some(&b'"') {
                            value.push('"');
                            offset += 2;
                        } else {
                            offset += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        let character = source[offset..].chars().next().unwrap();
                        value.push(character);
                        offset += character.len_utf8();
                    }
                }
                if !closed
                    || (offset < bytes.len() && !matches!(bytes[offset], b'\t' | b'\r' | b'\n'))
                {
                    return Self {
                        shape: Shape::Invalid,
                    };
                }
            } else {
                let start = offset;
                while offset < bytes.len() && !matches!(bytes[offset], b'\t' | b'\r' | b'\n') {
                    if bytes[offset] == b'"' {
                        return Self {
                            shape: Shape::Invalid,
                        };
                    }
                    offset += 1;
                }
                value.push_str(&source[start..offset]);
            }
            let id = format!("c{}", cells.len());
            cells.push((id.clone(), value));
            pieces.push(Piece::Cell { id, quoted });
            if offset == bytes.len() {
                break;
            }
            let end = offset
                + if bytes[offset] == b'\r' && bytes.get(offset + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            pieces.push(Piece::Literal(source[offset..end].to_owned()));
            offset = end;
        }
        Self {
            shape: Shape::Tsv {
                pieces,
                cells: CellBatch::new(cells),
            },
        }
    }
    pub(crate) fn is_structured(&self) -> bool {
        !matches!(self.shape, Shape::Plain(_))
    }
    pub(crate) fn has_work(&self) -> bool {
        match &self.shape {
            Shape::Plain(text) => text.has_work,
            Shape::Tsv { cells, .. } => cells.has_work(),
            Shape::Invalid => false,
        }
    }
    pub(crate) fn model_text(&self) -> String {
        match &self.shape {
            Shape::Plain(text) => text.masked.clone(),
            Shape::Tsv { cells, .. } => cells.model_text(),
            Shape::Invalid => String::new(),
        }
    }
    pub(crate) fn preview(&self, raw: &str) -> String {
        match &self.shape {
            Shape::Plain(text) => text.preview(raw),
            _ => String::new(),
        }
    }
    pub(crate) fn finish(&self, raw: &str) -> Result<String, String> {
        match &self.shape {
            Shape::Invalid => Err(invalid()),
            Shape::Plain(text) => text.finish(raw),
            Shape::Tsv { pieces, cells } => {
                let values: HashMap<_, _> = cells.finish(raw)?.into_iter().collect();
                let mut output = String::new();
                for piece in pieces {
                    match piece {
                        Piece::Literal(text) => output.push_str(text),
                        Piece::Cell { id, quoted } => {
                            let text = values.get(id).ok_or_else(invalid)?;
                            if *quoted {
                                output.push('"');
                                output.push_str(&text.replace('"', "\"\""));
                                output.push('"');
                            } else {
                                if text.contains(['"', '\t', '\r', '\n']) {
                                    return Err(invalid());
                                }
                                output.push_str(text);
                            }
                        }
                    }
                }
                Ok(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tsv_keeps_empty_cells_irregular_rows_and_numeric_text() {
        let plan = Plan::new("Name\t\t0012\r\nValue\t-3.50\t\r\n\t");
        assert!(plan.is_structured());
        assert_eq!(
            plan.finish(r#"{"cells":[{"id":"c3","text":"值"},{"id":"c0","text":"名称"}]}"#)
                .unwrap(),
            "名称\t\t0012\r\n值\t-3.50\t\r\n\t"
        );
        for raw in [
            r#"{"cells":[{"id":"c0","text":"a"}]}"#,
            r#"{"cells":[{"id":"c0","text":"a"},{"id":"c0","text":"b"}]}"#,
            r#"{"cells":[{"id":"c0","text":"a"},{"id":"other","text":"b"}]}"#,
            r#"{"cells":[{"id":"c0","text":"a\tb"},{"id":"c3","text":"c"}]}"#,
        ] {
            assert!(plan.finish(raw).is_err());
        }
    }
    #[test]
    fn protected_spans_require_each_marker_once_and_never_stream_partial_markers() {
        let text = ProtectedText::new(
            "Use `x <= 0`, $E=mc^2$, https://example.org/a?q=1 and {done}: 0012, -3.50, 0.25%.",
        );
        assert!(text.values.len() >= 7);
        assert_eq!(text.finish(&text.masked).unwrap(), text.original);
        let token = &text.values[0].0;
        assert!(!text
            .preview(&format!("hello {}", &token[..token.len() - 2]))
            .contains("__RS"));
        assert!(text.finish(&text.masked.replace(token, "")).is_err());
        assert!(text.finish(&format!("{}{}", text.masked, token)).is_err());
        assert!(text
            .finish(&format!("{}{}99__", text.masked, text.prefix))
            .is_err());
        assert!(text.finish(&format!("{} 999", text.masked)).is_err());
    }
    #[test]
    fn line_break_style_and_outer_spaces_are_local() {
        let text = ProtectedText::new("  File\r\nEdit  ");
        assert_eq!(text.finish("文件\n编辑").unwrap(), "  文件\r\n编辑  ");
        assert!(text.finish("文件 编辑").is_err());
        let cell = CellBatch::new([("r0c0".into(), "first\nsecond".into())]);
        assert!(cell
            .finish(r#"{"cells":[{"id":"r0c0","text":"一行"}]}"#)
            .is_err());
    }
    #[test]
    fn quoted_tsv_keeps_multiline_cells_quotes_and_tabs_inside_cells() {
        let plan = Plan::new("\"First\r\nSecond\"\t0012\r\n\"Say \"\"hello\"\"\"\t");
        let raw = r#"{"cells":[{"id":"c2","text":"说\"你好\""},{"id":"c0","text":"第一\n第二"}]}"#;
        assert_eq!(
            plan.finish(raw).unwrap(),
            "\"第一\r\n第二\"\t0012\r\n\"说\"\"你好\"\"\"\t"
        );
        assert!(Plan::new("\"unclosed\tvalue").finish("").is_err());
        let tab = Plan::new("\"First\tSecond\"\t0012");
        assert_eq!(
            tab.finish(r#"{"cells":[{"id":"c0","text":"第一\t第二"}]}"#)
                .unwrap(),
            "\"第一\t第二\"\t0012"
        );
    }
    #[test]
    fn tabs_inside_code_do_not_turn_a_paragraph_into_tsv() {
        let source = "Example:\n\u{60}\u{60}\u{60}rust\nlet\tx = 12;\n\u{60}\u{60}\u{60}";
        let plan = Plan::new(source);
        assert!(!plan.is_structured());
        assert_eq!(plan.finish(&plan.model_text()).unwrap(), source);
    }
}
