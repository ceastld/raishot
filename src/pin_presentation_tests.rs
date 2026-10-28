//! 实际共享外观模型的几何/alpha 合同定义；隔离阶段只定义，不编译或执行。
use super::*;
use crate::pin_model::Adjustments;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    scene::PixelPlane,
};

fn bounds() -> ScreenRect {
    ScreenRect {
        left: -1920,
        top: -700,
        width: 1000,
        height: 800,
    }
}
fn image(width: u32, height: u32, color: [u8; 4]) -> Image {
    Image {
        width,
        height,
        bgra: color.repeat(width as usize * height as usize),
        readback_ms: 17.5,
    }
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let offset = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[offset..offset + 4].try_into().unwrap()
}

fn background_projection(
    plan: PresentationPreviewPlan,
    region: PresentationRegion,
) -> OutputProjection {
    OutputProjection {
        source: plan.content,
        width: region.width,
        height: region.height,
        sx: plan.target.sx as f32,
        sy: plan.target.sy as f32,
        origin: Point {
            x: (f64::from(region.left) - plan.target.origin.x) as f32,
            y: (f64::from(region.top) - plan.target.origin.y) as f32,
        },
    }
}

fn assert_background_matches_linear(
    source: &SourcePixels<'_>,
    plan: PresentationPreviewPlan,
    region: PresentationRegion,
) {
    let projection = background_projection(plan, region);
    let actual = plan.render_unclipped_background(source, region).unwrap();
    for y in 0..region.height {
        for x in 0..region.width {
            assert_eq!(
                pixel(&actual, x, y),
                crate::pin_color_sample::linear_pixel(source, projection.source_center(x, y))
                    .unwrap(),
                "目标像素 ({x}, {y})"
            );
        }
    }
}

#[test]
fn unscaled_background_copy_keeps_nonzero_crop_and_clears_transparent_rgb() {
    let mut raw = image(23, 19, [21, 43, 65, 0]);
    for (index, pixel) in raw.bgra.chunks_exact_mut(4).enumerate() {
        pixel[0] = (index % 256) as u8;
        pixel[1] = ((index * 3) % 256) as u8;
        pixel[3] = [0, 1, 97, 254, 255][index % 5];
    }
    let before = raw.bgra.clone();
    let source = source_pixels(&raw);
    let plan = PresentationPreviewPlan::new(
        Rect {
            x: 4,
            y: 5,
            width: 11,
            height: 9,
        },
        SelectionFinishStyle::default(),
        PhysicalPoint { x: -3., y: 7. },
        1.,
    )
    .unwrap();
    let region = PresentationRegion {
        left: -2,
        top: 8,
        width: 8,
        height: 6,
    };
    let copied = read_unscaled_background(&source, background_projection(plan, region)).unwrap();
    assert!(copied.bgra.chunks_exact(4).any(|pixel| pixel[3] == 0));
    assert!(copied
        .bgra
        .chunks_exact(4)
        .filter(|pixel| pixel[3] == 0)
        .all(|pixel| pixel == [0, 0, 0, 0]));
    assert_background_matches_linear(&source, plan, region);
    assert_eq!(raw.bgra, before);
}

#[test]
fn unscaled_background_copy_keeps_multiple_planes_and_overlay_priority() {
    let left = [11, 22, 33, 97].repeat(3 * 6);
    let right = [44, 55, 66, 151].repeat(3 * 6);
    let lower = [77, 88, 99, 201].repeat(6 * 3);
    let overlay = [111, 122, 133, 0].repeat(2 * 3);
    let canvas = Rect::full(6, 6);
    let source = SourcePixels {
        canvas,
        fill: None,
        planes: vec![
            PixelPlane {
                rect: Rect::full(3, 6),
                bgra: &left,
            },
            PixelPlane {
                rect: Rect {
                    x: 3,
                    y: 0,
                    width: 3,
                    height: 6,
                },
                bgra: &right,
            },
            PixelPlane {
                rect: Rect {
                    x: 0,
                    y: 3,
                    width: 6,
                    height: 3,
                },
                bgra: &lower,
            },
        ],
        overlay: Some(PixelPlane {
            rect: Rect {
                x: 2,
                y: 1,
                width: 2,
                height: 3,
            },
            bgra: &overlay,
        }),
    };
    let plan = PresentationPreviewPlan::new(
        canvas,
        SelectionFinishStyle::default(),
        PhysicalPoint { x: 4., y: 6. },
        1.,
    )
    .unwrap();
    let region = PresentationRegion {
        left: 4,
        top: 6,
        width: 6,
        height: 6,
    };
    let copied = read_unscaled_background(&source, background_projection(plan, region)).unwrap();
    assert_eq!(pixel(&copied, 0, 0), [11, 22, 33, 97]);
    assert_eq!(pixel(&copied, 5, 0), [44, 55, 66, 151]);
    assert_eq!(pixel(&copied, 5, 5), [77, 88, 99, 201]);
    assert_eq!(pixel(&copied, 2, 3), [0; 4]);
    assert_background_matches_linear(&source, plan, region);
}

#[test]
fn unscaled_background_copy_falls_back_for_gaps_and_incomplete_plane_pixels() {
    let left = [11, 22, 33, 97].repeat(2 * 4);
    let right = [44, 55, 66, 151].repeat(2 * 4);
    let canvas = Rect::full(5, 4);
    let mut source = SourcePixels {
        canvas,
        fill: None,
        planes: vec![
            PixelPlane {
                rect: Rect::full(2, 4),
                bgra: &left,
            },
            PixelPlane {
                rect: Rect {
                    x: 3,
                    y: 0,
                    width: 2,
                    height: 4,
                },
                bgra: &right,
            },
        ],
        overlay: None,
    };
    let plan = PresentationPreviewPlan::new(
        canvas,
        SelectionFinishStyle::default(),
        PhysicalPoint { x: 0., y: 0. },
        1.,
    )
    .unwrap();
    let region = PresentationRegion {
        left: 0,
        top: 0,
        width: 5,
        height: 4,
    };
    let projection = background_projection(plan, region);
    assert!(read_unscaled_background(&source, projection).is_none());
    assert!(plan.render_unclipped_background(&source, region).is_err());
    source.fill = Some([91, 83, 71, 41]);
    assert!(read_unscaled_background(&source, projection).is_some());
    assert_background_matches_linear(&source, plan, region);
    source.planes[1].bgra = &right[..right.len() - 1];
    assert!(read_unscaled_background(&source, projection).is_none());
    assert!(plan.render_unclipped_background(&source, region).is_err());
}

