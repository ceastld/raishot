use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture::Image,
    crop_mask::Polygon,
    editor::Editor,
    geometry::Rect,
    scene::{Block, Layer, PixelPlane, Scene, SourceIdentity, SourcePixels},
};
use gpui::RenderImage;
use std::sync::Arc;

const WIDTH: u32 = 32;
const HEIGHT: u32 = 20_000;

fn fixture_pixel(x: u32, y: u32, band: [u8; 4]) -> [u8; 4] {
    if (8176..8256).contains(&y) {
        band
    } else {
        [
            ((x * 17 + y * 13) % 251) as u8,
            ((x * 11 + y * 3) % 251) as u8,
            ((x * 7 + y * 31) % 251) as u8,
            [0, 128, 255][((x + y) % 3) as usize],
        ]
    }
}

fn fixture_image(band: [u8; 4]) -> Image {
    let mut bgra = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            bgra.extend_from_slice(&fixture_pixel(x, y, band));
        }
    }
    Image {
        width: WIDTH,
        height: HEIGHT,
        bgra,
        readback_ms: 0.,
    }
}

fn original_pixels<'a>(
    source: &'a CanvasSource,
    previews: &'a [Arc<RenderImage>],
) -> SourcePixels<'a> {
    // 与生产 CaptureView 的适配相同：显示 ROI 不参与原图 stride 或 Scene 身份。
    SourcePixels {
        fill: None,
        canvas: source.full(),
        planes: previews
            .iter()
            .enumerate()
            .map(|(index, preview)| PixelPlane {
                rect: source.original_rect(index),
                bgra: source.original(preview).as_bytes(0).unwrap(),
            })
            .collect(),
        overlay: None,
    }
}

fn assert_original_roi(source: &CanvasSource, roi: Rect, band: [u8; 4]) {
    let actual = source.read_region(roi).unwrap();
    assert_eq!((actual.width, actual.height), (roi.width, roi.height));
    for y in 0..roi.height {
        for x in 0..roi.width {
            let index = (y as usize * roi.width as usize + x as usize) * 4;
            assert_eq!(
                &actual.bgra[index..index + 4],
                &fixture_pixel(roi.x + x, roi.y + y, band),
                "原图小 ROI 必须保留隐藏 RGB/alpha，不能读缩略图或棋盘"
            );
        }
    }
}

fn annotation(tool: Tool, start: (f32, f32), end: (f32, f32), style: Style) -> Annotation {
    let mut item = Annotation::new(
        tool,
        Point {
            x: start.0,
            y: start.1,
        },
        style,
    );
    item.update(Point { x: end.0, y: end.1 });
    assert!(item.valid());
    item
}

fn mosaic(scene: &Scene) -> &Arc<Vec<Block>> {
    let Layer::Mosaic { blocks } = &scene.layers[0] else {
        panic!("下层应为真实马赛克采样缓存")
    };
    blocks
}

fn blur(scene: &Scene, index: usize) -> &Arc<Image> {
    let Layer::Raster { image, .. } = &scene.layers[index] else {
        panic!("模糊应为真实局部栅格缓存")
    };
    image
}

fn rendered_roi(scene: &Scene, source: &SourcePixels<'_>, roi: Rect) -> Image {
    let mut image = source.read(roi);
    scene.apply(&mut image, roi);
    image
}

fn assert_effect_band(scene: &Scene, source: &SourcePixels<'_>, band: [u8; 4]) {
    // 常色邻域的马赛克与 Blur 仍为该独立字面色；中、上层位于 x >= 16。
    let image = rendered_roi(
        scene,
        source,
        Rect {
            x: 6,
            y: 8196,
            width: 3,
            height: 3,
        },
    );
    for pixel in image.bgra.chunks_exact(4) {
        assert_eq!(pixel, &band);
    }
}

