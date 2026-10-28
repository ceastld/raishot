//! 矩形四角属性的固定选择与局部草稿；确认仍使用 Editor 的一次对象增量。
use super::{Editor, Gesture, RadiusSelection};
use crate::{
    annotation::{Style, Tool},
    document::ObjectId,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RectangleCornerSelection {
    radius: RadiusSelection,
    targets: Vec<(ObjectId, Style)>,
    drawing_style: Option<Style>,
    pub(crate) values: [f32; 4],
    pub(crate) mixed: [bool; 4],
    pub(crate) linked: Option<bool>,
    pub(crate) max_radius: f32,
}
impl RectangleCornerSelection {
    pub(crate) fn count(&self) -> usize {
        self.targets.len()
    }
    pub(crate) fn has_independent(&self) -> bool {
        self.targets
            .iter()
            .any(|(_, s)| s.rectangle_radii.is_some())
            || self
                .drawing_style
                .is_some_and(|s| s.rectangle_radii.is_some())
    }
}

/// None 表示保留该角／模式，混合多选的空输入不会强行统一对象。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RectangleCornerChange {
    pub(crate) linked: Option<bool>,
    pub(crate) values: [Option<f32>; 4],
}

pub(crate) struct RectangleCornerDraft {
    pub(crate) selection: RectangleCornerSelection,
    pub(crate) active: usize,
    pub(crate) linked: bool,
    pub(crate) mode_changed: bool,
    pub(crate) texts: [String; 4],
}
impl RectangleCornerDraft {
    pub(crate) fn new(selection: RectangleCornerSelection) -> Self {
        let texts = std::array::from_fn(|i| {
            if selection.mixed[i] {
                String::new()
            } else {
                selection.values[i].to_string()
            }
        });
        Self {
            linked: selection.linked.unwrap_or(false),
            selection,
            active: 0,
            mode_changed: false,
            texts,
        }
    }
    pub(crate) fn store_text(&mut self, text: &str) {
        if self.linked {
            self.texts = std::array::from_fn(|_| text.to_owned());
        } else {
            self.texts[self.active] = text.to_owned();
        }
    }
    pub(crate) fn set_linked(&mut self, linked: bool) {
        if linked {
            let text = self.texts[self.active].clone();
            self.texts = std::array::from_fn(|_| text.clone());
        }
        self.linked = linked;
        self.mode_changed = true;
    }
    pub(crate) fn changes(&self) -> Result<RectangleCornerChange, &'static str> {
        let mut values = [None; 4];
        for (i, text) in self.texts.iter().enumerate() {
            if text.trim().is_empty() && self.selection.mixed[i] {
                continue;
            }
            let value = text
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && (0. ..=self.selection.max_radius).contains(v))
                .ok_or(tr!("请输入有效的角半径", "Enter a valid corner radius"))?;
            if self.selection.mixed[i] || value != self.selection.values[i] {
                values[i] = Some(value);
            }
        }
        Ok(RectangleCornerChange {
            linked: self.mode_changed.then_some(self.linked),
            values,
        })
    }
}

fn values(style: Style) -> [f32; 4] {
    style.rectangle_radii.unwrap_or([style.radius; 4])
}

fn changed_style(style: Style, change: RectangleCornerChange) -> Style {
    if change == RectangleCornerChange::default() {
        return style;
    }
    let mut after = style;
    let mut corners = values(style);
    for (corner, value) in corners.iter_mut().zip(change.values) {
        if let Some(value) = value {
            *corner = value;
        }
    }
    // 独立页的逐角编辑把旧联动目标转为四值，未编辑的角保留原半径。
    let linked = change.linked.unwrap_or_else(|| {
        change.values.iter().all(Option::is_none) && style.rectangle_radii.is_none()
    });
    if linked {
        // 回到联动时以该对象的局部左上角为基准；显式统一输入会写四个相同值。
        after.radius = corners[0];
        after.rectangle_radii = None;
    } else {
        if style.rectangle_radii == Some(corners) {
            return style;
        }
        after.radius = corners[0];
        after.rectangle_radii = Some(corners);
    }
    after
}

