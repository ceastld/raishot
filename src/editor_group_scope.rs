//! 瞬态组合导航；Scene／导出仍使用完整扁平文档，导航不写历史。
use super::Editor;
use crate::{
    annotation::{Annotation, Point, Tool},
    document::ObjectId,
};

impl Editor {
    pub(crate) fn group_scope_path(&self) -> Vec<u64> {
        self.document.valid_group_scope(&self.group_scope)
    }
    pub(crate) fn group_scope_depth(&self) -> usize {
        self.group_scope_path().len()
    }
    pub(crate) fn group_scope_generation(&self) -> u64 {
        self.group_scope_generation
    }
    pub(crate) fn object_in_group_scope(&self, id: ObjectId) -> bool {
        self.document.in_group_scope(id, &self.group_scope_path())
    }
    pub(crate) fn expand_selection(&self, ids: &[ObjectId]) -> Vec<ObjectId> {
        let scope = self.group_scope_path();
        if scope.is_empty() {
            self.document.expand_groups(ids)
        } else {
            self.document.expand_groups_in_scope(ids, &scope)
        }
    }
    pub(crate) fn grouped_in_current_scope(&self, id: ObjectId) -> bool {
        let scope = self.group_scope_path();
        if scope.is_empty() {
            self.document.grouped(id)
        } else {
            self.document.child_group(id, &scope).is_some()
        }
    }
    pub(crate) fn selected_group(&self) -> Option<u64> {
        let ids = self.selected_ids();
        let scope = self.group_scope_path();
        let group = self.document.child_group(*ids.first()?, &scope)?;
        ids.iter()
            .all(|id| self.document.child_group(*id, &scope) == Some(group))
            .then_some(group)
    }
    fn group_navigation_ready(&self) -> bool {
        self.gesture.is_none()
            && self.text_draft.is_none()
            && self.lasso_draft.is_none()
            && !self.watermark.has_preview()
    }
    pub(crate) fn can_enter_group_scope(&self) -> bool {
        self.group_navigation_ready() && self.selected_group().is_some()
    }
    pub(crate) fn can_group_selection(&self) -> bool {
        self.group_navigation_ready()
            && self
                .document
                .can_group_in_scope(&self.selected_ids(), &self.group_scope_path())
    }
    pub(crate) fn enter_group_scope(&mut self) -> bool {
        if !self.group_navigation_ready() {
            return false;
        }
        let Some(group) = self.selected_group() else {
            return false;
        };
        let preferred = self.selected;
        self.enter_selection_mode();
        let mut scope = self.group_scope_path();
        scope.push(group);
        self.group_scope = scope;
        self.group_scope_generation = self.group_scope_generation.wrapping_add(1);
        self.selection.clear();
        self.selected_node = None;
        self.selected = None;
        // 选择一个当前子单位，便于看到进入后真正可编辑的子组选框。
        // 不把所有子单位默认全选，否则下一次点击会继续移动整层。
        let target = preferred
            .filter(|id| self.object_in_group_scope(*id))
            .or_else(|| {
                self.document
                    .items()
                    .iter()
                    .find(|object| object.in_group_scope(&self.group_scope))
                    .map(|object| object.id)
            });
        if let Some(id) = target {
            self.select_group_child(id);
        }
        self.preview_revision += 1;
        self.sync_primary_selection_style();
        true
    }
    pub(crate) fn enter_group_scope_at(&mut self, point: Point, tolerance: f32) -> bool {
        if self.tool != Tool::Edit || !self.can_enter_group_scope() {
            return false;
        }
        let ids = self.selected_ids();
        let hit = self.document.items().iter().rev().find(|object| {
            ids.contains(&object.id) && object.annotation.hit(point, tolerance, true)
        });
        let Some(hit) = hit else {
            return false;
        };
        // 正文／视觉行、箭头和路径节点已有双击合同；工具栏仍能进入这种组。
        if hit.annotation.tool.text() || matches!(hit.annotation.tool, Tool::Arrow | Tool::Polyline)
        {
            return false;
        }
        let id = hit.id;
        if !self.enter_group_scope() {
            return false;
        }
        self.select_group_child(id);
        self.sync_primary_selection_style();
        true
    }
    fn select_group_child(&mut self, id: ObjectId) {
        self.selection = self.expand_selection(&[id]);
        self.selected = self.selection.contains(&id).then_some(id);
        self.selected_node = None;
    }
    pub(crate) fn exit_group_scope(&mut self) -> bool {
        if !self.group_navigation_ready() {
            return false;
        }
        let scope = self.group_scope_path();
        if scope.is_empty() {
            return false;
        }
        let preferred = self.selected;
        let ids: Vec<_> = self
            .document
            .items()
            .iter()
            .filter(|object| object.in_group_scope(&scope))
            .map(|object| object.id)
            .collect();
        self.enter_selection_mode();
        self.group_scope = scope[..scope.len() - 1].to_vec();
        self.group_scope_generation = self.group_scope_generation.wrapping_add(1);
        self.selection = self.expand_selection(&ids);
        self.selected = preferred
            .filter(|id| self.selection.contains(id))
            .or_else(|| self.selection.last().copied());
        self.selected_node = None;
        self.preview_revision += 1;
        self.sync_primary_selection_style();
        true
    }
    pub(crate) fn exit_all_group_scopes(&mut self) -> bool {
        let mut changed = false;
        while self.exit_group_scope() {
            changed = true;
        }
        changed
    }
    /// 范围、外部换图及管理型生成已结束自身编辑归属后显式调用。
    pub(crate) fn reset_group_scope(&mut self) -> bool {
        if self.group_scope.is_empty() {
            return false;
        }
        self.group_scope.clear();
        self.group_scope_generation = self.group_scope_generation.wrapping_add(1);
        self.selection.clear();
        self.selected = None;
        self.selected_node = None;
        self.preview_revision += 1;
        true
    }
    pub(crate) fn repair_group_scope(&mut self) {
        let valid = self.group_scope_path();
        if valid != self.group_scope {
            self.group_scope = valid;
            self.group_scope_generation = self.group_scope_generation.wrapping_add(1);
            self.selection.clear();
            self.selected = None;
            self.selected_node = None;
            self.preview_revision += 1;
        }
        if self
            .selected
            .is_some_and(|id| !self.object_in_group_scope(id))
        {
            self.selected = None;
            self.selected_node = None;
        }
        let scope = self.group_scope_path();
        self.selection
            .retain(|id| self.document.in_group_scope(*id, &scope));
    }
    pub(crate) fn insert_annotation_in_group_scope(&mut self, annotation: Annotation) -> ObjectId {
        let scope = self.group_scope_path();
        self.document.insert_in_group_scope(annotation, &scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::Style,
        geometry::Rect,
        scene::{PixelPlane, Scene, SourcePixels},
    };

    fn rectangle(x: f32) -> Annotation {
        let mut a = Annotation::new(Tool::Rectangle, Point { x, y: 40. }, Style::default());
        a.update(Point { x: x + 30., y: 70. });
        a
    }
    fn nested() -> (Editor, [ObjectId; 4], u64, u64) {
        let mut e = Editor::new(Rect::full(400, 300), Some(Rect::full(400, 300)));
        let a = e.document.insert(rectangle(30.));
        let b = e.document.insert(rectangle(110.));
        let c = e.document.insert(rectangle(210.));
        let outside = e.document.insert(rectangle(30.));
        assert!(e.document.set_group(&[a, b], true));
        let child = e.document.items()[0].group.unwrap();
        assert!(e.document.set_group(&[a, c], true));
        let root = e.document.items()[0].group.unwrap();
        e.select_tool(Tool::Edit);
        e.selected = Some(a);
        e.selection = e.expand_selection(&[a]);
        (e, [a, b, c, outside], root, child)
    }
    fn choose(e: &mut Editor, id: ObjectId) {
        e.selected = Some(id);
        e.selection = e.expand_selection(&[id]);
    }
    fn checkpoint(e: &Editor) -> (u64, usize) {
        (
            e.document.edit_revision(),
            e.document.bounded_history_snapshot().unwrap().undo.len(),
        )
    }
    fn assert_one_command(e: &Editor, before: (u64, usize)) {
        assert_eq!(e.document.edit_revision(), before.0 + 1);
        assert_eq!(
            e.document.bounded_history_snapshot().unwrap().undo.len(),
            before.1 + 1
        );
    }
    #[test]
    fn nested_scope_navigation_keeps_primary_presets_history_and_detects_same_path_aba() {
        let (mut e, ids, root, child) = nested();
        e.select_tool(Tool::Pen);
        e.set_width(13.);
        choose(&mut e, ids[0]);
        let removed = e.document.insert(rectangle(320.));
        e.document.undo();
        let before = e.document.bounded_history_snapshot().unwrap();
        let revisions = (e.document.revision(), e.document.edit_revision());
        assert!(e.enter_group_scope());
        assert_eq!(e.tool, Tool::Edit);
        assert_eq!(e.group_scope_path(), vec![root]);
        assert_eq!(e.selected, Some(ids[0]));
        assert_eq!(e.selected_ids(), ids[..2]);
        assert!(e.enter_group_scope());
        assert_eq!(e.group_scope_path(), vec![root, child]);
        assert_eq!(e.selected_ids(), vec![ids[0]]);
        let epoch = e.group_scope_generation();
        assert!(e.exit_group_scope());
        assert_eq!(e.selected_ids(), ids[..2]);
        assert!(e.enter_group_scope());
        assert_eq!(e.group_scope_path(), vec![root, child]);
        assert_ne!(e.group_scope_generation(), epoch);
        assert!(e.exit_all_group_scopes());
        assert!(e.group_scope_path().is_empty());
        assert_eq!(e.selected_ids(), ids[..3]);
        assert!(!e.exit_group_scope());
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(
            (e.document.revision(), e.document.edit_revision()),
            revisions
        );
        assert!(e.document.can_redo());
        e.select_tool(Tool::Pen);
        assert_eq!(e.style.width, 13.);
        e.document.redo();
        assert!(e.document.get(removed).is_some());
    }
    #[test]
    fn nested_scope_true_point_hits_select_children_and_ignore_outside_overlap_and_ctrl_a() {
        let (mut e, ids, root, child) = nested();
        let mut first = e.document.get(ids[0]).unwrap().clone();
        first.style.color = 0xff4b5c;
        let first_style = first.style;
        let mut second = e.document.get(ids[1]).unwrap().clone();
        second.style.color = 0x378de5;
        e.document
            .replace_many(&[(ids[0], first), (ids[1], second)]);
        let presets = e.tool_styles.clone();
        assert!(e.enter_group_scope());
        e.selected = None;
        e.selection.clear();
        let revision = e.document.revision();
        e.pointer_down((45, 55), 3.);
        assert_eq!(e.selected, Some(ids[0]), "开始整组移动也保留真实命中的主选");
        assert_eq!(e.style, first_style);
        e.pointer_move((45, 55));
        assert_eq!(e.style, first_style);
        e.pointer_up((45, 55));
        assert_eq!(e.selected, Some(ids[0]));
        assert_eq!(e.selected_ids(), ids[..2]);
        assert_eq!(e.document.revision(), revision);
        assert_eq!(e.style, first_style);
        assert_eq!(e.tool_styles, presets);
        e.select_all_objects();
        assert_eq!(e.selected_ids(), ids[..3]);
        choose(&mut e, ids[0]);
        assert!(e.enter_group_scope());
        assert_eq!(e.group_scope_path(), vec![root, child]);
        e.select_all_objects();
        assert_eq!(e.selected_ids(), ids[..2]);
        e.selected = None;
        e.selection.clear();
        e.pointer_down((225, 55), 3.);
        e.pointer_up((225, 55));
        assert!(e.selected_ids().is_empty());
        e.pointer_down((45, 55), 3.);
        e.pointer_up((45, 55));
        assert_eq!(e.selected_ids(), vec![ids[0]]);
        assert_eq!(e.document.get(ids[3]), Some(&rectangle(30.)));
    }
    #[test]
    fn nested_subgroup_real_move_rotate_and_uniform_scale_commit_once_and_keep_other_units() {
        let (mut e, ids, root, child) = nested();
        let mut effect = e.document.get(ids[0]).unwrap().clone();
        effect.tool = Tool::Mosaic;
        effect.style.effect_radius = 6.;
        effect.style.radius = 9.;
        e.document.replace(ids[0], effect);
        assert!(e.enter_group_scope());
        let initial = e.document.items().to_vec();
        let before_command = checkpoint(&e);
        e.pointer_down((45, 55), 3.);
        e.pointer_move((65, 65));
        assert_eq!(e.document.items(), initial);
        e.pointer_up((65, 65));
        assert_one_command(&e, before_command);
        for index in 0..2 {
            let mut expected = initial[index].annotation.clone();
            expected.translate(20., 10.);
            assert_eq!(e.document.get(ids[index]), Some(&expected));
        }
        assert_eq!(&e.document.items()[2..], &initial[2..]);
        e.undo();
        assert_eq!(e.document.items(), initial);
        choose(&mut e, ids[0]);
        let before_command = checkpoint(&e);
        let control = e.rotation_control(3.).unwrap();
        let vector = Point {
            x: control.position.x - control.pivot.x,
            y: control.position.y - control.pivot.y,
        };
        let target = Point {
            x: control.pivot.x - vector.y,
            y: control.pivot.y + vector.x,
        };
        e.pointer_down_at(control.position, 3., false);
        e.pointer_move_at(target);
        e.pointer_up_at(target);
        assert_one_command(&e, before_command);
        for id in &ids[..2] {
            assert!((e.document.get(*id).unwrap().rotation - 90.).abs() < 0.001);
        }
        assert_eq!(&e.document.items()[2..], &initial[2..]);
        e.undo();
        choose(&mut e, ids[0]);
        let bounds = e.selection_bounds().unwrap();
        let start = bounds.handles()[7];
        let anchor = bounds.handles()[0];
        let target = Point {
            x: anchor.x + (start.x - anchor.x) * 2.,
            y: anchor.y + (start.y - anchor.y) * 2.,
        };
        let before_command = checkpoint(&e);
        e.pointer_down_at(start, 3., false);
        e.pointer_move_at(target);
        e.pointer_up_at(target);
        assert_one_command(&e, before_command);
        assert_eq!(e.document.get(ids[0]).unwrap().style.effect_radius, 12.);
        assert_eq!(e.document.get(ids[0]).unwrap().style.radius, 18.);
        for index in 0..2 {
            let mut expected = initial[index].annotation.clone();
            expected.scale_about(anchor, 2.);
            assert_eq!(e.document.get(ids[index]), Some(&expected));
            assert_eq!(e.document.items()[index].group_path(), vec![root, child]);
        }
        assert_eq!(&e.document.items()[2..], &initial[2..]);
        e.undo();
        assert_eq!(e.document.items(), initial);
        e.redo();
        assert_eq!(e.document.get(ids[0]).unwrap().style.effect_radius, 12.);
    }
    #[test]
    fn nested_scope_cancel_noop_click_and_group_attempt_preserve_redo() {
        let (mut e, ids, _, _) = nested();
        assert!(e.enter_group_scope());
        let extra = e.document.insert(rectangle(320.));
        e.document.undo();
        let before = e.document.bounded_history_snapshot().unwrap();
        let revisions = (e.document.revision(), e.document.edit_revision());
        e.pointer_down((45, 55), 3.);
        e.pointer_move((85, 85));
        assert!(e.cancel_gesture());
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        e.pointer_down((45, 55), 3.);
        e.pointer_up((45, 55));
        choose(&mut e, ids[0]);
        assert!(!e.can_group_selection());
        assert!(!e.group_selection(true));
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(
            (e.document.revision(), e.document.edit_revision()),
            revisions
        );
        e.document.redo();
        assert!(e.document.get(extra).is_some());
    }
    #[test]
    fn nested_scope_spotlight_insert_and_shared_layer_replacements_are_one_command() {
        for has_existing in [false, true] {
            let (mut e, ids, root, child) = nested();
            if has_existing {
                let mut outside = e.document.get(ids[3]).unwrap().clone();
                outside.tool = Tool::Spotlight;
                e.document.replace(ids[3], outside);
            }
            assert!(e.enter_group_scope());
            assert!(e.enter_group_scope());
            e.select_tool(Tool::Spotlight);
            e.style.color = 0x123456;
            let before = e.document.items().to_vec();
            let before_command = checkpoint(&e);
            e.pointer_down((280, 150), 3.);
            e.pointer_move((320, 190));
            e.pointer_up((320, 190));
            let fresh = e.selected.unwrap();
            assert_eq!(e.selected_ids(), vec![fresh]);
            assert_one_command(&e, before_command);
            assert_eq!(e.document.items().len(), before.len() + 1);
            let created = e.document.items().iter().find(|o| o.id == fresh).unwrap();
            assert_eq!(created.group_path(), vec![root, child]);
            assert_eq!(created.annotation.tool, Tool::Spotlight);
            if has_existing {
                assert_eq!(e.document.items().last().unwrap().id, fresh);
                assert_eq!(e.document.get(ids[3]).unwrap().style.color, 0x123456);
                assert_eq!(e.document.items()[3].group_path(), Vec::<u64>::new());
            } else {
                assert_eq!(e.document.items()[0].id, fresh);
            }
            let after = e.document.items().to_vec();
            e.undo();
            assert_eq!(e.document.items(), before);
            assert_eq!(e.group_scope_path(), vec![root, child]);
            e.redo();
            assert_eq!(e.document.items(), after);
            assert!(e.document.bounded_history_snapshot().is_ok());
        }
    }
    #[test]
    fn nested_scope_new_drawing_delete_and_clear_are_incremental_and_repair_after_undo() {
        let (mut e, ids, root, child) = nested();
        assert!(e.enter_group_scope());
        assert!(e.enter_group_scope());
        e.select_tool(Tool::Pen);
        let before_command = checkpoint(&e);
        e.pointer_down((300, 160), 3.);
        e.pointer_move((330, 180));
        e.pointer_up((330, 180));
        assert_one_command(&e, before_command);
        let fresh = e.document.items().last().unwrap().id;
        assert_eq!(
            e.document.items().last().unwrap().group_path(),
            vec![root, child]
        );
        e.undo();
        assert!(e.document.get(fresh).is_none());
        assert_eq!(e.group_scope_path(), vec![root, child]);
        e.redo();
        choose(&mut e, fresh);
        e.delete_selected();
        assert!(e.document.get(fresh).is_none());
        e.undo();
        assert!(e.document.get(fresh).is_some());
        let before_command = checkpoint(&e);
        e.clear();
        assert_one_command(&e, before_command);
        assert_eq!(e.group_scope_path(), vec![root]);
        assert_eq!(
            e.document.items().iter().map(|o| o.id).collect::<Vec<_>>(),
            ids[2..]
        );
        e.undo();
        assert_eq!(e.document.items().len(), 5);
        assert_eq!(e.group_scope_path(), vec![root]);
        choose(&mut e, ids[0]);
        assert!(e.enter_group_scope());
        e.undo(); // 撤销原来新增的 scoped Pen，层级仍存在。
        assert_eq!(e.group_scope_path(), vec![root, child]);
    }
    #[test]
    fn nested_scope_protects_text_and_path_double_clicks_and_global_tools_reset_without_edits() {
        for tool in [Tool::Text, Tool::Arrow, Tool::Polyline] {
            let (mut e, ids, root, _) = nested();
            let mut protected = e.document.get(ids[0]).unwrap().clone();
            protected.tool = tool;
            if tool == Tool::Text {
                protected.text = "组合正文".into();
            }
            e.document.replace(ids[0], protected.clone());
            choose(&mut e, ids[0]);
            let p = Point { x: 45., y: 55. };
            assert!(protected.hit(p, 4., true));
            assert!(!e.enter_group_scope_at(p, 4.));
            assert!(e.group_scope_path().is_empty());
            assert!(e.enter_group_scope()); // 工具栏仍允许进入文字／路径组成的组。
            assert_eq!(e.group_scope_path(), vec![root]);
            let before = e.document.bounded_history_snapshot().unwrap();
            e.select_tool(Tool::Crop);
            assert!(e.group_scope_path().is_empty());
            assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        }
        for tool in [Tool::Crop, Tool::Lasso, Tool::SmartMosaic, Tool::Watermark] {
            let (mut e, _, _, _) = nested();
            assert!(e.enter_group_scope());
            let before = e.document.bounded_history_snapshot().unwrap();
            e.select_tool(tool);
            assert!(e.group_scope_path().is_empty());
            assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        }
    }
    #[test]
    fn nested_scope_text_edit_rejects_outside_and_last_empty_text_repairs_current_layer() {
        let (mut e, ids, root, child) = nested();
        let mut text = e.document.get(ids[0]).unwrap().clone();
        text.tool = Tool::Text;
        text.text = "原文".into();
        e.document.replace(ids[0], text);
        assert!(e.enter_group_scope());
        assert!(e.enter_group_scope());
        assert_eq!(e.group_scope_path(), vec![root, child]);
        assert!(!e.select_text_core(ids[3]));
        e.begin_text(Some(ids[3]), Point { x: 40., y: 50. });
        assert!(e.text_draft.is_none());
        e.document.remove(ids[1]);
        e.begin_text(Some(ids[0]), Point { x: 40., y: 50. });
        assert!(!e.exit_group_scope());
        e.set_text(String::new());
        e.finish_text();
        assert!(e.document.get(ids[0]).is_none());
        assert_eq!(e.group_scope_path(), vec![root]);
        e.undo();
        assert_eq!(e.document.get(ids[0]).unwrap().text, "原文");
        assert_eq!(e.group_scope_path(), vec![root]);
    }
    #[test]
    fn nested_group_metadata_and_scope_do_not_change_scene_export_pixels_or_layer_order() {
        let (mut e, ids, _, _) = nested();
        let canvas = Rect::full(400, 300);
        let mut data = vec![0; 400 * 300 * 4];
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index % 251) as u8, ((index * 3) % 251) as u8, 80, 255]);
        }
        let source = SourcePixels {
            canvas,
            fill: None,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &data,
            }],
            overlay: None,
        };
        let before_annotations = e.document_annotations();
        let mut original = source.read(canvas);
        let scene = Scene::prepare(&before_annotations, &source);
        scene.apply(&mut original, canvas);
        assert!(e.enter_group_scope());
        assert!(e.enter_group_scope());
        assert_eq!(e.document_annotations(), before_annotations);
        let refreshed = scene.updated(&e.annotations(), &source);
        let mut scoped = source.read(canvas);
        refreshed.apply(&mut scoped, canvas);
        assert_eq!(scoped.bgra, original.bgra);
        assert!(e.exit_all_group_scopes());
        choose(&mut e, ids[0]);
        assert!(e.group_selection(false));
        let ungrouped = scene.updated(&e.document_annotations(), &source);
        let mut out = source.read(canvas);
        ungrouped.apply(&mut out, canvas);
        assert_eq!(out.bgra, original.bgra);
        let crop = Rect {
            x: 20,
            y: 30,
            width: 250,
            height: 80,
        };
        let mut clipped = source.read(crop);
        ungrouped.apply(&mut clipped, crop);
        for y in 0..crop.height as usize {
            let start = ((y + crop.y as usize) * 400 + crop.x as usize) * 4;
            let length = crop.width as usize * 4;
            assert_eq!(
                &clipped.bgra[y * length..(y + 1) * length],
                &original.bgra[start..start + length]
            );
        }
    }
}
