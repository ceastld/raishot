use super::*;
use crate::recognition::{RecognizedLine, RecognizedWord};

fn geometry(rect: Rect) -> crate::recognition::TextGeometry {
    use crate::annotation::Point;
    crate::recognition::TextGeometry {
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
    }
}

fn source(rows: &[Vec<(&str, u32, u32)>]) -> RecognizedText {
    let mut result = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (row, cells) in rows.iter().enumerate() {
        for (text, x, width) in cells {
            let line = result.lines.len();
            let rect = Rect {
                x: *x,
                y: 10 + row as u32 * 40,
                width: *width,
                height: 20,
            };
            result.words.push(RecognizedWord {
                text: (*text).to_owned(),
                rect,
                line,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry(rect)),
            });
            result.lines.push(RecognizedLine {
                text: (*text).to_owned(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry(rect)),
                graphemes: None,
            });
            result.regions.push(rect);
        }
    }
    result.text = result
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    result
}

fn source_at(rows: &[(u32, Vec<(&str, u32, u32)>)]) -> RecognizedText {
    let mut result = source(
        &rows
            .iter()
            .map(|(_, cells)| cells.clone())
            .collect::<Vec<_>>(),
    );
    let mut line = 0;
    for (y, cells) in rows {
        for _ in cells {
            let mut rect = result.lines[line].rect.unwrap();
            rect.y = *y;
            result.lines[line].rect = Some(rect);
            result.lines[line].geometry = Some(geometry(rect));
            result.words[line].rect = rect;
            result.words[line].geometry = Some(geometry(rect));
            result.regions[line] = rect;
            line += 1;
        }
    }
    result
}

fn sample() -> RecognizedText {
    source(&[
        vec![("名称", 10, 40), ("编号", 120, 40)],
        vec![("甲项", 10, 40), ("0012", 120, 40)],
        vec![("乙项", 10, 40), ("0", 120, 40)],
    ])
}

#[test]
fn table_layout_wireless_uses_stable_whitespace_and_keeps_leading_zero() {
    let grid = recognize(&sample(), None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (3, 2));
    assert!(matches!(
        grid.source,
        TableSource::Alignment | TableSource::Legacy
    ));
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "名称\t编号\r\n甲项\t0012\r\n乙项\t0"
    );
}

#[test]
fn table_layout_all_text_columns_and_empty_cell_are_allowed() {
    let source = source(&[
        vec![("甲", 10, 40), ("红", 120, 40)],
        vec![("乙", 10, 40), ("蓝", 120, 40)],
        vec![("丙", 10, 40)],
    ]);
    let grid = recognize(&source, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "甲\t红\r\n乙\t蓝\r\n丙\t"
    );
}

#[test]
fn table_layout_header_spans_columns_without_splitting_text() {
    let source = source(&[
        vec![("合并表头", 10, 150)],
        vec![("甲", 10, 40), ("1", 120, 40)],
        vec![("乙", 10, 40), ("2", 120, 40)],
    ]);
    let grid = recognize(&source, None).unwrap();
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.row == 0 && cell.colspan == 2 && cell.text == "合并表头"));
}

