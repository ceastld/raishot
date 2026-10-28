//! API 文档的原生 Markdown 预览；解析与数学栅格化不进入逐帧绘制。
use crate::{
    capture::Image,
    capture_toolbar::Palette,
    markdown_document::{Alignment, Block, BlockKind, Document, Inline},
};
use gpui::{div, img, prelude::*, px, rgb, AnyElement, Div, FontWeight, ObjectFit, RenderImage};
use std::{collections::HashMap, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

const MAX_ATOMS: usize = 24_000;
const MAX_MATH: usize = 128;
const MAX_MATH_PIXELS: u64 = 12_000_000;
const MAX_COLUMNS: usize = 32;
const MAX_ROWS: usize = 512;
const MAX_TABLE_WIDTH: f32 = 4096.;

/// 可在后台线程创建。宿主持有并按 source、主题、字号替换；不要在 Render 中调用 prepare。
#[derive(Clone)]
pub(crate) struct Prepared {
    pub source: String,
    pub dark: bool,
    pub font_size: f32,
    pub diagnostics: Vec<String>,
    blocks: Vec<PreparedBlock>,
    images: Vec<Arc<RenderImage>>,
}

impl Prepared {
    pub fn matches(&self, source: &str, dark: bool, font_size: f32) -> bool {
        self.source == source && self.dark == dark && self.font_size == bounded_font(font_size)
    }

    /// 替换缓存后由宿主在合适的绘制轮次调用 Window::drop_image，避免图片 atlas 常驻增长。
    pub fn images(&self) -> impl Iterator<Item = Arc<RenderImage>> + '_ {
        self.images.iter().cloned()
    }
}

#[derive(Clone)]
enum PreparedBlock {
    Paragraph(Vec<Atom>),
    Heading(u8, Vec<Atom>),
    List(Option<u64>, Vec<Vec<PreparedBlock>>),
    Quote(Vec<PreparedBlock>),
    Code(Option<String>, String),
    Math(Math),
    Table(Table),
    Rule,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct InlineStyle {
    strong: bool,
    emphasis: bool,
    strike: bool,
    code: bool,
    link: bool,
    error: bool,
}

#[derive(Clone)]
enum Atom {
    Text(String, InlineStyle),
    Math(Math),
    Break,
}

#[derive(Clone)]
struct Math {
    source: String,
    image: Option<Arc<RenderImage>>,
    width: f32,
    height: f32,
    error: Option<String>,
}

#[derive(Clone)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Atom>>>,
    widths: Vec<f32>,
}

struct Builder<F> {
    dark: bool,
    font_size: f32,
    diagnostics: Vec<String>,
    cache: HashMap<(String, u32), Math>,
    images: Vec<Arc<RenderImage>>,
    atoms: usize,
    pixels: u64,
    rasterize: F,
}

/// 只做 CPU 工作；公式错误局部回退，不让一段坏公式清空整个文档。
pub(crate) fn prepare(document: &Document, dark: bool, font_size: f32) -> Prepared {
    prepare_with(document, dark, font_size, crate::formula_layout::render)
}

fn bounded_font(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(10., 48.)
    } else {
        16.
    }
}

fn prepare_with<F>(document: &Document, dark: bool, font_size: f32, rasterize: F) -> Prepared
where
    F: FnMut(&str, bool, f32) -> Result<Image, String>,
{
    let mut builder = Builder {
        dark,
        font_size: bounded_font(font_size),
        diagnostics: document
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect(),
        cache: HashMap::new(),
        images: Vec::new(),
        atoms: 0,
        pixels: 0,
        rasterize,
    };
    let blocks = builder.blocks(&document.blocks, 0);
    Prepared {
        source: document.source.clone(),
        dark,
        font_size: builder.font_size,
        diagnostics: builder.diagnostics,
        blocks,
        images: builder.images,
    }
}

