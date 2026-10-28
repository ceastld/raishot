use super::*;

#[test]
fn multiline_preserves_markdown_newlines_tabs_and_keeps_single_line_filtering() {
    let source = "其中 \\(n\\)\r\n\n| a | b |\n\t| 1 | 2 |\n";
    assert_eq!(field_insert_text(source, true, MULTILINE_MAX_CHARS), source);
    assert_eq!(
        field_insert_text(source, false, 4096),
        "其中 \\(n\\)| a | b || 1 | 2 |"
    );
    assert_eq!(
        field_insert_text("a\0b\u{7}c\nd", true, MULTILINE_MAX_CHARS),
        "abc\nd"
    );
    assert!(!enter_submits(true, false));
    assert!(enter_submits(true, true));
    assert!(enter_submits(false, false));
    assert!(enter_submits(false, true));
}

#[test]
fn multiline_rows_keep_crlf_offsets_and_empty_trailing_line() {
    let text = "中🙂\r\n\ne\u{301}\n";
    let ranges = multiline_ranges(text);
    assert_eq!(ranges, vec![0..7, 9..9, 10..13, 14..14]);
    assert_eq!(
        ranges
            .iter()
            .map(|range| &text[range.clone()])
            .collect::<Vec<_>>(),
        vec!["中🙂", "", "e\u{301}", ""]
    );
    for (row, range) in ranges.iter().enumerate() {
        assert_eq!(multiline_row(&ranges, range.start), row);
        assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
    }
    assert_eq!(multiline_row(&ranges, 8), 0); // CRLF 中间的原生位置仍属于前一行。
    assert_eq!(multiline_ranges(""), vec![0..0]);
}

#[test]
fn multiline_utf16_and_navigation_cross_chinese_emoji_and_combining_rows() {
    let text = "头🙂\n长e\u{301}行\n尾";
    for byte in text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain([text.len()])
    {
        assert_eq!(byte_offset(text, utf_offset(text, byte)), byte);
    }
    assert_eq!(byte_offset(text, 2), "头".len()); // 不落到 emoji surrogate 内部。
    let caret = "头🙂".len();
    let (_, down) = edit_boundary::navigate(text, caret..caret, false, "down", false, false);
    assert_eq!(&text[..down], "头🙂\n长e\u{301}");
    let (_, up) = edit_boundary::navigate(text, down..down, false, "up", false, false);
    assert_eq!(up, caret);
    let (_, home) = edit_boundary::navigate(text, down..down, false, "home", false, false);
    let (_, end) = edit_boundary::navigate(text, down..down, false, "end", false, false);
    assert_eq!(&text[home..end], "长e\u{301}行");
    let (_, doc_end) = edit_boundary::navigate(text, down..down, false, "end", false, true);
    assert_eq!(doc_end, text.len());
}

#[test]
fn multiline_scrolling_follows_edits_but_preserves_manual_scroll() {
    assert_eq!(
        multiline_scroll_y(px(0.), px(22.), 20, 12, px(132.), true),
        px(154.)
    );
    assert_eq!(
        multiline_scroll_y(px(154.), px(22.), 20, 0, px(132.), false),
        px(154.)
    );
    assert_eq!(
        multiline_scroll_y(px(154.), px(22.), 20, 0, px(132.), true),
        px(0.)
    );
    assert_eq!(
        multiline_scroll_y(px(999.), px(22.), 20, 0, px(132.), false),
        px(308.)
    );
    assert_eq!(
        multiline_scroll_y(px(10.), px(22.), 1, 0, px(132.), false),
        px(0.)
    );
}

#[test]
fn multiline_cancelled_composition_restores_source_and_reverse_selection() {
    let source = "其中 \\(n\\)\n\n| a | b |";
    let mut state = MultilineState::default();
    state.composition.begin(source, 3..source.len(), true);
    state
        .composition
        .begin("其中临时候选\n| a | b |", 6..9, false);
    assert_eq!(
        state.composition.cancel(),
        Some((source.into(), 3..source.len(), true))
    );
    state.composition.begin(source, 0..0, false);
    state.composition.confirm();
    assert!(state.composition.cancel().is_none());
}
