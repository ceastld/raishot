//! Uniscribe 统一字形排版、换行、光标和命中；无 GPUI 依赖。
use crate::{
    annotation::{Annotation, Point},
    text_format::{Alignment, FontContext},
    Result,
};
use std::{ffi::c_void, ops::Range};
use windows::Win32::{Globalization::*, Graphics::Gdi::*};
struct Shaped {
    raw: *mut c_void,
    len: usize,
    _utf: Vec<u16>,
    _native: crate::text_format::NativeTextLease,
}
impl Shaped {
    fn new(dc: HDC, text: &str) -> Result<Self> {
        let native = crate::text_format::native_text_lease();
        let mut utf: Vec<u16> = text.encode_utf16().collect();
        let len = utf.len();
        utf.push(0);
        let mut raw = std::ptr::null_mut();
        let result = unsafe {
            ScriptStringAnalyse(
                dc,
                utf.as_ptr().cast(),
                len as i32,
                (len * 2 + 16) as i32,
                -1,
                SSA_GLYPHS | SSA_FALLBACK | SSA_LINK | SSA_BREAK,
                1_000_000,
                None,
                None,
                None,
                None,
                std::ptr::null(),
                &mut raw,
            )
        };
        if let Err(e) = result {
            if !raw.is_null() {
                unsafe {
                    let _ = ScriptStringFree(&mut raw);
                }
            }
            return Err(e.into());
        }
        if raw.is_null() {
            return Err("原生文字分析返回空结果".into());
        }
        let valid = unsafe {
            !ScriptString_pSize(raw).is_null()
                && !ScriptString_pcOutChars(raw).is_null()
                && !ScriptString_pLogAttr(raw).is_null()
        };
        if !valid {
            unsafe {
                let _ = ScriptStringFree(&mut raw);
            }
            return Err("原生文字分析缺少字形或换行信息".into());
        }
        Ok(Self {
            raw,
            len,
            _utf: utf,
            _native: native,
        })
    }
    fn size(&self) -> (f32, f32) {
        unsafe {
            let s = &*ScriptString_pSize(self.raw);
            (s.cx as f32, s.cy as f32)
        }
    }
    fn widths(&self) -> Result<Vec<i32>> {
        // 链接/回退字体可能扩展内部字符数；原生函数按 pcOutChars 写入，不能按输入长度分配。
        let count = unsafe { *ScriptString_pcOutChars(self.raw) }.max(0) as usize;
        let mut out = vec![0; count.max(self.len)];
        unsafe {
            ScriptStringGetLogicalWidths(self.raw, out.as_mut_ptr())?;
        }
        out.truncate(self.len);
        Ok(out)
    }
    fn attr(&self, cp: usize) -> u8 {
        unsafe { (*ScriptString_pLogAttr(self.raw).add(cp))._bitfield }
    }
    fn x(&self, cp: usize, trailing: bool) -> f32 {
        if self.len == 0 {
            return 0.;
        }
        // icp 是字符位置；末尾光标须取最后一个 UTF-16 单元的 trailing edge。
        // 传入 len 在组合字尾部可能失败，旧回退 0 会把行尾错误映射到行首。
        let (cp, trailing) = if cp >= self.len {
            (self.len - 1, true)
        } else {
            (cp, trailing)
        };
        unsafe { ScriptStringCPtoX(self.raw, cp as i32, trailing).unwrap_or(0) as f32 }
    }
}
impl Drop for Shaped {
    fn drop(&mut self) {
        unsafe {
            let _ = GdiFlush();
            let _ = ScriptStringFree(&mut self.raw);
        }
    }
}
pub struct Row {
    shape: Option<Shaped>,
    pub range: Range<usize>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    stops: Vec<(usize, usize)>,
}
impl Row {
    /// 成品排版的 UTF-8 光标停点；不重新分析字形，也不改变原有行布局。
    pub(crate) fn caret_stops(&self) -> Vec<(usize, f32)> {
        self.stops
            .iter()
            .map(|(byte, cp)| {
                (
                    self.range.start + byte,
                    self.x + self.shape.as_ref().map_or(0., |shape| shape.x(*cp, false)),
                )
            })
            .collect()
    }
}
pub struct TextLayout {
    pub rows: Vec<Row>,
    pub height: f32,
    // 必须晚于 rows 释放；字形缓冲中的字体仍使用本设备。
    _font: Option<FontContext>,
}
impl TextLayout {
    pub fn new(a: &Annotation) -> Result<Self> {
        let font = FontContext::new(a.style)?;
        let mut result = Self::from_dc(a, font.dc)?;
        result._font = Some(font);
        Ok(result)
    }
    pub fn from_dc(a: &Annotation, dc: HDC) -> Result<Self> {
        let mut metric = TEXTMETRICW::default();
        unsafe {
            GetTextMetricsW(dc, &mut metric).ok()?;
        }
        let padding = a.style.text.padding();
        let available = (a.local_bounds().width - padding * 2.).max(1.);
        let mut rows = Vec::new();
        let mut y = padding;
        let mut offset = 0;
        for paragraph in a.text.split('\n') {
            let content = paragraph.strip_suffix('\r').unwrap_or(paragraph);
            let mut boundaries = vec![0];
            if !content.is_empty() {
                let shaped = Shaped::new(dc, content)?;
                let widths = shaped.widths()?;
                let chars = char_map(content);
                let stops: Vec<_> = chars
                    .into_iter()
                    .filter(|(_, cp)| *cp == 0 || *cp == shaped.len || shaped.attr(*cp) & 4 != 0)
                    .collect();
                let mut prefix = vec![0; widths.len() + 1];
                for (i, w) in widths.iter().enumerate() {
                    prefix[i + 1] = prefix[i] + w;
                }
                let mut start = 0;
                let mut soft = 0;
                for j in 1..stops.len() {
                    while (prefix[stops[j].1] - prefix[stops[start].1]) as f32 > available
                        && j > start + 1
                    {
                        let end = if soft > start && soft < j {
                            soft
                        } else {
                            j - 1
                        };
                        boundaries.push(stops[end].0);
                        start = end;
                    }
                    if stops[j].1 < shaped.len && shaped.attr(stops[j].1) & 1 != 0 {
                        soft = j;
                    }
                }
            }
            boundaries.push(content.len());
            boundaries.dedup();
            if boundaries.len() == 1 {
                boundaries.push(content.len());
            }
            for limits in boundaries.windows(2) {
                let text = &content[limits[0]..limits[1]];
                let shape = if text.is_empty() {
                    None
                } else {
                    Some(Shaped::new(dc, text)?)
                };
                let (width, height) = shape
                    .as_ref()
                    .map_or((0., metric.tmHeight as f32), Shaped::size);
                let height = height.max(metric.tmHeight as f32).max(1.);
                let x = padding
                    + match a.style.text.alignment {
                        Alignment::Left => 0.,
                        Alignment::Center => (available - width).max(0.) / 2.,
                        Alignment::Right => (available - width).max(0.),
                    };
                let stops = char_map(text)
                    .into_iter()
                    .filter(|(_, cp)| {
                        shape
                            .as_ref()
                            .is_none_or(|s| *cp == 0 || *cp == s.len || s.attr(*cp) & 4 != 0)
                    })
                    .collect();
                rows.push(Row {
                    shape,
                    range: offset + limits[0]..offset + limits[1],
                    x,
                    y,
                    width,
                    height,
                    stops,
                });
                y += height;
            }
            offset += paragraph.len() + 1;
        }
        Ok(Self {
            rows,
            height: y + padding,
            _font: None,
        })
    }
    pub fn draw(&self, x: i32, y: i32) -> Result<()> {
        self.draw_between(x, y, f32::NEG_INFINITY, f32::INFINITY)
    }
    /// 只输出与排版坐标纵向范围 `[top, bottom]` 相交的行（不含 `x`/`y` 偏移）。
    /// 分块重绘时整块外的行对该块没有像素，跳过后输出不变；字形可伸出行框，前后各留两倍行高。
    pub fn draw_between(&self, x: i32, y: i32, top: f32, bottom: f32) -> Result<()> {
        for r in self.rows_between(top, bottom) {
            if let Some(s) = &r.shape {
                unsafe {
                    ScriptStringOut(
                        s.raw,
                        x + r.x.round() as i32,
                        y + r.y.round() as i32,
                        ETO_OPTIONS(0),
                        None,
                        0,
                        0,
                        false,
                    )?;
                }
            }
        }
        Ok(())
    }
    /// 范围内是否有需要输出字形的行；与 [`Self::draw_between`] 同一判定。
    pub fn has_rows_between(&self, top: f32, bottom: f32) -> bool {
        self.rows_between(top, bottom).next().is_some()
    }
    fn rows_between(&self, top: f32, bottom: f32) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(move |r| {
            let margin = r.height.abs() * 2.;
            let row_top = r.y.round();
            r.shape.is_some() && row_top + r.height + margin >= top && row_top - margin <= bottom
        })
    }
    pub fn position(&self, index: usize) -> (Point, f32) {
        let row = self
            .rows
            .iter()
            .enumerate()
            .find(|(i, r)| {
                index < r.range.end
                    || *i + 1 == self.rows.len()
                    || (index == r.range.end
                        && self.rows.get(*i + 1).is_none_or(|n| n.range.start > index))
            })
            .map(|(_, r)| r)
            .unwrap_or(&self.rows[0]);
        let local = index.saturating_sub(row.range.start);
        let cp = row
            .stops
            .iter()
            .rev()
            .find(|(b, _)| *b <= local)
            .map_or(0, |(_, c)| *c);
        (
            Point {
                x: row.x + row.shape.as_ref().map_or(0., |s| s.x(cp, false)),
                y: row.y,
            },
            row.height,
        )
    }
    pub fn hit(&self, p: Point) -> usize {
        let row = self
            .rows
            .iter()
            .find(|r| p.y < r.y + r.height)
            .unwrap_or_else(|| self.rows.last().unwrap());
        let local = row
            .stops
            .iter()
            .min_by(|(_, a), (_, b)| {
                let x = |cp| row.x + row.shape.as_ref().map_or(0., |s| s.x(cp, false));
                (x(*a) - p.x).abs().total_cmp(&(x(*b) - p.x).abs())
            })
            .map_or(0, |(b, _)| *b);
        row.range.start + local
    }
    /// 命中实际排版的视觉行；行尾插入点也按鼠标的 y 归属，不猜下一行。
    pub fn visual_line_at(&self, p: Point) -> Option<Range<usize>> {
        self.rows
            .iter()
            .find(|row| p.y < row.y + row.height)
            .or_else(|| self.rows.last())
            .map(|row| row.range.clone())
    }
    pub fn selection(&self, range: Range<usize>) -> Vec<(Point, f32, f32)> {
        let mut out = Vec::new();
        for row in &self.rows {
            if !row.range.is_empty() && range.start <= row.range.start && range.end >= row.range.end
            {
                out.push((Point { x: row.x, y: row.y }, row.width.max(1.), row.height));
                continue;
            }
            for pair in row.stops.windows(2) {
                if row.range.start + pair[0].0 >= range.end
                    || row.range.start + pair[1].0 <= range.start
                {
                    continue;
                }
                if let Some(s) = &row.shape {
                    let x1 = s.x(pair[0].1, false);
                    let x2 = s.x(pair[1].1 - 1, true);
                    out.push((
                        Point {
                            x: row.x + x1.min(x2),
                            y: row.y,
                        },
                        (x1 - x2).abs().max(1.),
                        row.height,
                    ));
                }
            }
        }
        out
    }
}
fn char_map(text: &str) -> Vec<(usize, usize)> {
    let mut cp = 0;
    let mut out = Vec::new();
    for (byte, c) in text.char_indices() {
        out.push((byte, cp));
        cp += c.len_utf16();
    }
    out.push((text.len(), cp));
    out
}

