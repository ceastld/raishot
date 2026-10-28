//! 文字对象内的局部样式片段与段落格式；纯数据，不依赖 GPUI、Win32 或磁盘格式。
//! 长度都以对象字号为 1em，整体缩放只改对象字号；颜色 None 为默认色，随主题。
//! 片段按 UTF-8 byte 范围覆盖正文，空隙继承对象样式；段落与 `text.split('\n')` 一一对应。
use crate::{
    text_format::{Alignment, FontFamily},
    Result,
};
use std::{ops::Range, sync::Arc};

pub const MAX_RUNS: usize = 16384;
pub const MAX_PARAGRAPHS: usize = 8192;
pub const MAX_LINKS: usize = 1024;
pub const MAX_LINK_BYTES: usize = 2048;
pub const MAX_TABLES: usize = 64;
pub const MAX_CELLS: usize = 4096;
pub const MAX_COLUMNS: u16 = 256;
/// 缩进、间距、内边距的 em 上限。
pub const MAX_EM: f32 = 64.;
/// 片段字号相对对象字号的倍数范围。
pub const SIZE_RANGE: (f32, f32) = (0.1, 10.);
pub const LINE_HEIGHT_RANGE: (f32, f32) = (0.5, 10.);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Baseline {
    #[default]
    Normal,
    Sub,
    Super,
}