pub(super) fn wheel_style(style: Style, tool: Tool, steps: i32) -> Style {
    if tool != Tool::Rectangle || style.rectangle_radii.is_none() {
        return crate::interaction::Parameter::Radius.adjusted(style, tool, steps);
    }
    let mut after = style;
    let corners = values(style).map(|radius| {
        crate::interaction::Parameter::Radius
            .adjusted(
                Style {
                    radius,
                    ..Default::default()
                },
                Tool::Rectangle,
                steps,
            )
            .radius
    });
    if style.rectangle_radii == Some(corners) {
        return style;
    }
    after.radius = corners[0];
    after.rectangle_radii = Some(corners);
    after
}

impl Editor {
    pub(crate) fn rectangle_corner_selection(&self) -> Option<RectangleCornerSelection> {
        let radius = self.radius_selection();
        let targets: Vec<_> = self
            .selected_objects()
            .into_iter()
            .filter(|(_, a)| a.tool == Tool::Rectangle)
            .map(|(id, a)| (id, a.style))
            .collect();
        let drawing_style =
            if targets.is_empty() && radius.selected_count == 0 && radius.tool == Tool::Rectangle {
                Some(match &self.gesture {
                    Some(Gesture::Draw(a)) => a.style,
                    _ => self.style,
                })
            } else {
                None
            };
        let style = targets
            .iter()
            .find(|(id, _)| Some(*id) == self.selected)
            .or_else(|| targets.first())
            .map(|(_, s)| *s)
            .or(drawing_style)?;
        let preferred = values(style);
        if preferred.iter().any(|v| !v.is_finite() || *v < 0.) {
            return None;
        }
        let all: Vec<_> = targets
            .iter()
            .map(|(_, s)| *s)
            .chain(drawing_style)
            .collect();
        if all
            .iter()
            .any(|s| values(*s).iter().any(|v| !v.is_finite() || *v < 0.))
        {
            return None;
        }
        let linked = style.rectangle_radii.is_none();
        Some(RectangleCornerSelection {
            values: preferred,
            mixed: std::array::from_fn(|i| all.iter().any(|s| values(*s)[i] != preferred[i])),
            linked: all
                .iter()
                .all(|s| s.rectangle_radii.is_none() == linked)
                .then_some(linked),
            max_radius: all.iter().flat_map(|s| values(*s)).fold(50_f32, f32::max),
            radius,
            targets,
            drawing_style,
        })
    }

