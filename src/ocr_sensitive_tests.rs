use super::*;
use crate::recognition::graphemes::{GraphemeSampling, SamplingRun};

fn quad(rect: Rect, reversed: bool) -> TextGeometry {
    let (x, y, right, bottom) = (
        rect.x as f32,
        rect.y as f32,
        (rect.x + rect.width) as f32,
        (rect.y + rect.height) as f32,
    );
    let points = [
        Point { x, y },
        Point { x: right, y },
        Point {
            x: right,
            y: bottom,
        },
        Point { x, y: bottom },
    ];
    TextGeometry {
        quad: if reversed {
            [points[2], points[3], points[0], points[1]]
        } else {
            points
        },
    }
}

fn recognized(parts: &[&str]) -> RecognizedText {
    let text = parts.join(" ");
    let mut x = 10;
    let mut words = Vec::new();
    for part in parts {
        let rect = Rect {
            x,
            y: 20,
            width: part.chars().count() as u32 * 10,
            height: 20,
        };
        words.push(RecognizedWord {
            text: (*part).to_owned(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: Some(quad(rect, false)),
        });
        x += rect.width + 10;
    }
    let rect = words.iter().map(|word| word.rect).reduce(union_rect);
    RecognizedText {
        text: text.clone(),
        regions: Vec::new(),
        words,
        lines: vec![RecognizedLine {
            text,
            rect,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: rect.map(|rect| quad(rect, false)),
            graphemes: None,
        }],
    }
}

fn add_sampling(source: &mut RecognizedText, reversed: bool) {
    if reversed {
        source.words[0].horizontal_direction = HorizontalDirection::RightToLeft;
        source.words[0].geometry = Some(quad(source.words[0].rect, true));
        source.lines[0].horizontal_direction = HorizontalDirection::RightToLeft;
        source.lines[0].geometry = Some(quad(source.lines[0].rect.unwrap(), true));
    }
    let text = &source.lines[0].text;
    let runs: Vec<_> = text
        .char_indices()
        .enumerate()
        .map(|(index, (byte, ch))| SamplingRun {
            source_range: byte..byte + ch.len_utf8(),
            steps: index * 2..index * 2 + 1,
        })
        .collect();
    let steps = runs.len() * 2;
    let words: Vec<_> = source.words.iter().collect();
    source.lines[0].graphemes = Some(
        GraphemeSampling::from_ctc(0, &source.lines[0], &words, &runs, steps, steps, steps)
            .unwrap(),
    );
}

fn checked_id(prefix: &str) -> String {
    let weights = [7u32, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    let sum: u32 = prefix
        .bytes()
        .zip(weights)
        .map(|(byte, weight)| u32::from(byte - b'0') * weight)
        .sum();
    format!(
        "{prefix}{}",
        char::from(b"10X98765432"[(sum % 11) as usize])
    )
}

#[test]
fn trusted_formats_locate_original_words_and_utf8_ranges() {
    let source = recognized(&[
        "邮箱：",
        "alice@example.test",
        "手机：",
        "13800138000",
        "身份证：",
        "11010519491231002X",
    ]);
    let result = candidates(&source);
    assert_eq!(result.len(), 3);
    assert_eq!(
        result
            .iter()
            .map(|candidate| candidate.kind)
            .collect::<Vec<_>>(),
        [
            SensitiveKind::Email,
            SensitiveKind::Phone,
            SensitiveKind::NationalId
        ]
    );
    for (candidate, expected) in
        result
            .iter()
            .zip(["alice@example.test", "13800138000", "11010519491231002X"])
    {
        assert_eq!(&source.text[candidate.range.clone()], expected);
        assert_eq!(candidate.line, 0);
        assert_eq!(candidate.quads.len(), 1);
        assert!(quad_bounds(candidate.quads[0]).is_some());
    }
}

#[test]
fn arbitrary_numbers_and_malformed_emails_are_not_sensitive_candidates() {
    for line in [
        "13800138000",
        "订单：13800138000",
        "123456 345678 987654",
        "电话：12345678901",
        "电话：13800138000 13900139000",
        "电话：138-----0013--8000",
        "+86138---0013-8000",
        "电话：138 0013-8000",
        "电话：010--12345678",
        "电话：011-12345678",
        "headphone:13800138000",
        "a..b@example.test",
        "a@-example.test",
        "a@example",
        "a@@example.test",
    ] {
        assert!(classify_line(line).unwrap().is_empty(), "错误候选：{line}");
    }
    for line in [
        "电话：13800138000",
        "手机：138-0013-8000",
        "Phone:138 0013 8000",
        "Tel:010-12345678",
        "PHONE: (010) 12345678",
        "+86 13800138000",
        "+8613800138000",
    ] {
        let formats = classify_line(line).unwrap();
        assert_eq!(formats.len(), 1, "遗漏明确电话：{line}");
        assert_eq!(formats[0].0, SensitiveKind::Phone);
    }
}

#[test]
fn national_id_requires_region_date_sequence_and_checksum() {
    assert!(valid_national_id("11010519491231002X"));
    assert!(valid_national_id(&checked_id("11010520000229001")));
    for value in [
        checked_id("99010520000229001"),
        checked_id("11000020000229001"),
        checked_id("11010519000229001"),
        checked_id("11010520260431001"),
        checked_id("11010520000229000"),
        "110105194912310021".to_owned(),
        "11010519491231002X0".to_owned(),
    ] {
        assert!(!valid_national_id(&value), "错误身份证格式：{value}");
    }
}

#[test]
fn missing_or_invalid_word_geometry_never_returns_partial_candidate() {
    let mut source = recognized(&["电话：", "138", "0013", "8000"]);
    assert_eq!(candidates(&source).len(), 1);
    source.words[2].geometry = None;
    assert!(candidates(&source).is_empty());
    let mut source = recognized(&["alice@example.test"]);
    source.words[0].geometry.as_mut().unwrap().quad[1].x = f32::NAN;
    assert!(candidates(&source).is_empty());
    source.words[0].geometry = None;
    assert!(candidates(&source).is_empty());
}

#[test]
fn incomplete_word_coverage_and_duplicated_positions_are_rejected() {
    let mut source = recognized(&["a@example.test", "b@example.test"]);
    source.words.remove(0);
    assert!(candidates(&source).is_empty());
    let mut source = recognized(&["a@example.test", "a@example.test"]);
    source.words[1].rect = source.words[0].rect;
    source.words[1].geometry = source.words[0].geometry;
    source.lines[0].rect = Some(source.words[0].rect);
    assert!(candidates(&source).is_empty());
}

#[test]
fn word_internal_candidate_needs_complete_current_ctc_sampling() {
    let mut source = recognized(&["邮箱：alice@example.test"]);
    assert!(candidates(&source).is_empty());
    add_sampling(&mut source, false);
    let result = candidates(&source);
    assert_eq!(result.len(), 1);
    assert_eq!(&source.text[result[0].range.clone()], "alice@example.test");
    assert_eq!(result[0].quads.len(), "alice@example.test".len());
    source.words[0].rect.width -= 1;
    source.lines[0].rect = Some(source.words[0].rect);
    assert!(candidates(&source).is_empty());
}

#[test]
fn reversed_sampling_keeps_original_reading_quad_direction() {
    let mut source = recognized(&["邮箱：alice@example.test"]);
    add_sampling(&mut source, true);
    let result = candidates(&source);
    assert_eq!(result.len(), 1);
    for quad in &result[0].quads {
        assert!(quad[0].x > quad[1].x);
        assert!(quad_bounds(*quad).is_some());
    }
}

#[test]
fn changed_body_and_cross_line_fragments_are_rejected() {
    let mut source = recognized(&["alice@example.test"]);
    source.text = "bob@example.test".to_owned();
    assert!(Snapshot::from_recognized(&source).is_none());
    assert!(candidates(&source).is_empty());
    let mut source = recognized(&["alice@"]);
    source.text.push_str("\nexample.test");
    source.lines.push(RecognizedLine {
        text: "example.test".into(),
        rect: None,
        horizontal_direction: HorizontalDirection::LeftToRight,
        geometry: None,
        graphemes: None,
    });
    assert!(candidates(&source).is_empty());
}

#[test]
fn candidate_budget_refuses_the_batch_instead_of_silently_truncating() {
    let values: Vec<_> = (0..=MAX_CANDIDATES)
        .map(|index| format!("user{index}@example.test"))
        .collect();
    let parts: Vec<_> = values.iter().map(String::as_str).collect();
    assert!(candidates(&recognized(&parts)).is_empty());
    assert!(classify_line(&parts.join(" ")).is_none());
}

#[test]
fn preview_requires_installed_body_revision_and_current_selection_intent() {
    let source = recognized(&["alice@example.test"]);
    let snapshot = Snapshot::from_recognized(&source).unwrap();
    let stamp = (17u64, 3u64, 0u64); // 图像来源、OCR 安装代、正文修订。
    let mut preview = Preview::new(stamp, &snapshot);
    assert_eq!(preview.text(), source.text);
    assert_eq!(preview.candidates().len(), 1);
    let initial = Arc::clone(preview.identity());
    assert!(!preview.selected(0));
    assert_eq!(
        preview.selected_regions(&stamp, &source.text, &initial),
        Some(Vec::new())
    );
    assert!(!preview.set_selected(0, false));
    assert!(!preview.set_selected(3, true));
    assert!(preview.current(&stamp, &source.text, &initial));
    assert!(preview.set_selected(0, true));
    assert!(preview
        .selected_regions(&stamp, &source.text, &initial)
        .is_none());
    let selected = Arc::clone(preview.identity());
    assert_eq!(
        preview
            .selected_regions(&stamp, &source.text, &selected)
            .unwrap()
            .len(),
        1
    );
    assert!(preview
        .selected_regions(&(17, 3, 2), &source.text, &selected)
        .is_none()); // 编辑再改回同字节。
    assert!(preview
        .selected_regions(&(17, 4, 0), &source.text, &selected)
        .is_none()); // 同正文重识别安装。
    assert!(preview
        .selected_regions(&stamp, "bob@example.test", &selected)
        .is_none());
    let replacement = Preview::new(stamp, &snapshot);
    assert!(!replacement.current(&stamp, &source.text, &selected));
    assert!(preview.set_selected(0, false));
    assert_eq!(
        preview.selected_regions(&stamp, &source.text, preview.identity()),
        Some(Vec::new())
    );
}