impl<F: FnMut(&str, bool, f32) -> Result<Image, String>> Builder<F> {
    fn blocks(&mut self, blocks: &[Block], depth: usize) -> Vec<PreparedBlock> {
        blocks
            .iter()
            .map(|block| {
                if depth > 16 || self.atoms >= MAX_ATOMS {
                    self.diagnostics.push(
                        tr!(
                            "文档过于复杂，部分内容以源码显示",
                            "Complex document; some content is shown as source"
                        )
                        .into(),
                    );
                    return PreparedBlock::Code(None, block.source.clone());
                }
                let prepared = match &block.kind {
                    BlockKind::Paragraph(inline) => PreparedBlock::Paragraph(self.inline(
                        inline,
                        InlineStyle::default(),
                        self.font_size,
                        0,
                    )),
                    BlockKind::Heading { level, content } => {
                        let size = heading_size(self.font_size, *level);
                        PreparedBlock::Heading(
                            *level,
                            self.inline(
                                content,
                                InlineStyle {
                                    strong: true,
                                    ..Default::default()
                                },
                                size,
                                0,
                            ),
                        )
                    }
                    BlockKind::List { start, items } => PreparedBlock::List(
                        *start,
                        items
                            .iter()
                            .map(|item| self.blocks(item, depth + 1))
                            .collect(),
                    ),
                    BlockKind::Quote(children) => {
                        PreparedBlock::Quote(self.blocks(children, depth + 1))
                    }
                    BlockKind::Code { language, code } => {
                        PreparedBlock::Code(language.clone(), code.clone())
                    }
                    BlockKind::Math { source } => {
                        PreparedBlock::Math(self.math(source, self.font_size * 1.15))
                    }
                    BlockKind::Table { alignments, rows } => {
                        let columns = rows
                            .iter()
                            .map(Vec::len)
                            .max()
                            .unwrap_or(0)
                            .max(alignments.len());
                        if columns == 0 || columns > MAX_COLUMNS || rows.len() > MAX_ROWS {
                            self.diagnostics.push(
                                tr!(
                                    "表格尺寸超过预览上限，已保留源码",
                                    "Table exceeds preview limits; the source is preserved"
                                )
                                .into(),
                            );
                            return PreparedBlock::Code(None, block.source.clone());
                        }
                        let mut rows: Vec<Vec<Vec<Atom>>> = rows
                            .iter()
                            .map(|row| {
                                (0..columns)
                                    .map(|column| {
                                        self.inline(
                                            row.get(column).map_or(&[], Vec::as_slice),
                                            InlineStyle::default(),
                                            self.font_size,
                                            0,
                                        )
                                    })
                                    .collect()
                            })
                            .collect();
                        let widths = table_widths(&rows, columns, self.font_size);
                        for row in &mut rows {
                            for (column, cell) in row.iter_mut().enumerate() {
                                let before = cell.len();
                                split_cell_text(cell, widths[column] - 20., self.font_size);
                                self.atoms += cell.len().saturating_sub(before);
                            }
                        }
                        PreparedBlock::Table(Table {
                            alignments: alignments.clone(),
                            rows,
                            widths,
                        })
                    }
                    BlockKind::Rule => PreparedBlock::Rule,
                };
                if self.atoms > MAX_ATOMS {
                    self.diagnostics.push(
                        tr!(
                            "文档过于复杂，部分内容以源码显示",
                            "Complex document; some content is shown as source"
                        )
                        .into(),
                    );
                    PreparedBlock::Code(None, block.source.clone())
                } else {
                    prepared
                }
            })
            .collect()
    }

    fn inline(
        &mut self,
        inline: &[Inline],
        style: InlineStyle,
        size: f32,
        depth: usize,
    ) -> Vec<Atom> {
        let mut output = Vec::new();
        for item in inline {
            if depth > 32 {
                self.atoms = MAX_ATOMS + 1;
                break;
            }
            if self.atoms > MAX_ATOMS {
                break;
            }
            match item {
                Inline::Text(text) => self.text(text, style, &mut output),
                Inline::Strong(items) => output.extend(self.inline(
                    items,
                    InlineStyle {
                        strong: true,
                        ..style
                    },
                    size,
                    depth + 1,
                )),
                Inline::Emphasis(items) => output.extend(self.inline(
                    items,
                    InlineStyle {
                        emphasis: true,
                        ..style
                    },
                    size,
                    depth + 1,
                )),
                Inline::Strikethrough(items) => output.extend(self.inline(
                    items,
                    InlineStyle {
                        strike: true,
                        ..style
                    },
                    size,
                    depth + 1,
                )),
                Inline::Code(text) => self.text(
                    text,
                    InlineStyle {
                        code: true,
                        ..style
                    },
                    &mut output,
                ),
                // 链接仅显示文字，不自动联网或装入外部图片。
                Inline::Link { content, .. } => output.extend(self.inline(
                    content,
                    InlineStyle {
                        link: true,
                        ..style
                    },
                    size,
                    depth + 1,
                )),
                Inline::Math { source } => {
                    self.atoms += 1;
                    output.push(Atom::Math(self.math(source, size)));
                }
                Inline::SoftBreak => self.text(" ", style, &mut output),
                Inline::HardBreak => output.push(Atom::Break),
            }
        }
        output
    }