    pub(crate) fn apply_rectangle_corners(
        &mut self,
        snapshot: &RectangleCornerSelection,
        change: RectangleCornerChange,
    ) -> Result<bool, &'static str> {
        if change
            .values
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || !(0. ..=snapshot.max_radius).contains(v))
        {
            return Err(tr!(
                "圆角超出当前可调范围",
                "The radius is outside the adjustable range"
            ));
        }
        if self.rectangle_corner_selection().as_ref() != Some(snapshot) {
            return Err(tr!(
                "对象或选择已经改变，请重新打开圆角参数",
                "The object or selection changed; reopen the radius setting"
            ));
        }
        if snapshot.targets.is_empty() {
            let before = snapshot.drawing_style.ok_or(tr!(
                "所选对象不是矩形",
                "The selected object is not a rectangle"
            ))?;
            let after = changed_style(before, change);
            if after == before {
                return Ok(false);
            }
            self.set_style(after);
            return Ok(true);
        }
        if snapshot
            .targets
            .iter()
            .all(|(_, s)| changed_style(*s, change) == *s)
        {
            return Ok(false);
        }
        self.update_selected(|a| {
            if a.tool == Tool::Rectangle {
                a.style = changed_style(a.style, change);
            }
        });
        if let Some(style) = self
            .selected_annotation()
            .filter(|a| a.tool == Tool::Rectangle)
            .map(|a| a.style)
        {
            self.style.radius = style.radius;
            self.style.rectangle_radii = style.rectangle_radii;
        }
        Ok(true)
    }

    pub(crate) fn dragging_radius_value(&self) -> Option<f32> {
        match &self.gesture {
            Some(Gesture::Object {
                current,
                radius_delta: Some(_),
                radius_control,
                ..
            }) => Some(current.radius_at_corner(radius_control.map_or(0, |c| c.corner))),
            _ => None,
        }
    }

    /// 正在拖动的圆角柄所在局部角；不在圆角手势中为 None。
    pub(crate) fn dragging_radius_corner(&self) -> Option<usize> {
        match &self.gesture {
            Some(Gesture::Object {
                radius_delta: Some(_),
                radius_control,
                ..
            }) => Some(radius_control.map_or(0, |c| c.corner)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point},
        geometry::Rect,
        interaction::Parameter,
    };

    fn shape(tool: Tool, x: f32, radii: Option<[f32; 4]>) -> Annotation {
        let mut a = Annotation::new(
            tool,
            Point { x, y: 120. },
            Style {
                radius: radii.map_or(12., |r| r[0]),
                rectangle_radii: radii,
                ..crate::tool_presets::default_style(tool)
            },
        );
        a.update(Point {
            x: x + 300.,
            y: 300.,
        });
        a
    }
    fn select(e: &mut Editor, id: ObjectId) {
        e.select_tool(Tool::Edit);
        e.selection.clear();
        e.selected = Some(id);
        e.style = e.document.get(id).unwrap().style;
    }
    fn seeded(a: Annotation) -> (Editor, ObjectId) {
        let canvas = Rect::full(1200, 900);
        let mut e = Editor::new(canvas, Some(canvas));
        let id = e.document.insert(a);
        select(&mut e, id);
        (e, id)
    }
    fn inward(a: &Annotation, start: Point, corner: usize, distance: f32) -> Point {
        let p = a.to_local(start);
        let d = distance * std::f32::consts::FRAC_1_SQRT_2;
        a.to_world(Point {
            x: p.x + if corner == 0 || corner == 3 { d } else { -d },
            y: p.y + if corner < 2 { d } else { -d },
        })
    }
    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 0.001, "{a} != {b}");
    }

    #[test]
    fn rectangle_corner_page_filters_mixed_tools_and_preserves_each_unedited_corner_in_one_history()
    {
        let (mut e, id) = seeded(shape(Tool::Rectangle, 50., Some([3., 7., 11., 15.])));
        let other = e.document.insert(shape(Tool::Rectangle, 400., None));
        let third = e
            .document
            .insert(shape(Tool::Rectangle, 750., Some([5., 9., 13., 17.])));
        let high = e.document.insert(shape(Tool::HighlighterRect, 50., None));
        let mosaic = e.document.insert(shape(Tool::Mosaic, 400., None));
        e.restore_tool_styles([(
            Tool::Rectangle,
            Style {
                radius: 29.,
                ..Default::default()
            },
        )]);
        e.select_all_objects();
        e.selected = Some(id);
        let before = e.document.bounded_history_snapshot().unwrap();
        let presets: std::collections::HashMap<_, _> = e.tool_styles().collect();
        let revision = e.document.edit_revision();
        let snapshot = e.rectangle_corner_selection().unwrap();
        assert_eq!(snapshot.count(), 3);
        assert_eq!(snapshot.mixed, [true; 4]);
        let mut draft = RectangleCornerDraft::new(snapshot);
        draft.active = 2;
        draft.store_text("25");
        let change = draft.changes().unwrap();
        assert_eq!(change.values, [None, None, Some(25.), None]);
        assert_eq!(
            e.apply_rectangle_corners(&draft.selection, change),
            Ok(true)
        );
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.document.bounded_history_snapshot().unwrap().undo.len(),
            before.undo.len() + 1
        );
        assert_eq!(
            e.document.get(id).unwrap().style.rectangle_radii,
            Some([3., 7., 25., 15.])
        );
        assert_eq!(
            e.document.get(other).unwrap().style.rectangle_radii,
            Some([12., 12., 25., 12.])
        );
        assert_eq!(
            e.document.get(third).unwrap().style.rectangle_radii,
            Some([5., 9., 25., 17.])
        );
        for obj in before
            .items
            .iter()
            .filter(|o| o.id == high || o.id == mosaic)
        {
            assert_eq!(e.document.get(obj.id), Some(&obj.annotation));
        }
        assert_eq!(
            e.tool_styles().collect::<std::collections::HashMap<_, _>>(),
            presets
        );
        let after = e.document.items().to_vec();
        e.undo();
        assert_eq!(e.document.items(), before.items);
        e.redo();
        assert_eq!(e.document.items(), after);
    }

    #[test]
    fn rectangle_corner_page_selection_cancel_noop_and_stale_snapshot_keep_redo_and_document() {
        let (mut e, id) = seeded(shape(Tool::Rectangle, 100., Some([6., 12., 18., 24.])));
        let ghost = e.document.insert(shape(Tool::Ellipse, 500., None));
        e.document.undo();
        select(&mut e, id);
        let before = e.document.bounded_history_snapshot().unwrap();
        let revision = e.document.edit_revision();
        let snapshot = e.rectangle_corner_selection().unwrap();
        let mut draft = RectangleCornerDraft::new(snapshot.clone());
        draft.active = 3;
        draft.store_text("24");
        assert_eq!(draft.changes().unwrap(), RectangleCornerChange::default());
        assert_eq!(
            e.apply_rectangle_corners(&snapshot, draft.changes().unwrap()),
            Ok(false)
        );
        drop(draft); // 局部页取消不调用 Editor。
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(ghost).is_some());
        let current = e.document.bounded_history_snapshot().unwrap();
        assert!(e
            .apply_rectangle_corners(
                &snapshot,
                RectangleCornerChange {
                    linked: Some(true),
                    ..Default::default()
                }
            )
            .is_err());
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), current);
    }

    #[test]
    fn rectangle_corner_link_switch_preserves_equal_geometry_and_undoes_only_on_confirmation() {
        let (mut e, id) = seeded(shape(Tool::Rectangle, 100., None));
        let original = e.document.get(id).unwrap().clone();
        let old_path = original.vector_path();
        let revision = e.document.edit_revision();
        let mut draft = RectangleCornerDraft::new(e.rectangle_corner_selection().unwrap());
        draft.set_linked(false);
        draft.active = 2;
        assert_eq!(e.document.get(id), Some(&original));
        assert_eq!(
            e.apply_rectangle_corners(&draft.selection, draft.changes().unwrap()),
            Ok(true)
        );
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.document.get(id).unwrap().style.rectangle_radii,
            Some([12.; 4])
        );
        assert_eq!(e.document.get(id).unwrap().vector_path(), old_path);
        e.undo();
        assert_eq!(e.document.get(id), Some(&original));
        e.redo();
        select(&mut e, id);
        let snapshot = e.rectangle_corner_selection().unwrap();
        e.apply_rectangle_corners(
            &snapshot,
            RectangleCornerChange {
                values: [Some(9.), None, Some(24.), None],
                ..Default::default()
            },
        )
        .unwrap();
        let separated = e.document.get(id).unwrap().clone();
        let snapshot = e.rectangle_corner_selection().unwrap();
        assert_eq!(
            e.apply_rectangle_corners(
                &snapshot,
                RectangleCornerChange {
                    linked: Some(true),
                    ..Default::default()
                }
            ),
            Ok(true)
        );
        assert_eq!(e.document.get(id).unwrap().style.rectangle_radii, None);
        assert_eq!(e.document.get(id).unwrap().corner_radius(), 9.);
        e.undo();
        assert_eq!(e.document.get(id), Some(&separated));
    }

    #[test]
    fn rectangle_corner_rotated_handle_updates_only_its_local_corner_and_commits_once() {
        for angle in [0., 37., 90.] {
            for corner in 0..4 {
                let mut a = shape(Tool::Rectangle, 100., Some([6., 12., 18., 24.]));
                a.rotation = angle;
                let (mut e, id) = seeded(a.clone());
                let history = e.document.bounded_history_snapshot().unwrap();
                let revision = e.document.edit_revision();
                let start = e.radius_controls(6.)[corner].position;
                let end = inward(&a, start, corner, 15.);
                e.pointer_down_at(start, 6., false);
                assert!(e.is_dragging_radius());
                e.pointer_move_at(end);
                let preview = e.selected_annotation().unwrap().clone();
                let values = preview.style.rectangle_radii.unwrap();
                for (i, value) in values.iter().enumerate() {
                    close(
                        *value,
                        a.radius_at_corner(i) + if i == corner { 15. } else { 0. },
                    );
                }
                close(e.dragging_radius_value().unwrap(), values[corner]);
                assert_eq!(e.document.get(id), Some(&a));
                assert_eq!(e.document.edit_revision(), revision);
                e.pointer_up_at(end);
                let after = e.document.get(id).unwrap().clone();
                assert_eq!(after.points, a.points);
                assert_eq!(after.rotation, a.rotation);
                assert_eq!(e.document.edit_revision(), revision + 1);
                assert_eq!(
                    e.document.bounded_history_snapshot().unwrap().undo.len(),
                    history.undo.len() + 1
                );
                e.undo();
                assert_eq!(e.document.get(id), Some(&a));
                e.redo();
                assert_eq!(e.document.get(id), Some(&after));
            }
        }
    }

    #[test]
    fn rectangle_corner_handle_cancel_and_parameter_reanchor_keep_other_corners_and_redo() {
        let mut a = shape(Tool::Rectangle, 100., Some([6., 12., 18., 24.]));
        a.rotation = 37.;
        for cancel in [true, false] {
            let (mut e, id) = seeded(a.clone());
            let ghost = e.document.insert(shape(Tool::Ellipse, 600., None));
            e.document.undo();
            select(&mut e, id);
            let revision = e.document.edit_revision();
            let start = e.radius_controls(6.)[2].position;
            e.pointer_down_at(start, 6., false);
            if cancel {
                e.pointer_move_at(inward(&a, start, 2, 15.));
                assert!(e.cancel_gesture());
            } else {
                e.pointer_up_at(start);
            }
            assert_eq!(e.document.get(id), Some(&a));
            assert_eq!(e.style.rectangle_radii, a.style.rectangle_radii);
            assert_eq!(e.document.edit_revision(), revision);
            assert!(e.document.can_redo());
            e.redo();
            assert!(e.document.get(ghost).is_some());
        }
        let (mut e, id) = seeded(a.clone());
        let start = e.radius_controls(6.)[2].position;
        let end = inward(&a, start, 2, 15.);
        e.pointer_down_at(start, 6., false);
        e.pointer_move_at(end);
        let snapshot = e.rectangle_corner_selection().unwrap();
        e.apply_rectangle_corners(
            &snapshot,
            RectangleCornerChange {
                values: [None, None, Some(40.), None],
                ..Default::default()
            },
        )
        .unwrap();
        e.pointer_move_at(end);
        close(e.selected_annotation().unwrap().radius_at_corner(2), 40.);
        let next = inward(&a, start, 2, 20.);
        e.pointer_move_at(next);
        close(e.selected_annotation().unwrap().radius_at_corner(2), 45.);
        e.pointer_up_at(next);
        close(e.document.get(id).unwrap().radius_at_corner(2), 45.);
        assert_eq!(e.document.get(id).unwrap().radius_at_corner(1), 12.);
    }

    #[test]
    fn rectangle_corner_wheel_and_group_scale_reanchor_preserve_differences_and_one_transaction() {
        let (mut e, id) = seeded(shape(Tool::Rectangle, 100., Some([6., 12., 18., 24.])));
        let ellipse = e.document.insert(shape(Tool::Ellipse, 500., None));
        e.select_all_objects();
        e.selected = Some(id);
        let before = e.document.bounded_history_snapshot().unwrap();
        let revision = e.document.edit_revision();
        assert!(e.adjust_parameter(Parameter::Radius, 1, 74, (0, 0), 6.));
        assert_eq!(
            e.document.get(id).unwrap().style.rectangle_radii,
            Some([7., 13., 19., 25.])
        );
        assert_eq!(
            e.document.get(ellipse).unwrap(),
            &before.items[1].annotation
        );
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.document.bounded_history_snapshot().unwrap().undo.len(),
            before.undo.len() + 1
        );
        e.undo();
        e.redo();
        e.select_all_objects();
        e.selected = Some(id);
        let before = e.document.items().to_vec();
        let revision = e.document.edit_revision();
        let bounds = crate::selection_transform::bounds(&e.selected_objects());
        let start = bounds.handles()[7];
        let anchor = bounds.handles()[0];
        let end = Point {
            x: anchor.x + (start.x - anchor.x) * 2.,
            y: anchor.y + (start.y - anchor.y) * 2.,
        };
        e.begin_group(
            start,
            crate::selection_transform::Manipulation::Scale { bounds, handle: 7 },
        );
        e.pointer_move_at(end);
        assert_eq!(
            e.selected_objects()[0].1.style.rectangle_radii,
            Some([14., 26., 38., 50.])
        );
        let snapshot = e.rectangle_corner_selection().unwrap();
        e.apply_rectangle_corners(
            &snapshot,
            RectangleCornerChange {
                values: [None, Some(30.), None, None],
                ..Default::default()
            },
        )
        .unwrap();
        e.pointer_move_at(end);
        assert_eq!(
            e.selected_objects()[0].1.style.rectangle_radii,
            Some([14., 30., 38., 50.])
        );
        assert_eq!(e.document.items(), before);
        e.pointer_up_at(end);
        assert_eq!(
            e.document.get(id).unwrap().style.rectangle_radii,
            Some([14., 30., 38., 50.])
        );
        assert_eq!(e.document.edit_revision(), revision + 1);
        e.undo();
        assert_eq!(e.document.items(), before);
    }

    #[test]
    fn rectangle_corner_scene_nonzero_roi_rotation_raster_and_effect_cache_share_geometry() {
        use crate::{
            raster,
            scene::{PixelPlane, Scene, SourcePixels},
        };
        let canvas = Rect::full(220, 190);
        let bytes = [100, 100, 100, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
            overlay: None,
        };
        for angle in [0., 37., 90.] {
            let mut a = Annotation::new(
                Tool::Rectangle,
                Point { x: 50., y: 45. },
                Style {
                    filled: true,
                    opacity: 0.4,
                    color: 0x2244dd,
                    radius: 0.,
                    rectangle_radii: Some([0., 22., 34., 9.]),
                    ..Default::default()
                },
            );
            a.update(Point { x: 150., y: 125. });
            a.rotation = angle;
            let scene = Scene::prepare(&[a.clone()], &source);
            assert!(scene.first_error().is_none());
            let mut full = source.read(canvas);
            scene.apply(&mut full, canvas);
            let center = a.to_world(Point { x: 100., y: 85. });
            let i =
                ((center.y.floor() as u32 * canvas.width + center.x.floor() as u32) * 4) as usize;
            assert_eq!(&full.bgra[i..i + 4], &[148, 87, 74, 255]);
            let cut = a.to_world(Point { x: 146.5, y: 48.5 });
            assert!(!a.hit(cut, 0., true));
            let i = ((cut.y.floor() as u32 * canvas.width + cut.x.floor() as u32) * 4) as usize;
            assert_eq!(&full.bgra[i..i + 4], &[100, 100, 100, 255]);
            let (rect, raster_image) = raster::shape(&a, canvas).unwrap();
            let mut fallback = source.read(canvas);
            raster::composite(&mut fallback, canvas, &raster_image, rect);
            for (actual, expected) in full.bgra.iter().zip(&fallback.bgra) {
                // 沿已有Scene/Raster抗锯齿量化合同，两个真实渲染后端每通道最多1。
                assert!((*actual as i16 - *expected as i16).abs() <= 1);
            }
            let roi = Rect {
                x: 39,
                y: 30,
                width: 129,
                height: 117,
            };
            let mut clipped = source.read(roi);
            scene.apply(&mut clipped, roi);
            for y in 0..roi.height {
                for x in 0..roi.width {
                    let i = ((y * roi.width + x) * 4) as usize;
                    let j = (((y + roi.y) * canvas.width + x + roi.x) * 4) as usize;
                    assert_eq!(&clipped.bgra[i..i + 4], &full.bgra[j..j + 4]);
                }
            }
            for tool in [Tool::Mosaic, Tool::Blur] {
                let mut effect = Annotation::new(
                    tool,
                    Point { x: 40., y: 35. },
                    Style {
                        block_size: 8,
                        ..Default::default()
                    },
                );
                effect.update(Point { x: 165., y: 135. });
                let old = Scene::prepare(&[a.clone(), effect.clone()], &source);
                let mut changed = a.clone();
                changed.style.rectangle_radii = Some([0., 0., 34., 9.]);
                let hot = old.updated(&[changed.clone(), effect.clone()], &source);
                let cold = Scene::prepare(&[changed, effect], &source);
                assert!(hot.first_error().is_none() && cold.first_error().is_none());
                let mut original = source.read(canvas);
                old.apply(&mut original, canvas);
                let mut actual = source.read(canvas);
                hot.apply(&mut actual, canvas);
                let mut expected = source.read(canvas);
                cold.apply(&mut expected, canvas);
                assert_eq!(actual.bgra, expected.bgra);
                assert_ne!(actual.bgra, original.bgra);
            }
        }
    }
}

