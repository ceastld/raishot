//! 控制点、撤销和共享几何的回归；验证输入结果而非实现结构。
use crate::{
    annotation::{contains_path, Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
    vector_path::{Span, VectorPath},
};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn editor() -> Editor {
    Editor::new(Rect::full(640, 480), Some(Rect::full(640, 480)))
}
fn arrow(e: &mut Editor) {
    e.select_tool(Tool::Arrow);
    e.pointer_down((100, 100), 6.);
    e.pointer_up((300, 100));
}
fn poly(e: &mut Editor) {
    e.select_tool(Tool::Polyline);
    for pos in [(100, 100), (200, 200), (300, 100)] {
        e.pointer_down(pos, 6.);
        e.pointer_up(pos);
    }
    e.finish_gesture();
}
#[test]
fn arrow_passes_through_middle_and_head_follows_endpoint_tangent() {
    let mut a = Annotation::new(Tool::Arrow, p(100., 100.), Style::default());
    a.update(p(300., 100.));
    a.move_arrow_handle(2, p(200., 220.));
    let path = a.vector_path();
    assert!(path
        .flattened()
        .iter()
        .any(|&(a, b)| crate::annotation::distance_to_segment(p(200., 220.), a, b) < 0.04));
    let Span::Line { start, end } = path.spans[1] else {
        panic!()
    };
    assert_eq!(end, p(300., 100.));
    let Span::Line { end: other, .. } = path.spans[2] else {
        panic!()
    };
    assert!((start.x + other.x) / 2. < 300. && (start.y + other.y) / 2. > 100.);
    assert!(a.hit(p(200., 220.), 1., true));
    assert!(!a.hit(p(200., 100.), 1., true));
    assert!(a.bounds().contains(p(200., 220.), 0.001));
    for i in 0..100 {
        assert!(a.bounds().contains(path.spans[0].at(i as f32 / 99.), 0.001));
    }
    a.move_arrow_handle(1, p(100., 300.));
    assert!(a.arrow_midpoint().distance(p(-20., 200.)) < 0.001);
    assert!(a.valid());
}
#[test]
fn cubic_bounds_use_extrema_and_svg_separates_disconnected_heads() {
    let path = VectorPath {
        spans: vec![Span::Cubic {
            start: p(0., 0.),
            control1: p(0., 100.),
            control2: p(100., 100.),
            end: p(100., 0.),
        }],
        ..VectorPath::default()
    };
    let b = path.bounds().unwrap();
    assert!((b.height - 75.).abs() < 0.001);
    let path = VectorPath::arrow(p(20., 20.), p(120., 20.), p(70., 20.), Style::default());
    assert_eq!(path.svg_commands().matches('M').count(), 2);
}
#[test]
fn mid_and_endpoint_edits_are_one_undo_each_cancel_and_near_chord_snap() {
    let mut e = editor();
    arrow(&mut e);
    let id = e.selected.unwrap();
    let original = e.document.get(id).unwrap().clone();
    e.pointer_down((200, 100), 6.);
    e.pointer_move((200, 210));
    e.pointer_move((200, 220));
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.pointer_up((200, 220));
    let curved = e.document.get(id).unwrap().clone();
    assert_eq!(curved.points[2], p(200., 220.));
    e.pointer_down((300, 100), 6.);
    e.pointer_up((400, 100));
    assert_eq!(e.document.get(id).unwrap().arrow_midpoint(), p(250., 280.));
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &curved);
    e.redo();
    e.undo();
    e.selected = Some(id);
    e.pointer_down((200, 220), 6.);
    e.pointer_move((300, 300));
    e.cancel_gesture();
    assert_eq!(e.document.get(id).unwrap(), &curved);
    assert!(e.document.can_redo());
    e.pointer_down((200, 220), 6.);
    e.pointer_up((200, 105));
    assert_eq!(e.document.get(id).unwrap().points.len(), 2);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &curved);
    e.selected = Some(id);
    assert!(e.straighten_arrow_at((200, 220), 6.));
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &curved);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.undo();
    assert!(e.document.items().is_empty());
}
#[test]
fn node_drag_changes_one_vertex_and_shift_uses_adjacent_node() {
    let mut e = editor();
    poly(&mut e);
    let id = e.selected.unwrap();
    let original = e.document.get(id).unwrap().clone();
    assert_eq!(original.handles(), original.points);
    e.pointer_down((200, 200), 6.);
    assert_eq!(e.constrained_pointer((220, 140), true).1, 100);
    e.pointer_up((220, 140));
    let changed = e.document.get(id).unwrap();
    assert_eq!(
        changed.points,
        vec![p(100., 100.), p(220., 140.), p(300., 100.)]
    );
    assert_eq!(e.active_node(), Some(1));
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.undo();
    assert!(e.document.items().is_empty());
}
#[test]
fn insertion_deletion_are_reversible_and_two_nodes_are_retained() {
    let mut e = editor();
    poly(&mut e);
    let id = e.selected.unwrap();
    let original = e.document.get(id).unwrap().clone();
    assert!(e.insert_polyline_node((150, 150), 6.));
    assert_eq!(e.active_node(), Some(1));
    assert_eq!(e.document.get(id).unwrap().points.len(), 4);
    assert!(e.remove_polyline_point());
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.undo();
    assert_eq!(e.document.get(id).unwrap().points.len(), 4);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.selected = Some(id);
    e.pointer_down((200, 200), 6.);
    e.pointer_up((200, 200));
    assert!(e.remove_polyline_point());
    assert_eq!(e.document.get(id).unwrap().points.len(), 2);
    e.pointer_down((100, 100), 6.);
    e.pointer_up((100, 100));
    assert!(!e.remove_polyline_point());
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
}
#[test]
fn draft_backspace_and_doubleclick_finish_do_not_add_extra_history() {
    let mut e = editor();
    e.select_tool(Tool::Polyline);
    for pos in [(100, 100), (200, 200), (300, 100)] {
        e.pointer_down(pos, 6.);
        e.pointer_up(pos);
    }
    assert!(e.remove_polyline_point());
    assert!(!e.document.can_undo());
    assert!(e.insert_polyline_node((250, 250), 6.));
    assert_eq!(
        e.document.items()[0].annotation.points,
        vec![p(100., 100.), p(200., 200.), p(250., 250.)]
    );
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}
#[test]
fn smooth_polyline_passes_vertices_fill_and_closing_edge_hit() {
    let mut a = Annotation::new(Tool::Polyline, p(50., 50.), Style::default());
    a.points = vec![
        p(50., 50.),
        p(200., 50.),
        p(200., 200.),
        p(120., 100.),
        p(50., 200.),
    ];
    a.style.filled = true;
    assert!(a.hit(p(70., 100.), 0., true));
    assert!(!a.hit(p(140., 180.), 0., true));
    assert!(a.node_insertion(p(50., 120.), 6.).is_some());
    a.style.polyline_smooth = true;
    let path = a.vector_path();
    assert_eq!(path.spans.len(), a.points.len());
    for (i, s) in path.spans.iter().enumerate() {
        assert_eq!(s.start(), a.points[i]);
        assert_eq!(s.end(), a.points[(i + 1) % a.points.len()]);
    }
    assert!(contains_path(p(100., 70.), &path.flattened()));
}
#[test]
fn curve_and_translucent_closed_polyline_export_matches_region_render() {
    let canvas = Rect::full(360, 300);
    let pixels = vec![255; 360 * 300 * 4];
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let mut a = Annotation::new(Tool::Arrow, p(40., 40.), Style::default());
    a.update(p(300., 40.));
    a.move_arrow_handle(2, p(170., 190.));
    let mut b = Annotation::new(
        Tool::Polyline,
        p(40., 210.),
        Style {
            filled: true,
            opacity: 0.5,
            color: 0,
            polyline_smooth: true,
            ..Style::default()
        },
    );
    b.points = vec![p(40., 210.), p(120., 200.), p(110., 280.)];
    let scene = Scene::prepare(&[a, b], &source);
    let mut full = source.read(canvas);
    scene.apply(&mut full, canvas);
    let region = Rect {
        x: 150,
        y: 170,
        width: 50,
        height: 40,
    };
    let mut part = source.read(region);
    scene.apply(&mut part, region);
    for y in 0..region.height {
        let start = ((y + region.y) * canvas.width + region.x) as usize * 4;
        assert_eq!(
            &part.bgra[(y * region.width) as usize * 4..((y + 1) * region.width) as usize * 4],
            &full.bgra[start..start + region.width as usize * 4]
        );
    }
    let i = (220 * 360 + 85) * 4;
    assert!((full.bgra[i] as i32 - 127).abs() <= 1);
}

