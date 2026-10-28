//! 随仓库携带的合成表格回归与可选真实 OCR 评测。
use crate::{
    annotation::Point,
    geometry::Rect,
    recognition::{
        HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
    },
    table_grid::{TableCell, TableGrid, TableSource},
    table_metrics::{self, TableMetrics},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Deserialize)]
struct Fixtures {
    schema_version: u32,
    fixtures: Vec<Fixture>,
}
#[derive(Deserialize)]
struct Fixture {
    name: String,
    image: String,
    tags: Vec<String>,
    rows: usize,
    columns: usize,
    row_edges: Vec<u32>,
    column_edges: Vec<u32>,
    cells: Vec<FixtureCell>,
    ocr: Vec<FixtureOcr>,
}
#[derive(Deserialize)]
struct FixtureCell {
    row: usize,
    column: usize,
    rowspan: usize,
    colspan: usize,
    text: String,
}
#[derive(Deserialize)]
struct FixtureOcr {
    text: String,
    rect: FixtureRect,
}
#[derive(Deserialize)]
struct FixtureRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/table")
}
fn fixtures() -> Fixtures {
    let bytes = std::fs::read(fixture_root().join("ground-truth.json")).unwrap();
    let fixtures: Fixtures = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fixtures.schema_version, 1);
    assert_eq!(fixtures.fixtures.len(), 14);
    fixtures
}
impl Fixture {
    fn truth(&self) -> TableGrid {
        let mut grid = TableGrid {
            row_edges: self.row_edges.clone(),
            column_edges: self.column_edges.clone(),
            cells: Vec::new(),
            confidence: 1.,
            source: TableSource::Lines,
        };
        for cell in &self.cells {
            grid.cells.push(TableCell {
                row: cell.row,
                column: cell.column,
                rowspan: cell.rowspan,
                colspan: cell.colspan,
                text: cell.text.clone(),
                rect: grid
                    .cell_rect(cell.row, cell.column, cell.rowspan, cell.colspan)
                    .unwrap(),
                confidence: 1.,
            });
        }
        assert_eq!((grid.rows(), grid.columns()), (self.rows, self.columns));
        assert!(grid.validate());
        grid
    }
    fn source(&self) -> RecognizedText {
        let mut source = RecognizedText {
            text: String::new(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        };
        for item in &self.ocr {
            let rect = Rect {
                x: item.rect.x,
                y: item.rect.y,
                width: item.rect.width,
                height: item.rect.height,
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
            let line = source.lines.len();
            source.words.push(RecognizedWord {
                text: item.text.clone(),
                rect,
                line,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            });
            source.lines.push(RecognizedLine {
                text: item.text.clone(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
                graphemes: None,
            });
            source.regions.push(rect);
        }
        source.text = source
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        source
    }
}

#[test]
fn table_eval_synthetic_fixture_grid_and_text_contracts() {
    for fixture in fixtures().fixtures {
        let image = crate::export::read_image(&fixture_root().join(&fixture.image)).unwrap();
        let source = fixture.source();
        let predicted = crate::table_layout::recognize(&source, Some(&image))
            .unwrap_or_else(|| panic!("{} 未产生表格", fixture.name));
        let metrics = table_metrics::evaluate(&predicted, &fixture.truth()).unwrap();
        assert_eq!(metrics.cell_f1, 1., "{} 结构 {:?}", fixture.name, predicted);
        assert_eq!(
            metrics.cell_text_f1, 1.,
            "{} 文字 {:?}",
            fixture.name, predicted
        );
        assert_eq!(metrics.teds_lite, 1., "{} TEDS", fixture.name);
        assert_eq!(metrics.cer, 0., "{} CER", fixture.name);
    }
}

/// 像素网格不能保持旧阅读顺序时，以实际 TSV 的逻辑行列建立评测树。
/// 单位坐标只用于指标，既不返回生产缓存，也不冒充来源图片定位。
fn legacy_output_grid(
    source: &RecognizedText,
    legacy: &crate::recognition::table::OcrTableCopy,
) -> TableGrid {
    if let Some(grid) = TableGrid::from_legacy(source, legacy)
        .filter(|grid| crate::table_export::tsv(grid).as_deref() == Some(legacy.text.as_str()))
    {
        return grid;
    }
    let rows = legacy.rows.len();
    let columns = legacy.rows.first().unwrap().len();
    let mut grid = TableGrid {
        row_edges: (0..=rows).map(|row| row as u32).collect(),
        column_edges: (0..=columns).map(|column| column as u32).collect(),
        cells: Vec::new(),
        confidence: 1.,
        source: TableSource::Legacy,
    };
    for (row, cells) in legacy.rows.iter().enumerate() {
        for (column, cell) in cells.iter().enumerate() {
            grid.cells.push(TableCell {
                row,
                column,
                rowspan: 1,
                colspan: 1,
                text: source
                    .text
                    .get(cell.source_range.clone())
                    .unwrap()
                    .to_owned(),
                rect: grid.cell_rect(row, column, 1, 1).unwrap(),
                confidence: 1.,
            });
        }
    }
    assert!(grid.validate());
    assert_eq!(crate::table_export::tsv(&grid).unwrap(), legacy.text);
    grid
}

#[test]
#[ignore = "需要本机已有 Tiny OCR 模型缓存；不下载模型，不打开窗口"]
fn table_eval_real_ocr_reports_source_metrics_and_tag_buckets() {
    let owner = crate::recognition::task::CancelOwner::new();
    assert!(
        crate::ppocr::assets::cached_cancellable(&owner.token())
            .unwrap()
            .is_some(),
        "请先通过应用完成 Tiny OCR 依赖准备，再运行真实评测"
    );
    let mut buckets: BTreeMap<String, Vec<(TableMetrics, TableMetrics)>> = BTreeMap::new();
    let mut recognized = 0usize;
    let mut legacy_recognized = 0usize;
    println!("评测范围：14 张合成浅色/深色/有线/无线/合并夹具，不含 RTL 或倒序来源；新结果使用生产 TableCopy 最终择优。");
    for fixture in fixtures().fixtures {
        let image = crate::export::read_image(&fixture_root().join(&fixture.image)).unwrap();
        let started = Instant::now();
        let source = crate::ppocr::recognize(&image).unwrap();
        let ocr_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let selected =
            crate::pin_view::table_copy::TableCopy::with_image(&source, (), 0, Some(&image));
        let table_ms = started.elapsed().as_secs_f64() * 1000.;
        let truth = fixture.truth();
        let legacy_copy = source.copy_table();
        let legacy = legacy_copy
            .as_ref()
            .map(|table| legacy_output_grid(&source, table));
        let final_tsv = selected
            .text(&(), 0, &source.text)
            .filter(|text| !text.is_empty());
        let selected_grid = selected.grid(&(), 0, &source.text);
        let predicted = selected_grid.or_else(|| {
            final_tsv.filter(|text| {
                legacy_copy
                    .as_ref()
                    .is_some_and(|table| table.text == *text)
            })?;
            legacy.as_ref()
        });
        assert!(
            final_tsv.is_none() || predicted.is_some(),
            "{} 最终 TSV 有内容却没有可评测结构",
            fixture.name
        );
        let failed = TableMetrics {
            precision: 0.,
            recall: 0.,
            cell_f1: 0.,
            cell_text_f1: 0.,
            teds_lite: 0.,
            cer: 1.,
        };
        let new_metrics = predicted.map_or(failed, |grid| {
            table_metrics::evaluate(grid, &truth).unwrap()
        });
        let old_metrics = legacy.as_ref().map_or(failed, |grid| {
            table_metrics::evaluate(grid, &truth).unwrap()
        });
        recognized += usize::from(final_tsv.is_some());
        legacy_recognized += usize::from(legacy.is_some());
        println!(
            "{} OCR={:.1}ms Grid={:.1}ms",
            fixture.name, ocr_ms, table_ms
        );
        let final_source = if selected_grid.is_some() {
            format!("{:?}", selected_grid.unwrap().source)
        } else if final_tsv.is_some() {
            "Legacy TSV fallback（无富文本）".to_owned()
        } else {
            "无最终 TSV".to_owned()
        };
        let old_source = if legacy.is_some() {
            "Legacy"
        } else {
            "无 TSV"
        };
        for (label, source, metrics) in [
            ("新最终", final_source.as_str(), new_metrics),
            ("旧矩阵", old_source, old_metrics),
        ] {
            println!(
                "  {label} {source} F1={:.3} TextF1={:.3} TEDS={:.3} CharScore={:.3} CER={:.3}",
                metrics.cell_f1,
                metrics.cell_text_f1,
                metrics.teds_lite,
                (1. - metrics.cer).max(0.),
                metrics.cer
            );
        }
        // 无候选也纳入相同分母，不能只平均成功识别的样本。
        buckets
            .entry("全部".to_owned())
            .or_default()
            .push((new_metrics, old_metrics));
        for tag in &fixture.tags {
            buckets
                .entry(tag.clone())
                .or_default()
                .push((new_metrics, old_metrics));
        }
    }
    for (tag, comparisons) in buckets {
        for (label, baseline) in [("新最终", false), ("旧矩阵", true)] {
            let mean = |get: fn(&TableMetrics) -> f64| {
                comparisons
                    .iter()
                    .map(|(new, old)| get(if baseline { old } else { new }))
                    .sum::<f64>()
                    / comparisons.len() as f64
            };
            println!(
                "[{tag}] {label} n={} F1={:.3} TextF1={:.3} TEDS={:.3} CharScore={:.3} CER={:.3}",
                comparisons.len(),
                mean(|metric| metric.cell_f1),
                mean(|metric| metric.cell_text_f1),
                mean(|metric| metric.teds_lite),
                mean(|metric| (1. - metric.cer).max(0.)),
                mean(|metric| metric.cer)
            );
        }
    }
    println!("实际 TSV 新最终 {recognized}/14，旧矩阵 {legacy_recognized}/14；无最终 TSV 计零分；SLANet 对照未接入");
}

/// 可视化口径：导出每张夹具的 OCR 行框、自动检测与手动表格识别的单元格和耗时，
/// 写到 `target/qa/table-vis/results.json`，供本机生成对照页；不打开窗口。
#[test]
#[ignore = "需要本机已有 Tiny OCR 模型缓存；只写 target/qa 下的 JSON"]
fn table_eval_dump_visual_results() {
    use crate::pin_view::table_copy::TableCopy;
    let grid_json = |grid: Option<&TableGrid>| {
        grid.map(|grid| {
            serde_json::json!({
                "rows": grid.rows(),
                "columns": grid.columns(),
                "source": format!("{:?}", grid.source),
                "cells": grid.cells.iter().map(|cell| serde_json::json!({
                    "row": cell.row, "column": cell.column,
                    "rowspan": cell.rowspan, "colspan": cell.colspan,
                    "text": cell.text,
                    "rect": [cell.rect.x, cell.rect.y, cell.rect.width, cell.rect.height],
                })).collect::<Vec<_>>(),
            })
        })
    };
    let mut out = Vec::new();
    for fixture in fixtures().fixtures {
        let image = crate::export::read_image(&fixture_root().join(&fixture.image)).unwrap();
        let started = Instant::now();
        let source = crate::ppocr::recognize(&image).unwrap();
        let ocr_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let detected = crate::table_lines::detect(&image);
        let detect_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let auto = TableCopy::with_lines(&source, (), 0, detected.as_ref());
        let auto_ms = started.elapsed().as_secs_f64() * 1000.;
        let forced = TableCopy::forced(&source, (), 0, detected.as_ref());
        let truth = fixture.truth();
        let score = |grid: Option<&TableGrid>| {
            grid.and_then(|grid| table_metrics::evaluate(grid, &truth))
                .map(|m| serde_json::json!({ "f1": m.cell_f1, "cer": m.cer }))
        };
        let auto_grid = auto.grid(&(), 0, &source.text);
        let forced_grid = forced.grid(&(), 0, &source.text);
        out.push(serde_json::json!({
            "name": fixture.name,
            "image": fixture.image,
            "tags": fixture.tags,
            "width": image.width,
            "height": image.height,
            "ocr_ms": ocr_ms,
            "detect_ms": detect_ms,
            "assign_ms": auto_ms,
            "lines_kind": detected.as_ref().map(|d| format!("{:?}", d.kind)),
            "ocr": source.lines.iter().filter_map(|line| line.rect.map(|r| serde_json::json!({
                "text": line.text, "rect": [r.x, r.y, r.width, r.height],
            }))).collect::<Vec<_>>(),
            "auto": grid_json(auto_grid),
            "auto_tsv": auto.text(&(), 0, &source.text),
            "auto_score": score(auto_grid),
            "forced": grid_json(forced_grid),
            "forced_tsv": forced.text(&(), 0, &source.text),
            "forced_score": score(forced_grid),
            "truth": { "rows": fixture.rows, "columns": fixture.columns },
        }));
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa/table-vis");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_vec_pretty(&out).unwrap(),
    )
    .unwrap();
    println!("已写入 {}", dir.join("results.json").display());
}

/// 单张图片口径：`RUSTSHOT_TABLE_IMAGE=<png>` 指定图片，打印线检测、来源与行列边界。
#[test]
#[ignore = "需要本机 OCR 模型与环境变量指定的图片"]
fn table_eval_single_image() {
    let path = std::env::var("RUSTSHOT_TABLE_IMAGE").expect("设置 RUSTSHOT_TABLE_IMAGE");
    let image = crate::export::read_image(Path::new(&path)).unwrap();
    let source = crate::ppocr::recognize(&image).unwrap();
    let detected = crate::table_lines::detect(&image);
    if let Some(lines) = &detected {
        println!(
            "lines kind={:?} grid={} h={:?} v={:?}",
            lines.kind,
            lines.grid.is_some(),
            lines
                .horizontal
                .iter()
                .map(|l| l.coordinate)
                .collect::<Vec<_>>(),
            lines
                .vertical
                .iter()
                .map(|l| l.coordinate)
                .collect::<Vec<_>>()
        );
        if let Some(grid) = &lines.grid {
            let spans: Vec<_> = grid
                .cells
                .iter()
                .filter(|c| c.rowspan > 1 || c.colspan > 1)
                .map(|c| (c.row, c.column, c.rowspan, c.colspan))
                .collect();
            println!("line grid spans={spans:?}");
        }
    }
    let cache =
        crate::pin_view::table_copy::TableCopy::with_lines(&source, (), 0, detected.as_ref());
    match cache.grid(&(), 0, &source.text) {
        Some(grid) => println!(
            "source={:?} rows={:?} columns={:?} region={:?}",
            grid.source,
            grid.row_edges,
            grid.column_edges,
            cache.region(&(), 0, &source.text)
        ),
        None => println!("no grid; tsv={:?}", cache.text(&(), 0, &source.text)),
    }
    println!(
        "strict_pieces={} lenient_pieces={} forced={:?}",
        crate::table_layout::debug_pieces(&source, false),
        crate::table_layout::debug_pieces(&source, true),
        crate::table_layout::recognize_forced(&source, detected.as_ref()).map(|grid| (
            grid.source,
            grid.rows(),
            grid.columns()
        ))
    );
    for (index, line) in source.lines.iter().enumerate() {
        println!(
            "  line {index} {:?} rect={:?} dir={:?} graphemes={}",
            line.text,
            line.rect.map(|r| (r.x, r.y, r.width, r.height)),
            line.horizontal_direction,
            line.graphemes.is_some()
        );
    }
}
