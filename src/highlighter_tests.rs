//! 高亮混合的共享 Scene 回归：原始底图画刷、矢量覆盖及分块裁剪。
use crate::{
    annotation::{Annotation, HighlighterBlend, HighlighterShape, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    scene::{HighlighterTile, Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn make_source(canvas: Rect, bytes: &[u8]) -> SourcePixels<'_> {
    SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: bytes,
        }],
        overlay: None,
    }
}
fn mark(tool: Tool, blend: HighlighterBlend, opacity: f32) -> Annotation {
    let mut a = Annotation::new(
        tool,
        point(24., 24.),
        Style {
            highlighter_blend: blend,
            opacity,
            width: 12.,
            color: 0x80ff40,
            radius: 15.,
            ..Default::default()
        },
    );
    a.update(point(152., 120.));
    a
}
fn render(scene: &Scene, source: &SourcePixels<'_>, roi: Rect) -> Image {
    assert!(scene.errors.is_empty(), "{:?}", scene.errors);
    let mut image = source.read(roi);
    scene.apply(&mut image, roi);
    image
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * image.width + x) * 4) as usize;
    image.bgra[offset..offset + 4].try_into().unwrap()
}
fn brush(scene: &Scene, index: usize) -> &Arc<Vec<HighlighterTile>> {
    let Layer::Highlighter { tiles, mesh, .. } = &scene.layers[index] else {
        panic!("Multiply 未保留矢量高亮层");
    };
    assert!(!mesh.indices.is_empty());
    tiles
}
fn crop_matches(full: &Image, part: &Image, roi: Rect) {
    for y in 0..roi.height {
        let start = (((roi.y + y) * full.width + roi.x) * 4) as usize;
        let row = (y * roi.width * 4) as usize;
        assert_eq!(
            &part.bgra[row..row + roi.width as usize * 4],
            &full.bgra[start..start + roi.width as usize * 4]
        );
    }
}

#[test]
fn highlighter_scene_blends_have_distinct_gray_black_and_color_goldens() {
    let canvas = Rect::full(180, 144);
    for (background, multiply, alpha) in [
        (
            [128, 128, 128, 255],
            [32, 128, 64, 255],
            [104, 176, 128, 255],
        ),
        ([0, 0, 0, 255], [0, 0, 0, 255], [24, 96, 48, 255]),
        (
            [80, 120, 200, 255],
            [20, 120, 100, 255],
            [74, 171, 173, 255],
        ),
    ] {
        let bytes = background.repeat((canvas.width * canvas.height) as usize);
        let source = make_source(canvas, &bytes);
        let m = mark(Tool::HighlighterRect, HighlighterBlend::Multiply, 1.);
        let a = mark(Tool::HighlighterRect, HighlighterBlend::Alpha, 96. / 255.);
        let multiplied = Scene::prepare(std::slice::from_ref(&m), &source);
        let faded = Scene::prepare(std::slice::from_ref(&a), &source);
        assert_eq!(
            pixel(&render(&multiplied, &source, canvas), 88, 72),
            multiply
        );
        assert_eq!(pixel(&render(&faded, &source, canvas), 88, 72), alpha);
        assert_eq!(pixel(&brush(&multiplied, 0)[0].image, 0, 0), multiply);
        assert!(matches!(faded.layers[0], Layer::Vector { .. }));
    }
}

#[test]
fn highlighter_scene_multiply_keeps_brush_alpha_and_uses_normal_over() {
    let canvas = Rect::full(180, 144);
    for alpha in [0, 128, 255] {
        let original = [200, 100, 80, alpha];
        let bytes = original.repeat((canvas.width * canvas.height) as usize);
        let source = make_source(canvas, &bytes);
        let mut item = mark(Tool::HighlighterRect, HighlighterBlend::Multiply, 0.5);
        item.style.color = 0x8040c0;
        let scene = Scene::prepare(&[item], &source);
        let tint = [150, 25, 40, alpha];
        assert_eq!(pixel(&brush(&scene, 0)[0].image, 88, 72), tint);
        let mut expected = original;
        crate::raster::over_pixel(&mut expected, &tint[..3], alpha as f32 / 255. * 0.5);
        let image = render(&scene, &source, canvas);
        assert_eq!(pixel(&image, 88, 72), expected);
        assert_eq!(pixel(&image, 8, 8), original);
        if alpha == 0 {
            assert_eq!(pixel(&image, 88, 72)[3], 0);
        }
        if alpha == 128 {
            assert_eq!(pixel(&image, 88, 72)[3], 160);
        }
    }
}

