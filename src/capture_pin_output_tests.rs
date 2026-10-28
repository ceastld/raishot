//! 自有 CPU 合成图验证显式范围导出；不覆盖真实贴图首显、UIA 或剪贴板。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture::Image,
    capture_pin_output::{content_image, finished_origin, output_rect},
    crop_mask::Polygon,
    cursor::CapturedCursor,
    document::{Object, ObjectId},
    editor::Editor,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, Scene, SourcePixels},
    selection_finish::SelectionFinishStyle,
};

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}

fn fixture(
    width: u32,
    height: u32,
    mut color: impl FnMut(u32, u32) -> [u8; 4],
) -> (CanvasSource, Image) {
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            bgra.extend_from_slice(&color(x, y));
        }
    }
    let original = Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    };
    let (source, _) = CanvasSource::image(Image {
        width,
        height,
        bgra: original.bgra.clone(),
        readback_ms: 0.,
    })
    .unwrap();
    (source, original)
}

fn gradient(width: u32, height: u32) -> (CanvasSource, Image) {
    fixture(width, height, |x, y| [x as u8, y as u8, (x + y) as u8, 255])
}

fn source_pixels<'a>(image: &'a Image, cursor: Option<&'a CapturedCursor>) -> SourcePixels<'a> {
    SourcePixels {
        fill: None,
        canvas: Rect::full(image.width, image.height),
        planes: vec![PixelPlane {
            rect: Rect::full(image.width, image.height),
            bgra: &image.bgra,
        }],
        overlay: cursor.map(CapturedCursor::plane),
    }
}

fn rectangle(bounds: Rect, color: u32) -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Rectangle,
        point(bounds.x as f32, bounds.y as f32),
        Style {
            color,
            width: 0.,
            opacity: 1.,
            filled: true,
            radius: 0.,
            ..Style::default()
        },
    );
    annotation.update(point(
        (bounds.x + bounds.width) as f32,
        (bounds.y + bounds.height) as f32,
    ));
    annotation
}

fn rectangle_mask(bounds: Rect) -> Polygon {
    Polygon::new(&[
        point(bounds.x as f32, bounds.y as f32),
        point((bounds.x + bounds.width) as f32, bounds.y as f32),
        point(
            (bounds.x + bounds.width) as f32,
            (bounds.y + bounds.height) as f32,
        ),
        point(bounds.x as f32, (bounds.y + bounds.height) as f32),
    ])
    .unwrap()
}

fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let offset = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[offset..offset + 4].try_into().unwrap()
}

fn crop_bytes(image: &Image, region: Rect) -> Vec<u8> {
    assert!(region.valid(image.width, image.height));
    let mut bytes = Vec::new();
    for y in region.y..region.y + region.height {
        let offset = (y as usize * image.width as usize + region.x as usize) * 4;
        bytes.extend_from_slice(&image.bgra[offset..offset + region.width as usize * 4]);
    }
    bytes
}

#[derive(Debug, PartialEq)]
struct EditorSnapshot {
    crop: Option<Rect>,
    tool: Tool,
    style: Style,
    selected: Option<ObjectId>,
    selection: Vec<ObjectId>,
    scene_stamp: (u64, u64),
    edit_revision: u64,
    can_undo: bool,
    can_redo: bool,
    objects: Vec<Object>,
    crop_mask: Option<Polygon>,
}

fn editor_snapshot(editor: &Editor) -> EditorSnapshot {
    EditorSnapshot {
        crop: editor.crop,
        tool: editor.tool,
        style: editor.style,
        selected: editor.selected,
        selection: editor.selection.clone(),
        scene_stamp: editor.scene_stamp(),
        edit_revision: editor.document.edit_revision(),
        can_undo: editor.document.can_undo(),
        can_redo: editor.document.can_redo(),
        objects: editor.document.items().to_vec(),
        crop_mask: editor.document.crop_mask.clone(),
    }
}

#[test]
fn capture_pin_output_rejects_overflowing_host_before_mask_geometry() {
    let canvas = Rect::full(64, 48);
    let mask = rectangle_mask(rect(4, 4, 24, 20));
    for host in [
        rect(u32::MAX - 1, 4, 4, 8),
        rect(4, u32::MAX - 1, 8, 4),
        rect(4, 4, 0, 8),
        rect(4, 4, 8, 0),
        rect(60, 4, 8, 8),
    ] {
        assert!(output_rect(canvas, host, Some(&mask)).is_err(), "{host:?}");
        assert!(output_rect(canvas, host, None).is_err(), "{host:?}");
    }
    assert!(output_rect(canvas, rect(40, 30, 8, 8), Some(&mask)).is_err());
}

