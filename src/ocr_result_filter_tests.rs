use super::*;
use crate::{annotation::Point, recognition::TextGeometry};

fn line(text: &str, x: u32, y: u32, width: u32, height: u32) -> RecognizedLine {
    RecognizedLine {
        text: text.to_owned(),
        rect: Some(Rect {
            x,
            y,
            width,
            height,
        }),
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
        graphemes: None,
    }
}

fn result(lines: Vec<RecognizedLine>) -> RecognizedText {
    RecognizedText {
        text: lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        regions: Vec::new(),
        words: Vec::new(),
        lines,
    }
}

fn body() -> Vec<RecognizedLine> {
    vec![
        line("Keep the original reading order", 100, 100, 300, 20),
        line("Small details remain available", 100, 130, 280, 20),
        line("Recovery keeps the source intact", 100, 160, 300, 20),
    ]
}

fn detached(text: &str) -> RecognizedLine {
    line(text, 500, 30, 8, 8)
}

#[test]
fn isolated_small_graphic_needs_text_size_and_local_layout_evidence() {
    let mut lines = body();
    lines.push(detached("⚙"));
    let source = result(lines);
    let original_text = source.text.clone();
    let decisions = classify(&source);
    assert_eq!(
        decisions,
        vec![None, None, None, Some(FilterReason::IsolatedSmallGraphic)]
    );
    assert_eq!(decisions[3].unwrap().code(), "isolated-small-graphic");
    assert_eq!(source.text, original_text);
    assert_eq!(
        source.lines[3].rect,
        Some(Rect {
            x: 500,
            y: 30,
            width: 8,
            height: 8
        })
    );
}

#[test]
fn same_symbol_without_a_body_or_without_small_size_stays_visible() {
    assert_eq!(classify(&result(vec![detached("⚙")])), vec![None]);
    let mut lines = body();
    lines.push(line("⚙", 500, 30, 20, 20));
    assert!(classify(&result(lines)).iter().all(Option::is_none));
}

#[test]
fn valid_short_content_is_kept_even_when_small_and_detached() {
    for text in [
        "I",
        "OK",
        "A",
        "ii",
        "¥12.30",
        "2026-10-23",
        "{}",
        "=>",
        "||",
        "...",
        "×",
        "≡",
        "中",
        "あ",
        "한",
        "مرحبا",
        "😀",
        "[✓]",
    ] {
        let mut lines = body();
        lines.push(detached(text));
        assert!(
            classify(&result(lines)).iter().all(Option::is_none),
            "{text}"
        );
    }
}

#[test]
fn nearby_bullets_and_symbols_inside_the_body_are_retained() {
    for symbol in [line("▪", 82, 106, 8, 8), line("✓", 250, 138, 8, 8)] {
        let mut lines = body();
        lines.push(symbol);
        assert!(classify(&result(lines)).iter().all(Option::is_none));
    }
}

#[test]
fn table_column_and_neighbouring_single_characters_protect_symbols() {
    for neighbour in [line("✓", 500, 54, 8, 8), line("中", 522, 30, 8, 8)] {
        let mut lines = body();
        lines.extend([detached("□"), neighbour]);
        assert!(classify(&result(lines)).iter().all(Option::is_none));
    }
}

#[test]
fn local_small_font_context_is_not_compared_only_to_large_global_text() {
    let mut lines = vec![
        line("Large heading one", 100, 100, 300, 30),
        line("Large heading two", 100, 145, 300, 30),
        line("Large heading three", 100, 190, 300, 30),
        line("Large heading four", 100, 235, 300, 30),
        line("Small text line one", 1_000, 100, 130, 10),
        line("Small text line two", 1_000, 116, 130, 10),
        line("Small text line three", 1_000, 132, 130, 10),
    ];
    lines.push(line("⚙", 1_180, 55, 8, 8));
    assert!(classify(&result(lines)).iter().all(Option::is_none));
}

#[test]
fn detached_graphic_far_outside_available_context_is_not_guessed() {
    let mut lines = body();
    lines.push(line("⚙", 5_000, 30, 8, 8));
    assert!(classify(&result(lines)).iter().all(Option::is_none));
}

#[test]
fn scattered_text_without_a_coherent_body_does_not_enable_filtering() {
    let lines = vec![
        line("First small note", 100, 100, 140, 20),
        line("Second small note", 280, 200, 140, 20),
        line("Third small note", 450, 300, 140, 20),
        detached("⚙"),
    ];
    assert!(classify(&result(lines)).iter().all(Option::is_none));
}

