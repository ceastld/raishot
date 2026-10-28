//! 水印元数据与真实 Windows GDI 分块重绘；不为长图构造第二幅全图期望。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    output_projection::OutputProjection,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn watermark(canvas: Rect) -> Annotation {
    let mut a = Annotation::new(
        Tool::Watermark,
        Point { x: 2.25, y: 3.75 },
        Style {
            color: 0xe53971,
            font_size: 20.,
            opacity: 0.61,
            ..Style::default()
        },
    );
    a.points.push(Point {
        x: canvas.width as f32 - 2.25,
        y: canvas.height as f32 - 3.75,
    });
    a.text = "水印 Phase QA".into();
    a
}

fn base(canvas: Rect) -> Image {
    Image {
        width: canvas.width,
        height: canvas.height,
        bgra: [31, 71, 109, 255].repeat(canvas.width as usize * canvas.height as usize),
        readback_ms: 0.,
    }
}

fn source(image: &Image) -> SourcePixels<'_> {
    SourcePixels {
        fill: None,
        canvas: Rect::full(image.width, image.height),
        planes: vec![PixelPlane {
            rect: Rect::full(image.width, image.height),
            bgra: &image.bgra,
        }],
        overlay: None,
    }
}

fn projected(scene: &Scene, source: &SourcePixels<'_>, rect: Rect, zoom: f32) -> Image {
    let projection = OutputProjection::new(rect, zoom).unwrap();
    let mut image = projection.background(source).unwrap();
    scene
        .apply_projected(&mut image, source, &projection)
        .unwrap();
    image
}

#[test]
fn watermark_scene_long_canvas_is_metadata_and_blank_keeps_layer_without_raster() {
    let canvas = Rect::full(960, 32_000);
    // 几何与源身份检查不要求分配 117MiB 像素；空 planes 是生产允许的空源。
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: Vec::new(),
        overlay: None,
    };
    let a = watermark(canvas);
    let scene = Scene::prepare(std::slice::from_ref(&a), &source);
    assert!(scene.errors.is_empty());
    let [Layer::Watermark { annotation }] = scene.layers.as_slice() else {
        panic!("长图水印不应缓存全画布 Raster");
    };
    assert_eq!(annotation.as_ref(), &a);
    let same = scene.updated(std::slice::from_ref(&a), &source);
    let Layer::Watermark { annotation: cached } = &same.layers[0] else {
        panic!("没有共享元数据层");
    };
    assert!(Arc::ptr_eq(annotation, cached));
    let mut changed = a.clone();
    changed.text = "下一字".into();
    let next = scene.updated(std::slice::from_ref(&changed), &source);
    let Layer::Watermark {
        annotation: replaced,
    } = &next.layers[0]
    else {
        panic!("实时文字不得转换回 Raster");
    };
    assert!(!Arc::ptr_eq(annotation, replaced));
    changed.text = " \r\n ".into();
    let blank = next.updated(std::slice::from_ref(&changed), &source);
    assert!(matches!(blank.layers.as_slice(), [Layer::Empty]));
    assert_eq!(blank.annotations, vec![changed]);
    let roi = Rect {
        x: 32,
        y: 8000,
        width: 16,
        height: 16,
    };
    let mut image = base(Rect::full(16, 16));
    let before = image.bgra.clone();
    blank.try_apply(&mut image, roi).unwrap();
    assert_eq!(image.bgra, before);
}

#[test]
fn watermark_scene_identity_and_enlarged_crop_keep_negative_rotated_source_phase() {
    let canvas = Rect::full(700, 400);
    let image = base(canvas);
    let source = source(&image);
    let crop = Rect {
        x: 100,
        y: 40,
        width: 350,
        height: 260,
    };
    let mut a = watermark(canvas);
    a.points[0] = Point { x: -2.25, y: -3.75 };
    a.points[1] = Point {
        x: 697.75,
        y: 396.25,
    };
    a.style.text.outline = true;
    a.style.text.outline_width = 4.75;
    for rotation in [0., 13., -17.] {
        a.rotation = rotation;
        let scene = Scene::prepare(std::slice::from_ref(&a), &source);
        assert!(matches!(scene.layers[0], Layer::Watermark { .. }));
        for zoom in [1., 1.5] {
            let full = projected(&scene, &source, canvas, zoom);
            let part = projected(&scene, &source, crop, zoom);
            let x = (crop.x as f32 * zoom) as usize;
            let y = (crop.y as f32 * zoom) as usize;
            for row in 0..part.height as usize {
                let start = ((row + y) * full.width as usize + x) * 4;
                assert_eq!(
                    &part.bgra[row * part.width as usize * 4..(row + 1) * part.width as usize * 4],
                    &full.bgra[start..start + part.width as usize * 4],
                    "{rotation}° / {zoom}× 的源相位在裁剪后改变，第 {row} 行"
                );
            }
            assert!(full.bgra.chunks_exact(4).any(|p| p != [31, 71, 109, 255]));
            assert!(full.bgra.chunks_exact(4).all(|p| p[3] == 255));
        }
    }
}

