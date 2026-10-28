//! 最小效果参数的真实像素回归；只构造合成 CPU 图，不创建 UI/GPU。
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn fixture() -> Image {
    let (width, height) = (19, 17);
    let mut bgra = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            bgra.extend_from_slice(&[
                (3 * x + 7 * y) as u8,
                (9 * x + 2 * y) as u8,
                (5 * x + 9 * y) as u8,
                255,
            ]);
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}
fn source(image: &Image) -> SourcePixels<'_> {
    let canvas = Rect::full(image.width, image.height);
    SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &image.bgra,
        }],
        overlay: None,
    }
}
fn effect(tool: Tool, block_size: u32) -> Annotation {
    let (start, end) = if tool.freehand() {
        (Point { x: 3., y: 7. }, Point { x: 16., y: 7. })
    } else {
        (Point { x: 3., y: 3. }, Point { x: 16., y: 12. })
    };
    let mut a = Annotation::new(
        tool,
        start,
        Style {
            width: 8.,
            block_size,
            ..Default::default()
        },
    );
    a.update(end);
    a
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * image.width + x) * 4) as usize;
    image.bgra[offset..offset + 4].try_into().unwrap()
}
fn rendered(scene: &Scene, source: &SourcePixels<'_>) -> Image {
    assert!(scene.errors.is_empty());
    let mut image = source.read(source.canvas);
    scene.apply(&mut image, source.canvas);
    image
}
fn assert_crop_matches(scene: &Scene, source: &SourcePixels<'_>, full: &Image) {
    let crop = Rect {
        x: 7,
        y: 5,
        width: 8,
        height: 5,
    };
    let mut part = source.read(crop);
    scene.apply(&mut part, crop);
    for y in 0..crop.height {
        for x in 0..crop.width {
            assert_eq!(pixel(&part, x, y), pixel(full, crop.x + x, crop.y + y));
        }
    }
}
// 独立参考：从输入像素按虚拟画布的全局格点取均值，不调用 Scene/raster 采样。
fn global_cell_mean(image: &Image, size: u32, x: u32, y: u32) -> [u8; 4] {
    let (left, top) = (x / size * size, y / size * size);
    let (right, bottom) = (
        (left + size).min(image.width),
        (top + size).min(image.height),
    );
    let mut sums = [0_u32; 3];
    for sy in top..bottom {
        for sx in left..right {
            let color = pixel(image, sx, sy);
            for channel in 0..3 {
                sums[channel] += color[channel] as u32;
            }
        }
    }
    let count = (right - left) * (bottom - top);
    [
        (sums[0] / count) as u8,
        (sums[1] / count) as u8,
        (sums[2] / count) as u8,
        255,
    ]
}

#[test]
fn mosaic_two_three_pixel_cells_use_global_grid_for_rect_and_brush() {
    let image = fixture();
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::MosaicBrush] {
        let mut two = None;
        for size in [2, 3] {
            let scene = Scene::prepare(&[effect(tool, size)], &source);
            let output = rendered(&scene, &source);
            for (x, y) in [(8, 6), (9, 7), (13, 8), (14, 8)] {
                assert_eq!(
                    pixel(&output, x, y),
                    global_cell_mean(&image, size, x, y),
                    "{tool:?} size {size} at {x},{y}"
                );
            }
            assert_eq!(pixel(&output, 1, 1), pixel(&image, 1, 1));
            assert_crop_matches(&scene, &source, &output);
            if size == 2 {
                two = Some(output.bgra);
            } else {
                assert_ne!(
                    two.as_ref().unwrap(),
                    &output.bgra,
                    "2px 与 3px 不能都被渲染钳到 4px"
                );
            }
        }
    }
}

fn shared_effect(a: &Scene, b: &Scene) -> bool {
    match (&a.layers[1], &b.layers[1]) {
        (Layer::Mosaic { blocks: left }, Layer::Mosaic { blocks: right }) => {
            Arc::ptr_eq(left, right)
        }
        (Layer::Raster { image: left, .. }, Layer::Raster { image: right, .. }) => {
            Arc::ptr_eq(left, right)
        }
        _ => panic!("缺少局部效果层"),
    }
}