#[test]
fn unscaled_background_copy_falls_back_for_fractional_projection_and_canvas_edges() {
    let mut raw = image(7, 5, [21, 43, 65, 97]);
    for (index, pixel) in raw.bgra.chunks_exact_mut(4).enumerate() {
        pixel[0] = (index * 7) as u8;
        pixel[3] = if index % 2 == 0 { 0 } else { 201 };
    }
    let source = source_pixels(&raw);
    for (origin, zoom) in [
        (PhysicalPoint { x: -2.25, y: 3. }, 1.),
        (PhysicalPoint { x: -2., y: 3.5 }, 1.),
        (PhysicalPoint { x: -2., y: 3. }, 0.75),
        (PhysicalPoint { x: -2., y: 3. }, 1.25),
        (PhysicalPoint { x: -2., y: 3. }, 1.),
    ] {
        let plan = PresentationPreviewPlan::new(
            source.canvas,
            SelectionFinishStyle::default(),
            origin,
            zoom,
        )
        .unwrap();
        // 最后一组越出 canvas 左缘，保留原路径的边缘钳位。
        let region = PresentationRegion {
            left: -3,
            top: 3,
            width: 7,
            height: 5,
        };
        assert!(read_unscaled_background(&source, background_projection(plan, region)).is_none());
        assert_background_matches_linear(&source, plan, region);
    }
}

#[test]
#[ignore = "按需比较生产 1:1 首显底图快路与原线性采样成本"]
fn unscaled_background_copy_performance() {
    use std::{hint::black_box, time::Instant};
    for (width, height) in [(800, 600), (1920, 1080)] {
        let raw = image(width, height, [21, 43, 65, 255]);
        let source = source_pixels(&raw);
        let plan = PresentationPreviewPlan::new(
            source.canvas,
            SelectionFinishStyle::default(),
            PhysicalPoint { x: 8., y: 8. },
            1.,
        )
        .unwrap();
        let region = PresentationRegion {
            left: 8,
            top: 8,
            width,
            height,
        };
        let projection = background_projection(plan, region);
        let start = Instant::now();
        let copied = black_box(plan.render_unclipped_background(&source, region).unwrap());
        let copied_time = start.elapsed();
        let start = Instant::now();
        let linear =
            black_box(display_content(&source, &Scene::default(), None, projection).unwrap());
        let linear_time = start.elapsed();
        assert_eq!(copied.bgra, linear.bgra);
        eprintln!(
            "{width}x{height}: production_copy={:.3}ms baseline_linear={:.3}ms speedup={:.2}x",
            copied_time.as_secs_f64() * 1000.,
            linear_time.as_secs_f64() * 1000.,
            linear_time.as_secs_f64() / copied_time.as_secs_f64()
        );
    }
}
fn selected(width: u32, height: u32) -> Rect {
    Rect {
        x: 19,
        y: 23,
        width,
        height,
    }
}
fn close(a: PhysicalPoint, b: PhysicalPoint) {
    assert!(
        (a.x - b.x).abs() < 1e-10,
        "x 坐标未保持同一投影：{a:?} / {b:?}"
    );
    assert!(
        (a.y - b.y).abs() < 1e-10,
        "y 坐标未保持同一投影：{a:?} / {b:?}"
    );
}

#[test]
fn screenshot_payload_retains_raw_allocation_and_signed_one_pixel_content_origin() {
    let source = image(31, 25, [11, 33, 55, 117]);
    let raw_ptr = source.bgra.as_ptr();
    let raw = source.bgra.clone();
    let style = SelectionFinishStyle {
        border_enabled: true,
        border_thickness: 1.,
        ..Default::default()
    };
    let payload = ScreenshotPinPayload::new(bounds(), selected(31, 25), source, style).unwrap();
    let p = payload.presentation();
    assert_eq!(payload.raw().bgra.as_ptr(), raw_ptr);
    assert_eq!(payload.raw().bgra, raw);
    assert_eq!(
        p.content_screen(),
        ScreenRect {
            left: -1901,
            top: -677,
            width: 31,
            height: 25
        }
    );
    assert_eq!(
        p.outer_screen(),
        ScreenRect {
            left: -1902,
            top: -678,
            width: 33,
            height: 27
        }
    );
    let content = p.layout().content_rect;
    assert_eq!(
        i64::from(p.outer_screen().left) + i64::from(content.x),
        -1901
    );
    assert_eq!(i64::from(p.outer_screen().top) + i64::from(content.y), -677);
    let _ = payload.render_source_size().unwrap();
    assert_eq!(payload.raw().bgra, raw);
    assert_eq!(payload.raw().readback_ms, 17.5);
}

#[test]
fn screenshot_payload_rejects_mismatched_dimensions_and_bgra_without_normalizing_pixels() {
    for raw in [
        image(2, 3, [1, 2, 3, 4]),
        Image {
            width: 3,
            height: 2,
            bgra: vec![0; 23],
            readback_ms: 0.,
        },
    ] {
        assert!(ScreenshotPinPayload::new(
            bounds(),
            selected(3, 2),
            raw,
            SelectionFinishStyle::default()
        )
        .is_err());
    }
    for rect in [
        Rect::full(0, 2),
        Rect {
            x: 999,
            y: 0,
            width: 2,
            height: 2,
        },
        Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 2,
        },
    ] {
        assert!(PinPresentation::new(bounds(), rect, SelectionFinishStyle::default()).is_err());
    }
}

#[test]
fn pin_inside_border_preserves_capture_style_and_content_bounds() {
    for enabled in [false, true] {
        let source = SelectionFinishStyle {
            border_inside: true,
            border_enabled: enabled,
            border_thickness: 2.25,
            remember_enabled: true,
            ..Default::default()
        };
        let p = PinPresentation::new(bounds(), selected(11, 9), source).unwrap();
        assert_eq!(p.source_style(), source);
        assert_eq!(p.effective_style(), source);
        assert!(source.border_inside);
        assert_eq!(p.layout().padding.left, 0);
        assert_eq!(source.layout(11, 9).unwrap().padding.left, 0);
        assert_eq!(p.outer_screen(), p.content_screen());
    }
}

