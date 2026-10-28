//! 独立表格任务：按需加载 SLANet_plus，在后台识别结构，再将原分辨率 OCR 文字填入单元格。
use super::{runtime, table_structure, threads, OcrProfile, Tensor};
use crate::{
    capture::Image,
    recognition::{
        task::{is_cancelled, wait_for_lock, TaskToken},
        RecognizedText,
    },
    table_grid::{TableGrid, TableSource},
    Result,
};
use parking_lot::Mutex;
use std::sync::OnceLock;

pub(crate) struct TableRecognition {
    pub recognized: RecognizedText,
    pub grid: Option<TableGrid>,
    pub region: Option<Vec<usize>>,
    pub warning: Option<String>,
}

#[derive(Default)]
pub(crate) struct TableResult {
    pub grid: Option<TableGrid>,
    pub region: Option<Vec<usize>>,
    pub warning: Option<String>,
}

impl TableRecognition {
    pub(crate) fn into_parts(self) -> (RecognizedText, TableResult) {
        (
            self.recognized,
            TableResult {
                grid: self.grid,
                region: self.region,
                warning: self.warning,
            },
        )
    }
}

struct Engine {
    session: runtime::CpuSession,
    characters: Vec<String>,
    threads: usize,
}
static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();

fn structure(image: &Image, task: &TaskToken) -> Result<TableGrid> {
    super::validate_image(image)?;
    let mut state = wait_for_lock(ENGINE.get_or_init(|| Mutex::new(None)), task)?;
    let threads = threads::current();
    if state
        .as_ref()
        .is_none_or(|engine| engine.threads != threads)
    {
        let assets = super::assets::ensure_table_cancellable(task)?;
        runtime::initialize(&assets.runtime)?;
        task.checkpoint()?;
        let session = runtime::CpuSession::new(&assets.model, threads)?;
        task.checkpoint()?;
        let metadata = session
            .custom_metadata("character")?
            .ok_or("表格模型缺少字符表")?;
        let characters = table_structure::read_characters(&metadata)?;
        task.checkpoint()?;
        *state = Some(Engine {
            session,
            characters,
            threads,
        });
    }
    let engine = state.as_mut().ok_or("表格模型未初始化")?;
    let input = table_structure::preprocess(image)?;
    task.checkpoint()?;
    let run = runtime::TaskRunContext::new(task)?;
    let outputs = run.runner(&mut engine.session).run_named(input)?;
    let (probabilities, boxes) = model_outputs(&outputs, engine.characters.len())?;
    let grid = table_structure::decode(
        probabilities,
        boxes,
        &engine.characters,
        image.width,
        image.height,
    )?;
    if grid.confidence < 0.35 {
        return Err(tr!("表格结构模型置信度不足", "Low table structure confidence").into());
    }
    task.checkpoint()?;
    Ok(grid)
}

fn model_outputs(outputs: &[(String, Tensor)], vocabulary: usize) -> Result<(&Tensor, &Tensor)> {
    let select = |last| {
        let mut matches = outputs.iter().filter(|(_, tensor)| {
            tensor.shape.len() == 3 && tensor.shape[0] == 1 && tensor.shape[2] == last
        });
        let first = matches.next().map(|(_, tensor)| tensor);
        if matches.next().is_some() {
            None
        } else {
            first
        }
    };
    let probabilities = select(vocabulary).ok_or("表格模型缺少唯一结构输出")?;
    let boxes = select(8).ok_or("表格模型缺少唯一四角框输出")?;
    if probabilities.shape[1] != boxes.shape[1] {
        return Err("表格模型结构与四角框序列不一致".into());
    }
    Ok((probabilities, boxes))
}