#[test]
fn table_layout_ordinary_paragraph_and_invalid_source_keep_fallback() {
    let paragraph = source(&[
        vec![("普通一段文字", 10, 200)],
        vec![("第二行正文", 10, 180)],
        vec![("结尾", 10, 80)],
    ]);
    assert!(recognize(&paragraph, None).is_none());
    let mut bad = sample();
    bad.words[0].line = usize::MAX;
    assert!(recognize(&bad, None).is_none());
    let mut bad = sample();
    bad.lines[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(recognize(&bad, None).is_none());
    let mut bad = sample();
    bad.words[0].rect.width = 0;
    assert!(recognize(&bad, None).is_none());
    let mut bad = sample();
    bad.lines[0].geometry = None;
    assert!(recognize(&bad, None).is_none());
}

#[test]
fn table_layout_inline_cells_preserve_spaces_within_words() {
    let mut result = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (row, (left, right)) in [("A B", "001"), ("C D", "002"), ("E F", "003")]
        .into_iter()
        .enumerate()
    {
        let rect = Rect {
            x: 10,
            y: 10 + row as u32 * 40,
            width: 150,
            height: 20,
        };
        let text = format!("{left}     {right}");
        result.lines.push(RecognizedLine {
            text,
            rect: Some(rect),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(geometry(rect)),
            graphemes: None,
        });
        for (text, x) in [(left, 10), (right, 120)] {
            result.words.push(RecognizedWord {
                text: text.to_owned(),
                rect: Rect {
                    x,
                    y: rect.y,
                    width: 40,
                    height: 20,
                },
                line: row,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry(Rect {
                    x,
                    y: rect.y,
                    width: 40,
                    height: 20,
                })),
            });
        }
    }
    result.text = result
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let grid = recognize(&result, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "A B\t001\r\nC D\t002\r\nE F\t003"
    );
}

#[test]
fn table_layout_wired_assigns_graphemes_and_multiline_to_merged_cell() {
    let source = source(&[
        vec![("合并", 20, 110)],
        vec![("左", 20, 25), ("右", 110, 25)],
    ]);
    let mut image = Image {
        width: 161,
        height: 101,
        bgra: vec![255; 161 * 101 * 4],
        readback_ms: 0.,
    };
    let mut set = |x: u32, y: u32| {
        let index = (y * 161 + x) as usize * 4;
        image.bgra[index..index + 4].copy_from_slice(&[0, 0, 0, 255]);
    };
    for y in [5, 45, 85] {
        for x in 5..=155 {
            set(x, y);
        }
    }
    for x in [5, 155] {
        for y in 5..=85 {
            set(x, y);
        }
    }
    for y in 45..=85 {
        set(80, y);
    }
    let grid = recognize(&source, Some(&image)).unwrap();
    assert_eq!(grid.source, TableSource::Lines);
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.colspan == 2 && cell.text == "合并"));
    assert_eq!(crate::table_export::tsv(&grid).unwrap(), "合并\t\r\n左\t右");
}

#[test]
fn table_layout_full_crop_outside_word_union_is_valid_ppocr_source() {
    let mut source = sample();
    for line in &mut source.lines {
        let rect = line.rect.unwrap();
        let crop = Rect {
            x: rect.x - 2,
            y: rect.y - 2,
            width: rect.width + 4,
            height: rect.height + 4,
        };
        line.geometry = Some(geometry(crop));
        assert!(line.geometry.unwrap().validated_in(rect).is_none());
    }
    let grid = recognize(&source, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "名称\t编号\r\n甲项\t0012\r\n乙项\t0"
    );
}

#[test]
fn table_layout_sampling_uses_full_crop_without_clipping_to_word_union() {
    use crate::recognition::graphemes::{GraphemeSampling, SamplingRun};
    let mut source = sample();
    let mut expected_pieces = 0;
    for index in 0..source.lines.len() {
        let rect = source.lines[index].rect.unwrap();
        let crop = Rect {
            x: rect.x - 2,
            y: rect.y - 2,
            width: rect.width + 4,
            height: rect.height + 4,
        };
        source.lines[index].geometry = Some(geometry(crop));
        let runs: Vec<_> = source.lines[index]
            .text
            .char_indices()
            .enumerate()
            .map(|(character, (byte, text))| SamplingRun {
                source_range: byte..byte + text.len_utf8(),
                steps: 2 + character * 4..4 + character * 4,
            })
            .collect();
        expected_pieces += runs.len();
        let sampling = GraphemeSampling::from_ctc(
            index,
            &source.lines[index],
            &[&source.words[index]],
            &runs,
            runs.len() * 4 + 4,
            100,
            100,
        )
        .unwrap();
        source.lines[index].graphemes = Some(sampling);
    }
    let pieces = source_pieces(&source).unwrap();
    assert_eq!(pieces.len(), expected_pieces);
    assert_eq!(pieces[0].rect.y, source.lines[0].rect.unwrap().y - 2);
    assert!(pieces[0].rect.height > source.lines[0].rect.unwrap().height);
    assert_ne!(pieces[0].rect.x, source.words[0].rect.x);
    let grid = recognize(&source, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "名称\t编号\r\n甲项\t0012\r\n乙项\t0"
    );
}