#[test]
fn fullscreen_pin_border_matches_capture_pixels_and_keeps_selected_side_after_zoom() {
    let bounds = ScreenRect {
        left: -40,
        top: -24,
        width: 40,
        height: 24,
    };
    let selected = Rect::full(bounds.width, bounds.height);
    let content_pixel = [23, 71, 119, 255];
    let border_pixel = [0x56, 0x34, 0x12, 255];
    for inside in [false, true] {
        let style = SelectionFinishStyle {
            border_enabled: true,
            border_inside: inside,
            border_thickness: 3.,
            border_color: 0xff12_3456,
            ..Default::default()
        };
        let raw = image(40, 24, content_pixel);
        let payload = ScreenshotPinPayload::new(bounds, selected, raw, style).unwrap();
        let capture = style.compose(image(40, 24, content_pixel)).unwrap();
        let pin = payload.render_source_size().unwrap();
        assert_eq!((pin.width, pin.height), (capture.width, capture.height));
        assert_eq!(pin.bgra, capture.bgra);
        if inside {
            assert_eq!(payload.presentation().outer_screen(), bounds);
        }
        for zoom in [0.5, 1., 1.25, 2.] {
            let plan = PresentationOutputPlan::new(selected, style, zoom).unwrap();
            let output = plan
                .render(&source_pixels(payload.raw()), &Scene::default(), None)
                .unwrap();
            let (width, height) = if inside { (40., 24.) } else { (46., 30.) };
            // 窗口可缩小；普通输出沿用至少原尺寸的既有规则。
            let export_zoom = zoom.max(1.);
            assert_eq!(
                plan.physical_size(),
                (
                    (width * export_zoom).round() as u32,
                    (height * export_zoom).round() as u32
                )
            );
            let content = plan.content_bounds();
            assert_eq!(pixel(&output, 0, output.height / 2), border_pixel);
            assert_eq!(
                pixel(&output, content.left.ceil() as u32, output.height / 2),
                if inside { border_pixel } else { content_pixel }
            );
            assert_eq!(
                pixel(&output, output.width / 2, output.height / 2),
                content_pixel
            );
        }
        assert!(payload
            .raw()
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel == content_pixel));
    }
}

#[test]
fn pin_preview_border_contours_follow_inside_outside_at_fractional_scale() {
    let origin = PhysicalPoint { x: -3.25, y: 5.75 };
    for inside in [false, true] {
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 5.25,
            border_enabled: true,
            border_inside: inside,
            border_thickness: 2.25,
            ..Default::default()
        };
        let plan = PresentationPreviewPlan::new(Rect::full(31, 25), style, origin, 1.375).unwrap();
        assert_eq!(plan.style(), style);
        let (outer, inner) = plan.border_contours();
        let edge = plan.contour(0.);
        if inside {
            assert_eq!(outer, edge);
            assert_eq!(inner.bounds.left, origin.x + 2.25 * 1.375);
            assert_eq!(inner.radius_x, (5.25 - 2.25) * 1.375);
            assert_eq!(plan.outer_bounds(), plan.content_bounds());
        } else {
            assert_eq!(inner, edge);
            assert_eq!(outer.bounds.left, origin.x - 2.25 * 1.375);
            assert_eq!(outer.radius_x, (5.25 + 2.25) * 1.375);
        }
    }
    let thick = PresentationPreviewPlan::new(
        Rect::full(3, 5),
        SelectionFinishStyle {
            border_enabled: true,
            border_inside: true,
            border_thickness: 8.,
            ..Default::default()
        },
        origin,
        1.,
    )
    .unwrap();
    assert!(thick.border_contours().1.bounds.width <= 0.);
}

#[test]
fn shared_corner_radius_keeps_pin_border_pixels_and_contours_with_content() {
    let square = SelectionFinishStyle {
        corner_radius: 0.,
        border_enabled: true,
        border_thickness: 3.,
        border_color: 0xff12_3456,
        ..Default::default()
    };
    let rounded = crate::selection_finish::FinishField::Radius.applied(square, 4.);
    let disabled = crate::selection_finish::FinishField::Radius.applied(rounded, 0.);
    let origin = PhysicalPoint { x: -3.25, y: 5.75 };
    for original in [square, rounded, disabled] {
        for inside in [false, true] {
            let style = SelectionFinishStyle {
                border_inside: inside,
                ..original
            };
            let radius = style.geometry(15, 13).radius;
            for zoom in [1., 1.375] {
                let plan =
                    PresentationPreviewPlan::new(Rect::full(15, 13), style, origin, zoom).unwrap();
                let (outer, inner) = plan.border_contours();
                let (outer_radius, inner_radius) = if radius == 0. {
                    (0., 0.)
                } else if inside {
                    (radius * zoom, (radius - 3.) * zoom)
                } else {
                    ((radius + 3.) * zoom, radius * zoom)
                };
                assert_eq!(
                    (outer.radius_x, outer.radius_y),
                    (outer_radius, outer_radius)
                );
                assert_eq!(
                    (inner.radius_x, inner.radius_y),
                    (inner_radius, inner_radius)
                );
                let local = if inside { 0.5 } else { -2.5 };
                let corner = PhysicalPoint {
                    x: origin.x + local * zoom,
                    y: origin.y + local * zoom,
                };
                assert_eq!(
                    plan.target.decoration(corner, DecorationLayer::Border),
                    if radius == 0. {
                        [0x56, 0x34, 0x12, 255]
                    } else {
                        [0; 4]
                    },
                    "{style:?}, zoom={zoom}"
                );
                // 阴影的外扩轮廓仍保留自然圆扩散，不受直角边框修正影响。
                assert_eq!(plan.contour(3.).radius_x, (radius + 3.) * zoom);
            }
        }
    }
}

#[test]
fn exhausted_inside_border_fills_center_in_capture_and_scaled_pin_output() {
    for (width, height, thickness) in [(3, 5, 1.5), (4, 6, 2.), (3, 5, 8.)] {
        for rounded in [false, true] {
            let style = SelectionFinishStyle {
                corner_enabled: rounded,
                corner_radius: 1.5,
                border_enabled: true,
                border_inside: true,
                border_thickness: thickness,
                border_color: 0xffff_0000,
                ..Default::default()
            };
            let raw = image(width, height, [23, 71, 119, 255]);
            let capture = style
                .compose(image(width, height, [23, 71, 119, 255]))
                .unwrap();
            assert_eq!(pixel(&capture, width / 2, height / 2), [0, 0, 255, 255]);
            for zoom in [1., 1.25, 2.] {
                let output = PresentationOutputPlan::new(Rect::full(width, height), style, zoom)
                    .unwrap()
                    .render(&source_pixels(&raw), &Scene::default(), None)
                    .unwrap();
                assert_eq!(
                    pixel(&output, output.width / 2, output.height / 2),
                    [0, 0, 255, 255]
                );
            }
        }
    }
}

