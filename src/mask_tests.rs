//! 套索草稿/环绕/alpha、原图恢复与场景依赖的行为回归。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    crop_mask::Polygon,
    editor::Editor,
    geometry::Rect,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn square() -> Vec<Point> {
    vec![p(10., 10.), p(50., 10.), p(50., 50.), p(10., 50.)]
}
fn editor() -> Editor {
    Editor::new(Rect::full(100, 100), Some(Rect::full(100, 100)))
}
fn draft(e: &mut Editor) {
    e.select_tool(Tool::Lasso);
    e.pointer_down((10, 10), 2.);
    e.pointer_move((50, 10));
    e.pointer_move((50, 50));
    e.pointer_up((10, 50));
}
fn background(canvas: Rect, color: [u8; 4]) -> Vec<u8> {
    color.repeat(canvas.width as usize * canvas.height as usize)
}
fn pixels<'a>(canvas: Rect, data: &'a [u8]) -> SourcePixels<'a> {
    SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: data,
        }],
        overlay: None,
    }
}
fn rectangle() -> Annotation {
    let mut a = Annotation::new(
        Tool::Rectangle,
        p(5., 5.),
        Style {
            filled: true,
            color: 0xff0000,
            ..Style::default()
        },
    );
    a.update(p(95., 95.));
    a
}
fn eraser() -> Annotation {
    let mut a = Annotation::new(
        Tool::PixelEraser,
        p(20., 50.),
        Style {
            width: 16.,
            ..Style::default()
        },
    );
    a.update(p(80., 50.));
    a
}
#[test]
fn nonzero_mask_keeps_double_wound_region_and_respects_reversed_hole() {
    let mut points = square();
    points.extend(square());
    let mask = Polygon::new(&points).unwrap();
    let coverage = mask.coverage(Rect::full(100, 100));
    assert_eq!(coverage[30 * 100 + 30], 255);
    assert_eq!(coverage[5 * 100 + 5], 0);
    let mesh_area: f32 = mask
        .triangles
        .iter()
        .map(|[a, b, c]| ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).abs() / 2.)
        .sum();
    assert!((mesh_area - 1600.).abs() < 0.01);
    let outer = vec![
        p(10., 10.),
        p(70., 10.),
        p(70., 70.),
        p(10., 70.),
        p(10., 10.),
        p(25., 25.),
        p(25., 55.),
        p(55., 55.),
        p(55., 25.),
        p(25., 25.),
        p(10., 10.),
    ];
    let mask = Polygon::new(&outer).unwrap();
    let alpha = mask.coverage(Rect::full(100, 100));
    assert_eq!(alpha[15 * 100 + 40], 255);
    assert_eq!(alpha[40 * 100 + 40], 0);
}
#[test]
fn mask_tight_bounds_use_full_canvas_origin_and_crop_pixel_identity() {
    let canvas = Rect::full(640, 480);
    let host = Rect {
        x: 300,
        y: 200,
        width: 280,
        height: 220,
    };
    let mask = Polygon::new(&[p(310.25, 210.25), p(530.75, 210.25), p(420.5, 390.75)]).unwrap();
    let rect = mask.output_rect(host).unwrap();
    assert_eq!(
        rect,
        Rect {
            x: 310,
            y: 210,
            width: 221,
            height: 181
        }
    );
    let data = background(canvas, [45, 70, 90, 180]);
    let source = pixels(canvas, &data);
    let mut whole = source.read(canvas);
    mask.apply(&mut whole, canvas);
    let mut cropped = source.read(rect);
    mask.apply(&mut cropped, rect);
    for y in 0..rect.height {
        for x in 0..rect.width {
            let a = (((rect.y + y) * canvas.width + rect.x + x) * 4) as usize;
            let b = ((y * rect.width + x) * 4) as usize;
            assert_eq!(whole.bgra[a..a + 4], cropped.bgra[b..b + 4]);
        }
    }
    assert!(cropped.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 180));
    assert!(cropped.bgra.chunks_exact(4).any(|p| p == [0, 0, 0, 0]));
}
#[test]
fn lasso_draft_apply_cancel_clear_and_annotation_undo_keep_chronological_history() {
    let mut e = editor();
    let id = e.document.insert(rectangle());
    draft(&mut e);
    assert!(e.document.crop_mask.is_none());
    assert!(e.lasso_draft.is_some());
    assert_eq!(e.document.items().len(), 1);
    e.cancel_gesture();
    assert!(e.document.crop_mask.is_none());
    assert_eq!(e.document.items().len(), 1);
    draft(&mut e);
    assert!(e.apply_lasso());
    let mask = e.document.crop_mask.clone();
    e.select_tool(Tool::Pen);
    e.pointer_down((75, 75), 2.);
    e.pointer_up((90, 90));
    assert_eq!(e.document.items().len(), 2);
    e.undo();
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.crop_mask, mask);
    e.undo();
    assert!(e.document.crop_mask.is_none());
    e.redo();
    assert_eq!(e.document.crop_mask, mask);
    e.clear_lasso();
    assert!(e.document.crop_mask.is_none());
    e.undo();
    assert_eq!(e.document.crop_mask, mask);
    assert!(e.document.get(id).is_some());
}
#[test]
fn cancelled_lasso_and_noop_preserve_redo_and_changed_host_drops_only_mask_history() {
    let mut e = editor();
    e.document.insert(rectangle());
    draft(&mut e);
    e.apply_lasso();
    e.document.insert(eraser());
    e.undo();
    assert!(e.document.can_redo());
    draft(&mut e);
    e.cancel_gesture();
    assert!(e.document.can_redo());
    let applied = e.document.crop_mask.clone();
    e.document.set_crop_mask(applied);
    assert!(e.document.can_redo());
    e.set_crop(Some(Rect {
        x: 2,
        y: 2,
        width: 94,
        height: 94,
    }));
    assert!(e.document.crop_mask.is_none());
    e.redo();
    assert_eq!(e.document.items().len(), 2);
    e.undo();
    e.undo();
    assert!(e.document.items().is_empty());
}
#[test]
fn lasso_pointer_clamps_host_and_outside_drag_reselects_without_corrupting_objects() {
    let mut e = editor();
    e.crop = Some(Rect {
        x: 10,
        y: 10,
        width: 50,
        height: 50,
    });
    draft(&mut e);
    assert!(e.apply_lasso());
    let mask = e.document.crop_mask.clone();
    e.pointer_down((20, 20), 2.);
    e.pointer_move((100, 20));
    e.pointer_move((100, 100));
    e.pointer_up((20, 100));
    let points = &e.lasso_draft.as_ref().unwrap().points;
    assert!(points
        .iter()
        .all(|p| p.x >= 10. && p.x <= 60. && p.y >= 10. && p.y <= 60.));
    e.cancel_gesture();
    assert_eq!(e.document.crop_mask, mask);
    e.pointer_down((80, 80), 2.);
    e.pointer_up((95, 95));
    assert_eq!(
        e.crop,
        Some(Rect {
            x: 80,
            y: 80,
            width: 15,
            height: 15
        })
    );
    assert!(e.document.crop_mask.is_none());
}
#[test]
fn restore_erases_only_local_lower_ink_and_upper_ink_remains_visible() {
    let canvas = Rect::full(100, 100);
    let data = background(canvas, [30, 70, 140, 255]);
    let source = pixels(canvas, &data);
    let mut top = Annotation::new(
        Tool::Pen,
        p(50., 20.),
        Style {
            width: 5.,
            color: 0x00ff00,
            ..Style::default()
        },
    );
    top.update(p(50., 80.));
    let scene = Scene::prepare(&[rectangle(), eraser(), top], &source);
    assert!(matches!(scene.layers[1], Layer::Restore { .. }));
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    let pixel = |x: usize, y: usize| &image.bgra[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
    assert_eq!(pixel(30, 50), [30, 70, 140, 255]);
    assert_eq!(pixel(30, 30), [0, 0, 255, 255]);
    assert_eq!(pixel(50, 50), [0, 255, 0, 255]);
    let crop = Rect {
        x: 25,
        y: 25,
        width: 60,
        height: 60,
    };
    let mut part = source.read(crop);
    scene.apply(&mut part, crop);
    for y in 0..crop.height {
        for x in 0..crop.width {
            let a = (((y + crop.y) * 100 + x + crop.x) * 4) as usize;
            let b = ((y * crop.width + x) * 4) as usize;
            assert_eq!(image.bgra[a..a + 4], part.bgra[b..b + 4]);
        }
    }
}
#[test]
fn restore_handles_transparent_source_and_layer_cache_dependency() {
    let canvas = Rect::full(100, 100);
    let data = background(canvas, [0, 0, 0, 0]);
    let source = pixels(canvas, &data);
    let lower = rectangle();
    let restore = eraser();
    let first = Scene::prepare(&[lower.clone(), restore.clone()], &source);
    let mut image = source.read(canvas);
    first.apply(&mut image, canvas);
    assert_eq!(
        &image.bgra[(50 * 100 + 30) * 4..(50 * 100 + 30) * 4 + 4],
        &[0, 0, 0, 0]
    );
    let upper = Annotation::new(Tool::Pen, p(70., 70.), Style::default());
    let next = first.updated(&[lower.clone(), restore.clone(), upper], &source);
    let Layer::Restore { image: before, .. } = &first.layers[1] else {
        panic!()
    };
    let Layer::Restore { image: after, .. } = &next.layers[1] else {
        panic!()
    };
    assert!(std::sync::Arc::ptr_eq(before, after));
    let mut changed = lower;
    changed.style.color = 0x0000ff;
    let new = next.updated(&[changed, restore], &source);
    let Layer::Restore { image: after, .. } = &new.layers[1] else {
        panic!()
    };
    assert!(!std::sync::Arc::ptr_eq(before, after));
}
#[test]
fn restore_gesture_wheel_overlap_rotation_and_undo_preserve_original_objects() {
    let mut e = editor();
    let lower = e.document.insert(rectangle());
    e.select_tool(Tool::PixelEraser);
    e.pointer_down((20, 50), 2.);
    e.pointer_move((50, 50));
    e.set_width(24.);
    e.pointer_move((20, 50));
    e.pointer_up((80, 50));
    assert_eq!(e.document.items().len(), 2);
    let restore = e.document.items()[1].annotation.clone();
    assert_eq!(restore.style.width, 24.);
    assert!(e.document.get(lower).is_some());
    e.undo();
    assert_eq!(e.document.items().len(), 1);
    e.redo();
    assert_eq!(e.document.items()[1].annotation, restore);
    let canvas = Rect::full(100, 100);
    let data = background(canvas, [30, 70, 140, 255]);
    let source = pixels(canvas, &data);
    let mut restore = restore;
    restore.rotate_about(p(50., 50.), 90.);
    let scene = Scene::prepare(&[rectangle(), restore], &source);
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    assert_eq!(
        &image.bgra[(30 * 100 + 50) * 4..(30 * 100 + 50) * 4 + 4],
        &[30, 70, 140, 255]
    );
    assert_eq!(
        &image.bgra[(50 * 100 + 20) * 4..(50 * 100 + 20) * 4 + 4],
        &[0, 0, 255, 255]
    );
}
#[test]
fn mosaic_samples_restored_original_instead_of_hidden_annotations() {
    let canvas = Rect::full(100, 100);
    let data = background(canvas, [30, 70, 140, 255]);
    let source = pixels(canvas, &data);
    let mut restore = eraser();
    restore.style.width = 40.;
    let mut mosaic = Annotation::new(
        Tool::Mosaic,
        p(30., 40.),
        Style {
            block_size: 8,
            ..Style::default()
        },
    );
    mosaic.update(p(70., 60.));
    let scene = Scene::prepare(&[rectangle(), restore, mosaic], &source);
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    assert_eq!(
        &image.bgra[(50 * 100 + 40) * 4..(50 * 100 + 40) * 4 + 4],
        &[30, 70, 140, 255]
    );
}
#[test]
fn invalid_tiny_or_collinear_lasso_never_commits() {
    assert!(Polygon::new(&[p(1., 1.), p(4., 4.), p(6., 6.)]).is_none());
    assert!(Polygon::new(&[p(1., 1.), p(50., 1.), p(50., 1.)]).is_none());
    assert!(Polygon::new(&[p(1., 1.), p(f32::NAN, 20.), p(30., 50.)]).is_none());
    let mut e = editor();
    e.select_tool(Tool::Lasso);
    e.pointer_down((10, 10), 2.);
    e.pointer_up((30, 30));
    assert!(e.lasso_draft.is_none());
    assert!(!e.document.can_undo());
}

#[test]
fn transparent_source_keeps_straight_alpha_when_vector_and_raster_overlap() {
    let mut dst = [0u8; 4];
    crate::raster::over_pixel(&mut dst, &[20, 80, 160], 0.5);
    assert_eq!(dst, [20, 80, 160, 128]);
    crate::raster::over_pixel(&mut dst, &[180, 40, 20], 0.5);
    assert!((dst[3] as i32 - 192).abs() <= 1);
    assert!((dst[0] as i32 - 126).abs() <= 1);
    let canvas = Rect::full(100, 100);
    let data = background(canvas, [0, 0, 0, 0]);
    let source = pixels(canvas, &data);
    let mut pen = Annotation::new(
        Tool::Pen,
        p(20., 50.),
        Style {
            width: 6.,
            color: 0x123456,
            ..Style::default()
        },
    );
    pen.update(p(80., 50.));
    let scene = Scene::prepare(&[pen], &source);
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    assert_eq!(
        &image.bgra[(50 * 100 + 40) * 4..(50 * 100 + 40) * 4 + 4],
        &[0x56, 0x34, 0x12, 255]
    );
}

#[test]
fn clear_batches_mask_and_objects_and_recognition_matte_preserves_edges() {
    let mut e = editor();
    e.document.insert(rectangle());
    draft(&mut e);
    e.apply_lasso();
    let mask = e.document.crop_mask.clone();
    e.clear();
    assert!(e.document.items().is_empty() && e.document.crop_mask.is_none());
    e.undo();
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.crop_mask, mask);
    e.redo();
    assert!(e.document.items().is_empty() && e.document.crop_mask.is_none());
    let mut data = vec![20, 80, 160, 128, 0, 0, 0, 0, 30, 70, 90, 255];
    crate::export::flatten_on_white(&mut data);
    assert_eq!(
        data,
        [137, 167, 207, 255, 255, 255, 255, 255, 30, 70, 90, 255]
    );
}

