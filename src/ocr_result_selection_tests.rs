use super::*;

fn set(indices: &[usize]) -> BTreeSet<usize> {
    indices.iter().copied().collect()
}

fn ranges(source: &str) -> Vec<Range<usize>> {
    let mut offset = 0;
    source
        .split('\n')
        .map(|line| {
            let range = offset..offset + line.len();
            offset = range.end + 1;
            range
        })
        .collect()
}

#[test]
fn ctrl_continues_drag_selection_and_alt_wins() {
    let mut state = ResultSelection::new(vec![false; 5]);
    state.apply_hits(&set(&[]), [0, 1], SelectionMode::Replace);
    let base = state.selected.clone();
    state.apply_hits(
        &base,
        [3, 4],
        SelectionMode::for_gesture(true, false, false, true),
    );
    assert_eq!(state.selected, set(&[0, 1, 3, 4]));
    state.apply_hits(
        &state.selected.clone(),
        [1, 3],
        SelectionMode::for_gesture(true, true, true, true),
    );
    assert_eq!(state.selected, set(&[0, 4]));
    assert!(!state.can_undo());
}

#[test]
fn ctrl_click_toggles_and_shift_click_adds() {
    let mut state = ResultSelection::new(vec![false; 4]);
    state.replace_selection([1, 2]);
    state.apply_hits(
        &state.selected.clone(),
        [1],
        SelectionMode::for_gesture(true, false, false, false),
    );
    assert_eq!(state.selected, set(&[2]));
    state.apply_hits(
        &state.selected.clone(),
        [3],
        SelectionMode::for_gesture(false, true, false, false),
    );
    assert_eq!(state.selected, set(&[2, 3]));
    state.apply_hits(
        &state.selected.clone(),
        [0],
        SelectionMode::for_gesture(false, false, false, false),
    );
    assert_eq!(state.selected, set(&[0]));
}

#[test]
fn shrinking_drag_uses_original_snapshot_and_cancel_restores_it() {
    let mut state = ResultSelection::new(vec![false; 5]);
    state.replace_selection([0]);
    let base = state.selected.clone();
    state.apply_hits(&base, [1, 2, 3], SelectionMode::Add);
    state.apply_hits(&base, [2], SelectionMode::Add);
    assert_eq!(state.selected, set(&[0, 2]));
    state.replace_selection(base);
    assert_eq!(state.selected, set(&[0]));
    assert!(!state.can_undo());
}

#[test]
fn duplicate_toggle_hits_toggle_once_and_modified_blank_preserves_selection() {
    let mut state = ResultSelection::new(vec![false; 3]);
    let base = set(&[0, 1]);
    state.apply_hits(&base, [1, 1, 2, 2], SelectionMode::Toggle);
    assert_eq!(state.selected, set(&[0, 2]));
    for mode in [
        SelectionMode::Add,
        SelectionMode::Subtract,
        SelectionMode::Toggle,
    ] {
        state.apply_hits(&base, [], mode);
        assert_eq!(state.selected, base);
    }
    state.apply_hits(&base, [], SelectionMode::Replace);
    assert!(state.selected.is_empty());
}

#[test]
fn automatic_is_opt_in_and_toggling_keeps_manual_exclusions() {
    let mut state = ResultSelection::new(vec![true, false, true, false]);
    assert!(!state.auto_filter);
    assert_eq!(state.kept_count(), 4);
    state.replace_selection([1, 2, 3]);
    assert!(state.exclude_selected());
    assert!(state.set_auto_filter(true));
    assert_eq!(state.hidden_indices(), vec![0, 1, 2, 3]);
    assert!(state.set_auto_filter(false));
    assert_eq!(state.hidden_indices(), vec![1, 2, 3]);
    assert!(!state.set_auto_filter(false));
    assert!(state.undo());
    assert_eq!(state.kept_count(), 4);
    assert_eq!(state.selected, set(&[1, 2, 3]));
    assert!(!state.can_undo());
}

#[test]
fn enabling_auto_filter_drops_hidden_selection_and_undo_respects_current_switch() {
    let mut state = ResultSelection::new(vec![true, false, false]);
    state.replace_selection([0, 1, 2]);
    state.set_auto_filter(true);
    assert_eq!(state.selected, set(&[1, 2]));
    assert!(!state.is_manually_excluded(0));
    state.exclude_selected();
    assert!(state.is_manually_excluded(1));
    assert!(!state.is_manually_excluded(usize::MAX));
    state.set_auto_filter(false);
    state.undo();
    assert_eq!(state.selected, set(&[1, 2]));
    assert_eq!(state.kept_count(), 3);
    assert!(!state.is_manually_excluded(1));
    state.replace_selection([0]);
    state.exclude_selected();
    state.set_auto_filter(true);
    state.undo();
    assert!(state.is_hidden(0));
    assert!(state.selected.is_empty());
}