#[cfg(test)]
mod successor_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point},
        geometry::Rect,
        selection_transform::Manipulation,
    };
    fn shape(x: f32) -> Annotation {
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x, y: 120. },
            Style {
                radius: 6.,
                rectangle_radii: Some([6., 12., 18., 24.]),
                ..Default::default()
            },
        );
        a.update(Point {
            x: x + 300.,
            y: 300.,
        });
        a
    }
    #[test]
    fn rectangle_corner_group_parameter_reanchor_cancel_restores_document_style_and_redo() {
        let canvas = Rect::full(1200, 900);
        let mut e = Editor::new(canvas, Some(canvas));
        let id = e.document.insert(shape(100.));
        e.document.insert(shape(500.));
        let ghost = e.document.insert(shape(850.));
        e.document.undo();
        e.select_all_objects();
        e.selected = Some(id);
        let original = e.document.bounded_history_snapshot().unwrap();
        let revision = e.document.edit_revision();
        let bounds = crate::selection_transform::bounds(&e.selected_objects());
        let start = bounds.handles()[7];
        let anchor = bounds.handles()[0];
        let end = Point {
            x: anchor.x + (start.x - anchor.x) * 2.,
            y: anchor.y + (start.y - anchor.y) * 2.,
        };
        e.begin_group(start, Manipulation::Scale { bounds, handle: 7 });
        e.pointer_move_at(end);
        let snapshot = e.rectangle_corner_selection().unwrap();
        e.apply_rectangle_corners(
            &snapshot,
            RectangleCornerChange {
                values: [None, None, Some(40.), None],
                ..Default::default()
            },
        )
        .unwrap();
        e.pointer_move_at(end);
        assert_eq!(e.selected_annotation().unwrap().radius_at_corner(2), 40.);
        assert!(e.cancel_gesture());
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), original);
        assert_eq!(e.style.rectangle_radii, Some([6., 12., 18., 24.]));
        assert_eq!(e.style.radius_for(Tool::Rectangle), 6.);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(ghost).is_some());
    }
    #[test]
    fn rectangle_corner_drawing_defaults_restore_without_selected_object_leak_and_invalid_model_rejects(
    ) {
        let canvas = Rect::full(1200, 900);
        let mut e = Editor::new(canvas, Some(canvas));
        e.select_tool(Tool::Rectangle);
        let snapshot = e.rectangle_corner_selection().unwrap();
        assert_eq!(
            e.apply_rectangle_corners(
                &snapshot,
                RectangleCornerChange {
                    linked: Some(false),
                    values: [Some(3.), Some(8.), Some(15.), Some(22.)]
                }
            ),
            Ok(true)
        );
        assert!(e.adjust_parameter(crate::interaction::Parameter::Radius, 1, 200, (0, 0), 6.));
        assert_eq!(e.style.rectangle_radii, Some([4., 9., 16., 23.]));
        e.pointer_down((100, 120), 6.);
        assert!(e.adjust_parameter(crate::interaction::Parameter::Radius, 1, 201, (0, 0), 6.));
        e.pointer_up((400, 300));
        let id = e.document.items()[0].id;
        assert_eq!(
            e.document.get(id).unwrap().style.rectangle_radii,
            Some([5., 10., 17., 24.])
        );
        e.select_tool(Tool::Pen);
        e.select_tool(Tool::Rectangle);
        assert_eq!(e.style.rectangle_radii, Some([5., 10., 17., 24.]));
        e.select_tool(Tool::Edit);
        e.selected = Some(id);
        e.style = e.document.get(id).unwrap().style;
        let snapshot = e.rectangle_corner_selection().unwrap();
        e.apply_rectangle_corners(
            &snapshot,
            RectangleCornerChange {
                values: [None, None, Some(35.), None],
                ..Default::default()
            },
        )
        .unwrap();
        e.select_tool(Tool::Rectangle);
        assert_eq!(e.style.rectangle_radii, Some([5., 10., 17., 24.]));
        for invalid in [-1., f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut a = shape(100.);
            a.style.rectangle_radii = Some([6., invalid, 18., 24.]);
            assert!(!a.valid());
            let mut source = Editor::new(canvas, Some(canvas));
            source.document.insert(a);
            assert!(crate::pin_raster_document::WireRasterDocument::from_editor(
                &source,
                (1200, 900)
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point},
        geometry::Rect,
    };
    #[test]
    fn rectangle_corner_draft_preserves_other_invalid_input_and_links_the_active_value_without_early_commit(
    ) {
        let canvas = Rect::full(400, 300);
        let mut e = Editor::new(canvas, Some(canvas));
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 20., y: 30. },
            Style {
                radius: 3.,
                rectangle_radii: Some([3., 8., 15., 22.]),
                ..Default::default()
            },
        );
        a.update(Point { x: 250., y: 200. });
        let id = e.document.insert(a.clone());
        e.select_tool(Tool::Edit);
        e.selected = Some(id);
        e.style = a.style;
        let before = e.document.bounded_history_snapshot().unwrap();
        let mut draft = RectangleCornerDraft::new(e.rectangle_corner_selection().unwrap());
        draft.active = 1;
        draft.store_text("bad");
        draft.active = 2;
        draft.store_text("25");
        assert!(draft.changes().is_err());
        assert_eq!(draft.texts[1], "bad");
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        draft.active = 1;
        draft.store_text("8");
        draft.active = 2;
        assert_eq!(
            draft.changes().unwrap().values,
            [None, None, Some(25.), None]
        );
        draft.set_linked(true);
        let changes = draft.changes().unwrap();
        assert_eq!(changes.linked, Some(true));
        assert_eq!(changes.values, [Some(25.); 4]);
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(
            e.apply_rectangle_corners(&draft.selection, changes),
            Ok(true)
        );
        assert_eq!(e.document.get(id).unwrap().style.rectangle_radii, None);
        assert_eq!(e.document.get(id).unwrap().style.radius, 25.);
    }
}