#[test]
fn minimum_effect_parameters_refresh_cache_and_sample_changed_lower_layer() {
    let image = fixture();
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::MosaicBrush, Tool::Blur, Tool::BlurBrush] {
        let mut lower = Annotation::new(
            Tool::Rectangle,
            Point { x: 0., y: 0. },
            Style {
                width: 0.,
                filled: true,
                color: 0x2d8fc1,
                ..Default::default()
            },
        );
        lower.update(Point {
            x: image.width as f32,
            y: image.height as f32,
        });
        let mut annotation = effect(tool, 2);
        let initial = Scene::prepare(&[lower.clone(), annotation.clone()], &source);
        assert_eq!(
            pixel(&rendered(&initial, &source), 8, 7),
            [0xc1, 0x8f, 0x2d, 255]
        );
        let cached = initial.updated(&[lower.clone(), annotation.clone()], &source);
        assert!(shared_effect(&initial, &cached));
        annotation.style.block_size = 3;
        let resized = cached.updated(&[lower.clone(), annotation.clone()], &source);
        assert!(!shared_effect(&cached, &resized));
        let fresh = Scene::prepare(&[lower.clone(), annotation.clone()], &source);
        assert_eq!(
            rendered(&resized, &source).bgra,
            rendered(&fresh, &source).bgra
        );
        lower.style.color = 0xc72e65;
        let refreshed = resized.updated(&[lower.clone(), annotation.clone()], &source);
        assert!(!shared_effect(&resized, &refreshed));
        let expected = Scene::prepare(&[lower, annotation], &source);
        let output = rendered(&refreshed, &source);
        assert_eq!(output.bgra, rendered(&expected, &source).bgra);
        assert_eq!(pixel(&output, 8, 7), [0x65, 0x2e, 0xc7, 255]);
        assert_crop_matches(&refreshed, &source, &output);
    }
}

#[test]
fn blur_two_three_keep_existing_box_approximation_and_nonzero_crop() {
    let mut image = fixture();
    for color in image.bgra.chunks_exact_mut(4) {
        color.copy_from_slice(&[0, 0, 0, 255]);
    }
    let offset = ((7 * image.width + 8) * 4) as usize;
    image.bgra[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
    let source = source(&image);
    for tool in [Tool::Blur, Tool::BlurBrush] {
        for size in [2, 3] {
            let scene = Scene::prepare(&[effect(tool, size)], &source);
            let output = rendered(&scene, &source);
            // 原有三轮半径 1 的 box 近似、逐轴整数舍入；不声称与 Gaussian 内核完全一致。
            assert_eq!(pixel(&output, 8, 7), [16, 16, 16, 255]);
            assert_eq!(pixel(&output, 12, 7), [0, 0, 0, 255]);
            assert_crop_matches(&scene, &source, &output);
        }
    }
}

// 独立解析参考：从原始两点、角度与显式新字段计算局部圆角，并用四角圆心距离
// 判断源像素中心；不调用被测 EffectGeometry、segments、bounds 或 hit。
fn rounded_fixture() -> Image {
    let mut image = fixture();
    for y in 0..image.height {
        for x in 0..image.width {
            let offset = ((y * image.width + x) * 4) as usize;
            image.bgra[offset..offset + 4].copy_from_slice(&[
                (10 + ((x + y) % 2) * 220) as u8,
                (3 + (x % 3) * (y % 3) * 29) as u8,
                ((x * 17 + y * 43) % 256) as u8,
                255,
            ]);
        }
    }
    image
}
// 独立采样参考：逐点枚举 3×3 邻域，三轮水平/垂直整数均值。
// 不调用 production box_blur 或 Scene 掩模，避免旧硬矩形漏边污染圆角的采样参照。
// 本测试的全部对象 ROI 加既有 halo 覆盖整张 19×17 合成图。
fn rounded_blur_reference(image: &Image) -> Image {
    let mut output = Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: 0.,
    };
    for _ in 0..3 {
        for horizontal in [true, false] {
            let previous = output.bgra.clone();
            for y in 0..output.height {
                for x in 0..output.width {
                    let coordinate = if horizontal { x } else { y };
                    let limit = if horizontal {
                        output.width
                    } else {
                        output.height
                    };
                    let start = coordinate.saturating_sub(1);
                    let end = (coordinate + 2).min(limit);
                    let mut sum = [0_u32; 3];
                    for position in start..end {
                        let (sample_x, sample_y) = if horizontal {
                            (position, y)
                        } else {
                            (x, position)
                        };
                        let sample_offset = ((sample_y * output.width + sample_x) * 4) as usize;
                        for channel in 0..3 {
                            sum[channel] += u32::from(previous[sample_offset + channel]);
                        }
                    }
                    let offset = ((y * output.width + x) * 4) as usize;
                    for (channel, value) in sum.iter().enumerate() {
                        output.bgra[offset + channel] = (value / (end - start)) as u8;
                    }
                }
            }
        }
    }
    output
}

