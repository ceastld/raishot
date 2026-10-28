//! 表格 UI 的单次识别缓存；只持原文、来源键与 TSV，不拥有窗口或剪贴板。
use crate::recognition::RecognizedText;

pub(crate) struct TableCopy<K> {
    source: K,
    generation: u64,
    original: String,
    table: Option<String>,
    grid: Option<crate::table_grid::TableGrid>,
    /// 表格只占部分识别行（正文中夹着的表格）时的来源行号，升序；独立表格为 None。
    region: Option<Vec<usize>>,
    structure_task: bool,
}

impl<K: PartialEq> TableCopy<K> {
    /// 独立表格任务已在后台择取结构，不再用旧文字矩阵覆盖模型的行列与合并格。
    pub(crate) fn with_structure(
        recognized: &RecognizedText,
        source: K,
        generation: u64,
        grid: Option<crate::table_grid::TableGrid>,
        region: Option<Vec<usize>>,
    ) -> Self {
        let valid_region = region.as_ref().is_none_or(|lines| {
            !lines.is_empty()
                && lines.iter().all(|line| *line < recognized.lines.len())
                && lines.windows(2).all(|pair| pair[0] < pair[1])
        });
        let grid = grid.filter(|grid| valid_region && grid.validate());
        let table = grid.as_ref().and_then(crate::table_export::tsv);
        let region = grid.as_ref().and(region);
        Self {
            source,
            generation,
            original: recognized.text.clone(),
            table,
            grid,
            region,
            structure_task: true,
        }
    }

    pub(crate) fn is_structure_result(&self, source: &K, generation: u64, body: &str) -> bool {
        self.structure_task
            && self.source == *source
            && self.generation == generation
            && self.original == body
    }

    /// 包括 None 的保守判定均在 OCR 安装时缓存，渲染与点击不重新运行表格算法。
    pub(crate) fn new(recognized: &RecognizedText, source: K, generation: u64) -> Self {
        Self::with_image(recognized, source, generation, None)
    }
    pub(crate) fn with_image(
        recognized: &RecognizedText,
        source: K,
        generation: u64,
        image: Option<&crate::capture::Image>,
    ) -> Self {
        let detected = image.and_then(crate::table_lines::detect);
        Self::with_lines(recognized, source, generation, detected.as_ref())
    }
    /// 线检测已在后台按同一张识别图片算好时使用，避免在界面线程扫描像素。
    pub(crate) fn with_lines(
        recognized: &RecognizedText,
        source: K,
        generation: u64,
        detected: Option<&crate::table_lines::DetectedLines>,
    ) -> Self {
        if let Some(grid) = crate::table_layout::recognize_detected(recognized, detected) {
            return Self::from_grid(recognized, source, generation, Some(grid));
        }
        let region = crate::table_layout::recognize_region(recognized, detected);
        let (grid, lines) = region.map_or((None, None), |(grid, lines)| (Some(grid), Some(lines)));
        let mut cache = Self::from_grid(recognized, source, generation, grid);
        if cache.grid.is_some() {
            cache.region = lines;
        }
        cache
    }
    /// 用户明确选择表格识别：可信候选不足时放宽规则，尽量给出表格。
    pub(crate) fn forced(
        recognized: &RecognizedText,
        source: K,
        generation: u64,
        detected: Option<&crate::table_lines::DetectedLines>,
    ) -> Self {
        let auto = Self::with_lines(recognized, source, generation, detected);
        if auto.grid.is_some() {
            return auto;
        }
        let grid = crate::table_layout::recognize_forced(recognized, detected);
        Self::from_grid(recognized, auto.source, generation, grid)
    }
    fn from_grid(
        recognized: &RecognizedText,
        source: K,
        generation: u64,
        mut grid: Option<crate::table_grid::TableGrid>,
    ) -> Self {
        let mut table = grid.as_ref().and_then(crate::table_export::tsv);
        if let Some(legacy) = recognized.copy_table() {
            // 倒序、RTL 和 180° 来源仍按旧识别顺序复制；空间网格不能重排可信 TSV。
            let spatial_text = crate::table_grid::TableGrid::from_legacy(recognized, &legacy)
                .as_ref()
                .and_then(crate::table_export::tsv);
            if table.is_none() || spatial_text.as_deref() != Some(legacy.text.as_str()) {
                table = Some(legacy.text);
                grid = None;
            }
        }
        Self {
            source,
            generation,
            original: recognized.text.clone(),
            table,
            grid,
            region: None,
            structure_task: false,
        }
    }