#[test]
fn table_layout_full_crop_still_rejects_invalid_skew_and_negative_coordinates() {
    let mut negative = sample();
    negative.lines[0].geometry.as_mut().unwrap().quad[0].x = -1.;
    assert!(source_pieces(&negative).is_none());
    let mut skew = sample();
    for point in &mut skew.lines[0].geometry.as_mut().unwrap().quad[1..3] {
        point.y += 5.;
    }
    assert!(source_pieces(&skew).is_none());
    let mut invalid = sample();
    invalid.lines[0].geometry.as_mut().unwrap().quad[0].x = f32::NAN;
    assert!(source_pieces(&invalid).is_none());
}

#[test]
fn table_layout_real_detector_micro_tilt_remains_a_horizontal_table() {
    let mut source = source(&[
        vec![("Item", 10, 59), ("Qty", 120, 53)],
        vec![("Item 1", 10, 59), ("0012", 120, 53)],
        vec![("Item 2", 10, 59), ("0", 120, 53)],
    ]);
    // 使用 wired-light 真实 Tiny 检测的两个阅读轴差值（约 2.6° 和 3.8°）。
    for (index, dx, dy) in [(0, 58.099854_f32, 2.6291695_f32), (1, 52.11093, 3.4586182)] {
        let mut rect = source.lines[index].rect.unwrap();
        let left = rect.x as f32;
        let top = rect.y as f32;
        let geometry = TextGeometry {
            quad: [
                crate::annotation::Point { x: left, y: top },
                crate::annotation::Point {
                    x: left + dx,
                    y: top + dy,
                },
                crate::annotation::Point {
                    x: left + dx,
                    y: top + dy + 20.,
                },
                crate::annotation::Point {
                    x: left,
                    y: top + 20.,
                },
            ],
        };
        rect.height = (20. + dy).ceil() as u32;
        source.lines[index].rect = Some(rect);
        source.lines[index].geometry = Some(geometry);
        source.words[index].rect = rect;
        source.words[index].geometry = Some(geometry);
        source.regions[index] = rect;
    }
    assert!(source_pieces(&source).is_some());
    let grid = recognize(&source, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (3, 2));
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "Item\tQty\r\nItem 1\t0012\r\nItem 2\t0"
    );
}

#[test]
fn table_layout_vertical_and_rtl_crop_do_not_use_horizontal_table_layout() {
    let mut vertical = sample();
    let quad = vertical.lines[0].geometry.unwrap().quad;
    vertical.lines[0].geometry = Some(TextGeometry {
        quad: [quad[1], quad[2], quad[3], quad[0]],
    });
    assert!(Projection::new(vertical.lines[0].geometry.unwrap()).is_some());
    assert!(source_pieces(&vertical).is_none());
    let mut rtl = sample();
    let quad = rtl.lines[0].geometry.unwrap().quad;
    rtl.lines[0].geometry = Some(TextGeometry {
        quad: [quad[2], quad[3], quad[0], quad[1]],
    });
    rtl.lines[0].horizontal_direction = HorizontalDirection::RightToLeft;
    assert!(source_pieces(&rtl).is_none());
}