#[cfg(test)]
mod visual_line_tests {
    use super::*;
    use crate::{
        annotation::{Style, Tool},
        editor::Editor,
        geometry::Rect,
        text_input::TextInputState,
    };
    // 只构造已排版 Row；不创建字体、HDC、Uniscribe 或 GPUI 视图。
    fn synthetic(text: &str, ranges: &[Range<usize>]) -> TextLayout {
        TextLayout {
            rows: ranges
                .iter()
                .enumerate()
                .map(|(i, range)| Row {
                    shape: None,
                    range: range.clone(),
                    x: 4.,
                    y: 10. + i as f32 * 20.,
                    width: 80.,
                    height: 20.,
                    stops: char_map(&text[range.clone()]),
                })
                .collect(),
            height: 10. + ranges.len() as f32 * 20.,
            _font: None,
        }
    }
    #[test]
    fn explicit_lf_crlf_empty_and_utf8_lines_select_only_the_actual_row() {
        let text = "中🙂\n第二行\r\n\n尾";
        let ranges = [0..7, 8..17, 19..19, 20..23];
        let layout = synthetic(text, &ranges);
        let mut input = TextInputState::default();
        for (i, expected) in ranges.iter().enumerate() {
            let range = layout
                .visual_line_at(Point {
                    x: 999.,
                    y: 20. + i as f32 * 20.,
                })
                .unwrap();
            assert_eq!(&range, expected);
            assert!(crate::capture_text_selection::select_visual_line(
                &mut input, text, range
            ));
            assert_eq!(&input.selection, expected);
            assert!(!text[input.selection.clone()].contains(['\r', '\n']));
            assert!(input.drag_anchor.is_none());
        }
        assert_eq!(layout.visual_line_at(Point { x: 0., y: -1. }), Some(0..7));
        assert_eq!(
            layout.visual_line_at(Point { x: 0., y: 500. }),
            Some(20..23)
        );
        assert!(synthetic("", &[])
            .visual_line_at(Point { x: 0., y: 0. })
            .is_none());
    }
    #[test]
    fn wrapped_boundary_uses_pointer_y_instead_of_next_row_caret_affinity() {
        let text = "abcd中文🙂xyz";
        let layout = synthetic(text, &[0..4, 4..10, 10..14, 14..17]);
        assert_eq!(layout.position(4).0.y, 30.); // 共享边界 caret 属于下一行。
        assert_eq!(
            layout.visual_line_at(Point { x: 999., y: 29.99 }),
            Some(0..4)
        );
        assert_eq!(layout.visual_line_at(Point { x: 0., y: 30. }), Some(4..10));
        assert_eq!(
            layout.visual_line_at(Point { x: 999., y: 50. }),
            Some(10..14)
        );
    }
    #[test]
    fn line_selection_preserves_marked_draft_geometry_and_original_document_undo() {
        let mut editor = Editor::new(Rect::full(800, 600), Some(Rect::full(800, 600)));
        let mut a = Annotation::new(Tool::Text, Point { x: 100., y: 100. }, Style::default());
        a.points.push(Point { x: 400., y: 180. });
        a.text = "中🙂\n后一行".into();
        let id = editor.document.insert(a.clone());
        editor.select_tool(Tool::Text);
        editor.begin_text(Some(id), a.points[0]);
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let layout = synthetic(&a.text, &[0..7, 8..17]);
        let mut input = TextInputState {
            selection: 3..3,
            marked: Some(0..3),
            reversed: true,
            drag_anchor: Some(3),
        };
        let range = layout.visual_line_at(Point { x: 100., y: 35. }).unwrap();
        assert!(crate::capture_text_selection::select_visual_line(
            &mut input, &a.text, range
        ));
        assert_eq!(input.selection, 8..17);
        assert_eq!(input.marked, Some(0..3));
        assert!(!input.reversed);
        assert!(input.drag_anchor.is_none()); // 后续 Move 没有可重置行选区的拖选 anchor。
        assert_eq!(editor.text_draft.as_ref().unwrap().annotation, a);
        assert_eq!(editor.crop, Some(Rect::full(800, 600)));
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.finish_text();
        assert_eq!(editor.document.revision(), revision);
        editor.document.undo();
        assert!(editor.document.items().is_empty());
        editor.document.redo();
        assert_eq!(editor.document.get(id), Some(&a));
    }
}

