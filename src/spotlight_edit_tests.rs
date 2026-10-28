//! 聚光灯共享层、独立孔及跨会话参数的定向模型回归；不创建 GPU 或窗口。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    interaction::Parameter,
    tool_presets::{default_style, ToolPresets},
};

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn hole(left: f32, radius: f32) -> Annotation {
    let mut a = Annotation::new(
        Tool::Spotlight,
        point(left, 40.),
        Style {
            radius,
            ..default_style(Tool::Spotlight)
        },
    );
    a.update(point(left + 100., 120.));
    a
}
fn seeded() -> (Editor, ObjectId, ObjectId, ObjectId) {
    let canvas = Rect::full(640, 480);
    let mut e = Editor::new(canvas, Some(canvas));
    let first = e.document.insert(hole(40., 6.));
    let second = e.document.insert(hole(180., 21.));
    let mut rect = Annotation::new(
        Tool::Rectangle,
        point(420., 40.),
        Style {
            color: 0x123456,
            width: 8.,
            opacity: 0.2,
            radius: 12.,
            ..Style::default()
        },
    );
    rect.update(point(520., 120.));
    let rect = e.document.insert(rect);
    (e, first, second, rect)
}
fn select(e: &mut Editor, id: ObjectId) {
    e.select_tool(Tool::Edit);
    e.selected = Some(id);
    e.style = e.document.get(id).unwrap().style;
}
fn shared_style(e: &Editor, color: u32, width: f32, opacity: f32) {
    for o in e.document.items() {
        if o.annotation.tool == Tool::Spotlight {
            assert_eq!(o.annotation.style.color, color);
            assert_eq!(o.annotation.style.width, width);
            assert!((o.annotation.style.opacity - opacity).abs() < 0.00001);
        }
    }
}

#[test]
fn spotlight_wheel_modifiers_zero_border_and_scaled_values_are_monotonic() {
    assert_eq!(
        Parameter::for_tool(Tool::Spotlight, false, false),
        Some(Parameter::Opacity)
    );
    assert_eq!(
        Parameter::for_tool(Tool::Spotlight, false, true),
        Some(Parameter::Width)
    );
    assert_eq!(
        Parameter::for_tool(Tool::Spotlight, true, false),
        Some(Parameter::Radius)
    );
    assert_eq!(
        Parameter::for_tool(Tool::Spotlight, true, true),
        Some(Parameter::Radius)
    );
    let defaults = default_style(Tool::Spotlight);
    assert_eq!(defaults.color, 0xffffff);
    assert_eq!(defaults.width, 2.);
    assert_eq!(defaults.opacity, 0.5);
    assert_eq!(defaults.radius, 0.);
    assert_eq!(
        Parameter::Width
            .adjusted(defaults, Tool::Spotlight, -2)
            .width,
        0.
    );
    assert_eq!(
        Parameter::Width
            .adjusted(defaults, Tool::Spotlight, 200)
            .width,
        20.
    );
    assert_eq!(
        Parameter::Radius
            .adjusted(defaults, Tool::Spotlight, 200)
            .radius,
        50.
    );
    let scaled = Style {
        width: 35.,
        radius: 65.,
        ..defaults
    };
    assert_eq!(
        Parameter::Width.adjusted(scaled, Tool::Spotlight, 1).width,
        35.
    );
    assert_eq!(
        Parameter::Width.adjusted(scaled, Tool::Spotlight, -1).width,
        34.
    );
    assert_eq!(
        Parameter::Radius
            .adjusted(scaled, Tool::Spotlight, -1)
            .radius,
        64.
    );
}