    pub(crate) fn text(&self, source: &K, generation: u64, body: &str) -> Option<&str> {
        (self.source == *source && self.generation == generation && self.original == body)
            .then_some(self.table.as_deref())
            .flatten()
    }

    /// 打开/收起全文输入器仅换 UI frame；正文未变且代次前进时可保留原判定。
    pub(crate) fn grid(
        &self,
        source: &K,
        generation: u64,
        body: &str,
    ) -> Option<&crate::table_grid::TableGrid> {
        self.text(source, generation, body)?;
        self.grid.as_ref()
    }

    /// 表格只占部分识别行时的来源行号；独立表格或无表格为 None。
    pub(crate) fn region(&self, source: &K, generation: u64, body: &str) -> Option<&[usize]> {
        self.grid(source, generation, body)?;
        self.region.as_deref()
    }

    /// 打开/收起全文输入器仅换 UI frame；正文未变且代次前进时可保留原判定。
    pub(crate) fn advance_generation(
        &mut self,
        source: &K,
        before: u64,
        after: u64,
        body: &str,
    ) -> bool {
        if self.source != *source
            || self.generation != before
            || after <= before
            || self.original != body
        {
            return false;
        }
        self.generation = after;
        true
    }

    /// 仅供宿主已合格的自有标记/Undo 来源重绑定；正文/代次/原键不符一律拒绝。
    pub(crate) fn rebase(
        &mut self,
        before: Option<&K>,
        after: K,
        generation: u64,
        body: &str,
    ) -> bool {
        if before != Some(&self.source) || generation != self.generation || body != self.original {
            return false;
        }
        self.source = after;
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        annotation::Point,
        geometry::Rect,
        recognition::{HorizontalDirection, RecognizedLine, RecognizedWord, TextGeometry},
    };