#[test]
fn scale_and_canvas_offset_do_not_change_the_decision() {
    let mut lines = body();
    lines.push(detached("⚙"));
    let original = result(lines.clone());
    for scale in [2, 3, 4] {
        for line in &mut lines {
            let rect = line.rect.as_mut().unwrap();
            rect.x = rect.x * scale + 37;
            rect.y = rect.y * scale + 51;
            rect.width *= scale;
            rect.height *= scale;
        }
        assert_eq!(classify(&result(lines.clone())), classify(&original));
    }
}

#[test]
fn missing_invalid_or_stale_evidence_fails_open() {
    for rect in [
        None,
        Some(Rect {
            x: 500,
            y: 30,
            width: 0,
            height: 8,
        }),
        Some(Rect {
            x: u32::MAX,
            y: 30,
            width: 8,
            height: 8,
        }),
    ] {
        let mut lines = body();
        let mut symbol = detached("⚙");
        symbol.rect = rect;
        lines.push(symbol);
        assert!(classify(&result(lines)).iter().all(Option::is_none));
    }
    let mut lines = body();
    lines.push(detached("⚙"));
    let mut stale = result(lines);
    stale.text.push_str("modified");
    assert!(classify(&stale).iter().all(Option::is_none));
}

#[test]
fn rotated_vertical_reverse_and_invalid_projected_lines_stay_visible() {
    let mut variants = Vec::new();
    let mut reverse = detached("⚙");
    reverse.horizontal_direction = HorizontalDirection::RightToLeft;
    variants.push(reverse);
    variants.push(line("⚙", 500, 30, 8, 24));
    for quad in [
        [
            Point { x: 508., y: 30. },
            Point { x: 508., y: 38. },
            Point { x: 500., y: 38. },
            Point { x: 500., y: 30. },
        ],
        [
            Point { x: 500., y: 30. },
            Point { x: 508., y: 34. },
            Point { x: 504., y: 42. },
            Point { x: 496., y: 38. },
        ],
        [Point {
            x: f32::NAN,
            y: 30.,
        }; 4],
    ] {
        let mut symbol = detached("⚙");
        symbol.geometry = Some(TextGeometry { quad });
        variants.push(symbol);
    }
    for variant in variants {
        let mut lines = body();
        lines.push(variant);
        assert!(classify(&result(lines)).iter().all(Option::is_none));
    }
}

#[test]
fn unrelated_geometry_is_rejected_but_full_crop_can_extend_beyond_word_bounds() {
    for (left, right, expected) in [
        (0., 8., None),
        (500., 508., Some(FilterReason::IsolatedSmallGraphic)),
        (400., 508., Some(FilterReason::IsolatedSmallGraphic)),
    ] {
        let mut lines = body();
        let mut symbol = detached("⚙");
        symbol.geometry = Some(TextGeometry {
            quad: [
                Point { x: left, y: 30. },
                Point { x: right, y: 30. },
                Point { x: right, y: 38. },
                Point { x: left, y: 38. },
            ],
        });
        lines.push(symbol);
        assert_eq!(classify(&result(lines))[3], expected);
    }
}

#[test]
fn excessive_input_and_empty_result_keep_original_line_cardinality() {
    assert!(classify(&result(Vec::new())).is_empty());
    let mut lines = vec![line("Body content", 100, 100, 140, 20); MAX_LINES];
    lines.push(detached("⚙"));
    let source = result(lines);
    let decisions = classify(&source);
    assert_eq!(decisions.len(), source.lines.len());
    assert!(decisions.iter().all(Option::is_none));
}

#[test]
fn repeated_long_graphic_runs_are_not_mistaken_for_short_icons() {
    let mut lines = body();
    lines.push(detached("■■■■"));
    assert!(classify(&result(lines)).iter().all(Option::is_none));
}

fn paragraph_with_column(glyphs: [&str; 3]) -> RecognizedText {
    // 开发样本真实识别框的最小复现；不是手工编造的理想图形字符输出。
    result(vec![
        line("城市阅读计划", 47, 32, 189, 41),
        line("周末开放时间为上午九点至下午五点。", 47, 104, 385, 35),
        line(glyphs[0], 794, 120, 49, 50),
        line("请携带有效证件，并在入口完成登记。", 49, 150, 381, 32),
        line(glyphs[1], 796, 198, 42, 48),
        line("Reading brings people together.", 47, 217, 350, 35),
        line("Borrow books and share your discoveries.", 47, 262, 445, 36),
        line(glyphs[2], 792, 273, 54, 58),
        line("广告：附近咖啡店今日营业", 46, 413, 202, 28),
    ])
}