#[test]
fn table_layout_dark_and_thick_detector_tilt_keeps_complete_table() {
    // 已确认文字正确的 dark Qty、thick Qty/4 阅读轴差值；不放开竖排或方向误判。
    for (dx, dy) in [
        (46.879318_f32, 5.18567_f32),
        (49.448006, 4.922823),
        (23.927078, -2.1655083),
    ] {
        let mut source = source(&[
            vec![("Item", 10, 59), ("Qty", 120, 53)],
            vec![("Item 1", 10, 59), ("0012", 120, 53)],
            vec![("Item 2", 10, 59), ("0", 120, 53)],
        ]);
        let mut rect = source.lines[1].rect.unwrap();
        let left = rect.x as f32;
        let top = rect.y as f32 + (-dy).max(0.);
        let geometry = TextGeometry {
            quad: [
                crate::annotation::Point { x: left, y: top },
                crate::annotation::Point {
                    x: left + dx,
                    y: top + dy,
                },
                crate::annotation::Point {
                    x: left + dx,
                    y: top + dy + 20.,
                },
                crate::annotation::Point {
                    x: left,
                    y: top + 20.,
                },
            ],
        };
        rect.width = dx.ceil() as u32;
        rect.height = (20. + dy.abs()).ceil() as u32;
        source.lines[1].rect = Some(rect);
        source.lines[1].geometry = Some(geometry);
        source.words[1].rect = rect;
        source.words[1].geometry = Some(geometry);
        source.regions[1] = rect;
        assert!(
            source_pieces(&source).is_some(),
            "拒绝已确认微倾 dx={dx} dy={dy}"
        );
        let grid = recognize(&source, None).unwrap();
        assert_eq!((grid.rows(), grid.columns()), (3, 2));
        assert_eq!(
            crate::table_export::tsv(&grid).unwrap(),
            "Item\tQty\r\nItem 1\t0012\r\nItem 2\t0"
        );
    }
}

#[test]
fn table_layout_forced_accepts_two_rows_body_spans_and_single_column() {
    // 自动检测保守拒绝的两行键值表，手动表格识别仍整理成 2×2。
    let two_rows = source(&[
        vec![("姓名", 10, 40), ("张三", 120, 40)],
        vec![("电话", 10, 40), ("123", 120, 40)],
    ]);
    assert!(recognize(&two_rows, None).is_none());
    let grid = recognize_forced(&two_rows, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "姓名\t张三\r\n电话\t123"
    );
    // 正文行跨列：自动淘汰，手动保留为跨列单元格。
    let body_span = source(&[
        vec![("甲", 10, 40), ("1", 120, 40)],
        vec![("乙", 10, 40), ("2", 120, 40)],
        vec![("跨两列的备注", 10, 150)],
        vec![("丙", 10, 40), ("3", 120, 40)],
    ]);
    let grid = recognize_forced(&body_span, None).unwrap();
    assert!(grid
        .cells
        .iter()
        .any(|cell| cell.row == 2 && cell.colspan == 2 && cell.text == "跨两列的备注"));
    // 没有列留白的普通段落退化为单列逐行表，而不是无结果。
    let paragraph = source(&[
        vec![("普通一段文字", 10, 200)],
        vec![("第二行正文", 10, 180)],
    ]);
    assert!(recognize(&paragraph, None).is_none());
    let grid = recognize_forced(&paragraph, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (2, 1));
    // 已可信的结果与自动检测一致。
    assert_eq!(
        recognize_forced(&sample(), None),
        recognize(&sample(), None)
    );
}

#[test]
fn table_layout_region_finds_table_between_paragraphs_and_skips_prose_columns() {
    // 标题与说明夹着三行两列表格：整体不成表，表格行单独成表。
    let mixed = source(&[
        vec![("本周构建报告与说明文字", 10, 220)],
        vec![("模块", 10, 40), ("耗时", 160, 40)],
        vec![("capture", 10, 60), ("38s", 160, 40)],
        vec![("ocr", 10, 40), ("51s", 160, 40)],
        vec![("最后更新于周一由自动任务生成", 10, 260)],
    ]);
    assert!(recognize(&mixed, None).is_none());
    let (grid, lines) = recognize_region(&mixed, None).unwrap();
    assert_eq!(lines, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "模块\t耗时\r\ncapture\t38s\r\nocr\t51s"
    );
    // 双栏长句正文不当作表格。
    let prose = source(&[
        vec![
            ("这是左栏里很长的一句正文内容", 10, 260),
            ("右栏也是一句比较长的正文内容", 320, 260),
        ],
        vec![
            ("左栏第二行继续写下去的文字内容", 10, 260),
            ("右栏第二行同样是连续的正文", 320, 260),
        ],
        vec![
            ("左栏第三行仍然是普通段落正文", 10, 260),
            ("右栏第三行也只是段落的延续", 320, 260),
        ],
        vec![("结尾", 10, 40)],
    ]);
    assert!(recognize_region(&prose, None).is_none());
}

