//! 贴图选字只保存原文 byte 范围与成品几何；不修改文档或导出像素。
use crate::{
    annotation::{Annotation, Bounds, Point, Tool},
    recognition::{
        geometry::Projection, graphemes, HorizontalDirection, RecognizedText, RecognizedWord,
        TextGeometry,
    },
    text_layout::TextLayout,
    transform::Rotation,
    Result,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[path = "pin_text_navigation.rs"]
mod navigation;
pub(crate) use navigation::{KeyboardSelection, NavigationKey};
#[path = "pin_text_caret.rs"]
mod caret;
#[path = "ocr_find_selection.rs"]
pub(crate) mod find;

struct Cell {
    range: Range<usize>,
    bounds: Bounds,
    projected: Option<ProjectedCell>,
    source_id: usize,
    reverse_x: bool,
}
/// 统一行高的选区投影；u 来自 CTC 采样带或旧估算，不代表字形边界。
#[derive(Clone, Copy)]
struct ProjectedCell {
    source: TextGeometry,
    source_id: usize,
    start: f64,
    end: f64,
}
struct Line {
    range: Range<usize>,
    bounds: Bounds,
    rotation: Rotation,
    stops: Vec<(usize, f32)>,
    cells: Vec<Cell>,
    projected: bool,
}
#[derive(Clone, Copy)]
enum Unit {
    Caret,
    Word,
    Line,
}
#[derive(Clone)]
pub(crate) struct SelectionAnchor {
    range: Range<usize>,
    unit: Unit,
}
pub(crate) struct SelectionText {
    pub(crate) text: String,
    lines: Vec<Line>,
    words: Vec<Range<usize>>,
}
impl SelectionText {
    pub(crate) fn native_region_eligible(
        annotations: &[Annotation],
        full: crate::geometry::Rect,
    ) -> bool {
        Self::native_eligible(annotations)
            && annotations.iter().all(|annotation| {
                let bounds = annotation.bounds();
                [bounds.x, bounds.y, bounds.width, bounds.height]
                    .iter()
                    .all(|value| value.is_finite())
                    && bounds.width > 0.
                    && bounds.height > 0.
                    && bounds.x >= full.x as f32
                    && bounds.y >= full.y as f32
                    && bounds.x + bounds.width <= (full.x + full.width) as f32
                    && bounds.y + bounds.height <= (full.y + full.height) as f32
            })
    }
    /// 原文层不能表达对象遮挡；较大批次或重叠对象回到可见 Scene 的 OCR。
    pub(crate) fn native_eligible(annotations: &[Annotation]) -> bool {
        if annotations.is_empty()
            || annotations.len() > 64
            || annotations
                .iter()
                .map(|annotation| annotation.text.len())
                .sum::<usize>()
                > 65536
            || annotations
                .iter()
                .any(|annotation| annotation.tool != Tool::Text)
        {
            return false;
        }
        let bounds: Vec<_> = annotations.iter().map(Annotation::bounds).collect();
        for (index, a) in bounds.iter().enumerate() {
            if bounds[..index].iter().any(|b| {
                a.x < b.x + b.width
                    && b.x < a.x + a.width
                    && a.y < b.y + b.height
                    && b.y < a.y + a.height
            }) {
                return false;
            }
        }
        true
    }
    pub(crate) fn from_recognized(recognized: &RecognizedText) -> Self {
        // 只约束新增证据路线；旧提供者与超预算记录保留原选择回退。
        let sampling_ready = sampling_source_ready(recognized);
        let text = recognized.text.clone();
        let mut result = Self {
            text,
            lines: Vec::new(),
            words: Vec::new(),
        };
        let mut line_words = vec![Vec::new(); recognized.text.split('\n').count()];
        for word in &recognized.words {
            if let Some(words) = line_words.get_mut(word.line) {
                words.push(word);
            }
        }
        let mut offset = 0;
        for (index, content) in recognized.text.split('\n').enumerate() {
            let range = offset..offset + content.len();
            if sampling_ready {
                if let Some((cells, words)) =
                    sampled_cells(recognized, index, content, offset, &line_words[index])
                {
                    let bounds = crate::transform::bounds(
                        cells
                            .iter()
                            .flat_map(|cell| crate::transform::corners(cell.bounds)),
                    );
                    result.lines.push(Line {
                        range,
                        bounds,
                        rotation: Rotation::new(Point { x: 0., y: 0. }, 0.),
                        stops: Vec::new(),
                        cells,
                        projected: true,
                    });
                    result.words.extend(words);
                    offset += content.len() + 1;
                    continue;
                }
            }
            let mut cells = Vec::new();
            let mut stops = Vec::new();
            let mut words = Vec::new();
            let mut boundaries = vec![false; content.len() + 1];
            for (byte, _) in content.grapheme_indices(true) {
                boundaries[byte] = true;
            }
            boundaries[content.len()] = true;
            let mut cursor = 0;
            let mut complete = true;
            // 混合来源行在同一投影分支处理；缺失精细几何的合法框明确回退。
            let projected = line_words[index].iter().any(|word| {
                word.geometry
                    .and_then(|geometry| geometry.validated_in(word.rect))
                    .is_some()
            }) || recognized.lines.get(index).is_some_and(|line| {
                line.text == content
                    && line.rect.is_some_and(|rect| {
                        line.geometry
                            .and_then(|geometry| geometry.validated_in(rect))
                            .is_some()
                    })
            });
            for word in &line_words[index] {
                if word.text.is_empty() || !valid_ocr_rect(word.rect) {
                    complete = false;
                    break;
                }
                let Some(found) = content.get(cursor..).and_then(|rest| rest.find(&word.text))
                else {
                    complete = false;
                    break;
                };
                let start = cursor + found;
                let end = start + word.text.len();
                // 只有空白可以缺少词框。不能跳过缺失词后把重复原文绑定到另一个框。
                if !content[cursor..start].chars().all(char::is_whitespace)
                    || !boundaries[start]
                    || !boundaries[end]
                    || !append_recognized_cells(
                        &word.text,
                        offset + start,
                        word.rect,
                        word.horizontal_direction,
                        ocr_projection(word.rect, word.geometry, projected),
                        &mut cells,
                        &mut stops,
                    )
                {
                    complete = false;
                    break;
                }
                words.push(offset + start..offset + end);
                cursor = end;
            }
            complete &= content[cursor..].chars().all(char::is_whitespace);
            if !complete {
                cells.clear();
                stops.clear();
                words.clear();
                // OCR 行文字与词文字分别来自 API。整行来源必须原文一致且拥有完整有效框。
                if let Some((rect, direction, geometry)) =
                    recognized.lines.get(index).and_then(|line| {
                        (line.text == content)
                            .then_some(line.rect)
                            .flatten()
                            .filter(|rect| valid_ocr_rect(*rect))
                            .map(|rect| (rect, line.horizontal_direction, line.geometry))
                    })
                {
                    if append_recognized_cells(
                        content,
                        offset,
                        rect,
                        direction,
                        ocr_projection(rect, geometry, projected),
                        &mut cells,
                        &mut stops,
                    ) {
                        words.extend(
                            content
                                .unicode_word_indices()
                                .map(|(byte, word)| offset + byte..offset + byte + word.len()),
                        );
                    } else {
                        cells.clear();
                        stops.clear();
                    }
                }
            }
            if projected && cells.iter().any(|cell| cell.projected.is_some()) {
                // 同一 OCR 行返回的是合并内容；逐词框的高度和词间空隙不能把行选区拆碎。
                if let Some(geometry) = recognized
                    .lines
                    .get(index)
                    .filter(|line| line.text == content)
                    .and_then(|line| line.geometry)
                {
                    let _ = continuous_line_cells(&mut cells, geometry);
                }
            }
            if !cells.is_empty() {
                let bounds = crate::transform::bounds(
                    cells
                        .iter()
                        .flat_map(|cell| crate::transform::corners(cell.bounds)),
                );
                // 空隙有原文 byte 范围，但没有字形；拖过空隙仍使用相邻真实停点。
                result.lines.push(Line {
                    range,
                    bounds,
                    rotation: Rotation::new(Point { x: 0., y: 0. }, 0.),
                    projected: cells.iter().any(|cell| cell.projected.is_some()),
                    stops,
                    cells,
                });
                result.words.extend(words);
            }
            offset += content.len() + 1;
        }
        result
    }
    pub(crate) fn from_annotations(annotations: &[Annotation]) -> Result<Self> {
        let mut result = Self {
            text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
        };
        // 原生文字对象可以按任意顺序插入；选字范围沿可见的行序，不能沿绘制层序跳行。
        let mut ordered: Vec<_> = annotations.iter().collect();
        ordered.sort_by(|a, b| {
            let (a, b) = (a.bounds(), b.bounds());
            a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x))
        });
        for annotation in ordered {
            if annotation.tool != Tool::Text {
                continue;
            }
            if !result.text.is_empty() {
                result.text.push('\n');
            }
            let offset = result.text.len();
            result.text.push_str(&annotation.text);
            let layout = TextLayout::new(annotation)?;
            let origin = annotation.local_bounds();
            let mut boundaries = vec![false; annotation.text.len() + 1];
            for (byte, _) in annotation.text.grapheme_indices(true) {
                boundaries[byte] = true;
            }
            boundaries[annotation.text.len()] = true;
            for row in &layout.rows {
                let stops: Vec<_> = row
                    .caret_stops()
                    .into_iter()
                    .filter(|(byte, _)| boundaries[*byte])
                    .map(|(byte, x)| (offset + byte, origin.x + x))
                    .collect();
                let cells = stops
                    .windows(2)
                    .filter(|pair| pair[0].0 < pair[1].0)
                    .map(|pair| Cell {
                        range: pair[0].0..pair[1].0,
                        bounds: Bounds {
                            x: pair[0].1.min(pair[1].1),
                            y: origin.y + row.y,
                            width: (pair[0].1 - pair[1].1).abs().max(1.),
                            height: row.height,
                        },
                        projected: None,
                        source_id: 0,
                        reverse_x: pair[0].1 > pair[1].1,
                    })
                    .collect();
                result.lines.push(Line {
                    range: offset + row.range.start..offset + row.range.end,
                    bounds: Bounds {
                        x: origin.x + row.x,
                        y: origin.y + row.y,
                        width: row.width.max(1.),
                        height: row.height,
                    },
                    rotation: annotation.rotation_transform(),
                    projected: false,
                    stops,
                    cells,
                });
            }
            result.words.extend(
                annotation
                    .text
                    .unicode_word_indices()
                    .map(|(byte, word)| offset + byte..offset + byte + word.len()),
            );
        }
        Ok(result)
    }
    pub(crate) fn has_geometry(&self) -> bool {
        self.lines.iter().any(|line| !line.cells.is_empty())
    }
    pub(crate) fn translate(&mut self, x: f32, y: f32) {
        for line in &mut self.lines {
            line.bounds.x += x;
            line.bounds.y += y;
            line.rotation.center.x += x;
            line.rotation.center.y += y;
            for (_, stop) in &mut line.stops {
                *stop += x;
            }
            for cell in &mut line.cells {
                cell.bounds.x += x;
                cell.bounds.y += y;
                if let Some(projected) = &mut cell.projected {
                    for point in &mut projected.source.quad {
                        point.x += x;
                        point.y += y;
                    }
                }
            }
        }
    }
    pub(crate) fn has_text_at(&self, point: Point) -> bool {
        self.hit_cell(point).is_some()
    }
    fn hit_cell(&self, point: Point) -> Option<(&Line, &Cell)> {
        self.lines.iter().rev().find_map(|line| {
            let local = line.rotation.inverse().map(point);
            if !line.bounds.contains(local, 0.) {
                return None;
            }
            line.cells
                .iter()
                .find(|cell| {
                    cell_contains(cell, local)
                        && self
                            .text
                            .get(cell.range.clone())
                            .is_some_and(|text| !text.chars().all(char::is_whitespace))
                })
                .map(|cell| (line, cell))
        })
    }
    pub(crate) fn selected(&self, range: Range<usize>) -> Option<String> {
        if range.is_empty() {
            return None;
        }
        self.text.get(range).map(str::to_owned)
    }
    /// 标记仅取选中字符的已有几何，不填空白或换行占位；坐标沿当前选字空间。
    pub(crate) fn marking_quads(&self, range: Range<usize>) -> Vec<[Point; 4]> {
        if range.is_empty() || self.text.get(range.clone()).is_none() {
            return Vec::new();
        }
        let mut result = Vec::new();
        for line in &self.lines {
            if line.projected {
                result.extend(projected_quads(line, &self.text, &range, true));
                continue;
            }
            let mut spans: Vec<_> = line
                .cells
                .iter()
                .filter(|cell| cell.range.start < range.end && cell.range.end > range.start)
                .filter(|cell| {
                    self.text
                        .get(cell.range.clone())
                        .is_some_and(|text| !text.chars().all(char::is_whitespace))
                })
                .map(|cell| cell.bounds)
                .collect();
            spans.sort_by(|a, b| a.x.total_cmp(&b.x));
            let mut merged: Vec<Bounds> = Vec::new();
            for span in spans {
                if let Some(last) = merged.last_mut().filter(|last| {
                    (last.y - span.y).abs() < 0.1
                        && (last.height - span.height).abs() < 0.1
                        && span.x <= last.x + last.width
                }) {
                    last.width = (span.x + span.width).max(last.x + last.width) - last.x;
                } else {
                    merged.push(span);
                }
            }
            result.extend(merged.into_iter().map(|bounds| {
                crate::transform::corners(bounds).map(|point| line.rotation.map(point))
            }));
        }
        result
    }

    /// 持久标记必须完整覆盖所有选中非空白字素；复制与临时选择仍保留原文。
    /// 不增加 Find 的数量或可见裁剪限制，通过后沿用原四角合并与阅读方向。
    pub(crate) fn complete_marking_quads(&self, range: Range<usize>) -> Result<Vec<[Point; 4]>> {
        let quads = self.marking_quads(range.clone());
        if quads.is_empty() {
            return Err(tr!(
                "当前选区没有可标记的字形定位；文字仍可复制，请重选或重新识别",
                "No markable glyphs; text can still be copied. Reselect or retry OCR."
            )
            .into());
        }
        if !self.marking_range_complete(&range) {
            return Err(tr!(
                "选中文字缺少完整定位；请重选或重新识别，文字仍可复制",
                "Incomplete text locations. Reselect or retry OCR; copying still works."
            )
            .into());
        }
        Ok(quads)
    }

    fn marking_range_complete(&self, range: &Range<usize>) -> bool {
        if range.is_empty() || self.text.get(range.clone()).is_none() {
            return false;
        }
        let mut start_boundary = false;
        let mut end_boundary = range.end == self.text.len();
        let mut expected = Vec::new();
        // 全正文边界避免将组合字、ZWJ 或旗帜的合法 UTF-8 子串当成完整字素。
        for (byte, glyph) in self.text.grapheme_indices(true) {
            start_boundary |= byte == range.start;
            end_boundary |= byte == range.end;
            if byte >= range.end {
                break;
            }
            if byte >= range.start && !glyph.chars().all(char::is_whitespace) {
                expected.push((byte..byte + glyph.len(), false));
            }
        }
        if !start_boundary || !end_boundary || expected.is_empty() {
            return false;
        }
        for line in &self.lines {
            for cell in &line.cells {
                if cell.range.start >= range.end || cell.range.end <= range.start {
                    continue;
                }
                let Some(glyph) = self.text.get(cell.range.clone()) else {
                    return false;
                };
                if glyph.chars().all(char::is_whitespace) {
                    continue;
                }
                let index = expected.partition_point(|(glyph, _)| glyph.start < cell.range.start);
                let Some((glyph, covered)) = expected.get_mut(index) else {
                    return false;
                };
                if *glyph != cell.range
                    || *covered
                    || cell.range.start < line.range.start
                    || cell.range.end > line.range.end
                {
                    return false;
                }
                if let Some(projected) = cell.projected {
                    if projected.source_id != cell.source_id
                        || Projection::new(projected.source).is_none()
                        || !projected.start.is_finite()
                        || !projected.end.is_finite()
                        || projected.start < 0.
                        || projected.end > 1.
                        || projected.start >= projected.end
                    {
                        return false;
                    }
                } else if [
                    cell.bounds.x,
                    cell.bounds.y,
                    cell.bounds.width,
                    cell.bounds.height,
                ]
                .iter()
                .any(|value| !value.is_finite())
                    || cell.bounds.width <= 0.
                    || cell.bounds.height <= 0.
                {
                    return false;
                }
                let quad = cell_quad(cell).map(|point| line.rotation.map(point));
                if Projection::new(TextGeometry { quad }).is_none() {
                    return false;
                }
                *covered = true;
            }
        }
        expected.into_iter().all(|(_, covered)| covered)
    }

    /// Down 只命中有字形的非空白字符；空白保留贴图移动路线。
    pub(crate) fn begin(
        &self,
        point: Point,
        clicks: usize,
    ) -> Option<(SelectionAnchor, Range<usize>)> {
        let (line, cell) = self.hit_cell(point)?;
        let unit = if clicks >= 3 {
            Unit::Line
        } else if clicks == 2 {
            Unit::Word
        } else {
            Unit::Caret
        };
        let local = line.rotation.inverse().map(point);
        let range = match unit {
            Unit::Caret => {
                let byte = nearest_caret(line, local);
                byte..byte
            }
            Unit::Word => self
                .word_range(cell.range.start)
                .unwrap_or_else(|| cell.range.clone()),
            Unit::Line => line.range.clone(),
        };
        Some((
            SelectionAnchor {
                range: range.clone(),
                unit,
            },
            range,
        ))
    }
    pub(crate) fn extend(&self, anchor: &SelectionAnchor, point: Point) -> Range<usize> {
        let Some(end) = self.extension_target(anchor, point) else {
            return anchor.range.clone();
        };
        anchor.range.start.min(end.start)..anchor.range.end.max(end.end)
    }
    /// 鼠标拖选和键盘锚点复用同一目标行、停点及词／行范围，不另按屏幕 x 排序。
    fn extension_target(&self, anchor: &SelectionAnchor, point: Point) -> Option<Range<usize>> {
        let projected = self.lines.iter().any(|line| line.projected);
        let line = self.lines.iter().min_by(|a, b| {
            if projected {
                projected_line_distance(a, point).total_cmp(&projected_line_distance(b, point))
            } else {
                let (ay, ax) = line_distance(a, point);
                let (by, bx) = line_distance(b, point);
                ay.total_cmp(&by).then_with(|| ax.total_cmp(&bx))
            }
        })?;
        let local = line.rotation.inverse().map(point);
        let byte = if line.projected {
            nearest_caret(line, local)
        } else if local.y < line.bounds.y && line.range.start == 0 {
            line.range.start
        } else if local.y >= line.bounds.y + line.bounds.height && line.range.end == self.text.len()
        {
            line.range.end
        } else {
            nearest_stop(line, local.x)
        };
        Some(match anchor.unit {
            Unit::Caret => byte..byte,
            Unit::Word => self.word_range(byte).unwrap_or(byte..byte),
            Unit::Line => line.range.clone(),
        })
    }
    /// 捕获丢失或已经松开时结束拖选，保留最后有效选区；后续悬停不得继续改变它。
    pub(crate) fn drag(
        &self,
        anchor: &mut Option<SelectionAnchor>,
        selected: &mut Option<Range<usize>>,
        point: Point,
        left_pressed: bool,
        owns_capture: bool,
    ) -> bool {
        let Some(start) = anchor.as_ref() else {
            return false;
        };
        if !left_pressed || !owns_capture {
            *anchor = None;
        } else {
            *selected = Some(self.extend(start, point));
        }
        true
    }
    /// Up 可以没有最后一次 Move；调用方先核验本轮本窗口的合法 release（GPUI 可已释放捕获）。
    pub(crate) fn finish_drag(
        &self,
        anchor: &mut Option<SelectionAnchor>,
        selected: &mut Option<Range<usize>>,
        point: Point,
        valid_release: bool,
    ) -> bool {
        let Some(start) = anchor.take() else {
            return false;
        };
        if valid_release {
            *selected = Some(self.extend(&start, point));
        }
        true
    }
    fn word_range(&self, byte: usize) -> Option<Range<usize>> {
        self.words
            .iter()
            .find(|range| range.start <= byte && byte < range.end)
            .cloned()
    }
    pub(crate) fn highlights(&self, range: Range<usize>) -> Vec<[Point; 4]> {
        let mut result = Vec::new();
        for line in &self.lines {
            if line.projected {
                result.extend(projected_quads(line, &self.text, &range, false));
                if includes_line_ending(&self.text, line, &range) {
                    if let Some(quad) = projected_line_ending(line) {
                        result.push(quad);
                    }
                }
                continue;
            }
            let mut spans: Vec<_> = line
                .cells
                .iter()
                .filter(|cell| cell.range.start < range.end && cell.range.end > range.start)
                .map(|cell| cell.bounds)
                .collect();
            spans.sort_by(|a, b| a.x.total_cmp(&b.x));
            let mut merged: Vec<Bounds> = Vec::new();
            for span in spans {
                if let Some(last) = merged.last_mut().filter(|last| {
                    (last.y - span.y).abs() < 0.1 && span.x <= last.x + last.width + 0.5
                }) {
                    last.width = (span.x + span.width).max(last.x + last.width) - last.x;
                } else {
                    merged.push(span);
                }
            }
            if range.start <= line.range.end
                && range.end > line.range.end
                && self
                    .text
                    .get(line.range.end..)
                    .is_some_and(|tail| tail.starts_with('\n') || tail.starts_with("\r\n"))
            {
                let end = line.stops.last().map_or(line.bounds.x, |stop| stop.1);
                let reversed = line
                    .stops
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|stop| stop.1 > end);
                merged.push(Bounds {
                    x: if reversed { end - 5. } else { end },
                    y: line.bounds.y,
                    width: 5.,
                    height: line.bounds.height,
                });
            }
            result.extend(merged.into_iter().map(|bounds| {
                crate::transform::corners(bounds).map(|point| line.rotation.map(point))
            }));
        }
        result
    }
}
/// 元数据的一致性门不冒充图像身份；调用方原有任务来源／版本门仍负责迟到结果。
fn sampling_source_ready(source: &RecognizedText) -> bool {
    if source.text.len() > graphemes::MAX_SOURCE_BYTES
        || source.lines.len() > 4096
        || source.words.len() > 32768
        || source
            .words
            .iter()
            .any(|word| word.line >= source.lines.len())
    {
        return false;
    }
    let mut content = source.text.split('\n');
    let (mut groups, mut runs) = (0usize, 0usize);
    for line in &source.lines {
        if content.next() != Some(line.text.as_str()) {
            return false;
        }
        if let Some(sampling) = &line.graphemes {
            let Some(next_groups) = groups.checked_add(sampling.grapheme_count()) else {
                return false;
            };
            let Some(next_runs) = runs.checked_add(sampling.run_count()) else {
                return false;
            };
            (groups, runs) = (next_groups, next_runs);
            if groups > graphemes::MAX_TOTAL_GRAPHEMES || runs > graphemes::MAX_TOTAL_RUNS {
                return false;
            }
        }
    }
    content.next().is_none()
}