#[test]
fn detached_mixed_graphic_column_improves_on_the_retained_baseline() {
    let source = paragraph_with_column(["9", "N", "√"]);
    assert!(classify_baseline(&source).iter().all(Option::is_none));
    let decisions = classify(&source);
    assert_eq!(
        decisions
            .iter()
            .enumerate()
            .filter_map(|(index, reason)| reason.map(|_| index))
            .collect::<Vec<_>>(),
        vec![2, 4, 7]
    );
    for index in [2, 4, 7] {
        assert_eq!(decisions[index], Some(FilterReason::DetachedGraphicColumn));
        assert_eq!(decisions[index].unwrap().code(), "detached-graphic-column");
    }
}

#[test]
fn rotated_last_glyph_keeps_an_incomplete_graphic_column_visible() {
    let mut source = result(vec![
        line("会议纪要", 45, 26, 121, 45),
        line("下周完成第一轮用户访谈。", 47, 106, 257, 35),
        line("Q", 773, 108, 54, 54),
        line("把异常情况记录在同一份表格里。", 46, 153, 322, 32),
        line("N", 778, 192, 44, 46),
        line("Review pending items before Friday.", 45, 236, 306, 28),
        line("附件编号：2026-19B", 46, 292, 143, 25),
        line("L", 764, 256, 71, 74),
    ]);
    assert!(classify_baseline(&source).iter().all(Option::is_none));
    assert_eq!(
        classify(&source)
            .iter()
            .filter(|reason| reason.is_some())
            .count(),
        3
    );
    // 仅凭轴对齐Rect会形成完整三项列；真实开发结果的L保留了旋转投影，
    // 不能丢掉该元数据后把理想化输入的命中数当作真实OCR结果。
    source.lines[7].geometry = Some(TextGeometry {
        quad: [
            Point {
                x: 764.96436,
                y: 304.21237,
            },
            Point {
                x: 799.98627,
                y: 256.74506,
            },
            Point {
                x: 834.6177,
                y: 281.5717,
            },
            Point {
                x: 799.59576,
                y: 329.039,
            },
        ],
    });
    assert!(classify(&source).iter().all(Option::is_none));
}

#[test]
fn small_body_uses_its_own_character_scale_for_detached_column() {
    let source = result(vec![
        line("Notification settings", 30, 21, 245, 41),
        line("接收通知", 31, 91, 75, 31),
        line("Q", 706, 105, 34, 36),
        line("仅在任务完成或失败时提醒。", 34, 137, 175, 23),
        line("N", 707, 161, 33, 37),
        line("Schedule: Monday / Wednesday / Friday", 32, 182, 274, 26),
        line("√", 706, 211, 36, 49),
        line("应用版本0.4.14·已连接", 32, 229, 150, 25),
        line("三+", 700, 257, 49, 110),
        line("路径C:/work/notes/final.txt", 32, 272, 168, 25),
        line("2", 114, 411, 17, 21),
        line("3", 196, 411, 13, 20),
    ]);
    let decisions = classify(&source);
    assert_eq!(
        decisions
            .iter()
            .enumerate()
            .filter_map(|(index, reason)| reason.map(|_| index))
            .collect::<Vec<_>>(),
        vec![2, 4, 6]
    );
    assert!(decisions[8..].iter().all(Option::is_none));
}

#[test]
fn numbered_alphabetic_non_latin_and_word_columns_stay_visible() {
    for glyphs in [
        ["1", "2", "3"],
        ["A", "B", "C"],
        ["C", "B", "A"],
        ["A", "1", "B"],
        ["Q", "Q", "Q"],
        ["q", "n", "l"],
        ["北", "中", "南"],
        ["OK", "NO", "Go"],
    ] {
        assert!(
            classify(&paragraph_with_column(glyphs))
                .iter()
                .all(Option::is_none),
            "{glyphs:?}"
        );
    }
}

#[test]
fn adjacent_table_labels_or_ordinary_character_scale_keep_short_columns() {
    let mut source = paragraph_with_column(["9", "N", "√"]);
    source.lines.push(line("Row label", 640, 128, 130, 28));
    source = result(source.lines);
    assert!(classify(&source).iter().all(Option::is_none));

    let mut source = paragraph_with_column(["9", "N", "√"]);
    for index in [2, 4, 7] {
        let rect = source.lines[index].rect.as_mut().unwrap();
        rect.width = 25;
        rect.height = 30;
    }
    assert!(classify(&source).iter().all(Option::is_none));
}

#[test]
fn missing_members_or_scattered_short_items_do_not_form_a_graphic_column() {
    let mut source = paragraph_with_column(["9", "N", "√"]);
    source.lines[7].rect = None;
    assert!(classify(&source).iter().all(Option::is_none));
    let mut source = paragraph_with_column(["9", "N", "√"]);
    source.lines[4].rect.as_mut().unwrap().x += 100;
    assert!(classify(&source).iter().all(Option::is_none));
}