fn rounded_pixel_oracle(a: &Annotation, x: u32, y: u32) -> bool {
    let first = a.points[0];
    let last = a.points[1];
    let (left, top) = (
        f64::from(first.x.min(last.x)),
        f64::from(first.y.min(last.y)),
    );
    let (right, bottom) = (
        f64::from(first.x.max(last.x)),
        f64::from(first.y.max(last.y)),
    );
    let center = ((left + right) / 2., (top + bottom) / 2.);
    let (sin, cos) = match a.rotation.rem_euclid(360.) {
        0. => (0., 1.),
        90. => (1., 0.),
        180. => (0., -1.),
        270. => (-1., 0.),
        _ => f64::from(a.rotation).to_radians().sin_cos(),
    };
    let (dx, dy) = (f64::from(x) + 0.5 - center.0, f64::from(y) + 0.5 - center.1);
    let local = (
        center.0 + dx * cos + dy * sin,
        center.1 - dx * sin + dy * cos,
    );
    if local.0 < left || local.0 > right || local.1 < top || local.1 > bottom {
        return false;
    }
    let radius = f64::from(a.style.effect_radius)
        .max(0.)
        .min((right - left).min(bottom - top) / 2.);
    if radius == 0. {
        return true;
    }
    let circle_x = local.0.clamp(left + radius, right - radius);
    let circle_y = local.1.clamp(top + radius, bottom - radius);
    (local.0 - circle_x).powi(2) + (local.1 - circle_y).powi(2) <= radius.powi(2)
}

#[test]
fn ordinary_effect_radius_masks_pixels_and_hit_with_rotation_reverse_drag_and_clamp() {
    let image = rounded_fixture();
    let blur_reference = rounded_blur_reference(&image);
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::Blur] {
        for (start, end) in [
            (Point { x: 3., y: 3. }, Point { x: 16., y: 12. }),
            (Point { x: 16., y: 12. }, Point { x: 3., y: 3. }),
            (Point { x: -5., y: 2. }, Point { x: 14., y: 16. }),
        ] {
            for rotation in [0., 30., 90.] {
                for radius in [3., 99.] {
                    let mut a = Annotation::new(
                        tool,
                        start,
                        Style {
                            block_size: 3,
                            radius: 8.,
                            effect_radius: radius,
                            ..Default::default()
                        },
                    );
                    a.update(end);
                    a.rotation = rotation;
                    let mut hard = a.clone();
                    hard.style.effect_radius = 0.;
                    let hard_scene = Scene::prepare(&[hard], &source);
                    let hard_output = rendered(&hard_scene, &source);
                    let scene = Scene::prepare(&[a.clone()], &source);
                    assert!(matches!(scene.layers[0], Layer::Raster { .. }));
                    let output = rendered(&scene, &source);
                    let mut changed = 0;
                    let mut removed_corner = 0;
                    for y in 0..image.height {
                        for x in 0..image.width {
                            let covered = rounded_pixel_oracle(&a, x, y);
                            let expected = if !covered {
                                pixel(&image, x, y)
                            } else if tool == Tool::Mosaic {
                                global_cell_mean(&image, 3, x, y)
                            } else {
                                pixel(&blur_reference, x, y)
                            };
                            assert_eq!(
                                pixel(&output, x, y),
                                expected,
                                "{tool:?} angle={rotation} radius={radius} at ({x},{y})"
                            );
                            assert_eq!(
                                a.hit(
                                    Point {
                                        x: x as f32 + 0.5,
                                        y: y as f32 + 0.5
                                    },
                                    0.,
                                    true
                                ),
                                covered,
                                "轮廓命中必须与独立像素几何一致"
                            );
                            changed += usize::from(expected != pixel(&image, x, y));
                            removed_corner += usize::from(
                                !covered && pixel(&hard_output, x, y) != pixel(&image, x, y),
                            );
                        }
                    }
                    assert!(
                        changed > 0 && removed_corner > 0,
                        "必须确实处理内部且保留被圆角移除的像素"
                    );
                    let Layer::Raster { image: mask, .. } = &scene.layers[0] else {
                        unreachable!()
                    };
                    assert!(mask.bgra.chunks_exact(4).all(|p| p[3] == 0 || p[3] == 255));
                    assert_crop_matches(&scene, &source, &output);
                }
            }
        }
    }
}

