//! LocalFill 的局部采样、缓存与导出 oracle；所有像素来自自有合成缓冲区。
use crate::{
    annotation::{Annotation, Bounds, HighlighterBlend, LocalFillGeometry, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    local_fill_scene::{self, FillPatch},
    output_projection::{window_opacity, OutputProjection},
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds {
    Bounds {
        x,
        y,
        width,
        height,
    }
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

fn set(bytes: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 4]) {
    let i = (y * width + x) * 4;
    bytes[i..i + 4].copy_from_slice(&color);
}

fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[i..i + 4].try_into().unwrap()
}

fn regions(items: Vec<Bounds>) -> Annotation {
    let mut a = Annotation::new(Tool::LocalFill, p(0., 0.), Style::default());
    a.fill_geometry = LocalFillGeometry::Regions(items.into());
    assert!(a.valid());
    a
}

fn rectangle(a: Point, b: Point, color: u32) -> Annotation {
    let mut item = Annotation::new(
        Tool::Rectangle,
        a,
        Style {
            color,
            filled: true,
            ..Default::default()
        },
    );
    item.update(b);
    item
}

fn patches(scene: &Scene, index: usize) -> &Arc<[FillPatch]> {
    let Layer::LocalFill { patches } = &scene.layers[index] else {
        panic!("指定层应为 LocalFill");
    };
    patches
}

fn patch_pixel(patches: &[FillPatch], x: u32, y: u32) -> [u8; 4] {
    let mut value = [0; 4];
    for patch in patches {
        if patch.rect.contains((x as i32, y as i32)) {
            let next = pixel(&patch.image, x - patch.rect.x, y - patch.rect.y);
            if next[3] != 0 {
                value = next;
            }
        }
    }
    value
}

fn render(scene: &Scene, source: &SourcePixels<'_>, rect: Rect) -> Image {
    assert!(scene.errors.is_empty(), "{}", scene.errors.join(";"));
    let mut image = source.read(rect);
    scene.apply(&mut image, rect);
    image
}

#[test]
fn local_fill_scene_regions_use_one_baseline_and_later_overlap_wins() {
    let color_a = [30, 90, 150, 255];
    let color_b = [180, 40, 80, 255];
    let mut bytes = [230, 230, 230, 255].repeat(14);
    // A=[3,6) 外环 x=1,2,6,7，A 色有三个。B=[5,8) 外环
    // x=3,4,8,9，B/A 各两个且 B 先出现。若先写回 A，B 会错误选 A。
    for x in [1, 2, 6, 8, 9] {
        set(&mut bytes, 14, x, 0, color_a);
    }
    for x in [3, 4, 7] {
        set(&mut bytes, 14, x, 0, color_b);
    }
    let source = source(&bytes, 14, 1);
    let a = regions(vec![bounds(3., 0., 3., 1.), bounds(5., 0., 3., 1.)]);
    let below = Scene::prepare(&[], &source);
    let prepared = local_fill_scene::prepare(&a, &source, &below)
        .unwrap()
        .unwrap();
    assert_eq!(prepared.stats.requested, 6);
    assert_eq!(prepared.stats.filled, 6);
    let scene = below.with_prepared_fill(&a, prepared, &source).unwrap();
    let original = source.read(source.canvas);
    let image = render(&scene, &source, source.canvas);
    for x in 0..14 {
        let expected = match x {
            3 | 4 => color_a,
            5..=7 => color_b,
            _ => pixel(&original, x, 0),
        };
        assert_eq!(pixel(&image, x, 0), expected, "x={x}");
    }
}

#[test]
fn local_fill_scene_distant_regions_keep_gap_transparent_without_union_image() {
    let color_a = [30, 90, 150, 255];
    let color_b = [180, 40, 80, 255];
    let mut bytes = color_a.repeat(5000 * 4);
    for y in 0..4 {
        for x in 2500..5000 {
            set(&mut bytes, 5000, x, y, color_b);
        }
        if (1..3).contains(&y) {
            for x in (2..6).chain(4994..4998) {
                set(&mut bytes, 5000, x, y, [255, 255, 255, 255]);
            }
        }
    }
    let source = source(&bytes, 5000, 4);
    let a = regions(vec![bounds(2., 1., 4., 2.), bounds(4994., 1., 4., 2.)]);
    let scene = Scene::prepare(&[a], &source);
    let patches = patches(&scene, 0);
    let area: u64 = patches
        .iter()
        .map(|patch| u64::from(patch.rect.width) * u64::from(patch.rect.height))
        .sum();
    assert!(area < 256, "两处 tiny ROI 不应分配跨 4996 px 的 union 图");
    assert_eq!(patch_pixel(patches, 2500, 1), [0; 4]);
    let original = source.read(source.canvas);
    let image = render(&scene, &source, source.canvas);
    assert_eq!(pixel(&image, 3, 1), color_a);
    assert_eq!(pixel(&image, 4995, 1), color_b);
    for x in [6, 1000, 2499, 2500, 4000, 4993] {
        assert_eq!(
            pixel(&image, x, 1),
            pixel(&original, x, 1),
            "离散 ROI 间隙必须保留源像素"
        );
    }
}

#[test]
fn local_fill_scene_wide_roi_splits_patches_without_seams_or_changing_samples() {
    let color = [27, 83, 139, 255];
    let mut bytes = color.repeat(4102 * 5);
    for y in 1..4 {
        for x in 1..4101 {
            set(&mut bytes, 4102, x, y, [255, 255, 255, 255]);
        }
    }
    let source = source(&bytes, 4102, 5);
    let a = regions(vec![bounds(1., 1., 4100., 3.)]);
    let scene = Scene::prepare(&[a], &source);
    let patches = patches(&scene, 0);
    assert!(patches.len() >= 3);
    assert!(patches.iter().all(|patch| {
        patch.image.width <= 2048
            && patch.image.height <= 2048
            && patch.rect.width == patch.image.width
            && patch.rect.height == patch.image.height
    }));
    let image = render(&scene, &source, source.canvas);
    assert!(image.bgra.chunks_exact(4).all(|pixel| pixel == color));
}

#[test]
fn local_fill_scene_nonzero_crop_and_cross_plane_sampling_preserve_source_coordinates() {
    let color = [29, 93, 157, 255];
    let mut left = color.repeat(4 * 6);
    let mut right = color.repeat(4 * 6);
    // 两个 plane 分别从虚拟画布 (2,1)/(6,1) 开始，ROI 跨两屏。
    for y in 2..4 {
        for x in 2..4 {
            set(&mut left, 4, x, y, [255, 255, 255, 255]);
        }
        for x in 0..2 {
            set(&mut right, 4, x, y, [255, 255, 255, 255]);
        }
    }
    set(&mut left, 4, 0, 0, [240, 10, 20, 255]);
    let source = SourcePixels {
        fill: None,
        canvas: Rect::full(12, 8),
        planes: vec![
            PixelPlane {
                rect: Rect {
                    x: 2,
                    y: 1,
                    width: 4,
                    height: 6,
                },
                bgra: &left,
            },
            PixelPlane {
                rect: Rect {
                    x: 6,
                    y: 1,
                    width: 4,
                    height: 6,
                },
                bgra: &right,
            },
        ],
        overlay: None,
    };
    let a = regions(vec![bounds(4., 3., 4., 2.)]);
    let scene = Scene::prepare(&[a], &source);
    let crop = Rect {
        x: 1,
        y: 0,
        width: 10,
        height: 8,
    };
    let mut expected = source.read(crop);
    for y in 3..5 {
        for x in 4..8 {
            set(&mut expected.bgra, 10, x - 1, y, color);
        }
    }
    let image = render(&scene, &source, crop);
    assert_eq!(image.bgra, expected.bgra);
    assert_eq!(pixel(&image, 1, 1), [240, 10, 20, 255]);
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
}

#[test]
fn local_fill_scene_uses_lower_vectors_and_excludes_upper_vectors() {
    let bytes = [230, 230, 230, 255].repeat(24 * 18);
    let source = source(&bytes, 24, 18);
    let lower = rectangle(p(2., 2.), p(22., 16.), 0x219163);
    let fill = regions(vec![bounds(7., 6., 6., 5.)]);
    let upper = rectangle(p(7., 6.), p(13., 11.), 0xdc14b4);
    let scene = Scene::prepare(&[lower.clone(), fill.clone(), upper], &source);
    assert_eq!(patch_pixel(patches(&scene, 1), 9, 8), [99, 145, 33, 255]);
    let image = render(&scene, &source, source.canvas);
    assert_eq!(pixel(&image, 9, 8), [180, 20, 220, 255]);
    let lower_scene = Scene::prepare(&[lower, fill], &source);
    let image = render(&lower_scene, &source, source.canvas);
    assert_eq!(pixel(&image, 9, 8), [99, 145, 33, 255]);
}

#[test]
fn local_fill_scene_prefix_reuses_upper_changes_and_invalidates_lower_or_source() {
    let bytes = [230, 230, 230, 255].repeat(24 * 18);
    let source = source(&bytes, 24, 18);
    let lower = rectangle(p(2., 2.), p(22., 16.), 0x219163);
    let fill = regions(vec![bounds(7., 6., 6., 5.)]);
    let upper = rectangle(p(16., 5.), p(20., 10.), 0xdc14b4);
    let original = vec![lower, fill, upper];
    let first = Scene::prepare(&original, &source);
    let first_patches = patches(&first, 1).clone();

    let mut changed_upper = original.clone();
    changed_upper[2].style.color = 0x1838b8;
    let upper_scene = first.updated(&changed_upper, &source);
    assert!(Arc::ptr_eq(&first_patches, patches(&upper_scene, 1)));

    let mut changed_lower = original.clone();
    changed_lower[0].style.color = 0xc83c1c;
    let lower_scene = first.updated(&changed_lower, &source);
    assert!(!Arc::ptr_eq(&first_patches, patches(&lower_scene, 1)));
    assert_eq!(
        patch_pixel(patches(&lower_scene, 1), 9, 8),
        [28, 60, 200, 255]
    );

    let replacement = [80, 100, 120, 255].repeat(24 * 18);
    let replacement_source = self::source(&replacement, 24, 18);
    let source_scene = first.updated(&original, &replacement_source);
    assert!(!Arc::ptr_eq(&first_patches, patches(&source_scene, 1)));
    assert!(source_scene.errors.is_empty());

    let cursor = [250, 230, 210, 255].repeat(2 * 2);
    let overlay_source = SourcePixels {
        fill: None,
        canvas: source.canvas,
        planes: vec![PixelPlane {
            rect: source.canvas,
            bgra: &bytes,
        }],
        overlay: Some(PixelPlane {
            rect: Rect {
                x: 5,
                y: 4,
                width: 2,
                height: 2,
            },
            bgra: &cursor,
        }),
    };
    let overlay_scene = first.updated(&original, &overlay_source);
    assert!(!Arc::ptr_eq(&first_patches, patches(&overlay_scene, 1)));
}

#[test]
fn local_fill_scene_failed_refresh_becomes_empty_without_stale_filled_pixels() {
    let mut bytes = [39, 105, 171, 255].repeat(16 * 12);
    for y in 3..7 {
        for x in 3..7 {
            set(&mut bytes, 16, x, y, [255, 255, 255, 255]);
        }
    }
    let source = source(&bytes, 16, 12);
    let fill = regions(vec![bounds(3., 3., 4., 4.)]);
    let first = Scene::prepare(std::slice::from_ref(&fill), &source);
    assert_eq!(
        pixel(&render(&first, &source, source.canvas), 4, 4),
        [39, 105, 171, 255]
    );
    // 整幅都是孔，无邻域；重建失败不得继续显示上一笔的局部底图缓存。
    let invalid = regions(vec![bounds(0., 0., 16., 12.)]);
    let refreshed = first.updated(&[invalid], &source);
    assert!(!refreshed.errors.is_empty());
    assert!(matches!(&refreshed.layers[0], Layer::Empty));
    let mut image = source.read(source.canvas);
    assert!(refreshed.try_apply(&mut image, source.canvas).is_err());
    assert_eq!(image.bgra, bytes);
    assert_eq!(pixel(&image, 4, 4), [255, 255, 255, 255]);

    // 更换为像素不完整的源也须失效；只向自有完整目标合成，不能读无效 plane。
    let broken_bytes = [10, 20, 30, 255];
    let broken_source = self::source(&broken_bytes, 16, 12);
    let failed_source = first.updated(std::slice::from_ref(&fill), &broken_source);
    assert!(!failed_source.errors.is_empty());
    assert!(matches!(&failed_source.layers[0], Layer::Empty));
    let mut target = Image {
        width: 16,
        height: 12,
        bgra: [180, 190, 200, 255].repeat(16 * 12),
        readback_ms: 0.,
    };
    let expected = target.bgra.clone();
    assert!(failed_source.try_apply(&mut target, source.canvas).is_err());
    assert_eq!(target.bgra, expected, "失效失败不能显示旧源的填充 patch");
}

#[test]
fn local_fill_scene_failed_baseline_blocks_downstream_and_recovers_on_source_change() {
    let fill = regions(vec![bounds(3., 3., 4., 4.)]);
    let mut downstream = vec![fill.clone()];
    for tool in [
        Tool::Mosaic,
        Tool::Blur,
        Tool::PixelEraser,
        Tool::Highlighter,
        Tool::HighlighterRect,
    ] {
        let mut a = Annotation::new(
            tool,
            p(3., 3.),
            Style {
                width: 2.,
                block_size: 2,
                highlighter_blend: HighlighterBlend::Multiply,
                ..Default::default()
            },
        );
        a.update(p(7., 7.));
        assert!(a.valid());
        downstream.push(a);
    }
    let mut chain = vec![fill.clone()];
    chain.extend(downstream.clone());

    // 首个填充安全拒绝短缓冲；所有后续采样工具均不得再读这个无效 plane。
    // 不捕获 panic：任一下游仍访问 SourcePixels::read 的短 slice，测试直接失败。
    let broken_bytes = [10, 20, 30, 255];
    let broken_source = source(&broken_bytes, 16, 12);
    let broken = Scene::prepare(&chain, &broken_source);
    assert_eq!(broken.errors.len(), 1);
    assert!(broken.errors[0].contains("底图像素不完整"));
    assert_eq!(broken.layers.len(), chain.len());
    assert!(broken
        .layers
        .iter()
        .all(|layer| matches!(layer, Layer::Empty)));

    // 同一个失败前缀被缓存后追加效果，也不能把缓存错误丢掉或生成新采样层。
    let prefix = Scene::prepare(std::slice::from_ref(&fill), &broken_source);
    let cached = prefix.updated(&chain, &broken_source);
    assert_eq!(cached.errors, prefix.errors);
    assert_eq!(cached.layers.len(), chain.len());
    assert!(cached
        .layers
        .iter()
        .all(|layer| matches!(layer, Layer::Empty)));

    let color = [31, 95, 159, 255];
    let bytes = color.repeat(16 * 12);
    let valid_source = source(&bytes, 16, 12);
    // 源有效、后续小填充单独能够成功时，首个全屏孔失败仍必须阻断它。
    let all_hole = regions(vec![bounds(0., 0., 16., 12.)]);
    let full_prefix = Scene::prepare(std::slice::from_ref(&all_hole), &valid_source);
    assert!(!full_prefix.errors.is_empty());
    let alone = Scene::prepare(std::slice::from_ref(&fill), &valid_source);
    assert!(alone.errors.is_empty());
    assert!(matches!(&alone.layers[0], Layer::LocalFill { .. }));
    let mut full_chain = vec![all_hole];
    full_chain.extend(downstream);
    let full = Scene::prepare(&full_chain, &valid_source);
    assert_eq!(full.errors, full_prefix.errors);
    assert!(full
        .layers
        .iter()
        .all(|layer| matches!(layer, Layer::Empty)));
    let full_cached = full_prefix.updated(&full_chain, &valid_source);
    assert_eq!(full_cached.errors, full_prefix.errors);
    assert!(full_cached
        .layers
        .iter()
        .all(|layer| matches!(layer, Layer::Empty)));

    // 替换为完整源必须从首层重建；失败前缀和下游 Empty 不能污染恢复后的缓存。
    let recovered = cached.updated(&chain, &valid_source);
    assert!(recovered.errors.is_empty());
    assert_eq!(recovered.layers.len(), chain.len());
    assert!(recovered
        .layers
        .iter()
        .all(|layer| !matches!(layer, Layer::Empty)));
    assert_eq!(patch_pixel(patches(&recovered, 0), 4, 4), color);
    assert_eq!(patch_pixel(patches(&recovered, 1), 4, 4), color);
}

#[test]
fn local_fill_scene_partial_boundary_failure_delivers_no_prepared_patch() {
    let mut bytes = [220, 220, 220, 255].repeat(20);
    set(&mut bytes, 20, 0, 0, [30, 90, 150, 255]);
    let source = source(&bytes, 20, 1);
    let below = Scene::prepare(&[], &source);
    let fill = regions(vec![bounds(1., 0., 19., 1.)]);
    assert!(local_fill_scene::prepare(&fill, &source, &below).is_err());
    assert_eq!(source.read(source.canvas).bgra, bytes);
    // 全批次准备：第一个 ROI 可完成，第二个不能完成，也必须整体失败。
    let batch = regions(vec![bounds(1., 0., 2., 1.), bounds(1., 0., 19., 1.)]);
    assert!(local_fill_scene::prepare(&batch, &source, &below).is_err());
    assert!(below.layers.is_empty());
}

#[test]
fn local_fill_scene_prepared_commit_reuses_exact_patches_and_rejects_changed_source() {
    let bytes = [31, 95, 159, 255].repeat(16 * 12);
    let source = source(&bytes, 16, 12);
    let below = Scene::prepare(&[], &source);
    let fill = regions(vec![bounds(3., 3., 4., 4.)]);
    let prepared = local_fill_scene::prepare(&fill, &source, &below)
        .unwrap()
        .unwrap();
    let original_patches = prepared.patches.clone();
    let committed = below.with_prepared_fill(&fill, prepared, &source).unwrap();
    assert!(Arc::ptr_eq(&original_patches, patches(&committed, 0)));
    let refreshed = committed.updated(std::slice::from_ref(&fill), &source);
    assert!(Arc::ptr_eq(&original_patches, patches(&refreshed, 0)));

    let prepared = local_fill_scene::prepare(&fill, &source, &below)
        .unwrap()
        .unwrap();
    let replacement = [180, 120, 60, 255].repeat(16 * 12);
    let replacement_source = self::source(&replacement, 16, 12);
    assert!(below
        .with_prepared_fill(&fill, prepared, &replacement_source)
        .is_err());
    assert!(below.layers.is_empty());
}

#[test]
fn local_fill_scene_stroke_hard_mask_and_projected_output_keep_source_units() {
    let color = [41, 107, 173, 128];
    let mut bytes = color.repeat(20 * 12);
    for y in 5..7 {
        for x in 5..10 {
            set(&mut bytes, 20, x, y, [255, 255, 255, 128]);
        }
    }
    let source = source(&bytes, 20, 12);
    let mut fill = Annotation::new(
        Tool::LocalFill,
        p(4., 6.),
        Style {
            width: 2.,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    fill.update(p(11., 6.));
    let scene = Scene::prepare(&[fill], &source);
    let patches = patches(&scene, 0);
    assert!(patches.iter().all(|patch| {
        patch
            .image
            .bgra
            .chunks_exact(4)
            .all(|p| p[3] == 0 || p[3] == 255)
    }));
    assert!(patches
        .iter()
        .any(|patch| { patch.image.bgra.chunks_exact(4).any(|p| p[3] == 0) }));
    let crop = Rect {
        x: 3,
        y: 2,
        width: 12,
        height: 8,
    };
    let original = render(&scene, &source, crop);
    assert_eq!(pixel(&original, 3, 3), [41, 107, 173, 255]);
    assert_eq!(pixel(&original, 0, 0), color);
    let projection = OutputProjection::new(crop, 1.5).unwrap();
    let mut output = projection.background(&source).unwrap();
    scene
        .apply_projected(&mut output, &source, &projection)
        .unwrap();
    for y in 0..output.height {
        for x in 0..output.width {
            let sx = ((f64::from(x) + 0.5) * f64::from(crop.width) / f64::from(output.width))
                .floor() as u32;
            let sy = ((f64::from(y) + 0.5) * f64::from(crop.height) / f64::from(output.height))
                .floor() as u32;
            assert_eq!(pixel(&output, x, y), pixel(&original, sx, sy));
        }
    }
    window_opacity(&mut output, 128);
    assert_eq!(pixel(&output, 0, 0), [41, 107, 173, 64]);
    assert_eq!(pixel(&output, 5, 5), [41, 107, 173, 128]);
    let shrunk = OutputProjection::new(crop, 0.5).unwrap();
    assert!(shrunk.identity());
    let mut output = shrunk.background(&source).unwrap();
    scene
        .apply_projected(&mut output, &source, &shrunk)
        .unwrap();
    assert_eq!(output.bgra, original.bgra);
}