#[cfg(test)]
mod caret_tests {
    use super::*;
    use crate::annotation::{Style, Tool};

    fn layout(text: &str) -> TextLayout {
        let mut a = Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Style::default());
        a.update(Point { x: 900., y: 150. });
        a.text = text.into();
        TextLayout::new(&a).unwrap()
    }

    #[test]
    fn final_caret_after_combining_or_surrogate_text_stays_at_the_visual_line_end() {
        for text in ["lower e\u{301}", "中 文 🧑‍💻", "abc "] {
            let layout = layout(text);
            let row = &layout.rows[0];
            let end = row.caret_stops().last().unwrap().1;
            assert!(
                (end - (row.x + row.width)).abs() <= 1.,
                "{text}: {end} vs {}",
                row.x + row.width
            );
            assert_eq!(
                layout.hit(Point {
                    x: 10000.,
                    y: row.y + 1.
                }),
                text.len()
            );
            assert_eq!(layout.position(text.len()).0.x, end);
        }
    }

    #[test]
    fn final_caret_for_rtl_text_uses_the_last_logical_characters_trailing_edge() {
        let text = "مرحبا";
        let layout = layout(text);
        let row = &layout.rows[0];
        let shape = row.shape.as_ref().unwrap();
        let expected =
            unsafe { ScriptStringCPtoX(shape.raw, (shape.len - 1) as i32, true).unwrap() } as f32;
        assert_eq!(layout.position(text.len()).0.x, row.x + expected);
        assert!(layout.position(0).0.x > layout.position(text.len()).0.x);
    }
}
