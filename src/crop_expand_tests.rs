//! 选区外八格沿固定边扩展/收缩的模型验收；不创建窗口或读取桌面。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    crop_mask::Polygon,
    editor::Editor,
    geometry::{Rect, ScreenRect},
    interaction::Hover,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn editor() -> Editor {
    Editor::new(Rect::full(400, 300), Some(rect(100, 80, 120, 100)))
}

fn click(editor: &mut Editor, point: (i32, i32)) {
    editor.pointer_down(point, 4.);
    editor.pointer_up(point);
}

fn edit_editor() -> Editor {
    let mut editor = editor();
    editor.select_tool(Tool::Edit);
    editor
}

#[test]
fn edit_screenshot_eight_cells_share_down_hover_and_preserve_edit_mode() {
    for (point, edges, expected) in [
        ((50, 40), 5, rect(50, 40, 170, 140)),
        ((160, 40), 4, rect(100, 40, 120, 140)),
        ((270, 40), 6, rect(100, 40, 170, 140)),
        ((50, 130), 1, rect(50, 80, 170, 100)),
        ((270, 130), 2, rect(100, 80, 170, 100)),
        ((50, 220), 9, rect(50, 80, 170, 140)),
        ((160, 220), 8, rect(100, 80, 120, 140)),
        ((270, 220), 10, rect(100, 80, 170, 140)),
    ] {
        let mut editor = edit_editor();
        let before = editor.crop;
        assert_eq!(editor.screenshot_crop_outside_edges(point, 4.), Some(edges));
        assert!(editor.begin_screenshot_crop_outside(point, 4.));
        assert_eq!(editor.crop, before);
        assert_eq!(editor.hover(point, 4.), Hover::edges(edges));
        editor.pointer_up(point);
        assert_eq!(editor.crop, Some(expected));
        assert_eq!(editor.tool, Tool::Edit);
        assert!(!editor.document.can_undo());
    }
}

fn selected_outside_object() -> Editor {
    let mut editor = edit_editor();
    let mut annotation =
        Annotation::new(Tool::Rectangle, Point { x: 50., y: 130. }, Style::default());
    annotation.update(Point { x: 80., y: 160. });
    let outside = editor.document.insert(annotation);
    let mut inside = Annotation::new(
        Tool::Rectangle,
        Point { x: 130., y: 100. },
        Style::default(),
    );
    inside.update(Point { x: 180., y: 150. });
    let other = editor.document.insert(inside);
    editor.selected = Some(outside);
    editor.selection = vec![outside, other];
    editor
}

#[test]
fn edit_screenshot_outside_object_and_multiselection_cannot_steal_host_resize() {
    let mut editor = selected_outside_object();
    let annotations = editor.document_annotations();
    let selected = editor.selected;
    let selection = editor.selected_ids();
    assert!(editor
        .document
        .items()
        .iter()
        .any(|item| item.annotation.hit(Point { x: 50., y: 130. }, 4., true)));
    assert_eq!(editor.screenshot_crop_outside_edges((50, 130), 4.), Some(1));
    assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
    editor.pointer_up((50, 130));
    assert_eq!(editor.crop, Some(rect(50, 80, 170, 100)));
    assert_eq!(editor.tool, Tool::Edit);
    assert_eq!(editor.selected, selected);
    assert_eq!(editor.selected_ids(), selection);
    assert_eq!(editor.document_annotations(), annotations);
}

#[test]
fn edit_screenshot_outside_drag_keeps_side_through_crossing_and_shift_constraint() {
    let mut editor = edit_editor();
    assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
    editor.pointer_move((160, 130));
    assert_eq!(editor.crop, Some(rect(160, 80, 60, 100)));
    editor.pointer_move((260, 130));
    assert_eq!(editor.crop, Some(rect(220, 80, 40, 100)));
    assert_eq!(editor.hover((260, 130), 4.), Hover::Horizontal);
    editor.pointer_up((260, 130));
    assert_eq!(editor.tool, Tool::Edit);

    let mut diagonal = edit_editor();
    assert!(diagonal.begin_screenshot_crop_outside((50, 40), 4.));
    let constrained = diagonal.constrained_point(Point { x: 40., y: 60. }, true);
    diagonal.pointer_up_at(constrained);
    assert_eq!(diagonal.crop, Some(rect(40, 30, 180, 150)));
    assert_eq!(diagonal.tool, Tool::Edit);
}