#[test]
fn watermark_scene_downstream_mosaic_samples_real_watermark_and_reuses_only_valid_prefix() {
    let canvas = Rect::full(320, 240);
    let image = base(canvas);
    let source = source(&image);
    let a = watermark(canvas);
    let below = Scene::prepare(std::slice::from_ref(&a), &source);
    let mut expected = base(canvas);
    below.try_apply(&mut expected, canvas).unwrap();
    assert_ne!(expected.bgra, image.bgra);
    let mut mosaic = Annotation::new(Tool::Mosaic, Point { x: 0., y: 0. }, Style::default());
    mosaic.points.push(Point { x: 320., y: 240. });
    mosaic.style.block_size = 16;
    let mut pen = Annotation::new(Tool::Pen, Point { x: 5., y: 5. }, Style::default());
    pen.points.push(Point { x: 15., y: 5. });
    let scene = Scene::prepare(&[a.clone(), mosaic.clone(), pen.clone()], &source);
    let Layer::Mosaic { blocks } = &scene.layers[1] else {
        panic!("水印之后没有实际 Mosaic 色块");
    };
    for block in blocks.iter() {
        let mut sums = [0_u64; 3];
        for y in block.rect.y..block.rect.y + block.rect.height {
            for x in block.rect.x..block.rect.x + block.rect.width {
                let i = (y as usize * expected.width as usize + x as usize) * 4;
                for (c, sum) in sums.iter_mut().enumerate() {
                    *sum += expected.bgra[i + c] as u64;
                }
            }
        }
        let count = u64::from(block.rect.width) * u64::from(block.rect.height);
        let actual = [
            block.color as u8,
            (block.color >> 8) as u8,
            (block.color >> 16) as u8,
        ];
        assert_eq!(actual, sums.map(|sum| (sum / count) as u8));
    }
    pen.style.color ^= 0x00ff00;
    let upper = scene.updated(&[a.clone(), mosaic.clone(), pen.clone()], &source);
    let Layer::Mosaic { blocks: reused } = &upper.layers[1] else {
        panic!("Mosaic 消失");
    };
    assert!(Arc::ptr_eq(blocks, reused));
    let mut new_a = a;
    new_a.text = "变更下层文字".into();
    let lower = upper.updated(&[new_a, mosaic, pen], &source);
    let Layer::Mosaic {
        blocks: invalidated,
    } = &lower.layers[1]
    else {
        panic!("Mosaic 消失");
    };
    assert!(!Arc::ptr_eq(reused, invalidated));
}

#[test]
fn watermark_scene_invalid_metadata_and_runtime_budget_failures_reach_all_pixel_gates() {
    let canvas = Rect::full(128, 96);
    let image = base(canvas);
    let source = source(&image);
    let mut invalid = watermark(canvas);
    invalid.style.font_size = f32::NAN;
    let mut effect = Annotation::new(Tool::Blur, Point { x: 10., y: 10. }, Style::default());
    effect.points.push(Point { x: 100., y: 80. });
    let rejected = Scene::prepare(&[invalid, effect.clone()], &source);
    assert!(!rejected.errors.is_empty());
    assert!(rejected
        .layers
        .iter()
        .all(|layer| matches!(layer, Layer::Empty)));
    let mut output = base(canvas);
    assert!(rejected.try_apply(&mut output, canvas).is_err());
    assert_eq!(output.bgra, image.bgra);

    let valid = watermark(canvas);
    let mut runtime = Scene::prepare(std::slice::from_ref(&valid), &source);
    let mut over_budget = valid;
    over_budget.style.text.outline = true;
    over_budget.style.text.outline_width = 1_000_000.;
    runtime.layers[0] = Layer::Watermark {
        annotation: Arc::new(over_budget),
    };
    assert!(runtime.try_apply(&mut output, canvas).is_err());
    assert_eq!(output.bgra, image.bgra);
    assert!(crate::raster::try_effect(&effect, &source, &runtime).is_err());
    effect.tool = Tool::PixelEraser;
    assert!(crate::raster::try_restore(&effect, &source, &runtime).is_err());
    effect.tool = Tool::LocalFill;
    effect.points = vec![Point { x: 20., y: 20. }, Point { x: 50., y: 20. }];
    assert!(crate::local_fill_scene::prepare(&effect, &source, &runtime).is_err());
    let projection = OutputProjection::new(canvas, 1.5).unwrap();
    let mut enlarged = projection.background(&source).unwrap();
    assert!(runtime
        .apply_projected(&mut enlarged, &source, &projection)
        .is_err());
}