#[test]
fn highlighter_scene_self_crossing_has_one_alpha_but_separate_alpha_marks_accumulate() {
    let canvas = Rect::full(180, 144);
    let bytes = [160, 160, 160, 255].repeat((canvas.width * canvas.height) as usize);
    let source = make_source(canvas, &bytes);
    let mut item = mark(Tool::Highlighter, HighlighterBlend::Multiply, 0.4);
    item.style.color = 0xffc000;
    item.points = vec![
        point(30., 30.),
        point(140., 110.),
        point(30., 110.),
        point(140., 30.),
    ];
    let multiplied = Scene::prepare(std::slice::from_ref(&item), &source);
    let image = render(&multiplied, &source, canvas);
    assert_eq!(pixel(&image, 85, 70), [96, 144, 160, 255]);
    assert_eq!(pixel(&image, 50, 45), [96, 144, 160, 255]);
    let Layer::Highlighter { path, .. } = &multiplied.layers[0] else {
        panic!();
    };
    assert_eq!(path.as_ref(), &item.vector_path());
    item.style.opacity = 1.;
    let once = render(
        &Scene::prepare(std::slice::from_ref(&item), &source),
        &source,
        canvas,
    );
    let twice = render(
        &Scene::prepare(&[item.clone(), item.clone()], &source),
        &source,
        canvas,
    );
    for (x, y) in [(85, 70), (50, 45)] {
        assert_eq!(pixel(&once, x, y), [0, 120, 160, 255]);
        assert_eq!(
            pixel(&twice, x, y),
            pixel(&once, x, y),
            "实心内部不重复乘色"
        );
    }
    // 不同对象的 AA 边缘仍各自 source-over；从首图蓝通道恢复覆盖率，
    // 与独立数学结果比较，容许首图8位量化造成至多2级误差。
    let mut edge_samples = 0;
    for (first, second) in once.bgra.chunks_exact(4).zip(twice.bgra.chunks_exact(4)) {
        if !(32..=128).contains(&first[0]) {
            continue;
        }
        let coverage = (160. - first[0] as f32) / 160.;
        for (channel, tint) in [0_f32, 120., 160.].into_iter().enumerate() {
            let expected =
                (first[channel] as f32 * (1. - coverage) + tint * coverage).round() as u8;
            assert!((second[channel] as i16 - expected as i16).abs() <= 2,
                "AA二次合成错误：first={first:?}, second={second:?}, channel={channel}, expected={expected}");
        }
        assert!(
            second[0] < first[0],
            "两个独立对象的部分覆盖不应被当作一笔并集"
        );
        assert_eq!(second[3], 255);
        edge_samples += 1;
    }
    assert!(edge_samples > 0, "没有检测到可检验的矢量 AA 边缘");
    item.style.highlighter_blend = HighlighterBlend::Alpha;
    item.style.opacity = 0.4;
    let once = render(
        &Scene::prepare(std::slice::from_ref(&item), &source),
        &source,
        canvas,
    );
    let twice = render(
        &Scene::prepare(&[item.clone(), item], &source),
        &source,
        canvas,
    );
    assert_ne!(pixel(&once, 85, 70), pixel(&twice, 85, 70));
}

#[test]
fn highlighter_scene_rotated_round_rect_and_freehand_match_nonzero_roi() {
    let canvas = Rect::full(180, 144);
    let bytes: Vec<_> = (0..canvas.width * canvas.height)
        .flat_map(|i| {
            [
                (i % canvas.width) as u8,
                (i / canvas.width) as u8,
                170,
                if i % 3 == 0 { 128 } else { 255 },
            ]
        })
        .collect();
    let source = make_source(canvas, &bytes);
    let roi = Rect {
        x: 53,
        y: 44,
        width: 73,
        height: 68,
    };
    for tool in [Tool::HighlighterRect, Tool::Highlighter] {
        for blend in [HighlighterBlend::Multiply, HighlighterBlend::Alpha] {
            let mut item = mark(tool, blend, 0.63);
            item.rotation = 31.;
            item.style.radius = 29.;
            if tool == Tool::Highlighter {
                item.points.insert(1, point(40., 95.));
            }
            let scene = Scene::prepare(&[item], &source);
            let full = render(&scene, &source, canvas);
            let part = render(&scene, &source, roi);
            crop_matches(&full, &part, roi);
        }
    }
}

