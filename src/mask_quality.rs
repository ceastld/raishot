//! 合成原图、标注、恢复笔迹与透明套索，不读取用户桌面或剪贴板。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    crop_mask::Polygon,
    export,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
    Result,
};
use std::path::Path;
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
pub fn run() -> Result<()> {
    let folder = Path::new("target/qa/mask");
    std::fs::create_dir_all(folder)?;
    let canvas = Rect::full(900, 650);
    let pixels: Vec<u8> = (0..canvas.width * canvas.height)
        .flat_map(|i| {
            let (x, y) = (i % canvas.width, i / canvas.width);
            let grey = if (x / 28 + y / 28) % 2 == 0 { 240 } else { 208 };
            [grey, grey, grey, 255]
        })
        .collect();
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let mut red = Annotation::new(
        Tool::Rectangle,
        p(60., 50.),
        Style {
            filled: true,
            color: 0xe15b67,
            ..Style::default()
        },
    );
    red.update(p(830., 600.));
    let mut restore = Annotation::new(
        Tool::PixelEraser,
        p(90., 310.),
        Style {
            width: 48.,
            ..Style::default()
        },
    );
    for i in 1..=320 {
        restore.update(p(
            90. + i as f32 * 2.15,
            310. + (i as f32 / 22.).sin() * 75.,
        ));
    }
    let mut arrow = Annotation::new(
        Tool::Arrow,
        p(220., 160.),
        Style {
            color: 0x008e73,
            width: 12.,
            arrow_head: crate::arrow_geometry::ArrowHead::Triangle,
            ..Style::default()
        },
    );
    arrow.update(p(700., 450.));
    let before = Scene::default().updated(&[red.clone(), arrow.clone()], &source);
    let mut image = source.read(canvas);
    before.apply(&mut image, canvas);
    export::save_png(image, &folder.join("before-restore.png"))?;
    let after = Scene::default().updated(&[red, restore, arrow], &source);
    let mut image = source.read(canvas);
    after.apply(&mut image, canvas);
    export::save_png(image, &folder.join("after-restore.png"))?;
    let mask = Polygon::new(&[p(110., 100.), p(780., 140.), p(720., 550.), p(150., 510.)])
        .ok_or("合成套索无效")?;
    let rect = mask.output_rect(canvas).ok_or("合成套索无边界")?;
    let mut image = source.read(rect);
    after.apply(&mut image, rect);
    mask.apply(&mut image, rect);
    export::save_png(
        crate::capture::Image {
            width: image.width,
            height: image.height,
            bgra: image.bgra.clone(),
            readback_ms: image.readback_ms,
        },
        &folder.join("lasso-alpha.png"),
    )?;
    export::save_image(image, &folder.join("lasso-white.jpg"))?;
    println!("{{\"test\":\"mask_restoration_and_transparent_png_white_jpeg\",\"width\":{},\"height\":{},\"ok\":true}}",rect.width,rect.height);
    Ok(())
}