#[test]
fn border_antialias_preserves_subpixel_width_and_fractional_straight_edges() {
    for inside in [false, true] {
        for zoom in [0.125, 0.375, 1.375, 2.25] {
            let style = SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 12.,
                border_enabled: true,
                border_inside: inside,
                border_thickness: 1.25,
                border_color: 0xff12_3456,
                ..Default::default()
            };
            let origin = PhysicalPoint { x: 0.25, y: 0.75 };
            let plan =
                PresentationPreviewPlan::new(Rect::full(100, 100), style, origin, zoom).unwrap();
            let thickness = f64::from(style.border_thickness) * zoom;
            let (left, right) = if inside {
                (origin.x, origin.x + thickness)
            } else {
                (origin.x - thickness, origin.x)
            };
            let y = origin.y + 50. * zoom;
            let mut total_alpha = 0_u32;
            for x in -4..5 {
                let center = f64::from(x) + 0.5;
                let overlap = (right.min(center + 0.5) - left.max(center - 0.5)).max(0.);
                let expected = (overlap * 255.).round() as u8;
                let pixel = plan
                    .target
                    .decoration(PhysicalPoint { x: center, y }, DecorationLayer::Border);
                assert_eq!(pixel[3], expected, "inside={inside}, zoom={zoom}, x={x}");
                if expected > 0 {
                    assert_eq!(&pixel[..3], &[0x56, 0x34, 0x12]);
                }
                total_alpha += u32::from(pixel[3]);
            }
            assert!((f64::from(total_alpha) / 255. - thickness).abs() <= 2. / 255.);
        }
    }
}

#[test]
fn fractional_border_shadow_padding_rounds_outward_and_keeps_content_dimensions() {
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 5.25,
        border_enabled: true,
        border_thickness: 2.25,
        shadow_enabled: true,
        shadow_blur: 4.25,
        ..Default::default()
    };
    let p = PinPresentation::new(bounds(), selected(31, 25), style).unwrap();
    let layout = p.layout();
    // 阴影从外侧边框外沿投出：2.25 + 4.25 = 6.5，向外取整为 7。
    assert_eq!((layout.output_width, layout.output_height), (45, 39));
    assert_eq!(
        layout.content_rect,
        Rect {
            x: 7,
            y: 7,
            width: 31,
            height: 25
        }
    );
    assert_eq!(layout.geometry.radius, 5.25);
    assert_eq!(p.content_screen().width, 31);
    assert_eq!(p.content_screen().height, 25);
}

#[test]
fn checked_global_origin_rejects_overflow_instead_of_saturating_or_casting() {
    let border = SelectionFinishStyle {
        border_enabled: true,
        border_thickness: 1.,
        ..Default::default()
    };
    for rect in [
        ScreenRect {
            left: i32::MIN,
            top: 0,
            width: 2,
            height: 2,
        },
        ScreenRect {
            left: 0,
            top: i32::MIN,
            width: 2,
            height: 2,
        },
        ScreenRect {
            left: i32::MAX - 2,
            top: 0,
            width: 2,
            height: 2,
        },
        ScreenRect {
            left: 0,
            top: i32::MAX - 2,
            width: 2,
            height: 2,
        },
    ] {
        assert!(PinPresentation::new(rect, Rect::full(2, 2), border).is_err());
        assert!(
            PinPresentation::new(rect, Rect::full(2, 2), SelectionFinishStyle::default()).is_ok()
        );
    }
    let invalid_source = ScreenRect {
        left: i32::MAX - 1,
        top: 0,
        width: 2,
        height: 2,
    };
    assert!(PinPresentation::new(
        invalid_source,
        Rect::full(1, 1),
        SelectionFinishStyle::default()
    )
    .is_err());
}

#[test]
fn undecorated_output_is_byte_exact_including_straight_alpha_and_hidden_rgb() {
    let raw = Image {
        width: 3,
        height: 1,
        bgra: vec![4, 5, 6, 0, 11, 22, 33, 97, 60, 70, 80, 255],
        readback_ms: 2.5,
    };
    let expected = raw.bgra.clone();
    let p = ScreenshotPinPayload::new(
        bounds(),
        selected(3, 1),
        raw,
        SelectionFinishStyle::default(),
    )
    .unwrap();
    let out = p.render_source_size().unwrap();
    assert_eq!(out.bgra, expected);
    assert_eq!(p.raw().bgra, expected);
    assert_eq!((out.width, out.height), (3, 1));
    assert_eq!(out.readback_ms, 2.5);
    assert!(p.presentation().decoration_parts().is_empty());
}

#[test]
fn outside_square_border_never_overwrites_any_content_pixel_or_alpha() {
    for alpha in [0, 97, 255] {
        let style = SelectionFinishStyle {
            border_inside: false,
            border_enabled: true,
            border_thickness: 2.25,
            border_color: 0x9933_5577,
            shadow_enabled: true,
            shadow_blur: 3.25,
            ..Default::default()
        };
        let raw = image(11, 9, [23, 45, 67, alpha]);
        let p = ScreenshotPinPayload::new(bounds(), selected(11, 9), raw, style).unwrap();
        let out = p.render_source_size().unwrap();
        let content = p.presentation().layout().content_rect;
        for y in 0..9 {
            for x in 0..11 {
                assert_eq!(
                    pixel(&out, content.x + x, content.y + y),
                    [23, 45, 67, alpha]
                );
            }
        }
        assert!(pixel(&out, content.x - 1, content.y + 4)[3] > 0);
    }
}

#[test]
fn rounded_mask_uses_shared_coverage_once_and_preserves_raw_partial_alpha() {
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 4.,
        ..Default::default()
    };
    let p = ScreenshotPinPayload::new(
        bounds(),
        selected(11, 9),
        image(11, 9, [20, 40, 60, 97]),
        style,
    )
    .unwrap();
    let out = p.render_source_size().unwrap();
    for y in 0..9 {
        for x in 0..11 {
            let coverage = p.presentation().content_coverage(PhysicalPoint {
                x: f64::from(x) + 0.5,
                y: f64::from(y) + 0.5,
            });
            assert_eq!(pixel(&out, x, y)[3], (97. * coverage).round() as u8);
            assert_eq!(pixel(p.raw(), x, y), [20, 40, 60, 97]);
            if pixel(&out, x, y)[3] == 0 {
                assert_eq!(pixel(&out, x, y), [0; 4]);
            }
        }
    }
    assert_eq!(pixel(&out, 5, 4), [20, 40, 60, 97]);
}

