//! 用户输出快照使用真实 Scene 与 CPU RenderImage，不创建窗口或读取桌面。
use super::*;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    selection_finish::SelectionFinishStyle,
};

fn image(width: u32, height: u32, pixel: [u8; 4]) -> Image {
    Image {
        width,
        height,
        bgra: pixel.repeat((width * height) as usize),
        readback_ms: 0.,
    }
}
fn appearance() -> SelectionFinishStyle {
    SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 6.25,
        shadow_enabled: true,
        shadow_blur: 4.25,
        shadow_color: 0xff12_3456,
        shadow_opacity: 0.4,
        border_enabled: true,
        border_color: 0x9076_5432,
        border_thickness: 1.75,
        border_inside: true,
        ..Default::default()
    }
}
fn snapshot(
    image: Image,
    rect: Rect,
    zoom: f32,
    style: Option<SelectionFinishStyle>,
    opacity: u8,
    annotations: &[Annotation],
    mask: Option<Polygon>,
) -> UserExportSnapshot {
    let canvas = Rect::full(image.width, image.height);
    let source_pixels = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &image.bgra,
        }],
        overlay: None,
    };
    let scene = Arc::new(Scene::prepare(annotations, &source_pixels));
    assert!(scene.errors.is_empty());
    let presentation = style.map(|style| PresentationOutputPlan::new(rect, style, zoom).unwrap());
    let projection = presentation.as_ref().map_or_else(
        || OutputProjection::new(rect, zoom).unwrap(),
        |presentation| presentation.scene_projection(),
    );
    UserExportSnapshot {
        projection,
        opacity,
        source: crate::pin_view::preview(image).unwrap(),
        fill: None,
        canvas,
        scene,
        mask,
        presentation,
        capture_pixels: None,
        cursor: None,
    }
}
fn pixel(image: &Image, x: u32, y: u32) -> &[u8] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    &image.bgra[i..i + 4]
}

#[test]
fn styled_user_snapshot_preserves_raw_and_applies_whole_output_opacity_once() {
    let raw = image(43, 31, [23, 71, 119, 128]);
    let raw_pixels = raw.bgra.clone();
    let style = appearance();
    let full = Rect::full(raw.width, raw.height);
    let snap = snapshot(raw, full, 1., Some(style), 128, &[], None);
    let mut expected = style
        .normalized()
        .compose(Image {
            width: full.width,
            height: full.height,
            bgra: raw_pixels.clone(),
            readback_ms: 0.,
        })
        .unwrap();
    window_opacity(&mut expected, 128);
    let rendered = snap.render().unwrap();
    assert_eq!((rendered.width, rendered.height), (53, 41));
    assert_eq!(rendered.bgra, expected.bgra);
    assert_eq!(snap.source.as_bytes(0).unwrap(), raw_pixels);
    assert_eq!(pixel(&rendered, 26, 20), [23, 71, 119, 64]);
    assert_eq!(pixel(&rendered, 0, 0), [0, 0, 0, 0]);
    assert_eq!(
        snap.render().unwrap().bgra,
        rendered.bgra,
        "重复输出不能重复压入外观"
    );
}

#[test]
fn border_placement_preserves_capture_dimensions_and_pixels_in_user_output() {
    let raw_pixel = [23, 71, 119, 255];
    for inside in [false, true] {
        let style = SelectionFinishStyle {
            border_enabled: true,
            border_inside: inside,
            border_thickness: 2.,
            border_color: 0xffff_0000,
            ..Default::default()
        };
        for zoom in [1., 2.] {
            let raw = image(7, 5, raw_pixel);
            let raw_pixels = raw.bgra.clone();
            let snap = snapshot(raw, Rect::full(7, 5), zoom, Some(style), 255, &[], None);
            let rendered = snap.render().unwrap();
            let scale = zoom as u32;
            let pad = if inside { 0 } else { 2 };
            assert_eq!(
                (rendered.width, rendered.height),
                ((7 + 2 * pad) * scale, (5 + 2 * pad) * scale)
            );
            assert_eq!(
                pixel(&rendered, (3 + pad) * scale, (2 + pad) * scale),
                raw_pixel
            );
            let edge = pixel(&rendered, pad * scale + scale - 1, (2 + pad) * scale);
            assert_eq!(edge, if inside { [0, 0, 255, 255] } else { raw_pixel });
            if !inside {
                assert_eq!(
                    pixel(&rendered, scale - 1, (2 + pad) * scale),
                    [0, 0, 255, 255]
                );
            }
            assert_eq!(snap.source.as_bytes(0).unwrap(), raw_pixels);
            if zoom == 1. {
                let expected = style.compose(image(7, 5, raw_pixel)).unwrap();
                assert_eq!(
                    rendered.bgra, expected.bgra,
                    "贴图输出与截图外观合成保持一致"
                );
            }
        }
    }
}