#[test]
fn legacy_radius_does_not_enable_effect_rounding_and_new_zero_keeps_mosaic_blocks() {
    let image = fixture();
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::Blur] {
        let reference = rendered(&Scene::prepare(&[effect(tool, 3)], &source), &source);
        for legacy_radius in [8., 2048.] {
            let mut a = effect(tool, 3);
            a.style.radius = legacy_radius;
            assert_eq!(a.corner_radius(), 0.);
            let scene = Scene::prepare(&[a], &source);
            if tool == Tool::Mosaic {
                assert!(matches!(scene.layers[0], Layer::Mosaic { .. }));
            }
            assert_eq!(rendered(&scene, &source).bgra, reference.bgra);
        }
    }
}

#[test]
fn effect_radius_changes_refresh_own_and_downstream_cache_but_upper_changes_reuse() {
    let image = fixture();
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut lower = Annotation::new(
            Tool::Rectangle,
            Point { x: 2., y: 4. },
            Style {
                width: 0.,
                filled: true,
                color: 0x2d8fc1,
                ..Default::default()
            },
        );
        lower.update(Point { x: 12., y: 14. });
        let mut a = effect(tool, 3);
        a.style.effect_radius = 3.;
        let mut upper = Annotation::new(Tool::Pen, Point { x: 17., y: 1. }, Style::default());
        upper.update(Point { x: 17., y: 15. });
        let downstream = effect(Tool::Blur, 5);
        let items = [lower.clone(), a.clone(), upper.clone(), downstream.clone()];
        let initial = Scene::prepare(&items, &source);
        let cached = initial.updated(&items, &source);
        assert!(shared_effect(&initial, &cached));
        upper.translate(1., 0.);
        let upper_items = [lower.clone(), a.clone(), upper.clone(), downstream.clone()];
        let upper_changed = cached.updated(&upper_items, &source);
        assert!(shared_effect(&cached, &upper_changed));
        a.style.effect_radius = 4.;
        let changed_items = [lower.clone(), a.clone(), upper.clone(), downstream.clone()];
        let radius_changed = upper_changed.updated(&changed_items, &source);
        assert!(!shared_effect(&upper_changed, &radius_changed));
        let (Layer::Raster { image: old, .. }, Layer::Raster { image: new, .. }) =
            (&upper_changed.layers[3], &radius_changed.layers[3])
        else {
            panic!("下游效果必须存在")
        };
        assert!(!Arc::ptr_eq(old, new));
        assert_eq!(
            rendered(&radius_changed, &source).bgra,
            rendered(&Scene::prepare(&changed_items, &source), &source).bgra
        );
        lower.style.color = 0xc72e65;
        let lower_items = [lower.clone(), a.clone(), upper.clone(), downstream.clone()];
        let lower_changed = radius_changed.updated(&lower_items, &source);
        assert!(!shared_effect(&radius_changed, &lower_changed));
        assert_eq!(
            rendered(&lower_changed, &source).bgra,
            rendered(&Scene::prepare(&lower_items, &source), &source).bgra
        );
        a.style.effect_radius = 0.;
        let zero_items = [lower, a, upper, downstream];
        let zero = lower_changed.updated(&zero_items, &source);
        if tool == Tool::Mosaic {
            assert!(matches!(zero.layers[1], Layer::Mosaic { .. }));
        }
        assert_eq!(
            rendered(&zero, &source).bgra,
            rendered(&Scene::prepare(&zero_items, &source), &source).bgra
        );
    }
}