type Candidate = (TableGrid, Option<Vec<usize>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RulePreference {
    MoreText,
    VisibleGrid,
}

/// 只比较可信规则候选。强制单列表是失败兜底，不能用来压过模型结果。
fn prefer_rules(
    model: &Candidate,
    rules: &Candidate,
    total_lines: usize,
    wired: bool,
) -> Option<RulePreference> {
    let coverage = |region: &Option<Vec<usize>>| {
        let mut covered = vec![region.is_none(); total_lines];
        if let Some(lines) = region {
            if lines.is_empty() || lines.windows(2).any(|pair| pair[0] >= pair[1]) {
                return None;
            }
            for &line in lines {
                *covered.get_mut(line)? = true;
            }
        }
        Some(covered)
    };
    let model_lines = coverage(&model.1)?;
    let rule_lines = coverage(&rules.1)?;
    // 两个候选各自包含不同正文时没有充分证据替换模型；规则必须完整包含模型来源。
    if model_lines
        .iter()
        .zip(&rule_lines)
        .any(|(model, rules)| *model && !rules)
    {
        return None;
    }
    if model_lines
        .iter()
        .zip(&rule_lines)
        .any(|(model, rules)| !model && *rules)
    {
        return Some(RulePreference::MoreText);
    }
    // 相同文字覆盖且有完整真实线网格时，以像素边界及缺线跨度为准。
    (wired && rules.0.source == TableSource::Lines).then_some(RulePreference::VisibleGrid)
}

/// 仅显式表格入口调用；模型失败保留 OCR 和可信规则结果，取消仍为有类型的取消错误。
pub(crate) fn recognize_cancellable(
    image: &Image,
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<TableRecognition> {
    task.checkpoint()?;
    let recognized = super::recognize_profile_cancellable(image, profile, task)?;
    let detected = crate::table_lines::detect(image);
    task.checkpoint()?;
    let trusted = crate::table_layout::recognize_detected(&recognized, detected.as_ref())
        .map(|grid| (grid, None))
        .or_else(|| {
            crate::table_layout::recognize_region(&recognized, detected.as_ref())
                .map(|(grid, lines)| (grid, Some(lines)))
        });
    task.checkpoint()?;
    let model = structure(image, task).and_then(|grid| {
        crate::table_layout::fill_structure(&recognized, grid, detected.as_ref()).ok_or_else(|| {
            tr!(
                "表格结构未完整覆盖识别文字",
                "Table structure does not fully cover the text"
            )
            .into()
        })
    });
    task.checkpoint()?;
    let warning = match model {
        Ok(candidate) => {
            let preference = trusted.as_ref().and_then(|rules| {
                prefer_rules(
                    &candidate,
                    rules,
                    recognized.lines.len(),
                    detected
                        .as_ref()
                        .is_some_and(|lines| lines.kind == crate::table_lines::LineKind::Wired),
                )
            });
            let ((grid, region), warning) = match preference {
                Some(preference) => (
                    trusted.unwrap(),
                    Some(match preference {
                        RulePreference::MoreText => tr!(
                            "结构模型未覆盖完整表格，已保留覆盖更完整的表格",
                            "Kept the more complete table; the model missed some text"
                        )
                        .to_owned(),
                        RulePreference::VisibleGrid => tr!(
                            "已按原图表格线保留行列及合并单元格",
                            "Kept rows, columns and merged cells from visible table lines"
                        )
                        .to_owned(),
                    }),
                ),
                None => (candidate, None),
            };
            task.checkpoint()?;
            return Ok(TableRecognition {
                recognized,
                grid: Some(grid),
                region,
                warning,
            });
        }
        Err(error) if is_cancelled(error.as_ref()) => return Err(error),
        Err(error) => trf!(
            "结构模型不可用，已保留文字及可用表格：{}",
            "Structure model unavailable; kept text and available table: {}",
            error
        ),
    };
    task.checkpoint()?;
    let fallback = trusted.or_else(|| {
        crate::table_layout::recognize_forced(&recognized, detected.as_ref())
            .map(|grid| (grid, None))
    });
    let (grid, region) = fallback.map_or((None, None), |(grid, region)| (Some(grid), region));
    task.checkpoint()?;
    Ok(TableRecognition {
        recognized,
        grid,
        region,
        warning: Some(warning),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition::task::CancelOwner;

    fn candidate(source: TableSource, region: Option<Vec<usize>>) -> Candidate {
        (
            TableGrid {
                row_edges: vec![0, 10],
                column_edges: vec![0, 10],
                cells: vec![crate::table_grid::TableCell {
                    row: 0,
                    column: 0,
                    rowspan: 1,
                    colspan: 1,
                    text: String::new(),
                    rect: crate::geometry::Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 10,
                    },
                    confidence: 1.,
                }],
                confidence: 1.,
                source,
            },
            region,
        )
    }

    #[test]
    fn table_model_keeps_complete_trusted_table_when_model_misses_rows() {
        let model = candidate(TableSource::Model, Some(vec![1, 2]));
        let rules = candidate(TableSource::Alignment, Some(vec![1, 2, 3]));
        assert_eq!(
            prefer_rules(&model, &rules, 5, false),
            Some(RulePreference::MoreText)
        );
        let whole = candidate(TableSource::Lines, None);
        assert_eq!(
            prefer_rules(&model, &whole, 5, true),
            Some(RulePreference::MoreText)
        );
    }

    #[test]
    fn table_model_keeps_wireless_model_when_it_covers_more_or_different_rows() {
        let model = candidate(TableSource::Model, Some(vec![1, 2, 3]));
        let rules = candidate(TableSource::Alignment, Some(vec![1, 2]));
        assert_eq!(prefer_rules(&model, &rules, 5, false), None);
        let different = candidate(TableSource::Lines, Some(vec![0, 1, 2]));
        assert_eq!(prefer_rules(&model, &different, 5, true), None);
        let with_more_body = candidate(TableSource::Lines, Some(vec![0, 1, 2, 4]));
        assert_eq!(prefer_rules(&model, &with_more_body, 5, true), None);
    }

    #[test]
    fn table_model_prefers_visible_grid_only_with_same_text_coverage() {
        let model = candidate(TableSource::Model, Some(vec![1, 2, 3]));
        let wired = candidate(TableSource::Lines, Some(vec![1, 2, 3]));
        assert_eq!(
            prefer_rules(&model, &wired, 5, true),
            Some(RulePreference::VisibleGrid)
        );
        assert_eq!(prefer_rules(&model, &wired, 5, false), None);
        let aligned = candidate(TableSource::Alignment, Some(vec![1, 2, 3]));
        assert_eq!(prefer_rules(&model, &aligned, 5, true), None);
        let whole_model = candidate(TableSource::Model, None);
        let whole_wired = candidate(TableSource::Lines, Some(vec![0, 1, 2]));
        assert_eq!(
            prefer_rules(&whole_model, &whole_wired, 3, true),
            Some(RulePreference::VisibleGrid)
        );
    }

    #[test]
    fn table_recognition_cancelled_before_start_does_not_load_assets() {
        let owner = CancelOwner::new();
        owner.cancel().unwrap();
        let image = Image {
            width: 0,
            height: 0,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        let error = recognize_cancellable(&image, OcrProfile::Tiny, &owner.token())
            .err()
            .unwrap();
        assert!(is_cancelled(error.as_ref()));
    }

    #[test]
    #[ignore = "需要下载表格与 OCR 依赖；可用 RUSTSHOT_TABLE_IMAGE 指定图片，不打开窗口"]
    fn table_model_real_image() {
        let path = std::env::var("RUSTSHOT_TABLE_IMAGE")
            .unwrap_or_else(|_| "fixtures/table/images/wireless-light.png".into());
        let image = crate::export::read_image(std::path::Path::new(&path)).unwrap();
        let owner = CancelOwner::new();
        let raw = structure(&image, &owner.token()).expect("需要真实结构模型推理成功");
        assert!(raw.validate());
        println!(
            "raw model rows={} columns={} confidence={}",
            raw.rows(),
            raw.columns(),
            raw.confidence
        );
        let result = recognize_cancellable(&image, OcrProfile::Tiny, &owner.token()).unwrap();
        println!("warning={:?} region={:?}", result.warning, result.region);
        let grid = result.grid.expect("需要可用表格");
        println!(
            "source={:?} rows={} columns={} confidence={}\n{}",
            grid.source,
            grid.rows(),
            grid.columns(),
            grid.confidence,
            crate::table_export::tsv(&grid).unwrap()
        );
        assert!(grid.validate());
    }
}