#[test]
fn table_layout_model_fills_merged_cells_without_cutting_source_text() {
    let source = source_at(&[
        (10, vec![("表格上方的说明", 20, 500)]),
        (60, vec![("系统", 20, 64), ("First value", 180, 240)]),
        (88, vec![("Version 24H2", 180, 240)]),
        (160, vec![("Last value", 180, 240)]),
    ]);
    let mut grid = TableGrid {
        row_edges: vec![40, 130, 220],
        column_edges: vec![10, 140, 600],
        cells: Vec::new(),
        confidence: 0.9,
        source: TableSource::Model,
    };
    for (row, column, rowspan) in [(0, 0, 2), (0, 1, 1), (1, 1, 1)] {
        grid.cells.push(TableCell {
            row,
            column,
            rowspan,
            colspan: 1,
            text: String::new(),
            rect: grid.cell_rect(row, column, rowspan, 1).unwrap(),
            confidence: 0.9,
        });
    }
    let (filled, region) = fill_structure(&source, grid.clone(), None).unwrap();
    assert_eq!(region, Some(vec![1, 2, 3, 4]));
    assert_eq!(filled.cells[0].rowspan, 2);
    assert_eq!(filled.cells[0].text, "系统");
    assert_eq!(filled.cells[1].text, "First value\nVersion 24H2");
    assert_eq!(filled.cells[2].text, "Last value");
    grid.column_edges[2] = 220;
    for cell in grid.cells.iter_mut().filter(|cell| cell.column == 1) {
        cell.rect.width = 80;
    }
    assert!(fill_structure(&source, grid, None).is_none());
}

#[test]
fn table_layout_region_keeps_wrapped_key_values_and_excludes_surrounding_text() {
    let mixed = source_at(&[
        (10, vec![("系统信息与硬件配置", 20, 520)]),
        (60, vec![("设备名称", 20, 96), ("DESKTOP TEST", 180, 200)]),
        (108, vec![("设备型号", 20, 96), ("ThinkPad T14", 180, 200)]),
        (156, vec![("处理器", 20, 72), ("Intel Core i7", 180, 200)]),
        (204, vec![("已装内存", 20, 96), ("32 GB RAM", 180, 160)]),
        (
            252,
            vec![("操作系统", 20, 96), ("Microsoft Windows 11 Pro", 180, 360)],
        ),
        (280, vec![("Version 24H2 build 26100", 180, 340)]),
        (
            328,
            vec![
                ("显示适配器", 20, 96),
                ("NVIDIA GeForce RTX 4070", 180, 350),
            ],
        ),
        (356, vec![("Driver version 566.14", 180, 300)]),
        (
            404,
            vec![("存储设备", 20, 96), ("Samsung SSD 990 PRO 2TB", 180, 350)],
        ),
        (432, vec![("Firmware version 4B2QJXD7", 180, 360)]),
        (
            480,
            vec![("网络适配器", 20, 96), ("Intel Wi Fi 6E AX211", 180, 310)],
        ),
        (508, vec![("Connection speed 2402 Mbps", 180, 360)]),
        // 右列的孤立说明与表格隔着明显空白，不能当成最后一格的续行。
        (
            620,
            vec![(
                "This information was collected by the support team",
                180,
                480,
            )],
        ),
    ]);

    assert!(recognize_detected(&mixed, None).is_none());
    let (grid, lines) = recognize_region(&mixed, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (8, 2));
    assert_eq!(lines, (1..21).collect::<Vec<_>>());
    assert_eq!(
        grid.cells
            .iter()
            .filter(|cell| cell.column == 1)
            .map(|cell| cell.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "DESKTOP TEST",
            "ThinkPad T14",
            "Intel Core i7",
            "32 GB RAM",
            "Microsoft Windows 11 Pro\nVersion 24H2 build 26100",
            "NVIDIA GeForce RTX 4070\nDriver version 566.14",
            "Samsung SSD 990 PRO 2TB\nFirmware version 4B2QJXD7",
            "Intel Wi Fi 6E AX211\nConnection speed 2402 Mbps",
        ]
    );
}