    fn text(&mut self, text: &str, style: InlineStyle, output: &mut Vec<Atom>) {
        if self.atoms >= MAX_ATOMS {
            output.push(Atom::Text(text.to_owned(), style));
            return;
        }
        // Unicode 词边界保留空白；长标识符分段以免窄面板横向撑开正文。
        for (offset, word) in text.split_word_bound_indices() {
            if self.atoms >= MAX_ATOMS {
                output.push(Atom::Text(text[offset..].to_owned(), style));
                self.atoms += 1;
                break;
            }
            let mut token = String::new();
            let mut count = 0;
            for glyph in word.graphemes(true) {
                token.push_str(glyph);
                count += 1;
                if count == 16 {
                    output.push(Atom::Text(std::mem::take(&mut token), style));
                    self.atoms += 1;
                    count = 0;
                }
            }
            if !token.is_empty() {
                output.push(Atom::Text(token, style));
                self.atoms += 1;
            }
        }
    }

    fn math(&mut self, source: &str, size: f32) -> Math {
        let key = (source.to_owned(), size.to_bits());
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let rendered = if self.cache.len() >= MAX_MATH || self.pixels >= MAX_MATH_PIXELS {
            Err(tr!(
                "公式数量或预览像素超过上限",
                "Formula count or preview pixels exceed the limit"
            )
            .into())
        } else {
            crate::formula::normalize_latex(source)
                .and_then(|latex| (self.rasterize)(&latex, self.dark, size))
                .and_then(trim_math)
        };
        let math = match rendered {
            Ok(image)
                if self.pixels + u64::from(image.width) * u64::from(image.height)
                    <= MAX_MATH_PIXELS =>
            {
                self.pixels += u64::from(image.width) * u64::from(image.height);
                let (width, height) = (image.width as f32, image.height as f32);
                let image =
                    crate::canvas_source::render_image(image.width, image.height, image.bgra);
                self.images.push(image.clone());
                Math {
                    source: source.into(),
                    image: Some(image),
                    width,
                    height,
                    error: None,
                }
            }
            result => {
                let error = match result {
                    Err(error) => error,
                    Ok(_) => tr!(
                        "公式预览像素超过上限",
                        "Formula preview pixels exceed the limit"
                    )
                    .into(),
                };
                self.diagnostics.push(error.clone());
                Math {
                    source: source.into(),
                    image: None,
                    width: 0.,
                    height: 0.,
                    error: Some(error),
                }
            }
        };
        self.cache.insert(key, math.clone());
        math
    }
}

