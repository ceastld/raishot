use super::*;
use crate::{
    geometry::Rect,
    recognition::{HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord},
};

fn access() -> OcrFindAccess {
    OcrFindAccess {
        active: true,
        idle: true,
        source_current: true,
        composing: false,
    }
}
fn document(text: &str) -> OcrFindDocument<u64> {
    OcrFindDocument::new(
        OcrFindStamp {
            source: 1,
            result_generation: 2,
            content_revision: 3,
        },
        Arc::from(text),
    )
    .unwrap()
}
fn search(text: &str, query: &str) -> (OcrFind<u64>, OcrFindDocument<u64>) {
    let document = document(text);
    let mut find = OcrFind::default();
    find.update(&document, query, access()).unwrap();
    (find, document)
}
fn ranges(find: &OcrFind<u64>, document: &OcrFindDocument<u64>) -> Vec<Range<usize>> {
    find.ranges(document, access()).unwrap().to_vec()
}
fn current(find: &OcrFind<u64>, document: &OcrFindDocument<u64>) -> Range<usize> {
    let ticket = find.current_ticket(document, access()).unwrap();
    find.resolve_ticket(&ticket, document, access()).unwrap()
}
fn clip() -> Bounds {
    Bounds {
        x: -1000.,
        y: -1000.,
        width: 4000.,
        height: 4000.,
    }
}
/// 真实 SelectionText::from_recognized；四角沿来源阅读轴，不按桌面 x/y 重排。
fn geometry(raw: &str, axis: [f32; 2]) -> SelectionText {
    let normal = [-axis[1], axis[0]];
    let mut recognized = RecognizedText {
        text: raw.into(),
        regions: Vec::new(),
        lines: Vec::new(),
        words: Vec::new(),
    };
    for (line, content) in raw.split('\n').enumerate() {
        let width = (content.graphemes(true).count() as f32 * 20.).max(20.);
        let origin = Point {
            x: 500. + normal[0] * line as f32 * 60.,
            y: 500. + normal[1] * line as f32 * 60.,
        };
        let point = |u: f32, v: f32| Point {
            x: origin.x + axis[0] * u + normal[0] * v,
            y: origin.y + axis[1] * u + normal[1] * v,
        };
        let quad = [
            point(0., 0.),
            point(width, 0.),
            point(width, 20.),
            point(0., 20.),
        ];
        let bounds = crate::transform::bounds(quad);
        let rect = Rect {
            x: bounds.x.floor() as u32,
            y: bounds.y.floor() as u32,
            width: ((bounds.x + bounds.width).ceil() - bounds.x.floor()) as u32,
            height: ((bounds.y + bounds.height).ceil() - bounds.y.floor()) as u32,
        };
        let horizontal_direction = if axis[0] < 0. {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        };
        let projection = Some(TextGeometry { quad });
        recognized.words.push(RecognizedWord {
            text: content.into(),
            rect,
            line,
            horizontal_direction,
            geometry: projection,
        });
        recognized.lines.push(RecognizedLine {
            graphemes: None,
            text: content.into(),
            rect: Some(rect),
            horizontal_direction,
            geometry: projection,
        });
    }
    SelectionText::from_recognized(&recognized)
}
fn prepare(find: &mut OcrFind<u64>, document: &OcrFindDocument<u64>, text: &SelectionText) {
    let ticket = find.current_ticket(document, access()).unwrap();
    find.prepare_location(&ticket, document, text, clip(), access())
        .unwrap();
}