#[test]
fn table_layout_region_keeps_values_wrapping_from_the_first_row() {
    let mixed = source_at(&[
        (10, vec![("配置说明", 20, 520)]),
        (
            60,
            vec![("系统", 20, 64), ("Microsoft Windows 11 Pro", 180, 360)],
        ),
        (88, vec![("Version 24H2", 180, 220)]),
        (
            140,
            vec![("显卡", 20, 64), ("NVIDIA GeForce RTX 4070", 180, 350)],
        ),
        (168, vec![("Driver version 566.14", 180, 300)]),
        (
            220,
            vec![("存储", 20, 64), ("Samsung SSD 990 PRO 2TB", 180, 350)],
        ),
        (248, vec![("Firmware version 4B2QJXD7", 180, 360)]),
        (340, vec![("以上是当前设备配置的完整说明", 20, 520)]),
    ]);
    let (grid, lines) = recognize_region(&mixed, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (3, 2));
    assert_eq!(lines, (1..10).collect::<Vec<_>>());
    assert_eq!(grid.cells[1].text, "Microsoft Windows 11 Pro\nVersion 24H2");
    assert_eq!(
        grid.cells[5].text,
        "Samsung SSD 990 PRO 2TB\nFirmware version 4B2QJXD7"
    );
}

#[test]
fn table_layout_borderless_key_values_use_logical_rows_for_multiline_values() {
    let source = source_at(&[
        (10, vec![("设备", 20, 64), ("DESKTOP TEST", 180, 240)]),
        (
            100,
            vec![("系统", 20, 64), ("Microsoft Windows 11 Pro", 180, 360)],
        ),
        (128, vec![("Version 24H2", 180, 220)]),
        (156, vec![("OS build 26100.1742", 180, 300)]),
        (
            210,
            vec![("显卡", 20, 64), ("NVIDIA GeForce RTX 4070", 180, 350)],
        ),
        (238, vec![("Driver version 566.14", 180, 300)]),
        (
            300,
            vec![("存储", 20, 64), ("Samsung SSD 990 PRO 2TB", 180, 350)],
        ),
        (328, vec![("Firmware version 4B2QJXD7", 180, 360)]),
        (
            390,
            vec![("网络", 20, 64), ("Intel Wi Fi 6E AX211", 180, 310)],
        ),
        (418, vec![("Connection speed 2402 Mbps", 180, 360)]),
        (446, vec![("Security type WPA3 Personal", 180, 360)]),
    ]);

    let grid = recognize_detected(&source, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (5, 2));
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "设备\tDESKTOP TEST\r\n系统\t\"Microsoft Windows 11 Pro\nVersion 24H2\nOS build 26100.1742\"\r\n显卡\t\"NVIDIA GeForce RTX 4070\nDriver version 566.14\"\r\n存储\t\"Samsung SSD 990 PRO 2TB\nFirmware version 4B2QJXD7\"\r\n网络\t\"Intel Wi Fi 6E AX211\nConnection speed 2402 Mbps\nSecurity type WPA3 Personal\""
    );
}

