use super::*;
use crate::{annotation::HighlighterBlend, scene::PixelPlane};

fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn source(bytes: &[u8], width: u32, height: u32) -> SourcePixels<'_> {
    let rect = Rect::full(width, height);
    SourcePixels {
        fill: None,
        canvas: rect,
        planes: vec![PixelPlane { rect, bgra: bytes }],
        overlay: None,
    }
}
fn mark(tool: Tool, a: Point, b: Point, style: Style) -> Annotation {
    let mut item = Annotation::new(tool, a, style);
    item.update(b);
    item
}
fn pixel(image: &Image, x: u32, y: u32) -> &[u8] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    &image.bgra[i..i + 4]
}
fn render(scene: &Scene, source: &SourcePixels<'_>, projection: &OutputProjection) -> Image {
    let mut image = projection.background(source).unwrap();
    scene
        .apply_projected(&mut image, source, projection)
        .unwrap();
    image
}

#[test]
fn enlarged_export_scene_identity_preserves_old_pixels_and_cached_layers() {
    let bytes = [70, 100, 130, 128].repeat(32 * 24);
    let source = source(&bytes, 32, 24);
    let pen = mark(
        Tool::Pen,
        p(3.2, 2.4),
        p(28.1, 20.6),
        Style {
            width: 2.,
            opacity: 0.5,
            ..Default::default()
        },
    );
    let blur = mark(Tool::Blur, p(12., 3.), p(28., 18.), Style::default());
    let scene = Scene::prepare(&[pen, blur], &source);
    let rect = Rect {
        x: 4,
        y: 2,
        width: 23,
        height: 18,
    };
    let projection = OutputProjection::new(rect, 1.).unwrap();
    let mut old = source.read(rect);
    scene.apply(&mut old, rect);
    let projected = render(&scene, &source, &projection);
    assert_eq!(old.bgra, projected.bgra);
}