#[test]
fn rounded_outer_border_keeps_interior_and_antialiases_the_pixel_area() {
    let corner = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 4.,
        ..Default::default()
    };
    let style = SelectionFinishStyle {
        border_enabled: true,
        border_thickness: 1.,
        border_color: 0x9911_3355,
        ..corner
    };
    let p = ScreenshotPinPayload::new(
        bounds(),
        selected(11, 9),
        image(11, 9, [50, 90, 170, 97]),
        style,
    )
    .unwrap();
    let base = corner.compose(image(11, 9, [50, 90, 170, 97])).unwrap();
    let out = p.render_source_size().unwrap();
    let plan = p.presentation();
    let layout = plan.layout();
    let border = plan
        .render_decoration(DecorationPart {
            region: Rect::full(layout.output_width, layout.output_height),
            layer: DecorationLayer::Border,
        })
        .unwrap();
    let mut partial_edges = 0;
    for y in 0..9 {
        for x in 0..11 {
            let sd = layout
                .geometry
                .signed_distance(f64::from(x) + 0.5, f64::from(y) + 0.5);
            let edge = pixel(
                &border,
                layout.content_rect.x + x,
                layout.content_rect.y + y,
            );
            if sd <= -std::f64::consts::FRAC_1_SQRT_2 {
                assert_eq!(edge[3], 0);
                assert_eq!(
                    pixel(&out, layout.content_rect.x + x, layout.content_rect.y + y),
                    pixel(&base, x, y)
                );
            } else if edge[3] > 0 && edge[3] < 0x99 {
                partial_edges += 1;
            }
        }
    }
    assert!(partial_edges > 0);
}

// 独立模拟预览的 straight-alpha 层序，以真实几何/真实边带比对延迟 compose。
fn over(dst: &mut [u8], source: [u8; 4]) {
    let sa = u32::from(source[3]);
    if sa == 0 {
        return;
    }
    let da = u32::from(dst[3]);
    let alpha = sa * 255 + da * (255 - sa);
    for c in 0..3 {
        let value = u32::from(source[c]) * sa * 255 + u32::from(dst[c]) * da * (255 - sa);
        dst[c] = ((value + alpha / 2) / alpha) as u8;
    }
    dst[3] = ((alpha + 127) / 255) as u8;
}
fn put_part(destination: &mut Image, part: DecorationPart, pixels: &Image) {
    for y in 0..part.region.height {
        for x in 0..part.region.width {
            let i = ((part.region.y + y) as usize * destination.width as usize
                + (part.region.x + x) as usize)
                * 4;
            over(&mut destination.bgra[i..i + 4], pixel(pixels, x, y));
        }
    }
}

#[test]
fn preview_layers_and_corner_alpha_match_delayed_source_output_on_tiny_and_fractional_shapes() {
    for (width, height, radius) in [(11, 9, 4.), (3, 3, 1.5), (31, 25, 5.25)] {
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: radius,
            shadow_enabled: true,
            shadow_blur: 3.25,
            shadow_opacity: 0.55,
            shadow_color: 0xaa11_3355,
            border_enabled: true,
            border_inside: true,
            border_thickness: 2.25,
            border_color: 0x9933_5577,
            ..Default::default()
        };
        let p = ScreenshotPinPayload::new(
            bounds(),
            selected(width, height),
            image(width, height, [50, 90, 170, 97]),
            style,
        )
        .unwrap();
        let plan = p.presentation();
        let layout = plan.layout();
        let parts = plan.decoration_parts();
        let mut preview = image(layout.output_width, layout.output_height, [0; 4]);
        for part in parts
            .iter()
            .copied()
            .filter(|part| part.layer == DecorationLayer::Shadow)
        {
            put_part(&mut preview, part, &plan.render_decoration(part).unwrap());
        }
        for y in 0..height {
            for x in 0..width {
                let outer = PhysicalPoint {
                    x: f64::from(layout.content_rect.x + x) + 0.5,
                    y: f64::from(layout.content_rect.y + y) + 0.5,
                };
                let mut source = pixel(p.raw(), x, y);
                source[3] = (f64::from(source[3]) * plan.content_coverage(outer)).round() as u8;
                if source[3] == 0 {
                    source.fill(0);
                }
                let i = ((layout.content_rect.y + y) as usize * preview.width as usize
                    + (layout.content_rect.x + x) as usize)
                    * 4;
                over(&mut preview.bgra[i..i + 4], source);
            }
        }
        for part in parts
            .iter()
            .copied()
            .filter(|part| part.layer == DecorationLayer::Border)
        {
            put_part(&mut preview, part, &plan.render_decoration(part).unwrap());
        }
        assert_eq!(preview.bgra, p.render_source_size().unwrap().bgra);
        assert!(p.raw().bgra.chunks_exact(4).all(|p| p == [50, 90, 170, 97]));
    }
}

#[test]
fn decoration_tiles_are_bounded_disjoint_per_layer_and_reuse_without_content_or_origin() {
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 5.25,
        shadow_enabled: true,
        shadow_blur: 4.25,
        border_enabled: true,
        border_thickness: 2.25,
        ..Default::default()
    };
    let source = ScreenRect {
        left: -20000,
        top: -400,
        width: 20000,
        height: 200,
    };
    let p = PinPresentation::new(source, Rect::full(20000, 200), style).unwrap();
    let parts = p.decoration_parts();
    for (index, part) in parts.iter().enumerate() {
        assert!(part
            .region
            .valid(p.layout().output_width, p.layout().output_height));
        assert!(part.region.width <= 4096 && part.region.height <= 4096);
        for other in &parts[index + 1..] {
            if part.layer == other.layer {
                assert!(part.region.intersection(other.region).is_none());
            }
        }
    }
    let total: u64 = parts
        .iter()
        .filter(|p| p.layer == DecorationLayer::Border)
        .map(|p| u64::from(p.region.width) * u64::from(p.region.height))
        .sum();
    assert!(total < u64::from(p.layout().output_width) * u64::from(p.layout().output_height) / 2);
    let moved = PinPresentation::new(
        ScreenRect {
            left: 100,
            top: 100,
            ..source
        },
        Rect::full(20000, 200),
        style,
    )
    .unwrap();
    assert_eq!(p.decoration_key(), moved.decoration_key());
    assert_eq!(p.decoration_parts(), moved.decoration_parts());
    let same = PinPresentation::new(
        source,
        Rect::full(20000, 200),
        SelectionFinishStyle {
            remember_enabled: true,
            ..style
        },
    )
    .unwrap();
    assert_eq!(p.decoration_key(), same.decoration_key());
    let different = PinPresentation::new(
        source,
        Rect::full(20000, 200),
        SelectionFinishStyle {
            border_color: 0xff12_3456,
            ..style
        },
    )
    .unwrap();
    assert_ne!(p.decoration_key(), different.decoration_key());
    assert!(p
        .render_decoration(DecorationPart {
            region: Rect::full(4097, 1),
            layer: DecorationLayer::Border
        })
        .is_err());
}

