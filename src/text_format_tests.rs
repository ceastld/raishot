//! 文字字形排版、样式、透明度、旋转与编辑事务的验收。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
    raster,
    scene::{PixelPlane, Scene, SourcePixels},
    text_format::{self, Alignment, FontFamily},
    text_layout::TextLayout,
};
fn text(content: &str) -> Annotation {
    let mut a = Annotation::new(Tool::Text, Point { x: 30., y: 30. }, Style::default());
    a.update(Point { x: 290., y: 120. });
    a.text = content.into();
    a
}
#[test]
fn font_catalog_has_unique_preferred_fonts_and_can_restore_names() {
    let fonts = text_format::fonts();
    assert_eq!(fonts[0], "Microsoft YaHei UI");
    for (i, f) in fonts.iter().enumerate() {
        assert!(!f.starts_with('@'));
        assert_eq!(FontFamily::named(f), FontFamily(i));
        assert!(!fonts[..i].iter().any(|other| other.eq_ignore_ascii_case(f)));
    }
    assert_eq!(
        FontFamily::named("nonexistent-font-for-test"),
        FontFamily::default()
    );
}
#[test]
fn shared_layout_wraps_unicode_clusters_and_caret_hit_uses_same_positions() {
    let mut a = text("Rust 中文🙂 e\u{301}\n尾行");
    a.points[1].x = 145.;
    let layout = TextLayout::new(&a).unwrap();
    assert!(layout.rows.len() >= 3);
    assert!(layout.height >= layout.rows.iter().map(|r| r.height).sum::<f32>());
    for row in &layout.rows {
        assert!(a.text.is_char_boundary(row.range.start));
        assert!(a.text.is_char_boundary(row.range.end));
        let (p, h) = layout.position(row.range.start);
        assert!(h > 0.);
        assert_eq!(
            layout.hit(Point {
                x: p.x,
                y: p.y + h / 2.
            }),
            row.range.start
        );
    }
    for c in a.text.char_indices().filter(|(_, c)| *c == '🙂') {
        let (p, h) = layout.position(c.0);
        let hit = layout.hit(Point {
            x: p.x,
            y: p.y + h / 2.,
        });
        assert!(a.text.is_char_boundary(hit));
    }
    let empty = TextLayout::new(&text("")).unwrap();
    assert_eq!(empty.rows.len(), 1);
    assert_eq!(empty.hit(Point { x: 0., y: 0. }), 0);
}
#[test]
fn left_center_right_alignment_moves_glyphs_and_caret_together() {
    let mut a = text("Rust");
    let left = TextLayout::new(&a).unwrap();
    let width = left.rows[0].width;
    let base = left.position(0).0.x;
    let mut previous = 0;
    for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
        a.style.text.alignment = alignment;
        let layout = TextLayout::new(&a).unwrap();
        let expected = match alignment {
            Alignment::Left => 0.,
            Alignment::Center => (260. - width) / 2.,
            Alignment::Right => 260. - width,
        };
        assert!((layout.position(0).0.x - base - expected).abs() < 0.001);
        let (rect, image) = raster::text(&a, Rect::full(360, 180)).unwrap().unwrap();
        let x = (0..image.width)
            .find(|x| {
                (0..image.height).any(|y| image.bgra[((y * image.width + x) * 4 + 3) as usize] > 0)
            })
            .unwrap()
            + rect.x;
        if alignment != Alignment::Left {
            assert!(x > previous);
        }
        previous = x;
    }
}
#[test]
fn outline_background_alpha_and_rotated_region_share_identical_pixels() {
    let canvas = Rect::full(360, 220);
    let pixels = [90u8, 130, 170, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let mut a = text("中文 Rust🙂");
    a.style.text.font = FontFamily::named("Consolas");
    a.style.text.italic = true;
    a.style.text.outline = true;
    a.style.text.outline_width = 6.;
    a.style.text.background = true;
    a.style.text.background_opacity = 0.35;
    a.style.text.background_radius = 12.;
    a.style.opacity = 0.5;
    let (_, layer) = raster::text(&a, canvas).unwrap().unwrap();
    assert!(layer.bgra.chunks_exact(4).all(|p| p[3] <= 128));
    assert!(layer.bgra.chunks_exact(4).any(|p| p[3] > 120));
    assert!(layer.bgra.chunks_exact(4).any(|p| p[3] > 40 && p[3] < 50));
    a.rotate_about(crate::transform::center(a.local_bounds()), 27.);
    let scene = Scene::prepare(&[a], &source);
    assert!(scene.errors.is_empty());
    let mut full = source.read(canvas);
    scene.apply(&mut full, canvas);
    let crop = Rect {
        x: 40,
        y: 20,
        width: 250,
        height: 160,
    };
    let mut region = source.read(crop);
    scene.apply(&mut region, crop);
    for y in 0..crop.height {
        let i = (((y + crop.y) * canvas.width + crop.x) * 4) as usize;
        assert_eq!(
            &region.bgra[(y * crop.width * 4) as usize..((y + 1) * crop.width * 4) as usize],
            &full.bgra[i..i + (crop.width * 4) as usize]
        );
    }
}
#[test]
fn font_and_decoration_changes_are_atomic_and_preserve_mixed_styles_and_rotation() {
    let canvas = Rect::full(640, 480);
    let mut e = Editor::new(canvas, Some(canvas));
    let mut a = text("原文字");
    a.style.text.background = true;
    a.style.text.background_opacity = 0.35;
    a.rotate_about(crate::transform::center(a.local_bounds()), 35.);
    let anchor = a.to_world(a.points[0]);
    let id = e.document.insert(a.clone());
    e.tool = Tool::Edit;
    e.selected = Some(id);
    e.style = a.style;
    let mut s = e.style;
    s.text.font = FontFamily::named("Consolas");
    s.text.italic = true;
    e.set_style(s);
    let changed = e.document.get(id).unwrap();
    assert_eq!(changed.rotation, 35.);
    assert!(changed.to_world(changed.points[0]).distance(anchor) < 0.001);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    let mut b = text("第二个");
    b.style.text.background_opacity = 0.85;
    e.document.insert(b.clone());
    let mut rectangle =
        Annotation::new(Tool::Rectangle, Point { x: 350., y: 50. }, Style::default());
    rectangle.update(Point { x: 450., y: 150. });
    let rid = e.document.insert(rectangle.clone());
    e.select_all_objects();
    let before = e.document.items().to_vec();
    let mut s = e.style;
    s.text.alignment = Alignment::Center;
    e.set_style(s);
    assert_eq!(
        e.document.items()[0]
            .annotation
            .style
            .text
            .background_opacity,
        0.35
    );
    assert_eq!(
        e.document.items()[1]
            .annotation
            .style
            .text
            .background_opacity,
        0.85
    );
    assert_eq!(e.document.get(rid), Some(&rectangle));
    e.undo();
    assert_eq!(e.document.items(), before);
}
#[test]
fn formatted_draft_cancel_keeps_redo_and_enter_commits_one_transaction() {
    let canvas = Rect::full(640, 480);
    let mut e = Editor::new(canvas, Some(canvas));
    let a = text("已有");
    let id = e.document.insert(a.clone());
    e.document.remove(id);
    e.document.undo();
    e.tool = Tool::Edit;
    e.selected = Some(id);
    e.style = a.style;
    e.edit_text();
    let mut s = e.style;
    s.text.outline = true;
    s.text.background = true;
    e.set_style(s);
    e.set_text("修改中文🙂".into());
    assert!(e.cancel_text());
    assert_eq!(e.document.get(id), Some(&a));
    assert!(e.document.can_redo());
    e.selected = Some(id);
    e.edit_text();
    e.set_text("完成文字".into());
    e.set_style(s);
    e.finish_text();
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}
#[test]
fn glyph_layout_raii_releases_native_font_and_analysis_resources() {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS};
    let mut a = text("中文🙂 العربية");
    a.style.text.font = FontFamily::named("Consolas");
    for _ in 0..5 {
        drop(TextLayout::new(&a).unwrap());
    }
    let before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    for _ in 0..200 {
        drop(TextLayout::new(&a).unwrap());
    }
    let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    assert!(
        after <= before + 24,
        "GDI objects leaked: {before} -> {after}"
    );
}

#[test]
fn fallback_glyph_analysis_concurrent_build_and_drop_is_safe() {
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || {
                for i in 0..60 {
                    let mut a = text("Rust 中文🙂 e\u{301}\nالعربية 尾行");
                    a.style.text.font = FontFamily::named(if (worker + i) % 2 == 0 {
                        "Consolas"
                    } else {
                        "Microsoft YaHei UI"
                    });
                    a.points[1].x = 120. + (i % 4) as f32 * 12.;
                    let layout = TextLayout::new(&a).unwrap();
                    let _ = layout.position(12);
                    let _ = layout.selection(0..a.text.len());
                    assert!(layout.height > 0.);
                    if i % 10 == 0 {
                        assert!(raster::text(&a, Rect::full(360, 240)).unwrap().is_some());
                    }
                }
            });
        }
    });
}
