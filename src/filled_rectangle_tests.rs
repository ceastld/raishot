//! 填充矩形的有效几何回归；只合成自有像素，不创建窗口或 GPU。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    scene::{Layer, PixelPlane, Scene, SourcePixels},
};
use std::sync::Arc;

fn canvas() -> Rect {
    Rect::full(256, 216)
}
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn rectangle(width: f32) -> Annotation {
    let mut a = Annotation::new(
        Tool::Rectangle,
        p(64., 48.),
        Style {
            width,
            color: 0x2244dd,
            opacity: 0.5,
            filled: true,
            radius: 18.,
            dashed: true,
            ..Default::default()
        },
    );
    a.update(p(184., 144.));
    a
}
fn source(bytes: &[u8]) -> SourcePixels<'_> {
    SourcePixels {
        fill: None,
        canvas: canvas(),
        planes: vec![PixelPlane {
            rect: canvas(),
            bgra: bytes,
        }],
        overlay: None,
    }
}
fn render(scene: &Scene, source: &SourcePixels<'_>, region: Rect) -> Image {
    assert!(scene.errors.is_empty(), "{:?}", scene.errors);
    let mut image = source.read(region);
    scene.apply(&mut image, region);
    image
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * image.width + x) * 4) as usize;
    image.bgra[offset..offset + 4].try_into().unwrap()
}
fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.03, "{actual} != {expected}");
}

#[test]
fn filled_rectangle_keeps_metadata_for_switching_back_to_outline() {
    let bytes = [100, 100, 100, 255].repeat((canvas().width * canvas().height) as usize);
    let source = source(&bytes);
    let item = rectangle(40.);
    let original = item.clone();
    let scene = Scene::prepare(std::slice::from_ref(&item), &source);
    let Layer::Vector {
        mesh,
        segments,
        style,
        ..
    } = &scene.layers[0]
    else {
        panic!("填充矩形未使用矢量层");
    };
    assert_eq!(style.width, 0.);
    assert!(style.filled && segments.is_empty());
    assert_eq!(style.opacity, original.style.opacity);
    assert!(!mesh.indices.is_empty());
    assert!(mesh
        .vertices
        .iter()
        .all(|&p| item.local_bounds().contains(p, 0.001)));
    let filled = render(&scene, &source, canvas());
    assert_eq!(pixel(&filled, 55, 96), [100, 100, 100, 255]);
    assert_eq!(item, original);

    let mut outline = item.clone();
    outline.style.filled = false;
    assert_eq!(outline.style.width, 40.);
    assert!(outline.style.dashed);
    assert_eq!(outline.style.radius, original.style.radius);
    assert!(!outline.fill_only());
    let outlined = scene.updated(std::slice::from_ref(&outline), &source);
    let Layer::Vector {
        mesh,
        segments,
        style,
        ..
    } = &outlined.layers[0]
    else {
        panic!("空心矩形未使用矢量层");
    };
    assert_eq!(style.width, 40.);
    assert!(style.dashed && !segments.is_empty());
    assert!(mesh
        .vertices
        .iter()
        .any(|&p| !outline.local_bounds().contains(p, 0.001)));
}