#[test]
fn long_image_editor_projected_source_identity_and_effects_survive_preview_replacement() {
    let band = [20, 60, 100, 255];
    let input = fixture_image(band);
    let raw_pointer = input.bgra.as_ptr();
    let raw_length = input.bgra.len();
    let (mut source, mut previews) = CanvasSource::editor_image(input).unwrap();
    let full = Rect::full(WIDTH, HEIGHT);
    assert_eq!(source.full(), full);
    assert_eq!(previews.len(), 1);
    assert_ne!(source.original(&previews[0]).id, previews[0].id);

    let annotations = vec![
        annotation(
            Tool::Mosaic,
            (2., 8192.),
            (14., 8204.),
            Style {
                block_size: 4,
                ..Default::default()
            },
        ),
        annotation(
            Tool::Blur,
            (4., 8194.),
            (12., 8202.),
            Style {
                block_size: 2,
                ..Default::default()
            },
        ),
        annotation(
            Tool::Rectangle,
            (18., 8196.),
            (26., 8204.),
            Style {
                filled: true,
                color: 0x1122dd,
                ..Default::default()
            },
        ),
        annotation(
            Tool::Blur,
            (16., 8192.),
            (28., 8206.),
            Style {
                block_size: 2,
                ..Default::default()
            },
        ),
    ];
    let crop = Rect {
        x: 1,
        y: 8188,
        width: 30,
        height: 44,
    };
    let mut editor = Editor::new(full, Some(crop));
    let ids = editor.document.insert_many(annotations.clone());
    editor.selected = Some(ids[2]);
    editor.selection = vec![ids[2]];
    editor.document.set_crop_mask(Polygon::new(&[
        Point { x: 1., y: 8178. },
        Point { x: 30., y: 8178. },
        Point { x: 30., y: 8240. },
        Point { x: 1., y: 8240. },
    ]));
    assert!(editor.document.crop_mask.is_some());
    editor.document.insert(annotation(
        Tool::Rectangle,
        (2., 16.),
        (16., 28.),
        Style::default(),
    ));
    editor.document.undo();
    assert!(editor.document.can_redo());
    let objects_before = editor.document.items().to_vec();
    let mask_before = editor.document.crop_mask.clone();
    let stamp_before = editor.scene_stamp();
    let edit_revision_before = editor.document.edit_revision();
    let selection_before = editor.selection.clone();

    let pixels = original_pixels(&source, &previews);
    assert_eq!(pixels.planes.len(), 1);
    assert_eq!(pixels.canvas, full);
    assert_eq!(pixels.planes[0].rect, full);
    assert_eq!(pixels.planes[0].bgra.as_ptr(), raw_pointer);
    assert_eq!(pixels.planes[0].bgra.len(), raw_length);
    let identity = SourceIdentity::of(&pixels);
    let mut scene = Scene::prepare(&editor.document_annotations(), &pixels);
    assert!(scene.errors.is_empty());
    assert_effect_band(&scene, &pixels, band);
    drop(pixels);

    for (roi, density) in [
        (
            Rect {
                x: 2,
                y: 8190,
                width: 28,
                height: 40,
            },
            1.,
        ),
        (
            Rect {
                x: 3,
                y: 8193,
                width: 27,
                height: 37,
            },
            0.375,
        ),
        (
            Rect {
                x: 4,
                y: 8195,
                width: 26,
                height: 34,
            },
            0.37,
        ),
    ] {
        let old_preview_id = previews[0].id;
        let next = source
            .refresh_shared_preview_at(roi, density)
            .unwrap()
            .unwrap();
        assert_ne!(next.id, old_preview_id);
        previews[0] = next;
        let pixels = original_pixels(&source, &previews);
        assert_eq!(pixels.planes[0].rect, full);
        assert_eq!(pixels.planes[0].bgra.as_ptr(), raw_pointer);
        assert_eq!(pixels.planes[0].bgra.len(), raw_length);
        assert!(SourceIdentity::of(&pixels) == identity);
        let updated = scene.updated(&editor.document_annotations(), &pixels);
        assert!(updated.errors.is_empty());
        assert!(Arc::ptr_eq(mosaic(&scene), mosaic(&updated)));
        assert!(Arc::ptr_eq(blur(&scene, 1), blur(&updated, 1)));
        assert!(Arc::ptr_eq(blur(&scene, 3), blur(&updated, 3)));
        assert_effect_band(&updated, &pixels, band);
        scene = updated;
        assert_original_roi(
            &source,
            Rect {
                x: 2,
                y: 4095,
                width: 5,
                height: 4,
            },
            band,
        );
        assert_original_roi(
            &source,
            Rect {
                x: 4,
                y: HEIGHT - 3,
                width: 3,
                height: 3,
            },
            band,
        );
        assert_eq!(editor.crop, Some(crop));
        assert_eq!(editor.document.crop_mask, mask_before);
        assert_eq!(editor.document.items(), objects_before.as_slice());
        assert_eq!(editor.scene_stamp(), stamp_before);
        assert_eq!(editor.document.edit_revision(), edit_revision_before);
        assert_eq!(editor.selected, Some(ids[2]));
        assert_eq!(editor.selection, selection_before);
        assert!(editor.document.can_redo());
    }

    let display_region = source.screen_rect(0);
    let display_bounds = source.preview_bounds(0);
    let display_id = previews[0].id;
    assert!(source
        .refresh_shared_preview_at(
            Rect {
                x: WIDTH,
                ..display_region
            },
            0.37
        )
        .is_err());
    assert!(source
        .refresh_shared_preview_at(display_region, f32::NAN)
        .is_err());
    assert_eq!(source.screen_rect(0), display_region);
    assert_eq!(source.preview_bounds(0), display_bounds);
    assert_eq!(previews[0].id, display_id);
    // 同一合法 key 仍为 no-op，证明失败没有推进不可见的内部刷新 key。
    assert!(source
        .refresh_shared_preview_at(display_region, 0.37)
        .unwrap()
        .is_none());
    let pixels = original_pixels(&source, &previews);
    assert!(SourceIdentity::of(&pixels) == identity);
    assert_eq!(pixels.planes[0].bgra.as_ptr(), raw_pointer);
    assert_eq!(pixels.planes[0].bgra.len(), raw_length);
    assert_eq!(pixels.planes[0].rect, full);
    assert_eq!(editor.document.items(), objects_before.as_slice());
    assert_eq!(editor.crop, Some(crop));
    assert_eq!(editor.document.crop_mask, mask_before);
    assert_eq!(editor.scene_stamp(), stamp_before);
    assert!(editor.document.can_redo());
    let after_rejected_refresh = scene.updated(&editor.document_annotations(), &pixels);
    assert!(Arc::ptr_eq(mosaic(&scene), mosaic(&after_rejected_refresh)));
    assert!(Arc::ptr_eq(
        blur(&scene, 1),
        blur(&after_rejected_refresh, 1)
    ));
    assert!(Arc::ptr_eq(
        blur(&scene, 3),
        blur(&after_rejected_refresh, 3)
    ));

    let upper_roi = Rect {
        x: 19,
        y: 8197,
        width: 4,
        height: 4,
    };
    let old_upper = rendered_roi(&scene, &pixels, upper_roi);
    let mut changed = annotations;
    changed[2].style.color = 0xdd2211;
    let changed_scene = scene.updated(&changed, &pixels);
    assert!(changed_scene.errors.is_empty());
    assert!(Arc::ptr_eq(mosaic(&scene), mosaic(&changed_scene)));
    assert!(Arc::ptr_eq(blur(&scene, 1), blur(&changed_scene, 1)));
    assert!(!Arc::ptr_eq(blur(&scene, 3), blur(&changed_scene, 3)));
    assert_ne!(
        old_upper.bgra,
        rendered_roi(&changed_scene, &pixels, upper_roi).bgra
    );
    assert_effect_band(&changed_scene, &pixels, band);
    drop(pixels);

    let replacement_band = [180, 90, 30, 255];
    let (replacement_source, replacement_previews) =
        CanvasSource::editor_image(fixture_image(replacement_band)).unwrap();
    let replacement_pixels = original_pixels(&replacement_source, &replacement_previews);
    assert!(SourceIdentity::of(&replacement_pixels) != identity);
    assert_ne!(replacement_pixels.planes[0].bgra.as_ptr(), raw_pointer);
    let replacement_scene = changed_scene.updated(&changed, &replacement_pixels);
    assert!(replacement_scene.errors.is_empty());
    assert!(!Arc::ptr_eq(
        mosaic(&changed_scene),
        mosaic(&replacement_scene)
    ));
    assert!(!Arc::ptr_eq(
        blur(&changed_scene, 1),
        blur(&replacement_scene, 1)
    ));
    assert!(!Arc::ptr_eq(
        blur(&changed_scene, 3),
        blur(&replacement_scene, 3)
    ));
    assert_effect_band(&replacement_scene, &replacement_pixels, replacement_band);
}