#[test]
fn edit_screenshot_cancel_and_no_change_preserve_selection_mask_and_document_history() {
    let mut editor = selected_outside_object();
    editor.document.set_crop_mask(Some(crop_mask()));
    let crop = editor.crop;
    let mask = editor.document.crop_mask.clone();
    let annotations = editor.document_annotations();
    let selected = editor.selected;
    let selection = editor.selected_ids();
    let stamp = editor.scene_stamp();
    let revision = (editor.document.revision(), editor.document.edit_revision());
    assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
    editor.pointer_move((20, 130));
    assert!(editor.cancel_gesture());
    assert_eq!(editor.crop, crop);
    assert_eq!(editor.document.crop_mask, mask);
    assert_eq!(editor.scene_stamp().0, stamp.0);
    // 既有 cancel 会推进预览修订以清掉拖动画面，文档/遮罩本身不发生修改。
    assert!(editor.scene_stamp().1 > stamp.1);
    assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
    editor.pointer_up((100, 130));
    assert_eq!(editor.crop, crop);
    assert_eq!(editor.document.crop_mask, mask);
    assert_eq!(editor.selected, selected);
    assert_eq!(editor.selected_ids(), selection);
    assert_eq!(editor.tool, Tool::Edit);
    assert_eq!(editor.document_annotations(), annotations);
    assert_eq!(
        (editor.document.revision(), editor.document.edit_revision()),
        revision
    );
}

#[test]
fn edit_screenshot_committed_resize_invalidates_mask_without_an_object_command() {
    let mut editor = edit_editor();
    let mut annotation =
        Annotation::new(Tool::Rectangle, Point { x: 50., y: 130. }, Style::default());
    annotation.update(Point { x: 80., y: 160. });
    let id = editor.document.insert(annotation);
    editor.selected = Some(id);
    editor.document.set_crop_mask(Some(crop_mask()));
    let annotations = editor.document_annotations();
    let stamp = editor.scene_stamp();
    assert!(editor.begin_screenshot_crop_outside((50, 130), 4.));
    editor.pointer_up((50, 130));
    assert_eq!(editor.crop, Some(rect(50, 80, 170, 100)));
    assert!(editor.document.crop_mask.is_none());
    assert_ne!(editor.scene_stamp(), stamp);
    assert_eq!(editor.selected, Some(id));
    assert_eq!(editor.document_annotations(), annotations);
    // 裁剪本身没有添加对象撤销步骤：下一次 Undo 仍撤销原来的对象插入。
    editor.undo();
    assert!(editor.document.items().is_empty());
    editor.redo();
    assert_eq!(editor.document_annotations(), annotations);
}

#[test]
fn edit_screenshot_outside_handles_are_specific_while_inside_and_shared_default_stay_original() {
    for (crop, point, edges) in [
        (rect(100, 80, 120, 100), (98, 78), 5),
        (rect(100, 80, 2, 40), (104, 100), 2),
        (rect(100, 80, 40, 2), (120, 84), 8),
    ] {
        let mut editor = Editor::new(Rect::full(400, 300), Some(crop));
        editor.select_tool(Tool::Edit);
        assert_eq!(editor.screenshot_crop_outside_edges(point, 6.), Some(edges));
        assert!(editor.begin_screenshot_crop_outside(point, 6.));
        assert_eq!(editor.hover(point, 6.), Hover::edges(edges));
    }
    let mut shared = selected_outside_object();
    let crop = shared.crop;
    assert_eq!(shared.screenshot_crop_outside_edges((160, 130), 4.), None);
    for point in [(-1, 130), (400, 130), (160, -1), (160, 300)] {
        assert!(!shared.begin_screenshot_crop_outside(point, 4.));
    }
    // 图片/贴图仍调用这个默认入口；外侧对象编辑不会因此变成截图范围调整。
    shared.pointer_down((50, 130), 4.);
    assert!(!shared.is_changing_crop());
    assert_eq!(shared.crop, crop);
    assert!(shared.cancel_gesture());
}