#[test]
fn table_layout_horizontal_rules_keep_every_line_of_key_values() {
    let source = source_at(&[
        (
            10,
            vec![("系统", 20, 64), ("Microsoft Windows 11 Pro", 180, 360)],
        ),
        (38, vec![("Version 24H2", 180, 220)]),
        (66, vec![("OS build 26100.1742", 180, 300)]),
        (
            110,
            vec![("显卡", 20, 64), ("NVIDIA GeForce RTX 4070", 180, 350)],
        ),
        (138, vec![("Driver version 566.14", 180, 300)]),
        (
            210,
            vec![("存储", 20, 64), ("Samsung SSD 990 PRO 2TB", 180, 350)],
        ),
        (238, vec![("Firmware version 4B2QJXD7", 180, 360)]),
    ]);
    let detected = DetectedLines {
        kind: LineKind::Ruled,
        horizontal: [0, 100, 200, 300]
            .map(|coordinate| LineSegment {
                coordinate,
                start: 10,
                end: 580,
            })
            .to_vec(),
        vertical: Vec::new(),
        grid: None,
    };

    let grid = recognize_detected(&source, Some(&detected)).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (3, 2));
    assert_eq!(grid.row_edges, vec![0, 100, 200, 300]);
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "系统\t\"Microsoft Windows 11 Pro\nVersion 24H2\nOS build 26100.1742\"\r\n显卡\t\"NVIDIA GeForce RTX 4070\nDriver version 566.14\"\r\n存储\t\"Samsung SSD 990 PRO 2TB\nFirmware version 4B2QJXD7\""
    );
}

#[test]
fn table_layout_horizontal_rule_keeps_a_real_empty_key_row() {
    let source = source_at(&[
        (10, vec![("系统", 20, 64), ("Windows 11", 180, 200)]),
        (38, vec![("This is a separate row", 180, 360)]),
        (110, vec![("显卡", 20, 64), ("GeForce RTX", 180, 200)]),
        (210, vec![("存储", 20, 64), ("Samsung SSD", 180, 200)]),
    ]);
    let detected = DetectedLines {
        kind: LineKind::Ruled,
        horizontal: [0, 34, 100, 200, 300]
            .map(|coordinate| LineSegment {
                coordinate,
                start: 10,
                end: 580,
            })
            .to_vec(),
        vertical: Vec::new(),
        grid: None,
    };
    let grid = recognize_detected(&source, Some(&detected)).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (4, 2));
    assert_eq!(grid.cells[1].text, "Windows 11");
    assert_eq!(grid.cells[2].text, "");
    assert_eq!(grid.cells[3].text, "This is a separate row");
}

#[test]
fn table_layout_wrapped_prose_in_both_columns_is_not_a_key_value_region() {
    let prose = source_at(&[
        (10, vec![("两栏文章标题与编辑说明", 20, 580)]),
        (
            60,
            vec![
                ("左侧是完整段落的第一句正文内容", 20, 260),
                ("右侧同样是完整段落的第一句正文", 340, 260),
            ],
        ),
        (
            88,
            vec![
                ("左侧换行后继续上一句的详细说明", 20, 260),
                ("右侧换行后继续上一句的详细说明", 340, 260),
            ],
        ),
        (
            140,
            vec![
                ("左侧第二段仍然采用正常文字叙述", 20, 260),
                ("右侧第二段也继续采用文字叙述", 340, 260),
            ],
        ),
        (
            168,
            vec![
                ("左侧第二段换行而不含短标签字段", 20, 260),
                ("右侧第二段换行而不含字段对应", 340, 260),
            ],
        ),
        (
            220,
            vec![
                ("左侧第三段没有键与值之间的关系", 20, 260),
                ("右侧第三段没有键与值之间的关系", 340, 260),
            ],
        ),
        (
            248,
            vec![
                ("左侧第三段的末句保持正文显示", 20, 260),
                ("右侧第三段的末句保持正文显示", 340, 260),
            ],
        ),
        (340, vec![("文章到这里结束", 20, 200)]),
    ]);

    assert!(recognize_region(&prose, None).is_none());
}

#[test]
fn table_layout_alignment_snaps_edges_to_detected_lines() {
    // 线检测没拼出网格（如叠了两套线）时，对齐候选的边界仍吸附到留白中的真实线上。
    let segment = |coordinate, start, end| LineSegment {
        coordinate,
        start,
        end,
    };
    let detected = DetectedLines {
        kind: LineKind::None,
        horizontal: [5, 40, 80, 115].map(|y| segment(y, 0, 210)).to_vec(),
        vertical: [5, 70, 200].map(|x| segment(x, 0, 120)).to_vec(),
        grid: None,
    };
    let pieces = source_pieces(&sample()).unwrap();
    let grid = alignment(&pieces, Some(&detected)).unwrap();
    assert_eq!(grid.column_edges, vec![5, 70, 200]);
    assert_eq!(grid.row_edges, vec![5, 40, 80, 115]);
    // 没有检测线时保持留白中点。
    let plain = alignment(&pieces, None).unwrap();
    assert_eq!(plain.column_edges, vec![10, 85, 160]);
}