#[test]
fn enlarged_export_vector_repaints_diagonal_aa_and_one_object_alpha() {
    let bytes = [255, 255, 255, 255].repeat(32 * 32);
    let source = source(&bytes, 32, 32);
    let mut pen = Annotation::new(
        Tool::Pen,
        p(2.1, 1.2),
        Style {
            width: 1.,
            opacity: 0.5,
            color: 0,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    for point in [p(26.1, 22.7), p(2.1, 22.7), p(26.1, 1.2)] {
        pen.update(point);
    }
    let scene = Scene::prepare(&[pen], &source);
    let projection = OutputProjection::new(source.canvas, 2.75).unwrap();
    let image = render(&scene, &source, &projection);
    let mut one = source.read(source.canvas);
    scene.apply(&mut one, source.canvas);
    let mut different = 0;
    let mut edge_shades = std::collections::HashSet::new();
    for y in 0..image.height {
        for x in 0..image.width {
            let s = projection.source_center(x, y);
            let observed = pixel(&image, x, y)[0];
            if pixel(&one, s.x.floor() as u32, s.y.floor() as u32)[0] != observed {
                different += 1;
            }
            if observed > 128 && observed < 255 {
                edge_shades.insert(observed);
            }
            assert!(observed >= 128, "单个透明对象交叉处不能重复叠加alpha");
        }
    }
    assert!(different > 100, "目标重绘必须区别于1×整图最近邻放大");
    assert!(edge_shades.len() > 12, "目标分辨率必须有连续的边缘覆盖率");
}

#[test]
fn enlarged_export_restore_premultiplied_alpha_keeps_unerased_high_resolution() {
    let bytes = [255, 0, 0, 128].repeat(32 * 32);
    let source = source(&bytes, 32, 32);
    let fill = mark(
        Tool::Rectangle,
        p(0., 0.),
        p(32., 32.),
        Style {
            filled: true,
            color: 0xff0000,
            ..Default::default()
        },
    );
    let erase = mark(
        Tool::PixelEraser,
        p(8., 4.),
        p(8., 28.),
        Style {
            width: 4.,
            opacity: 0.5,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    let scene = Scene::prepare(&[fill.clone(), erase], &source);
    let projection = OutputProjection::new(source.canvas, 2.).unwrap();
    let image = render(&scene, &source, &projection);
    assert_eq!(
        pixel(&image, 16, 24),
        &[85, 0, 170, 192],
        "恢复必须按预乘alpha插值再转回straight BGRA"
    );
    assert_eq!(
        pixel(&image, 32, 24),
        &[0, 0, 255, 255],
        "笔迹之外保持下层，不恢复整个缓存包围盒"
    );
    let pen = mark(
        Tool::Pen,
        p(10.2, 1.),
        p(10.2, 31.),
        Style {
            width: 0.5,
            color: 0,
            opacity: 0.6,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    let eraser = mark(
        Tool::PixelEraser,
        p(8., 8.),
        p(24., 24.),
        Style {
            width: 2.,
            opacity: 1.,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    let lower = Scene::prepare(std::slice::from_ref(&pen), &source);
    let all = Scene::prepare(&[pen, eraser], &source);
    let projection = OutputProjection::new(source.canvas, 5.).unwrap();
    let before = render(&lower, &source, &projection);
    let after = render(&all, &source, &projection);
    assert_eq!(
        pixel(&before, 51, 100),
        pixel(&after, 51, 100),
        "恢复笔包围盒内部未擦到的高清铅笔边缘不能退回1×结果"
    );
}

#[test]
fn enlarged_export_highlighter_tile_seam_preserves_source_alpha_once() {
    let bytes = [100, 200, 250, 128].repeat(2060 * 8);
    let source = source(&bytes, 2060, 8);
    let a = mark(
        Tool::HighlighterRect,
        p(2040., 1.),
        p(2056., 7.),
        Style {
            color: 0xff8080,
            opacity: 0.5,
            highlighter_blend: HighlighterBlend::Multiply,
            ..Default::default()
        },
    );
    let scene = Scene::prepare(&[a], &source);
    let rect = Rect {
        x: 2040,
        y: 1,
        width: 16,
        height: 6,
    };
    let projection = OutputProjection::new(rect, 2.75).unwrap();
    let image = render(&scene, &source, &projection);
    let left = pixel(&image, 21, 8);
    let right = pixel(&image, 22, 8);
    assert_eq!(left, right, "2048源tile接缝不能叠加alpha或漏画");
    let mut expected = [100, 200, 250, 128];
    crate::raster::over_pixel(&mut expected, &[50, 100, 250], 0.5 * 128. / 255.);
    assert_eq!(right, &expected);
    assert!(right[3] < 200, "刷色与原alpha各只乘一次");
}

#[test]
fn enlarged_export_spotlight_union_crop_has_no_fake_border() {
    let bytes = [100, 100, 100, 255].repeat(40 * 24);
    let source = source(&bytes, 40, 24);
    let style = Style {
        width: 0.,
        opacity: 0.5,
        color: 0xffffff,
        ..Default::default()
    };
    let a = mark(Tool::Spotlight, p(-8., 4.), p(20., 20.), style);
    let b = mark(Tool::Spotlight, p(12., 4.), p(32., 20.), style);
    let scene = Scene::prepare(&[a, b], &source);
    let projection = OutputProjection::new(source.canvas, 2.5).unwrap();
    let image = render(&scene, &source, &projection);
    assert_eq!(
        pixel(&image, 0, 24),
        &[100, 100, 100, 255],
        "画布交线不能创建暗层AA或边框"
    );
    assert_eq!(
        pixel(&image, 48, 24),
        &[100, 100, 100, 255],
        "孔并集内部不能有接缝或重复暗化"
    );
    assert_eq!(pixel(&image, 90, 24), &[50, 50, 50, 255]);
    let crop = Rect {
        x: 7,
        y: 3,
        width: 20,
        height: 18,
    };
    let projection = OutputProjection::new(crop, 2.).unwrap();
    let clipped = render(&scene, &source, &projection);
    let full = render(
        &scene,
        &source,
        &OutputProjection::new(source.canvas, 2.).unwrap(),
    );
    for y in 0..clipped.height {
        for x in 0..clipped.width {
            assert_eq!(
                pixel(&clipped, x, y),
                pixel(&full, x + 14, y + 6),
                "目标裁剪不能改变孔边缘AA相位"
            );
        }
    }
}

#[test]
fn enlarged_export_cached_effect_and_mosaic_keep_source_sampling() {
    let bytes: Vec<u8> = (0..32 * 24)
        .flat_map(|i| [(i % 251) as u8, (i * 3 % 251) as u8, 200, 255])
        .collect();
    let source = source(&bytes, 32, 24);
    let a = mark(
        Tool::Blur,
        p(2., 2.),
        p(18., 18.),
        Style {
            block_size: 6,
            ..Default::default()
        },
    );
    let b = mark(
        Tool::Mosaic,
        p(16., 4.),
        p(30., 22.),
        Style {
            block_size: 4,
            ..Default::default()
        },
    );
    let scene = Scene::prepare(&[a, b], &source);
    let projection = OutputProjection::new(
        Rect {
            x: 3,
            y: 1,
            width: 27,
            height: 21,
        },
        2.3,
    )
    .unwrap();
    let image = render(&scene, &source, &projection);
    let mut cached = source.read(source.canvas);
    scene.apply(&mut cached, source.canvas);
    for y in 0..image.height {
        for x in 0..image.width {
            let s = projection.source_center(x, y);
            assert_eq!(
                pixel(&image, x, y),
                pixel(&cached, s.x.floor() as u32, s.y.floor() as u32),
                "cached Blur/Mosaic不应在放大后改变源颜色、块单位或全屏相位"
            );
        }
    }
}

#[test]
fn enlarged_export_dashed_rounded_rotated_geometry_keeps_crop_phase() {
    let bytes = [255, 255, 255, 255].repeat(64 * 48);
    let source = source(&bytes, 64, 48);
    let mut rectangle = mark(
        Tool::Rectangle,
        p(8.25, 7.75),
        p(51.5, 39.5),
        Style {
            width: 1.5,
            radius: 9.,
            dashed: true,
            opacity: 0.5,
            color: 0x3010e0,
            ..Default::default()
        },
    );
    rectangle.rotation = 19.;
    let arrow = mark(
        Tool::Arrow,
        p(3.5, 22.25),
        p(59.25, 21.5),
        Style {
            width: 1.,
            dashed: true,
            color: 0x129438,
            ..Default::default()
        },
    );
    let scene = Scene::prepare(&[rectangle, arrow], &source);
    let scale = 2.5;
    let full = render(
        &scene,
        &source,
        &OutputProjection::new(source.canvas, scale).unwrap(),
    );
    let crop = Rect {
        x: 10,
        y: 8,
        width: 30,
        height: 24,
    };
    let clipped = render(
        &scene,
        &source,
        &OutputProjection::new(crop, scale).unwrap(),
    );
    for y in 0..clipped.height {
        for x in 0..clipped.width {
            assert_eq!(
                pixel(&clipped, x, y),
                pixel(&full, x + 25, y + 20),
                "裁剪不能重置虚线、箭头或圆角旋转轮廓的目标覆盖率"
            );
        }
    }
    assert!(
        full.bgra.chunks_exact(4).any(|p| p[0] < 240),
        "夹具必须实际绘制轮廓"
    );
}