    // 只构造 DTO 输入；被测判定就是生产 RecognizedText::copy_table，不镜像表格算法或 Host。
    pub(crate) fn recognized() -> RecognizedText {
        let cells = [" 甲🙂 ", "0", "乙\"项", "0012", "丙项", "-3.50%"];
        let mut result = RecognizedText {
            text: cells.join("\n"),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        };
        for (index, text) in cells.iter().enumerate() {
            let rect = Rect {
                x: 10 + (index % 2) as u32 * 150,
                y: 10 + (index / 2) as u32 * 40,
                width: 60,
                height: 20,
            };
            let geometry = TextGeometry {
                quad: [
                    Point {
                        x: rect.x as f32,
                        y: rect.y as f32,
                    },
                    Point {
                        x: (rect.x + rect.width) as f32,
                        y: rect.y as f32,
                    },
                    Point {
                        x: (rect.x + rect.width) as f32,
                        y: (rect.y + rect.height) as f32,
                    },
                    Point {
                        x: rect.x as f32,
                        y: (rect.y + rect.height) as f32,
                    },
                ],
            };
            result.regions.push(rect);
            result.words.push(RecognizedWord {
                text: (*text).into(),
                rect,
                line: index,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            });
            result.lines.push(RecognizedLine {
                graphemes: None,
                text: (*text).into(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            });
        }
        result
    }

    #[test]
    fn model_table_cache_preserves_structure_instead_of_legacy_text() {
        let source = recognized();
        let legacy = source.copy_table().unwrap();
        let mut grid = crate::table_grid::TableGrid::from_legacy(&source, &legacy).unwrap();
        grid.source = crate::table_grid::TableSource::Model;
        grid.cells[0].text = "model\nwrapped".into();
        let expected = crate::table_export::tsv(&grid).unwrap();
        assert_ne!(expected, legacy.text);
        let cache = TableCopy::with_structure(&source, 7, 3, Some(grid), Some(vec![0, 1, 2]));
        assert_eq!(cache.text(&7, 3, &source.text), Some(expected.as_str()));
        assert_eq!(
            cache.grid(&7, 3, &source.text).unwrap().source,
            crate::table_grid::TableSource::Model
        );
        assert_eq!(
            cache.region(&7, 3, &source.text),
            Some([0, 1, 2].as_slice())
        );
        assert!(cache.grid(&7, 4, &source.text).is_none());
        assert!(cache.is_structure_result(&7, 3, &source.text));
        assert!(!cache.is_structure_result(&7, 4, &source.text));
    }

    #[test]
    fn model_table_cache_rejects_invalid_source_line_membership() {
        let source = recognized();
        let legacy = source.copy_table().unwrap();
        let grid = crate::table_grid::TableGrid::from_legacy(&source, &legacy).unwrap();
        for region in [vec![], vec![1, 1], vec![2, 1], vec![source.lines.len()]] {
            let cache = TableCopy::with_structure(&source, (), 0, Some(grid.clone()), Some(region));
            assert!(cache.text(&(), 0, &source.text).is_none());
            assert!(cache.grid(&(), 0, &source.text).is_none());
        }
    }

    #[test]
    fn inline_two_column_result_reaches_the_same_full_table_ui_cache() {
        let rows = [("甲", "0"), ("乙", "0012"), ("丙", "-3.50%")];
        let mut result = RecognizedText {
            text: rows
                .iter()
                .map(|(label, value)| format!("{label}    {value}"))
                .collect::<Vec<_>>()
                .join("\n"),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        };
        let geometry = |rect: Rect| TextGeometry {
            quad: [
                Point {
                    x: rect.x as f32,
                    y: rect.y as f32,
                },
                Point {
                    x: (rect.x + rect.width) as f32,
                    y: rect.y as f32,
                },
                Point {
                    x: (rect.x + rect.width) as f32,
                    y: (rect.y + rect.height) as f32,
                },
                Point {
                    x: rect.x as f32,
                    y: (rect.y + rect.height) as f32,
                },
            ],
        };
        for (row, (label, value)) in rows.iter().enumerate() {
            let parent = Rect {
                x: 10,
                y: 10 + row as u32 * 40,
                width: 180,
                height: 20,
            };
            for (text, x, width) in [(*label, 10, 20), (*value, 110, 80)] {
                let rect = Rect { x, width, ..parent };
                result.regions.push(rect);
                result.words.push(RecognizedWord {
                    text: text.into(),
                    rect,
                    line: row,
                    horizontal_direction: HorizontalDirection::LeftToRight,
                    geometry: Some(geometry(rect)),
                });
            }
            result.lines.push(RecognizedLine {
                graphemes: None,
                text: format!("{label}    {value}"),
                rect: Some(parent),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry(parent)),
            });
        }
        let original = result.text.clone();
        let cache = TableCopy::new(&result, (3, 9), 6);
        assert_eq!(
            cache.text(&(3, 9), 6, &original),
            Some("甲\t0\r\n乙\t0012\r\n丙\t-3.50%")
        );
        assert_eq!(result.text.as_bytes(), original.as_bytes());
        assert!(cache.text(&(3, 10), 6, &original).is_none());
        assert!(cache.text(&(3, 9), 7, &original).is_none());
        result.words[0].geometry = None;
        let unavailable = TableCopy::new(&result, (3, 9), 6);
        assert!(unavailable.text(&(3, 9), 6, &original).is_none());
        assert_eq!(result.text.as_bytes(), original.as_bytes());
    }