/// 移除公式预览的外围留白和纯底色，混排时不出现亮暗矩形；不改公式字形。
pub(crate) fn trim_math(image: Image) -> Result<Image, String> {
    let expected = u64::from(image.width)
        .checked_mul(u64::from(image.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(u64::MAX);
    if expected == 0 || expected > MAX_MATH_PIXELS * 4 || image.bgra.len() as u64 != expected {
        return Err(tr!("公式预览尺寸无效", "Invalid formula preview dimensions").into());
    }
    let bg = &image.bgra[..4];
    let blank = |pixel: &[u8]| pixel[3] == 0 || (0..3).all(|i| pixel[i].abs_diff(bg[i]) <= 1);
    let (mut left, mut top, mut right, mut bottom) = (image.width, image.height, 0, 0);
    for (index, pixel) in image.bgra.chunks_exact(4).enumerate() {
        if !blank(pixel) {
            let x = index as u32 % image.width;
            let y = index as u32 / image.width;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    if right <= left || bottom <= top {
        return Err(tr!(
            "公式没有可见内容，已显示源码",
            "The formula has no visible content; showing source"
        )
        .into());
    }
    left = left.saturating_sub(2);
    top = top.saturating_sub(2);
    right = (right + 2).min(image.width);
    bottom = (bottom + 2).min(image.height);
    let (width, height) = (right - left, bottom - top);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in top..bottom {
        for x in left..right {
            let index = (y as usize * image.width as usize + x as usize) * 4;
            let pixel = &image.bgra[index..index + 4];
            if blank(pixel) {
                bgra.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                bgra.extend_from_slice(pixel);
            }
        }
    }
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

fn heading_size(size: f32, level: u8) -> f32 {
    size * match level {
        1 => 1.55,
        2 => 1.35,
        3 => 1.18,
        _ => 1.05,
    }
}

fn split_cell_text(atoms: &mut Vec<Atom>, width: f32, size: f32) {
    let count = (width / (size * 1.1)).floor().max(1.) as usize;
    *atoms = std::mem::take(atoms)
        .into_iter()
        .flat_map(|atom| match atom {
            Atom::Text(text, style) => text
                .graphemes(true)
                .collect::<Vec<_>>()
                .chunks(count)
                .map(|chunk| Atom::Text(chunk.concat(), style))
                .collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect();
}

fn atom_width(atom: &Atom, font_size: f32) -> f32 {
    match atom {
        Atom::Text(text, _) => text
            .graphemes(true)
            .map(|g| {
                if g.is_ascii() {
                    font_size * 0.56 * g.len() as f32
                } else {
                    font_size
                }
            })
            .sum(),
        Atom::Math(math) => math.width.min(360.),
        Atom::Break => 0.,
    }
}

fn table_widths(rows: &[Vec<Vec<Atom>>], columns: usize, font_size: f32) -> Vec<f32> {
    let mut widths = vec![80_f32; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            let mut line: f32 = 0.;
            let mut widest: f32 = 0.;
            for atom in cell {
                if matches!(atom, Atom::Break) {
                    widest = widest.max(line);
                    line = 0.;
                } else {
                    line += atom_width(atom, font_size);
                }
            }
            widths[column] = widths[column].max((widest.max(line) + 24.).clamp(80., 420.));
        }
    }
    let total: f32 = widths.iter().sum();
    if total > MAX_TABLE_WIDTH {
        let minimum = 80. * columns as f32;
        let ratio = (MAX_TABLE_WIDTH - minimum) / (total - minimum);
        for width in &mut widths {
            *width = 80. + (*width - 80.) * ratio;
        }
    }
    widths
}

/// 仅构建 GPUI 元素，所有数学与宽度来自 prepare 缓存。外层负责纵向滚动。
pub(crate) fn render(prepared: &Prepared, palette: Palette) -> Div {
    let mut next_id = 0;
    div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(prepared.font_size * 0.6))
        .text_size(px(prepared.font_size))
        .text_color(rgb(palette.foreground))
        .children(
            prepared
                .blocks
                .iter()
                .map(|block| render_block(block, prepared.font_size, palette, &mut next_id)),
        )
}

fn render_block(block: &PreparedBlock, size: f32, p: Palette, next_id: &mut usize) -> AnyElement {
    let index = *next_id;
    *next_id += 1;
    match block {
        PreparedBlock::Paragraph(atoms) => {
            inline_flow(atoms, size, p, Alignment::Left, None).into_any_element()
        }
        PreparedBlock::Heading(level, atoms) => {
            inline_flow(atoms, heading_size(size, *level), p, Alignment::Left, None)
                .font_weight(FontWeight::BOLD)
                .into_any_element()
        }
        PreparedBlock::List(start, items) => div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .children(items.iter().enumerate().map(|(number, blocks)| {
                let marker = start.map_or_else(
                    || "•".to_owned(),
                    |start| format!("{}.", start.saturating_add(number as u64)),
                );
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().flex_shrink_0().min_w(px(20.)).child(marker))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .children(
                                blocks
                                    .iter()
                                    .map(|block| render_block(block, size, p, next_id)),
                            ),
                    )
            }))
            .into_any_element(),
        PreparedBlock::Quote(blocks) => div()
            .border_l_2()
            .border_color(rgb(p.border))
            .pl(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(
                blocks
                    .iter()
                    .map(|block| render_block(block, size, p, next_id)),
            )
            .into_any_element(),
        PreparedBlock::Code(language, source) => div()
            .id(("markdown-code", index))
            .w_full()
            .min_w(px(0.))
            .overflow_x_scroll()
            .p(px(10.))
            .rounded_md()
            .bg(rgb(p.track))
            .font_family("Consolas")
            .text_size(px(size * 0.92))
            .children(language.iter().map(|name| {
                div()
                    .text_color(rgb(p.muted))
                    .text_size(px(11.))
                    .child(name.clone())
            }))
            .child(div().whitespace_nowrap().child(source.clone()))
            .into_any_element(),
        PreparedBlock::Math(math) => div()
            .id(("markdown-math", index))
            .w_full()
            .min_w(px(0.))
            .overflow_x_scroll()
            .py(px(4.))
            .child(math_element(math, 1200., size, p))
            .into_any_element(),
        PreparedBlock::Table(table) => {
            let width: f32 = table.widths.iter().sum();
            div()
                .id(("markdown-table", index))
                .w_full()
                .min_w(px(0.))
                .overflow_x_scroll()
                .child(
                    div()
                        .w(px(width))
                        .flex_shrink_0()
                        .border_t_1()
                        .border_l_1()
                        .border_color(rgb(p.border))
                        .flex()
                        .flex_col()
                        .children(table.rows.iter().enumerate().map(|(row_index, row)| {
                            div()
                                .flex()
                                .w(px(width))
                                .children(row.iter().enumerate().map(|(column, cell)| {
                                    div()
                                        .w(px(table.widths[column]))
                                        .flex_shrink_0()
                                        .border_r_1()
                                        .border_b_1()
                                        .border_color(rgb(p.border))
                                        .p(px(9.))
                                        .when(row_index == 0, |cell| {
                                            cell.bg(rgb(p.track)).font_weight(FontWeight::SEMIBOLD)
                                        })
                                        .child(inline_flow(
                                            cell,
                                            size,
                                            p,
                                            table
                                                .alignments
                                                .get(column)
                                                .copied()
                                                .unwrap_or(Alignment::Left),
                                            Some(table.widths[column] - 20.),
                                        ))
                                }))
                        })),
                )
                .into_any_element()
        }
        PreparedBlock::Rule => div()
            .h(px(1.))
            .w_full()
            .bg(rgb(p.border))
            .into_any_element(),
    }
}

fn inline_flow(
    atoms: &[Atom],
    size: f32,
    p: Palette,
    alignment: Alignment,
    available: Option<f32>,
) -> Div {
    let mut flow = div()
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_wrap()
        .items_center()
        .text_size(px(size))
        .line_height(px(size * 1.55));
    flow = match alignment {
        Alignment::Center => flow.justify_center(),
        Alignment::Right => flow.justify_end(),
        _ => flow,
    };
    flow.children(atoms.iter().map(|atom| {
        match atom {
            Atom::Break => div().w_full().h(px(0.)).into_any_element(),
            Atom::Math(math) => math_element(math, available.unwrap_or(360.), size, p),
            Atom::Text(text, style) => div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .child(text.clone())
                .when(style.strong, |d| d.font_weight(FontWeight::BOLD))
                .when(style.emphasis, |d| d.italic())
                .when(style.strike, |d| d.line_through())
                .when(style.code, |d| {
                    d.font_family("Consolas").bg(rgb(p.track)).rounded_sm()
                })
                .when(style.link, |d| d.text_color(rgb(p.accent)).underline())
                .when(style.error, |d| d.text_color(rgb(p.danger)))
                .into_any_element(),
        }
    }))
}

fn math_element(math: &Math, max_width: f32, size: f32, p: Palette) -> AnyElement {
    if let Some(image) = &math.image {
        let scale = (max_width / math.width.max(1.))
            .min(400. / math.height.max(1.))
            .min(1.);
        return div()
            .flex_shrink_0()
            .px(px(0.))
            .child(
                img(image.clone())
                    .w(px(math.width * scale))
                    .h(px(math.height * scale))
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element();
    }
    div()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .text_color(rgb(p.danger))
        .text_size(px(size * 0.9))
        .child(div().font_family("Consolas").child(math.source.clone()))
        .children(
            math.error
                .iter()
                .map(|error| div().text_size(px(size * 0.78)).child(error.clone())),
        )
        .into_any_element()
}

#[cfg(test)]
#[path = "markdown_view_tests.rs"]
mod tests;