/// None 表示继承对象样式。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunStyle {
    pub font: Option<FontFamily>,
    pub size: Option<f32>,
    pub color: Option<u32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub background: Option<u32>,
    pub link: Option<u16>,
    pub baseline: Baseline,
}
impl RunStyle {
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }
    fn valid(&self, links: usize) -> bool {
        self.size
            .is_none_or(|s| s.is_finite() && (SIZE_RANGE.0..=SIZE_RANGE.1).contains(&s))
            && self.color.is_none_or(|c| c <= 0xffffff)
            && self.background.is_none_or(|c| c <= 0xffffff)
            && self.link.is_none_or(|l| usize::from(l) < links)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub style: RunStyle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paragraph {
    pub align: Option<Alignment>,
    pub indent: f32,
    /// 首行相对 `indent` 的偏移，负值为悬挂缩进（列表符号）。
    pub first_indent: f32,
    pub space_before: f32,
    pub space_after: f32,
    /// 行高倍数；None 为字体自然行高。
    pub line_height: Option<f32>,
    /// false：pre／nowrap，只在正文换行处断行。
    pub wrap: bool,
    pub background: Option<u32>,
    pub quote_bar: Option<u32>,
    pub rule_after: Option<u32>,
    /// (表, 行)；行内单元格以 '\t' 分隔。
    pub table_row: Option<(u16, u16)>,
}
impl Default for Paragraph {
    fn default() -> Self {
        Self {
            align: None,
            indent: 0.,
            first_indent: 0.,
            space_before: 0.,
            space_after: 0.,
            line_height: None,
            wrap: true,
            background: None,
            quote_bar: None,
            rule_after: None,
            table_row: None,
        }
    }
}
impl Paragraph {
    fn valid(&self, tables: usize) -> bool {
        let em = |v: f32| v.is_finite() && v.abs() <= MAX_EM;
        em(self.indent)
            && em(self.first_indent)
            && em(self.space_before)
            && em(self.space_after)
            && self.space_before >= 0.
            && self.space_after >= 0.
            && self.line_height.is_none_or(|h| {
                h.is_finite() && (LINE_HEIGHT_RANGE.0..=LINE_HEIGHT_RANGE.1).contains(&h)
            })
            && [self.background, self.quote_bar, self.rule_after]
                .iter()
                .all(|c| c.is_none_or(|c| c <= 0xffffff))
            && self.table_row.is_none_or(|(t, _)| usize::from(t) < tables)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub row: u16,
    pub col: u16,
    pub col_span: u16,
    pub header: bool,
    pub background: Option<u32>,
}

/// 单元格按 (行, 列) 升序，同一行内跨列不重叠。
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub columns: u16,
    pub border: Option<u32>,
    pub padding: f32,
    pub cells: Vec<Cell>,
}
impl Table {
    fn valid(&self) -> bool {
        if !(1..=MAX_COLUMNS).contains(&self.columns)
            || !self.padding.is_finite()
            || !(0. ..=MAX_EM).contains(&self.padding)
            || self.border.is_some_and(|c| c > 0xffffff)
        {
            return false;
        }
        let mut previous: Option<(u16, u32)> = None;
        self.cells.iter().all(|c| {
            let end = u32::from(c.col) + u32::from(c.col_span);
            let ordered = previous
                .is_none_or(|(row, next)| c.row > row || c.row == row && u32::from(c.col) >= next);
            previous = Some((c.row, end));
            ordered
                && c.col_span >= 1
                && end <= u32::from(self.columns)
                && c.background.is_none_or(|b| b <= 0xffffff)
        })
    }
}

/// 对象级样式改动对应的片段属性；改对象设置等同全选后设置，清掉片段上的覆盖。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribute {
    Font,
    Color,
    Bold,
    Italic,
    Underline,
    Strike,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RichText {
    pub runs: Vec<Run>,
    pub paragraphs: Vec<Paragraph>,
    pub tables: Vec<Table>,
    pub links: Vec<Arc<str>>,
    /// 来源整卡底色；Some 时卡片保留来源配色，不随主题。
    pub card_fill: Option<u32>,
    /// “忽略格式”：数据保留，按对象基准样式绘制。
    pub plain: bool,
}

pub fn paragraph_count(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count() + 1
}

/// 只保留可在浏览器外安全打开的链接。
pub fn link_allowed(link: &str) -> bool {
    link.len() <= MAX_LINK_BYTES
        && ["http://", "https://", "mailto:"].iter().any(|scheme| {
            link.len() > scheme.len()
                && link
                    .get(..scheme.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(scheme))
        })
        && !link.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn newlines(text: &str) -> usize {
    text.bytes().filter(|b| *b == b'\n').count()
}

impl RichText {
    #[cfg_attr(not(test), allow(dead_code))] // HTML 导入使用。
    pub fn new(text: &str) -> Self {
        Self {
            runs: Vec::new(),
            paragraphs: vec![Paragraph::default(); paragraph_count(text)],
            tables: Vec::new(),
            links: Vec::new(),
            card_fill: None,
            plain: false,
        }
    }

    pub fn validate(&self, text: &str) -> Result<()> {
        let len = text.len();
        if len > u32::MAX as usize
            || self.runs.len() > MAX_RUNS
            || self.paragraphs.len() > MAX_PARAGRAPHS
            || self.paragraphs.len() != paragraph_count(text)
            || self.links.len() > MAX_LINKS
            || self.tables.len() > MAX_TABLES
            || self.tables.iter().map(|t| t.cells.len()).sum::<usize>() > MAX_CELLS
            || self.card_fill.is_some_and(|c| c > 0xffffff)
        {
            return Err("文字局部样式数量超限或段落与正文不一致".into());
        }
        let mut previous = 0;
        for run in &self.runs {
            let (start, end) = (run.start as usize, run.end as usize);
            if start < previous
                || start >= end
                || end > len
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
                || !run.style.valid(self.links.len())
            {
                return Err("文字样式片段范围或取值无效".into());
            }
            previous = end;
        }
        if !self.paragraphs.iter().all(|p| p.valid(self.tables.len())) {
            return Err("文字段落格式无效".into());
        }
        if !self.links.iter().all(|l| link_allowed(l)) {
            return Err("文字链接无效".into());
        }
        if !self.tables.iter().all(Table::valid) {
            return Err("文字表格结构无效".into());
        }
        Ok(())
    }

    /// byte 所在字符的片段样式；空隙为默认。
    pub fn style_at(&self, byte: usize) -> RunStyle {
        let i = self.runs.partition_point(|r| r.end as usize <= byte);
        self.runs
            .get(i)
            .filter(|r| r.start as usize <= byte)
            .map_or_else(RunStyle::default, |r| r.style)
    }

    /// 插入文字继承插入点左侧字符（段首取右侧）；只有两侧同属一个链接时才延续链接。
    fn inherited(&self, old: &str, start: usize, end: usize) -> RunStyle {
        if let Some(c) = old[..start].chars().next_back().filter(|c| *c != '\n') {
            let mut style = self.style_at(start - c.len_utf8());
            if style.link.is_some() && (end >= old.len() || self.style_at(end).link != style.link) {
                style.link = None;
            }
            return style;
        }
        if end < old.len() {
            let mut style = self.style_at(end);
            style.link = None;
            return style;
        }
        RunStyle::default()
    }

    /// `old[range]` 换成 `inserted` 后的片段与段落；调用者随后改写正文。
    /// 合并段落保留首段格式，拆出的新段沿用原段格式。
    pub fn replace(&mut self, old: &str, range: Range<usize>, inserted: &str) {
        let (start, end) = (range.start, range.end);
        debug_assert!(start <= end && old.is_char_boundary(start) && old.is_char_boundary(end));
        let inherited = self.inherited(old, start, end);
        let moved = |offset: usize| (offset - end + start + inserted.len()) as u32;
        let mut runs = Vec::with_capacity(self.runs.len() + 1);
        let mut after = Vec::new();
        for run in &self.runs {
            let (s, e) = (run.start as usize, run.end as usize);
            if s < start {
                runs.push(Run {
                    end: e.min(start) as u32,
                    ..*run
                });
            }
            if e > end {
                after.push(Run {
                    start: moved(s.max(end)),
                    end: moved(e),
                    style: run.style,
                });
            }
        }
        if !inserted.is_empty() && !inherited.is_plain() {
            runs.push(Run {
                start: start as u32,
                end: (start + inserted.len()) as u32,
                style: inherited,
            });
        }
        runs.extend(after);
        self.runs = runs;
        let first = newlines(&old[..start]);
        if let Some(&paragraph) = self.paragraphs.get(first) {
            let last = (first + newlines(&old[start..end]) + 1).min(self.paragraphs.len());
            self.paragraphs.splice(
                first..last,
                std::iter::repeat_n(paragraph, newlines(inserted) + 1),
            );
        }
        self.normalize();
    }

    /// 正文整串替换时用公共前后缀推出编辑区间；有精确区间时用 [`Self::replace`]。
    pub fn rebase(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let prefix: usize = old
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum();
        let limit = old.len().min(new.len()) - prefix;
        let mut suffix = 0;
        for (a, b) in old[prefix..].chars().rev().zip(new[prefix..].chars().rev()) {
            if a != b || suffix + a.len_utf8() > limit {
                break;
            }
            suffix += a.len_utf8();
        }
        self.replace(
            old,
            prefix..old.len() - suffix,
            &new[prefix..new.len() - suffix],
        );
    }

    /// 对范围内每段样式（含空隙）调用 `f`，范围外不变。
    #[cfg_attr(not(test), allow(dead_code))] // HTML 导入与局部格式使用。
    pub fn apply(&mut self, range: Range<usize>, mut f: impl FnMut(&mut RunStyle)) {
        if range.start >= range.end {
            return;
        }
        let (start, end) = (range.start as u32, range.end as u32);
        let mut styled = |start: u32, end: u32, mut style: RunStyle, out: &mut Vec<Run>| {
            f(&mut style);
            out.push(Run { start, end, style });
        };
        let mut runs = Vec::with_capacity(self.runs.len() + 2);
        let mut cursor = start;
        for run in &self.runs {
            if run.end <= start {
                runs.push(*run);
                continue;
            }
            if run.start >= end {
                if cursor < end {
                    styled(cursor, end, RunStyle::default(), &mut runs);
                    cursor = end;
                }
                runs.push(*run);
                continue;
            }
            if run.start < start {
                runs.push(Run { end: start, ..*run });
            }
            let s = run.start.max(start);
            if cursor < s {
                styled(cursor, s, RunStyle::default(), &mut runs);
            }
            let e = run.end.min(end);
            styled(s, e, run.style, &mut runs);
            cursor = e;
            if run.end > end {
                runs.push(Run { start: end, ..*run });
            }
        }
        if cursor < end {
            styled(cursor, end, RunStyle::default(), &mut runs);
        }
        self.runs = runs;
        self.normalize();
    }

    pub fn clear(&mut self, attribute: Attribute) {
        for run in &mut self.runs {
            let style = &mut run.style;
            match attribute {
                Attribute::Font => style.font = None,
                Attribute::Color => style.color = None,
                Attribute::Bold => style.bold = None,
                Attribute::Italic => style.italic = None,
                Attribute::Underline => style.underline = None,
                Attribute::Strike => style.strike = None,
            }
        }
        self.normalize();
    }

    /// 去掉空片段和与对象样式相同的片段，合并相邻同样式片段。
    pub fn normalize(&mut self) {
        let mut runs: Vec<Run> = Vec::with_capacity(self.runs.len());
        for run in self.runs.drain(..) {
            if run.start >= run.end || run.style.is_plain() {
                continue;
            }
            if let Some(last) = runs.last_mut() {
                if last.end == run.start && last.style == run.style {
                    last.end = run.end;
                    continue;
                }
            }
            runs.push(run);
        }
        self.runs = runs;
    }

    /// `text[range]` 的样式副本，供复制所选；表格与链接表原样保留，索引不变。
    #[cfg_attr(not(test), allow(dead_code))] // 复制所选为 HTML 时使用。
    pub fn slice(&self, text: &str, range: Range<usize>) -> Self {
        let (start, end) = (range.start, range.end);
        let runs = self
            .runs
            .iter()
            .filter(|r| r.end as usize > start && (r.start as usize) < end)
            .map(|r| Run {
                start: ((r.start as usize).max(start) - start) as u32,
                end: ((r.end as usize).min(end) - start) as u32,
                style: r.style,
            })
            .collect();
        let first = newlines(&text[..start]);
        let paragraphs = (first..first + paragraph_count(&text[start..end]))
            .map(|i| self.paragraphs.get(i).copied().unwrap_or_default())
            .collect();
        Self {
            runs,
            paragraphs,
            tables: self.tables.clone(),
            links: self.links.clone(),
            card_fill: self.card_fill,
            plain: self.plain,
        }
    }

    /// 字体从验证用符号映射到本机字体；不合并片段，保持原始结构。
    pub fn map_fonts(&mut self, mut f: impl FnMut(FontFamily) -> FontFamily) {
        for run in &mut self.runs {
            if let Some(font) = &mut run.style.font {
                *font = f(*font);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold() -> RunStyle {
        RunStyle {
            bold: Some(true),
            ..Default::default()
        }
    }
    fn red() -> RunStyle {
        RunStyle {
            color: Some(0xff0000),
            ..Default::default()
        }
    }
    fn run(start: u32, end: u32, style: RunStyle) -> Run {
        Run { start, end, style }
    }
    fn rich(text: &str, runs: Vec<Run>) -> RichText {
        let mut rich = RichText::new(text);
        rich.runs = runs;
        rich.validate(text).unwrap();
        rich
    }
    /// 逐字样式，用于和增量结果对照。
    fn per_char(rich: &RichText, text: &str) -> Vec<(char, RunStyle)> {
        text.char_indices()
            .map(|(i, c)| (c, rich.style_at(i)))
            .collect()
    }

    #[test]
    fn rich_text_validate_rejects_bad_ranges_values_and_paragraph_mismatch() {
        let text = "粗体 plain\n第二段";
        let good = rich(text, vec![run(0, 6, bold())]);
        let mut bad = good.clone();
        bad.runs = vec![run(0, 4, bold())];
        assert!(bad.validate(text).is_err(), "不在字符边界");
        bad.runs = vec![run(0, 6, bold()), run(3, 9, red())];
        assert!(bad.validate(text).is_err(), "重叠");
        bad.runs = vec![run(6, 6, bold())];
        assert!(bad.validate(text).is_err(), "空片段");
        bad.runs = vec![run(0, 99, bold())];
        assert!(bad.validate(text).is_err(), "越界");
        bad.runs = vec![run(
            0,
            3,
            RunStyle {
                size: Some(20.),
                ..Default::default()
            },
        )];
        assert!(bad.validate(text).is_err(), "字号倍数");
        bad.runs = vec![run(
            0,
            3,
            RunStyle {
                link: Some(0),
                ..Default::default()
            },
        )];
        assert!(bad.validate(text).is_err(), "链接索引");
        let mut paragraphs = good.clone();
        paragraphs.paragraphs.pop();
        assert!(paragraphs.validate(text).is_err());
        let mut table = good.clone();
        table.paragraphs[1].table_row = Some((0, 0));
        assert!(table.validate(text).is_err(), "表格不存在");
        table.tables.push(Table {
            columns: 2,
            border: None,
            padding: 0.3,
            cells: vec![
                Cell {
                    row: 0,
                    col: 1,
                    col_span: 1,
                    header: false,
                    background: None,
                },
                Cell {
                    row: 0,
                    col: 0,
                    col_span: 1,
                    header: false,
                    background: None,
                },
            ],
        });
        assert!(table.validate(text).is_err(), "单元格乱序");
        table.tables[0].cells.swap(0, 1);
        table.validate(text).unwrap();
    }

    #[test]
    fn rich_text_links_only_keep_safe_schemes() {
        assert!(link_allowed("https://example.com/a?b=1"));
        assert!(link_allowed("MAILTO:a@b.c"));
        for bad in [
            "javascript:alert(1)",
            "file:///C:/x",
            "https://",
            "https://a b",
            "",
        ] {
            assert!(!link_allowed(bad), "{bad}");
        }
        assert!(!link_allowed(&format!(
            "https://{}",
            "a".repeat(MAX_LINK_BYTES)
        )));
    }

    #[test]
    fn rich_text_insert_inherits_left_and_paragraph_start_takes_right() {
        let text = "ab\ncd";
        let mut r = rich(text, vec![run(0, 2, bold()), run(3, 5, red())]);
        // 在 a 后插入：继承 a 的粗体。
        r.replace(text, 1..1, "X");
        assert_eq!(r.runs, vec![run(0, 3, bold()), run(4, 6, red())]);
        // 段首插入：继承右侧 c 的红色。
        let text = "aXb\ncd";
        r.replace(text, 4..4, "Y");
        assert_eq!(r.runs, vec![run(0, 3, bold()), run(4, 7, red())]);
        r.validate("aXb\nYcd").unwrap();
    }

    #[test]
    fn rich_text_links_extend_only_inside_and_not_at_edges() {
        let text = "go here!";
        let link = RunStyle {
            link: Some(0),
            color: Some(0x0645ad),
            ..Default::default()
        };
        let mut r = rich(text, vec![]);
        r.links.push("https://example.com".into());
        r.runs = vec![run(3, 7, link)];
        r.validate(text).unwrap();
        let mut inside = r.clone();
        inside.replace(text, 5..5, "--");
        assert_eq!(inside.runs, vec![run(3, 9, link)]);
        let mut after = r.clone();
        after.replace(text, 7..7, "?");
        let expected = RunStyle { link: None, ..link };
        assert_eq!(after.runs, vec![run(3, 7, link), run(7, 8, expected)]);
    }

    #[test]
    fn rich_text_newline_edits_split_and_merge_paragraph_styles() {
        let text = "标题\n正文";
        let mut r = rich(text, vec![]);
        r.paragraphs[0].space_after = 0.5;
        r.paragraphs[1].indent = 2.;
        let mut split = r.clone();
        split.replace(text, 3..3, "\n");
        assert_eq!(split.paragraphs.len(), 3);
        assert_eq!(split.paragraphs[0], split.paragraphs[1]);
        assert_eq!(split.paragraphs[2].indent, 2.);
        split.validate("标\n题\n正文").unwrap();
        let mut merged = r.clone();
        merged.replace(text, 6..7, "");
        assert_eq!(merged.paragraphs, vec![r.paragraphs[0]]);
        merged.validate("标题正文").unwrap();
    }

    #[test]
    fn rich_text_delete_across_runs_shrinks_and_merges() {
        let text = "aaabbbccc";
        let mut r = rich(
            text,
            vec![run(0, 3, bold()), run(3, 6, red()), run(6, 9, bold())],
        );
        r.replace(text, 2..7, "");
        assert_eq!(r.runs, vec![run(0, 4, bold())]);
        r.validate("aacc").unwrap();
    }

    #[test]
    fn rich_text_rebase_matches_exact_replace_for_unambiguous_edits() {
        let text = "红色 bold 普通\n第二段 🧑‍💻 尾";
        let base = rich(
            text,
            vec![run(0, 6, red()), run(7, 11, bold()), run(29, 33, red())],
        );
        let edits: [(Range<usize>, &str); 6] = [
            (0..0, "前"),
            (3..6, ""),
            (7..11, "BOLD!"),
            (18..19, "\n新"),
            (text.len()..text.len(), "末尾"),
            (0..text.len(), "全部替换"),
        ];
        for (range, inserted) in edits {
            let mut new = text.to_owned();
            new.replace_range(range.clone(), inserted);
            let mut exact = base.clone();
            exact.replace(text, range.clone(), inserted);
            exact.validate(&new).unwrap();
            let mut rebased = base.clone();
            rebased.rebase(text, &new);
            rebased.validate(&new).unwrap();
            assert_eq!(
                per_char(&rebased, &new),
                per_char(&exact, &new),
                "{range:?}"
            );
        }
    }

    #[test]
    fn rich_text_random_edits_keep_invariants() {
        // 固定种子的线性同余，覆盖随机插删、换行和多字节字符。
        let mut seed = 0x2545f491u32;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as usize % n.max(1)
        };
        let pieces = ["a", "字", "\n", "🙂", "bc", ""];
        let mut text = String::from("初始 text\n第二段");
        let mut r = rich(&text, vec![run(0, 6, bold()), run(7, 11, red())]);
        for _ in 0..400 {
            let bounds: Vec<usize> = text
                .char_indices()
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            let a = bounds[next(bounds.len())];
            let b = bounds[next(bounds.len())];
            let range = a.min(b)..a.max(b);
            let inserted = pieces[next(pieces.len())];
            if next(3) == 0 {
                let style = if next(2) == 0 { bold() } else { red() };
                r.apply(range.clone(), |s| *s = style);
            }
            r.replace(&text, range.clone(), inserted);
            text.replace_range(range, inserted);
            r.validate(&text).unwrap();
        }
    }

    #[test]
    fn rich_text_apply_splits_runs_and_fills_gaps() {
        let text = "0123456789";
        let mut r = rich(text, vec![run(2, 5, red())]);
        r.apply(4..8, |s| s.bold = Some(true));
        let both = RunStyle {
            bold: Some(true),
            ..red()
        };
        assert_eq!(
            r.runs,
            vec![run(2, 4, red()), run(4, 5, both), run(5, 8, bold())]
        );
        r.clear(Attribute::Bold);
        assert_eq!(r.runs, vec![run(2, 5, red())]);
        r.clear(Attribute::Color);
        assert!(r.runs.is_empty());
    }

    #[test]
    fn rich_text_slice_shifts_runs_and_paragraphs() {
        let text = "ab\ncd\nef";
        let mut r = rich(text, vec![run(1, 4, bold())]);
        r.paragraphs[1].indent = 1.;
        let slice = r.slice(text, 3..8);
        slice.validate("cd\nef").unwrap();
        assert_eq!(slice.runs, vec![run(0, 1, bold())]);
        assert_eq!(slice.paragraphs[0].indent, 1.);
        assert_eq!(slice.paragraphs[1].indent, 0.);
    }
}