#[test]
fn enter_drops_rubberband_preview_and_repeated_click_does_not_add_nodes() {
    let mut e = editor();
    e.select_tool(Tool::Polyline);
    for pos in [(100, 100), (200, 200), (200, 200)] {
        e.pointer_down(pos, 6.);
        e.pointer_up(pos);
    }
    e.pointer_move((400, 400));
    e.finish_gesture();
    assert_eq!(
        e.document.items()[0].annotation.points,
        vec![p(100., 100.), p(200., 200.)]
    );
    e.undo();
    e.select_tool(Tool::Polyline);
    e.pointer_down((100, 100), 6.);
    e.pointer_move((300, 300));
    e.finish_gesture();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}

#[test]
fn dashed_curve_keeps_complete_arrowheads_with_uniform_opacity() {
    let mut a = Annotation::new(
        Tool::Arrow,
        p(20., 30.),
        Style {
            dashed: true,
            opacity: 0.5,
            width: 6.,
            arrow_start: true,
            ..Style::default()
        },
    );
    a.update(p(280., 30.));
    a.move_arrow_handle(2, p(150., 180.));
    let (r, image) = crate::raster::shape(&a, Rect::full(320, 240)).unwrap();
    for span in a.vector_path().spans.iter().skip(1) {
        for t in [0.2, 0.5, 0.8] {
            let q = span.at(t);
            let x = q.x.floor() as u32 - r.x;
            let y = q.y.floor() as u32 - r.y;
            assert!((image.bgra[((y * r.width + x) * 4 + 3) as usize] as i32 - 128).abs() <= 1);
        }
    }
}
