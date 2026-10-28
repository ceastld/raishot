use super::*;
use crate::{
    geometry::Rect,
    recognition::{HorizontalDirection, RecognizedLine, RecognizedWord},
};

fn result(raw: &str) -> RecognizedText {
    let mut result = RecognizedText {
        text: raw.into(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (line, content) in raw.split('\n').enumerate() {
        let rect = Rect {
            x: 10,
            y: 10 + line as u32 * 40,
            width: 200,
            height: 20,
        };
        result.regions.push(rect);
        result.words.push(RecognizedWord {
            text: content.into(),
            rect,
            line,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        });
        result.lines.push(RecognizedLine {
            text: content.into(),
            rect: Some(rect),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
            graphemes: None,
        });
    }
    result
}

fn state(raw: &str) -> (RecognizedText, SelectionText, FilterState) {
    let result = result(raw);
    let text = SelectionText::from_recognized(&result);
    let state = FilterState::new(&result, &text);
    (result, text, state)
}

fn modifiers(control: bool, shift: bool, alt: bool) -> gpui::Modifiers {
    gpui::Modifiers {
        control,
        shift,
        alt,
        ..Default::default()
    }
}

fn drag_row(state: &mut FilterState, row: usize, offset: Point, modifiers: gpui::Modifiers) {
    let y = offset.y + 10. + row as f32 * 40.;
    state.begin(
        Point {
            x: offset.x + 5.,
            y: y - 5.,
        },
        modifiers,
    );
    state.update(
        Point {
            x: offset.x + 215.,
            y: y + 25.,
        },
        true,
    );
}

fn click_row(state: &mut FilterState, row: usize, modifiers: gpui::Modifiers) {
    let point = Point {
        x: 30.,
        y: 20. + row as f32 * 40.,
    };
    state.begin(point, modifiers);
    state.update(point, true);
}

fn selected(state: &FilterState) -> Vec<usize> {
    state.model.selected.iter().copied().collect()
}

#[test]
fn roi_translation_uses_canvas_coordinates_without_moving_recognized_source() {
    for offset in [Point { x: -1920., y: 160. }, Point { x: 310., y: 240. }] {
        let result = result("第一行\n第二行\n第三行");
        let source_regions = result.regions.clone();
        let source_word_rects: Vec<_> = result.words.iter().map(|word| word.rect).collect();
        let source_line_rects: Vec<_> = result.lines.iter().map(|line| line.rect).collect();
        let mut text = SelectionText::from_recognized(&result);
        text.translate(offset.x, offset.y);
        let original_quads = text.highlights(0..text.text.len());
        assert!(!original_quads.is_empty());
        let mut filter = FilterState::new(&result, &text);
        drag_row(&mut filter, 1, offset, gpui::Modifiers::default());
        assert_eq!(selected(&filter), vec![1]);
        assert_eq!(filter.copy(&text.text, None, true), "第二行");
        assert_eq!(text.text, result.text);
        assert_eq!(text.highlights(0..text.text.len()), original_quads);
        assert_eq!(result.regions, source_regions);
        assert_eq!(
            result
                .words
                .iter()
                .map(|word| word.rect)
                .collect::<Vec<_>>(),
            source_word_rects
        );
        assert_eq!(
            result
                .lines
                .iter()
                .map(|line| line.rect)
                .collect::<Vec<_>>(),
            source_line_rects
        );
    }
}

#[test]
fn ctrl_continues_boxes_and_ctrl_click_toggles_a_single_line() {
    let (_, _, mut filter) = state("第一行\n第二行\n第三行");
    let origin = Point { x: 0., y: 0. };
    drag_row(&mut filter, 0, origin, gpui::Modifiers::default());
    drag_row(&mut filter, 2, origin, modifiers(true, false, false));
    assert_eq!(selected(&filter), vec![0, 2]);
    click_row(&mut filter, 0, modifiers(true, false, false));
    assert_eq!(selected(&filter), vec![2]);
    click_row(&mut filter, 1, modifiers(false, true, false));
    assert_eq!(selected(&filter), vec![1, 2]);
    assert!(!filter.dragging());
    assert!(filter.marquee().is_empty());
    assert!(!filter.model.can_undo());
}

#[test]
fn alt_has_priority_over_ctrl_for_subtracting_rows() {
    let (_, text, mut filter) = state("首行\n不能夹带\n末行");
    filter.begin(Point { x: 5., y: 5. }, gpui::Modifiers::default());
    filter.update(Point { x: 215., y: 115. }, true);
    assert_eq!(selected(&filter), vec![0, 1, 2]);
    drag_row(
        &mut filter,
        1,
        Point { x: 0., y: 0. },
        modifiers(true, true, true),
    );
    assert_eq!(selected(&filter), vec![0, 2]);
    assert_eq!(
        filter.copy(&text.text, Some(0..text.text.len()), true),
        "首行\n末行"
    );
    click_row(&mut filter, 0, modifiers(true, false, true));
    assert_eq!(selected(&filter), vec![2]);
}

#[test]
fn disjoint_copy_exclusion_review_and_restore_do_not_leak_middle_row() {
    let (_, text, mut filter) = state("需要一\n中间杂项\n需要三");
    click_row(&mut filter, 0, gpui::Modifiers::default());
    click_row(&mut filter, 2, modifiers(true, false, false));
    assert_eq!(filter.copy(&text.text, None, true), "需要一\n需要三");
    filter.model.replace_selection([1]);
    assert!(filter.model.exclude_selected());
    assert_eq!(filter.copy(&text.text, None, false), "需要一\n需要三");
    filter.model.replace_selection([0, 2]);
    let original_selection = selected(&filter);
    assert!(filter.review_quads(&text, false).is_empty());
    assert_eq!(
        filter.review_quads(&text, true),
        text.highlights(filter.ranges[1].clone())
    );
    assert_eq!(filter.quads(&text, false), filter.quads(&text, true));
    assert!(filter.model.restore([1]));
    assert_eq!(selected(&filter), original_selection);
    assert_eq!(filter.copy(&text.text, None, true), "需要一\n需要三");
    assert_eq!(filter.copy(&text.text, None, false), text.text);
    assert!(filter.review_quads(&text, true).is_empty());
}

#[test]
fn cancelling_box_drag_restores_base_and_discards_transient_marquee() {
    let (_, text, mut filter) = state("第一行\n第二行\n第三行");
    click_row(&mut filter, 0, gpui::Modifiers::default());
    let base = selected(&filter);
    filter.begin(Point { x: 5., y: 85. }, modifiers(true, false, false));
    filter.update(Point { x: 215., y: 115. }, false);
    assert_eq!(selected(&filter), vec![0, 2]);
    assert!(filter.dragging());
    assert_eq!(filter.marquee().len(), 1);
    filter.cancel();
    assert_eq!(selected(&filter), base);
    assert!(!filter.dragging());
    assert!(filter.marquee().is_empty());
    assert_eq!(filter.copy(&text.text, None, true), "第一行");
    filter.update(Point { x: 215., y: 115. }, true);
    assert_eq!(selected(&filter), base);
    assert!(!filter.model.can_undo());
}

#[test]
fn new_result_resets_exclusions_selection_modes_and_in_progress_gesture() {
    let (_, _, mut old) = state("旧一\n旧二");
    old.model.replace_selection([0]);
    old.model.exclude_selected();
    old.model.set_auto_filter(true);
    old.changed();
    old.begin(Point { x: 5., y: 5. }, gpui::Modifiers::default());
    let (new_result, new_text, new) = state("新一\n新二\n新三");
    assert!(old.model.has_hidden());
    assert!(old.model.can_undo());
    assert!(old.dragging());
    assert_eq!(new.ranges.len(), 3);
    assert!(!new.model.auto_filter);
    assert!(!new.model.has_hidden());
    assert!(!new.model.can_undo());
    assert!(!new.dragging());
    assert!(new.model.selected.is_empty());
    assert_eq!(new.revision, 0);
    assert_eq!(new.copy(&new_text.text, None, true), new_result.text);
}

#[test]
fn missing_geometry_and_empty_results_keep_copy_available_without_panicking() {
    for raw in ["有文字\n没有几何", ""] {
        let result = RecognizedText {
            text: raw.into(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        };
        let mut text = SelectionText::from_recognized(&result);
        text.translate(-2560., 120.);
        let mut filter = FilterState::new(&result, &text);
        filter.begin(Point { x: -2550., y: 125. }, gpui::Modifiers::default());
        filter.update(Point { x: -2200., y: 300. }, false);
        filter.cancel();
        assert!(filter.model.selected.is_empty());
        assert!(filter.quads(&text, false).is_empty());
        assert!(filter.review_quads(&text, true).is_empty());
        assert_eq!(filter.copy(&text.text, None, false), raw);
        assert!(!filter.model.exclude_selected());
        assert!(!filter.model.undo());
    }
}

#[test]
fn text_mode_clips_utf8_range_and_skips_hidden_rows() {
    let (_, text, mut filter) = state("中文\n不带出\n尾🙂");
    filter.model.replace_selection([1]);
    filter.model.exclude_selected();
    let selected = 3..text.text.len() - "🙂".len();
    assert_eq!(filter.copy(&text.text, Some(selected), true), "文\n尾");
    assert_eq!(
        filter.copy(&text.text, Some(0..text.text.len()), true),
        "中文\n尾🙂"
    );
    assert_eq!(filter.copy(&text.text, Some(1..2), true), "");
    assert_eq!(filter.copy(&text.text, None, false), "中文\n尾🙂");
}

fn fills(document: &Document) -> Vec<(ObjectId, Vec<ShapeBounds>)> {
    document
        .items()
        .iter()
        .filter(|object| object.annotation.tool == Tool::LocalFill)
        .filter_map(|object| match &object.annotation.fill_geometry {
            LocalFillGeometry::Regions(regions) => Some((object.id, regions.to_vec())),
            LocalFillGeometry::Stroke => None,
        })
        .collect()
}

fn padded_row(row: usize) -> ShapeBounds {
    ShapeBounds {
        x: 8.,
        y: 8. + row as f32 * 40.,
        width: 204.,
        height: 24.,
    }
}

fn exclude_rows(
    filter: &mut FilterState,
    text: &SelectionText,
    document: &mut Document,
    ledger: &mut crate::ocr_markup::MarkLedger,
    rows: &[usize],
) -> bool {
    filter.model.replace_selection(rows.iter().copied());
    let edit = filter.exclude(text, document).expect("所选行可排除");
    filter.commit(edit, document, ledger, true)
}

fn restore_rows(
    filter: &mut FilterState,
    document: &mut Document,
    ledger: &mut crate::ocr_markup::MarkLedger,
    rows: Vec<usize>,
) -> bool {
    let edit = filter.restore(rows, document).expect("隐藏行可恢复");
    filter.commit(edit, document, ledger, true)
}

fn undo_once(filter: &mut FilterState, document: &mut Document) {
    match filter.undo_target(document) {
        UndoTarget::Filter { linked } => {
            let command = filter.undo();
            assert_eq!(command.is_some(), linked);
            if let Some(command) = command {
                assert!(document.undo_exact(&command));
            }
        }
        UndoTarget::Document => document.undo(),
        UndoTarget::None => panic!("没有可撤销的筛选"),
    }
}

fn underlay_document() -> Document {
    let mut document = Document::default();
    document.insert(Annotation::new(
        Tool::Pen,
        Point { x: 300., y: 300. },
        Style::default(),
    ));
    document
}

#[test]
fn excluding_rows_inserts_one_padded_local_fill_owned_by_the_session() {
    let (_, text, mut filter) = state("标题\n导航杂项\n侧栏卡片\n正文");
    let mut document = underlay_document();
    let mut ledger = crate::ocr_markup::MarkLedger::default();
    assert!(exclude_rows(
        &mut filter,
        &text,
        &mut document,
        &mut ledger,
        &[1, 2]
    ));
    assert_eq!(filter.model.hidden_indices(), vec![1, 2]);
    let fill = fills(&document);
    assert_eq!(fill.len(), 1);
    assert_eq!(fill[0].1, vec![padded_row(1), padded_row(2)]);
    assert_eq!(document.items().len(), 2);
    assert!(ledger.matches_underlay(&document));

    // 再排除一批仍是同一个填充对象，区域按行序合并。
    assert!(exclude_rows(
        &mut filter,
        &text,
        &mut document,
        &mut ledger,
        &[0]
    ));
    let next = fills(&document);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].0, fill[0].0);
    assert_eq!(next[0].1, vec![padded_row(0), padded_row(1), padded_row(2)]);
}

#[test]
fn one_undo_reverts_fill_and_exclusion_together_after_newer_edits_first() {
    let (_, text, mut filter) = state("一\n二\n三");
    let mut document = underlay_document();
    let mut ledger = crate::ocr_markup::MarkLedger::default();
    exclude_rows(&mut filter, &text, &mut document, &mut ledger, &[1]);
    exclude_rows(&mut filter, &text, &mut document, &mut ledger, &[2]);
    document.insert(Annotation::new(
        Tool::Pen,
        Point { x: 400., y: 400. },
        Style::default(),
    ));
    // 填充之后的手工标注先撤销，筛选不动。
    assert_eq!(filter.undo_target(&document), UndoTarget::Document);
    undo_once(&mut filter, &mut document);
    assert_eq!(filter.model.hidden_indices(), vec![1, 2]);
    assert_eq!(
        filter.undo_target(&document),
        UndoTarget::Filter { linked: true }
    );
    undo_once(&mut filter, &mut document);
    assert_eq!(filter.model.hidden_indices(), vec![1]);
    assert_eq!(fills(&document)[0].1, vec![padded_row(1)]);
    undo_once(&mut filter, &mut document);
    assert!(!filter.model.has_hidden());
    assert!(fills(&document).is_empty());
    assert_eq!(document.items().len(), 1);
    // 筛选没有重做，绑定命令撤销后不能单独重做出填充。
    assert!(!document.can_redo());
    assert_eq!(filter.undo_target(&document), UndoTarget::None);
    assert!(ledger.matches_underlay(&document));
}

#[test]
fn restoring_rows_shrinks_the_fill_and_restore_all_deletes_it() {
    let (_, text, mut filter) = state("一\n二\n三");
    let mut document = underlay_document();
    let mut ledger = crate::ocr_markup::MarkLedger::default();
    exclude_rows(&mut filter, &text, &mut document, &mut ledger, &[0, 1, 2]);
    let id = fills(&document)[0].0;
    assert!(restore_rows(
        &mut filter,
        &mut document,
        &mut ledger,
        vec![1]
    ));
    assert_eq!(
        fills(&document),
        vec![(id, vec![padded_row(0), padded_row(2)])]
    );
    let hidden = filter.model.hidden_indices();
    assert!(restore_rows(
        &mut filter,
        &mut document,
        &mut ledger,
        hidden
    ));
    assert!(fills(&document).is_empty());
    assert!(!filter.model.has_hidden());
    assert!(ledger.matches_underlay(&document));
    // 撤销全部恢复后填充对象按原 ID 回来，可继续逐项编辑。
    undo_once(&mut filter, &mut document);
    assert_eq!(
        fills(&document),
        vec![(id, vec![padded_row(0), padded_row(2)])]
    );
    assert_eq!(filter.model.hidden_indices(), vec![0, 2]);
    assert!(restore_rows(
        &mut filter,
        &mut document,
        &mut ledger,
        vec![0]
    ));
    assert_eq!(fills(&document), vec![(id, vec![padded_row(2)])]);
}

#[test]
fn automatic_filter_only_hides_and_never_changes_the_document() {
    let (_, text, mut filter) = state("正文\n图标\n正文");
    filter.model = ResultSelection::new(vec![false, true, false]);
    let mut document = underlay_document();
    let mut ledger = crate::ocr_markup::MarkLedger::default();
    filter.model.set_auto_filter(true);
    assert_eq!(filter.model.hidden_indices(), vec![1]);
    assert_eq!(document.items().len(), 1);
    // 恢复自动项是筛选增量，但没有填充可改。
    assert!(!restore_rows(
        &mut filter,
        &mut document,
        &mut ledger,
        vec![1]
    ));
    assert_eq!(document.items().len(), 1);
    exclude_rows(&mut filter, &text, &mut document, &mut ledger, &[0]);
    assert_eq!(fills(&document)[0].1, vec![padded_row(0)]);
    undo_once(&mut filter, &mut document);
    assert_eq!(
        filter.undo_target(&document),
        UndoTarget::Filter { linked: false }
    );
    undo_once(&mut filter, &mut document);
    assert_eq!(filter.model.hidden_indices(), vec![1]);
    assert_eq!(document.items().len(), 1);
}

#[test]
fn rows_without_reliable_geometry_or_rejected_scene_are_hidden_but_not_erased() {
    let mut result = result("有定位\n无定位\n有定位");
    result.words.retain(|word| word.line != 1);
    result.lines[1].rect = None;
    let text = SelectionText::from_recognized(&result);
    let mut filter = FilterState::new(&result, &text);
    let mut document = underlay_document();
    let mut ledger = crate::ocr_markup::MarkLedger::default();

    filter.model.replace_selection([1]);
    let edit = filter.exclude(&text, &document).unwrap();
    assert_eq!((edit.lines, edit.unlocated), (1, 1));
    assert!(edit.annotations(&document).is_none());
    assert!(!filter.commit(edit, &mut document, &mut ledger, true));
    assert_eq!(filter.model.hidden_indices(), vec![1]);
    assert_eq!(document.items().len(), 1);

    // Scene 校验失败时只排除，原图与历史不变；之后的批次仍可正常抹去。
    filter.model.replace_selection([0]);
    let edit = filter.exclude(&text, &document).unwrap();
    assert!(edit.annotations(&document).is_some());
    assert!(!filter.commit(edit, &mut document, &mut ledger, false));
    assert_eq!(filter.model.hidden_indices(), vec![0, 1]);
    assert!(fills(&document).is_empty());
    assert!(exclude_rows(
        &mut filter,
        &text,
        &mut document,
        &mut ledger,
        &[2]
    ));
    assert_eq!(fills(&document)[0].1, vec![padded_row(2)]);
    for hidden in [vec![0, 1], vec![1], vec![]] {
        undo_once(&mut filter, &mut document);
        assert_eq!(filter.model.hidden_indices(), hidden);
    }
    assert!(fills(&document).is_empty());
}

#[test]
fn text_selection_and_line_selection_coexist_for_copy_and_exclusion() {
    let (_, text, mut filter) = state(
        "第一行
第二行
第三行",
    );
    let range = filter.ranges[0].start + 3..filter.ranges[1].end - 3;
    assert_eq!(
        filter.rows_in(&range).into_iter().collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(!filter.lines_selected());
    assert_eq!(
        filter.copy(&text.text, Some(range.clone()), true),
        "一行
第二"
    );
    // 选字涉及的行转为整行多选后，复制与排除都按整行。
    filter.model.replace_selection(filter.rows_in(&range));
    assert!(filter.lines_selected());
    assert_eq!(
        filter.copy(&text.text, Some(range.clone()), true),
        "第一行
第二行"
    );
    let mut document = Document::default();
    let mut ledger = crate::ocr_markup::MarkLedger::default();
    let edit = filter.exclude(&text, &document).unwrap();
    assert_eq!(edit.lines, 2);
    filter.commit(edit, &mut document, &mut ledger, true);
    assert!(!filter.lines_selected());
    assert_eq!(
        filter.rows_in(&range).into_iter().collect::<Vec<_>>(),
        Vec::<usize>::new()
    );
    assert_eq!(filter.copy(&text.text, None, false), "第三行");
}