#[test]
fn highlighter_scene_mosaic_reads_the_committed_multiply_result() {
    let canvas = Rect::full(180, 144);
    let bytes: Vec<_> = (0..canvas.width * canvas.height)
        .flat_map(|i| {
            let x = i % canvas.width;
            [
                if x.is_multiple_of(2) { 40 } else { 200 },
                (i / canvas.width) as u8,
                120,
                255,
            ]
        })
        .collect();
    let source = make_source(canvas, &bytes);
    let highlighter = mark(Tool::HighlighterRect, HighlighterBlend::Multiply, 0.7);
    let prefix = Scene::prepare(std::slice::from_ref(&highlighter), &source);
    let below = render(&prefix, &source, canvas);
    let mut mosaic = Annotation::new(
        Tool::Mosaic,
        point(64., 48.),
        Style {
            block_size: 8,
            ..Default::default()
        },
    );
    mosaic.update(point(112., 112.));
    let scene = prefix.updated(&[highlighter.clone(), mosaic.clone()], &source);
    let image = render(&scene, &source, canvas);
    for (x, y) in [(64, 48), (80, 72), (104, 104)] {
        let mut sum = [0_u32; 3];
        for cy in y..y + 8 {
            for cx in x..x + 8 {
                for (channel, total) in sum.iter_mut().enumerate() {
                    *total += pixel(&below, cx, cy)[channel] as u32;
                }
            }
        }
        let expected = [
            (sum[0] / 64) as u8,
            (sum[1] / 64) as u8,
            (sum[2] / 64) as u8,
            255,
        ];
        assert_eq!(pixel(&image, x + 3, y + 4), expected);
    }
    let reordered = render(
        &Scene::prepare(&[mosaic, highlighter], &source),
        &source,
        canvas,
    );
    assert_ne!(pixel(&reordered, 67, 52), pixel(&image, 67, 52));
}

#[test]
fn highlighter_scene_brush_survives_lower_edits_but_source_replacement_invalidates_it() {
    let canvas = Rect::full(180, 144);
    let bytes = [128, 128, 128, 255].repeat((canvas.width * canvas.height) as usize);
    let source = make_source(canvas, &bytes);
    let mut lower = Annotation::new(
        Tool::Rectangle,
        point(10., 10.),
        Style {
            filled: true,
            color: 0xff0000,
            ..Default::default()
        },
    );
    lower.update(point(170., 130.));
    let item = mark(Tool::HighlighterRect, HighlighterBlend::Multiply, 1.);
    let first = Scene::prepare(&[lower.clone(), item.clone()], &source);
    let bulk = Scene::prepare(&[lower.clone(), item.clone(), item.clone()], &source);
    assert!(
        Arc::ptr_eq(&brush(&bulk, 1)[0].image, &brush(&bulk, 2)[0].image),
        "首次批量加载同色对象也应共享源画刷"
    );
    lower.style.color = 0x0000ff;
    let annotations = [lower, item];
    let edited = first.updated(&annotations, &source);
    assert!(Arc::ptr_eq(
        &brush(&first, 1)[0].image,
        &brush(&edited, 1)[0].image
    ));
    assert_eq!(
        pixel(&render(&edited, &source, canvas), 88, 72),
        [32, 128, 64, 255],
        "Multiply 取原始底图，而非递归采样下层蓝色对象"
    );
    let replacement = [64, 64, 64, 255].repeat((canvas.width * canvas.height) as usize);
    let new_source = make_source(canvas, &replacement);
    let changed = edited.updated(&annotations, &new_source);
    assert!(!Arc::ptr_eq(
        &brush(&edited, 1)[0].image,
        &brush(&changed, 1)[0].image
    ));
    assert_eq!(
        pixel(&render(&changed, &new_source, canvas), 88, 72),
        [16, 64, 32, 255]
    );
}