#[test]
fn spotlight_shared_wheel_undo_restores_all_holes_and_radius_stays_local() {
    let (mut e, first, second, rect) = seeded();
    let original = e.document.items().to_vec();
    let rectangle = e.document.get(rect).unwrap().clone();
    select(&mut e, first);
    assert!(e.adjust_parameter(Parameter::Opacity, 2, 91, (0, 0), 4.));
    assert!(e.adjust_parameter(Parameter::Opacity, 3, 91, (0, 0), 4.));
    shared_style(&e, 0xffffff, 2., 0.55);
    assert_eq!(e.document.get(rect).unwrap(), &rectangle);
    e.undo();
    assert_eq!(e.document.items(), original);
    e.redo();
    shared_style(&e, 0xffffff, 2., 0.55);
    select(&mut e, first);
    let mut style = e.style;
    style.radius = 17.;
    e.set_style(style);
    assert_eq!(e.document.get(first).unwrap().style.radius, 17.);
    assert_eq!(e.document.get(second).unwrap().style.radius, 21.);
    e.undo();
    assert_eq!(e.document.get(first).unwrap().style.radius, 6.);
    shared_style(&e, 0xffffff, 2., 0.55);
}

#[test]
fn spotlight_draft_shared_preview_cancel_and_insert_are_atomic() {
    let (mut e, _, _, _) = seeded();
    let original = e.document.items().to_vec();
    let revision = e.document.revision();
    e.select_tool(Tool::Spotlight);
    e.pointer_down((320, 210), 4.);
    e.pointer_move((400, 290));
    assert!(e.adjust_parameter(Parameter::Opacity, 10, 101, (0, 0), 4.));
    assert!(e.adjust_parameter(Parameter::Width, -2, 102, (0, 0), 4.));
    assert!(e.adjust_parameter(Parameter::Radius, 17, 103, (0, 0), 4.));
    assert_eq!(e.document.items(), original);
    assert_eq!(e.document.revision(), revision);
    let preview = e.annotations();
    let holes: Vec<_> = preview
        .iter()
        .filter(|a| a.tool == Tool::Spotlight)
        .collect();
    assert_eq!(holes.len(), 3);
    for a in &holes {
        assert_eq!(a.style.width, 0.);
        assert!((a.style.opacity - 0.6).abs() < 0.00001);
    }
    assert_eq!(holes[0].style.radius, 6.);
    assert_eq!(holes[1].style.radius, 21.);
    assert_eq!(holes[2].style.radius, 17.);
    assert!(e.cancel_gesture());
    assert_eq!(e.document.items(), original);
    assert_eq!(
        e.annotations(),
        original
            .iter()
            .map(|o| o.annotation.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(e.document.revision(), revision);

    // 草稿参数可作为下一笔的默认值，正式插入与旧孔联动只提交一条命令。
    e.pointer_down((320, 210), 4.);
    e.pointer_up((400, 290));
    assert_eq!(e.document.items().len(), original.len() + 1);
    shared_style(&e, 0xffffff, 0., 0.6);
    let committed = e.document.items().to_vec();
    e.undo();
    assert_eq!(e.document.items(), original);
    e.redo();
    assert_eq!(e.document.items(), committed);
}

#[test]
fn spotlight_mixed_selection_only_spreads_changed_layer_fields() {
    let (mut e, first, second, rect) = seeded();
    let original = e.document.items().to_vec();
    e.select_tool(Tool::Edit);
    e.selection = vec![first, rect];
    e.selected = Some(rect);
    e.style = e.document.get(rect).unwrap().style;
    let mut style = e.style;
    style.color = 0x39c996;
    e.set_style(style);
    shared_style(&e, 0x39c996, 2., 0.5);
    assert_eq!(e.document.get(first).unwrap().style.radius, 6.);
    assert_eq!(e.document.get(second).unwrap().style.radius, 21.);
    let rectangle = e.document.get(rect).unwrap();
    assert_eq!(rectangle.style.color, 0x39c996);
    assert_eq!(rectangle.style.width, 8.);
    assert_eq!(rectangle.style.opacity, 0.2);
    assert_eq!(rectangle.style.radius, 12.);
    e.undo();
    assert_eq!(e.document.items(), original);
    e.redo();
    shared_style(&e, 0x39c996, 2., 0.5);
}

#[test]
fn spotlight_hole_scale_and_midgesture_style_keep_shared_border_width() {
    let (mut e, first, second, rect) = seeded();
    let original = e.document.items().to_vec();
    e.select_tool(Tool::Edit);
    e.selection = vec![first, second];
    e.selected = Some(second);
    let handle = e.selection_bounds().unwrap().handles()[7];
    e.pointer_down((handle.x as i32, handle.y as i32), 4.);
    e.pointer_move((400, 160));
    let preview = e.selected_objects();
    let old_width = original[0].annotation.local_bounds().width;
    assert!(preview[0].1.local_bounds().width > old_width);
    assert_eq!(preview[0].1.style.width, 2.);
    assert!(preview[0].1.style.radius > 6.);
    assert!(e.adjust_parameter(Parameter::Opacity, 1, 201, (0, 0), 4.));
    // 同一几何再次采样不能把共享 width 错误除以组缩放因子。
    e.pointer_move((400, 160));
    assert_eq!(e.document.items(), original);
    e.pointer_up((400, 160));
    shared_style(&e, 0xffffff, 2., 0.51);
    assert_eq!(e.document.get(rect).unwrap(), &original[2].annotation);
    e.undo();
    assert_eq!(e.document.items(), original);
}

#[test]
fn spotlight_rounded_hit_rotation_and_scale_use_the_same_contour() {
    let mut a = hole(40., 30.);
    assert!(a.hit(point(90., 80.), 0., true));
    assert!(!a.hit(point(41., 41.), 0., true));
    assert!(a.hit(point(52., 52.), 0., true));
    assert!(a.segments().len() > 4);
    let pivot = crate::transform::center(a.local_bounds());
    a.rotate_about(pivot, 37.);
    assert!(!a.hit(a.to_world(point(41., 41.)), 0., true));
    a.scale_about(pivot, 2.);
    assert_eq!(a.style.radius, 60.);
    assert_eq!(a.style.width, 2.);
    assert!(a.hit(pivot, 0., true));
}

#[test]
fn spotlight_zero_border_roundtrip_defaults_and_tool_isolation() {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/qa")
        .join(format!(
            "spotlight-presets-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("tool-presets.json");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(folder.clone());
    // 旧 INI 夹具经首次读取迁移为 JSON。
    std::fs::write(
        folder.join("tool-presets.ini"),
        "[spotlight]\nwidth=0\nradius=17\n[rectangle]\nwidth=0\n",
    )
    .unwrap();
    let mut presets = ToolPresets::load_at(&path).unwrap();
    assert_eq!(presets.style(Tool::Spotlight).width, 0.);
    assert_eq!(presets.style(Tool::Rectangle).width, 4.);
    let mut invalid = presets.style(Tool::Spotlight);
    invalid.width = f32::NAN;
    invalid.radius = -1.;
    assert!(!presets.update(Tool::Spotlight, invalid));
    for invalid_width in [-1., f32::INFINITY, 512.1] {
        invalid.width = invalid_width;
        assert!(!presets.update(Tool::Spotlight, invalid));
    }
    let canvas = Rect::full(640, 480);
    let mut e = Editor::new(canvas, Some(canvas));
    e.restore_tool_styles(presets.styles());
    e.select_tool(Tool::Rectangle);
    e.set_width(9.);
    e.select_tool(Tool::Spotlight);
    assert_eq!(e.style.width, 0.);
    assert_eq!(e.style.color, 0xffffff);
    assert_eq!(e.style.radius, 17.);
    e.pointer_down((80, 80), 4.);
    e.pointer_up((180, 160));
    let id = e.selected.unwrap();
    select(&mut e, id);
    e.set_width(2.);
    e.set_width(0.);
    let mut style = e.style;
    style.opacity = 0.65;
    e.set_style(style);
    for (tool, style) in e.tool_styles() {
        presets.update(tool, style);
    }
    assert!(presets.save_at(&path).unwrap());
    let loaded = ToolPresets::load_at(&path).unwrap();
    assert_eq!(loaded.style(Tool::Spotlight).width, 0.);
    assert_eq!(loaded.style(Tool::Spotlight).radius, 17.);
    assert_eq!(loaded.style(Tool::Spotlight).opacity, 0.65);
    assert_eq!(loaded.style(Tool::Rectangle).width, 9.);
    assert_eq!(loaded.style(Tool::Rectangle).color, Style::default().color);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved["tools"]["spotlight"]["width"], 0.0);
}
