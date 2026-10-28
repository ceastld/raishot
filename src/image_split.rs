//! 内部分割工具：先建立有限计划，预览和逐项保存使用同一行优先编号。
use crate::{
    capture::Image,
    image_tools::{
        allocate_pixels, check_cancel, require_request, validate_input, validate_output,
        ProcessedImage, SavedImage, ToolError, ToolLimits, ToolResult,
    },
    save_options::SaveEncoding,
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AxisSplit {
    EqualParts(u32),
    /// 每片像素数；最后一片保留不足整片的余数。
    Pixels(u32),
    /// 整数比例；累计边界向下取整，末边界固定为原图尺寸。
    Weights(Vec<u32>),
}

/// 自定义行列比例；只接受冒号或逗号分隔的正整数，不省略空项。
pub(crate) fn parse_weights(text: &str, max_items: usize) -> ToolResult<Vec<u32>> {
    const MAX_TEXT_BYTES: usize = 64 * 1024;
    let invalid = || {
        ToolError::Invalid(
            tr!(
                "比例须为冒号或逗号分隔的正整数，且项数不能超过处理上限",
                "Use positive integer ratios separated by colons or commas, within the item limit"
            )
            .into(),
        )
    };
    if text.len() > MAX_TEXT_BYTES {
        return Err(ToolError::Invalid(
            tr!("比例文本过长", "Ratio text is too long").into(),
        ));
    }
    let terms = || text.split([':', ',', '：', '，']);
    let count = terms().count();
    if max_items == 0 || count > max_items || text.trim().is_empty() {
        return Err(invalid());
    }
    let mut weights = Vec::new();
    weights.try_reserve_exact(count).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存解析分割比例",
                "Not enough memory to parse split ratios"
            )
            .into(),
        )
    })?;
    for term in terms() {
        let weight = term.trim().parse::<u32>().map_err(|_| invalid())?;
        if weight == 0 {
            return Err(invalid());
        }
        weights.push(weight);
    }
    Ok(weights)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SplitOptions {
    pub columns: AxisSplit,
    pub rows: AxisSplit,
}

impl SplitOptions {
    pub(crate) fn horizontal(rows: AxisSplit) -> Self {
        Self {
            columns: AxisSplit::EqualParts(1),
            rows,
        }
    }