    #[test]
    fn table_cache_preserves_complete_source_bytes_and_real_geometry() {
        let result = recognized();
        let original = result.text.clone();
        let words = result
            .words
            .iter()
            .map(|word| (word.text.clone(), word.rect, word.line, word.geometry))
            .collect::<Vec<_>>();
        let lines = result
            .lines
            .iter()
            .map(|line| (line.text.clone(), line.rect, line.geometry))
            .collect::<Vec<_>>();
        let regions = result.regions.clone();
        let cache = TableCopy::new(&result, 7, 2);
        assert_eq!(
            cache.text(&7, 2, &result.text),
            Some(" 甲🙂 \t0\r\n\"乙\"\"项\"\t0012\r\n丙项\t-3.50%")
        );
        assert_eq!(result.text.as_bytes(), original.as_bytes());
        assert_eq!(result.regions, regions);
        assert_eq!(
            result
                .words
                .iter()
                .map(|word| (word.text.clone(), word.rect, word.line, word.geometry))
                .collect::<Vec<_>>(),
            words
        );
        assert_eq!(
            result
                .lines
                .iter()
                .map(|line| (line.text.clone(), line.rect, line.geometry))
                .collect::<Vec<_>>(),
            lines
        );
    }

    #[test]
    fn insufficient_geometry_caches_none_without_changing_body() {
        let mut result = recognized();
        result.words[0].geometry = None;
        let body = result.text.clone();
        let cache = TableCopy::new(&result, 1, 1);
        assert!(cache.text(&1, 1, &body).is_none());
        assert!(cache.text(&1, 1, &body).is_none());
        assert_eq!(body.as_bytes(), result.text.as_bytes());
    }

    #[test]
    fn stale_source_generation_and_changed_body_cannot_publish_old_geometry() {
        let result = recognized();
        let mut cache = TableCopy::new(&result, 1, 3);
        assert!(cache.text(&2, 3, &result.text).is_none());
        assert!(cache.text(&1, 4, &result.text).is_none());
        assert!(cache.text(&1, 3, &(result.text.clone() + " ")).is_none());
        assert!(cache.text(&1, 3, &result.text).is_some());
        assert!(!cache.advance_generation(&2, 3, 4, &result.text));
        assert!(!cache.advance_generation(&1, 3, 4, "edited"));
        assert!(!cache.advance_generation(&1, 2, 4, &result.text));
        assert!(!cache.advance_generation(&1, 3, 0, &result.text));
        assert!(cache.advance_generation(&1, 3, 4, &result.text));
        assert!(cache.text(&1, 3, &result.text).is_none());
        assert!(cache.text(&1, 4, &result.text).is_some());
    }

    #[test]
    fn qualified_rebase_requires_exact_previous_source_body_and_generation() {
        let result = recognized();
        let mut cache = TableCopy::new(&result, 1, 3);
        assert!(!cache.rebase(None, 2, 3, &result.text));
        assert!(!cache.rebase(Some(&9), 2, 3, &result.text));
        assert!(!cache.rebase(Some(&1), 2, 4, &result.text));
        assert!(!cache.rebase(Some(&1), 2, 3, "edited"));
        assert!(cache.text(&1, 3, &result.text).is_some());
        assert!(cache.rebase(Some(&1), 2, 3, &result.text));
        assert!(cache.text(&1, 3, &result.text).is_none());
        assert!(cache.text(&2, 3, &result.text).is_some());
    }