/// 成功时提交整行完整字素；任何证据错误均让原路线接手，不留下半批停点。
fn sampled_cells(
    source: &RecognizedText,
    index: usize,
    content: &str,
    offset: usize,
    line_words: &[&RecognizedWord],
) -> Option<(Vec<Cell>, Vec<Range<usize>>)> {
    let line = source.lines.get(index)?;
    let sampling = line.graphemes.as_ref()?;
    let bands = sampling.validated_bands(index, line, line_words)?;
    let geometry = sampling.selection_geometry();
    // full-crop 可能大于词框联合 rect；不能以 line.rect 裁掉合法原投影。
    let projection = Projection::new(geometry)?;
    let mut word_ranges = Vec::with_capacity(line_words.len());
    let mut cursor = 0;
    for word in line_words {
        let found = content.get(cursor..)?.find(&word.text)?;
        let start = cursor.checked_add(found)?;
        let end = start.checked_add(word.text.len())?;
        word_ranges.push(start..end);
        cursor = end;
    }
    let mut cells = Vec::with_capacity(bands.len());
    for band in bands {
        let cluster = content.get(band.source_range.clone())?;
        if cluster.chars().all(char::is_whitespace) {
            continue;
        }
        // 同一 OCR 行共用有证据的统一行高；实际采样仍由原裁行校验。
        let projected = ProjectedCell {
            source: geometry,
            source_id: 0,
            start: band.start,
            end: band.end,
        };
        let quad = projected.quad(&projection);
        // 检查派生采样四边形，而不是只检查它的轴对齐外包。
        Projection::new(TextGeometry { quad })?;
        let bounds = crate::transform::bounds(quad);
        if bounds.width <= 0. || bounds.height <= 0. {
            return None;
        }
        cells.push(Cell {
            range: offset + band.source_range.start..offset + band.source_range.end,
            bounds,
            projected: Some(projected),
            source_id: projected.source_id,
            reverse_x: false,
        });
    }
    if cells.is_empty() {
        return None;
    }
    continuous_line_cells(&mut cells, geometry)?;
    trim_sampled_line_ends(&mut cells, sampling.selection_reading_range())?;
    let mut boundaries = vec![false; content.len() + 1];
    for (byte, _) in content.grapheme_indices(true) {
        boundaries[byte] = true;
    }
    boundaries[content.len()] = true;
    let words = if word_ranges
        .iter()
        .all(|word| boundaries[word.start] && boundaries[word.end])
    {
        word_ranges
            .into_iter()
            .map(|word| offset + word.start..offset + word.end)
            .collect()
    } else {
        // 词典／fragment 可在组合字素内部切词，双击范围不能切开完整字素。
        content
            .unicode_word_indices()
            .filter(|(byte, word)| boundaries[*byte] && boundaries[*byte + word.len()])
            .map(|(byte, word)| offset + byte..offset + byte + word.len())
            .collect()
    };
    Some((cells, words))
}