#[test]
fn filled_rectangle_rounding_hit_and_rotated_region_ignore_saved_width() {
    for angle in [0., 31., 90.] {
        let mut thin = rectangle(2.);
        thin.style.radius = 28.;
        thin.rotation = angle;
        let mut wide = thin.clone();
        wide.style.width = 80.;
        for a in [&thin, &wide] {
            assert!(!a.hit(a.to_world(p(63., 96.)), 0., false));
            assert!(!a.hit(a.to_world(p(65., 49.)), 0., true));
            assert!(a.hit(a.to_world(p(124., 96.)), 0., false));
            assert!(a.hit(a.to_world(p(63.5, 96.)), 1., false));
        }
        assert_eq!(thin.render_bounds(), wide.render_bounds());
        assert_eq!(
            crate::raster::region(&thin, canvas()),
            crate::raster::region(&wide, canvas())
        );
        // 旋转圆角矩形的支持函数：矩形核心的投影加圆半径；不含存储线宽。
        let (sin, cos) = angle.to_radians().sin_cos();
        let extent_x = cos.abs() * (60. - 28.) + sin.abs() * (48. - 28.) + 28.;
        let extent_y = sin.abs() * (60. - 28.) + cos.abs() * (48. - 28.) + 28.;
        let actual = wide.render_bounds();
        near(actual.x, 124. - extent_x);
        near(actual.y, 96. - extent_y);
        near(actual.width, extent_x * 2.);
        near(actual.height, extent_y * 2.);
        if angle == 31. {
            assert!(actual.width < wide.bounds().width - 1.);
            assert!(actual.height < wide.bounds().height - 1.);
        }
        let mut outline = wide.clone();
        outline.style.filled = false;
        assert!(outline.hit(outline.to_world(p(63., 96.)), 0., false));
        assert_eq!(outline.render_bounds(), outline.bounds());
    }
}

#[test]
fn filled_rectangle_transparency_matches_fill_mesh_and_direct_raster() {
    let bytes = [0, 0, 0, 0].repeat((canvas().width * canvas().height) as usize);
    let source = source(&bytes);
    let thin = rectangle(2.);
    let mut wide = thin.clone();
    wide.style.width = 80.;
    let thin_scene = Scene::prepare(std::slice::from_ref(&thin), &source);
    let wide_scene = Scene::prepare(std::slice::from_ref(&wide), &source);
    let actual = render(&wide_scene, &source, canvas());
    assert_eq!(actual.bgra, render(&thin_scene, &source, canvas()).bgra);
    assert_eq!(pixel(&actual, 124, 96), [0xdd, 0x44, 0x22, 128]);
    assert_eq!(pixel(&actual, 55, 96), [0, 0, 0, 0]);
    assert_eq!(pixel(&actual, 65, 49), [0, 0, 0, 0]);
    assert_eq!(actual.bgra.chunks_exact(4).map(|p| p[3]).max(), Some(128));
    assert!(actual.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 128));
    let (thin_rect, thin_raster) = crate::raster::shape(&thin, canvas()).unwrap();
    let (rect, raster) = crate::raster::shape(&wide, canvas()).unwrap();
    assert_eq!(thin_rect, rect);
    assert_eq!(thin_raster.bgra, raster.bgra);
    let mut fallback = source.read(canvas());
    crate::raster::composite(&mut fallback, canvas(), &raster, rect);
    for (actual, expected) in actual.bgra.iter().zip(&fallback.bgra) {
        assert!((*actual as i16 - *expected as i16).abs() <= 1);
    }
    // 网格供 GPU 预览使用，直接光栅供输出使用：两者的实体边界都是填充轮廓。
    let Layer::Vector { mesh, .. } = &wide_scene.layers[0] else {
        panic!("填充矩形未使用矢量层");
    };
    let contours = mesh.boundary_contours().unwrap();
    let edges: Vec<_> = contours
        .iter()
        .flat_map(|c| crate::vector_path::contour_edges(c))
        .collect();
    assert!(crate::annotation::contains_path(p(124., 96.), &edges));
    assert!(!crate::annotation::contains_path(p(55., 96.), &edges));
    assert!(!crate::annotation::contains_path(p(65., 49.), &edges));
}