#[test]
fn fractional_projection_uses_one_outer_rounding_without_independent_border_rounds() {
    let style = SelectionFinishStyle {
        border_enabled: true,
        border_thickness: 1.,
        ..Default::default()
    };
    let p = PinPresentation::new(bounds(), selected(31, 25), style).unwrap();
    let output = p
        .output_projection(1.25, PinOrientation::default())
        .unwrap();
    assert_eq!(output.physical_size(), (41, 34));
    let start = output.map_content(PhysicalPoint { x: 0., y: 0. });
    let end = output.map_content(PhysicalPoint { x: 31., y: 25. });
    close(
        start,
        PhysicalPoint {
            x: 41. / 33.,
            y: 34. / 27.,
        },
    );
    close(
        end,
        PhysicalPoint {
            x: 32. * 41. / 33.,
            y: 26. * 34. / 27.,
        },
    );
    assert!(start.x.fract() > 0. && start.y.fract() > 0.);
    for point in [
        PhysicalPoint { x: -0.5, y: 0.25 },
        PhysicalPoint { x: 15.5, y: 12.5 },
        PhysicalPoint { x: 31., y: 25. },
    ] {
        close(output.unmap_content(output.map_content(point)), point);
    }
    assert_eq!(
        p.preview_projection((41, 34), PinOrientation::default())
            .unwrap(),
        output
    );
}

#[test]
fn all_quarter_turn_flip_projections_match_existing_pixel_orientation_centers() {
    let raw = Image {
        width: 2,
        height: 3,
        bgra: (0..6_u8)
            .flat_map(|n| [n, n + 10, n + 20, n * 40])
            .collect(),
        readback_ms: 0.,
    };
    let p = ScreenshotPinPayload::new(
        bounds(),
        selected(2, 3),
        raw,
        SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            ..Default::default()
        },
    )
    .unwrap();
    let base = p.render_source_size().unwrap();
    for turns in 0..4 {
        for flip_x in [false, true] {
            for flip_y in [false, true] {
                let orientation = PinOrientation::new(turns, flip_x, flip_y);
                let expected = Adjustments {
                    turns,
                    flip_x,
                    flip_y,
                    ..Default::default()
                }
                .apply(&base)
                .unwrap();
                let projection = p.presentation().output_projection(1., orientation).unwrap();
                assert_eq!(
                    projection.physical_size(),
                    (expected.width, expected.height)
                );
                for y in 0..base.height {
                    for x in 0..base.width {
                        let source = PhysicalPoint {
                            x: f64::from(x) + 0.5,
                            y: f64::from(y) + 0.5,
                        };
                        let target = projection.map_outer(source);
                        assert_eq!(
                            pixel(&expected, target.x.floor() as u32, target.y.floor() as u32),
                            pixel(&base, x, y)
                        );
                        close(projection.unmap_outer(target), source);
                    }
                }
                let zoomed = p
                    .presentation()
                    .output_projection(1.25, orientation)
                    .unwrap();
                for corner in zoomed.content_quad() {
                    assert!(corner.x >= 0. && corner.y >= 0.);
                    assert!(corner.x <= f64::from(zoomed.physical_size().0));
                    assert!(corner.y <= f64::from(zoomed.physical_size().1));
                    close(zoomed.map_content(zoomed.unmap_content(corner)), corner);
                }
            }
        }
    }
}

#[test]
fn output_plan_keeps_existing_ties_even_minimum_scale_and_counts_outer_budget() {
    let p =
        PinPresentation::new(bounds(), selected(1, 3), SelectionFinishStyle::default()).unwrap();
    assert_eq!(
        p.output_projection(1.5, PinOrientation::default())
            .unwrap()
            .physical_size(),
        (2, 4)
    );
    for zoom in [0.1, 0.5, 1.] {
        assert_eq!(
            p.output_projection(zoom, PinOrientation::default())
                .unwrap()
                .physical_size(),
            (1, 3)
        );
    }
    for zoom in [0., f32::NAN, f32::INFINITY, f32::MAX] {
        assert!(p
            .output_projection(zoom, PinOrientation::default())
            .is_err());
    }
    for (width, height) in [(1000, 32000), (65535, 1)] {
        let source = ScreenRect {
            left: -1000,
            top: -500,
            width,
            height,
        };
        assert!(PinPresentation::new(
            source,
            Rect::full(width, height),
            SelectionFinishStyle::default()
        )
        .is_ok());
        assert!(PinPresentation::new(
            source,
            Rect::full(width, height),
            SelectionFinishStyle {
                border_enabled: true,
                border_thickness: 1.,
                ..Default::default()
            }
        )
        .is_err());
    }
    assert!(p
        .preview_projection((0, 3), PinOrientation::default())
        .is_err());
}

#[test]
fn dpi_conversion_is_final_and_does_not_change_layout_or_shared_projection() {
    let p = PinPresentation::new(
        bounds(),
        selected(31, 25),
        SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            ..Default::default()
        },
    )
    .unwrap();
    let before = p;
    let projection = p
        .preview_projection((41, 34), PinOrientation::new(2, true, false))
        .unwrap();
    let physical = projection.map_content(PhysicalPoint { x: 3.5, y: 6.5 });
    for dpi in [1., 1.25, 1.5, 2.] {
        let dip = physical.to_dip(dpi).unwrap();
        close(
            PhysicalPoint {
                x: dip.x * dpi,
                y: dip.y * dpi,
            },
            physical,
        );
    }
    for dpi in [0., -1., f64::NAN, f64::INFINITY, f64::MIN_POSITIVE] {
        assert!(physical.to_dip(dpi).is_none());
    }
    assert_eq!(p, before);
    assert_eq!(
        projection,
        p.preview_projection((41, 34), PinOrientation::new(2, true, false))
            .unwrap()
    );
}

#[test]
fn screenshot_payload_into_parts_moves_original_pixels_and_preserves_inside_border() {
    let raw = image(31, 25, [10, 20, 30, 97]);
    let ptr = raw.bgra.as_ptr();
    let payload = ScreenshotPinPayload::new(
        bounds(),
        selected(31, 25),
        raw,
        SelectionFinishStyle {
            border_inside: true,
            border_enabled: true,
            ..Default::default()
        },
    )
    .unwrap();
    let (raw, presentation) = payload.into_parts();
    assert_eq!(raw.bgra.as_ptr(), ptr);
    assert!(presentation.effective_style().border_inside);
}

fn source_pixels(raw: &Image) -> SourcePixels<'_> {
    let canvas = Rect::full(raw.width, raw.height);
    SourcePixels {
        canvas,
        fill: None,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &raw.bgra,
        }],
        overlay: None,
    }
}