#[test]
fn capture_pin_output_mask_intersection_drives_content_and_negative_origin() {
    let (source, original) = gradient(64, 48);
    let host = rect(8, 6, 16, 14);
    let mask = rectangle_mask(rect(4, 4, 12, 16));
    let selected = output_rect(source.full(), host, Some(&mask)).unwrap();
    assert_eq!(selected, rect(8, 6, 8, 14));
    let output = content_image(&source, &Scene::default(), selected, None, Some(&mask)).unwrap();
    assert_eq!((output.width, output.height), (8, 14));
    assert_eq!(output.bgra, crop_bytes(&original, selected));
    assert_eq!(pixel(&output, 0, 0), [8, 6, 14, 255]);
    let bounds = ScreenRect {
        left: -1920,
        top: -240,
        width: 64,
        height: 48,
    };
    assert_eq!(
        finished_origin(bounds, selected, SelectionFinishStyle::default()),
        (-1912, -234)
    );
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
}

#[test]
fn capture_pin_output_scene_crop_keeps_fullscreen_annotations_and_undo_redo() {
    let (source, original) = fixture(64, 48, |_, _| [20, 40, 60, 255]);
    let mut editor = Editor::new(source.full(), None);
    let inside = editor
        .document
        .insert(rectangle(rect(18, 14, 16, 12), 0xe01020));
    editor
        .document
        .insert(rectangle(rect(40, 28, 18, 16), 0x1020d0));
    let redo_annotation = rectangle(rect(2, 2, 8, 8), 0x40b060);
    editor.document.insert(redo_annotation.clone());
    editor.document.undo();
    editor.tool = Tool::Edit;
    editor.selected = Some(inside);
    editor.selection = vec![inside];
    let before = editor_snapshot(&editor);
    assert!(before.can_undo && before.can_redo);
    let scene = Scene::prepare(
        &editor.document_annotations(),
        &source_pixels(&original, None),
    );
    assert!(scene.errors.is_empty(), "{:?}", scene.errors);
    let full = content_image(&source, &scene, source.full(), None, None).unwrap();
    let selected = rect(12, 10, 30, 24);
    let cropped = content_image(&source, &scene, selected, None, None).unwrap();
    assert_eq!(cropped.bgra, crop_bytes(&full, selected));
    assert_eq!(
        pixel(&cropped, 24 - selected.x, 20 - selected.y),
        [0x20, 0x10, 0xe0, 255]
    );
    assert_eq!(pixel(&full, 45, 35), [0xd0, 0x20, 0x10, 255]);
    assert_eq!(editor_snapshot(&editor), before);
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
    editor.document.redo();
    assert_eq!(
        editor.document.items().last().unwrap().annotation,
        redo_annotation
    );
    editor.document.undo();
    assert_eq!(editor.document.items(), before.objects);
}

#[test]
fn capture_pin_output_mask_preserves_straight_alpha_and_clears_hidden_rgb() {
    let (source, original) = fixture(32, 24, |x, y| {
        let alpha = match (x, y) {
            (6, 6) => 128,
            (7, 6) => 0,
            _ => 255,
        };
        [40, 80, 120, alpha]
    });
    let mask = Polygon::new(&[point(4.25, 4.25), point(27.75, 4.25), point(4.25, 19.75)]).unwrap();
    let selected = output_rect(source.full(), source.full(), Some(&mask)).unwrap();
    assert_eq!(selected, rect(4, 4, 24, 16));
    let output = content_image(&source, &Scene::default(), selected, None, Some(&mask)).unwrap();
    assert_eq!(
        pixel(&output, 6 - selected.x, 6 - selected.y),
        [40, 80, 120, 128]
    );
    assert_eq!(pixel(&output, 7 - selected.x, 6 - selected.y), [0, 0, 0, 0]);
    assert_eq!(
        pixel(&output, 26 - selected.x, 18 - selected.y),
        [0, 0, 0, 0]
    );
    let edge = pixel(&output, 4 - selected.x, 6 - selected.y);
    assert_eq!(&edge[..3], &[40, 80, 120]);
    assert!(
        edge[3] > 0 && edge[3] < 255,
        "边缘应保留部分覆盖率：{edge:?}"
    );
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
}