#[test]
fn filled_rectangle_rotated_clipping_matches_nonzero_roi_and_raster_fallback() {
    let bytes = [100, 100, 100, 255].repeat((canvas().width * canvas().height) as usize);
    let source = source(&bytes);
    let mut item = rectangle(80.);
    item.points = vec![p(-18.25, 40.75), p(138.75, 156.5)];
    item.rotation = -23.;
    item.style.radius = 37.75;
    item.style.opacity = 0.43;
    let scene = Scene::prepare(std::slice::from_ref(&item), &source);
    let full = render(&scene, &source, canvas());
    let roi = Rect {
        x: 17,
        y: 63,
        width: 107,
        height: 98,
    };
    let part = render(&scene, &source, roi);
    for y in 0..roi.height {
        for x in 0..roi.width {
            assert_eq!(pixel(&part, x, y), pixel(&full, x + roi.x, y + roi.y));
        }
    }
    let (rect, raster) = crate::raster::shape(&item, canvas()).unwrap();
    assert_eq!(rect.x, 0);
    assert!(rect.valid(canvas().width, canvas().height));
    let mut fallback = source.read(canvas());
    crate::raster::composite(&mut fallback, canvas(), &raster, rect);
    for (actual, expected) in full.bgra.iter().zip(&fallback.bgra) {
        assert!((*actual as i16 - *expected as i16).abs() <= 1);
    }
}

#[test]
fn filled_rectangle_downstream_mosaic_samples_only_fill_and_invalidates_on_radius_edit() {
    let bytes = [100, 100, 100, 255].repeat((canvas().width * canvas().height) as usize);
    let source = source(&bytes);
    let mut item = rectangle(80.);
    item.style.radius = 0.;
    let mut mosaic = Annotation::new(
        Tool::Mosaic,
        p(40., 32.),
        Style {
            block_size: 8,
            ..Default::default()
        },
    );
    mosaic.update(p(96., 80.));
    let before = Scene::prepare(&[item.clone(), mosaic.clone()], &source);
    let Layer::Mosaic { blocks: old } = &before.layers[1] else {
        panic!("下游马赛克层缺失");
    };
    let block = |blocks: &[crate::scene::Block], x, y| {
        blocks
            .iter()
            .find(|b| b.rect.x == x && b.rect.y == y)
            .unwrap()
            .color
    };
    assert_eq!(block(old, 40, 48), 0x646464);
    assert_eq!(block(old, 64, 48), 0x4354a1);
    item.style.radius = 30.;
    let after = before.updated(&[item.clone(), mosaic.clone()], &source);
    let Layer::Mosaic { blocks: new } = &after.layers[1] else {
        panic!("更新后的马赛克层缺失");
    };
    assert!(!Arc::ptr_eq(old, new));
    assert_eq!(block(new, 64, 48), 0x646464);
    let cold = Scene::prepare(&[item.clone(), mosaic.clone()], &source);
    assert_eq!(
        render(&after, &source, canvas()).bgra,
        render(&cold, &source, canvas()).bgra
    );
    item.style.width = 2.;
    let thin = after.updated(&[item, mosaic], &source);
    assert_eq!(
        render(&thin, &source, canvas()).bgra,
        render(&after, &source, canvas()).bgra
    );
}

#[test]
fn filled_rectangle_change_does_not_remove_other_tools_strokes() {
    let bytes = [0, 0, 0, 0].repeat((canvas().width * canvas().height) as usize);
    let source = source(&bytes);
    for tool in [Tool::Ellipse, Tool::Polyline, Tool::Arrow] {
        let mut item = rectangle(20.);
        item.tool = tool;
        item.style.dashed = false;
        if tool == Tool::Polyline {
            item.points = vec![p(64., 48.), p(184., 48.), p(184., 144.), p(64., 144.)];
        }
        assert!(!item.fill_only());
        assert_eq!(item.render_bounds(), item.bounds());
        let scene = Scene::prepare(std::slice::from_ref(&item), &source);
        let Layer::Vector {
            style, segments, ..
        } = &scene.layers[0]
        else {
            panic!("{tool:?} 未使用矢量层");
        };
        assert_eq!(style.width, 20., "{tool:?}");
        assert!(!segments.is_empty(), "{tool:?}");
        if tool != Tool::Arrow {
            assert!(item.hit(p(59., 96.), 0., false), "{tool:?}");
            assert!(
                pixel(&render(&scene, &source, canvas()), 59, 96)[3] > 0,
                "{tool:?}"
            );
        }
    }
}