#[test]
fn actual_output_plan_identity_matches_existing_compose_without_mutating_source() {
    let raw = image(37, 29, [23, 71, 119, 128]);
    let before = raw.bgra.clone();
    let source = source_pixels(&raw);
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 6.25,
        shadow_enabled: true,
        shadow_blur: 4.25,
        border_enabled: true,
        border_inside: true,
        border_thickness: 1.75,
        border_color: 0x9076_5432,
        ..Default::default()
    };
    let plan = PresentationOutputPlan::new(Rect::full(37, 29), style, 1.).unwrap();
    let expected = style.compose(image(37, 29, [23, 71, 119, 128])).unwrap();
    assert_eq!(plan.physical_size(), (47, 39));
    assert_eq!(
        plan.render(&source, &Scene::default(), None).unwrap().bgra,
        expected.bgra
    );
    assert_eq!(raw.bgra, before);
}

#[test]
fn target_output_projection_keeps_nonzero_crop_and_one_whole_grid_rounding() {
    let crop = Rect {
        x: 17,
        y: 13,
        width: 31,
        height: 25,
    };
    let style = SelectionFinishStyle {
        border_enabled: true,
        border_thickness: 1.,
        ..Default::default()
    };
    let plan = PresentationOutputPlan::new(crop, style, 1.25).unwrap();
    let p = plan.scene_projection();
    assert_eq!(plan.physical_size(), (41, 34));
    assert_eq!(p.source, crop);
    let bounds = plan.content_bounds();
    close(
        PhysicalPoint {
            x: bounds.left,
            y: bounds.top,
        },
        PhysicalPoint {
            x: 41. / 33.,
            y: 34. / 27.,
        },
    );
    for source in [
        Point { x: 17., y: 13. },
        Point { x: 33.25, y: 25.75 },
        Point { x: 48., y: 38. },
    ] {
        let roundtrip = p.unmap(p.map(source));
        assert!((roundtrip.x - source.x).abs() < 0.0001);
        assert!((roundtrip.y - source.y).abs() < 0.0001);
    }
    for zoom in [0.1, 0.5, 1.] {
        assert_eq!(
            PresentationOutputPlan::new(crop, style, zoom)
                .unwrap()
                .physical_size(),
            (33, 27)
        );
    }
    assert!(PresentationOutputPlan::new(Rect::full(1000, 32000), style, 1.).is_err());
}

#[test]
fn preview_body_and_corner_partitions_cover_fractional_signed_grid_once() {
    for (width, height, radius, zoom, origin) in [
        (31, 25, 5.25, 1.375, PhysicalPoint { x: 3.25, y: 5.75 }),
        (3, 3, 1.5, 0.35, PhysicalPoint { x: -0.25, y: -0.75 }),
        (11, 9, 0., 1.25, PhysicalPoint { x: 7.25, y: 13.75 }),
    ] {
        let plan = PresentationPreviewPlan::new(
            Rect::full(width, height),
            SelectionFinishStyle {
                corner_enabled: radius > 0.,
                corner_radius: radius,
                ..Default::default()
            },
            origin,
            zoom,
        )
        .unwrap();
        let mut parts = plan.body_regions();
        parts.extend(plan.corner_regions());
        let raster = plan.raster;
        let mut coverage = vec![0_u8; raster.width as usize * raster.height as usize];
        for part in parts {
            for y in 0..part.height {
                for x in 0..part.width {
                    let local_x = i64::from(part.left) + i64::from(x) - i64::from(raster.left);
                    let local_y = i64::from(part.top) + i64::from(y) - i64::from(raster.top);
                    assert!(
                        local_x >= 0
                            && local_y >= 0
                            && local_x < i64::from(raster.width)
                            && local_y < i64::from(raster.height)
                    );
                    coverage[local_y as usize * raster.width as usize + local_x as usize] += 1;
                }
            }
        }
        assert!(coverage.into_iter().all(|value| value == 1));
    }
}

#[test]
fn target_preview_corner_scene_mask_and_alpha_keep_complete_grid_phase() {
    let raw = image(83, 67, [31, 67, 127, 128]);
    let before = raw.bgra.clone();
    let source = source_pixels(&raw);
    let crop = Rect {
        x: 17,
        y: 13,
        width: 31,
        height: 25,
    };
    let origin = PhysicalPoint { x: 3.25, y: 5.75 };
    let plan = PresentationPreviewPlan::new(
        crop,
        SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 6.25,
            ..Default::default()
        },
        origin,
        1.375,
    )
    .unwrap();
    let mut mark = Annotation::new(
        Tool::Rectangle,
        Point { x: 17.5, y: 13.5 },
        Style {
            filled: true,
            color: 0x00ff00,
            opacity: 0.75,
            ..Default::default()
        },
    );
    mark.update(Point { x: 23.5, y: 19.5 });
    let scene = Scene::prepare(&[mark], &source);
    let mask = Polygon::new(&[
        Point { x: 17.25, y: 13.25 },
        Point { x: 47.75, y: 13.5 },
        Point { x: 47.5, y: 37.75 },
        Point { x: 17.5, y: 37.5 },
    ])
    .unwrap();
    let full_projection = OutputProjection {
        source: crop,
        width: 64,
        height: 64,
        sx: 1.375,
        sy: 1.375,
        origin: Point {
            x: -origin.x as f32,
            y: -origin.y as f32,
        },
    };
    let full = display_content(&source, &scene, Some(&mask), full_projection).unwrap();
    let mut partial_alpha = 0;
    for region in plan.corner_regions() {
        let corner = plan
            .render_corner(&source, &scene, Some(&mask), region)
            .unwrap();
        for y in 0..region.height {
            for x in 0..region.width {
                let wx = region.left as u32 + x;
                let wy = region.top as u32 + y;
                let mut expected = pixel(&full, wx, wy);
                expected[3] = (f64::from(expected[3])
                    * plan.target.coverage(PhysicalPoint {
                        x: f64::from(wx) + 0.5,
                        y: f64::from(wy) + 0.5,
                    }))
                .round() as u8;
                if expected[3] == 0 {
                    expected.fill(0);
                }
                assert_eq!(pixel(&corner, x, y), expected);
                partial_alpha += usize::from(expected[3] > 0 && expected[3] < 255);
            }
        }
    }
    assert!(partial_alpha > 0);
    assert_eq!(raw.bgra, before);
}

