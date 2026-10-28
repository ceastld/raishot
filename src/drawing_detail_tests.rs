//! 绘制细节的行为回归：输入、原始数据、几何、命中、裁剪与增量历史。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    arrow_geometry::{ArrowHead, ArrowPreset},
    editor::Editor,
    geometry::Rect,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn editor() -> Editor {
    Editor::new(Rect::full(640, 480), Some(Rect::full(640, 480)))
}
fn a(preset: ArrowPreset, curve: bool) -> Annotation {
    let mut s = Style {
        width: 6.,
        ..Style::default()
    };
    preset.apply(&mut s);
    let mut a = Annotation::new(Tool::Arrow, p(80., 100.), s);
    a.update(p(450., 100.));
    if curve {
        a.move_arrow_handle(2, p(265., 230.));
    }
    a
}
#[test]
fn shift_pen_captures_mode_snap_keeps_only_two_points_and_one_undo() {
    let mut e = editor();
    e.select_tool(Tool::Pen);
    e.pointer_down_with_shift((100, 100), 6., true);
    for i in 1..180 {
        e.pointer_move((100 + i, 100 + i / 3));
    }
    e.set_width(8.);
    assert!(e.document.items().is_empty());
    assert_eq!(
        e.constrained_pointer((300, 180), true),
        e.constrained_pointer((300, 180), false)
    );
    e.pointer_up((300, 180));
    let line = e.document.items()[0].annotation.clone();
    assert_eq!(line.points.len(), 2);
    assert_eq!(line.style.width, 8.);
    assert_eq!(line.vector_path().spans.len(), 1);
    let end = line.points[1];
    assert!(((end.y - 100.).atan2(end.x - 100.).to_degrees() - 22.5).abs() < 0.2);
    e.undo();
    assert!(e.document.items().is_empty() && !e.document.can_undo());
    e.redo();
    assert_eq!(e.document.items()[0].annotation, line);
}
#[test]
fn shift_pen_outside_canvas_preserves_angle_clipped_export_redo_and_freehand() {
    let mut e = editor();
    e.select_tool(Tool::Pen);
    e.pointer_down((20, 20), 6.);
    e.pointer_up((80, 30));
    e.undo();
    e.pointer_down_with_shift((600, 400), 6., true);
    e.pointer_move((800, 480));
    let path = e.annotations();
    let end = *path[0].points.last().unwrap();
    // 标注保留完整几何；画布只限制可见/导出像素，不能夹紧端点而破坏角度。
    assert!(end.x > 640. && end.y > 480.);
    assert!(((end.y - 400.).atan2(end.x - 600.).to_degrees() - 22.5).abs() < 0.5);
    let canvas = e.canvas;
    let pixels = [255u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let scene = Scene::prepare(&path, &source);
    assert!(scene.errors.is_empty(), "{:?}", scene.errors);
    let mut full = source.read(canvas);
    scene.apply(&mut full, canvas);
    assert_eq!((full.width, full.height), (640, 480));
    assert_eq!(full.bgra.len(), pixels.len());
    let region = Rect {
        x: 590,
        y: 390,
        width: 50,
        height: 40,
    };
    let mut crop = source.read(region);
    scene.apply(&mut crop, region);
    let mut ink = 0;
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            let i = ((y * canvas.width + x) * 4) as usize;
            let j = (((y - region.y) * region.width + x - region.x) * 4) as usize;
            assert_eq!(full.bgra[i..i + 4], crop.bgra[j..j + 4]);
            ink += usize::from(crop.bgra[j..j + 4] != [255, 255, 255, 255]);
        }
    }
    assert!(ink > 20);
    e.cancel_gesture();
    assert!(e.document.can_redo() && !e.document.can_undo());
    e.pointer_down((100, 100), 6.);
    for i in 1..30 {
        let q = e.constrained_pointer((100 + i, 100 + i * i / 30), true);
        e.pointer_move(q);
    }
    e.pointer_up((130, 130));
    assert!(e.document.items()[0].annotation.points.len() > 20);
}
#[test]
fn pencil_drawing_shift_line_edit_mode_and_round_dot() {
    let mut e = editor();
    e.select_tool(Tool::Pen);
    e.pointer_down_with_shift((100, 100), 6., true);
    e.pointer_up((300, 100));
    let original = e.document.items()[0].annotation.clone();
    assert!(e.selected_ids().is_empty());
    e.select_tool(Tool::Edit);
    e.pointer_down_with_shift((200, 100), 6., true);
    e.pointer_up((220, 140));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.items()[0].annotation.points.len(), 2);
    e.undo();
    assert_eq!(e.document.items()[0].annotation, original);
    e.select_tool(Tool::Pen);
    e.pointer_down_with_shift((500, 400), 6., true);
    e.pointer_up((500, 400));
    let dot = &e.document.items()[1].annotation;
    assert_eq!(dot.points.len(), 1);
    assert!(dot.hit(p(500., 400.), 0., false));
}
#[test]
fn arrow_all_presets_have_visible_finite_geometry_short_long_curved_and_rotated() {
    for preset in ArrowPreset::ALL {
        for width in [1., 4., 48.] {
            for length in [4., 12., 370.] {
                for bend in [0., 180., -180.] {
                    let mut arrow = a(preset, false);
                    arrow.style.width = width;
                    arrow.points = vec![
                        p(80., 240.),
                        p(80. + length, 240.),
                        p(80. + length / 2., 240. + bend),
                    ];
                    let local = arrow.vector_path();
                    assert!(!local.spans.is_empty() || !local.fills.is_empty());
                    let b = local.bounds().unwrap();
                    assert!([b.x, b.y, b.width, b.height].iter().all(|v| v.is_finite()));
                    for contour in &local.fills {
                        assert!(contour.len() >= 3);
                        for &q in contour {
                            assert!(b.contains(q, 0.001));
                        }
                    }
                    arrow.rotate_about(p(200., 200.), 37.);
                    let rotated = arrow.vector_path();
                    let b = rotated.bounds().unwrap();
                    for contour in &rotated.fills {
                        for &q in contour {
                            assert!(b.contains(q, 0.002));
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn trimmed_curved_shaft_extrema_and_filled_head_hit_include_visible_geometry() {
    for preset in [
        ArrowPreset::Single,
        ArrowPreset::Double,
        ArrowPreset::FilledSingle,
        ArrowPreset::FilledDouble,
        ArrowPreset::Dart,
    ] {
        let arrow = a(preset, true);
        assert!(arrow.bounds().contains(p(265., 230.), 0.04));
        assert!(arrow.hit(p(265., 230.), 1., true));
        if preset != ArrowPreset::Dart {
            assert!(arrow.vector_path().spans[0].end().distance(arrow.points[1]) > 1.);
        }
        let path = arrow.vector_path();
        for contour in path.fills {
            let center = if contour.len() == 3 {
                p(
                    contour.iter().map(|q| q.x).sum::<f32>() / 3.,
                    contour.iter().map(|q| q.y).sum::<f32>() / 3.,
                )
            } else {
                p(265., 230.)
            };
            assert!(arrow.hit(center, 0., false));
        }
        assert!(!arrow.hit(p(265., 100.), 0., true));
    }
}
#[test]
fn filled_and_dart_stay_vector_with_single_alpha_rotation_and_cropped_export() {
    let canvas = Rect::full(640, 480);
    let pixels = [255u8, 255, 255, 255].repeat((640 * 480) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    for preset in [
        ArrowPreset::FilledSingle,
        ArrowPreset::FilledDouble,
        ArrowPreset::Dart,
    ] {
        let mut arrow = a(preset, true);
        arrow.rotate_about(p(320., 240.), -25.);
        let full = Scene::prepare(&[arrow.clone()], &source);
        assert!(matches!(full.layers[0], Layer::Vector { .. }));
        let mut opaque = source.read(canvas);
        full.apply(&mut opaque, canvas);
        arrow.style.opacity = 0.5;
        let scene = Scene::prepare(&[arrow], &source);
        // 半透明箭头也保持共享矢量层，覆盖率合并后只混色一次。
        assert!(matches!(scene.layers[0], Layer::Vector { .. }));
        let mut transparent = source.read(canvas);
        scene.apply(&mut transparent, canvas);
        let region = Rect {
            x: 100,
            y: 60,
            width: 420,
            height: 300,
        };
        let mut crop = source.read(region);
        scene.apply(&mut crop, region);
        let mut ink = 0;
        for y in region.y..region.y + region.height {
            for x in region.x..region.x + region.width {
                let i = ((y * 640 + x) * 4) as usize;
                let j = (((y - region.y) * region.width + x - region.x) * 4) as usize;
                assert_eq!(transparent.bgra[i..i + 4], crop.bgra[j..j + 4]);
                if opaque.bgra[i + 1] == 0x4b {
                    ink += 1;
                    assert!((transparent.bgra[i + 1] as i32 - 165).abs() <= 1);
                }
            }
        }
        assert!(ink > 1000);
    }
}
#[test]
fn arrow_preset_edit_preserves_other_styles_curve_and_single_undo() {
    let mut e = editor();
    let id = e.document.insert(a(ArrowPreset::Single, true));
    let other = e.document.insert(Annotation::new(
        Tool::Pen,
        p(40., 40.),
        Style {
            color: 0x123456,
            width: 13.,
            ..Style::default()
        },
    ));
    e.tool = Tool::Edit;
    e.selected = Some(id);
    e.selection = vec![other, id];
    e.style = e.document.get(id).unwrap().style;
    let original = e.document.get(id).unwrap().clone();
    let pen = e.document.get(other).unwrap().clone();
    let mut style = e.style;
    ArrowPreset::FilledDouble.apply(&mut style);
    e.set_style(style);
    let changed = e.document.get(id).unwrap();
    assert_eq!(changed.style.arrow_head, ArrowHead::Triangle);
    assert!(changed.style.arrow_start);
    assert_eq!(changed.points, original.points);
    assert_eq!(e.document.get(other).unwrap().style.color, pen.style.color);
    assert_eq!(e.document.get(other).unwrap().style.width, pen.style.width);
    assert_eq!(e.document.get(other).unwrap(), &pen);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.redo();
    assert_eq!(
        e.document.get(id).unwrap().style.arrow_head,
        ArrowHead::Triangle
    );
}
#[test]
fn dashed_filled_heads_are_complete_without_double_alpha() {
    let mut arrow = a(ArrowPreset::FilledDouble, false);
    arrow.style.dashed = true;
    arrow.style.opacity = 0.5;
    let (rect, image) = crate::raster::shape(&arrow, Rect::full(640, 480)).unwrap();
    for triangle in arrow.vector_path().fills {
        let q = p(
            triangle.iter().map(|p| p.x).sum::<f32>() / 3.,
            triangle.iter().map(|p| p.y).sum::<f32>() / 3.,
        );
        let i = (((q.y.floor() as u32 - rect.y) * rect.width + q.x.floor() as u32 - rect.x) * 4 + 3)
            as usize;
        assert!((image.bgra[i] as i32 - 128).abs() <= 1);
    }
}

#[test]
fn pencil_drawing_over_existing_strokes_adds_ink_without_moving_or_selecting() {
    let mut e = editor();
    e.select_tool(Tool::Pen);
    e.pointer_down((100, 100), 6.);
    e.pointer_move((200, 100));
    e.pointer_up((300, 100));
    let original = e.document.items()[0].annotation.clone();
    assert!(e.selected_ids().is_empty());
    assert!(e.rotation_handle(6.).is_none());
    for point in [(100, 100), (200, 100), (300, 100)] {
        assert_eq!(e.hover(point, 6.), crate::interaction::Hover::Crosshair);
    }
    e.pointer_down((200, 100), 6.);
    e.pointer_move((220, 160));
    e.pointer_up((240, 200));
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(e.document.items()[0].annotation, original);
    assert!(e.selected_ids().is_empty());
    e.pointer_down_with_shift((100, 100), 6., true);
    e.pointer_up((100, 220));
    assert_eq!(e.document.items().len(), 3);
    assert_eq!(e.document.items()[2].annotation.points.len(), 2);
    assert!(e.selected_ids().is_empty());
    e.undo();
    e.undo();
    assert_eq!(e.document.items()[0].annotation, original);
    e.undo();
    assert!(e.document.items().is_empty());
    e.redo();
    assert_eq!(e.document.items()[0].annotation, original);
    assert!(e.selected_ids().is_empty());
}
#[test]
fn pencil_drawing_style_changes_affect_next_stroke_and_edit_mode_is_explicit() {
    let mut e = editor();
    e.select_tool(Tool::Pen);
    e.pointer_down((100, 100), 6.);
    e.pointer_move((200, 100));
    e.pointer_up((300, 100));
    let original = e.document.items()[0].annotation.clone();
    e.adjust_parameter(crate::interaction::Parameter::Width, 2, 1, (200, 100), 6.);
    let mut style = e.style;
    style.color = 0x123456;
    style.pencil_smooth = false;
    e.set_style(style);
    assert_eq!(e.document.items()[0].annotation, original);
    e.pointer_down((200, 100), 6.);
    e.pointer_up((200, 200));
    assert_eq!(e.document.items()[1].annotation.style, style);
    e.undo();
    assert_eq!(e.document.items().len(), 1);
    assert!(e.document.can_redo());
    e.pointer_down((200, 100), 6.);
    e.pointer_move((200, 200));
    e.cancel_gesture();
    assert_eq!(e.document.items()[0].annotation, original);
    assert!(e.document.can_redo());
    e.select_tool(Tool::Edit);
    assert_eq!(e.hover((160, 100), 6.), crate::interaction::Hover::Move);
    e.pointer_down((160, 100), 6.);
    e.pointer_up((180, 120));
    assert_eq!(e.selected_ids().len(), 1);
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.items()[0].annotation.points[0], p(120., 120.));
    e.select_tool(Tool::Pen);
    assert!(e.selected_ids().is_empty());
    e.pointer_down((180, 120), 6.);
    e.pointer_up((180, 200));
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(e.document.items()[0].annotation.points[0], p(120., 120.));
}
