//! Markdown 的有界本机图片导出；使用系统字形排版和现有数学栅格，不启动外部程序。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    markdown_document::{Alignment, Block, BlockKind, Document, Inline},
    text_format::FontFamily,
};
use std::{collections::HashMap, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

const PAD: f32 = 20.;
const MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 24_000_000;
const MAX_CACHE_PIXELS: u64 = 12_000_000;
const MAX_COMMANDS: usize = 32_768;

/// 宽度含外围留白，表格可以向右扩展；过大／坏公式明确报错，不导出残缺内容。
pub(crate) fn export_image(
    document: &Document,
    width: u32,
    font_size: f32,
) -> Result<Image, String> {
    if !(160..=4096).contains(&width)
        || !font_size.is_finite()
        || !(10.0..=64.0).contains(&font_size)
    {
        return Err(tr!(
            "文档导出宽度或字号无效",
            "Invalid document export width or font size"
        )
        .into());
    }
    if document.source.len() > crate::markdown_document::MAX_SOURCE_BYTES {
        return Err(too_large());
    }
    if let Some(diagnostic) = document.diagnostics.first() {
        return Err(trf!(
            "文档待校正：{}",
            "Document needs correction: {}",
            diagnostic.message
        ));
    }
    if let Some(source) = single_math(document) {
        // 单独公式保留原来的紧致图片尺寸；仅混排文档使用段落宽度。
        return crate::formula_layout::render(source, false, font_size);
    }
    let mut renderer = Renderer {
        cache: HashMap::new(),
        cache_pixels: 0,
    };
    let plan = renderer.blocks(&document.blocks, width as f32 - PAD * 2., font_size, 0)?;
    let output_width = (plan.width + PAD * 2.).ceil().max(width as f32) as u32;
    let output_height = (plan.height + PAD * 2.).ceil().max(1.) as u32;
    checked_size(output_width, output_height)?;
    if plan.commands.len() > MAX_COMMANDS {
        return Err(too_large());
    }
    let mut output = Image {
        width: output_width,
        height: output_height,
        bgra: vec![255; output_width as usize * output_height as usize * 4],
        readback_ms: 0.,
    };
    let canvas = Rect::full(output_width, output_height);
    for command in plan.commands {
        match command {
            Command::Bitmap { x, y, raster } => {
                let rect = Rect {
                    x: (x + PAD + raster.dx).round().max(0.) as u32,
                    y: (y + PAD + raster.dy).round().max(0.) as u32,
                    width: raster.image.width,
                    height: raster.image.height,
                };
                crate::raster::composite(&mut output, canvas, &raster.image, rect);
            }
            Command::Fill {
                x,
                y,
                width,
                height,
            } => {
                let left = (x + PAD).round().max(0.) as u32;
                let top = (y + PAD).round().max(0.) as u32;
                let right = (left + width.ceil().max(1.) as u32).min(output.width);
                let bottom = (top + height.ceil().max(1.) as u32).min(output.height);
                for y in top..bottom {
                    for x in left..right {
                        let pixel = ((y * output.width + x) * 4) as usize;
                        output.bgra[pixel..pixel + 4].copy_from_slice(&[0, 0, 0, 255]);
                    }
                }
            }
        }
    }
    Ok(output)
}

fn single_math(document: &Document) -> Option<&str> {
    let mut found = None;
    for block in &document.blocks {
        match &block.kind {
            BlockKind::Math { source } if found.is_none() => found = Some(source.as_str()),
            BlockKind::Paragraph(items) => {
                for item in items {
                    match item {
                        Inline::Math { source } if found.is_none() => found = Some(source.as_str()),
                        Inline::Text(text) if text.trim().is_empty() => {}
                        Inline::SoftBreak | Inline::HardBreak => {}
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    found
}

fn too_large() -> String {
    tr!(
        "文档图片尺寸或复杂度超过上限，请分段复制",
        "Document image exceeds size or complexity limits; copy a smaller section"
    )
    .into()
}

fn checked_size(width: u32, height: u32) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(too_large());
    }
    Ok(())
}

#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]
struct TextStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

#[derive(Hash, PartialEq, Eq)]
struct Key {
    source: String,
    size: u32,
    style: TextStyle,
    math: bool,
}

struct Raster {
    image: Image,
    width: f32,
    height: f32,
    dx: f32,
    dy: f32,
}

enum Piece {
    Raster(Arc<Raster>),
    Break,
}

enum Command {
    Bitmap {
        x: f32,
        y: f32,
        raster: Arc<Raster>,
    },
    Fill {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
}

impl Command {
    fn translate(&mut self, dx: f32, dy: f32) {
        match self {
            Self::Bitmap { x, y, .. } | Self::Fill { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
    }
}

struct Plan {
    width: f32,
    height: f32,
    commands: Vec<Command>,
}

impl Plan {
    fn empty(width: f32) -> Self {
        Self {
            width,
            height: 0.,
            commands: vec![],
        }
    }
    fn append(&mut self, mut other: Plan, x: f32, y: f32) -> Result<(), String> {
        self.width = self.width.max(x + other.width);
        self.height = self.height.max(y + other.height);
        if !self.width.is_finite()
            || !self.height.is_finite()
            || self.width > MAX_SIDE as f32 - PAD * 2.
            || self.height > MAX_SIDE as f32 - PAD * 2.
            || self.commands.len() + other.commands.len() > MAX_COMMANDS
        {
            return Err(too_large());
        }
        for command in &mut other.commands {
            command.translate(x, y);
        }
        self.commands.extend(other.commands);
        Ok(())
    }
}

struct Renderer {
    cache: HashMap<Key, Arc<Raster>>,
    cache_pixels: u64,
}

impl Renderer {
    fn remember(&mut self, key: Key, raster: Raster) -> Result<Arc<Raster>, String> {
        self.cache_pixels += u64::from(raster.image.width) * u64::from(raster.image.height);
        if self.cache_pixels > MAX_CACHE_PIXELS {
            return Err(too_large());
        }
        let raster = Arc::new(raster);
        self.cache.insert(key, raster.clone());
        Ok(raster)
    }

    fn text(&mut self, text: &str, size: f32, style: TextStyle) -> Result<Arc<Raster>, String> {
        let key = Key {
            source: text.into(),
            size: size.to_bits(),
            style,
            math: false,
        };
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }
        let mut native = Style {
            color: 0,
            width: 0.,
            font_size: size,
            bold: style.bold,
            ..Style::default()
        };
        native.text.italic = style.italic;
        native.text.strike = style.strike;
        native.text.underline = style.link;
        if style.code {
            native.text.font = FontFamily::named("Consolas");
        }
        let mut annotation = Annotation::new(Tool::Text, Point { x: 3., y: 3. }, native);
        annotation.text = text.into();
        annotation.update(Point {
            x: 4099.,
            y: size * 4. + 3.,
        });
        let layout = crate::text_layout::TextLayout::new(&annotation)
            .map_err(|_| tr!("文档文字排版失败", "Document text layout failed").to_owned())?;
        let width = layout
            .rows
            .iter()
            .map(|row| row.width)
            .fold(0_f32, f32::max);
        let height = layout.height.max(size);
        if layout.rows.len() != 1 || width > 4096. {
            return Err(too_large());
        }
        let (image_width, image_height) = (
            (width + 8.).ceil().max(1.) as u32,
            (height + 8.).ceil() as u32,
        );
        checked_size(image_width, image_height)?;
        annotation.update(Point {
            x: 3. + width.max(1.) + 1.,
            y: 3. + height,
        });
        drop(layout);
        let (rect, image) = crate::raster::text(&annotation, Rect::full(image_width, image_height))
            .map_err(|_| tr!("文档文字绘制失败", "Document text drawing failed").to_owned())?
            .ok_or_else(|| {
                tr!(
                    "文档文字没有可绘制区域",
                    "Document text has no drawable area"
                )
                .to_owned()
            })?;
        self.remember(
            key,
            Raster {
                image,
                width,
                height,
                dx: rect.x as f32 - 3.,
                dy: rect.y as f32 - 3.,
            },
        )
    }

    fn math(&mut self, source: &str, size: f32) -> Result<Arc<Raster>, String> {
        let key = Key {
            source: source.into(),
            size: size.to_bits(),
            style: TextStyle::default(),
            math: true,
        };
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }
        let normalized = crate::formula::normalize_latex(source)?;
        let image = crate::markdown_view::trim_math(crate::formula_layout::render(
            &normalized,
            false,
            size,
        )?)?;
        let (width, height) = (image.width as f32, image.height as f32);
        self.remember(
            key,
            Raster {
                image,
                width,
                height,
                dx: 0.,
                dy: 0.,
            },
        )
    }

    fn text_pieces(
        &mut self,
        text: &str,
        size: f32,
        style: TextStyle,
        available: f32,
        output: &mut Vec<Piece>,
    ) -> Result<(), String> {
        let normalized = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        for (line, text) in normalized.split('\n').enumerate() {
            if line > 0 {
                output.push(Piece::Break);
            }
            for word in text.split_word_bounds() {
                let glyphs: Vec<&str> = word.graphemes(true).collect();
                // 先限测量块，超长标识符不会在到达 grapheme 换行前撞上原生行宽上限。
                for chunk in glyphs.chunks(32) {
                    let word = chunk.concat();
                    // 单词只在确实超宽时拆到 grapheme，组合字和 emoji 不拆开。
                    let raster = self.text(&word, size, style)?;
                    if raster.width <= available {
                        output.push(Piece::Raster(raster));
                    } else {
                        for glyph in word.graphemes(true) {
                            output.push(Piece::Raster(self.text(glyph, size, style)?));
                        }
                    }
                    if output.len() > MAX_COMMANDS {
                        return Err(too_large());
                    }
                }
            }
        }
        Ok(())
    }

    fn inline(
        &mut self,
        items: &[Inline],
        size: f32,
        style: TextStyle,
        available: f32,
        depth: usize,
        output: &mut Vec<Piece>,
    ) -> Result<(), String> {
        if depth > 32 {
            return Err(too_large());
        }
        for item in items {
            match item {
                Inline::Text(text) => self.text_pieces(text, size, style, available, output)?,
                Inline::Code(text) => self.text_pieces(
                    text,
                    size,
                    TextStyle {
                        code: true,
                        ..style
                    },
                    available,
                    output,
                )?,
                Inline::Strong(children) => self.inline(
                    children,
                    size,
                    TextStyle {
                        bold: true,
                        ..style
                    },
                    available,
                    depth + 1,
                    output,
                )?,
                Inline::Emphasis(children) => self.inline(
                    children,
                    size,
                    TextStyle {
                        italic: true,
                        ..style
                    },
                    available,
                    depth + 1,
                    output,
                )?,
                Inline::Strikethrough(children) => self.inline(
                    children,
                    size,
                    TextStyle {
                        strike: true,
                        ..style
                    },
                    available,
                    depth + 1,
                    output,
                )?,
                Inline::Link { content, .. } => self.inline(
                    content,
                    size,
                    TextStyle {
                        link: true,
                        ..style
                    },
                    available,
                    depth + 1,
                    output,
                )?,
                Inline::Math { source } => output.push(Piece::Raster(self.math(source, size)?)),
                Inline::SoftBreak => self.text_pieces(" ", size, style, available, output)?,
                Inline::HardBreak => output.push(Piece::Break),
            }
            if output.len() > MAX_COMMANDS {
                return Err(too_large());
            }
        }
        Ok(())
    }

    fn inline_plan(
        &mut self,
        items: &[Inline],
        size: f32,
        bold: bool,
        width: f32,
        alignment: Alignment,
    ) -> Result<Plan, String> {
        let mut pieces = vec![];
        self.inline(
            items,
            size,
            TextStyle {
                bold,
                ..Default::default()
            },
            width,
            0,
            &mut pieces,
        )?;
        flow(pieces, width, size, alignment)
    }

    fn blocks(
        &mut self,
        blocks: &[Block],
        width: f32,
        size: f32,
        depth: usize,
    ) -> Result<Plan, String> {
        if depth > 16 || width < 24. {
            return Err(too_large());
        }
        let mut plan = Plan::empty(width);
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                plan.height += size * 0.65;
            }
            let child = self.block(block, width, size, depth)?;
            let y = plan.height;
            plan.append(child, 0., y)?;
        }
        Ok(plan)
    }

    fn block(
        &mut self,
        block: &Block,
        width: f32,
        size: f32,
        depth: usize,
    ) -> Result<Plan, String> {
        match &block.kind {
            BlockKind::Paragraph(items) => {
                self.inline_plan(items, size, false, width, Alignment::Left)
            }
            BlockKind::Heading { level, content } => {
                let scale = match level {
                    1 => 1.55,
                    2 => 1.35,
                    3 => 1.18,
                    _ => 1.05,
                };
                self.inline_plan(content, size * scale, true, width, Alignment::Left)
            }
            BlockKind::Math { source } => flow(
                vec![Piece::Raster(self.math(source, size * 1.15)?)],
                width,
                size,
                Alignment::Left,
            ),
            BlockKind::Code { language, code } => {
                let mut pieces = vec![];
                if let Some(language) = language {
                    self.text_pieces(
                        language,
                        size * 0.8,
                        TextStyle {
                            bold: true,
                            code: true,
                            ..Default::default()
                        },
                        width - 16.,
                        &mut pieces,
                    )?;
                    pieces.push(Piece::Break);
                }
                self.text_pieces(
                    code,
                    size * 0.92,
                    TextStyle {
                        code: true,
                        ..Default::default()
                    },
                    width - 16.,
                    &mut pieces,
                )?;
                let inside = flow(pieces, width - 16., size, Alignment::Left)?;
                let mut plan = Plan::empty(width);
                plan.append(inside, 8., 8.)?;
                plan.height += 8.;
                plan.commands.push(Command::Fill {
                    x: 0.,
                    y: 0.,
                    width: 1.,
                    height: plan.height,
                });
                Ok(plan)
            }
            BlockKind::Quote(blocks) => {
                let inside = self.blocks(blocks, width - 14., size, depth + 1)?;
                let mut plan = Plan::empty(width);
                plan.append(inside, 14., 0.)?;
                plan.commands.push(Command::Fill {
                    x: 0.,
                    y: 0.,
                    width: 2.,
                    height: plan.height,
                });
                Ok(plan)
            }
            BlockKind::List { start, items } => {
                let mut plan = Plan::empty(width);
                for (index, item) in items.iter().enumerate() {
                    let marker = start.map_or_else(
                        || "•".to_owned(),
                        |first| format!("{}.", first.saturating_add(index as u64)),
                    );
                    let marker = self.text(&marker, size, TextStyle::default())?;
                    let indent = marker.width.max(20.) + 10.;
                    let body = self.blocks(item, width - indent, size, depth + 1)?;
                    let y = plan.height;
                    let marker_height = marker.height;
                    plan.commands.push(Command::Bitmap {
                        x: 0.,
                        y,
                        raster: marker,
                    });
                    plan.append(body, indent, y)?;
                    plan.height = plan.height.max(y + marker_height) + size * 0.25;
                }
                Ok(plan)
            }
            BlockKind::Table { alignments, rows } => self.table(alignments, rows, size),
            BlockKind::Rule => Ok(Plan {
                width,
                height: 4.,
                commands: vec![Command::Fill {
                    x: 0.,
                    y: 1.,
                    width,
                    height: 1.,
                }],
            }),
        }
    }

    fn table(
        &mut self,
        alignments: &[Alignment],
        rows: &[Vec<Vec<Inline>>],
        size: f32,
    ) -> Result<Plan, String> {
        let columns = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(alignments.len());
        if rows.is_empty()
            || columns == 0
            || columns > 32
            || rows.len() > 512
            || rows.len().saturating_mul(columns) > 4096
        {
            return Err(too_large());
        }
        let mut widths = vec![80_f32; columns];
        for (row_index, row) in rows.iter().enumerate() {
            for (column, cell) in row.iter().enumerate() {
                let mut pieces = vec![];
                self.inline(
                    cell,
                    size,
                    TextStyle {
                        bold: row_index == 0,
                        ..Default::default()
                    },
                    4096.,
                    0,
                    &mut pieces,
                )?;
                let mut line: f32 = 0.;
                let mut widest: f32 = 0.;
                for piece in pieces {
                    match piece {
                        Piece::Break => {
                            widest = widest.max(line);
                            line = 0.;
                        }
                        Piece::Raster(raster) => line += raster.width,
                    }
                }
                widths[column] = widths[column].max((widest.max(line) + 16.).min(360.));
            }
        }
        let total: f32 = widths.iter().sum();
        let max = MAX_SIDE as f32 - PAD * 2. - 1.;
        if total > max {
            let minimum = columns as f32 * 80.;
            let ratio = (max - minimum) / (total - minimum);
            for width in &mut widths {
                *width = 80. + (*width - 80.) * ratio;
            }
        }
        let width: f32 = widths.iter().sum();
        let mut plan = Plan::empty(width + 1.);
        for (row_index, row) in rows.iter().enumerate() {
            let mut cells = Vec::new();
            let mut height = size * 1.55 + 16.;
            for (column, width) in widths.iter().enumerate() {
                let empty = vec![];
                let cell = row.get(column).unwrap_or(&empty);
                let child = self.inline_plan(
                    cell,
                    size,
                    row_index == 0,
                    width - 16.,
                    alignments.get(column).copied().unwrap_or(Alignment::Left),
                )?;
                height = height.max(child.height + 16.);
                cells.push(child);
            }
            let y = plan.height;
            let mut x = 0.;
            plan.commands.push(Command::Fill {
                x: 0.,
                y,
                width: width + 1.,
                height: 1.,
            });
            for (column, cell) in cells.into_iter().enumerate() {
                plan.append(cell, x + 8., y + 8.)?;
                x += widths[column];
            }
            plan.height = y + height;
        }
        let height = plan.height;
        plan.commands.push(Command::Fill {
            x: 0.,
            y: height,
            width: width + 1.,
            height: 1.,
        });
        let mut x = 0.;
        for column in widths {
            plan.commands.push(Command::Fill {
                x,
                y: 0.,
                width: 1.,
                height: height + 1.,
            });
            x += column;
        }
        plan.commands.push(Command::Fill {
            x: width,
            y: 0.,
            width: 1.,
            height: height + 1.,
        });
        plan.height += 1.;
        Ok(plan)
    }
}

fn flow(pieces: Vec<Piece>, width: f32, size: f32, alignment: Alignment) -> Result<Plan, String> {
    if width < 1. || !width.is_finite() {
        return Err(too_large());
    }
    let mut plan = Plan::empty(width);
    let mut line = vec![];
    let mut used = 0.;
    let mut height = size * 1.55;
    let flush = |plan: &mut Plan, line: &mut Vec<Arc<Raster>>, used: f32, height: f32| {
        let mut x = match alignment {
            Alignment::Center => (width - used).max(0.) / 2.,
            Alignment::Right => (width - used).max(0.),
            _ => 0.,
        };
        for raster in line.drain(..) {
            plan.commands.push(Command::Bitmap {
                x,
                y: plan.height + (height - raster.height) / 2.,
                raster: raster.clone(),
            });
            x += raster.width;
        }
        plan.height += height;
    };
    for piece in pieces {
        match piece {
            Piece::Break => {
                flush(&mut plan, &mut line, used, height);
                used = 0.;
                height = size * 1.55;
            }
            Piece::Raster(raster) => {
                let raster = if raster.width > width {
                    scale_raster(raster, width)?
                } else {
                    raster
                };
                if used + raster.width > width + 0.1 && !line.is_empty() {
                    flush(&mut plan, &mut line, used, height);
                    used = 0.;
                    height = size * 1.55;
                }
                used += raster.width;
                height = height.max(raster.height);
                line.push(raster);
            }
        }
        if plan.height > MAX_SIDE as f32 - PAD * 2. || plan.commands.len() > MAX_COMMANDS {
            return Err(too_large());
        }
    }
    if !line.is_empty() || plan.height == 0. {
        flush(&mut plan, &mut line, used, height);
    }
    Ok(plan)
}

fn scale_raster(raster: Arc<Raster>, width: f32) -> Result<Arc<Raster>, String> {
    let scale = width / raster.width;
    let target_width = (raster.image.width as f32 * scale).round().max(1.) as u32;
    let target_height = (raster.image.height as f32 * scale).round().max(1.) as u32;
    checked_size(target_width, target_height)?;
    let source = image::RgbaImage::from_raw(
        raster.image.width,
        raster.image.height,
        raster.image.bgra.clone(),
    )
    .ok_or_else(too_large)?;
    let scaled = image::imageops::resize(
        &source,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    Ok(Arc::new(Raster {
        image: Image {
            width: target_width,
            height: target_height,
            bgra: scaled.into_raw(),
            readback_ms: 0.,
        },
        width,
        height: raster.height * scale,
        dx: raster.dx * scale,
        dy: raster.dy * scale,
    }))
}

#[cfg(test)]
#[path = "markdown_export_tests.rs"]
mod tests;