#[test]
fn outside_eight_cells_click_expand_only_their_corresponding_edges() {
    for (point, expected) in [
        ((50, 40), rect(50, 40, 170, 140)),
        ((160, 40), rect(100, 40, 120, 140)),
        ((270, 40), rect(100, 40, 170, 140)),
        ((50, 130), rect(50, 80, 170, 100)),
        ((270, 130), rect(100, 80, 170, 100)),
        ((50, 220), rect(50, 80, 170, 140)),
        ((160, 220), rect(100, 80, 120, 140)),
        ((270, 220), rect(100, 80, 170, 140)),
    ] {
        let mut editor = editor();
        let before = editor.crop;
        editor.pointer_down(point, 4.);
        assert_eq!(editor.crop, before, "down at {point:?}");
        assert!(editor.is_dragging());
        editor.pointer_up(point);
        assert_eq!(editor.crop, Some(expected), "click at {point:?}");
        assert!(!editor.is_dragging());
        assert!(!editor.document.can_undo());
    }
}

#[test]
fn outside_drag_keeps_the_initial_side_and_can_shrink_or_cross_the_opposite_edge() {
    let mut editor = editor();
    editor.pointer_down((50, 130), 4.);
    editor.pointer_move((160, 130));
    assert_eq!(editor.crop, Some(rect(160, 80, 60, 100)));
    editor.pointer_move((260, 130));
    assert_eq!(editor.crop, Some(rect(220, 80, 40, 100)));
    editor.pointer_up((260, 130));
    assert_eq!(editor.crop, Some(rect(220, 80, 40, 100)));

    let mut diagonal = self::editor();
    diagonal.pointer_down((50, 40), 4.);
    diagonal.pointer_up((270, 220));
    assert_eq!(diagonal.crop, Some(rect(220, 180, 50, 40)));
}

#[test]
fn outside_drag_clamps_to_canvas_and_reuses_corner_aspect_constraint() {
    let mut upper = editor();
    upper.pointer_down((50, 40), 4.);
    upper.pointer_up((-100, -80));
    assert_eq!(upper.crop, Some(rect(0, 0, 220, 180)));

    let mut lower = editor();
    lower.pointer_down((270, 220), 4.);
    lower.pointer_up((900, 600));
    assert_eq!(lower.crop, Some(rect(100, 80, 300, 220)));

    let mut constrained = editor();
    constrained.pointer_down((50, 40), 4.);
    let point = constrained.constrained_point(Point { x: 40., y: 60. }, true);
    constrained.pointer_up_at(point);
    assert_eq!(constrained.crop, Some(rect(40, 30, 180, 150)));
}

#[test]
fn crop_handles_and_center_move_keep_priority_over_outside_expansion() {
    let mut edge = editor();
    edge.pointer_down((103, 130), 4.);
    edge.pointer_up((150, 130));
    assert_eq!(edge.crop, Some(rect(150, 80, 70, 100)));

    let mut center = editor();
    center.pointer_down((160, 130), 4.);
    center.pointer_up((180, 150));
    assert_eq!(center.crop, Some(rect(120, 100, 120, 100)));

    let mut new_selection = Editor::new(Rect::full(400, 300), None);
    new_selection.pointer_down((270, 220), 4.);
    new_selection.pointer_up((50, 40));
    assert_eq!(new_selection.crop, Some(rect(50, 40, 220, 180)));
    new_selection.set_crop(None);
    new_selection.pointer_down((50, 40), 4.);
    new_selection.pointer_up((270, 220));
    assert_eq!(new_selection.crop, Some(rect(50, 40, 220, 180)));
}

fn crop_mask() -> Polygon {
    Polygon::new(&[
        Point { x: 100., y: 80. },
        Point { x: 220., y: 80. },
        Point { x: 220., y: 180. },
        Point { x: 100., y: 180. },
    ])
    .unwrap()
}