#[test]
fn rounded_effect_crop_and_enlarged_output_preserve_source_pixels_and_global_phase() {
    let image = rounded_fixture();
    let source = source(&image);
    let crop = Rect {
        x: 1,
        y: 1,
        width: 17,
        height: 13,
    };
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut a = effect(tool, 3);
        a.style.effect_radius = 4.;
        a.rotation = 30.;
        let scene = Scene::prepare(&[a], &source);
        let full = rendered(&scene, &source);
        for zoom in [1., 1.5, 2.75] {
            let projection = crate::output_projection::OutputProjection::new(crop, zoom).unwrap();
            let mut output = projection.background(&source).unwrap();
            scene
                .apply_projected(&mut output, &source, &projection)
                .unwrap();
            for y in 0..output.height {
                for x in 0..output.width {
                    let sx = crop.x
                        + ((f64::from(x) + 0.5) * f64::from(crop.width) / f64::from(output.width))
                            .floor() as u32;
                    let sy = crop.y
                        + ((f64::from(y) + 0.5) * f64::from(crop.height) / f64::from(output.height))
                            .floor() as u32;
                    assert_eq!(
                        pixel(&output, x, y),
                        pixel(&full, sx, sy),
                        "{tool:?} zoom={zoom} at ({x},{y})"
                    );
                }
            }
        }
    }
}

