//! 有界的 Markdown 文档模型；数学只切分原文，不修补或改写公式。
//!
//! 普通文字 OCR 不使用本模块。调用者显式选择文档／公式呈现后，才解析 Markdown
//! 表格与数学定界。所有块保留原始 source，局部失败可直接显示源文。
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use std::ops::Range;

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_NESTING: usize = 32;
pub const MAX_TABLE_COLUMNS: usize = 32;
pub const MAX_TABLE_ROWS: usize = 512;
pub const MAX_TABLE_CELLS: usize = 4096;
const MAX_NODES: usize = 32_768;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub source: String,
    pub blocks: Vec<Block>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Document {
    /// 只给显式文档／译文视图选择呈现方式；不得据此自动改写普通 OCR。
    pub fn has_rich_content(&self) -> bool {
        self.blocks.iter().any(|block| match &block.kind {
            BlockKind::Paragraph(inlines) => inlines.iter().any(inline_is_rich),
            _ => true,
        })
    }
}

fn inline_is_rich(inline: &Inline) -> bool {
    !matches!(
        inline,
        Inline::Text(_) | Inline::SoftBreak | Inline::HardBreak
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// 原始 UTF-8 source 中的字节范围。
    pub range: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// 含 Markdown／数学定界的原始片段；段间空白仍由 Document.source 完整持有。
    /// 不要求包含块结束后的换行，也不要用渲染结果覆盖它。
    pub source: String,
    pub kind: BlockKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Code {
        language: Option<String>,
        code: String,
    },
    /// 不含外层定界的原始 LaTeX；交给数学排版层局部校验。
    Math {
        source: String,
    },
    /// 第一行为表头。空单元格仍占一个位置，不推断数字类型。
    Table {
        alignments: Vec<Alignment>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    Math {
        source: String,
    },
    Link {
        destination: String,
        content: Vec<Inline>,
    },
    SoftBreak,
    HardBreak,
}

/// formula_mode 额外接受没有定界的纯 LaTeX；混合段落始终按文档拆分。
pub fn parse_with_mode(source: &str, formula_mode: bool) -> Result<Document, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(tr!(
            "文档过长，请分段处理",
            "The document is too long; split it into smaller parts"
        )
        .into());
    }
    let mut diagnostics = Vec::new();
    let protected = protected_markdown_ranges(source)?;
    let mut math = scan_math(source, &protected, &mut diagnostics)?;
    let masked = mask_math(source, &math);
    let nodes = parse_nodes(source, &masked, &mut math)?;
    for span in &math {
        if span.body.is_some() && !span.used {
            diagnostics.push(Diagnostic {
                range: span.range.clone(),
                message: tr!(
                    "数学定界无法独立解析，已保留源文",
                    "Math delimiters could not be parsed separately; the source is preserved"
                )
                .into(),
            });
        }
    }
    let mut converter = Converter {
        source,
        diagnostics,
        table_cells: 0,
    };
    let mut blocks = converter.blocks(&nodes);
    if formula_mode
        && math.is_empty()
        && converter.diagnostics.is_empty()
        && bare_latex(source, &nodes)
    {
        blocks = vec![Block {
            source: source.to_owned(),
            kind: BlockKind::Math {
                source: source.trim().to_owned(),
            },
        }];
    }
    Ok(Document {
        source: source.to_owned(),
        blocks,
        diagnostics: converter.diagnostics,
    })
}