#[test]
fn cancelled_or_unchanged_crop_preserves_mask_and_document_history() {
    let mut editor = editor();
    let mask = crop_mask();
    editor.document.set_crop_mask(Some(mask.clone()));
    let before = editor.crop;
    let revision = editor.document.revision();
    let edit_revision = editor.document.edit_revision();
    click(&mut editor, (160, 130));
    assert_eq!(editor.crop, before);
    assert_eq!(editor.document.crop_mask, Some(mask.clone()));
    assert_eq!(editor.document.revision(), revision);
    assert_eq!(editor.document.edit_revision(), edit_revision);

    editor.pointer_down((50, 40), 4.);
    editor.pointer_move((20, 10));
    assert_ne!(editor.crop, before);
    assert!(editor.cancel_gesture());
    assert_eq!(editor.crop, before);
    assert_eq!(editor.document.crop_mask, Some(mask.clone()));
    assert_eq!(editor.document.revision(), revision);

    editor.pointer_down((50, 130), 4.);
    editor.pointer_up((100, 130));
    assert_eq!(editor.crop, before);
    assert_eq!(editor.document.crop_mask, Some(mask));
    assert_eq!(editor.document.revision(), revision);
    editor.undo();
    assert!(editor.document.crop_mask.is_none());
    editor.redo();
    assert_eq!(editor.document.crop_mask, Some(crop_mask()));
}