    #[test]
    fn pin_table_publication_failure_and_retry_keep_same_complete_tsv() {
        use super::super::ocr_clipboard::{publish_text, Content};
        let result = recognized();
        let cache = TableCopy::new(&result, 1, 3);
        let payload = cache.text(&1, 3, &result.text).unwrap();
        let calls = std::cell::Cell::new(0);
        let first = publish_text(payload, Content::Body, |text| {
            calls.set(calls.get() + 1);
            assert_eq!(text.as_bytes(), payload.as_bytes());
            assert_eq!(text.as_ptr(), payload.as_ptr());
            Err::<(), _>("真实适配器失败模型")
        });
        assert_eq!(first, Err("真实适配器失败模型"));
        assert_eq!(calls.get(), 1);
        assert_eq!(
            cache.text(&1, 3, &result.text).unwrap().as_ptr(),
            payload.as_ptr()
        );
        let retry = publish_text(payload, Content::Body, |text| {
            calls.set(calls.get() + 1);
            assert_eq!(text.as_bytes(), payload.as_bytes());
            Ok::<(), &str>(())
        });
        assert_eq!(retry, Ok(true));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn table_cache_descending_ltr_rows_preserve_legacy_tsv_without_grid() {
        let mut source = recognized();
        for index in 0..source.lines.len() {
            let mut rect = source.lines[index].rect.unwrap();
            let new_y = 10 + (2 - index / 2) as u32 * 40;
            let mut geometry = source.lines[index].geometry.unwrap();
            for point in &mut geometry.quad {
                point.y += new_y as f32 - rect.y as f32;
            }
            rect.y = new_y;
            source.lines[index].rect = Some(rect);
            source.lines[index].geometry = Some(geometry);
            source.words[index].rect = rect;
            source.words[index].geometry = Some(geometry);
            source.regions[index] = rect;
        }
        let expected = " 甲🙂 \t0\r\n\"乙\"\"项\"\t0012\r\n丙项\t-3.50%";
        assert_eq!(source.copy_table().unwrap().text, expected);
        let cache = TableCopy::new(&source, 1, 3);
        assert_eq!(cache.text(&1, 3, &source.text), Some(expected));
        assert!(cache.grid(&1, 3, &source.text).is_none());
    }

    #[test]
    fn table_cache_keeps_ruled_empty_column_instead_of_legacy_two_columns() {
        let source = recognized();
        let legacy = source.copy_table().unwrap();
        let mut image = crate::capture::Image {
            width: 240,
            height: 125,
            bgra: vec![255; 240 * 125 * 4],
            readback_ms: 0.,
        };
        for y in 0..image.height {
            for x in 0..image.width {
                if ([5, 100, 145, 235].contains(&x) && (5..=120).contains(&y))
                    || ([5, 40, 80, 120].contains(&y) && (5..=235).contains(&x))
                {
                    let index = (y * image.width + x) as usize * 4;
                    image.bgra[index..index + 3].fill(0);
                }
            }
        }
        let detected = crate::table_lines::detect(&image).unwrap().grid.unwrap();
        assert_eq!((detected.rows(), detected.columns()), (3, 3));
        let cache = TableCopy::with_image(&source, 1, 3, Some(&image));
        let grid = cache.grid(&1, 3, &source.text).unwrap();
        assert_eq!((grid.rows(), grid.columns()), (3, 3));
        assert_eq!(grid.source, crate::table_grid::TableSource::Lines);
        assert_ne!(cache.text(&1, 3, &source.text), Some(legacy.text.as_str()));
        assert_eq!(grid.cells[1].text, "");
    }

    #[test]
    fn table_cache_rotated_rtl_rows_preserve_legacy_tsv_without_grid() {
        let mut source = recognized();
        for index in 0..source.lines.len() {
            let mut rect = source.lines[index].rect.unwrap();
            rect.x = 10 + (1 - index % 2) as u32 * 150;
            rect.y = 10 + (2 - index / 2) as u32 * 40;
            let left = rect.x as f32;
            let top = rect.y as f32;
            let right = (rect.x + rect.width) as f32;
            let bottom = (rect.y + rect.height) as f32;
            // 复用旧表格合同的 180° 四角顺序：右下、左下、左上、右上。
            let geometry = TextGeometry {
                quad: [
                    Point {
                        x: right,
                        y: bottom,
                    },
                    Point { x: left, y: bottom },
                    Point { x: left, y: top },
                    Point { x: right, y: top },
                ],
            };
            source.lines[index].rect = Some(rect);
            source.lines[index].geometry = Some(geometry);
            source.lines[index].horizontal_direction = HorizontalDirection::RightToLeft;
            source.words[index].rect = rect;
            source.words[index].geometry = Some(geometry);
            source.words[index].horizontal_direction = HorizontalDirection::RightToLeft;
            source.regions[index] = rect;
        }
        let expected = " 甲🙂 \t0\r\n\"乙\"\"项\"\t0012\r\n丙项\t-3.50%";
        let legacy = source.copy_table().unwrap();
        assert_eq!(legacy.text, expected);
        assert_eq!(
            legacy.horizontal_direction,
            HorizontalDirection::RightToLeft
        );
        let cache = TableCopy::new(&source, 1, 3);
        assert_eq!(cache.text(&1, 3, &source.text), Some(expected));
        assert!(cache.grid(&1, 3, &source.text).is_none());
    }
}