#[test]
fn compound_brush_fill_and_pixel_eraser_ignore_new_effect_radius() {
    let image = fixture();
    let base_source = source(&image);
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut a = effect(tool, 3);
        a.set_effect_regions(
            vec![
                Bounds {
                    x: 3.,
                    y: 3.,
                    width: 4.,
                    height: 3.,
                },
                Bounds {
                    x: 12.,
                    y: 8.,
                    width: 3.,
                    height: 3.,
                },
            ],
            true,
        )
        .unwrap();
        let reference = rendered(&Scene::prepare(&[a.clone()], &base_source), &base_source);
        a.style.effect_radius = 23.;
        assert!(!a.supports_radius() && a.radius_controls(6.).is_empty());
        assert!(a.hit(Point { x: 3.5, y: 3.5 }, 0., true));
        assert_eq!(
            rendered(&Scene::prepare(&[a], &base_source), &base_source).bgra,
            reference.bgra
        );
    }
    for tool in [Tool::MosaicBrush, Tool::BlurBrush, Tool::PixelEraser] {
        let mut a = effect(tool, 3);
        let mut lower = Annotation::new(
            Tool::Rectangle,
            Point { x: 1., y: 1. },
            Style {
                filled: true,
                color: 0xf53b29,
                ..Default::default()
            },
        );
        lower.update(Point { x: 18., y: 16. });
        let reference = rendered(
            &Scene::prepare(&[lower.clone(), a.clone()], &base_source),
            &base_source,
        );
        a.style.effect_radius = 23.;
        assert!(!a.supports_radius() && a.radius_controls(6.).is_empty());
        assert_eq!(
            rendered(&Scene::prepare(&[lower, a], &base_source), &base_source).bgra,
            reference.bgra
        );
    }
    let mut fill_image = fixture();
    for p in fill_image.bgra.chunks_exact_mut(4) {
        p.copy_from_slice(&[41, 107, 173, 255]);
    }
    for y in 7..9 {
        for x in 5..10 {
            let offset = ((y * fill_image.width + x) * 4) as usize;
            fill_image.bgra[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    let fill_source = source(&fill_image);
    let mut fill = Annotation::new(
        Tool::LocalFill,
        Point { x: 4., y: 8. },
        Style {
            width: 2.,
            pencil_smooth: false,
            ..Default::default()
        },
    );
    fill.update(Point { x: 12., y: 8. });
    let reference = rendered(&Scene::prepare(&[fill.clone()], &fill_source), &fill_source);
    assert_ne!(reference.bgra, fill_image.bgra, "必须确实经过局部填充");
    fill.style.effect_radius = 23.;
    assert!(!fill.supports_radius() && fill.radius_controls(6.).is_empty());
    assert_eq!(
        rendered(&Scene::prepare(&[fill], &fill_source), &fill_source).bgra,
        reference.bgra
    );
}

#[test]
fn ordinary_effect_radius_controls_scale_and_outline_follow_dedicated_field() {
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut a = Annotation::new(
            tool,
            Point { x: 20., y: 30. },
            Style {
                radius: 8.,
                effect_radius: 21.,
                ..Default::default()
            },
        );
        a.update(Point { x: 320., y: 230. });
        assert!(a.supports_radius());
        let controls = a.radius_controls(6.);
        assert_eq!(controls.len(), 4);
        let mut legacy_changed = a.clone();
        legacy_changed.style.radius = 99.;
        assert_eq!(legacy_changed.radius_controls(6.), controls);
        assert_eq!(a.radius_after_delta(4.), 25.);
        a.set_corner_radius(25.);
        assert_eq!((a.style.radius, a.style.effect_radius), (8., 25.));
        assert!(a.segments().len() > 4, "实际效果对象轮廓须包含圆角曲线");
        assert!(!a.hit(Point { x: 20.5, y: 30.5 }, 0., true));
        assert!(a.hit(Point { x: 45., y: 30. }, 0., true));
        assert!(a.hit(Point { x: 19., y: 55. }, 1., true));
        a.scale_about(Point { x: 20., y: 30. }, 2.);
        assert_eq!((a.style.radius, a.style.effect_radius), (16., 50.));
        a.set_effect_regions(
            vec![Bounds {
                x: 20.,
                y: 30.,
                width: 300.,
                height: 200.,
            }],
            true,
        )
        .unwrap();
        assert!(!a.supports_radius() && a.radius_controls(6.).is_empty());
    }
}

#[test]
fn ordinary_effect_radius_cardinal_closed_edges_keep_zero_radius_legacy_pixels() {
    let image = rounded_fixture();
    let blur_reference = rounded_blur_reference(&image);
    let source = source(&image);
    for tool in [Tool::Mosaic, Tool::Blur] {
        for angle in [0., 90., 180., 270., -90., 450.] {
            for radius in [3., 99.] {
                let mut a = Annotation::new(
                    tool,
                    Point { x: -5., y: 2. },
                    Style {
                        block_size: 3,
                        radius: 8.,
                        effect_radius: radius,
                        ..Default::default()
                    },
                );
                a.update(Point { x: 14., y: 16. });
                a.rotation = angle;
                let scene = Scene::prepare(&[a.clone()], &source);
                let output = rendered(&scene, &source);
                for y in 0..image.height {
                    for x in 0..image.width {
                        let covered = rounded_pixel_oracle(&a, x, y);
                        assert_eq!(
                            a.hit(
                                Point {
                                    x: x as f32 + 0.5,
                                    y: y as f32 + 0.5
                                },
                                0.,
                                true
                            ),
                            covered
                        );
                        let expected = if !covered {
                            pixel(&image, x, y)
                        } else if tool == Tool::Mosaic {
                            global_cell_mean(&image, 3, x, y)
                        } else {
                            pixel(&blur_reference, x, y)
                        };
                        assert_eq!(
                            pixel(&output, x, y),
                            expected,
                            "{tool:?} angle={angle} radius={radius} at ({x},{y})"
                        );
                    }
                }
                if angle == 90. {
                    let boundary_y = if radius == 3. { 15 } else { 6 };
                    assert!(a.hit(
                        Point {
                            x: 11.5,
                            y: boundary_y as f32 + 0.5
                        },
                        0.,
                        true
                    ));
                    assert!(!a.hit(
                        Point {
                            x: 11.5001,
                            y: boundary_y as f32 + 0.5
                        },
                        0.,
                        true
                    ));
                }
            }
        }
        let mut near = Annotation::new(
            tool,
            Point { x: -5., y: 2. },
            Style {
                block_size: 3,
                effect_radius: 3.,
                ..Default::default()
            },
        );
        near.update(Point { x: 14., y: 16. });
        near.rotation = 89.999;
        assert!(near.hit(Point { x: 11.5, y: 15.5 }, 0., true));
        near.rotation = 90.001;
        assert!(!near.hit(Point { x: 11.5, y: 15.5 }, 0., true));
        let mut legacy = Annotation::new(
            tool,
            Point { x: -5., y: 2. },
            Style {
                block_size: 3,
                radius: 8.,
                effect_radius: 0.,
                ..Default::default()
            },
        );
        legacy.update(Point { x: 14., y: 16. });
        legacy.rotation = 90.;
        let legacy_output = rendered(&Scene::prepare(&[legacy], &source), &source);
        // 已有 f32 Rotation 的旧硬矩形边界像素必须保留；这里只修新增正圆角。
        for y in 10..=15 {
            assert_eq!(pixel(&legacy_output, 11, y), pixel(&image, 11, y));
        }
    }
}