#[test]
fn highlighter_scene_long_line_tiles_reuse_global_grid_and_preserve_seam_pixels() {
    let canvas = Rect::full(18_000, 48);
    let bytes: Vec<_> = (0..canvas.width * canvas.height)
        .flat_map(|i| [(i % 251) as u8, 120, 200, 255])
        .collect();
    let source = make_source(canvas, &bytes);
    let mut line = Annotation::new(
        Tool::Highlighter,
        point(17., 24.),
        Style {
            highlighter_shape: HighlighterShape::Line,
            highlighter_blend: HighlighterBlend::Multiply,
            width: 12.,
            opacity: 1.,
            color: 0x8040c0,
            ..Default::default()
        },
    );
    line.update(point(17_970., 24.));
    let first = Scene::prepare(std::slice::from_ref(&line), &source);
    assert_eq!(brush(&first, 0).len(), 9);
    for tile in brush(&first, 0).iter() {
        assert_eq!(tile.rect.x % 2048, 0);
        assert!(tile.rect.width <= 2048 && tile.rect.height <= 2048);
        assert_eq!(
            (tile.image.width, tile.image.height),
            (tile.image_rect.width, tile.image_rect.height)
        );
    }
    let roi = Rect {
        x: 2038,
        y: 18,
        width: 26,
        height: 12,
    };
    let image = render(&first, &source, roi);
    let original_roi = source.read(roi);
    for x in 0..roi.width {
        let raw = pixel(&original_roi, x, 6);
        assert_eq!(
            pixel(&image, x, 6),
            [(raw[0] as u32 * 192 / 255) as u8, 30, 100, 255]
        );
    }
    line.translate(1., 0.);
    line.style.opacity = 0.42;
    let moved = first.updated(std::slice::from_ref(&line), &source);
    for (a, b) in brush(&first, 0).iter().zip(brush(&moved, 0).iter()) {
        assert_eq!(a.rect, b.rect);
        assert_eq!(a.image_rect, b.image_rect);
        assert!(
            Arc::ptr_eq(&a.image, &b.image),
            "几何移动/opacity 改变不重读或重传同一源画刷块"
        );
    }
    let Layer::Highlighter {
        path,
        segments,
        style,
        ..
    } = &moved.layers[0]
    else {
        panic!();
    };
    assert_eq!(path.as_ref(), &line.vector_path());
    assert_eq!(segments.as_ref(), &line.segments());
    assert_eq!(*style, line.style);
}

#[test]
fn highlighter_scene_fractional_alpha_edges_and_halo_do_not_double_at_tile_seam() {
    let canvas = Rect::full(4200, 56);
    let bytes: Vec<_> = (0..canvas.width * canvas.height)
        .flat_map(|i| [(i % 251) as u8, 120, 200, 96])
        .collect();
    let source = make_source(canvas, &bytes);
    let mut item = mark(Tool::HighlighterRect, HighlighterBlend::Multiply, 0.63);
    item.points = vec![point(2035.25, 10.75), point(2065.75, 43.25)];
    item.style.color = 0x8040c0;
    item.style.radius = 12.5;
    item.rotation = 7.;
    let scene = Scene::prepare(&[item], &source);
    let tiles = brush(&scene, 0);
    assert_eq!(tiles.len(), 2);
    assert_eq!(tiles[0].rect.x + tiles[0].rect.width, tiles[1].rect.x);
    assert_eq!(tiles[0].image_rect.x + tiles[0].image_rect.width, 2049);
    assert_eq!(tiles[1].image_rect.x, 2047);
    // 两块的跨边界邻居是真实源像素，分数 GPU 采样不会各自钳到不同颜色。
    for x in [2047, 2048] {
        let left = pixel(&tiles[0].image, x - tiles[0].image_rect.x, 24);
        let right = pixel(&tiles[1].image, x - tiles[1].image_rect.x, 24);
        assert_eq!(left, right);
        let raw = pixel(
            &source.read(Rect {
                x,
                y: 24,
                width: 1,
                height: 1,
            }),
            0,
            0,
        );
        assert_eq!(left, [(raw[0] as u32 * 192 / 255) as u8, 30, 100, 96]);
    }
    let full = render(&scene, &source, canvas);
    let Layer::Highlighter {
        path,
        segments,
        style,
        ..
    } = &scene.layers[0]
    else {
        panic!();
    };
    let mut untiled_brush = source.read(canvas);
    for pixel in untiled_brush.bgra.chunks_exact_mut(4) {
        for (channel, tint) in [192_u32, 64, 128].into_iter().enumerate() {
            pixel[channel] = (pixel[channel] as u32 * tint / 255) as u8;
        }
    }
    let mut untiled = source.read(canvas);
    crate::raster::paint_vector_image(
        &mut untiled,
        canvas,
        path,
        segments,
        *style,
        canvas,
        canvas,
        &untiled_brush,
    );
    assert_eq!(full.bgra, untiled.bgra);
    assert_eq!(full.bgra.chunks_exact(4).map(|p| p[3]).max(), Some(134));
    assert!(full.bgra.chunks_exact(4).any(|p| p[3] > 96 && p[3] < 134));
    let roi = Rect {
        x: 2042,
        y: 8,
        width: 18,
        height: 40,
    };
    crop_matches(&full, &render(&scene, &source, roi), roi);
}