#[test]
fn table_layout_double_borders_collapse_but_real_empty_rows_stay() {
    let pieces = source_pieces(&sample()).unwrap();
    let build = |row_edges: Vec<u32>, thin_rows: &[usize]| {
        let mut grid = TableGrid {
            row_edges,
            column_edges: vec![0, 100, 200],
            cells: Vec::new(),
            confidence: 1.,
            source: TableSource::Lines,
        };
        for row in 0..grid.rows() {
            // 双线之间没有隔线，线检测给出整行合并格。
            let spans: &[(usize, usize)] = if thin_rows.contains(&row) {
                &[(0, 2)]
            } else {
                &[(0, 1), (1, 1)]
            };
            for &(column, colspan) in spans {
                grid.cells.push(TableCell {
                    row,
                    column,
                    rowspan: 1,
                    colspan,
                    text: String::new(),
                    rect: grid.cell_rect(row, column, 1, colspan).unwrap(),
                    confidence: 1.,
                });
            }
        }
        assert!(grid.validate());
        grid
    };
    let doubled = collapse_thin(build(vec![0, 40, 46, 80, 86, 120], &[1, 3]), &pieces);
    assert_eq!(doubled.row_edges, vec![0, 43, 83, 120]);
    assert!(doubled.cells.iter().all(|cell| cell.colspan == 1));
    // 与文字行等高的空行是真实空行，保留。
    let real = build(vec![0, 40, 80, 120, 160], &[]);
    assert_eq!(collapse_thin(real.clone(), &pieces), real);
}

#[test]
fn table_layout_region_drops_adjacent_two_part_title_and_status_rows() {
    // 卡片标题、图下状态行各有两段留白分隔，紧挨着表格，不能并进表格。
    let mixed = source(&[
        vec![("dense", 10, 50), ("wired 860", 400, 90)],
        vec![
            ("H1", 10, 30),
            ("H2", 120, 30),
            ("H3", 230, 30),
            ("H4", 340, 30),
        ],
        vec![
            ("10", 10, 30),
            ("11", 120, 30),
            ("12", 230, 30),
            ("13", 340, 30),
        ],
        vec![
            ("20", 10, 30),
            ("21", 120, 30),
            ("22", 230, 30),
            ("23", 340, 30),
        ],
        vec![("auto 4x8", 10, 80), ("manual", 300, 60)],
    ]);
    let (grid, lines) = recognize_region(&mixed, None).unwrap();
    assert_eq!((grid.rows(), grid.columns()), (3, 4));
    assert_eq!(lines, (2..14).collect::<Vec<_>>());
}

#[test]
fn table_layout_single_digit_cell_with_vertical_crop_axis_keeps_table() {
    // 单个数字“8”的裁行高于宽，检测给出竖直阅读轴；不能因此整表放弃。
    let mut source = sample();
    let rect = Rect {
        x: 120,
        y: 88,
        width: 14,
        height: 24,
    };
    let quad = geometry(rect).quad;
    // 窄高框的阅读轴被检测成从上到下。
    let vertical = crate::recognition::TextGeometry {
        quad: [quad[1], quad[2], quad[3], quad[0]],
    };
    source.lines[5].rect = Some(rect);
    source.lines[5].geometry = Some(vertical);
    source.words[5].rect = rect;
    source.words[5].geometry = Some(geometry(rect));
    source.regions[5] = rect;
    let grid = recognize(&source, None).unwrap();
    assert_eq!(
        crate::table_export::tsv(&grid).unwrap(),
        "名称\t编号\r\n甲项\t0012\r\n乙项\t0"
    );
}