#[test]
fn actual_target_output_recomputes_edge_alpha_instead_of_stretching_source_finish() {
    let raw = image(31, 25, [23, 45, 67, 97]);
    let source = source_pixels(&raw);
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 5.25,
        border_enabled: true,
        border_thickness: 1.25,
        border_color: 0x9955_3311,
        shadow_enabled: true,
        shadow_blur: 3.25,
        ..Default::default()
    };
    let plan = PresentationOutputPlan::new(Rect::full(31, 25), style, 2.75).unwrap();
    let out = plan.render(&source, &Scene::default(), None).unwrap();
    let baked = style.compose(image(31, 25, [23, 45, 67, 97])).unwrap();
    let stretched = OutputProjection::new(Rect::full(baked.width, baked.height), 2.75)
        .unwrap()
        .background(&source_pixels(&baked))
        .unwrap();
    // 留白 = 外侧边框 1.25 + 阴影 3.25，向外取整 5px，再按 2.75 倍缩放。
    assert_eq!((out.width, out.height), (113, 96));
    assert!(
        out.bgra
            .iter()
            .zip(&stretched.bgra)
            .filter(|(a, b)| a != b)
            .count()
            > 100
    );
    assert_eq!(pixel(&out, out.width / 2, out.height / 2), [23, 45, 67, 97]);
    assert_eq!(pixel(&out, 0, 0), [0; 4]);
}

#[test]
fn preview_cache_geometry_preserves_fractional_origin_and_rejects_invalid_spans() {
    let content = Rect::full(31, 25);
    let style = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 5.25,
        ..Default::default()
    };
    let plan =
        PresentationPreviewPlan::new(content, style, PhysicalPoint { x: 3.25, y: 5.75 }, 1.375)
            .unwrap();
    let changed =
        PresentationPreviewPlan::new(content, style, PhysicalPoint { x: 3.5, y: 5.75 }, 1.375)
            .unwrap();
    assert_ne!(plan, changed);
    assert_eq!(plan.content_bounds().left, 3.25);
    assert_eq!(plan.contour(1.25).radius_x, (5.25 + 1.25) * 1.375);
    assert_eq!(plan.outer_bounds().width, 31. * 1.375);
    for zoom in [0., f64::NAN, f64::INFINITY, f64::MIN_POSITIVE] {
        assert!(PresentationPreviewPlan::new(
            content,
            style,
            PhysicalPoint { x: 3.25, y: 5.75 },
            zoom
        )
        .is_err());
    }
    assert!(PresentationPreviewPlan::new(
        content,
        style,
        PhysicalPoint {
            x: f64::from(i32::MAX),
            y: 0.
        },
        1.
    )
    .is_err());
}

#[test]
fn actual_fractional_content_roi_matches_corner_phase_and_transparent_edges() {
    let mut raw = image(83, 67, [0; 4]);
    // 不同颜色与透明度的梯度会区分 nearest/linear，透明像素还保留隐藏 RGB。
    for y in 0..raw.height {
        for x in 0..raw.width {
            let at = (y as usize * raw.width as usize + x as usize) * 4;
            raw.bgra[at..at + 4].copy_from_slice(&[
                (x * 7 + y * 13) as u8,
                (x * 11 + y * 17) as u8,
                (x * 19 + y * 23) as u8,
                [0, 97, 128, 255][(x + y) as usize % 4],
            ]);
        }
    }
    let before = raw.bgra.clone();
    let source = source_pixels(&raw);
    let plan = PresentationPreviewPlan::new(
        Rect {
            x: 17,
            y: 13,
            width: 31,
            height: 25,
        },
        SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 5.25,
            ..Default::default()
        },
        PhysicalPoint { x: -3.25, y: 1.75 },
        1.375,
    )
    .unwrap();
    let mut mark = Annotation::new(
        Tool::Rectangle,
        Point { x: 17.5, y: 13.5 },
        Style {
            filled: true,
            color: 0x00ff00,
            opacity: 0.75,
            ..Default::default()
        },
    );
    mark.update(Point { x: 23.5, y: 19.5 });
    let scene = Scene::prepare(&[mark], &source);
    let region = PresentationRegion {
        left: -6,
        top: -1,
        width: 15,
        height: 9,
    };
    let roi = plan
        .render_content_region(&source, &scene, None, region)
        .unwrap();
    assert_eq!(pixel(&roi, 0, 0), [0; 4]);
    assert!(roi.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 97));
    let raw_roi = plan
        .render_content_region(&source, &Scene::default(), None, region)
        .unwrap();
    let mut differs_from_nearest = 0;
    for y in 0..region.height {
        for x in 0..region.width {
            let absolute = PhysicalPoint {
                x: f64::from(region.left) + f64::from(x) + 0.5,
                y: f64::from(region.top) + f64::from(y) + 0.5,
            };
            let source_point = Point {
                x: (absolute.x as f32 + 3.25) / 1.375 + 17.,
                y: (absolute.y as f32 - 1.75) / 1.375 + 13.,
            };
            let mut expected =
                crate::pin_color_sample::linear_pixel(&source, source_point).unwrap();
            let coverage = plan.target.coverage(absolute);
            expected[3] = (f64::from(expected[3]) * coverage).round() as u8;
            if expected[3] == 0 {
                expected.fill(0);
            }
            assert_eq!(pixel(&raw_roi, x, y), expected);
            let mut nearest = source.sample(source_point);
            nearest[3] = (f64::from(nearest[3]) * coverage).round() as u8;
            if nearest[3] == 0 {
                nearest.fill(0);
            }
            differs_from_nearest += usize::from(expected != nearest);
        }
    }
    assert!(differs_from_nearest > 10);
    assert_ne!(roi.bgra, raw_roi.bgra, "Scene 必须实际改变梯度内容");
    let mut compared = 0;
    for corner_region in plan.corner_regions() {
        let corner = plan
            .render_corner(&source, &scene, None, corner_region)
            .unwrap();
        for y in 0..corner_region.height {
            for x in 0..corner_region.width {
                let rx = i64::from(corner_region.left) + i64::from(x) - i64::from(region.left);
                let ry = i64::from(corner_region.top) + i64::from(y) - i64::from(region.top);
                if rx >= 0
                    && ry >= 0
                    && rx < i64::from(region.width)
                    && ry < i64::from(region.height)
                {
                    assert_eq!(pixel(&roi, rx as u32, ry as u32), pixel(&corner, x, y));
                    compared += 1;
                }
            }
        }
    }
    assert!(compared > 0);
    for invalid in [
        PresentationRegion { width: 0, ..region },
        PresentationRegion {
            width: 4097,
            ..region
        },
        PresentationRegion {
            left: i32::MAX,
            ..region
        },
        PresentationRegion {
            top: i32::MAX,
            ..region
        },
    ] {
        assert!(plan
            .render_content_region(&source, &scene, None, invalid)
            .is_err());
    }
    assert_eq!(raw.bgra, before);
}
