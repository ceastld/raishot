//! 临时：逐笔绘制每次移动的模型/Scene 成本（不含 GPUI 绘制）。
use crate::{
    annotation::Tool,
    editor::Editor,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
};

fn run(tool: Tool, tweak: impl Fn(&mut Editor)) -> (f64, f64) {
    let full = Rect::full(1920, 1080);
    let bytes = vec![128u8; 1920 * 1080 * 4];
    let source = SourcePixels {
        fill: None,
        canvas: full,
        planes: vec![PixelPlane {
            rect: full,
            bgra: &bytes,
        }],
        overlay: None,
    };
    let mut editor = Editor::new(full, Some(full));
    editor.select_tool(tool);
    tweak(&mut editor);
    let mut scene = std::sync::Arc::new(Scene::prepare(&editor.annotations(), &source));
    editor.pointer_down((100, 100), 4.);
    let mut worst = 0f64;
    let started = std::time::Instant::now();
    let n = 600;
    for i in 0..n {
        let t = i as f32 / n as f32;
        let p = (
            (100. + 1600. * t) as i32,
            (540. + 400. * (t * 12.).sin()) as i32,
        );
        let s = std::time::Instant::now();
        editor.pointer_move(p);
        let a = s.elapsed().as_secs_f64() * 1000.;
        let anns = editor.annotations();
        let b = s.elapsed().as_secs_f64() * 1000.;
        scene = std::sync::Arc::new(scene.updated_with_draft(
            &anns,
            &source,
            editor.live_stroke_draft_index(),
        ));
        let c = s.elapsed().as_secs_f64() * 1000.;
        if i % 150 == 0 || i == n - 1 {
            eprintln!(
                "  move {i}: pointer_move {a:.2} annotations {:.2} scene {:.2}",
                b - a,
                c - b
            );
        }
        worst = worst.max(c);
    }
    (started.elapsed().as_secs_f64() * 1000. / n as f64, worst)
}

#[test]
#[ignore]
fn pin_draw_per_move_cost() {
    for (name, tool, op) in [
        ("pen", Tool::Pen, 1.0f32),
        ("pen-50%", Tool::Pen, 0.5),
        ("highlighter", Tool::Highlighter, 1.0),
    ] {
        let (avg, worst) = run(tool, |e| {
            let mut s = e.style;
            s.opacity = op;
            e.set_style(s);
        });
        eprintln!("{name}: avg {avg:.3} ms, worst {worst:.3} ms");
    }
}

#[test]
fn committed_stroke_is_rebuilt_from_the_full_mesh_not_the_draft() {
    let full = Rect::full(400, 300);
    let bytes = vec![128u8; 400 * 300 * 4];
    let source = SourcePixels {
        fill: None,
        canvas: full,
        planes: vec![PixelPlane {
            rect: full,
            bgra: &bytes,
        }],
        overlay: None,
    };
    let mut editor = Editor::new(full, Some(full));
    editor.select_tool(Tool::Pen);
    let mut scene = Scene::prepare(&editor.annotations(), &source);
    editor.pointer_down((20, 20), 4.);
    for i in 0..60 {
        editor.pointer_move((20 + i * 5, 20 + (i * 7) % 90));
        scene = scene.updated_with_draft(
            &editor.annotations(),
            &source,
            editor.live_stroke_draft_index(),
        );
    }
    assert_eq!(scene.draft_layer, Some(0));
    editor.pointer_up((320, 80));
    assert_eq!(editor.live_stroke_draft_index(), None);
    let committed = scene.updated(&editor.annotations(), &source);
    let fresh = Scene::prepare(&editor.annotations(), &source);
    assert_eq!(committed.draft_layer, None);
    let mesh = |s: &Scene| match &s.layers[0] {
        crate::scene::Layer::Vector { mesh, .. } => (mesh.vertices.len(), mesh.indices.clone()),
        _ => panic!("pen layer"),
    };
    assert_eq!(mesh(&committed), mesh(&fresh));
}

#[test]
#[ignore]
fn stroke_stage_costs() {
    use crate::annotation::{Annotation, Point, Style};
    let mut a = Annotation::new(
        Tool::Pen,
        Point { x: 100., y: 100. },
        Style {
            width: 4.,
            ..Default::default()
        },
    );
    for i in 0..600 {
        let t = i as f32 / 600.;
        a.update(Point {
            x: 100. + 1600. * t,
            y: 540. + 400. * (t * 12.).sin(),
        });
    }
    let time = |name: &str, f: &mut dyn FnMut()| {
        let s = std::time::Instant::now();
        for _ in 0..5 {
            f();
        }
        eprintln!("{name}: {:.2} ms", s.elapsed().as_secs_f64() * 1000. / 5.);
    };
    let path = a.vector_path();
    eprintln!("spans {}", path.spans.len());
    time("vector_path", &mut || {
        let _ = a.vector_path();
    });
    time("flattened", &mut || {
        let _ = path.flattened();
    });
    time("build_draft", &mut || {
        let _ = crate::vector_mesh::VectorMesh::build_draft(&path, a.style, false);
    });
    time("build_full", &mut || {
        let _ = crate::vector_mesh::VectorMesh::build(&path, a.style, false);
    });
    let m = crate::vector_mesh::VectorMesh::build_draft(&path, a.style, false).unwrap();
    eprintln!(
        "draft verts {} tris {}",
        m.vertices.len(),
        m.indices.len() / 3
    );
}

#[test]
#[ignore]
fn mosaic_drag_over_many_lower_items() {
    use crate::annotation::{Annotation, Point, Style};
    let full = Rect::full(1920, 1080);
    let bytes: Vec<u8> = (0..1920 * 1080 * 4).map(|i| (i * 7 % 251) as u8).collect();
    let source = SourcePixels {
        fill: None,
        canvas: full,
        planes: vec![PixelPlane {
            rect: full,
            bgra: &bytes,
        }],
        overlay: None,
    };
    let style = Style {
        width: 4.,
        ..Default::default()
    };
    let shape = |tool: Tool, i: usize| {
        let x = (i * 97 % 1700) as f32;
        let y = (i * 53 % 900) as f32;
        let mut a = Annotation::new(tool, Point { x, y }, style);
        if tool == Tool::Pen {
            for k in 0..80 {
                let t = k as f32;
                a.update(Point {
                    x: x + t * 3.,
                    y: y + (t * 0.3).sin() * 40.,
                });
            }
        } else {
            a.update(Point {
                x: x + 180.,
                y: y + 120.,
            });
        }
        a
    };
    for (name, tool) in [
        ("pen", Tool::Pen),
        ("rect", Tool::Rectangle),
        ("arrow", Tool::Arrow),
        ("ellipse", Tool::Ellipse),
    ] {
        let mut items: Vec<_> = (0..300).map(|i| shape(tool, i)).collect();
        let mut mosaic = Annotation::new(
            Tool::Mosaic,
            Point { x: 200., y: 200. },
            Style {
                block_size: 12,
                ..Default::default()
            },
        );
        mosaic.update(Point { x: 700., y: 600. });
        items.push(mosaic);
        let mut scene = Scene::prepare(&items, &source);
        let n = 60;
        let started = std::time::Instant::now();
        for _ in 0..n {
            items.last_mut().unwrap().translate(3., 2.);
            scene = scene.updated(&items, &source);
        }
        eprintln!(
            "{name}: {:.2} ms / move",
            started.elapsed().as_secs_f64() * 1000. / n as f64
        );
    }
}