#[test]
fn restoring_automatic_items_survives_switches_and_preserves_selection() {
    let mut state = ResultSelection::new(vec![true, false, true]);
    state.set_auto_filter(true);
    state.replace_selection([1]);
    assert!(state.restore([0, 0]));
    assert_eq!(state.selected, set(&[1]));
    assert_eq!(state.hidden_indices(), vec![2]);
    state.set_auto_filter(false);
    state.set_auto_filter(true);
    assert_eq!(state.hidden_indices(), vec![2]);
    assert!(state.undo());
    assert_eq!(state.hidden_indices(), vec![0, 2]);
    assert_eq!(state.selected, set(&[1]));
    assert!(!state.can_undo());
}

#[test]
fn restored_item_can_be_manually_excluded_and_undo_restores_exemption() {
    let mut state = ResultSelection::new(vec![true, false]);
    state.set_auto_filter(true);
    state.restore([0]);
    state.replace_selection([0]);
    state.exclude_selected();
    assert_eq!(state.hidden_indices(), vec![0]);
    state.undo();
    assert!(!state.is_hidden(0));
    assert_eq!(state.selected, set(&[0]));
    state.undo();
    assert!(state.is_hidden(0));
    assert!(state.selected.is_empty());
}

#[test]
fn restoring_manual_automatic_item_while_filter_off_remembers_override() {
    let mut state = ResultSelection::new(vec![true, false]);
    state.replace_selection([0]);
    state.exclude_selected();
    assert!(state.restore([0]));
    state.set_auto_filter(true);
    assert!(!state.is_hidden(0));
    assert!(state.undo());
    assert!(state.is_hidden(0));
}

#[test]
fn bulk_restore_and_exclude_are_one_undo_each_and_empty_edits_are_ignored() {
    let mut state = ResultSelection::new(vec![false; 4]);
    assert!(!state.exclude_selected());
    assert!(!state.restore([0, 100]));
    assert!(!state.undo());
    state.replace_selection([0, 2, 3]);
    assert!(state.exclude_selected());
    assert!(!state.exclude_selected());
    assert!(state.restore(state.hidden_indices()));
    assert!(!state.has_hidden());
    assert!(state.undo());
    assert_eq!(state.hidden_indices(), vec![0, 2, 3]);
    assert!(state.undo());
    assert!(!state.has_hidden());
    assert_eq!(state.selected, set(&[0, 2, 3]));
    assert!(!state.undo());
}

#[test]
fn unknown_and_hidden_indices_never_enter_selection_or_copy() {
    let mut state = ResultSelection::new(vec![false, true]);
    state.set_auto_filter(true);
    state.apply_hits(&set(&[1, 88]), [0, 1, 2, usize::MAX], SelectionMode::Add);
    assert_eq!(state.selected, set(&[0]));
    state.replace_selection([0, 1, usize::MAX]);
    assert_eq!(state.selected_count(), 1);
    assert_eq!(
        state.copy_text("正文\n杂项\n越界", &ranges("正文\n杂项\n越界"), false),
        "正文"
    );
    assert!(!state.restore([2, usize::MAX]));
    assert!(!state.is_hidden(usize::MAX));
    assert!(!state.can_undo());
}

#[test]
fn copying_disjoint_unicode_rows_preserves_order_without_middle_text() {
    let source = "第一行🙂\n不复制这一行\n e\u{301} / 第三行 ";
    let mut state = ResultSelection::new(vec![false; 3]);
    state.replace_selection([2, 0]);
    assert_eq!(
        state.copy_text(source, &ranges(source), true),
        "第一行🙂\n e\u{301} / 第三行 "
    );
    assert_eq!(state.copy_text(source, &ranges(source), false), source);
    state.exclude_selected();
    assert_eq!(
        state.copy_text(source, &ranges(source), false),
        "不复制这一行"
    );
    assert_eq!(state.copy_text(source, &ranges(source), true), "");
}

#[test]
fn copying_rejects_invalid_utf8_ranges_and_preserves_real_whitespace() {
    let source = "中文\n \t \n末尾";
    let state = ResultSelection::new(vec![false; 5]);
    let invalid = vec![1..3, 0..6, 7..10, 30..40, 6..6];
    assert_eq!(state.copy_text(source, &invalid, false), "中文\n \t ");
    assert_eq!(state.copy_text(source, &ranges(source), true), "");
}

#[test]
fn empty_results_remain_empty_for_all_actions() {
    let mut state = ResultSelection::new(Vec::new());
    state.set_auto_filter(true);
    state.apply_hits(&set(&[0]), [0, usize::MAX], SelectionMode::Toggle);
    assert!(state.selected.is_empty());
    assert_eq!(state.selected_count(), 0);
    assert_eq!(state.kept_count(), 0);
    assert!(!state.has_hidden());
    assert!(!state.exclude_selected());
    assert!(!state.restore([0]));
    assert!(!state.can_undo());
    assert!(!state.undo());
    assert_eq!(state.copy_text("", &[0..0], false), "");
}