#[test]
fn capture_pin_output_cursor_scene_mask_order_matches_frozen_physical_coordinates() {
    let (source, original) = fixture(32, 24, |_, _| [10, 20, 30, 255]);
    let cursor = CapturedCursor {
        rect: rect(8, 6, 16, 12),
        image: Image {
            width: 16,
            height: 12,
            bgra: [200, 40, 10, 255].repeat(16 * 12),
            readback_ms: 0.,
        },
    };
    let cursor_before = cursor.image.bgra.clone();
    let scene = Scene::prepare(
        &[rectangle(rect(10, 8, 8, 6), 0x208040)],
        &source_pixels(&original, Some(&cursor)),
    );
    assert!(scene.errors.is_empty(), "{:?}", scene.errors);
    let mask = Polygon::new(&[point(4., 4.), point(28., 4.), point(4., 20.)]).unwrap();
    let selected = output_rect(source.full(), source.full(), Some(&mask)).unwrap();
    let output = content_image(&source, &scene, selected, Some(&cursor), Some(&mask)).unwrap();
    assert_eq!(
        pixel(&output, 12 - selected.x, 10 - selected.y),
        [64, 128, 32, 255]
    );
    assert_eq!(
        pixel(&output, 20 - selected.x, 7 - selected.y),
        [200, 40, 10, 255]
    );
    assert_eq!(
        pixel(&output, 6 - selected.x, 6 - selected.y),
        [10, 20, 30, 255]
    );
    assert_eq!(
        pixel(&output, 20 - selected.x, 16 - selected.y),
        [0, 0, 0, 0]
    );
    assert_eq!(cursor.image.bgra, cursor_before);
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
}

#[test]
fn capture_pin_output_scene_error_and_invalid_region_produce_no_image_or_source_change() {
    let (source, original) = gradient(32, 24);
    let mut failed = Scene::default();
    failed.errors.push("自有合成场景失败".into());
    let error = content_image(&source, &failed, rect(4, 4, 8, 8), None, None)
        .err()
        .unwrap();
    assert!(error.to_string().contains("自有合成场景失败"));
    for selected in [
        rect(u32::MAX - 1, 4, 4, 8),
        rect(30, 4, 8, 8),
        rect(4, 4, 8, 0),
    ] {
        assert!(content_image(&source, &Scene::default(), selected, None, None).is_err());
    }
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
}

#[test]
fn capture_pin_output_origin_and_composed_padding_keep_same_content_anchor() {
    let (source, original) = gradient(64, 48);
    let selected = rect(8, 6, 16, 12);
    let bounds = ScreenRect {
        left: -1920,
        top: -240,
        width: 64,
        height: 48,
    };
    let decorated = SelectionFinishStyle {
        shadow_enabled: true,
        shadow_blur: 4.1,
        shadow_opacity: 0.5,
        border_enabled: true,
        border_thickness: 2.2,
        ..SelectionFinishStyle::default()
    };
    let inside_border = SelectionFinishStyle {
        border_inside: true,
        shadow_enabled: false,
        ..decorated
    };
    let invisible = SelectionFinishStyle {
        shadow_opacity: 0.,
        border_color: 0x00ff_ffff,
        ..decorated
    };
    for (style, expected_pad) in [(decorated, 5), (inside_border, 0), (invisible, 0)] {
        let output = style
            .compose(content_image(&source, &Scene::default(), selected, None, None).unwrap())
            .unwrap();
        let origin = finished_origin(bounds, selected, style);
        assert_eq!(
            origin,
            (-1912 - expected_pad as i32, -234 - expected_pad as i32)
        );
        assert_eq!(
            (
                origin.0 + expected_pad as i32,
                origin.1 + expected_pad as i32
            ),
            (-1912, -234)
        );
        assert_eq!(
            (output.width, output.height),
            (16 + expected_pad * 2, 12 + expected_pad * 2)
        );
        assert_eq!(
            pixel(&output, expected_pad + 8, expected_pad + 6),
            pixel(&original, 16, 12)
        );
    }
    assert_eq!(
        source.read_region(source.full()).unwrap().bgra,
        original.bgra
    );
}