/// 连续字素已沿原阅读坐标分配；只移动首尾外边界，不重分配任何内部分界。
fn trim_sampled_line_ends(cells: &mut [Cell], [start, end]: [f64; 2]) -> Option<()> {
    if start == 0. && end == 1. {
        return Some(());
    }
    if !start.is_finite() || !end.is_finite() || start < 0. || end > 1. || start >= end {
        return None;
    }
    let last = cells.len().checked_sub(1)?;
    let mut changes = Vec::with_capacity(2);
    for (position, index) in [0, last].into_iter().enumerate() {
        if position == 1 && last == 0 {
            break;
        }
        let mut projected = cells[index].projected?;
        if index == 0 {
            projected.start = start;
        }
        if index == last {
            projected.end = end;
        }
        if projected.start >= projected.end {
            return None;
        }
        let projection = Projection::new(projected.source)?;
        let quad = projected.quad(&projection);
        Projection::new(TextGeometry { quad })?;
        let bounds = crate::transform::bounds(quad);
        if bounds.width <= 0. || bounds.height <= 0. {
            return None;
        }
        changes.push((index, projected, bounds));
    }
    for (index, projected, bounds) in changes {
        cells[index].projected = Some(projected);
        cells[index].bounds = bounds;
    }
    Some(())
}