#[test]
fn styled_enlarged_user_snapshot_repaints_vector_in_whole_outer_grid() {
    let make = || image(37, 29, [255, 255, 255, 255]);
    let mut pen = Annotation::new(
        Tool::Pen,
        Point { x: 4.2, y: 5.1 },
        Style {
            width: 1.25,
            opacity: 0.6,
            color: 0x1278cd,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    pen.update(Point { x: 31.4, y: 23.6 });
    let rect = Rect::full(37, 29);
    let enlarged = snapshot(
        make(),
        rect,
        2.75,
        Some(appearance()),
        255,
        &[pen.clone()],
        None,
    )
    .render()
    .unwrap();
    // 只有原截图像素可以采样，不能把来源尺寸的装饰/标注当作可放大底图。
    let baked = snapshot(make(), rect, 1., Some(appearance()), 255, &[pen], None)
        .render()
        .unwrap();
    let baked_rect = Rect::full(baked.width, baked.height);
    let source = SourcePixels {
        fill: None,
        canvas: baked_rect,
        planes: vec![PixelPlane {
            rect: baked_rect,
            bgra: &baked.bgra,
        }],
        overlay: None,
    };
    let stretched = OutputProjection::new(baked_rect, 2.75)
        .unwrap()
        .background(&source)
        .unwrap();
    assert_eq!((enlarged.width, enlarged.height), (129, 107));
    let changed = enlarged
        .bgra
        .iter()
        .zip(&stretched.bgra)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 100,
        "目标分辨率的矢量边缘必须区别于来源合成图拉伸，实际{changed}"
    );
    assert!(enlarged
        .bgra
        .chunks_exact(4)
        .any(|p| p[3] > 0 && p[3] < 255));
}

#[test]
fn styled_crop_keeps_scene_mask_in_content_coordinates_and_clears_padding() {
    let raw = image(83, 67, [31, 67, 127, 255]);
    let rect = Rect {
        x: 17,
        y: 13,
        width: 41,
        height: 33,
    };
    let mask = Polygon::new(&[
        Point { x: 18.3, y: 14.2 },
        Point { x: 56.7, y: 15.1 },
        Point { x: 39.4, y: 43.8 },
    ])
    .unwrap();
    let mut mark = Annotation::new(
        Tool::Rectangle,
        Point { x: 30., y: 23. },
        Style {
            filled: true,
            color: 0x00ff00,
            ..Default::default()
        },
    );
    mark.update(Point { x: 44., y: 31. });
    let snap = snapshot(
        raw,
        rect,
        1.75,
        Some(appearance()),
        255,
        &[mark],
        Some(mask),
    );
    let exported = snap.render().unwrap();
    let target = snap.projection.map(Point { x: 37., y: 27. });
    assert_eq!(
        pixel(&exported, target.x.floor() as u32, target.y.floor() as u32),
        [0, 255, 0, 255]
    );
    assert_eq!(pixel(&exported, 0, 0), [0, 0, 0, 0]);
    let below_mask = snap.projection.map(Point { x: 21., y: 42. });
    assert_eq!(
        pixel(
            &exported,
            below_mask.x.floor() as u32,
            below_mask.y.floor() as u32
        ),
        [0, 0, 0, 0]
    );
    assert_eq!((exported.width, exported.height), (89, 75));
}

#[test]
fn legacy_user_snapshot_keeps_old_dimensions_and_solid_source_contract() {
    let rect = Rect::full(13, 7);
    let mut snap = snapshot(
        image(13, 7, [200, 150, 100, 255]),
        rect,
        1.5,
        None,
        255,
        &[],
        None,
    );
    assert_eq!((snap.projection.width, snap.projection.height), (20, 10));
    snap.fill = Some([17, 27, 37, 128]);
    let output = snap.render().unwrap();
    assert_eq!((output.width, output.height), (20, 10));
    assert!(output.bgra.chunks_exact(4).all(|p| p == [17, 27, 37, 128]));
    assert_eq!(u32::from(snap.source.size(0).width), 13);
}

#[test]
fn captured_user_output_uses_original_sampling_scene_cursor_and_mask_without_private_context_chunks(
) {
    use crate::pin_raster_document::capture_source::{
        resource,
        test_fixture::{Case, TestDir},
    };
    let case = Case::new();
    let raw = case.raw();
    let dir = TestDir::new("user-output");
    let scene = Arc::new(case.scene(&case.editor));
    let make = || UserExportSnapshot {
        projection: OutputProjection::new(case.selected, 1.).unwrap(),
        opacity: 255,
        source: crate::pin_view::preview(case.raw()).unwrap(),
        fill: None,
        canvas: case.source.full(),
        scene: scene.clone(),
        mask: case.editor.document.crop_mask.clone(),
        presentation: None,
        capture_pixels: Some(case.pixels.clone()),
        cursor: case.metadata().cursor_model(),
    };
    let exported = make().render().unwrap();
    assert_eq!(exported.bgra, case.output(&case.editor).bgra);
    assert_ne!(exported.bgra, raw.bgra);
    let mut styled = make();
    let plan = PresentationOutputPlan::new(case.selected, case.style, 2.25).unwrap();
    styled.projection = plan.scene_projection();
    styled.presentation = Some(plan);
    styled.opacity = 180;
    let mut source = case.pixels.source_pixels();
    source.overlay = Some(case.cursor.plane());
    let mut expected = plan
        .render(&source, &scene, case.editor.document.crop_mask.as_ref())
        .unwrap();
    window_opacity(&mut expected, 180);
    assert_eq!(styled.render().unwrap().bgra, expected.bgra);
    assert_eq!(
        styled.content_only().unwrap().render().unwrap().bgra,
        exported.bgra,
        "历史缓存只含选区内容与矢量，不带外观或私有源域"
    );
    let path = dir.0.join("export.png");
    crate::export::save_png_pixels(exported.width, exported.height, &exported.bgra, &path).unwrap();
    let image = crate::export::read_image(&path).unwrap();
    assert_eq!(image.bgra, exported.bgra);
    assert_eq!(
        (image.width, image.height),
        (case.selected.width, case.selected.height)
    );
    assert!(resource::load_context(&path, &case.metadata(), &raw).is_err());
}