#[test]
fn committed_expansion_invalidates_mask_stamp_without_moving_annotations_or_effect_cache() {
    let canvas = Rect::full(32, 24);
    let mut editor = Editor::new(canvas, Some(rect(10, 8, 12, 10)));
    let mut lower = Annotation::new(Tool::Rectangle, Point { x: 2., y: 2. }, Style::default());
    lower.update(Point { x: 14., y: 10. });
    let mut mosaic = Annotation::new(
        Tool::Mosaic,
        Point { x: 2., y: 2. },
        Style {
            width: 4.,
            ..Style::default()
        },
    );
    mosaic.update(Point { x: 24., y: 20. });
    editor.document.insert_many(vec![lower, mosaic]);
    let annotations = editor.document_annotations();
    let mask = Polygon::new(&[
        Point { x: 10., y: 8. },
        Point { x: 22., y: 8. },
        Point { x: 22., y: 18. },
        Point { x: 10., y: 18. },
    ])
    .unwrap();
    editor.document.set_crop_mask(Some(mask));
    let before_stamp = editor.scene_stamp();
    let pixels = [20_u8, 40, 60, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let scene = Scene::prepare(&annotations, &source);
    click(&mut editor, (0, 0));
    assert_eq!(editor.crop, Some(rect(0, 0, 22, 18)));
    assert!(editor.document.crop_mask.is_none());
    assert_ne!(editor.scene_stamp(), before_stamp);
    assert_eq!(editor.document_annotations(), annotations);
    let refreshed = scene.updated(&editor.document_annotations(), &source);
    let (Layer::Mosaic { blocks: before }, Layer::Mosaic { blocks: after }) =
        (&scene.layers[1], &refreshed.layers[1])
    else {
        panic!("missing mosaic cache");
    };
    assert!(!before.is_empty());
    assert!(Arc::ptr_eq(before, after));
    editor.undo();
    assert!(editor.document.items().is_empty());
    assert!(editor.document.crop_mask.is_none());
    editor.redo();
    assert_eq!(editor.document_annotations(), annotations);
    assert!(editor.document.crop_mask.is_none());
}

#[test]
fn expansion_across_monitors_uses_virtual_desktop_coordinates_with_negative_origin() {
    let screens = [
        ScreenRect {
            left: -600,
            top: -120,
            width: 600,
            height: 420,
        },
        ScreenRect {
            left: 0,
            top: -120,
            width: 400,
            height: 420,
        },
    ];
    let desktop = ScreenRect::union(&screens).unwrap();
    let mut editor = Editor::new(
        Rect::full(desktop.width, desktop.height),
        Some(rect(300, 100, 200, 100)),
    );
    let mut annotation = Annotation::new(
        Tool::Rectangle,
        Point { x: 350., y: 130. },
        Style::default(),
    );
    annotation.update(Point { x: 450., y: 170. });
    editor.document.insert(annotation.clone());
    let global_point = (100, 180);
    let local_point = (global_point.0 - desktop.left, global_point.1 - desktop.top);
    click(&mut editor, local_point);
    assert_eq!(editor.crop, Some(rect(300, 100, 400, 200)));
    let expanded = editor.crop.unwrap();
    assert!(expanded.x < screens[1].local(desktop).x);
    assert!(expanded.x + expanded.width > screens[1].local(desktop).x);
    assert_eq!(desktop.left + expanded.x as i32, -300);
    assert_eq!(desktop.top + expanded.y as i32, -20);
    assert_eq!(editor.document_annotations(), vec![annotation]);
}

#[test]
fn tiny_crop_right_and_bottom_edges_keep_opposite_origin() {
    for tolerance in [6., 7.5, 9., 10.5, 12.] {
        let mut narrow = Editor::new(Rect::full(400, 300), Some(rect(100, 80, 2, 40)));
        narrow.pointer_down((102, 100), tolerance);
        narrow.pointer_up((112, 100));
        assert_eq!(
            narrow.crop,
            Some(rect(100, 80, 12, 40)),
            "right, tolerance={tolerance}"
        );
        let mut short = Editor::new(Rect::full(400, 300), Some(rect(100, 80, 40, 2)));
        short.pointer_down((120, 82), tolerance);
        short.pointer_up((120, 92));
        assert_eq!(
            short.crop,
            Some(rect(100, 80, 40, 12)),
            "bottom, tolerance={tolerance}"
        );
    }
}

#[test]
fn tiny_crop_four_corners_choose_the_nearest_sides() {
    for (from, to, expected) in [
        ((100, 80), (90, 70), rect(90, 70, 12, 12)),
        ((102, 80), (112, 70), rect(100, 70, 12, 12)),
        ((100, 82), (90, 92), rect(90, 80, 12, 12)),
        ((102, 82), (112, 92), rect(100, 80, 12, 12)),
    ] {
        let mut editor = Editor::new(Rect::full(400, 300), Some(rect(100, 80, 2, 2)));
        editor.pointer_down(from, 7.5);
        editor.pointer_up(to);
        assert_eq!(editor.crop, Some(expected), "corner={from:?}");
        assert!(!editor.document.can_undo());
    }
}

#[test]
fn tiny_crop_cancel_preserves_mask_and_history() {
    let initial = rect(100, 80, 2, 2);
    let mut editor = Editor::new(Rect::full(400, 300), Some(initial));
    let mask = crop_mask();
    editor.document.set_crop_mask(Some(mask.clone()));
    let stamp = editor.scene_stamp();
    let edit_revision = editor.document.edit_revision();
    let history = (editor.document.can_undo(), editor.document.can_redo());
    editor.pointer_down((102, 82), 7.5);
    editor.pointer_move((112, 92));
    assert_eq!(editor.crop, Some(rect(100, 80, 12, 12)));
    assert!(editor.cancel_gesture());
    assert_eq!(editor.crop, Some(initial));
    assert_eq!(editor.document.crop_mask, Some(mask));
    // 取消会推进预览代次以清掉手势缓存，但不提交文档或历史。
    assert_eq!(editor.scene_stamp(), (stamp.0, stamp.1 + 1));
    assert_eq!(editor.document.edit_revision(), edit_revision);
    assert_eq!(
        (editor.document.can_undo(), editor.document.can_redo()),
        history
    );
}

#[test]
fn narrow_crop_outside_middle_handles_are_single_axis() {
    use crate::interaction::Hover;
    for tolerance in [6., 7.5, 9., 12.] {
        for (crop, from, to, hover, expected) in [
            (
                rect(100, 80, 10, 40),
                (105, 77),
                (105, 70),
                Hover::Vertical,
                rect(100, 70, 10, 50),
            ),
            (
                rect(100, 80, 10, 40),
                (105, 123),
                (105, 130),
                Hover::Vertical,
                rect(100, 80, 10, 50),
            ),
            (
                rect(100, 80, 40, 10),
                (97, 85),
                (90, 85),
                Hover::Horizontal,
                rect(90, 80, 50, 10),
            ),
            (
                rect(100, 80, 40, 10),
                (143, 85),
                (150, 85),
                Hover::Horizontal,
                rect(100, 80, 50, 10),
            ),
        ] {
            let mut editor = Editor::new(Rect::full(400, 300), Some(crop));
            assert_eq!(editor.hover(from, tolerance), hover);
            editor.pointer_down(from, tolerance);
            assert_eq!(editor.hover(from, tolerance), hover);
            editor.pointer_up(to);
            assert_eq!(editor.crop, Some(expected));
        }
    }
}