#[derive(Debug)]
struct MathSpan {
    range: Range<usize>,
    body: Option<Range<usize>>,
    display: bool,
    used: bool,
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

/// 先排除代码／原始 HTML／链接目的地，避免将其中的美元号当作数学。
fn protected_markdown_ranges(source: &str) -> Result<Vec<Range<usize>>, String> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut count = 0usize;
    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        count += 1;
        if count > MAX_NODES {
            return Err(complexity_error());
        }
        match event {
            Event::Start(tag) => {
                depth += 1;
                if depth > MAX_NESTING {
                    return Err(complexity_error());
                }
                if matches!(
                    tag,
                    Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::Link { .. } | Tag::Image { .. }
                ) {
                    ranges.push(range);
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Code(_) | Event::Html(_) | Event::InlineHtml(_) => ranges.push(range),
            _ => {}
        }
    }
    ranges.sort_by_key(|range| range.start);
    Ok(ranges)
}

fn escaped(source: &str, at: usize) -> bool {
    source.as_bytes()[..at]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        != 0
}

fn scan_math(
    source: &str,
    protected: &[Range<usize>],
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<MathSpan>, String> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut exclusion = 0;
    let mut steps = 0usize;
    while i < bytes.len() {
        steps += 1;
        if steps > MAX_SOURCE_BYTES * 8 || spans.len() >= MAX_NODES / 2 {
            return Err(complexity_error());
        }
        while exclusion < protected.len() && protected[exclusion].end <= i {
            exclusion += 1;
        }
        if let Some(range) = protected.get(exclusion).filter(|range| range.contains(&i)) {
            i = range.end;
            continue;
        }
        let rest = &bytes[i..];
        let pair = if rest.starts_with(b"\\(") && !escaped(source, i) {
            Some((2, b"\\)".as_slice(), false))
        } else if rest.starts_with(b"\\[") && !escaped(source, i) {
            Some((2, b"\\]".as_slice(), true))
        } else if bytes[i] == b'$' && !escaped(source, i) {
            if rest.starts_with(b"$$") {
                Some((2, b"$$".as_slice(), true))
            } else {
                Some((1, b"$".as_slice(), false))
            }
        } else {
            None
        };
        if let Some((opening, closing, display)) = pair {
            let body_start = i + opening;
            let mut at = body_start;
            let mut found = None;
            // 不能借用代码／链接里的定界符来闭合前面的普通正文。
            let search_end = protected
                .get(exclusion)
                .map_or(bytes.len(), |range| range.start);
            while at + closing.len() <= search_end {
                steps += 1;
                if steps > MAX_SOURCE_BYTES * 8 {
                    return Err(complexity_error());
                }
                // 行内定界不跨空段落；不猜测它属于后面的另一段公式。
                if !display
                    && (bytes[at..].starts_with(b"\n\n") || bytes[at..].starts_with(b"\r\n\r\n"))
                {
                    break;
                }
                if bytes[at..].starts_with(closing) && !escaped(source, at) {
                    let dollar_run_matches = closing[0] != b'$'
                        || (bytes.get(at.wrapping_sub(1)) != Some(&b'$')
                            && bytes.get(at + closing.len()) != Some(&b'$'));
                    // 与常见美元数学规则一致：单美元的内部边缘不留空白，
                    // 结束美元后面不能紧接数字，避免把 $5 and $10 解释成公式。
                    let dollar_edges_match = closing != b"$"
                        || (at > body_start
                            && !bytes[body_start].is_ascii_whitespace()
                            && !bytes[at - 1].is_ascii_whitespace()
                            && !bytes.get(at + 1).is_some_and(u8::is_ascii_digit));
                    let empty_dollars = closing == b"$" && source[body_start..at].trim().is_empty();
                    if dollar_run_matches && (dollar_edges_match || empty_dollars) {
                        found = Some(at);
                        break;
                    }
                }
                at += 1;
            }
            if let Some(close) = found {
                let end = close + closing.len();
                let nonempty = !source[body_start..close].trim().is_empty();
                if !nonempty {
                    diagnostics.push(Diagnostic {
                        range: i..end,
                        message: tr!(
                            "数学定界内没有内容，已保留源文",
                            "Math delimiters are empty; the source is preserved"
                        )
                        .into(),
                    });
                }
                spans.push(MathSpan {
                    range: i..end,
                    body: nonempty.then_some(body_start..close),
                    display,
                    used: false,
                });
                i = end;
            } else {
                let range = i..i + opening;
                let currency = closing == b"$"
                    && (bytes.get(body_start).is_some_and(u8::is_ascii_digit)
                        || bytes.get(body_start) == Some(&b'.')
                            && bytes.get(body_start + 1).is_some_and(u8::is_ascii_digit));
                if !currency {
                    diagnostics.push(Diagnostic {
                        range: range.clone(),
                        message: tr!(
                            "数学定界未闭合，已保留源文",
                            "Math delimiters are not closed; the source is preserved"
                        )
                        .into(),
                    });
                }
                spans.push(MathSpan {
                    range,
                    body: None,
                    display,
                    used: false,
                });
                i += opening;
            }
        } else if (rest.starts_with(b"\\)") || rest.starts_with(b"\\]")) && !escaped(source, i) {
            let range = i..i + 2;
            diagnostics.push(Diagnostic {
                range: range.clone(),
                message: tr!(
                    "数学结束定界没有对应开头，已保留源文",
                    "A closing math delimiter has no opening delimiter; the source is preserved"
                )
                .into(),
            });
            spans.push(MathSpan {
                range,
                body: None,
                display: false,
                used: false,
            });
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(spans)
}

/// 等字节长度遮罩让 Markdown 不解释数学内的下划线、竖线或换行。
/// OffsetIter 的范围仍直接指向原文；遮罩内容绝不进入公开模型。
fn mask_math(source: &str, spans: &[MathSpan]) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for span in spans {
        bytes[span.range.clone()].fill(b'x');
        if span.body.is_some() {
            let width = if span.display { 2 } else { 1 };
            bytes[span.range.start..span.range.start + width].fill(b'$');
            bytes[span.range.end - width..span.range.end].fill(b'$');
        }
    }
    // 完整数学片段（包括原 UTF-8 字符）统一换为 ASCII，其他字节未动。
    String::from_utf8(bytes).expect("math masks replace complete UTF-8 ranges")
}

#[derive(Debug)]
struct Node {
    range: Range<usize>,
    kind: NodeKind,
    children: Vec<Node>,
}

#[derive(Debug)]
enum NodeKind {
    Container(Tag<'static>),
    Text(String),
    Code(String),
    Math { source: String, display: bool },
    SoftBreak,
    HardBreak,
    Rule,
}

fn parse_nodes(source: &str, masked: &str, math: &mut [MathSpan]) -> Result<Vec<Node>, String> {
    let mut roots = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut count = 0usize;
    for (event, range) in
        Parser::new_ext(masked, options() | Options::ENABLE_MATH).into_offset_iter()
    {
        count += 1;
        if count > MAX_NODES {
            return Err(complexity_error());
        }
        let kind = match event {
            Event::Start(tag) => {
                if stack.len() >= MAX_NESTING {
                    return Err(complexity_error());
                }
                stack.push(Node {
                    range,
                    kind: NodeKind::Container(tag.into_static()),
                    children: Vec::new(),
                });
                continue;
            }
            Event::End(_) => {
                let Some(mut node) = stack.pop() else {
                    return Err(complexity_error());
                };
                node.range.end = node.range.end.max(range.end);
                append_node(&mut roots, &mut stack, node);
                continue;
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => {
                let index = math.partition_point(|span| span.range.end <= range.start);
                if let Some(span) = math.get_mut(index).filter(|span| {
                    span.body.is_some()
                        && span.range.start <= range.start
                        && span.range.end >= range.end
                }) {
                    span.used = true;
                    NodeKind::Math {
                        source: source[span.body.clone().unwrap()].to_owned(),
                        display: span.display,
                    }
                } else {
                    NodeKind::Text(source[range.clone()].to_owned())
                }
            }
            Event::Text(text) => {
                let index = math.partition_point(|span| span.range.end <= range.start);
                let overlaps_mask = math
                    .get(index)
                    .is_some_and(|span| span.range.start < range.end);
                NodeKind::Text(if overlaps_mask {
                    source[range.clone()].to_owned()
                } else {
                    text.into_string()
                })
            }
            Event::Code(code) => NodeKind::Code(code.into_string()),
            Event::Html(_) | Event::InlineHtml(_) => {
                NodeKind::Text(source[range.clone()].to_owned())
            }
            Event::SoftBreak => NodeKind::SoftBreak,
            Event::HardBreak => NodeKind::HardBreak,
            Event::Rule => NodeKind::Rule,
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {
                NodeKind::Text(source[range.clone()].to_owned())
            }
        };
        append_node(
            &mut roots,
            &mut stack,
            Node {
                range,
                kind,
                children: Vec::new(),
            },
        );
    }
    if !stack.is_empty() {
        return Err(complexity_error());
    }
    Ok(roots)
}

fn append_node(roots: &mut Vec<Node>, stack: &mut [Node], node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        roots.push(node);
    }
}

fn complexity_error() -> String {
    tr!(
        "文档结构过于复杂，请分段处理",
        "The document structure is too complex; split it into smaller parts"
    )
    .into()
}

struct Converter<'a> {
    source: &'a str,
    diagnostics: Vec<Diagnostic>,
    table_cells: usize,
}

impl Converter<'_> {
    fn raw(&self, range: &Range<usize>) -> String {
        self.source[range.clone()].to_owned()
    }

    fn blocks(&mut self, nodes: &[Node]) -> Vec<Block> {
        let mut result = Vec::new();
        let mut inline_start = 0;
        for (index, node) in nodes.iter().enumerate() {
            if is_block(node) {
                self.paragraphs(&nodes[inline_start..index], &mut result);
                match &node.kind {
                    NodeKind::Container(Tag::Paragraph) => {
                        self.paragraphs(&node.children, &mut result)
                    }
                    NodeKind::Container(Tag::Heading { level, .. }) => result.push(Block {
                        source: self.raw(&node.range),
                        kind: BlockKind::Heading {
                            level: *level as u8,
                            content: self.inlines(&node.children),
                        },
                    }),
                    NodeKind::Container(Tag::List(start)) => {
                        let items = node
                            .children
                            .iter()
                            .filter(|child| matches!(child.kind, NodeKind::Container(Tag::Item)))
                            .map(|child| self.blocks(&child.children))
                            .collect();
                        result.push(Block {
                            source: self.raw(&node.range),
                            kind: BlockKind::List {
                                start: *start,
                                items,
                            },
                        });
                    }
                    NodeKind::Container(Tag::BlockQuote(_)) => {
                        let content = self.blocks(&node.children);
                        result.push(Block {
                            source: self.raw(&node.range),
                            kind: BlockKind::Quote(content),
                        });
                    }
                    NodeKind::Container(Tag::CodeBlock(kind)) => {
                        let language = match kind {
                            CodeBlockKind::Fenced(info) => {
                                info.split_whitespace().next().map(str::to_owned)
                            }
                            CodeBlockKind::Indented => None,
                        };
                        let code = node
                            .children
                            .iter()
                            .filter_map(|child| match &child.kind {
                                NodeKind::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        result.push(Block {
                            source: self.raw(&node.range),
                            kind: BlockKind::Code { language, code },
                        });
                    }
                    NodeKind::Container(Tag::Table(alignments)) => {
                        result.push(self.table(node, alignments))
                    }
                    NodeKind::Math {
                        source,
                        display: true,
                    } => result.push(Block {
                        source: self.raw(&node.range),
                        kind: BlockKind::Math {
                            source: source.clone(),
                        },
                    }),
                    NodeKind::Rule => result.push(Block {
                        source: self.raw(&node.range),
                        kind: BlockKind::Rule,
                    }),
                    _ => result.push(Block {
                        source: self.raw(&node.range),
                        kind: BlockKind::Paragraph(vec![Inline::Text(self.raw(&node.range))]),
                    }),
                }
                inline_start = index + 1;
            }
        }
        self.paragraphs(&nodes[inline_start..], &mut result);
        result
    }

    /// $$ 和 \[ 即使夹在文字段落中也独立为块，前后文字不会送入数学排版器。
    fn paragraphs(&self, nodes: &[Node], result: &mut Vec<Block>) {
        let mut start = 0;
        for (index, node) in nodes.iter().enumerate() {
            if let NodeKind::Math {
                source,
                display: true,
            } = &node.kind
            {
                self.inline_paragraph(&nodes[start..index], result);
                result.push(Block {
                    source: self.raw(&node.range),
                    kind: BlockKind::Math {
                        source: source.clone(),
                    },
                });
                start = index + 1;
            }
        }
        self.inline_paragraph(&nodes[start..], result);
    }

    fn inline_paragraph(&self, nodes: &[Node], result: &mut Vec<Block>) {
        if let (Some(first), Some(last)) = (nodes.first(), nodes.last()) {
            result.push(Block {
                source: self.raw(&(first.range.start..last.range.end)),
                kind: BlockKind::Paragraph(self.inlines(nodes)),
            });
        }
    }

    fn inlines(&self, nodes: &[Node]) -> Vec<Inline> {
        let mut result = Vec::new();
        for node in nodes {
            let inline = match &node.kind {
                NodeKind::Text(text) => Inline::Text(text.clone()),
                NodeKind::Code(code) => Inline::Code(code.clone()),
                NodeKind::Math { source, .. } => Inline::Math {
                    source: source.clone(),
                },
                NodeKind::SoftBreak => Inline::SoftBreak,
                NodeKind::HardBreak => Inline::HardBreak,
                NodeKind::Container(Tag::Strong) => Inline::Strong(self.inlines(&node.children)),
                NodeKind::Container(Tag::Emphasis) => {
                    Inline::Emphasis(self.inlines(&node.children))
                }
                NodeKind::Container(Tag::Strikethrough) => {
                    Inline::Strikethrough(self.inlines(&node.children))
                }
                NodeKind::Container(Tag::Link { dest_url, .. }) => Inline::Link {
                    destination: dest_url.to_string(),
                    content: self.inlines(&node.children),
                },
                // 原始 HTML 和图片语法作为文字显示；解析层不执行或获取外部资源。
                _ => Inline::Text(self.raw(&node.range)),
            };
            if let Inline::Text(text) = inline {
                if let Some(Inline::Text(previous)) = result.last_mut() {
                    previous.push_str(&text);
                } else {
                    result.push(Inline::Text(text));
                }
            } else {
                result.push(inline);
            }
        }
        result
    }

    fn table(&mut self, node: &Node, alignments: &[pulldown_cmark::Alignment]) -> Block {
        let row_nodes: Vec<&Node> = node
            .children
            .iter()
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::Container(Tag::TableHead | Tag::TableRow)
                )
            })
            .collect();
        let cells: usize = row_nodes.iter().map(|row| row.children.len()).sum();
        let exceeds = alignments.len() > MAX_TABLE_COLUMNS
            || row_nodes.len() > MAX_TABLE_ROWS
            || self.table_cells.saturating_add(cells) > MAX_TABLE_CELLS;
        if exceeds {
            self.diagnostics.push(Diagnostic {
                range: node.range.clone(),
                message: tr!(
                    "表格超过呈现上限，已保留完整源文",
                    "The table exceeds display limits; the complete source is preserved"
                )
                .into(),
            });
            return Block {
                source: self.raw(&node.range),
                kind: BlockKind::Paragraph(vec![Inline::Text(self.raw(&node.range))]),
            };
        }
        self.table_cells += cells;
        let rows = row_nodes
            .iter()
            .map(|row| {
                row.children
                    .iter()
                    .map(|cell| self.inlines(&cell.children))
                    .collect()
            })
            .collect();
        let alignments = alignments
            .iter()
            .map(|alignment| match alignment {
                pulldown_cmark::Alignment::None => Alignment::None,
                pulldown_cmark::Alignment::Left => Alignment::Left,
                pulldown_cmark::Alignment::Center => Alignment::Center,
                pulldown_cmark::Alignment::Right => Alignment::Right,
            })
            .collect();
        Block {
            source: self.raw(&node.range),
            kind: BlockKind::Table { alignments, rows },
        }
    }
}

