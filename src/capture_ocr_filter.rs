//! OCR 行集合选择与可恢复筛选；原始正文不随筛选改变。手动排除的行在原图上
//! 以一个局部填充标注抹去，筛选增量与该文档命令绑定为同一次撤销。
#[cfg(test)]
#[path = "capture_ocr_filter_tests.rs"]
mod tests;
use super::{panel_click_host::PanelIntent, *};
use crate::{
    annotation::{Annotation, LocalFillGeometry, Style, Tool},
    capture_toolbar::Palette,
    document::{Document, HistoryCommand, ObjectId},
    ocr_result_filter::classify,
    ocr_result_selection::{ResultSelection, SelectionMode},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq)]
pub(super) enum FilterAction {
    Automatic(bool),
    Exclude,
    Restore(usize),
    RestoreAll,
    Undo,
}

pub(super) struct FilterState {
    pub(super) model: ResultSelection,
    pub(super) revision: u64,
    pub(super) ranges: Vec<Range<usize>>,
    bounds: Vec<Option<ShapeBounds>>,
    gesture: Option<BoxGesture>,
    /// 已在原图抹去的手动排除行及其填充区域，按行序构成同一个填充标注。
    erased: BTreeMap<usize, ShapeBounds>,
    fill: Option<ObjectId>,
    /// 与筛选模型的撤销栈一一对应。
    links: Vec<FillLink>,
}

/// 一次筛选增量绑定的文档命令；撤销时只有它仍在文档栈顶才一起回退。
struct FillLink {
    erased: BTreeMap<usize, ShapeBounds>,
    command: Option<HistoryCommand>,
}

enum FillChange {
    Insert(Annotation),
    Replace(ObjectId, Annotation),
    Remove(ObjectId),
}

/// 筛选已改变、文档尚未提交的填充计划；宿主先用 Scene 校验，再提交。
pub(super) struct FillEdit {
    before: BTreeMap<usize, ShapeBounds>,
    erased: BTreeMap<usize, ShapeBounds>,
    change: Option<FillChange>,
    pub(super) lines: usize,
    pub(super) unlocated: usize,
}