#[cfg(test)]
mod imported_noop_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point},
        geometry::Rect,
        interaction::Parameter,
    };
    #[test]
    fn rectangle_corner_imported_inactive_uniform_value_does_not_create_click_wheel_or_mode_noop_history(
    ) {
        let canvas = Rect::full(900, 700);
        let mut e = Editor::new(canvas, Some(canvas));
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 100., y: 120. },
            Style {
                radius: 19.,
                rectangle_radii: Some([50.; 4]),
                ..Default::default()
            },
        );
        a.update(Point { x: 400., y: 300. });
        assert!(a.valid());
        let id = e.document.insert(a.clone());
        let ghost = e.document.insert(a.clone());
        e.document.undo();
        e.select_tool(Tool::Edit);
        e.selected = Some(id);
        e.style = a.style;
        let before = e.document.bounded_history_snapshot().unwrap();
        let revision = e.document.edit_revision();
        let start = e.radius_controls(6.)[2].position;
        e.pointer_down_at(start, 6., false);
        e.pointer_up_at(start);
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert!(!e.adjust_parameter(Parameter::Radius, 1, 202, (0, 0), 6.));
        let snapshot = e.rectangle_corner_selection().unwrap();
        assert_eq!(
            e.apply_rectangle_corners(
                &snapshot,
                RectangleCornerChange {
                    linked: Some(false),
                    ..Default::default()
                }
            ),
            Ok(false)
        );
        assert_eq!(e.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(ghost).is_some());
    }
}