#[test]
fn literal_case_sensitive_matching_preserves_original_bytes() {
    let (find, doc) = search("Rust rust RUST", "rust");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 5, end: 9 }]);
    assert_eq!(&doc.text()[current(&find, &doc)], "rust");
    assert_eq!(doc.text(), "Rust rust RUST");
}
#[test]
fn overlapping_hits_cycle_in_original_order() {
    let (mut find, doc) = search("aaaa", "aaa");
    assert_eq!(ranges(&find, &doc), vec![0..3, 1..4]);
    assert_eq!(current(&find, &doc), 0..3);
    find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    assert_eq!(current(&find, &doc), 1..4);
    find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    assert_eq!(current(&find, &doc), 0..3);
    find.cycle(&doc, OcrFindDirection::Previous, access())
        .unwrap();
    assert_eq!(current(&find, &doc), 1..4);
}
#[test]
fn chinese_combining_and_emoji_ranges_are_complete_graphemes() {
    let raw = "中e\u{301}👨‍👩‍👧🇨🇳 中e\u{301}👨‍👩‍👧🇨🇳";
    for query in ["中", "e\u{301}", "👨‍👩‍👧", "🇨🇳", "中e\u{301}"] {
        let (find, doc) = search(raw, query);
        let boundaries: Vec<_> = raw
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain([raw.len()])
            .collect();
        assert_eq!(find.summary(&doc, access()).unwrap().total, 2);
        for range in ranges(&find, &doc) {
            assert!(boundaries.contains(&range.start) && boundaries.contains(&range.end));
            assert_eq!(&raw[range], query);
        }
    }
}
#[test]
fn partial_combining_emoji_and_flags_do_not_match() {
    for (raw, query) in [
        ("e\u{301}", "e"),
        ("e\u{301}", "\u{301}"),
        ("👨‍👩‍👧", "👩"),
        ("🇨🇳", "🇨"),
    ] {
        let (find, doc) = search(raw, query);
        assert!(ranges(&find, &doc).is_empty());
    }
}
#[test]
fn canonical_equivalents_are_not_normalized() {
    let (find, doc) = search("e\u{301} é", "é");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 4, end: 6 }]);
}
#[test]
fn crlf_and_multiline_hits_preserve_exact_break_bytes() {
    let (find, doc) = search("A\r\nB A\nB", "A\r\nB");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 0, end: 4 }]);
    let (find, doc) = search("A\r\nB A\nB", "A\nB");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 5, end: 8 }]);
    let (find, doc) = search("A\r\nB\nC", "\n");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 4, end: 5 }]);
    let (find, doc) = search("A\r\nB", "\r");
    assert!(ranges(&find, &doc).is_empty());
    let (find, doc) = search("A\r\nB", "\r\n");
    assert_eq!(ranges(&find, &doc), vec![Range { start: 1, end: 3 }]);
}
#[test]
fn empty_query_clears_hits_and_invalidates_previous_intent() {
    let (mut find, doc) = search("a a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert_eq!(find.update(&doc, "", access()).unwrap().total, 0);
    assert!(find.current_ticket(&doc, access()).is_none());
    assert_eq!(
        find.resolve_ticket(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
    assert_eq!(find.query(&doc, access()), Some(""));
}
#[test]
fn no_hit_and_single_hit_navigation_are_stable() {
    let (mut find, doc) = search("xyz", "none");
    assert!(find.cycle(&doc, OcrFindDirection::Next, access()).is_none());
    find.update(&doc, "y", access()).unwrap();
    for direction in [OcrFindDirection::Next, OcrFindDirection::Previous] {
        let ticket = find.cycle(&doc, direction, access()).unwrap();
        assert_eq!(find.resolve_ticket(&ticket, &doc, access()).unwrap(), 1..2);
    }
}
#[test]
fn manual_selection_clears_active_find_intent_but_retains_query() {
    let (mut find, doc) = search("a a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    find.clear_current();
    assert_eq!(
        find.resolve_ticket(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
    assert_eq!(find.query(&doc, access()), Some("a"));
    assert_eq!(find.summary(&doc, access()).unwrap().current, None);
    let ticket = find
        .cycle(&doc, OcrFindDirection::Previous, access())
        .unwrap();
    assert_eq!(find.resolve_ticket(&ticket, &doc, access()).unwrap(), 2..3);
    find.clear_current();
    let ticket = find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    assert_eq!(find.resolve_ticket(&ticket, &doc, access()).unwrap(), 0..1);
}
#[test]
fn query_and_navigation_aba_do_not_resurrect_action_tickets() {
    let (mut find, doc) = search("a b a", "a");
    let old = find.current_ticket(&doc, access()).unwrap();
    find.update(&doc, "b", access()).unwrap();
    find.update(&doc, "a", access()).unwrap();
    assert_eq!(
        find.resolve_ticket(&old, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
    let old = find.current_ticket(&doc, access()).unwrap();
    find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    find.cycle(&doc, OcrFindDirection::Previous, access())
        .unwrap();
    assert_eq!(
        find.resolve_ticket(&old, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
    let old = find.current_ticket(&doc, access()).unwrap();
    find.update(&doc, "a", access()).unwrap();
    assert_eq!(
        find.resolve_ticket(&old, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
}
#[test]
fn source_result_and_editor_revision_each_invalidate_old_hits() {
    for stamp in [
        OcrFindStamp {
            source: 9,
            result_generation: 2,
            content_revision: 3,
        },
        OcrFindStamp {
            source: 1,
            result_generation: 9,
            content_revision: 3,
        },
        OcrFindStamp {
            source: 1,
            result_generation: 2,
            content_revision: 9,
        },
    ] {
        let (mut find, doc) = search("a", "a");
        let ticket = find.current_ticket(&doc, access()).unwrap();
        let replacement = OcrFindDocument::new(stamp, Arc::clone(&doc.text)).unwrap();
        assert!(find.summary(&replacement, access()).is_none());
        find.synchronize(&replacement);
        assert_eq!(find.summary(&replacement, access()).unwrap().total, 0);
        assert_eq!(
            find.resolve_ticket(&ticket, &replacement, access()),
            Err(OcrFindError::StaleIntent)
        );
    }
}
#[test]
fn fresh_snapshot_rejects_same_stamp_and_body_aba() {
    let (mut find, doc) = search("a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    let replacement = OcrFindDocument::new(
        OcrFindStamp {
            source: 1,
            result_generation: 2,
            content_revision: 3,
        },
        Arc::clone(&doc.text),
    )
    .unwrap();
    find.synchronize(&replacement);
    assert_eq!(
        find.resolve_ticket(&ticket, &replacement, access()),
        Err(OcrFindError::StaleIntent)
    );
    assert_eq!(replacement.stamp(), doc.stamp());
}
#[test]
fn changed_body_with_reused_numbers_cannot_share_document_identity() {
    let (mut find, doc) = search("a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    let replacement = document("b");
    find.synchronize(&replacement);
    assert!(find.query(&replacement, access()).unwrap().is_empty());
    assert_eq!(
        find.resolve_ticket(&ticket, &replacement, access()),
        Err(OcrFindError::StaleIntent)
    );
}
#[test]
fn document_and_ticket_clones_share_immutable_body_without_text_copy() {
    let (find, doc) = search("a", "a");
    let copy = doc.clone();
    assert!(Arc::ptr_eq(&doc.text, &copy.text));
    assert!(Arc::ptr_eq(&doc.stamp, &copy.stamp));
    let ticket = find.current_ticket(&copy, access()).unwrap();
    let copy_ticket = ticket.clone();
    assert!(Arc::ptr_eq(
        &ticket.document.text,
        &copy_ticket.document.text
    ));
    assert_eq!(
        find.resolve_ticket(&copy_ticket, &doc, access()).unwrap(),
        0..1
    );
}
#[test]
fn every_real_host_gate_hides_results_and_denies_actions() {
    let blocked = [
        OcrFindAccess {
            active: false,
            ..access()
        },
        OcrFindAccess {
            idle: false,
            ..access()
        },
        OcrFindAccess {
            source_current: false,
            ..access()
        },
        OcrFindAccess {
            composing: true,
            ..access()
        },
        OcrFindAccess::default(),
    ];
    for denied in blocked {
        let (mut find, doc) = search("a", "a");
        let ticket = find.current_ticket(&doc, access()).unwrap();
        assert!(find.summary(&doc, denied).is_none());
        assert!(find.ranges(&doc, denied).is_none());
        assert!(find.current_ticket(&doc, denied).is_none());
        assert!(find.cycle(&doc, OcrFindDirection::Next, denied).is_none());
        assert_eq!(
            find.resolve_ticket(&ticket, &doc, denied),
            Err(OcrFindError::AccessDenied)
        );
        assert_eq!(
            find.update(&doc, "b", denied),
            Err(OcrFindError::AccessDenied)
        );
        assert_eq!(find.query(&doc, access()), Some("a"));
    }
}
#[test]
fn blocked_source_replacement_clears_prior_query_before_returning() {
    let (mut find, doc) = search("a", "a");
    let replacement = document("a");
    assert!(matches!(
        find.update(&replacement, "a", OcrFindAccess::default()),
        Err(OcrFindError::AccessDenied)
    ));
    assert_eq!(find.summary(&replacement, access()).unwrap().total, 0);
    assert!(find.summary(&doc, access()).is_none());
}
#[test]
fn text_and_query_limits_fail_explicitly_and_drop_previous_intent() {
    let max = "a".repeat(MAX_FIND_TEXT_BYTES);
    assert!(OcrFindDocument::new(
        OcrFindStamp {
            source: (),
            result_generation: 1,
            content_revision: 1
        },
        Arc::from(max.as_str())
    )
    .is_ok());
    let too_large = format!("{max}a");
    assert!(matches!(
        OcrFindDocument::new(
            OcrFindStamp {
                source: (),
                result_generation: 1,
                content_revision: 1
            },
            Arc::from(too_large.as_str())
        ),
        Err(OcrFindError::TextTooLarge)
    ));
    let (mut find, doc) = search("a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    let query = "a".repeat(MAX_FIND_QUERY_BYTES + 1);
    assert_eq!(
        find.update(&doc, &query, access()),
        Err(OcrFindError::QueryTooLarge)
    );
    assert_eq!(find.summary(&doc, access()).unwrap().total, 0);
    assert_eq!(
        find.resolve_ticket(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
    assert!(find
        .update(&doc, &query[..MAX_FIND_QUERY_BYTES], access())
        .is_ok());
}
#[test]
fn capped_matches_keep_exact_total_and_never_claim_partial_batch_is_all() {
    let text = "a".repeat(MAX_FIND_HITS + 7);
    let (mut find, doc) = search(&text, "a");
    assert_eq!(
        find.summary(&doc, access()).unwrap(),
        OcrFindSummary {
            stored: MAX_FIND_HITS,
            total: MAX_FIND_HITS + 7,
            truncated: true,
            current: Some(1)
        }
    );
    assert!(find.complete_ranges(&doc, access()).is_none());
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert!(matches!(
        find.resolve_complete_ranges(&ticket, &doc, access()),
        Err(OcrFindError::HitsTruncated)
    ));
    find.cycle(&doc, OcrFindDirection::Previous, access())
        .unwrap();
    assert_eq!(current(&find, &doc), MAX_FIND_HITS - 1..MAX_FIND_HITS);
}
#[test]
fn explicit_batch_range_access_revalidates_same_query_intent() {
    let (mut find, doc) = search("a a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.resolve_complete_ranges(&ticket, &doc, access())
            .unwrap(),
        &[0..1, 2..3]
    );
    find.clear_current();
    assert!(matches!(
        find.resolve_complete_ranges(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    ));
}
#[test]
fn multiline_location_uses_only_existing_nonwhitespace_glyph_quads() {
    let raw = "A \r\nB";
    let selection = geometry(raw, [1., 0.]);
    let (mut find, doc) = search(raw, raw);
    prepare(&mut find, &doc, &selection);
    let location = find.location(&doc, clip(), access()).unwrap();
    assert_eq!(location.range, 0..raw.len());
    assert_eq!(location.quads.len(), 2);
    assert!(location.quads[0][0].y < location.quads[1][0].y);
    assert_eq!(selection.selected(location.range.clone()).unwrap(), raw);
}
#[test]
fn reverse_vertical_and_tilted_locations_preserve_source_reading_axes() {
    for axis in [[1., 0.], [-1., 0.], [0., 1.], [0., -1.], [0.8, 0.6]] {
        let raw = "AB\nCD";
        let selection = geometry(raw, axis);
        let (mut find, doc) = search(raw, "B\nC");
        prepare(&mut find, &doc, &selection);
        let location = find.location(&doc, clip(), access()).unwrap();
        assert_eq!(location.quads.len(), 2);
        let first = location.quads[0];
        let along = [first[1].x - first[0].x, first[1].y - first[0].y];
        assert!(along[0] * axis[0] + along[1] * axis[1] > 0.);
        assert_eq!(selection.selected(location.range.clone()).unwrap(), "B\nC");
    }
}
#[test]
fn negative_global_origin_translation_is_retained() {
    let mut selection = geometry("AB", [1., 0.]);
    selection.translate(-800., -900.);
    let (mut find, doc) = search("AB", "A");
    prepare(&mut find, &doc, &selection);
    let location = find.location(&doc, clip(), access()).unwrap();
    assert!(location.bounds.x < 0. && location.bounds.y < 0.);
}
#[test]
fn whitespace_matches_have_ranges_but_no_synthetic_glyph_location() {
    for raw in ["  ", "\r\n", "\n"] {
        let selection = geometry(raw, [1., 0.]);
        let (mut find, doc) = search(raw, raw);
        let ticket = find.current_ticket(&doc, access()).unwrap();
        assert_eq!(
            find.resolve_ticket(&ticket, &doc, access()).unwrap(),
            0..raw.len()
        );
        assert_eq!(
            find.prepare_location(&ticket, &doc, &selection, clip(), access()),
            Err(OcrFindError::MissingGlyphGeometry)
        );
    }
}
#[test]
fn missing_or_duplicated_source_cells_are_rejected_without_guessing() {
    let (mut find, doc) = search("AB", "AB");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    let mut missing = geometry("AB", [1., 0.]);
    missing.lines[0].cells.pop();
    assert_eq!(
        find.prepare_location(&ticket, &doc, &missing, clip(), access()),
        Err(OcrFindError::MissingGlyphGeometry)
    );
    let mut duplicate = geometry("AB", [1., 0.]);
    let first = &duplicate.lines[0].cells[0];
    let cell = super::super::Cell {
        range: first.range.clone(),
        bounds: first.bounds,
        projected: first.projected,
        source_id: first.source_id,
        reverse_x: first.reverse_x,
    };
    duplicate.lines[0].cells.push(cell);
    assert_eq!(
        find.prepare_location(&ticket, &doc, &duplicate, clip(), access()),
        Err(OcrFindError::AmbiguousGlyphGeometry)
    );
    assert!(find.location(&doc, clip(), access()).is_none());
}
#[test]
fn malformed_projected_geometry_never_falls_back_to_axis_aligned_rect() {
    let (mut find, doc) = search("AB", "A");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    for invalid in 0..4 {
        let mut selection = geometry("AB", [1., 0.]);
        let projected = selection.lines[0].cells[0].projected.as_mut().unwrap();
        match invalid {
            0 => projected.source.quad[1] = projected.source.quad[0],
            1 => projected.start = f64::NAN,
            2 => projected.end = 1.1,
            _ => projected.source_id += 1,
        }
        assert_eq!(
            find.prepare_location(&ticket, &doc, &selection, clip(), access()),
            Err(OcrFindError::InvalidGlyphGeometry)
        );
    }
}
#[test]
fn line_ownership_and_grapheme_cell_ranges_must_be_exact() {
    let (mut find, doc) = search("AB", "AB");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    let mut selection = geometry("AB", [1., 0.]);
    selection.lines[0].range.start = 1;
    assert_eq!(
        find.prepare_location(&ticket, &doc, &selection, clip(), access()),
        Err(OcrFindError::AmbiguousGlyphGeometry)
    );
    let mut selection = geometry("AB", [1., 0.]);
    selection.lines[0].cells[0].range = 0..2;
    assert_eq!(
        find.prepare_location(&ticket, &doc, &selection, clip(), access()),
        Err(OcrFindError::AmbiguousGlyphGeometry)
    );
}
#[test]
fn body_mismatch_invalid_clip_and_partial_visibility_hide_previous_cache() {
    let (mut find, doc) = search("AB", "A");
    let selection = geometry("AB", [1., 0.]);
    prepare(&mut find, &doc, &selection);
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.prepare_location(&ticket, &doc, &geometry("AC", [1., 0.]), clip(), access()),
        Err(OcrFindError::TextMismatch)
    );
    assert!(find.location(&doc, clip(), access()).is_none());
    assert_eq!(
        find.prepare_location(
            &ticket,
            &doc,
            &selection,
            Bounds {
                width: f32::NAN,
                ..clip()
            },
            access()
        ),
        Err(OcrFindError::InvalidClip)
    );
    assert_eq!(
        find.prepare_location(
            &ticket,
            &doc,
            &selection,
            Bounds {
                x: 510.,
                y: 490.,
                width: 100.,
                height: 100.
            },
            access()
        ),
        Err(OcrFindError::OutsideVisibleBounds)
    );
    assert!(find.location(&doc, clip(), access()).is_none());
    assert_eq!(find.summary(&doc, access()).unwrap().total, 1);
}
#[test]
fn location_cache_is_borrowed_and_invalidated_by_viewport_and_query_changes() {
    let (mut find, doc) = search("AB", "A");
    let selection = geometry("AB", [1., 0.]);
    prepare(&mut find, &doc, &selection);
    let first = find.location(&doc, clip(), access()).unwrap() as *const OcrFindLocation;
    let second = find.location(&doc, clip(), access()).unwrap() as *const OcrFindLocation;
    assert_eq!(first, second);
    assert!(find
        .location(&doc, Bounds { x: -999., ..clip() }, access())
        .is_none());
    assert!(find
        .location(
            &doc,
            clip(),
            OcrFindAccess {
                idle: false,
                ..access()
            }
        )
        .is_none());
    find.update(&doc, "B", access()).unwrap();
    assert!(find.location(&doc, clip(), access()).is_none());
}
#[test]
fn location_cost_limits_do_not_erase_valid_text_hits() {
    let raw = "a".repeat(MAX_FIND_LOCATION_QUADS + 1);
    let selection = geometry(&raw, [1., 0.]);
    let (mut find, doc) = search(&raw, &raw);
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.prepare_location(
            &ticket,
            &doc,
            &selection,
            Bounds {
                x: 0.,
                y: 0.,
                width: 20000.,
                height: 20000.
            },
            access()
        ),
        Err(OcrFindError::GeometryTooLarge)
    );
    assert_eq!(
        find.resolve_ticket(&ticket, &doc, access()).unwrap(),
        0..raw.len()
    );
    let mut huge = geometry("a", [1., 0.]);
    let first = &huge.lines[0].cells[0];
    let (bounds, projected, source_id, reverse_x) = (
        first.bounds,
        first.projected,
        first.source_id,
        first.reverse_x,
    );
    huge.lines[0].cells = (0..=MAX_FIND_SOURCE_CELLS)
        .map(|_| super::super::Cell {
            range: 0..1,
            bounds,
            projected,
            source_id,
            reverse_x,
        })
        .collect();
    let (mut find, doc) = search("a", "a");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.prepare_location(&ticket, &doc, &huge, clip(), access()),
        Err(OcrFindError::GeometryTooLarge)
    );
}
#[test]
fn query_navigation_and_location_never_modify_selection_text_or_marks() {
    let selection = geometry("A B\nC", [1., 0.]);
    let original = selection.text.clone();
    let manual = 0..3;
    let old_mark_quads = selection.marking_quads(manual.clone());
    let (mut find, doc) = search(&original, "B\nC");
    prepare(&mut find, &doc, &selection);
    find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    assert_eq!(selection.text, original);
    assert_eq!(selection.selected(manual.clone()).unwrap(), "A B");
    assert_eq!(selection.marking_quads(manual), old_mark_quads);
    assert_eq!(doc.text(), original);
}
#[test]
fn invalidate_drops_document_cache_and_rejects_delayed_intent() {
    let (mut find, doc) = search("A", "A");
    let ticket = find.current_ticket(&doc, access()).unwrap();
    prepare(&mut find, &doc, &geometry("A", [1., 0.]));
    find.invalidate();
    assert!(find.document.is_none());
    assert!(find.summary(&doc, access()).is_none());
    assert!(find.location(&doc, clip(), access()).is_none());
    assert_eq!(
        find.resolve_ticket(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    );
}
#[test]
fn all_ticket_requires_complete_nonempty_matches_and_survives_no_active_hit() {
    let (mut find, doc) = search("a a", "a");
    let old = find.all_ticket(&doc, access()).unwrap();
    find.clear_current();
    assert!(matches!(
        find.resolve_all_ticket(&old, &doc, access()),
        Err(OcrFindError::StaleIntent)
    ));
    let fresh = find.all_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.resolve_all_ticket(&fresh, &doc, access()).unwrap(),
        &[0..1, 2..3]
    );
    assert!(find.current_ticket(&doc, access()).is_none());
    find.update(&doc, "none", access()).unwrap();
    assert!(find.all_ticket(&doc, access()).is_none());
    find.update(&doc, "", access()).unwrap();
    assert!(find.all_ticket(&doc, access()).is_none());
    let raw = "a".repeat(MAX_FIND_HITS + 1);
    let (find, doc) = search(&raw, "a");
    assert!(find.all_ticket(&doc, access()).is_none());
}
#[test]
fn batch_projection_deduplicates_overlapping_graphemes_in_original_order() {
    let selection = geometry("aaaa", [1., 0.]);
    let (find, doc) = search("aaaa", "aaa");
    let ticket = find.all_ticket(&doc, access()).unwrap();
    let quads = find
        .resolve_all_quads(&ticket, &doc, &selection, clip(), access())
        .unwrap();
    assert_eq!(quads.len(), 4);
    for pair in quads.windows(2) {
        assert!(pair[0][0].x < pair[1][0].x);
    }
    assert_eq!(
        find.resolve_all_ticket(&ticket, &doc, access()).unwrap(),
        &[0..3, 1..4]
    );
}
#[test]
fn batch_missing_one_hit_geometry_rejects_the_entire_result() {
    let mut selection = geometry("A A", [1., 0.]);
    selection.lines[0].cells.pop();
    let (find, doc) = search("A A", "A");
    let ticket = find.all_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.resolve_all_quads(&ticket, &doc, &selection, clip(), access()),
        Err(OcrFindError::MissingGlyphGeometry)
    );
    assert_eq!(
        find.resolve_all_ticket(&ticket, &doc, access())
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn batch_intent_source_navigation_manual_selection_and_access_are_rechecked() {
    let selection = geometry("A A", [1., 0.]);
    let (mut find, doc) = search("A A", "A");
    let old = find.all_ticket(&doc, access()).unwrap();
    find.cycle(&doc, OcrFindDirection::Next, access()).unwrap();
    assert_eq!(
        find.resolve_all_quads(&old, &doc, &selection, clip(), access()),
        Err(OcrFindError::StaleIntent)
    );
    let ticket = find.all_ticket(&doc, access()).unwrap();
    assert_eq!(
        find.resolve_all_quads(
            &ticket,
            &doc,
            &selection,
            clip(),
            OcrFindAccess {
                idle: false,
                ..access()
            }
        ),
        Err(OcrFindError::AccessDenied)
    );
    assert_eq!(
        find.resolve_all_quads(&ticket, &document("A A"), &selection, clip(), access()),
        Err(OcrFindError::StaleIntent)
    );
    find.clear_current();
    assert!(matches!(
        find.resolve_all_ticket(&ticket, &doc, access()),
        Err(OcrFindError::StaleIntent)
    ));
}
#[test]
fn batch_quads_budget_is_independent_of_text_hit_limit_and_never_partially_returns() {
    let unit = "a".repeat(9);
    let row = [unit.as_str(); 8].join(" ");
    let raw = vec![row.as_str(); 57].join("\n");
    let selection = geometry(&raw, [1., 0.]);
    let (find, doc) = search(&raw, &unit);
    assert_eq!(find.summary(&doc, access()).unwrap().total, 456);
    let ticket = find.all_ticket(&doc, access()).unwrap();
    let visible = Bounds {
        x: 0.,
        y: 0.,
        width: 65000.,
        height: 65000.,
    };
    // 456*9=4104 个独立字素，超过整批4096；不能返回前4096框作为整批。
    assert_eq!(
        find.resolve_all_quads(&ticket, &doc, &selection, visible, access()),
        Err(OcrFindError::GeometryTooLarge)
    );
    assert_eq!(
        find.resolve_all_ticket(&ticket, &doc, access())
            .unwrap()
            .len(),
        456
    );
}