/// CTC 带与逐词估算只包住预测峰值，行首右缩、字间留空，同行选区会碎成小块。
/// 因此各字只取中心作锚点，沿完整裁行在相邻中心的中线切分，
/// 首尾贴到裁行两端，生成首尾相接、同一行高的连续单元。失败时不改动原单元。
fn continuous_line_cells(cells: &mut [Cell], line: TextGeometry) -> Option<()> {
    let projection = Projection::new(line)?;
    let mut centers = Vec::with_capacity(cells.len());
    for cell in cells.iter() {
        // 投影单元取自身 u 中点的中线点；透视下四角平均点不是带中心。
        let u = match cell.projected {
            Some(projected) if projected.source == line => (projected.start + projected.end) / 2.,
            Some(projected) => {
                let center = Projection::new(projected.source)?
                    .point((projected.start + projected.end) / 2., 0.5);
                projection.inverse(center)?[0]
            }
            None => {
                let bounds = cell.bounds;
                projection.inverse(Point {
                    x: bounds.x + bounds.width / 2.,
                    y: bounds.y + bounds.height / 2.,
                })?[0]
            }
        };
        if !u.is_finite() {
            return None;
        }
        centers.push(u.clamp(0., 1.));
    }
    if centers.windows(2).any(|pair| pair[1] <= pair[0]) {
        // 阅读序与中心不单调时不能凭中线交错；按字数等分整行。
        let count = centers.len() as f64;
        for (index, center) in centers.iter_mut().enumerate() {
            *center = (index as f64 + 0.5) / count;
        }
    }
    let mut rebuilt = Vec::with_capacity(cells.len());
    for index in 0..cells.len() {
        let start = if index == 0 {
            0.
        } else {
            (centers[index - 1] + centers[index]) / 2.
        };
        let end = if index + 1 == cells.len() {
            1.
        } else {
            (centers[index] + centers[index + 1]) / 2.
        };
        let projected = ProjectedCell {
            source: line,
            source_id: 0,
            start,
            end,
        };
        let quad = projected.quad(&projection);
        let bounds = crate::transform::bounds(quad);
        if end <= start || bounds.width <= 0. || bounds.height <= 0. {
            return None;
        }
        rebuilt.push((projected, bounds));
    }
    for (cell, (projected, bounds)) in cells.iter_mut().zip(rebuilt) {
        cell.projected = Some(projected);
        cell.bounds = bounds;
        cell.source_id = 0;
        cell.reverse_x = false;
    }
    Some(())
}
fn valid_ocr_rect(rect: crate::geometry::Rect) -> bool {
    rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|right| right as f32 > rect.x as f32)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|bottom| bottom as f32 > rect.y as f32)
}
fn ocr_projection(
    rect: crate::geometry::Rect,
    geometry: Option<TextGeometry>,
    projected: bool,
) -> Option<Projection> {
    if !projected || !valid_ocr_rect(rect) {
        return None;
    }
    // 水平停点不证明 v 方向；缺失／无效投影保留旧矩形及物理上下沿。
    geometry.and_then(|geometry| geometry.validated_in(rect))
}
fn append_recognized_cells(
    text: &str,
    offset: usize,
    rect: crate::geometry::Rect,
    direction: HorizontalDirection,
    projection: Option<Projection>,
    cells: &mut Vec<Cell>,
    stops: &mut Vec<(usize, f32)>,
) -> bool {
    let Some(projection) = projection else {
        return append_ocr_cells(text, offset, rect, direction, cells, stops);
    };
    if !valid_ocr_rect(rect) {
        return false;
    }
    let clusters: Vec<_> = text.grapheme_indices(true).collect();
    if clusters.is_empty() {
        return false;
    }
    let source = TextGeometry {
        quad: projection.quad(),
    };
    let source_id = cells.len();
    // 在同一有效裁行四边形中等分字素，避免字节／标量分割组合符和 ZWJ。
    // 这是字符停点估算；CTC 或裁行边界都不提供精确墨迹字框。
    let mut projected_cells = Vec::with_capacity(clusters.len());
    for (index, (byte, cluster)) in clusters.iter().enumerate() {
        let start = index as f64 / clusters.len() as f64;
        let end = (index + 1) as f64 / clusters.len() as f64;
        let projected = ProjectedCell {
            source,
            source_id,
            start,
            end,
        };
        let quad = projected.quad(&projection);
        let bounds = crate::transform::bounds(quad);
        if !quad
            .iter()
            .all(|point| point.x.is_finite() && point.y.is_finite())
            || bounds.width <= 0.
            || bounds.height <= 0.
        {
            return false;
        }
        projected_cells.push(Cell {
            range: offset + byte..offset + byte + cluster.len(),
            bounds,
            projected: Some(projected),
            source_id,
            reverse_x: false,
        });
    }
    cells.extend(projected_cells);
    true
}
impl ProjectedCell {
    fn quad(self, projection: &Projection) -> [Point; 4] {
        [
            projection.point(self.start, 0.),
            projection.point(self.end, 0.),
            projection.point(self.end, 1.),
            projection.point(self.start, 1.),
        ]
    }
}
fn cell_contains(cell: &Cell, point: Point) -> bool {
    let Some(projected) = cell.projected else {
        return cell.bounds.contains(point, 0.);
    };
    Projection::new(projected.source)
        .and_then(|projection| projection.inverse(point))
        .is_some_and(|[u, v]| {
            u >= projected.start && u <= projected.end && (0.0..=1.0).contains(&v)
        })
}
fn segment_distance_squared(point: Point, start: Point, end: Point) -> f64 {
    let (dx, dy) = ((end.x - start.x) as f64, (end.y - start.y) as f64);
    let (px, py) = ((point.x - start.x) as f64, (point.y - start.y) as f64);
    let length = dx * dx + dy * dy;
    let t = if length > 0. {
        (px * dx + py * dy) / length
    } else {
        0.
    }
    .clamp(0., 1.);
    (px - t * dx).powi(2) + (py - t * dy).powi(2)
}
fn nearest_caret(line: &Line, point: Point) -> usize {
    if !line.projected {
        return nearest_stop(line, point.x);
    }
    line.cells
        .iter()
        .flat_map(|cell| {
            let quad = cell_quad(cell);
            [
                (
                    cell.range.start,
                    segment_distance_squared(point, quad[0], quad[3]),
                ),
                (
                    cell.range.end,
                    segment_distance_squared(point, quad[1], quad[2]),
                ),
            ]
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(line.range.start, |stop| stop.0)
}
fn cell_quad(cell: &Cell) -> [Point; 4] {
    if let Some(projected) = cell.projected {
        if let Some(projection) = Projection::new(projected.source) {
            return projected.quad(&projection);
        }
    }
    let quad = crate::transform::corners(cell.bounds);
    if cell.reverse_x {
        [quad[2], quad[3], quad[0], quad[1]]
    } else {
        quad
    }
}
fn quad_distance_squared(quad: [Point; 4], point: Point) -> f64 {
    // 调用方只传有效投影或旧矩形；凸四边形距离不附加 Projection 坐标上限，
    // 大矩形安全回退仍可以正确判定内部点。
    let cross: [f64; 4] = std::array::from_fn(|index| {
        let start = quad[index];
        let end = quad[(index + 1) % 4];
        (f64::from(end.x) - f64::from(start.x)) * (f64::from(point.y) - f64::from(start.y))
            - (f64::from(end.y) - f64::from(start.y)) * (f64::from(point.x) - f64::from(start.x))
    });
    if cross.iter().all(|value| *value >= 0.) || cross.iter().all(|value| *value <= 0.) {
        return 0.;
    }
    (0..4)
        .map(|index| segment_distance_squared(point, quad[index], quad[(index + 1) % 4]))
        .fold(f64::INFINITY, f64::min)
}
fn projected_line_distance(line: &Line, point: Point) -> f64 {
    let point = line.rotation.inverse().map(point);
    if !line.projected {
        return quad_distance_squared(crate::transform::corners(line.bounds), point);
    }
    line.cells
        .iter()
        .map(|cell| quad_distance_squared(cell_quad(cell), point))
        .fold(f64::INFINITY, f64::min)
}
fn projected_quads(
    line: &Line,
    text: &str,
    range: &Range<usize>,
    glyphs_only: bool,
) -> Vec<[Point; 4]> {
    struct Span {
        projected: Option<ProjectedCell>,
        source_id: usize,
        bounds: Bounds,
        quad: [Point; 4],
    }
    let mut spans: Vec<Span> = Vec::new();
    for cell in &line.cells {
        if cell.range.start >= range.end
            || cell.range.end <= range.start
            || (glyphs_only
                && text
                    .get(cell.range.clone())
                    .is_none_or(|text| text.chars().all(char::is_whitespace)))
        {
            continue;
        }
        let Some(projected) = cell.projected else {
            if let Some(last) = spans.last_mut().filter(|last| {
                last.projected.is_none()
                    && last.source_id == cell.source_id
                    && (last.bounds.y - cell.bounds.y).abs() < 0.1
                    && (last.bounds.height - cell.bounds.height).abs() < 0.1
                    && cell.bounds.x <= last.bounds.x + last.bounds.width
                    && last.bounds.x <= cell.bounds.x + cell.bounds.width
            }) {
                let right =
                    (last.bounds.x + last.bounds.width).max(cell.bounds.x + cell.bounds.width);
                last.bounds.x = last.bounds.x.min(cell.bounds.x);
                last.bounds.width = right - last.bounds.x;
                last.quad = crate::transform::corners(last.bounds);
            } else {
                // 旧矩形 v 沿物理屏幕；RTL 只有 x 停点证据，不能推断上下翻转。
                spans.push(Span {
                    projected: None,
                    source_id: cell.source_id,
                    bounds: cell.bounds,
                    quad: crate::transform::corners(cell.bounds),
                });
            }
            continue;
        };
        if let Some(span) = spans.last_mut() {
            if let Some(last) = &mut span.projected {
                // 选区是连续原文范围；同一裁行上被跳过的空白不能把一行切成多段。
                if last.source_id == projected.source_id
                    && last.source == projected.source
                    && projected.start >= last.end - f64::EPSILON
                {
                    if let Some(projection) = Projection::new(last.source) {
                        last.end = projected.end;
                        span.quad = last.quad(&projection);
                        continue;
                    }
                }
            }
        }
        spans.push(Span {
            projected: Some(projected),
            source_id: cell.source_id,
            bounds: cell.bounds,
            quad: cell_quad(cell),
        });
    }
    spans.into_iter().map(|span| span.quad).collect()
}
fn includes_line_ending(text: &str, line: &Line, range: &Range<usize>) -> bool {
    range.start <= line.range.end
        && range.end > line.range.end
        && text
            .get(line.range.end..)
            .is_some_and(|tail| tail.starts_with('\n') || tail.starts_with("\r\n"))
}
fn projected_line_ending(line: &Line) -> Option<[Point; 4]> {
    let quad = cell_quad(line.cells.last()?);
    let tangent = Point {
        x: (quad[1].x - quad[0].x + quad[2].x - quad[3].x) / 2.,
        y: (quad[1].y - quad[0].y + quad[2].y - quad[3].y) / 2.,
    };
    let length = tangent.x.hypot(tangent.y);
    if !length.is_finite() || length <= 0. {
        return None;
    }
    let extend = |point: Point| Point {
        x: point.x + tangent.x * 5. / length,
        y: point.y + tangent.y * 5. / length,
    };
    Some([quad[1], extend(quad[1]), extend(quad[2]), quad[2]])
}
fn append_ocr_cells(
    text: &str,
    offset: usize,
    rect: crate::geometry::Rect,
    direction: HorizontalDirection,
    cells: &mut Vec<Cell>,
    stops: &mut Vec<(usize, f32)>,
) -> bool {
    if !valid_ocr_rect(rect) {
        return false;
    }
    let clusters: Vec<_> = text.grapheme_indices(true).collect();
    if clusters.is_empty() {
        return false;
    }
    let source_id = cells.len();
    for (i, (byte, cluster)) in clusters.iter().enumerate() {
        let position = |index: usize| {
            let fraction = index as f32 / clusters.len() as f32;
            let fraction = match direction {
                HorizontalDirection::LeftToRight => fraction,
                HorizontalDirection::RightToLeft => 1. - fraction,
            };
            rect.x as f32 + rect.width as f32 * fraction
        };
        let start = position(i);
        let end = position(i + 1);
        let left = start.min(end);
        let right = start.max(end);
        if right <= left {
            return false;
        }
        let bytes = offset + byte..offset + byte + cluster.len();
        stops.push((bytes.start, start));
        cells.push(Cell {
            range: bytes.clone(),
            bounds: Bounds {
                x: left,
                y: rect.y as f32,
                width: right - left,
                height: rect.height as f32,
            },
            projected: None,
            source_id,
            reverse_x: direction == HorizontalDirection::RightToLeft,
        });
        stops.push((bytes.end, end));
    }
    true
}
fn nearest_stop(line: &Line, x: f32) -> usize {
    line.stops
        .iter()
        .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
        .map_or(line.range.start, |stop| stop.0)
}
fn line_distance(line: &Line, point: Point) -> (f32, f32) {
    let point = line.rotation.inverse().map(point);
    let dy = (line.bounds.y - point.y)
        .max(point.y - line.bounds.y - line.bounds.height)
        .max(0.);
    let dx = (line.bounds.x - point.x)
        .max(point.x - line.bounds.x - line.bounds.width)
        .max(0.);
    (dy, dx)
}

#[cfg(test)]
#[path = "pin_text_projection_tests.rs"]
mod projection_tests;

#[cfg(test)]
#[path = "pin_text_grapheme_tests.rs"]
mod grapheme_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        geometry::Rect,
        recognition::{RecognizedLine, RecognizedWord},
    };
    #[test]
    fn native_object_selection_follows_visual_rows_instead_of_insertion_order() {
        let mut top = Annotation::new(Tool::Text, Point { x: 20., y: 10. }, Default::default());
        top.update(Point { x: 250., y: 60. });
        top.text = "upper  中".into();
        let mut bottom = Annotation::new(Tool::Text, Point { x: 20., y: 80. }, Default::default());
        bottom.update(Point { x: 250., y: 130. });
        bottom.text = "lower e\u{301}".into();
        let annotations = [bottom, top];
        assert!(SelectionText::native_region_eligible(
            &annotations,
            Rect::full(300, 200)
        ));
        let text = SelectionText::from_annotations(&annotations).unwrap();
        assert_eq!(text.text, "upper  中\nlower e\u{301}");
        let first = &text.lines[0].cells[0];
        let (anchor, _) = text
            .begin(
                Point {
                    x: first.bounds.x + 0.1,
                    y: first.bounds.y + 1.,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 500., y: 90. }))
                .unwrap(),
            text.text
        );
    }
    #[test]
    fn native_overlay_rejects_clipped_or_nonfinite_text_ranges() {
        let mut text = Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        text.update(Point { x: 200., y: 100. });
        text.text = "visible text".into();
        let full = Rect::full(300, 200);
        assert!(SelectionText::native_region_eligible(&[text.clone()], full));
        text.points[0].x = -1.;
        assert!(!SelectionText::native_region_eligible(
            &[text.clone()],
            full
        ));
        text.points[0].x = 20.;
        text.points[1].y = 201.;
        assert!(!SelectionText::native_region_eligible(
            &[text.clone()],
            full
        ));
        text.rotation = f32::NAN;
        assert!(!SelectionText::native_region_eligible(&[text], full));
    }
    fn ocr() -> SelectionText {
        SelectionText::from_recognized(&RecognizedText {
            text: "中e\u{301}👨‍👩‍👧  X\nnext".into(),
            regions: Vec::new(),
            words: vec![
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "中e\u{301}👨‍👩‍👧".into(),
                    rect: Rect {
                        x: 10,
                        y: 10,
                        width: 90,
                        height: 20,
                    },
                    line: 0,
                },
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "X".into(),
                    rect: Rect {
                        x: 130,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    line: 0,
                },
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "next".into(),
                    rect: Rect {
                        x: 10,
                        y: 50,
                        width: 80,
                        height: 20,
                    },
                    line: 1,
                },
            ],
            lines: vec![
                RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "中e\u{301}👨‍👩‍👧  X".into(),
                    rect: None,
                },
                RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "next".into(),
                    rect: None,
                },
            ],
        })
    }
    #[test]
    fn marking_quads_horizontal_directions_keep_word_cells_and_real_gaps() {
        for (direction, first_x, second_x, first_character_x) in [
            (HorizontalDirection::LeftToRight, 10, 90, 10.),
            (HorizontalDirection::RightToLeft, 90, 10, 110.),
        ] {
            let text = SelectionText::from_recognized(&RecognizedText {
                text: "AB CD".into(),
                regions: Vec::new(),
                words: vec![
                    directional_word(
                        "AB",
                        Rect {
                            x: first_x,
                            y: 10,
                            width: 40,
                            height: 20,
                        },
                        0,
                        direction,
                    ),
                    directional_word(
                        "CD",
                        Rect {
                            x: second_x,
                            y: 10,
                            width: 40,
                            height: 20,
                        },
                        0,
                        direction,
                    ),
                ],
                lines: Vec::new(),
            });
            assert_eq!(text.selected(0..1).unwrap(), "A");
            assert_eq!(
                text.marking_quads(0..1),
                vec![crate::transform::corners(Bounds {
                    x: first_character_x,
                    y: 10.,
                    width: 20.,
                    height: 20.,
                })],
            );
            assert_eq!(text.selected(1..4).unwrap(), "B C");
            assert_eq!(
                text.marking_quads(1..4),
                vec![
                    crate::transform::corners(Bounds {
                        x: 30.,
                        y: 10.,
                        width: 20.,
                        height: 20.
                    }),
                    crate::transform::corners(Bounds {
                        x: 90.,
                        y: 10.,
                        width: 20.,
                        height: 20.
                    }),
                ],
            );
            assert_eq!(text.selected(0..5).unwrap(), "AB CD");
            assert_eq!(
                text.marking_quads(0..5),
                vec![
                    crate::transform::corners(Bounds {
                        x: 10.,
                        y: 10.,
                        width: 40.,
                        height: 20.
                    }),
                    crate::transform::corners(Bounds {
                        x: 90.,
                        y: 10.,
                        width: 40.,
                        height: 20.
                    }),
                ],
            );
            assert_eq!(text.selected(2..3).unwrap(), " ");
            assert!(text.marking_quads(2..3).is_empty());
        }
    }

    #[test]
    fn marking_quads_graphemes_stay_whole_and_merge_only_touching_cells() {
        let first = "中e\u{301}👨‍👩‍👧";
        let reversed = SelectionText::from_recognized(&RecognizedText {
            text: format!("{first}  X\nnext"),
            regions: Vec::new(),
            words: vec![
                directional_word(
                    first,
                    Rect {
                        x: 40,
                        y: 10,
                        width: 90,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
                directional_word(
                    "X",
                    Rect {
                        x: 10,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
                directional_word(
                    "next",
                    Rect {
                        x: 10,
                        y: 50,
                        width: 80,
                        height: 20,
                    },
                    1,
                    HorizontalDirection::LeftToRight,
                ),
            ],
            lines: Vec::new(),
        });
        for (text, combining_x, emoji_x, adjacent_x, first_span, second_span) in [
            (
                ocr(),
                40.,
                70.,
                10.,
                Bounds {
                    x: 10.,
                    y: 10.,
                    width: 90.,
                    height: 20.,
                },
                Bounds {
                    x: 130.,
                    y: 10.,
                    width: 20.,
                    height: 20.,
                },
            ),
            (
                reversed,
                70.,
                40.,
                70.,
                Bounds {
                    x: 10.,
                    y: 10.,
                    width: 20.,
                    height: 20.,
                },
                Bounds {
                    x: 40.,
                    y: 10.,
                    width: 90.,
                    height: 20.,
                },
            ),
        ] {
            assert_eq!(text.selected(3..6).unwrap(), "e\u{301}");
            assert_eq!(
                text.marking_quads(3..6),
                vec![crate::transform::corners(Bounds {
                    x: combining_x,
                    y: 10.,
                    width: 30.,
                    height: 20.,
                })],
            );
            assert_eq!(text.selected(6..first.len()).unwrap(), "👨‍👩‍👧");
            assert_eq!(
                text.marking_quads(6..first.len()),
                vec![crate::transform::corners(Bounds {
                    x: emoji_x,
                    y: 10.,
                    width: 30.,
                    height: 20.,
                })],
            );
            assert_eq!(text.selected(0..6).unwrap(), "中e\u{301}");
            assert_eq!(
                text.marking_quads(0..6),
                vec![crate::transform::corners(Bounds {
                    x: adjacent_x,
                    y: 10.,
                    width: 60.,
                    height: 20.,
                })],
            );
            assert_eq!(
                text.selected(0..first.len() + 3).unwrap(),
                format!("{first}  X")
            );
            assert_eq!(
                text.marking_quads(0..first.len() + 3),
                vec![
                    crate::transform::corners(first_span),
                    crate::transform::corners(second_span)
                ],
            );
            assert_eq!(text.selected(first.len()..first.len() + 2).unwrap(), "  ");
            assert!(text.marking_quads(first.len()..first.len() + 2).is_empty());
        }
    }

    #[test]
    fn marking_quads_excludes_lf_crlf_tail_blocks_and_newline_only_ranges() {
        for ending in ["\n", "\r\n"] {
            for direction in [
                HorizontalDirection::LeftToRight,
                HorizontalDirection::RightToLeft,
            ] {
                let text = SelectionText::from_recognized(&RecognizedText {
                    text: format!("AB{ending}next"),
                    regions: Vec::new(),
                    words: vec![
                        directional_word(
                            "AB",
                            Rect {
                                x: 10,
                                y: 10,
                                width: 40,
                                height: 20,
                            },
                            0,
                            direction,
                        ),
                        directional_word(
                            "next",
                            Rect {
                                x: 10,
                                y: 50,
                                width: 80,
                                height: 20,
                            },
                            1,
                            HorizontalDirection::LeftToRight,
                        ),
                    ],
                    lines: Vec::new(),
                });
                let first_and_ending = 0..2 + ending.len();
                assert_eq!(
                    text.selected(first_and_ending.clone()).unwrap(),
                    format!("AB{ending}")
                );
                assert_eq!(text.highlights(first_and_ending.clone()).len(), 2);
                assert_eq!(
                    text.marking_quads(first_and_ending),
                    vec![crate::transform::corners(Bounds {
                        x: 10.,
                        y: 10.,
                        width: 40.,
                        height: 20.
                    })],
                );
                assert_eq!(text.selected(2..2 + ending.len()).unwrap(), ending);
                assert_eq!(text.highlights(2..2 + ending.len()).len(), 1);
                assert!(text.marking_quads(2..2 + ending.len()).is_empty());
                assert_eq!(
                    text.marking_quads(0..text.text.len()),
                    vec![
                        crate::transform::corners(Bounds {
                            x: 10.,
                            y: 10.,
                            width: 40.,
                            height: 20.
                        }),
                        crate::transform::corners(Bounds {
                            x: 10.,
                            y: 50.,
                            width: 80.,
                            height: 20.
                        }),
                    ],
                );
            }
        }
    }

    #[test]
    fn marking_quads_exact_line_fallback_excludes_whitespace_cells() {
        for direction in [
            HorizontalDirection::LeftToRight,
            HorizontalDirection::RightToLeft,
        ] {
            let rect = Rect {
                x: 10,
                y: 10,
                width: 80,
                height: 20,
            };
            let text = SelectionText::from_recognized(&RecognizedText {
                text: "A \tB".into(),
                regions: Vec::new(),
                words: vec![directional_word(
                    "?",
                    rect,
                    0,
                    HorizontalDirection::LeftToRight,
                )],
                lines: vec![RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    text: "A \tB".into(),
                    rect: Some(rect),
                    horizontal_direction: direction,
                }],
            });
            assert_eq!(text.lines[0].cells.len(), 4);
            assert_eq!(text.selected(0..4).unwrap(), "A \tB");
            assert_eq!(
                text.highlights(0..4),
                vec![crate::transform::corners(Bounds {
                    x: 10.,
                    y: 10.,
                    width: 80.,
                    height: 20.
                })],
            );
            assert_eq!(
                text.marking_quads(0..4),
                vec![
                    crate::transform::corners(Bounds {
                        x: 10.,
                        y: 10.,
                        width: 20.,
                        height: 20.
                    }),
                    crate::transform::corners(Bounds {
                        x: 70.,
                        y: 10.,
                        width: 20.,
                        height: 20.
                    }),
                ],
            );
            assert_eq!(text.selected(1..3).unwrap(), " \t");
            assert!(text.marking_quads(1..3).is_empty());
        }
    }

    #[test]
    fn marking_quads_native_rotation_preserves_visible_character_cells() {
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 200., y: 100. });
        annotation.text = " A B ".into();
        annotation.rotation = 90.;
        assert_eq!(
            annotation.local_bounds(),
            Bounds {
                x: 20.,
                y: 20.,
                width: 180.,
                height: 80.
            }
        );
        let text = SelectionText::from_annotations(&[annotation]).unwrap();
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].cells[1].range, 1..2);
        assert_eq!(text.lines[0].cells[3].range, 3..4);
        assert_eq!(text.selected(0..5).unwrap(), " A B ");
        assert_eq!(text.selected(1..2).unwrap(), "A");
        assert_eq!(text.selected(3..4).unwrap(), "B");
        let a = text.lines[0].cells[1].bounds;
        let b = text.lines[0].cells[3].bounds;
        // 中心固定 (110,60)，人工 90° 公式 (x',y')=(170-y,x-50)。
        // 字宽来自真实字体单元证据；oracle 不调用 marking_quads/highlights/Rotation.map。
        let expected = |cell: Bounds| {
            [
                Point {
                    x: 170. - cell.y,
                    y: cell.x - 50.,
                },
                Point {
                    x: 170. - cell.y,
                    y: cell.x + cell.width - 50.,
                },
                Point {
                    x: 170. - cell.y - cell.height,
                    y: cell.x + cell.width - 50.,
                },
                Point {
                    x: 170. - cell.y - cell.height,
                    y: cell.x - 50.,
                },
            ]
        };
        let marks = text.marking_quads(0..5);
        assert_eq!(marks.len(), 2);
        for (actual, wanted) in marks.iter().zip([expected(a), expected(b)]) {
            for (point, wanted_point) in actual.iter().zip(wanted) {
                assert!(point.distance(wanted_point) < 0.001);
            }
        }
        assert!(text.marking_quads(0..1).is_empty());
        assert!(text.marking_quads(2..3).is_empty());
        assert!(text.marking_quads(4..5).is_empty());
    }

    #[test]
    fn complete_marks_reject_missing_nonblank_geometry_across_lines_without_losing_copy() {
        let text = SelectionText::from_recognized(&RecognizedText {
            text: "A\nB".into(),
            regions: Vec::new(),
            words: vec![
                directional_word(
                    "A",
                    Rect {
                        x: 5,
                        y: 5,
                        width: 10,
                        height: 10,
                    },
                    0,
                    HorizontalDirection::LeftToRight,
                ),
                directional_word(
                    "B",
                    Rect {
                        x: 5,
                        y: 20,
                        width: 0,
                        height: 10,
                    },
                    1,
                    HorizontalDirection::LeftToRight,
                ),
            ],
            lines: Vec::new(),
        });
        assert!(text.has_geometry());
        assert_eq!(text.marking_quads(0..3).len(), 1);
        assert!(text.complete_marking_quads(0..3).is_err());
        assert!(text.complete_marking_quads(1..3).is_err());
        assert_eq!(text.selected(0..3).as_deref(), Some("A\nB"));
        assert_eq!(text.selected(2..3).as_deref(), Some("B"));
        assert_eq!(
            text.complete_marking_quads(0..1).unwrap(),
            vec![[
                Point { x: 5., y: 5. },
                Point { x: 15., y: 5. },
                Point { x: 15., y: 15. },
                Point { x: 5., y: 15. },
            ]]
        );
    }

    #[test]
    fn complete_marks_allow_unlocated_whitespace_between_valid_glyphs() {
        for direction in [
            HorizontalDirection::LeftToRight,
            HorizontalDirection::RightToLeft,
        ] {
            let body = "A\t \r\n\u{2003}B";
            let text = SelectionText::from_recognized(&RecognizedText {
                text: body.into(),
                regions: Vec::new(),
                lines: Vec::new(),
                words: vec![
                    directional_word(
                        "A",
                        Rect {
                            x: 5,
                            y: 5,
                            width: 10,
                            height: 10,
                        },
                        0,
                        direction,
                    ),
                    directional_word(
                        "B",
                        Rect {
                            x: 5,
                            y: 20,
                            width: 10,
                            height: 10,
                        },
                        1,
                        direction,
                    ),
                ],
            });
            let quads = text.complete_marking_quads(0..body.len()).unwrap();
            assert_eq!(
                quads,
                vec![
                    [
                        Point { x: 5., y: 5. },
                        Point { x: 15., y: 5. },
                        Point { x: 15., y: 15. },
                        Point { x: 5., y: 15. }
                    ],
                    [
                        Point { x: 5., y: 20. },
                        Point { x: 15., y: 20. },
                        Point { x: 15., y: 30. },
                        Point { x: 5., y: 30. }
                    ],
                ]
            );
            assert_eq!(text.selected(0..body.len()).as_deref(), Some(body));
            assert!(text.complete_marking_quads(1..body.len() - 1).is_err());
        }
    }

    #[test]
    fn complete_marks_require_whole_unicode_graphemes_not_only_utf8_boundaries() {
        let body = "e\u{301}👨‍👩‍👧🇨🇳";
        let text = SelectionText::from_recognized(&RecognizedText {
            text: body.into(),
            regions: Vec::new(),
            lines: Vec::new(),
            words: vec![directional_word(
                body,
                Rect {
                    x: 5,
                    y: 5,
                    width: 90,
                    height: 10,
                },
                0,
                HorizontalDirection::LeftToRight,
            )],
        });
        for range in [0..1, 1..3, 3..7, 7..21, 21..25, 0..2, 0..usize::MAX] {
            assert!(text.complete_marking_quads(range).is_err());
        }
        for (byte, glyph) in body.grapheme_indices(true) {
            assert!(text
                .complete_marking_quads(byte..byte + glyph.len())
                .is_ok());
        }
        assert_eq!(
            text.complete_marking_quads(0..body.len()).unwrap(),
            vec![[
                Point { x: 5., y: 5. },
                Point { x: 95., y: 5. },
                Point { x: 95., y: 15. },
                Point { x: 5., y: 15. },
            ]]
        );
        assert_eq!(text.selected(0..1).as_deref(), Some("e"));
        assert_eq!(text.selected(0..body.len()).as_deref(), Some(body));
    }

    #[test]
    fn complete_marks_reject_empty_whitespace_and_all_missing_geometry() {
        for body in ["", " \t\r\n", "missing"] {
            let text = SelectionText::from_recognized(&RecognizedText {
                text: body.into(),
                regions: Vec::new(),
                words: Vec::new(),
                lines: Vec::new(),
            });
            for range in [0..0, 0..body.len(), 0..body.len() + 1] {
                assert!(text.complete_marking_quads(range).is_err());
            }
            if !body.is_empty() {
                assert_eq!(text.selected(0..body.len()).as_deref(), Some(body));
            }
        }
    }

    #[test]
    fn complete_marks_keep_projected_reading_direction_and_native_rotation() {
        for coordinates in [
            [(10., 10.), (30., 10.), (30., 90.), (10., 90.)],
            [(30., 90.), (10., 90.), (10., 10.), (30., 10.)],
            [(30., 10.), (30., 90.), (10., 90.), (10., 10.)],
            [(10., 90.), (10., 10.), (30., 10.), (30., 90.)],
        ] {
            let quad = coordinates.map(|(x, y)| Point { x, y });
            let mut word = directional_word(
                "AB",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 80,
                },
                0,
                HorizontalDirection::LeftToRight,
            );
            word.geometry = Some(TextGeometry { quad });
            let text = SelectionText::from_recognized(&RecognizedText {
                text: "AB".into(),
                regions: Vec::new(),
                words: vec![word],
                lines: Vec::new(),
            });
            assert_eq!(text.complete_marking_quads(0..2).unwrap(), vec![quad]);
        }
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 200., y: 100. });
        annotation.text = " A B ".into();
        annotation.rotation = 90.;
        let text = SelectionText::from_annotations(&[annotation]).unwrap();
        let a = text.lines[0].cells[1].bounds;
        let b = text.lines[0].cells[3].bounds;
        let expected = |bounds: Bounds| {
            [
                Point {
                    x: 170. - bounds.y,
                    y: bounds.x - 50.,
                },
                Point {
                    x: 170. - bounds.y,
                    y: bounds.x + bounds.width - 50.,
                },
                Point {
                    x: 170. - bounds.y - bounds.height,
                    y: bounds.x + bounds.width - 50.,
                },
                Point {
                    x: 170. - bounds.y - bounds.height,
                    y: bounds.x - 50.,
                },
            ]
        };
        let quads = text.complete_marking_quads(0..5).unwrap();
        assert_eq!(quads.len(), 2);
        for (actual, wanted) in quads.into_iter().zip([expected(a), expected(b)]) {
            for (point, wanted_point) in actual.into_iter().zip(wanted) {
                assert!(point.distance(wanted_point) < 0.001);
            }
        }
    }

    #[test]
    fn marking_quads_empty_invalid_and_missing_geometry_never_guess() {
        let text = ocr();
        for range in [
            0..0,
            3..3,
            Range { start: 3, end: 1 },
            1..2,
            2..3,
            0..text.text.len() + 1,
            text.text.len()..text.text.len() + 1,
            0..usize::MAX,
        ] {
            assert!(text.selected(range.clone()).is_none());
            assert!(text.marking_quads(range).is_empty());
        }
        let no_geometry = SelectionText::from_recognized(&RecognizedText {
            text: "A B".into(),
            regions: vec![Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 20,
            }],
            words: vec![directional_word(
                "A",
                Rect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 20,
                },
                0,
                HorizontalDirection::LeftToRight,
            )],
            lines: Vec::new(),
        });
        assert_eq!(no_geometry.selected(0..3).unwrap(), "A B");
        assert!(!no_geometry.has_geometry());
        assert!(no_geometry.marking_quads(0..3).is_empty());
        let empty = SelectionText::from_recognized(&RecognizedText {
            text: String::new(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        });
        assert!(!empty.has_geometry());
        assert!(empty.marking_quads(0..0).is_empty());
    }

    #[test]
    fn ocr_reversed_cjk_drag_selects_both_words_in_each_direction() {
        let text = SelectionText::from_recognized(&RecognizedText {
            text: "中文".into(),
            regions: Vec::new(),
            words: vec![
                directional_word(
                    "中",
                    Rect {
                        x: 70,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
                directional_word(
                    "文",
                    Rect {
                        x: 10,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
            ],
            lines: Vec::new(),
        });
        for (down, up, expected_anchor) in [
            (Point { x: 89., y: 15. }, Point { x: 11., y: 15. }, 0),
            (Point { x: 11., y: 15. }, Point { x: 89., y: 15. }, 6),
        ] {
            let (anchor, range) = text.begin(down, 1).unwrap();
            assert_eq!(range, expected_anchor..expected_anchor);
            let selected = text.extend(&anchor, up);
            assert_eq!(selected, 0..6);
            assert_eq!(text.selected(selected.clone()).unwrap(), "中文");
            let highlights = text.highlights(selected);
            assert_eq!(highlights.len(), 2);
            assert_eq!(
                highlights[0],
                crate::transform::corners(Bounds {
                    x: 10.,
                    y: 10.,
                    width: 20.,
                    height: 20.
                })
            );
            assert_eq!(
                highlights[1],
                crate::transform::corners(Bounds {
                    x: 70.,
                    y: 10.,
                    width: 20.,
                    height: 20.
                })
            );
        }
        assert!(text.begin(Point { x: 50., y: 15. }, 1).is_none());
    }
    fn directional_word(
        text: &str,
        rect: Rect,
        line: usize,
        horizontal_direction: HorizontalDirection,
    ) -> RecognizedWord {
        RecognizedWord {
            geometry: None,
            text: text.into(),
            rect,
            line,
            horizontal_direction,
        }
    }
    #[test]
    fn ocr_reversed_latin_carets_highlight_the_logical_first_character() {
        let rect = Rect {
            x: 40,
            y: 10,
            width: 40,
            height: 20,
        };
        let text = SelectionText::from_recognized(&RecognizedText {
            text: "AB".into(),
            regions: Vec::new(),
            words: vec![directional_word(
                "AB",
                rect,
                0,
                HorizontalDirection::RightToLeft,
            )],
            lines: vec![RecognizedLine {
                graphemes: None,
                geometry: None,
                text: "AB".into(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::RightToLeft,
            }],
        });
        let (anchor, range) = text.begin(Point { x: 79., y: 15. }, 1).unwrap();
        assert_eq!(range, 0..0);
        let selected = text.extend(&anchor, Point { x: 59., y: 15. });
        assert_eq!(selected, 0..1);
        assert_eq!(text.selected(selected.clone()).unwrap(), "A");
        assert_eq!(
            text.highlights(selected),
            vec![crate::transform::corners(Bounds {
                x: 60.,
                y: 10.,
                width: 20.,
                height: 20.
            })]
        );
        let (anchor, range) = text.begin(Point { x: 41., y: 15. }, 1).unwrap();
        assert_eq!(range, 2..2);
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 59., y: 15. }))
                .unwrap(),
            "B"
        );
        for clicks in [2, 3] {
            let (_, range) = text.begin(Point { x: 75., y: 15. }, clicks).unwrap();
            assert_eq!(range, 0..2);
            assert_eq!(text.selected(range).unwrap(), "AB");
        }
    }
    #[test]
    fn ocr_mixed_horizontal_directions_preserve_graphemes_spaces_and_original_y() {
        let first = "中e\u{301}👨‍👩‍👧";
        let first_line = format!("{first}  X");
        let raw = format!("{first_line}\nnext");
        let mut text = SelectionText::from_recognized(&RecognizedText {
            text: raw.clone(),
            regions: Vec::new(),
            words: vec![
                directional_word(
                    first,
                    Rect {
                        x: 70,
                        y: 10,
                        width: 90,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
                directional_word(
                    "X",
                    Rect {
                        x: 10,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    0,
                    HorizontalDirection::RightToLeft,
                ),
                directional_word(
                    "next",
                    Rect {
                        x: 10,
                        y: 50,
                        width: 80,
                        height: 20,
                    },
                    1,
                    HorizontalDirection::LeftToRight,
                ),
            ],
            lines: Vec::new(),
        });
        let expected_clusters: Vec<_> = first
            .grapheme_indices(true)
            .map(|(byte, cluster)| byte..byte + cluster.len())
            .collect();
        assert_eq!(
            text.lines[0]
                .cells
                .iter()
                .take(3)
                .map(|cell| cell.range.clone())
                .collect::<Vec<_>>(),
            expected_clusters
        );
        assert_eq!(text.lines[0].cells.len(), 4);
        assert!(!text.has_text_at(Point { x: 50., y: 15. }));
        let (anchor, _) = text.begin(Point { x: 159., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 61., y: 15. }))
                .unwrap(),
            first
        );
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 39., y: 15. }))
                .unwrap(),
            format!("{first}  ")
        );
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 11., y: 15. }))
                .unwrap(),
            first_line
        );
        let (reverse, _) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&reverse, Point { x: 159., y: 15. }))
                .unwrap(),
            first_line
        );
        let (combining, _) = text.begin(Point { x: 129., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&combining, Point { x: 101., y: 15. }))
                .unwrap(),
            "e\u{301}"
        );
        let (emoji, _) = text.begin(Point { x: 99., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&emoji, Point { x: 71., y: 15. }))
                .unwrap(),
            "👨‍👩‍👧"
        );
        // 水平方向不翻转原图的上下越界合同，也不重排视觉行。
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 999., y: 999. }))
                .unwrap(),
            raw
        );
        let (last, _) = text.begin(Point { x: 89., y: 55. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&last, Point { x: -99., y: -99. }))
                .unwrap(),
            raw
        );
        text.translate(200., -7.);
        let (translated, range) = text.begin(Point { x: 359., y: 8. }, 1).unwrap();
        assert_eq!(range, 0..0);
        assert_eq!(
            text.selected(text.extend(&translated, Point { x: 211., y: 8. }))
                .unwrap(),
            first_line
        );
        assert_eq!(
            text.highlights(0..3),
            vec![crate::transform::corners(Bounds {
                x: 330.,
                y: 3.,
                width: 30.,
                height: 20.
            })]
        );
    }
    #[test]
    fn ocr_reversed_exact_line_fallback_keeps_direction_and_rejects_false_geometry() {
        let rect = Rect {
            x: 10,
            y: 10,
            width: 100,
            height: 20,
        };
        let source = |line_text: &str, line_rect: Option<Rect>| RecognizedText {
            text: "AB CD".into(),
            regions: Vec::new(),
            words: vec![directional_word(
                "?",
                rect,
                0,
                HorizontalDirection::LeftToRight,
            )],
            lines: vec![RecognizedLine {
                graphemes: None,
                geometry: None,
                text: line_text.into(),
                rect: line_rect,
                horizontal_direction: HorizontalDirection::RightToLeft,
            }],
        };
        let text = SelectionText::from_recognized(&source("AB CD", Some(rect)));
        let (anchor, range) = text.begin(Point { x: 109., y: 15. }, 1).unwrap();
        assert_eq!(range, 0..0);
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 91., y: 15. }))
                .unwrap(),
            "A"
        );
        assert_eq!(
            text.highlights(0..1),
            vec![crate::transform::corners(Bounds {
                x: 90.,
                y: 10.,
                width: 20.,
                height: 20.
            })]
        );
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 11., y: 15. }))
                .unwrap(),
            "AB CD"
        );
        assert!(text.begin(Point { x: 60., y: 15. }, 1).is_none());
        assert_eq!(text.begin(Point { x: 99., y: 15. }, 2).unwrap().1, 0..2);
        for (line_text, line_rect) in [
            ("different", Some(rect)),
            ("AB CD", None),
            ("AB CD", Some(Rect { width: 0, ..rect })),
            (
                "AB CD",
                Some(Rect {
                    x: u32::MAX,
                    ..rect
                }),
            ),
        ] {
            let invalid = SelectionText::from_recognized(&source(line_text, line_rect));
            assert!(!invalid.has_geometry());
            assert!(invalid.begin(Point { x: 109., y: 15. }, 1).is_none());
            assert!(invalid.highlights(0..5).is_empty());
            assert_eq!(invalid.selected(0..5).unwrap(), "AB CD");
        }
    }
    #[test]
    fn ocr_reversed_newline_marker_follows_final_cell_without_line_metadata() {
        for ending in ["\n", "\r\n"] {
            for include_line in [false, true] {
                let first_line = if ending == "\r\n" { "AB\r" } else { "AB" };
                let rect = Rect {
                    x: 10,
                    y: 10,
                    width: 40,
                    height: 20,
                };
                let text = SelectionText::from_recognized(&RecognizedText {
                    text: format!("AB{ending}next"),
                    regions: Vec::new(),
                    words: vec![
                        directional_word("AB", rect, 0, HorizontalDirection::RightToLeft),
                        directional_word(
                            "next",
                            Rect {
                                x: 10,
                                y: 50,
                                width: 80,
                                height: 20,
                            },
                            1,
                            HorizontalDirection::LeftToRight,
                        ),
                    ],
                    lines: if include_line {
                        vec![RecognizedLine {
                            graphemes: None,
                            geometry: None,
                            text: first_line.into(),
                            rect: Some(rect),
                            // 完整词的末停点优先，不能被未提供方向的行覆盖。
                            horizontal_direction: HorizontalDirection::LeftToRight,
                        }]
                    } else {
                        Vec::new()
                    },
                });
                let range = 0..2 + ending.len();
                assert_eq!(text.selected(range.clone()).unwrap(), format!("AB{ending}"));
                assert_eq!(
                    text.highlights(range),
                    vec![
                        crate::transform::corners(Bounds {
                            x: 10.,
                            y: 10.,
                            width: 40.,
                            height: 20.
                        }),
                        crate::transform::corners(Bounds {
                            x: 5.,
                            y: 10.,
                            width: 5.,
                            height: 20.
                        }),
                    ]
                );
            }
        }
    }
    #[test]
    fn ocr_drag_uses_grapheme_boundaries_and_preserves_exact_spaces() {
        let text = ocr();
        let (anchor, range) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        assert!(range.is_empty());
        let selected = text.extend(&anchor, Point { x: 148., y: 15. });
        assert_eq!(text.selected(selected).unwrap(), "中e\u{301}👨‍👩‍👧  X");
        assert_eq!(text.lines[0].cells.len(), 4);
    }
    #[test]
    fn ocr_incomplete_words_fall_back_to_exact_line_graphemes_and_preserve_crlf() {
        let first = "中e\u{301} 👨‍👩‍👧  X\r";
        let raw = format!("{first}\nnext");
        let recognized = RecognizedText {
            text: raw.clone(),
            regions: Vec::new(),
            words: vec![
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "不一致".into(),
                    rect: Rect {
                        x: 10,
                        y: 10,
                        width: 100,
                        height: 20,
                    },
                    line: 0,
                },
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "next".into(),
                    rect: Rect {
                        x: 10,
                        y: 50,
                        width: 80,
                        height: 20,
                    },
                    line: 1,
                },
            ],
            lines: vec![
                RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: first.into(),
                    rect: Some(Rect {
                        x: 10,
                        y: 10,
                        width: 320,
                        height: 20,
                    }),
                },
                RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "next".into(),
                    rect: None,
                },
            ],
        };
        let text = SelectionText::from_recognized(&recognized);
        assert_eq!(text.text, raw);
        assert_eq!(text.lines[0].cells.len(), first.graphemes(true).count());
        for cell in &text.lines[0].cells {
            assert_eq!(
                text.selected(cell.range.clone())
                    .unwrap()
                    .graphemes(true)
                    .count(),
                1
            );
        }
        let (anchor, _) = text.begin(Point { x: 10.1, y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 999., y: 999. }))
                .unwrap(),
            raw
        );
        // 可见字符遗漏也必须整体回退，不能让已有词的精细框遮住剩余原文。
        for partial in ["中e\u{301}", "X"] {
            let mut partial_source = RecognizedText {
                text: "中e\u{301}  X".into(),
                regions: Vec::new(),
                lines: vec![RecognizedLine {
                    graphemes: None,
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "中e\u{301}  X".into(),
                    rect: Some(Rect {
                        x: 10,
                        y: 10,
                        width: 320,
                        height: 20,
                    }),
                }],
                words: Vec::new(),
            };
            partial_source.words.push(RecognizedWord {
                geometry: None,
                horizontal_direction: Default::default(),
                text: partial.into(),
                rect: Rect {
                    x: 130,
                    y: 10,
                    width: 20,
                    height: 20,
                },
                line: 0,
            });
            let selection = SelectionText::from_recognized(&partial_source);
            assert_eq!(selection.lines[0].cells.len(), 5);
            assert_eq!(selection.lines[0].bounds.x, 10.);
            assert_eq!(selection.lines[0].bounds.width, 320.);
        }
    }
    #[test]
    fn ocr_mismatched_word_cannot_rebind_a_later_duplicate_to_the_first_word() {
        let source = RecognizedText {
            text: "A A".into(),
            regions: Vec::new(),
            words: vec![
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "?".into(),
                    rect: Rect {
                        x: 10,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    line: 0,
                },
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "A".into(),
                    rect: Rect {
                        x: 130,
                        y: 10,
                        width: 20,
                        height: 20,
                    },
                    line: 0,
                },
            ],
            lines: vec![RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A A".into(),
                rect: Some(Rect {
                    x: 10,
                    y: 10,
                    width: 140,
                    height: 20,
                }),
            }],
        };
        let text = SelectionText::from_recognized(&source);
        assert_eq!(text.lines[0].cells.len(), 3);
        assert_eq!(text.words, [0..1, 2..3]);
        let (_, second) = text.begin(Point { x: 140., y: 15. }, 2).unwrap();
        assert_eq!(second, 2..3);
        assert_eq!(text.selected(second).unwrap(), "A");
    }
    #[test]
    fn ocr_incomplete_mapping_without_an_exact_valid_line_box_keeps_only_raw_text() {
        let valid = Rect {
            x: 10,
            y: 10,
            width: 100,
            height: 20,
        };
        for line in [
            None,
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A B".into(),
                rect: None,
            }),
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "different".into(),
                rect: Some(valid),
            }),
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A B".into(),
                rect: Some(Rect { width: 0, ..valid }),
            }),
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A B".into(),
                rect: Some(Rect { height: 0, ..valid }),
            }),
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A B".into(),
                rect: Some(Rect {
                    x: u32::MAX - 10,
                    width: 20,
                    ..valid
                }),
            }),
            Some(RecognizedLine {
                graphemes: None,
                geometry: None,
                horizontal_direction: Default::default(),
                text: "A B".into(),
                rect: Some(Rect {
                    y: u32::MAX - 10,
                    height: 20,
                    ..valid
                }),
            }),
        ] {
            let source = RecognizedText {
                text: "A B".into(),
                regions: Vec::new(),
                lines: line.into_iter().collect(),
                words: vec![RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "A".into(),
                    rect: valid,
                    line: 0,
                }],
            };
            let text = SelectionText::from_recognized(&source);
            assert!(!text.has_geometry());
            assert!(text.begin(Point { x: 11., y: 15. }, 1).is_none());
            assert!(text.words.is_empty());
            assert!(text.highlights(0..3).is_empty());
            assert_eq!(text.selected(0..3).unwrap(), "A B");
        }
        // 一个词自身的无效几何同样不能贡献残缺选区。
        for rect in [
            Rect { width: 0, ..valid },
            Rect {
                x: u32::MAX,
                ..valid
            },
        ] {
            let text = SelectionText::from_recognized(&RecognizedText {
                text: "A".into(),
                regions: Vec::new(),
                lines: Vec::new(),
                words: vec![RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "A".into(),
                    rect,
                    line: 0,
                }],
            });
            assert!(!text.has_geometry());
            assert_eq!(text.selected(0..1).unwrap(), "A");
        }
    }
    #[test]
    fn drag_continues_through_gap_and_outside_in_both_directions() {
        let text = ocr();
        let (anchor, _) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 115., y: 15. }))
                .unwrap(),
            "中e\u{301}👨‍👩‍👧"
        );
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 999., y: 999. }))
                .unwrap(),
            text.text
        );
        let (anchor, _) = text.begin(Point { x: 89., y: 55. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: -99., y: -99. }))
                .unwrap(),
            text.text
        );
    }
    #[test]
    fn blank_down_does_not_select_but_double_word_and_triple_line_do() {
        let text = ocr();
        assert!(text.begin(Point { x: 115., y: 15. }, 1).is_none());
        let (_, range) = text.begin(Point { x: 45., y: 15. }, 2).unwrap();
        assert_eq!(text.selected(range).unwrap(), "中e\u{301}👨‍👩‍👧");
        let (_, range) = text.begin(Point { x: 45., y: 15. }, 3).unwrap();
        assert_eq!(text.selected(range).unwrap(), "中e\u{301}👨‍👩‍👧  X");
    }
    #[test]
    fn native_text_uses_actual_row_stops_and_preserves_newlines() {
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 180., y: 160. });
        annotation.text = "hello  中e\u{301}\nsecond".into();
        let text = SelectionText::from_annotations(&[annotation.clone()]).unwrap();
        let layout = TextLayout::new(&annotation).unwrap();
        let origin = annotation.local_bounds();
        for (line, row) in text.lines.iter().zip(&layout.rows) {
            let mut boundaries: Vec<_> = annotation
                .text
                .grapheme_indices(true)
                .map(|(byte, _)| byte)
                .collect();
            boundaries.push(annotation.text.len());
            assert_eq!(
                line.stops,
                row.caret_stops()
                    .into_iter()
                    .filter(|(byte, _)| boundaries.contains(byte))
                    .map(|(byte, x)| (byte, x + origin.x))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(text.selected(0..text.text.len()).unwrap(), annotation.text);
        assert!(text.highlights(0..text.text.len()).len() >= 2);
    }
    #[test]
    fn reversed_native_rotation_hits_same_transformed_geometry() {
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 300., y: 90. });
        annotation.text = "hello world".into();
        annotation.rotation = 35.;
        let text = SelectionText::from_annotations(&[annotation.clone()]).unwrap();
        let first = &text.lines[0].cells[0];
        let point = annotation.to_world(Point {
            x: first.bounds.x + first.bounds.width * 0.25,
            y: first.bounds.y + first.bounds.height * 0.5,
        });
        let (_, range) = text.begin(point, 2).unwrap();
        assert_eq!(text.selected(range.clone()).unwrap(), "hello");
        assert_eq!(
            text.highlights(range)[0][0],
            annotation.to_world(Point {
                x: first.bounds.x,
                y: first.bounds.y
            })
        );
    }
    #[test]
    fn invalid_byte_selection_never_panics_or_corrupts_unicode() {
        let text = ocr();
        assert!(text.selected(1..2).is_none());
        assert!(text.selected(0..usize::MAX).is_none());
        assert!(text.selected(0..0).is_none());
    }
    #[test]
    fn cropped_ocr_translation_keeps_copy_and_hit_on_canvas_origin() {
        let mut text = ocr();
        text.translate(240., 180.);
        assert!(!text.has_text_at(Point { x: 11., y: 15. }));
        assert!(text.has_text_at(Point { x: 251., y: 195. }));
        let (_, range) = text.begin(Point { x: 251., y: 195. }, 3).unwrap();
        assert_eq!(text.selected(range.clone()).unwrap(), "中e\u{301}👨‍👩‍👧  X");
        assert_eq!(text.highlights(range)[0][0], Point { x: 250., y: 190. });
    }
    #[test]
    fn native_combining_and_zwj_clusters_are_indivisible() {
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 900., y: 100. });
        annotation.text = "e\u{301}👨‍👩‍👧🇨🇳X".into();
        let text = SelectionText::from_annotations(&[annotation.clone()]).unwrap();
        let mut boundaries: Vec<_> = annotation
            .text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .collect();
        boundaries.push(annotation.text.len());
        assert!(text
            .lines
            .iter()
            .flat_map(|line| &line.stops)
            .all(|(byte, _)| boundaries.contains(byte)));
        for line in &text.lines {
            for cell in &line.cells {
                assert!(
                    boundaries.contains(&cell.range.start) && boundaries.contains(&cell.range.end)
                );
                assert!(text.selected(cell.range.clone()).is_some());
            }
        }
    }
    #[test]
    fn long_to_short_row_drag_clamps_the_row_under_the_pointer() {
        let text = SelectionText::from_recognized(&RecognizedText {
            text: "longlonglong\nx".into(),
            regions: Vec::new(),
            lines: Vec::new(),
            words: vec![
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "longlonglong".into(),
                    rect: Rect {
                        x: 10,
                        y: 10,
                        width: 300,
                        height: 20,
                    },
                    line: 0,
                },
                RecognizedWord {
                    geometry: None,
                    horizontal_direction: Default::default(),
                    text: "x".into(),
                    rect: Rect {
                        x: 10,
                        y: 50,
                        width: 40,
                        height: 20,
                    },
                    line: 1,
                },
            ],
        });
        let (anchor, _) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 250., y: 55. }))
                .unwrap(),
            "longlonglong\nx"
        );
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 10000., y: 55. }))
                .unwrap(),
            "longlonglong\nx"
        );
    }
    #[test]
    fn native_blank_rows_and_trailing_newline_keep_exact_range_and_highlight() {
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        annotation.update(Point { x: 500., y: 200. });
        annotation.text = "a\n\nb\n".into();
        let text = SelectionText::from_annotations(&[annotation]).unwrap();
        assert_eq!(text.lines.len(), 4);
        assert!(text.lines[1].range.is_empty() && text.lines[3].range.is_empty());
        let first = &text.lines[0].cells[0];
        let (anchor, _) = text
            .begin(
                Point {
                    x: first.bounds.x + 0.1,
                    y: first.bounds.y + 1.,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            text.selected(text.extend(&anchor, Point { x: 9999., y: 9999. }))
                .unwrap(),
            "a\n\nb\n"
        );
        assert!(text
            .highlights(0..text.text.len())
            .iter()
            .any(|quad| quad[0].y == text.lines[1].bounds.y));
        assert!(!text.has_text_at(Point {
            x: text.lines[1].bounds.x,
            y: text.lines[1].bounds.y + 1.
        }));
    }
    #[test]
    fn capture_loss_preserves_selection_and_later_hover_cannot_extend_it() {
        let text = ocr();
        let (start, range) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        let mut anchor = Some(start);
        let mut selected = Some(range);
        assert!(text.drag(
            &mut anchor,
            &mut selected,
            Point { x: 148., y: 15. },
            true,
            true
        ));
        let before = selected.clone();
        assert!(text.drag(
            &mut anchor,
            &mut selected,
            Point { x: 999., y: 999. },
            true,
            false
        ));
        assert_eq!(selected, before);
        assert!(anchor.is_none());
        assert!(!text.drag(
            &mut anchor,
            &mut selected,
            Point { x: 999., y: 999. },
            true,
            true
        ));
        assert_eq!(selected, before);
    }
    #[test]
    fn release_outside_stops_selection_even_if_capture_is_still_owned() {
        let text = ocr();
        let (start, range) = text.begin(Point { x: 11., y: 15. }, 2).unwrap();
        let mut anchor = Some(start);
        let mut selected = Some(range.clone());
        assert!(text.drag(
            &mut anchor,
            &mut selected,
            Point { x: 999., y: 999. },
            false,
            true
        ));
        assert!(anchor.is_none());
        assert_eq!(selected, Some(range));
    }
    #[test]
    fn release_samples_final_point_without_move_and_rejects_invalid_release() {
        let text = ocr();
        for (down, up) in [
            (Point { x: 11., y: 15. }, Point { x: 999., y: 999. }),
            (Point { x: 89., y: 55. }, Point { x: -99., y: -99. }),
        ] {
            let (start, range) = text.begin(down, 1).unwrap();
            let mut anchor = Some(start);
            let mut selected = Some(range);
            assert!(text.finish_drag(&mut anchor, &mut selected, up, true));
            assert!(anchor.is_none());
            assert_eq!(text.selected(selected.unwrap()).unwrap(), text.text);
        }
        let (start, range) = text.begin(Point { x: 11., y: 15. }, 2).unwrap();
        let mut anchor = Some(start);
        let mut selected = Some(range.clone());
        assert!(text.finish_drag(
            &mut anchor,
            &mut selected,
            Point { x: 999., y: 999. },
            false
        ));
        assert!(anchor.is_none());
        assert_eq!(selected, Some(range));
        assert!(!text.finish_drag(&mut anchor, &mut selected, Point { x: 999., y: 999. }, true));
    }
    #[test]
    fn native_overlay_rejects_occlusion_and_excessive_object_batches() {
        let mut first = Annotation::new(Tool::Text, Point { x: 20., y: 20. }, Default::default());
        first.update(Point { x: 200., y: 90. });
        first.text = "original".into();
        let mut second = first.clone();
        second.translate(0., 80.);
        assert!(SelectionText::native_eligible(&[
            first.clone(),
            second.clone()
        ]));
        second.translate(0., -60.);
        assert!(!SelectionText::native_eligible(&[first.clone(), second]));
        assert!(!SelectionText::native_eligible(&vec![first.clone(); 65]));
        first.text = "x".repeat(65537);
        assert!(!SelectionText::native_eligible(&[first]));
    }
}