impl FillEdit {
    /// 提交后的完整标注列表；不需要改文档时为 None。
    pub(super) fn annotations(&self, document: &Document) -> Option<Vec<Annotation>> {
        let change = self.change.as_ref()?;
        let mut all: Vec<_> = document
            .items()
            .iter()
            .filter_map(|object| match change {
                FillChange::Replace(id, after) if *id == object.id => Some(after.clone()),
                FillChange::Remove(id) if *id == object.id => None,
                _ => Some(object.annotation.clone()),
            })
            .collect();
        if let FillChange::Insert(annotation) = change {
            all.push(annotation.clone());
        }
        Some(all)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UndoTarget {
    None,
    /// 撤销最近的筛选增量；`linked` 表示其填充命令在文档栈顶，一起撤销。
    Filter {
        linked: bool,
    },
    /// 填充之后还有更新的文档编辑，先按普通撤销回退它们。
    Document,
}

struct BoxGesture {
    start: Point,
    end: Point,
    base: BTreeSet<usize>,
    mode: SelectionMode,
    toggle: bool,
    moved: bool,
}

impl FilterState {
    pub(super) fn new(result: &RecognizedText, text: &SelectionText) -> Self {
        let ranges = crate::ocr_panel::line_ranges(&text.text);
        let classified = classify(result);
        let reasons: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(i, range)| {
                result
                    .lines
                    .get(i)
                    .filter(|line| Some(line.text.as_str()) == text.text.get(range.clone()))
                    .and_then(|_| classified.get(i).copied().flatten())
            })
            .collect();
        let bounds = ranges
            .iter()
            .map(|range| quad_bounds(&text.highlights(range.clone())))
            .collect();
        Self {
            model: ResultSelection::new(reasons.iter().map(Option::is_some).collect()),
            revision: 0,
            ranges,
            bounds,
            gesture: None,
            erased: BTreeMap::new(),
            fill: None,
            links: Vec::new(),
        }
    }

    /// 排除所选行；有完整定位的行加入抹去集合，缺定位的只隐藏并计数。
    pub(super) fn exclude(
        &mut self,
        text: &SelectionText,
        document: &Document,
    ) -> Option<FillEdit> {
        let batch: Vec<_> = self
            .model
            .selected
            .iter()
            .copied()
            .filter(|&i| i < self.ranges.len() && !self.model.is_hidden(i))
            .collect();
        if !self.model.exclude_selected() {
            return None;
        }
        let mut erased = self.erased.clone();
        let mut unlocated = 0;
        for i in &batch {
            match fill_region(text, self.ranges[*i].clone()) {
                Some(region) => {
                    erased.insert(*i, region);
                }
                None => unlocated += 1,
            }
        }
        Some(self.plan(document, erased, batch.len(), unlocated))
    }

    /// 恢复的行从填充区域移除；全部恢复后删除该填充标注。
    pub(super) fn restore(&mut self, indices: Vec<usize>, document: &Document) -> Option<FillEdit> {
        let lines = indices.iter().filter(|&&i| self.model.is_hidden(i)).count();
        if !self.model.restore(indices) {
            return None;
        }
        let mut erased = self.erased.clone();
        erased.retain(|i, _| self.model.is_manually_excluded(*i));
        Some(self.plan(document, erased, lines, 0))
    }

    fn plan(
        &self,
        document: &Document,
        erased: BTreeMap<usize, ShapeBounds>,
        lines: usize,
        unlocated: usize,
    ) -> FillEdit {
        let current = self
            .fill
            .filter(|id| document.get(*id).is_some_and(|a| a.tool == Tool::LocalFill));
        let regions: Vec<_> = erased.values().copied().collect();
        let change = match (current, regions.is_empty()) {
            (None, true) => None,
            (Some(id), true) => Some(FillChange::Remove(id)),
            (None, false) => Some(FillChange::Insert(fill_annotation(regions))),
            (Some(id), false) => {
                let after = fill_annotation(regions);
                (document.get(id) != Some(&after)).then_some(FillChange::Replace(id, after))
            }
        };
        FillEdit {
            before: self.erased.clone(),
            erased,
            change,
            lines,
            unlocated,
        }
    }

    /// 每次筛选增量后必须调用一次；`accept` 为 false（Scene 校验失败）时原图不变，
    /// 只保留筛选变化。返回是否提交了文档命令。
    pub(super) fn commit(
        &mut self,
        edit: FillEdit,
        document: &mut Document,
        ledger: &mut crate::ocr_markup::MarkLedger,
        accept: bool,
    ) -> bool {
        let FillEdit {
            before,
            erased,
            change,
            ..
        } = edit;
        let command = match change {
            Some(change) if accept => {
                let revision = document.edit_revision();
                let inserted = match change {
                    FillChange::Insert(after) => {
                        ledger.commit_owned(document, vec![after], &[], &[])
                    }
                    FillChange::Replace(id, after) => {
                        ledger.commit_owned(document, Vec::new(), &[(id, after)], &[])
                    }
                    FillChange::Remove(id) => ledger.commit_owned(document, Vec::new(), &[], &[id]),
                };
                if let Some(&id) = inserted.first() {
                    self.fill = Some(id);
                }
                self.erased = erased;
                (document.edit_revision() != revision)
                    .then(|| document.last_undo().cloned())
                    .flatten()
            }
            Some(_) => None,
            None => {
                self.erased = erased;
                None
            }
        };
        let committed = command.is_some();
        self.links.push(FillLink {
            erased: before,
            command,
        });
        committed
    }

    pub(super) fn undo_target(&self, document: &Document) -> UndoTarget {
        if !self.model.can_undo() {
            return UndoTarget::None;
        }
        match self.links.last().and_then(|link| link.command.as_ref()) {
            Some(command) if document.last_undo() == Some(command) => {
                UndoTarget::Filter { linked: true }
            }
            Some(command) if document.undo_contains(command) => UndoTarget::Document,
            _ => UndoTarget::Filter { linked: false },
        }
    }

    /// 撤销最近的筛选增量，返回需一起撤销的文档命令（宿主确认仍在栈顶后撤销）。
    pub(super) fn undo(&mut self) -> Option<HistoryCommand> {
        if !self.model.undo() {
            return None;
        }
        let link = self.links.pop()?;
        self.erased = link.erased;
        link.command
    }

    /// 整行多选与选字并存：空白处拖框得到行集合，文字上拖动得到选字范围，二者互斥生效。
    pub(super) fn lines_selected(&self) -> bool {
        !self.model.selected.is_empty()
    }

    /// 选字范围涉及的未隐藏行，用于从选字转为整行多选或排除。
    pub(super) fn rows_in(&self, range: &Range<usize>) -> BTreeSet<usize> {
        self.ranges
            .iter()
            .enumerate()
            .filter(|(i, row)| {
                !self.model.is_hidden(*i) && row.start < range.end && row.end > range.start
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub(super) fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub(super) fn dragging(&self) -> bool {
        self.gesture.is_some()
    }
    pub(super) fn cancel(&mut self) {
        if let Some(gesture) = self.gesture.take() {
            self.model.replace_selection(gesture.base);
            self.changed();
        }
    }
    pub(super) fn begin(&mut self, p: Point, m: gpui::Modifiers) {
        self.gesture = Some(BoxGesture {
            start: p,
            end: p,
            base: self.model.selected.clone(),
            mode: SelectionMode::for_gesture(m.control, m.shift, m.alt, true),
            toggle: m.control && !m.alt,
            moved: false,
        });
        self.changed();
    }
    pub(super) fn update(&mut self, p: Point, finish: bool) {
        let Some(mut gesture) = self.gesture.take() else {
            return;
        };
        gesture.end = p;
        gesture.moved |= (p.x - gesture.start.x).hypot(p.y - gesture.start.y) > 4.;
        let x = p.x.min(gesture.start.x);
        let y = p.y.min(gesture.start.y);
        let right = p.x.max(gesture.start.x);
        let bottom = p.y.max(gesture.start.y);
        let hits: Vec<_> = self
            .bounds
            .iter()
            .enumerate()
            .filter_map(|(i, bounds)| {
                let bounds = bounds.as_ref()?;
                let hit = if gesture.moved {
                    bounds.x < right
                        && bounds.x + bounds.width > x
                        && bounds.y < bottom
                        && bounds.y + bounds.height > y
                } else {
                    bounds.contains(p, 0.)
                };
                (hit && !self.model.is_hidden(i)).then_some(i)
            })
            .collect();
        let mode = if finish && !gesture.moved && gesture.toggle {
            SelectionMode::Toggle
        } else {
            gesture.mode
        };
        // 单击只命中最上面的一行；拖框可以同时选中多个重叠行。
        let hits = if !gesture.moved {
            hits.last().copied().into_iter().collect()
        } else {
            hits
        };
        self.model.apply_hits(&gesture.base, hits, mode);
        if !finish {
            self.gesture = Some(gesture);
        }
        self.changed();
    }
    pub(super) fn quads(&self, text: &SelectionText, selected: bool) -> Vec<[Point; 4]> {
        self.ranges
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                !self.model.is_hidden(*i) && (!selected || self.model.selected.contains(i))
            })
            .flat_map(|(_, range)| text.highlights(range.clone()))
            .collect()
    }
    pub(super) fn review_quads(&self, text: &SelectionText, reviewing: bool) -> Vec<[Point; 4]> {
        if !reviewing {
            return Vec::new();
        }
        self.ranges
            .iter()
            .enumerate()
            .filter(|(i, _)| self.model.is_hidden(*i))
            .flat_map(|(_, range)| text.highlights(range.clone()))
            .collect()
    }
    pub(super) fn marquee(&self) -> Vec<[Point; 4]> {
        self.gesture
            .as_ref()
            .filter(|g| g.moved)
            .map(|g| {
                let (x, y, r, b) = (
                    g.start.x.min(g.end.x),
                    g.start.y.min(g.end.y),
                    g.start.x.max(g.end.x),
                    g.start.y.max(g.end.y),
                );
                vec![[
                    Point { x, y },
                    Point { x: r, y },
                    Point { x: r, y: b },
                    Point { x, y: b },
                ]]
            })
            .unwrap_or_default()
    }
    pub(super) fn copy(
        &self,
        text: &str,
        selected: Option<Range<usize>>,
        selected_only: bool,
    ) -> String {
        if self.lines_selected() {
            return self.model.copy_text(text, &self.ranges, selected_only);
        }
        let Some(selected) = selected.filter(|_| selected_only) else {
            return self.model.copy_text(text, &self.ranges, false);
        };
        self.ranges
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.model.is_hidden(*i))
            .filter_map(|(_, range)| {
                let start = range.start.max(selected.start);
                let end = range.end.min(selected.end);
                (start < end).then(|| text.get(start..end)).flatten()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 四角外接矩形；退化或非有限的定位不参与命中与填充。
fn quad_bounds(quads: &[[Point; 4]]) -> Option<ShapeBounds> {
    let mut points = quads.iter().flatten();
    let first = *points.next()?;
    let (mut x, mut y, mut r, mut b) = (first.x, first.y, first.x, first.y);
    for point in points {
        x = x.min(point.x);
        y = y.min(point.y);
        r = r.max(point.x);
        b = b.max(point.y);
    }
    ([x, y, r, b].iter().all(|v| v.is_finite()) && r > x && b > y).then_some(ShapeBounds {
        x,
        y,
        width: r - x,
        height: b - y,
    })
}

/// 完整裁行框（倾斜／竖排取四角外接）外扩 2px 覆盖抗锯齿边；字形定位不完整时不抹去。
fn fill_region(text: &SelectionText, range: Range<usize>) -> Option<ShapeBounds> {
    let bounds = quad_bounds(&text.complete_marking_quads(range).ok()?)?;
    Some(ShapeBounds {
        x: bounds.x - 2.,
        y: bounds.y - 2.,
        width: bounds.width + 4.,
        height: bounds.height + 4.,
    })
}

/// 底色与纹理沿用局部填充的四周取样。
fn fill_annotation(regions: Vec<ShapeBounds>) -> Annotation {
    let first = regions[0];
    let mut annotation = Annotation::new(
        Tool::LocalFill,
        Point {
            x: first.x,
            y: first.y,
        },
        Style::default(),
    );
    annotation.fill_geometry = LocalFillGeometry::Regions(regions.into());
    annotation
}

impl CaptureView {
    pub(super) fn ocr_filter_action(
        &mut self,
        action: FilterAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.end_ocr_overlay_drag();
        self.cancel_ocr_sensitive_preview();
        self.clear_find_preview();
        if window.is_window_active() && !self.focus.is_focused(window) {
            window.focus(&self.focus);
        }
        if action == FilterAction::Undo {
            self.undo_ocr_filter(window, cx);
            return;
        }
        let Some(overlay) = &mut self.ocr_overlay else {
            return;
        };
        overlay.panel_click.borrow_mut().cancel();
        let mut edit = None;
        match action {
            FilterAction::Automatic(enabled) => {
                overlay.filter.model.set_auto_filter(enabled);
                overlay.selected = None;
                overlay.keyboard_selection = None;
            }
            FilterAction::Exclude => {
                // 只有选字时按其涉及的整行排除。
                if !overlay.filter.lines_selected() {
                    let Some(range) = overlay.selected.clone() else {
                        return;
                    };
                    let rows = overlay.filter.rows_in(&range);
                    overlay.filter.model.replace_selection(rows);
                    overlay.selected = None;
                    overlay.keyboard_selection = None;
                }
                edit = overlay.filter.exclude(&overlay.text, &self.editor.document);
            }
            FilterAction::Restore(index) => {
                edit = overlay.filter.restore(vec![index], &self.editor.document);
            }
            FilterAction::RestoreAll => {
                let ids = overlay.filter.model.hidden_indices();
                edit = overlay.filter.restore(ids, &self.editor.document);
            }
            FilterAction::Undo => unreachable!("撤销已单独处理"),
        }
        overlay.filter.changed();
        if let Some(edit) = edit {
            self.commit_ocr_exclude_fill(edit, action == FilterAction::Exclude);
        }
        cx.notify();
    }

    /// 校验填充后的 Scene 再提交；失败时筛选照常生效，原图不变并提示原因。
    fn commit_ocr_exclude_fill(&mut self, edit: FillEdit, excluding: bool) {
        let (lines, unlocated) = (edit.lines, edit.unlocated);
        let scene = edit.annotations(&self.editor.document).map(|all| {
            let scene = self.prepared_scene().updated(&all, &self.source_pixels());
            if scene.errors.is_empty() {
                Ok(scene)
            } else {
                Err(scene.errors.join(";"))
            }
        });
        let Some(overlay) = &mut self.ocr_overlay else {
            return;
        };
        let accept = !matches!(scene, Some(Err(_)));
        let committed =
            overlay
                .filter
                .commit(edit, &mut self.editor.document, &mut overlay.ledger, accept);
        if committed {
            self.editor.reset_group_scope();
            self.scene_stamp = None;
            if let Some(target) = self.current_ocr_target() {
                self.ocr_overlay.as_mut().expect("原位文字层已核验").target = target.clone();
                self.refresh_ocr_result_target(target);
            }
        }
        self.status = match scene {
            Some(Err(error)) => trf!("原图未抹去：{}", "Image not erased: {}", error),
            Some(Ok(scene)) if committed => {
                self.scene = Arc::new(scene);
                if !excluding {
                    return;
                }
                exclude_status(lines, unlocated)
            }
            _ if !excluding => return,
            _ => exclude_status(lines, unlocated),
        }
        .into();
    }

    /// 筛选撤销与其绑定的填充命令一起回退；更新的文档编辑先按普通撤销处理。
    fn undo_ocr_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = &mut self.ocr_overlay else {
            return;
        };
        overlay.panel_click.borrow_mut().cancel();
        match overlay.filter.undo_target(&self.editor.document) {
            UndoTarget::None => return,
            UndoTarget::Document => {
                self.perform(Action::Undo, window, cx);
            }
            UndoTarget::Filter { .. } => {
                let command = overlay.filter.undo();
                overlay.filter.changed();
                if command.is_some_and(|command| self.editor.undo_exact(&command)) {
                    self.reset_input();
                    self.refresh_ocr_overlay_after_history();
                }
            }
        }
        cx.notify();
    }

    pub(super) fn ocr_filter_rows(
        &mut self,
        rows: Range<usize>,
        modifiers: gpui::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_ocr_sensitive_preview();
        self.clear_find_preview();
        if window.is_window_active() && !self.focus.is_focused(window) {
            window.focus(&self.focus);
        }
        let Some(overlay) = &mut self.ocr_overlay else {
            return;
        };
        // Ctrl／Alt 点击列表行时，以当前选字涉及的行为起点追加或减选。
        overlay.panel.close_menu();
        let base = match overlay.selected.take() {
            Some(range) if !overlay.filter.lines_selected() => overlay.filter.rows_in(&range),
            _ => overlay.filter.model.selected.clone(),
        };
        overlay.keyboard_selection = None;
        let mode =
            SelectionMode::for_gesture(modifiers.control, modifiers.shift, modifiers.alt, false);
        overlay.filter.model.apply_hits(&base, rows, mode);
        overlay.filter.changed();
        cx.notify();
    }

    pub(super) fn render_ocr_filter_popup(
        &self,
        layout: crate::ocr_panel::PanelLayout,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let overlay = self.ocr_overlay.as_ref().expect("OCR 存在");
        let filter = &overlay.filter;
        let chip = |id, label: String, active| {
            div()
                .id(id)
                .flex_shrink_0()
                .px(px(7.))
                .py(px(4.))
                .rounded_sm()
                .text_size(px(12.))
                .cursor_pointer()
                .when(active, |v| v.bg(rgb(p.selected)).text_color(rgb(p.accent)))
                .hover(move |s| s.bg(rgb(p.hover)))
                .child(label)
        };
        let bind = |id, label, active, action, cx: &mut Context<Self>| {
            self.bind_ocr_panel_click(
                chip(id, label, active),
                id,
                PanelIntent::Filter(action),
                window,
                cx,
            )
        };
        let excludable = if filter.lines_selected() {
            filter.model.selected_count()
        } else {
            overlay
                .selected
                .as_ref()
                .map_or(0, |range| filter.rows_in(range).len())
        };
        let mut actions = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .flex_wrap()
            .gap(px(4.));
        if excludable > 0 {
            actions = actions.child(bind(
                "ocr-filter-exclude",
                trf!("排除所选 {}", "Exclude {}", excludable),
                false,
                FilterAction::Exclude,
                cx,
            ));
        }
        if filter.model.can_undo() {
            actions = actions.child(bind(
                "ocr-filter-undo",
                tr!("撤销", "Undo").to_owned(),
                false,
                FilterAction::Undo,
                cx,
            ));
        }
        let mut hidden_header = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .gap(px(4.))
            .text_size(px(12.))
            .text_color(rgb(p.muted))
            .child(trf!(
                "已过滤 {}",
                "Filtered {}",
                filter.model.hidden_indices().len()
            ));
        if filter.model.has_hidden() {
            hidden_header = hidden_header.child(bind(
                "ocr-filter-restore-all",
                tr!("恢复全部", "Restore all").to_owned(),
                false,
                FilterAction::RestoreAll,
                cx,
            ));
        }
        let automatic = self.bind_ocr_panel_click(
            div()
                .id("ocr-filter-auto")
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(7.))
                .py(px(6.))
                .rounded_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(p.hover)))
                .child(tr!("自动过滤", "Auto filter"))
                .child(super::panel_host::mini_switch(filter.model.auto_filter, p)),
            "ocr-filter-auto",
            PanelIntent::Filter(FilterAction::Automatic(!filter.model.auto_filter)),
            window,
            cx,
        );
        let mut popup = div()
            .id("ocr-filter-popup")
            .absolute()
            .left(px(8.))
            .right(px(8.))
            .bottom(px(crate::ocr_panel::FOOTER_HEIGHT + 6.))
            .max_h(px((layout.panel.height
                - crate::ocr_panel::HEADER_HEIGHT
                - crate::ocr_panel::FOOTER_HEIGHT
                - 16.)
                .clamp(1., 360.)))
            .flex()
            .flex_col()
            .px(px(8.))
            .py(px(8.))
            .gap(px(6.))
            .rounded_md()
            .bg(rgb(p.background))
            .occlude()
            .border_1()
            .border_color(rgb(p.border))
            .shadow_lg()
            .overflow_y_scroll()
            .track_scroll(overlay.panel.filter_scroll())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.cancel_ocr_panel_click();
                    cx.stop_propagation();
                }),
            )
            .child(automatic);
        if excludable > 0 || filter.model.can_undo() {
            popup = popup.child(actions);
        }
        popup = popup
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "空白处拖框选整行，Ctrl 追加 · Alt 减选 · Delete 排除",
                        "Drag from blank to select lines; Ctrl adds, Alt removes, Delete excludes"
                    )),
            )
            .child(div().h(px(1.)).flex_shrink_0().bg(rgb(p.border)))
            .child(hidden_header);
        if filter.model.has_hidden() {
            popup = popup.children(self.render_ocr_filtered_rows(window, p, cx));
        } else {
            popup = popup.child(
                div()
                    .flex_shrink_0()
                    .px(px(7.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(tr!("没有已过滤的内容", "Nothing filtered")),
            );
        }
        popup.into_any_element()
    }

    pub(super) fn render_ocr_filtered_rows(
        &self,
        window: &Window,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let overlay = self.ocr_overlay.as_ref().expect("OCR 存在");
        overlay
            .filter
            .model
            .hidden_indices()
            .into_iter()
            .map(|i| {
                let content = overlay
                    .filter
                    .ranges
                    .get(i)
                    .and_then(|r| overlay.text.text.get(r.clone()))
                    .unwrap_or("")
                    .to_owned();
                let button = div()
                    .id(("ocr-filter-restore", i))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .px(px(6.))
                    .py(px(3.))
                    .text_size(px(12.))
                    .text_color(rgb(p.accent))
                    .child(tr!("恢复", "Restore"));
                let button = self.bind_ocr_panel_click(
                    button,
                    "ocr-filter-restore",
                    PanelIntent::Filter(FilterAction::Restore(i)),
                    window,
                    cx,
                );
                div()
                    .flex_shrink_0()
                    .px(px(8.))
                    .py(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(13.))
                                    .child(content),
                            )
                            .child(button),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

fn exclude_status(lines: usize, unlocated: usize) -> String {
    if unlocated > 0 {
        trf!(
            "已排除 {} 行；{} 行无可靠定位，未抹去",
            "Excluded {} lines; {} without reliable location were not erased",
            lines,
            unlocated
        )
    } else {
        trf!(
            "已排除 {} 行并抹去原图文字；Ctrl+Z 撤销",
            "Excluded {} lines and erased them from the image; Ctrl+Z undoes",
            lines
        )
    }
}
