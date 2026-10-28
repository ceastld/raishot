//! 复制排版独立于原图选字；按识别行的几何分块判断段落，只替换同段续行的分隔符。
use super::{HorizontalDirection, RecognizedText, RecognizedWord, TextGeometry};
use crate::{annotation::Point, geometry::Rect, Result};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyLayoutMode {
    #[default]
    Original,
    Auto,
}

impl CopyLayoutMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "original" => Ok(Self::Original),
            "auto" => Ok(Self::Auto),
            _ => Err("OCR 复制排版须为 original 或 auto".into()),
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopySpanKind {
    Verbatim,
    LineJoin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopySpan {
    pub source_range: Range<usize>,
    pub output_range: Range<usize>,
    pub kind: CopySpanKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopiedText {
    pub text: String,
    pub source_range: Range<usize>,
    /// 原文与输出的连续分区；被移除的换行仍有空 output_range 的 LineJoin。
    pub spans: Vec<CopySpan>,
}

#[derive(Clone, Debug)]
struct LineJoin {
    source: Range<usize>,
    replacement: &'static str,
}

/// 仅持有原文、字素停点与经原图几何筛选的换行决定，不持有推理 DTO 或图片。
#[derive(Clone, Debug)]
pub struct OcrCopyLayout {
    text: String,
    boundaries: Vec<usize>,
    joins: Vec<LineJoin>,
}

impl OcrCopyLayout {
    pub fn new(result: &RecognizedText) -> Self {
        let mut boundaries: Vec<_> = result.text.grapheme_indices(true).map(|(i, _)| i).collect();
        boundaries.push(result.text.len());
        Self {
            text: result.text.clone(),
            boundaries,
            joins: continuation_joins(result),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn copy(&self, range: Range<usize>, mode: CopyLayoutMode) -> Result<String> {
        Ok(self.copy_mapped(range, mode)?.text)
    }

    /// 全文用 0..text().len()；选区仍是原文 byte 范围，非法端点拒绝而不移动。
    pub fn copy_mapped(&self, range: Range<usize>, mode: CopyLayoutMode) -> Result<CopiedText> {
        if range.start > range.end
            || range.end > self.text.len()
            || self.boundaries.binary_search(&range.start).is_err()
            || self.boundaries.binary_search(&range.end).is_err()
        {
            return Err("OCR 复制范围须位于原文 UTF-8 字素边界内".into());
        }
        let mut output = CopiedText {
            text: String::with_capacity(range.len()),
            source_range: range.clone(),
            spans: Vec::new(),
        };
        let mut cursor = range.start;
        if mode == CopyLayoutMode::Auto {
            for join in &self.joins {
                // 两侧都必须有选中的字素；仅选换行或在换行后结束仍复制原换行。
                if range.start < join.source.start && join.source.end < range.end {
                    append_verbatim(&mut output, &self.text, cursor..join.source.start);
                    let start = output.text.len();
                    output.text.push_str(join.replacement);
                    output.spans.push(CopySpan {
                        source_range: join.source.clone(),
                        output_range: start..output.text.len(),
                        kind: CopySpanKind::LineJoin,
                    });
                    cursor = join.source.end;
                }
            }
        }
        append_verbatim(&mut output, &self.text, cursor..range.end);
        Ok(output)
    }
}

fn append_verbatim(output: &mut CopiedText, text: &str, source: Range<usize>) {
    if source.is_empty() {
        return;
    }
    let start = output.text.len();
    output.text.push_str(&text[source.clone()]);
    output.spans.push(CopySpan {
        source_range: source,
        output_range: start..output.text.len(),
        kind: CopySpanKind::Verbatim,
    });
}

struct SourceLine {
    content: Range<usize>,
    separator: Range<usize>,
}

fn source_lines(text: &str) -> Vec<SourceLine> {
    let bytes = text.as_bytes();
    let mut rows = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' || bytes[i] == b'\r' {
            let end = i + usize::from(bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n')) + 1;
            rows.push(SourceLine {
                content: start..i,
                separator: i..end,
            });
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    rows.push(SourceLine {
        content: start..text.len(),
        separator: text.len()..text.len(),
    });
    rows
}

fn checked_rect(rect: Rect) -> Option<Rect> {
    (rect.width > 0
        && rect.height > 0
        && rect.x.checked_add(rect.width).is_some()
        && rect.y.checked_add(rect.height).is_some())
    .then_some(rect)
}

/// 行内的表格或代码迹象：这类行与上下行之间始终保留原换行。
fn structured_text(text: &str) -> bool {
    text.is_empty()
        || text.trim() != text
        || text.contains(['\t', '|', '`', '{', '}', '='])
        || text
            .char_indices()
            .any(|(i, c)| c == ';' && !text[i + 1..].starts_with(' '))
        || text.contains("</")
        || text.contains("/>")
        || text
            .chars()
            .zip(text.chars().skip(1))
            .any(|(a, b)| a.is_whitespace() && b.is_whitespace())
        || shell_prompt(text)
        // 无中文的“a,b,c”是 CSV 片段；中文行里的半角逗号多是 OCR 把全角逗号识别成半角。
        || !text.graphemes(true).any(cjk_grapheme)
            && text.split(',').count() > 1
            && text.split(',').all(|part| {
                !part.is_empty() && part.trim() == part && !part.chars().any(char::is_whitespace)
            })
}

fn shell_prompt(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.split_once("$ ").is_some_and(|(prompt, _)| {
        prompt.is_empty() || prompt.contains('@') || prompt.contains(':')
    }) || text.starts_with("PS ") && text.contains('>')
        || bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
            && text.contains('>')
        || ["λ ", "❯ ", "» "]
            .iter()
            .any(|prompt| text.starts_with(*prompt))
        // 无提示符的命令行：以常见命令开头的行极少是正文续行。
        || [
            "cargo ", "git ", "npm ", "pnpm ", "yarn ", "python ", "python3 ", "pip ", "dotnet ",
            "rustc ", "node ", "docker ",
        ]
        .iter()
        .any(|command| text.starts_with(*command))
}

/// 项目符号、复选框、编号、引用或 Markdown 标题开头的行。
fn list_marker(text: &str) -> bool {
    let first = text.chars().next().unwrap_or_default();
    if matches!(
        first,
        '-' | '+'
            | '*'
            | '•'
            | '◦'
            | '●'
            | '▪'
            | '‣'
            | '>'
            | '#'
            | '§'
            | '☑'
            | '☐'
            | '☒'
            | '✓'
            | '✔'
            | '□'
            | '■'
            | '▢'
            | '▣'
            | '✅'
            | '❎'
            | '❌'
    ) || matches!(first as u32, 0x2460..=0x24ff | 0x2776..=0x2793)
    {
        return true;
    }
    let numbered = text.trim_start_matches(['(', '（', '[', '【']);
    let letters = numbered.bytes().take_while(u8::is_ascii_alphabetic).count();
    if letters > 0
        && (letters == 1
            || numbered[..letters]
                .chars()
                .all(|c| "ivxlcdmIVXLCDM".contains(c)))
        && numbered[letters..].starts_with(['.', ')', '）', ']', '】'])
    {
        return true;
    }
    let digits = numbered
        .char_indices()
        .take_while(|(_, c)| c.is_numeric() || "一二三四五六七八九十百千万零〇".contains(*c))
        .map(|(i, c)| i + c.len_utf8())
        .last();
    digits.is_some_and(|end| {
        let mut rest = numbered[end..].chars();
        match rest.next() {
            Some(')' | '）' | ']' | '】' | '、') => true,
            // 紧跟数字的是时间、版本号或小数（09:21、3.14），不是编号。
            Some('.' | '：' | ':') => !rest.next().is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        }
    })
}

/// 允许的最大倾斜 tan 2°：ppocr 由最小外接矩形得到的四角常有亚像素到 1px 级倾斜，
/// 不能要求严格轴对齐；垂直、180°、明显倾斜或透视的文字仍保留原换行。
const SKEW: f32 = 0.035;

/// 缺少投影保留既有 Windows/AABB 合同；有投影时须有效、正向且接近水平。
fn forward_horizontal(geometry: Option<TextGeometry>, rect: Rect) -> bool {
    let Some(geometry) = geometry else {
        return true;
    };
    if geometry.validated_in(rect).is_none() {
        return false;
    }
    let [a, b, c, d] = geometry.quad;
    // 短框（末行两三个字、单字词框）的角度噪声大，另给 0.1 倍框高的绝对容差。
    let tolerance = rect.height as f32 * 0.1;
    let along = |from: Point, to: Point| {
        let dx = to.x - from.x;
        dx > 0. && (to.y - from.y).abs() <= dx * SKEW + tolerance
    };
    let across = |from: Point, to: Point| {
        let dy = to.y - from.y;
        dy > 0. && (to.x - from.x).abs() <= dy * SKEW + tolerance
    };
    along(a, b) && along(d, c) && across(a, d) && across(b, c)
}

fn words_valid(words: &[&RecognizedWord], rect: Rect) -> bool {
    !words.iter().any(|word| {
        word.text.is_empty()
            || checked_rect(word.rect).is_none()
            || word.horizontal_direction != HorizontalDirection::LeftToRight
            || !forward_horizontal(word.geometry, word.rect)
            || word.rect.x < rect.x
            || word.rect.y < rect.y
            || u64::from(word.rect.x) + u64::from(word.rect.width)
                > u64::from(rect.x) + u64::from(rect.width)
            || u64::from(word.rect.y) + u64::from(word.rect.height)
                > u64::from(rect.y) + u64::from(rect.height)
    })
}

/// 词间空隙超过 1.5 倍行高视为表格或分栏；词框须已按 x 排序。
fn wide_word_gap(words: &[&RecognizedWord], height: u32) -> bool {
    words.windows(2).any(|pair| {
        let right = u64::from(pair[0].rect.x) + u64::from(pair[0].rect.width);
        u64::from(pair[1].rect.x).saturating_sub(right) * 2 > u64::from(height) * 3
    })
}

fn repeated_numeric_columns(
    before: &[&RecognizedWord],
    after: &[&RecognizedWord],
    height: u32,
) -> bool {
    if before.len() < 2 || before.len() != after.len() {
        return false;
    }
    let numeric = |word: &&RecognizedWord| {
        word.text.chars().any(char::is_numeric)
            && word.text.chars().all(|c| {
                c.is_numeric()
                    || matches!(
                        c,
                        '.' | ',' | '+' | '-' | '%' | '$' | '€' | '¥' | '，' | '．' | '％'
                    )
            })
    };
    if !before.iter().all(numeric) || !after.iter().all(numeric) {
        return false;
    }
    let distinct_columns = |words: &[&RecognizedWord]| {
        words.windows(2).all(|pair| {
            let right = u64::from(pair[0].rect.x) + u64::from(pair[0].rect.width);
            u64::from(pair[1].rect.x).saturating_sub(right) * 2 >= u64::from(height)
        })
    };
    distinct_columns(before)
        && distinct_columns(after)
        && before
            .iter()
            .zip(after)
            .all(|(a, b)| u64::from(a.rect.x.abs_diff(b.rect.x)) * 5 <= u64::from(height))
}

/// 字宽单位：CJK 与全角字符记 2，其余字素记 1，用于从行宽估算字宽。
fn width_units(text: &str) -> f64 {
    text.graphemes(true)
        .map(|g| if wide_grapheme(g) { 2. } else { 1. })
        .sum()
}

fn wide_grapheme(grapheme: &str) -> bool {
    cjk_grapheme(grapheme)
        || grapheme
            .chars()
            .next()
            .is_some_and(|c| matches!(c as u32, 0x3000..=0x303f | 0xff01..=0xff60 | 0x2026))
}

/// 参与段落分析的识别行：几何可信、正向水平，词框按 x 排序。
struct Row<'a> {
    text: &'a str,
    rect: Rect,
    words: Vec<&'a RecognizedWord>,
    marker: bool,
    structured: bool,
}

impl Row<'_> {
    fn left(&self) -> f64 {
        f64::from(self.rect.x)
    }
    fn right(&self) -> f64 {
        f64::from(self.rect.x) + f64::from(self.rect.width)
    }
    fn center(&self) -> f64 {
        f64::from(self.rect.y) + f64::from(self.rect.height) / 2.
    }
    fn height(&self) -> f64 {
        f64::from(self.rect.height)
    }

    /// 行首不可拆分片段的宽度：CJK 一个字，拉丁一个空白分隔的词。ppocr 会把
    /// “(word)”拆成多个词框，按文本把相连的词框拼回一个词；没有词框时按平均字宽估算。
    fn first_word_width(&self, unit: f64) -> f64 {
        let first = self.text.graphemes(true).next().unwrap_or_default();
        let token = if wide_grapheme(first) {
            first
        } else {
            self.text
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or_default()
        };
        let mut covered = 0;
        for word in &self.words {
            covered += word.text.len();
            if covered >= token.len() {
                return f64::from(word.rect.x) + f64::from(word.rect.width)
                    - f64::from(self.words[0].rect.x);
            }
        }
        width_units(token) * unit
    }
}

/// 源文相邻两行上下堆叠：同一块内才比较段落信号；否则是块边界，必断。
/// 多栏、侧栏、同一行的多块和回到上方的阅读顺序都在这里分开，不连累其它块。
fn stacked(a: &Row, b: &Row) -> bool {
    let small = a.height().min(b.height());
    let large = a.height().max(b.height());
    let pitch = b.center() - a.center();
    let overlap = a.right().min(b.right()) - a.left().max(b.left());
    let narrow = f64::from(a.rect.width.min(b.rect.width));
    large <= small * 1.3 && pitch > small * 0.6 && pitch <= small * 2.2 && overlap >= narrow * 0.5
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

/// 段落续行判定：先按上下堆叠把相邻行分块，再在块内用相对行距、缩进、
/// “下一行首词放得下却换行”、中文句末、列表与代码表格迹象找出段落边界；
/// 没有边界证据的换行视为同段软换行。只看源文相邻的两行，不重排原文。
fn continuation_joins(result: &RecognizedText) -> Vec<LineJoin> {
    let sources = source_lines(&result.text);
    if sources.len() != result.lines.len()
        || result.words.iter().any(|word| word.line >= sources.len())
        || sources
            .iter()
            .zip(&result.lines)
            .any(|(row, line)| result.text[row.content.clone()] != line.text)
    {
        return Vec::new();
    }
    let mut row_words = vec![Vec::new(); sources.len()];
    for word in &result.words {
        row_words[word.line].push(word);
    }
    let rows: Vec<Option<Row>> = sources
        .iter()
        .zip(&result.lines)
        .zip(row_words)
        .map(|((source, line), mut words)| {
            let text = &result.text[source.content.clone()];
            let rect = line.rect.and_then(checked_rect)?;
            if text.is_empty()
                || line.horizontal_direction != HorizontalDirection::LeftToRight
                || !forward_horizontal(line.geometry, rect)
                || !words_valid(&words, rect)
            {
                return None;
            }
            words.sort_by_key(|word| word.rect.x);
            let structured = structured_text(text) || wide_word_gap(&words, rect.height);
            Some(Row {
                text,
                rect,
                words,
                marker: list_marker(text),
                structured,
            })
        })
        .collect();
    let link = |i: usize| match (&rows[i], &rows[i + 1]) {
        (Some(a), Some(b)) => {
            matches!(&result.text[sources[i].separator.clone()], "\n" | "\r\n") && stacked(a, b)
        }
        _ => false,
    };
    let mut joins = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        while end + 1 < rows.len() && link(end) {
            end += 1;
        }
        let block: Vec<&Row> = rows[start..=end].iter().flatten().collect();
        if block.len() >= 2 {
            joins.extend(block_joins(&block, &sources[start..=end]));
        }
        start = end + 1;
    }
    joins
}

fn block_joins(block: &[&Row], sources: &[SourceLine]) -> Vec<LineJoin> {
    let right = block.iter().map(|row| row.right()).fold(f64::MIN, f64::max);
    let unit = block.iter().map(|row| f64::from(row.rect.width)).sum::<f64>()
        / block.iter().map(|row| width_units(row.text)).sum::<f64>().max(1.);
    let pitch_of = |i: usize| block[i + 1].center() - block[i].center();
    let mut pitches: Vec<f64> = (0..block.len() - 1).map(pitch_of).collect();
    let typical = median(&mut pitches);
    let mut heights: Vec<f64> = block.iter().map(|row| row.height()).collect();
    let spaced = median(&mut heights).and_then(|h| spaced_pitch(&pitches, h));
    let mut joins = Vec::new();
    let mut paragraph_start = true;
    for i in 0..block.len() - 1 {
        let (a, b) = (block[i], block[i + 1]);
        let pitch = pitch_of(i);
        // 列表项、紧凑段落的段间距只比行距大一点：行距明显分成两档且上一行以句末
        // 标点结束时才算段界，避免单靠检测框抖动把正文拆开。
        let item_gap = spaced.is_some_and(|from| pitch >= from) && sentence_end(a.text);
        let joined =
            !item_gap && same_paragraph(a, b, pitch, typical, right, unit, paragraph_start);
        paragraph_start = !joined;
        if !joined {
            continue;
        }
        let replacement = seam(a.text, b.text);
        if join_preserves_graphemes(a.text, b.text, replacement) {
            joins.push(LineJoin {
                source: sources[i].separator.clone(),
                replacement,
            });
        } else {
            paragraph_start = true;
        }
    }
    joins
}

/// 行距分两档时返回较大一档的下限：较小一档（正常行距）至少两个且彼此接近，
/// 两档之差超过行高的 12%、较小档自身跨度的两倍和 10%。`pitches` 已排序。
fn spaced_pitch(pitches: &[f64], height: f64) -> Option<f64> {
    if pitches.len() < 3 {
        return None;
    }
    let split = (1..pitches.len())
        .max_by(|&i, &j| (pitches[i] - pitches[i - 1]).total_cmp(&(pitches[j] - pitches[j - 1])))?;
    let (low, high) = pitches.split_at(split);
    let gap = high[0] - low[low.len() - 1];
    let spread = low[low.len() - 1] - low[0];
    (low.len() >= 2
        && gap >= height * 0.12
        && gap >= spread * 2.
        && high[0] >= low[low.len() - 1] * 1.1)
        .then_some(high[0])
}

/// 行尾是句末标点（允许其后跟右引号或右括号）。
fn sentence_end(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['"', '\'', ')', '”', '’', '）', '」', '』'])
        .graphemes(true)
        .next_back()
        .is_some_and(|g| matches!(g, "." | "!" | "?" | "…" | "。" | "！" | "？"))
}

fn same_paragraph(
    a: &Row,
    b: &Row,
    pitch: f64,
    typical: Option<f64>,
    right: f64,
    unit: f64,
    a_starts_paragraph: bool,
) -> bool {
    let h = a.height().min(b.height());
    // 段间距：相对本块的典型行距；只有两行时用绝对阈值。
    let gap_break = match typical {
        Some(m) => pitch > m * 1.25 && pitch - m > h * 0.25,
        None => pitch > h * 1.45,
    };
    let indent = b.left() - a.left();
    let indent_break = if a.marker {
        // 列表项只接悬挂缩进的续行；与标记同列的下一行是新内容。
        b.marker || indent < h * 0.25 || indent > h * 3.
    } else if indent >= h * 0.5 {
        // 下一行首行缩进：新段落开始。
        true
    } else {
        // 向左凸出只允许出现在首行缩进段落的第二行。
        -indent >= h * 0.5 && !(a_starts_paragraph && -indent <= h * 4.)
    };
    // 上一行没写满：下一行首词本可放在上一行末尾，说明是硬换行。
    let slack = right - a.right();
    let short_line = slack > b.first_word_width(unit) + unit;
    // 不足 8 倍行高（约 12 个汉字或 24 个拉丁字符）的行没有折行证据：
    // 行距均匀的短行列表、聊天记录即使恰好对齐也逐行保留。
    let narrow = a.right() - a.left() < h * 8.;
    // 中文句末且未贴近右缘半个字以上，多为段落最后一行。
    let terminal = a
        .text
        .graphemes(true)
        .next_back()
        .is_some_and(|g| matches!(g, "。" | "！" | "？" | "!" | "?" | "…"))
        && a.text.graphemes(true).any(cjk_grapheme)
        && slack > unit;
    !(gap_break
        || indent_break
        || short_line
        || narrow
        || terminal
        || b.marker
        || a.structured
        || b.structured
        || repeated_numeric_columns(&a.words, &b.words, a.rect.height.min(b.rect.height)))
}

/// 续行接缝：CJK 正文之间零宽；拉丁连字符断词保留连字符不加空格；其余一个空格。
fn seam(before: &str, after: &str) -> &'static str {
    let cjk_pair = cjk_continuation_edge(before.graphemes(true).rev())
        && cjk_continuation_edge(after.graphemes(true));
    let hyphenated = before
        .strip_suffix('-')
        .is_some_and(|stem| stem.chars().next_back().is_some_and(|c| c.is_alphabetic()))
        && after.chars().next().is_some_and(|c| c.is_lowercase());
    if cjk_pair || hyphenated {
        ""
    } else {
        " "
    }
}

fn join_preserves_graphemes(before: &str, after: &str, replacement: &str) -> bool {
    let (Some(left), Some(right)) = (
        before.graphemes(true).next_back(),
        after.graphemes(true).next(),
    ) else {
        return false;
    };
    let seam = format!("{left}{replacement}{right}");
    [left.len(), left.len() + replacement.len()]
        .into_iter()
        .all(|boundary| seam.grapheme_indices(true).any(|(i, _)| i == boundary))
}

// 标点本身不能证明语种；只跨完整的许可字素，紧邻必须是 CJK 正文。
// OCR 换行不保证标点禁则；两侧都允许相同白名单，未知字素和原始空白阻断。
fn cjk_continuation_edge<'a>(mut graphemes: impl Iterator<Item = &'a str>) -> bool {
    graphemes
        .find(|grapheme| {
            !matches!(
                *grapheme,
                "、" | "。"
                    | "，"
                    | "．"
                    | "："
                    | "；"
                    | "？"
                    | "！"
                    | "…"
                    | "（"
                    | "）"
                    | "［"
                    | "］"
                    | "｛"
                    | "｝"
                    | "【"
                    | "】"
                    | "〔"
                    | "〕"
                    | "〈"
                    | "〉"
                    | "《"
                    | "》"
                    | "「"
                    | "」"
                    | "『"
                    | "』"
                    | "〖"
                    | "〗"
                    | "〘"
                    | "〙"
                    | "〚"
                    | "〛"
                    | "“"
                    | "”"
                    | "‘"
                    | "’"
            )
        })
        .is_some_and(cjk_grapheme)
}

fn cjk_grapheme(grapheme: &str) -> bool {
    grapheme.chars().next().is_some_and(|c| {
        matches!(c as u32,
            0x2e80..=0x2fdf | 0x3041..=0x3096 | 0x30a1..=0x30fa | 0x30fc | 0x31f0..=0x31ff |
            0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af |
            0xf900..=0xfaff | 0x20000..=0x2fa1f | 0x30000..=0x323af)
    })
}

#[cfg(test)]
#[path = "recognition_layout_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recognition_layout_projected_tests.rs"]
mod projected_tests;

#[cfg(test)]
#[path = "recognition_layout_paragraph_tests.rs"]
mod paragraph_tests;