fn is_block(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Container(
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::List(_)
                | Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::Table(_)
                | Tag::HtmlBlock
        ) | NodeKind::Math { display: true, .. }
            | NodeKind::Rule
    )
}

fn bare_latex(source: &str, nodes: &[Node]) -> bool {
    let source = source.trim();
    if source.is_empty()
        || nodes.len() != 1
        || !matches!(nodes[0].kind, NodeKind::Container(Tag::Paragraph))
    {
        return false;
    }
    // 有明确 LaTeX 命令开头的旧式结果（包括 \text{中文}）可以直接交给数学层。
    if source.starts_with('\\')
        && source
            .as_bytes()
            .get(1)
            .is_some_and(u8::is_ascii_alphabetic)
    {
        return true;
    }
    // 普通自然语言不因 formula_mode 变成数学。无定界的非 ASCII 段落保留正文。
    if !source.is_ascii() {
        return false;
    }
    let mut word = 0usize;
    let mut command = false;
    for byte in source.bytes() {
        if byte == b'\\' {
            command = true;
            word = 0;
        } else if byte.is_ascii_alphabetic() {
            if !command {
                word += 1;
                if word > 2 {
                    return false;
                }
            }
        } else {
            command = false;
            word = 0;
        }
    }
    source
        .chars()
        .any(|character| "=+-*/_^<>(){}\\".contains(character))
        || source.len() <= 2 && source.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
#[path = "markdown_document_tests.rs"]
mod tests;
