//! 文字正文的原对象按下归属；不创建对象移动手势，不提交文档命令。
use crate::{
    annotation::{Annotation, Bounds, Point, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    text_input::TextInputState,
};
use std::ops::Range;

pub(crate) fn select_visual_line(
    input: &mut TextInputState,
    text: &str,
    range: Range<usize>,
) -> bool {
    if range.start > range.end
        || !text.is_char_boundary(range.start)
        || !text.is_char_boundary(range.end)
    {
        return false;
    }
    input.select_to(
        crate::text_input::edit_boundary::floor(text, range.start),
        crate::text_input::edit_boundary::ceil(text, range.end),
    );
    input.drag_anchor = None;
    true
}

pub(crate) fn text_core_hit(a: &Annotation, point: Point, tolerance: f32, scale: f32) -> bool {
    if a.tool != Tool::Text
        || a.handles()
            .iter()
            .any(|handle| handle.distance(point) <= tolerance)
    {
        return false;
    }
    let b = a.local_bounds();
    let inset = 4. * scale;
    let core = if b.width > inset * 2. && b.height > inset * 2. {
        Bounds {
            x: b.x + inset,
            y: b.y + inset,
            width: b.width - inset * 2.,
            height: b.height - inset * 2.,
        }
    } else {
        b
    };
    core.contains(a.to_local(point), 0.)
}

pub(crate) struct TextCorePress {
    pub(crate) original: Annotation,
    pub(crate) point: Point,
    id: ObjectId,
    crop: Option<Rect>,
    revision: u64,
    group_scope: Vec<u64>,
    group_scope_generation: u64,
    pub(crate) dragged: bool,
    /// 仅宿主实际 SetCapture/GetCapture 成功后填写，不参与纯模型的原生调用。
    pub(crate) capture_owner: Option<usize>,
}

/// 成功结束正文编辑先撤原按下；no-op 文档收尾也不能让迟到 Up 再建草稿。
pub(crate) fn terminate_text_core_press(
    press: &mut Option<TextCorePress>,
    down: &mut Option<(i32, i32)>,
    input: &TextInputState,
) -> bool {
    if input.marked.is_some() || press.take().is_none() {
        return false;
    }
    *down = None;
    true
}
impl TextCorePress {
    pub(crate) fn candidate(
        editor: &Editor,
        point: Point,
        tolerance: f32,
        scale: f32,
        ordinary: bool,
    ) -> Option<Self> {
        if editor.tool != Tool::Text
            || editor.text_draft.is_some()
            || editor.is_dragging()
            || editor.has_pending_drawing()
            || editor.selected_ids().len() > 1
            || editor.lasso_draft.is_some()
            || editor.watermark.has_preview()
            || editor.local_fill_candidate().is_some()
            || !point.x.is_finite()
            || !point.y.is_finite()
            || (!editor.canvas.contains(point.rounded()) && (ordinary || editor.crop.is_some()))
            || (ordinary
                && !editor.crop.is_some_and(|crop| {
                    crop.valid(editor.canvas.width, editor.canvas.height)
                        && crop.contains(point.rounded())
                }))
        {
            return None;
        }
        // 已选对象的控制点优先于背后的正文命中。
        if editor
            .selected_annotation()
            .is_some_and(|a| a.handles().iter().any(|h| h.distance(point) <= tolerance))
        {
            return None;
        }
        let group_scope = editor.group_scope_path();
        let object = editor
            .selected
            .and_then(|id| {
                editor.document.items().iter().find(|o| {
                    o.id == id
                        && o.in_group_scope(&group_scope)
                        && text_core_hit(&o.annotation, point, tolerance, scale)
                })
            })
            .or_else(|| {
                editor.document.items().iter().rev().find(|o| {
                    o.in_group_scope(&group_scope)
                        && o.annotation.tool == Tool::Text
                        && o.annotation.hit(point, tolerance, true)
                })
            })?;
        if editor.expand_selection(&[object.id]).len() > 1
            || !text_core_hit(&object.annotation, point, tolerance, scale)
        {
            return None;
        }
        Some(Self {
            original: object.annotation.clone(),
            point,
            id: object.id,
            crop: editor.crop,
            revision: editor.document.revision(),
            group_scope,
            group_scope_generation: editor.group_scope_generation(),
            dragged: false,
            capture_owner: None,
        })
    }
    pub(crate) fn accept(&self, editor: &mut Editor) -> bool {
        self.source_matches(editor) && editor.select_text_core(self.id)
    }
    fn source_matches(&self, editor: &Editor) -> bool {
        editor.tool == Tool::Text
            && editor.crop == self.crop
            && editor.document.revision() == self.revision
            && editor.group_scope_path() == self.group_scope
            && editor.group_scope_generation() == self.group_scope_generation
            && editor.object_in_group_scope(self.id)
            && editor.document.get(self.id) == Some(&self.original)
            && !editor.is_dragging()
            && !editor.has_pending_drawing()
            && editor.lasso_draft.is_none()
            && !editor.watermark.has_preview()
            && editor.local_fill_candidate().is_none()
    }
    pub(crate) fn valid(&self, editor: &Editor) -> bool {
        self.source_matches(editor)
            && editor.selected == Some(self.id)
            && editor.selection.is_empty()
            && editor
                .text_draft
                .as_ref()
                .is_none_or(|d| d.id == Some(self.id) && d.annotation == self.original)
    }
    pub(crate) fn owns_pointer(&self, down: Option<(i32, i32)>) -> bool {
        down == Some(self.point.rounded())
    }
    pub(crate) fn observe(&mut self, point: Point, threshold: f32) {
        // 远移再返回依然属于选字；不能改原 press 或转成对象移动。
        self.dragged |= (point.x - self.point.x).abs() >= threshold
            || (point.y - self.point.y).abs() >= threshold;
    }
    pub(crate) fn select(
        &self,
        editor: &mut Editor,
        input: &mut TextInputState,
        anchor: usize,
        caret: usize,
    ) -> bool {
        if !self.valid(editor)
            || input.marked.is_some()
            || !self.original.text.is_char_boundary(anchor)
            || !self.original.text.is_char_boundary(caret)
        {
            return false;
        }
        if editor.text_draft.is_none() {
            editor.begin_text(Some(self.id), self.point);
        }
        input.select_to(
            crate::text_input::edit_boundary::floor(&self.original.text, anchor),
            crate::text_input::edit_boundary::floor(&self.original.text, caret),
        );
        input.drag_anchor = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Style;

    #[test]
    fn text_core_user_selection_keeps_full_graphemes_and_original_object_ownership() {
        let mut editor = Editor::new(Rect::full(800, 600), Some(Rect::full(800, 600)));
        let mut annotation =
            Annotation::new(Tool::Text, Point { x: 100., y: 100. }, Style::default());
        annotation.points.push(Point { x: 340., y: 180. });
        let text = "甲👨‍👩‍👧‍👦🇨🇳e\u{301}乙";
        annotation.text = text.into();
        let id = editor.document.insert(annotation);
        editor.select_tool(Tool::Text);
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let press = admitted(&mut editor);
        let mut input = TextInputState::default();
        let first = "甲".len();
        let last = first + "👨‍👩‍👧‍👦".len();
        assert!(press.select(
            &mut editor,
            &mut input,
            first + "👨".len(),
            last + "🇨".len()
        ));
        assert_eq!(input.selection, first..last);
        assert_eq!(editor.text_draft.as_ref().unwrap().id, Some(id));
        assert_eq!(editor.text_draft.as_ref().unwrap().annotation.text, text);
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        let selection = input.selection.clone();
        input.marked = Some(first..last);
        assert!(!press.select(&mut editor, &mut input, 0, text.len()));
        assert_eq!(input.selection, selection);
        input.marked = None;
        assert!(select_visual_line(
            &mut input,
            text,
            first + "👨".len()..last + "🇨".len()
        ));
        assert_eq!(input.selection, first..last + "🇨🇳".len());
        assert_eq!(editor.document.revision(), revision);
        assert!(!select_visual_line(&mut input, text, first + 1..text.len()));
    }

    fn fixture(crop: Option<Rect>) -> (Editor, ObjectId) {
        let mut editor = Editor::new(Rect::full(800, 600), crop);
        let mut text = Annotation::new(Tool::Text, Point { x: 100., y: 100. }, Style::default());
        text.points.push(Point { x: 340., y: 180. });
        text.text = "中🙂abcdefgh".into();
        let id = editor.document.insert(text);
        editor.select_tool(Tool::Text);
        (editor, id)
    }
    fn point() -> Point {
        Point { x: 160., y: 130. }
    }
    fn admitted(editor: &mut Editor) -> TextCorePress {
        let press = TextCorePress::candidate(editor, point(), 6., 1., true).unwrap();
        assert!(press.accept(editor));
        press
    }
    #[test]
    fn first_core_press_waits_for_real_up_and_enters_original_caret_without_document_command() {
        let (mut editor, id) = fixture(Some(Rect::full(800, 600)));
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let mut press = admitted(&mut editor);
        assert_eq!(editor.selected, Some(id));
        assert!(!editor.is_dragging());
        assert!(editor.text_draft.is_none());
        press.observe(point(), 3.);
        assert!(!press.dragged);
        let mut input = TextInputState::default();
        assert!(press.select(&mut editor, &mut input, 7, 7));
        assert_eq!(input.selection, 7..7);
        assert_eq!(editor.text_draft.as_ref().unwrap().id, Some(id));
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.finish_text();
        assert_eq!(editor.document.revision(), revision);
        editor.document.undo();
        assert!(editor.document.items().is_empty()); // 原 insert 仍是唯一命令。
        editor.document.redo();
        assert_eq!(editor.document.items(), items);
        let (pin, _) = fixture(None);
        assert!(TextCorePress::candidate(&pin, point(), 6., 1., false).is_some());
        assert!(TextCorePress::candidate(&pin, point(), 6., 1., true).is_none());
    }
    #[test]
    fn core_drag_selects_from_press_and_far_return_never_moves_or_adds_history() {
        let (mut editor, _) = fixture(Some(Rect::full(800, 600)));
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let mut press = admitted(&mut editor);
        let mut input = TextInputState::default();
        press.observe(Point { x: 200., y: 130. }, 3.);
        assert!(press.dragged);
        assert!(press.select(&mut editor, &mut input, 3, 12));
        assert_eq!(input.selection, 3..12);
        press.observe(point(), 3.);
        assert!(press.dragged);
        assert_eq!(press.point, point());
        assert!(press.select(&mut editor, &mut input, 3, 3));
        assert_eq!(input.selection, 3..3);
        assert!(!editor.is_dragging());
        assert_eq!(
            editor.text_draft.as_ref().unwrap().annotation,
            items[0].annotation
        );
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        editor.finish_text();
        assert_eq!(editor.document.revision(), revision);
    }
    #[test]
    fn original_source_tool_crop_replacement_cancel_and_marked_input_cannot_resume_old_press() {
        let (mut editor, id) = fixture(Some(Rect::full(800, 600)));
        let press = admitted(&mut editor);
        let revision = editor.document.revision();
        assert!(press.owns_pointer(Some(point().rounded())));
        assert!(!press.owns_pointer(None)); // Pin seal 清 Down，恢复旧 Entity 也不能补原按下。
        assert!(!press.owns_pointer(Some((500, 300))));
        let mut input = TextInputState {
            marked: Some(0..3),
            ..Default::default()
        };
        assert!(!press.select(&mut editor, &mut input, 0, 3));
        assert!(editor.text_draft.is_none());
        assert_eq!(input.marked, Some(0..3));
        input.marked = None;
        editor.tool = Tool::Edit;
        assert!(!press.select(&mut editor, &mut input, 0, 3));
        editor.tool = Tool::Text;
        editor.crop = None;
        assert!(!press.valid(&editor));
        editor.crop = Some(Rect::full(800, 600));
        let mut replacement = editor.document.get(id).unwrap().clone();
        replacement.text.push('新');
        editor.document.replace(id, replacement);
        assert!(!press.select(&mut editor, &mut input, 0, 3));
        editor.document.undo();
        assert_eq!(editor.document.get(id), Some(&press.original));
        assert!(!press.valid(&editor)); // undo ABA 也不能重认旧 press。
        assert!(editor.document.revision() > revision);
        let (mut other, _) = fixture(Some(Rect::full(800, 600)));
        let pending = Some(admitted(&mut other));
        drop(pending); // cancel 仅撤本次输入，不开始草稿或提交。
        assert!(other.text_draft.is_none());
        assert!(!other.is_dragging());

        let (mut finished, _) = fixture(Some(Rect::full(800, 600)));
        let mut press = admitted(&mut finished);
        let revision = finished.document.revision();
        let items = finished.document.items().to_vec();
        press.observe(Point { x: 200., y: 130. }, 3.);
        assert!(press.select(&mut finished, &mut input, 3, 12));
        assert!(finished.text_draft.is_some());
        let mut pending = Some(press);
        let mut down = Some(point().rounded());
        input.marked = Some(0..3);
        assert!(!terminate_text_core_press(&mut pending, &mut down, &input));
        assert!(pending.as_ref().unwrap().owns_pointer(down));
        assert!(finished.text_draft.is_some()); // IME 拒绝收尾时保留草稿和原输入归属。
        input.marked = None;
        assert!(terminate_text_core_press(&mut pending, &mut down, &input));
        finished.finish_text();
        assert_eq!(finished.document.revision(), revision); // 本次结束确实是 no-op。
        assert_eq!(finished.document.items(), items);
        assert!(down.is_none());
        assert!(pending.is_none());
        if let Some(press) = pending
            .as_ref()
            .filter(|press| press.valid(&finished) && press.owns_pointer(down))
        {
            press.select(&mut finished, &mut input, 3, 3);
        }
        assert!(finished.text_draft.is_none()); // Enter/Esc/右键成功结束后的迟到 Up 无法重开。
        let mut unrelated_down = Some((500, 300));
        assert!(!terminate_text_core_press(
            &mut None,
            &mut unrelated_down,
            &input
        ));
        assert_eq!(unrelated_down, Some((500, 300))); // 无 CorePress 不撤其它手势。
    }
    #[test]
    fn core_bounds_handles_rotation_and_other_editor_routes_keep_geometric_gestures() {
        let (mut editor, id) = fixture(Some(Rect::full(800, 600)));
        let a = editor.document.get(id).unwrap();
        assert!(text_core_hit(a, point(), 6., 1.));
        assert!(!text_core_hit(a, Point { x: 101., y: 130. }, 6., 1.));
        assert!(!text_core_hit(a, a.handles()[0], 6., 1.));
        let mut rotated = a.clone();
        rotated.rotation = 45.;
        assert!(text_core_hit(&rotated, rotated.to_world(point()), 6., 1.));
        assert!(!text_core_hit(
            &rotated,
            rotated.to_world(Point { x: 101., y: 130. }),
            6.,
            1.
        ));
        editor.tool = Tool::Edit;
        assert!(TextCorePress::candidate(&editor, point(), 6., 1., true).is_none());
        let before = editor.document.get(id).unwrap().clone();
        editor.pointer_down_at(point(), 6., false);
        assert!(editor.is_dragging());
        editor.pointer_up_at(Point { x: 170., y: 140. });
        assert_ne!(editor.document.get(id), Some(&before));
        editor.document.undo();
        assert_eq!(editor.document.get(id), Some(&before));
        editor.tool = Tool::Text;
        assert!(
            TextCorePress::candidate(&editor, Point { x: 500., y: 300. }, 6., 1., true).is_none()
        );
        editor.pointer_down_at(Point { x: 500., y: 300. }, 6., false);
        assert!(editor.text_draft.as_ref().unwrap().id.is_none());

        let (mut grouped, first) = fixture(Some(Rect::full(800, 600)));
        let mut second = grouped.document.get(first).unwrap().clone();
        second.translate(300., 0.);
        let second = grouped.document.insert(second);
        assert!(grouped.document.set_group(&[first, second], true));
        let items = grouped.document.items().to_vec();
        let revision = grouped.document.revision();
        // 未选中的已分组文字首击沿 Editor 原 expand_groups 路线。
        assert!(grouped.selected.is_none());
        assert!(TextCorePress::candidate(&grouped, point(), 6., 1., true).is_none());
        grouped.pointer_down_at(point(), 6., false);
        assert!(grouped.is_dragging());
        assert_eq!(grouped.selected_ids(), vec![first, second]);
        grouped.pointer_up_at(point());
        assert!(grouped.text_draft.is_none());
        assert_eq!(grouped.document.items(), items);
        assert_eq!(grouped.document.revision(), revision);
        // 既有多选正文拖动仍一次移动整组，随后角柄仍缩放整组。
        assert!(TextCorePress::candidate(&grouped, point(), 6., 1., true).is_none());
        grouped.pointer_down_at(point(), 6., false);
        grouped.pointer_up_at(Point { x: 170., y: 140. });
        for before in &items {
            let mut expected = before.annotation.clone();
            expected.translate(10., 10.);
            assert_eq!(grouped.document.get(before.id), Some(&expected));
        }
        grouped.document.undo();
        assert_eq!(grouped.document.items(), items);
        let handle = grouped.selection_bounds().unwrap().handles()[0];
        assert!(TextCorePress::candidate(&grouped, handle, 6., 1., true).is_none());
        grouped.pointer_down_at(handle, 6., false);
        assert!(grouped.is_dragging());
        grouped.pointer_up_at(Point {
            x: handle.x - 10.,
            y: handle.y - 10.,
        });
        assert_ne!(grouped.document.items(), items);
        assert!(grouped.text_draft.is_none());
        grouped.document.undo();
        assert_eq!(grouped.document.items(), items);

        // View 唯一生产 caller 的 !ordinary 仅属于已通过原入口门的 Pin / Image。
        // crop=None 的既有外部正文走首击/拖选；普通截图或仍有 crop 保持原边界。
        for dragged in [false, true] {
            let (mut outside, id) = fixture(None);
            let mut text = outside.document.get(id).unwrap().clone();
            text.translate(-200., 0.);
            outside.document.replace(id, text);
            let point = Point { x: -40., y: 130. };
            assert!(!outside.canvas.contains(point.rounded()));
            assert!(text_core_hit(
                outside.document.get(id).unwrap(),
                point,
                6.,
                1.
            ));
            assert!(TextCorePress::candidate(&outside, point, 6., 1., true).is_none());
            outside.crop = Some(Rect::full(800, 600));
            assert!(TextCorePress::candidate(&outside, point, 6., 1., true).is_none());
            assert!(TextCorePress::candidate(&outside, point, 6., 1., false).is_none());
            outside.crop = None;
            for point in [
                Point {
                    x: f32::NAN,
                    y: 130.,
                },
                Point {
                    x: -40.,
                    y: f32::INFINITY,
                },
            ] {
                assert!(TextCorePress::candidate(&outside, point, 6., 1., false).is_none());
            }
            let before = outside.document.items().to_vec();
            let revision = outside.document.revision();
            let mut press = TextCorePress::candidate(&outside, point, 6., 1., false).unwrap();
            assert!(press.accept(&mut outside));
            assert!(!outside.is_dragging());
            assert!(outside.text_draft.is_none());
            assert!(press.owns_pointer(Some(point.rounded())));
            let mut input = TextInputState::default();
            if dragged {
                press.observe(Point { x: -20., y: 135. }, 3.);
                assert!(press.dragged);
                assert!(press.select(&mut outside, &mut input, 3, 12));
                assert_eq!(input.selection, 3..12);
            } else {
                press.observe(point, 3.);
                assert!(!press.dragged);
                assert!(press.select(&mut outside, &mut input, 7, 7));
                assert_eq!(input.selection, 7..7);
            }
            assert_eq!(outside.text_draft.as_ref().unwrap().id, Some(id));
            assert_eq!(outside.document.items(), before);
            assert_eq!(outside.document.revision(), revision);
            assert!(!outside.is_dragging());
            outside.finish_text();
            assert_eq!(outside.document.items(), before);
            assert_eq!(outside.document.revision(), revision);
            assert!(outside.crop.is_none());
        }
    }

    #[test]
    fn core_press_in_group_scope_filters_outer_text_and_rejects_navigation_aba() {
        let (mut editor, id) = fixture(Some(Rect::full(800, 600)));
        let mut rectangle = Annotation::new(
            Tool::Rectangle,
            Point { x: 400., y: 250. },
            Style::default(),
        );
        rectangle.update(Point { x: 500., y: 350. });
        let sibling = editor.document.insert(rectangle);
        assert!(editor.document.set_group(&[id, sibling], true));
        let mut outside = editor.document.get(id).unwrap().clone();
        outside.translate(450., 0.);
        outside.text = "组合外正文".into();
        let outside_id = editor.document.insert(outside);
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        assert!(editor.enter_group_scope());
        editor.select_tool(Tool::Text);
        let scope = editor.group_scope_path();
        let revision = editor.document.revision();
        assert!(
            TextCorePress::candidate(&editor, Point { x: 610., y: 130. }, 6., 1., true).is_none()
        );
        assert!(!editor.select_text_core(outside_id));
        let press = TextCorePress::candidate(&editor, point(), 6., 1., true).unwrap();
        assert!(press.accept(&mut editor));
        assert_eq!(editor.selected, Some(id));
        assert!(press.valid(&editor));
        assert!(editor.exit_group_scope());
        assert!(!press.accept(&mut editor));
        assert!(editor.enter_group_scope());
        editor.select_tool(Tool::Text);
        assert_eq!(editor.group_scope_path(), scope);
        assert_eq!(editor.document.revision(), revision);
        assert!(
            !press.accept(&mut editor),
            "同一路径导航ABA也不能重认迟到press"
        );
        assert!(!press.valid(&editor));
        assert!(editor.text_draft.is_none());
        let current = TextCorePress::candidate(&editor, point(), 6., 1., true).unwrap();
        assert!(current.accept(&mut editor));
        let mut input = TextInputState::default();
        assert!(current.select(&mut editor, &mut input, 3, 7));
        assert_eq!(editor.text_draft.as_ref().unwrap().id, Some(id));
        assert_eq!(editor.document.revision(), revision);
    }
}