#[test]
fn large_canvas_restoration_uses_global_physical_points_after_live_wheel() {
    let canvas = Rect::full(4480, 1440);
    let mut e = Editor::new(
        canvas,
        Some(Rect {
            x: 250,
            y: 100,
            width: 900,
            height: 580,
        }),
    );
    let mut lower = rectangle();
    lower.points = vec![p(470., 240.), p(1030., 560.)];
    e.document.insert(lower);
    e.select_tool(Tool::PixelEraser);
    e.pointer_down((530, 370), 6.);
    e.pointer_move((730, 370));
    assert!(e.adjust_parameter(crate::interaction::Parameter::Width, 1, 10, (650, 350), 6.));
    e.pointer_move((900, 370));
    e.pointer_up((900, 370));
    let data = background(canvas, [247, 247, 247, 255]);
    let source = pixels(canvas, &data);
    let scene = Scene::prepare(
        &e.document
            .items()
            .iter()
            .map(|o| o.annotation.clone())
            .collect::<Vec<_>>(),
        &source,
    );
    let output = Rect {
        x: 250,
        y: 100,
        width: 900,
        height: 580,
    };
    let mut image = source.read(output);
    scene.apply(&mut image, output);
    for x in [600, 650, 750] {
        let i = ((270 * 900 + x - 250) * 4) as usize;
        assert_eq!(&image.bgra[i..i + 4], &[247, 247, 247, 255]);
    }
}

#[test]
fn restoration_tool_remains_brush_over_older_strokes_but_edit_can_move_them() {
    let mut e = editor();
    e.select_tool(Tool::PixelEraser);
    e.pointer_down((20, 50), 2.);
    e.pointer_up((80, 50));
    assert!(e.selected.is_none());
    assert_eq!(e.hover((40, 50), 2.), crate::interaction::Hover::Crosshair);
    e.pointer_down((40, 50), 2.);
    e.pointer_up((60, 70));
    assert_eq!(e.document.items().len(), 2);
    e.undo();
    e.select_tool(Tool::Edit);
    e.pointer_down((40, 50), 2.);
    e.pointer_up((45, 60));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.items()[0].annotation.points[0], p(25., 60.));
    e.undo();
    assert_eq!(e.document.items()[0].annotation.points[0], p(20., 50.));
}