    pub(crate) fn vertical(columns: AxisSplit) -> Self {
        Self {
            columns,
            rows: AxisSplit::EqualParts(1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u32,
    length: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SplitPiece {
    pub index: usize,
    /// 行列从 0 起；文件名中的行列从 1 起。
    pub column: usize,
    pub row: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub(crate) struct SplitPlan {
    source_width: u32,
    source_height: u32,
    columns: Vec<Span>,
    rows: Vec<Span>,
    limits: ToolLimits,
    request_id: u64,
}

impl SplitPlan {
    /// 调用者在源或参数变化时递增 request_id；旧计划不能用于新请求的保存。
    pub(crate) fn new(
        source: &Image,
        options: &SplitOptions,
        limits: ToolLimits,
        request_id: u64,
    ) -> ToolResult<Self> {
        validate_input(source, limits)?;
        // 切片没有重叠，所有成品的总像素数等于原图，不逐片绕过总预算。
        validate_output(source.width, source.height, limits)?;
        let column_count = axis_count(source.width, &options.columns, limits.max_items)?;
        let row_count = axis_count(source.height, &options.rows, limits.max_items)?;
        if column_count
            .checked_mul(row_count)
            .is_none_or(|count| count > limits.max_items)
        {
            return Err(too_many_pieces());
        }
        Ok(Self {
            source_width: source.width,
            source_height: source.height,
            columns: axis_spans(source.width, &options.columns, column_count)?,
            rows: axis_spans(source.height, &options.rows, row_count)?,
            limits,
            request_id,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.columns.len() * self.rows.len()
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 不提前展开所有小块的像素或矩形；只按索引计算一块。
    pub(crate) fn piece(&self, index: usize) -> Option<SplitPiece> {
        if index >= self.len() {
            return None;
        }
        let column = index % self.columns.len();
        let row = index / self.columns.len();
        let x = self.columns[column];
        let y = self.rows[row];
        Some(SplitPiece {
            index,
            column,
            row,
            x: x.start,
            y: y.start,
            width: x.length,
            height: y.length,
        })
    }

    fn validate_source(&self, source: &Image) -> ToolResult<()> {
        validate_input(source, self.limits)?;
        if (source.width, source.height) != (self.source_width, self.source_height) {
            return Err(ToolError::Invalid(
                tr!(
                    "原图尺寸已变化，请重新分割",
                    "Source dimensions changed; create a new split plan"
                )
                .into(),
            ));
        }
        Ok(())
    }

    /// 返回精确切片供预览或保存，BGRA 与 alpha 原样保留。
    pub(crate) fn render(
        &self,
        source: &Image,
        index: usize,
        cancel: &AtomicBool,
    ) -> ToolResult<ProcessedImage> {
        check_cancel(cancel)?;
        self.validate_source(source)?;
        let piece = self.piece(index).ok_or_else(|| {
            ToolError::Invalid(tr!("分割块索引无效", "Invalid split piece index").into())
        })?;
        let bytes = validate_output(piece.width, piece.height, self.limits)?;
        let mut bgra = allocate_pixels(bytes)?;
        let source_stride = source.width as usize * 4;
        let output_stride = piece.width as usize * 4;
        for (row, output) in bgra.chunks_exact_mut(output_stride).enumerate() {
            check_cancel(cancel)?;
            let start = (piece.y as usize + row) * source_stride + piece.x as usize * 4;
            output.copy_from_slice(&source.bgra[start..start + output_stride]);
        }
        check_cancel(cancel)?;
        Ok(ProcessedImage::new(
            Image {
                width: piece.width,
                height: piece.height,
                bgra,
                readback_ms: 0.0,
            },
            self.request_id,
        ))
    }

    /// 一次只保留一块像素及其编码。失败继续其它项；取消后每个剩余项明确标为取消。
    /// 返回成功项只表示已提交的文件；后续取消或失败不回滚这些新文件。
    pub(crate) fn save_all(
        &self,
        source: &Image,
        output: &SplitOutput,
        expected_request_id: u64,
        protected_sources: &[PathBuf],
        cancel: &AtomicBool,
        mut progress: impl FnMut(&SplitItemResult),
    ) -> ToolResult<SplitReport> {
        require_request(self.request_id, expected_request_id)?;
        self.validate_source(source)?;
        // 在任何写出之前验证所有最终文件名，但不预先创建文件。
        for index in 0..self.len() {
            output.path_for(self.piece(index).unwrap())?;
        }
        let mut items = Vec::new();
        items.try_reserve_exact(self.len()).map_err(|_| {
            ToolError::Failed(
                tr!(
                    "没有足够内存保存分割结果",
                    "Not enough memory for split results"
                )
                .into(),
            )
        })?;
        let mut cancelled = false;
        for index in 0..self.len() {
            let piece = self.piece(index).unwrap();
            let path = output.path_for(piece)?;
            let result = if cancelled {
                Err(ToolError::Cancelled)
            } else {
                self.render(source, index, cancel).and_then(|image| {
                    image.save(
                        expected_request_id,
                        &path,
                        output.encoding,
                        protected_sources,
                        cancel,
                    )
                })
            };
            let outcome = match result {
                Ok(saved) => SplitOutcome::Saved(saved),
                Err(ToolError::Cancelled) => {
                    cancelled = true;
                    SplitOutcome::Cancelled
                }
                Err(error) => SplitOutcome::Failed(error.to_string()),
            };
            let item = SplitItemResult {
                piece,
                path,
                outcome,
            };
            progress(&item);
            items.push(item);
        }
        Ok(SplitReport {
            request_id: self.request_id,
            items,
        })
    }
}

fn too_many_pieces() -> ToolError {
    ToolError::Invalid(
        tr!(
            "分割块数量超过处理预算",
            "The number of split pieces exceeds the limit"
        )
        .into(),
    )
}

fn axis_count(length: u32, split: &AxisSplit, max_items: usize) -> ToolResult<usize> {
    if let AxisSplit::Weights(weights) = split {
        if weights.len() > max_items || weights.len() as u64 > u64::from(length) {
            return Err(too_many_pieces());
        }
    }
    let count = match split {
        AxisSplit::EqualParts(count) if *count != 0 && *count <= length => u64::from(*count),
        AxisSplit::Pixels(size) if *size != 0 => u64::from(length).div_ceil(u64::from(*size)),
        AxisSplit::Weights(weights) if !weights.is_empty() && !weights.contains(&0) => {
            u64::try_from(weights.len()).map_err(|_| too_many_pieces())?
        }
        _ => {
            return Err(ToolError::Invalid(
                tr!(
                    "分割参数必须为正数，且每块至少一像素",
                    "Split values must be positive and each piece must contain pixels"
                )
                .into(),
            ))
        }
    };
    let count = usize::try_from(count).map_err(|_| too_many_pieces())?;
    if count == 0 || count > max_items || count as u64 > u64::from(length) {
        return Err(too_many_pieces());
    }
    Ok(count)
}

fn axis_spans(length: u32, split: &AxisSplit, count: usize) -> ToolResult<Vec<Span>> {
    let mut spans = Vec::new();
    spans.try_reserve_exact(count).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存建立分割计划",
                "Not enough memory for the split plan"
            )
            .into(),
        )
    })?;
    let total_weight = if let AxisSplit::Weights(weights) = split {
        weights
            .iter()
            .try_fold(0u64, |sum, weight| sum.checked_add(u64::from(*weight)))
            .ok_or_else(|| {
                ToolError::Invalid(tr!("分割比例之和溢出", "Split weight sum overflows").into())
            })?
    } else {
        0
    };
    let mut accumulated_weight = 0u64;
    let mut start = 0u32;
    for index in 0..count {
        let end = match split {
            AxisSplit::EqualParts(_) => {
                (u64::from(length) * (index as u64 + 1) / count as u64) as u32
            }
            AxisSplit::Pixels(size) => {
                (u64::from(*size) * (index as u64 + 1)).min(u64::from(length)) as u32
            }
            AxisSplit::Weights(weights) => {
                accumulated_weight += u64::from(weights[index]);
                (u128::from(length) * u128::from(accumulated_weight) / u128::from(total_weight))
                    as u32
            }
        };
        if end <= start {
            return Err(ToolError::Invalid(
                tr!(
                    "分割比例产生了不足一像素的块，请减少块数或调整比例",
                    "Split weights produce an empty piece; reduce the count or adjust the weights"
                )
                .into(),
            ));
        }
        spans.push(Span {
            start,
            length: end - start,
        });
        start = end;
    }
    debug_assert_eq!(start, length);
    Ok(spans)
}

pub(crate) struct SplitOutput {
    directory: PathBuf,
    stem: String,
    encoding: SaveEncoding,
}

impl SplitOutput {
    /// 只规划命名；目录必须由调用者明确选择且已存在。
    pub(crate) fn new(directory: PathBuf, stem: &str, encoding: SaveEncoding) -> ToolResult<Self> {
        crate::save_options::validate_directory(&directory)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        encoding
            .validate()
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        let name = crate::save_options::file_name(stem, encoding.format)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        let stem = std::path::Path::new(&name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap()
            .to_owned();
        Ok(Self {
            directory,
            stem,
            encoding,
        })
    }

    /// UI 与实际保存共用本方法，避免预览顺序与命名脱节。
    pub(crate) fn path_for(&self, piece: SplitPiece) -> ToolResult<PathBuf> {
        let row = piece.row.checked_add(1).ok_or_else(too_many_pieces)?;
        let column = piece.column.checked_add(1).ok_or_else(too_many_pieces)?;
        let stem = format!("{}_r{:03}_c{:03}", self.stem, row, column);
        let name = crate::save_options::file_name(&stem, self.encoding.format)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        Ok(self.directory.join(name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SplitOutcome {
    Saved(SavedImage),
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitItemResult {
    pub piece: SplitPiece,
    pub path: PathBuf,
    pub outcome: SplitOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitReport {
    pub request_id: u64,
    pub items: Vec<SplitItemResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustshot-image-split-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn source(width: u32, height: u32) -> Image {
        let bgra = (0..width * height)
            .flat_map(|index| [index as u8, 12, 34, 128])
            .collect();
        Image {
            width,
            height,
            bgra,
            readback_ms: 0.0,
        }
    }

    #[test]
    fn image_split_grid_covers_every_pixel_once_and_keeps_bgra() {
        let source = source(7, 5);
        let plan = SplitPlan::new(
            &source,
            &SplitOptions {
                columns: AxisSplit::EqualParts(3),
                rows: AxisSplit::EqualParts(2),
            },
            ToolLimits::default(),
            1,
        )
        .unwrap();
        let mut visits = vec![0; 35];
        for index in 0..plan.len() {
            let piece = plan.piece(index).unwrap();
            let rendered = plan
                .render(&source, index, &AtomicBool::new(false))
                .unwrap();
            for y in 0..piece.height {
                for x in 0..piece.width {
                    let original = ((piece.y + y) * 7 + piece.x + x) as usize;
                    let local = (y * piece.width + x) as usize * 4;
                    visits[original] += 1;
                    assert_eq!(
                        &rendered.image().bgra[local..local + 4],
                        &source.bgra[original * 4..original * 4 + 4]
                    );
                }
            }
        }
        assert_eq!(visits, vec![1; 35]);
        assert!(plan.piece(6).is_none());
    }

    #[test]
    fn image_split_pixels_and_weights_keep_remainders() {
        let source = source(7, 5);
        let options = SplitOptions {
            columns: AxisSplit::Pixels(3),
            rows: AxisSplit::Weights(vec![1, 2]),
        };
        let plan = SplitPlan::new(&source, &options, ToolLimits::default(), 9).unwrap();
        assert_eq!(
            (plan.piece(2).unwrap().x, plan.piece(2).unwrap().width),
            (6, 1)
        );
        assert_eq!(
            (plan.piece(3).unwrap().y, plan.piece(3).unwrap().height),
            (1, 4)
        );
        assert_eq!(
            plan.render(&source, 5, &AtomicBool::new(false))
                .unwrap()
                .request_id(),
            9
        );
    }

    #[test]
    fn image_split_custom_ratios_parse_and_keep_independent_axis_remainders() {
        let source = source(7, 5);
        let options = SplitOptions {
            columns: AxisSplit::Weights(parse_weights(" 1：2:1 ", 3).unwrap()),
            rows: AxisSplit::Weights(parse_weights("2，1", 2).unwrap()),
        };
        let plan = SplitPlan::new(&source, &options, ToolLimits::default(), 21).unwrap();
        assert_eq!(plan.len(), 6);
        let middle = plan.piece(1).unwrap();
        assert_eq!(
            (middle.x, middle.y, middle.width, middle.height),
            (1, 0, 4, 3)
        );
        let last = plan.piece(5).unwrap();
        assert_eq!((last.x, last.y, last.width, last.height), (5, 3, 2, 2));
        assert_eq!(
            (0..plan.len())
                .map(|index| {
                    let piece = plan.piece(index).unwrap();
                    piece.width * piece.height
                })
                .sum::<u32>(),
            35
        );
        for invalid in [
            "",
            "1:",
            ":1",
            "1,,2",
            "0:1",
            "-1:2",
            "1.5:2",
            "4294967296:1",
        ] {
            assert!(parse_weights(invalid, 4).is_err(), "{invalid}");
        }
        assert!(parse_weights("1:1:1", 2).is_err());
        assert!(parse_weights("1", 0).is_err());
        assert!(parse_weights(&"1".repeat(64 * 1024 + 1), 4).is_err());
        assert_eq!(parse_weights("4294967295", 1).unwrap(), [u32::MAX]);
    }

    #[test]
    fn image_split_rejects_empty_and_excessive_plans() {
        let source = source(7, 5);
        for split in [
            AxisSplit::EqualParts(0),
            AxisSplit::EqualParts(8),
            AxisSplit::Pixels(0),
            AxisSplit::Weights(vec![]),
            AxisSplit::Weights(vec![0, 1]),
            AxisSplit::Weights(vec![1, 100]),
        ] {
            assert!(SplitPlan::new(
                &source,
                &SplitOptions::vertical(split),
                ToolLimits::default(),
                0
            )
            .is_err());
        }
        let options = SplitOptions {
            columns: AxisSplit::Pixels(1),
            rows: AxisSplit::Pixels(1),
        };
        assert!(SplitPlan::new(
            &source,
            &options,
            ToolLimits {
                max_items: 34,
                ..ToolLimits::default()
            },
            0
        )
        .is_err());
        assert!(SplitPlan::new(
            &source,
            &options,
            ToolLimits {
                max_output_pixels: 34,
                ..ToolLimits::default()
            },
            0
        )
        .is_err());
    }

    #[test]
    fn image_split_cancel_and_source_change_have_no_rendered_result() {
        let source = source(7, 5);
        let plan = SplitPlan::new(
            &source,
            &SplitOptions::horizontal(AxisSplit::EqualParts(2)),
            ToolLimits::default(),
            3,
        )
        .unwrap();
        assert!(matches!(
            plan.render(&source, 0, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        assert!(plan
            .render(&super::tests::source(5, 7), 0, &AtomicBool::new(false))
            .is_err());
    }

    #[test]
    fn image_split_save_reports_existing_source_failure_and_continues() {
        let directory = TestDirectory::new();
        let source = source(7, 5);
        let plan = SplitPlan::new(
            &source,
            &SplitOptions::vertical(AxisSplit::Pixels(3)),
            ToolLimits::default(),
            11,
        )
        .unwrap();
        let output =
            SplitOutput::new(directory.0.clone(), "part.png", SaveEncoding::default()).unwrap();
        let original_path = output.path_for(plan.piece(0).unwrap()).unwrap();
        std::fs::write(&original_path, b"preserve source").unwrap();
        let mut seen = Vec::new();
        let report = plan
            .save_all(
                &source,
                &output,
                11,
                &[original_path.clone()],
                &AtomicBool::new(false),
                |item| seen.push(item.piece.index),
            )
            .unwrap();
        assert_eq!(seen, vec![0, 1, 2]);
        assert_eq!(report.request_id, 11);
        assert!(matches!(report.items[0].outcome, SplitOutcome::Failed(_)));
        assert!(matches!(report.items[1].outcome, SplitOutcome::Saved(_)));
        assert!(matches!(report.items[2].outcome, SplitOutcome::Saved(_)));
        assert_eq!(std::fs::read(&original_path).unwrap(), b"preserve source");
        let saved = crate::export::read_image(&report.items[2].path).unwrap();
        assert_eq!((saved.width, saved.height), (1, 5));
        assert_eq!(
            saved.bgra,
            plan.render(&source, 2, &AtomicBool::new(false))
                .unwrap()
                .image()
                .bgra
        );
        // 未显式提供原路径时，创建新文件语义仍拒绝现存输出。
        let repeat = plan
            .save_all(&source, &output, 11, &[], &AtomicBool::new(false), |_| {})
            .unwrap();
        assert!(repeat
            .items
            .iter()
            .all(|item| matches!(item.outcome, SplitOutcome::Failed(_))));
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 3);
    }

    #[test]
    fn image_split_cancel_keeps_committed_piece_and_marks_the_rest() {
        let directory = TestDirectory::new();
        let source = source(3, 5);
        let plan = SplitPlan::new(
            &source,
            &SplitOptions::horizontal(AxisSplit::EqualParts(3)),
            ToolLimits::default(),
            12,
        )
        .unwrap();
        let output =
            SplitOutput::new(directory.0.clone(), "part", SaveEncoding::default()).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(plan
            .save_all(&source, &output, 13, &[], &cancel, |_| {})
            .is_err());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        let report = plan
            .save_all(&source, &output, 12, &[], &cancel, |item| {
                if matches!(item.outcome, SplitOutcome::Saved(_)) {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
            })
            .unwrap();
        assert!(matches!(report.items[0].outcome, SplitOutcome::Saved(_)));
        assert_eq!(report.items[1].outcome, SplitOutcome::Cancelled);
        assert_eq!(report.items[2].outcome, SplitOutcome::Cancelled);
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
        assert!(crate::export::read_image(&report.items[0].path).is_ok());
    }
}
