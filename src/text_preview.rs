//! 普通文字和序号只在可见局部按实际物理密度重绘，不改变原始布局或导出。
use crate::{
    annotation::{Annotation, Point, Tool},
    capture::Image,
    capture_view::CaptureView,
    geometry::Rect,
    output_projection::OutputProjection,
    scene::{Layer, Scene},
    Result,
};
use gpui::{RenderImage, Window};
use std::sync::Arc;

// 预算针对全部可见对象，不按图层数均分；普通小标签不会降低大文字对象的密度。
const PIXEL_BUDGET: u64 = 32_000_000;
const MAX_SIDE: u32 = 16_384;
type PreviewError = Box<dyn std::error::Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewSpec {
    layer: usize,
    rect: Rect,
    dimensions: (u32, u32),
    density_bits: u32,
    dpi_bits: u32,
}

#[derive(Clone)]
pub(crate) struct TextPreview {
    spec: PreviewSpec,
    annotation: Arc<Annotation>,
    pub(crate) image: Arc<RenderImage>,
}

impl TextPreview {
    pub(crate) fn layer(&self) -> usize {
        self.spec.layer
    }

    pub(crate) fn rect(&self) -> Rect {
        self.spec.rect
    }

    fn matches(&self, spec: PreviewSpec, annotation: &Annotation) -> bool {
        self.spec == spec && self.annotation.as_ref() == annotation
    }

    /// 同对象、同倍率与 DPI，只是可见范围变了（拖动、部分移出屏幕）。
    fn same_density(&self, spec: PreviewSpec, annotation: &Annotation) -> bool {
        self.spec.layer == spec.layer
            && self.spec.density_bits == spec.density_bits
            && self.spec.dpi_bits == spec.dpi_bits
            && self.annotation.as_ref() == annotation
    }

    /// 已有全密度预览覆盖本帧需要的范围时直接复用，平移不必重绘。
    fn covers(&self, spec: PreviewSpec, annotation: &Annotation) -> bool {
        full_density(self.spec)
            && full_density(spec)
            && self.spec.rect.intersection(spec.rect) == Some(spec.rect)
            && self.same_density(spec, annotation)
    }
}

/// 未因总预算或单边上限降低密度的预览。
fn full_density(spec: PreviewSpec) -> bool {
    let density = f64::from(f32::from_bits(spec.density_bits));
    let side = |length: u32| (f64::from(length) * density).ceil().max(1.);
    side(spec.rect.width) == f64::from(spec.dimensions.0)
        && side(spec.rect.height) == f64::from(spec.dimensions.1)
}

/// 可见范围四边各外扩半个可见尺寸，限制在原图内；平移重绘时多画一圈，之后的拖动直接复用。
fn panning_region(visible: Rect, full: Rect) -> Rect {
    let grow = |start: u32, length: u32, low: u32, high: u32| {
        let from = start.saturating_sub(length / 2).max(low);
        let to = start
            .saturating_add(length)
            .saturating_add(length / 2)
            .min(high);
        (from, to.saturating_sub(from))
    };
    let (x, width) = grow(visible.x, visible.width, full.x, full.x + full.width);
    let (y, height) = grow(visible.y, visible.height, full.y, full.y + full.height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn local_region(a: &Annotation, full: Rect, visible: Rect, density: f32) -> Result<Option<Rect>> {
    let b = a.bounds();
    let outline = if a.tool == Tool::Text && a.style.text.outline {
        a.style.text.outline_width
    } else {
        0.
    };
    let pad = f64::from(a.style.width.max(outline)) + 3. + 2. / f64::from(density);
    if ![b.x, b.y, b.width, b.height, a.style.width, outline]
        .iter()
        .all(|v| v.is_finite())
        || !pad.is_finite()
    {
        return Err("文字显示范围无效".into());
    }
    if b.width <= 0. || b.height <= 0. {
        return Ok(None);
    }
    let Some(clip) = full.intersection(visible) else {
        return Ok(None);
    };
    let left = (f64::from(b.x) - pad).floor().max(f64::from(clip.x));
    let top = (f64::from(b.y) - pad).floor().max(f64::from(clip.y));
    let right = (f64::from(b.x) + f64::from(b.width) + pad)
        .ceil()
        .min(f64::from(clip.x + clip.width));
    let bottom = (f64::from(b.y) + f64::from(b.height) + pad)
        .ceil()
        .min(f64::from(clip.y + clip.height));
    Ok((right > left && bottom > top).then(|| Rect {
        x: left as u32,
        y: top as u32,
        width: right as u32 - left as u32,
        height: bottom as u32 - top as u32,
    }))
}

fn dimensions(rect: Rect, density: f64, round_up: bool) -> (u32, u32) {
    let side = |length: u32| {
        let pixels = f64::from(length) * density;
        let pixels = if round_up {
            pixels.ceil()
        } else {
            pixels.floor()
        };
        pixels.clamp(1., f64::from(MAX_SIDE)) as u32
    };
    (side(rect.width), side(rect.height))
}

fn preview_specs(regions: &[(usize, Rect)], density: f32, dpi: f32) -> Result<Vec<PreviewSpec>> {
    if !density.is_finite() || density <= 0. || !dpi.is_finite() || dpi <= 0. {
        return Err("文字显示密度无效".into());
    }
    if regions.len() as u64 > PIXEL_BUDGET {
        return Err("文字对象数量超过显示预算".into());
    }
    if regions
        .iter()
        .any(|(_, rect)| rect.width == 0 || rect.height == 0)
    {
        return Err("文字显示区域为空".into());
    }
    let requested: u64 = regions
        .iter()
        .map(|(_, rect)| {
            let (w, h) = dimensions(*rect, f64::from(density), true);
            u64::from(w) * u64::from(h)
        })
        .sum();
    let limited = requested > PIXEL_BUDGET;
    let fit = if limited {
        (PIXEL_BUDGET as f64 / requested as f64).sqrt()
    } else {
        1.
    };
    let mut remaining = PIXEL_BUDGET;
    let mut next = Vec::with_capacity(regions.len());
    for (index, &(layer, rect)) in regions.iter().enumerate() {
        let (mut width, mut height) = dimensions(rect, f64::from(density) * fit, !limited);
        // 为之后的对象至少保留一个像素；极端细长/很多对象时仍严格有界。
        let available = remaining - (regions.len() - index - 1) as u64;
        if u64::from(width) * u64::from(height) > available {
            let shrink = (available as f64 / (f64::from(width) * f64::from(height))).sqrt();
            width = (f64::from(width) * shrink).floor().max(1.) as u32;
            height = (f64::from(height) * shrink).floor().max(1.) as u32;
            while u64::from(width) * u64::from(height) > available {
                if width >= height {
                    width -= 1;
                } else {
                    height -= 1;
                }
            }
        }
        remaining -= u64::from(width) * u64::from(height);
        next.push(PreviewSpec {
            layer,
            rect,
            dimensions: (width, height),
            density_bits: density.to_bits(),
            dpi_bits: dpi.to_bits(),
        });
    }
    Ok(next)
}

fn render_text(a: &Annotation, spec: PreviewSpec) -> Result<Image> {
    let (width, height) = spec.dimensions;
    let bytes = width as usize * height as usize * 4;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(bytes)?;
    bgra.resize(bytes, 0);
    let mut image = Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    };
    let projection = OutputProjection {
        source: spec.rect,
        width,
        height,
        sx: width as f32 / spec.rect.width as f32,
        sy: height as f32 / spec.rect.height as f32,
        origin: Point { x: 0., y: 0. },
    };
    crate::projected_text::paint(a, &mut image, &projection)?;
    Ok(image)
}

fn replace_previews(
    current: &mut Vec<TextPreview>,
    next: Vec<TextPreview>,
    retired: &mut Vec<Arc<RenderImage>>,
) {
    for old in std::mem::replace(current, next) {
        if !current
            .iter()
            .any(|current| Arc::ptr_eq(&current.image, &old.image))
            && !retired
                .iter()
                .any(|retired| Arc::ptr_eq(retired, &old.image))
        {
            retired.push(old.image);
        }
    }
}

impl CaptureView {
    fn replace_text_previews(&mut self, next: Vec<TextPreview>) {
        replace_previews(
            &mut self.text_previews,
            next,
            &mut self.retired_raster_previews,
        );
    }

    pub(crate) fn prepare_text_previews(&mut self, window: &Window, scene: &Scene) -> Result<()> {
        let result = self.next_text_previews(window, scene);
        match result {
            Ok((next, error)) => {
                self.replace_text_previews(next);
                error.map_or(Ok(()), Err)
            }
            Err(error) => {
                // 不保留已不对应本帧样式/视口的显示图；该层仍可用原始 Scene 位图。
                self.replace_text_previews(Vec::new());
                Err(error)
            }
        }
    }

    fn next_text_previews(
        &self,
        window: &Window,
        scene: &Scene,
    ) -> Result<(Vec<TextPreview>, Option<PreviewError>)> {
        if self.pin_mode
            && self
                .pin_annotation_preview
                .as_ref()
                .is_some_and(|preview| !preview.visible())
        {
            return Ok((Vec::new(), None));
        }
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let host = Rect::full(
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        if host.width == 0 || host.height == 0 {
            return Ok((Vec::new(), None));
        }
        let viewport = self.canvas_viewport(window);
        let full = self.source.full();
        let visible = if self.pin_mode {
            Some(crate::pin_annotation::shared_preview_region(self, window))
        } else {
            self.canvas_visible_source_region(window)?
        };
        let Some(visible) = visible else {
            return Ok((Vec::new(), None));
        };
        // Scene 的每层对应同索引 Annotation；结构层和底图像素不进入此缓存。
        let mut regions = Vec::new();
        for (layer, (item, a)) in scene.layers.iter().zip(&scene.annotations).enumerate() {
            if matches!(item, Layer::Raster { .. }) && matches!(a.tool, Tool::Text | Tool::Serial) {
                if let Some(rect) = local_region(a, full, visible, viewport.zoom)? {
                    regions.push((layer, rect));
                }
            }
        }
        let specs = preview_specs(&regions, viewport.zoom, dpi)?;
        let mut requested: u64 = specs
            .iter()
            .map(|spec| u64::from(spec.dimensions.0) * u64::from(spec.dimensions.1))
            .sum();
        let mut next = Vec::with_capacity(specs.len());
        let mut first_error = None;
        for mut spec in specs {
            let a = &scene.annotations[spec.layer];
            if let Some(cached) = self
                .text_previews
                .iter()
                .find(|cached| cached.matches(spec, a) || cached.covers(spec, a))
            {
                next.push(cached.clone());
                continue;
            }
            // 只因平移失效时外扩重绘；首显、缩放与编辑仍只画可见部分。
            if full_density(spec)
                && self
                    .text_previews
                    .iter()
                    .any(|cached| cached.same_density(spec, a))
            {
                let wider = local_region(a, full, panning_region(visible, full), viewport.zoom)?
                    .map(|rect| PreviewSpec {
                        rect,
                        dimensions: dimensions(rect, f64::from(viewport.zoom), true),
                        ..spec
                    })
                    .filter(|wider| full_density(*wider));
                if let Some(wider) = wider {
                    let pixels =
                        |s: PreviewSpec| u64::from(s.dimensions.0) * u64::from(s.dimensions.1);
                    let total = requested - pixels(spec) + pixels(wider);
                    if total <= PIXEL_BUDGET {
                        requested = total;
                        spec = wider;
                    }
                }
            }
            let result = render_text(a, spec).and_then(|image| {
                let frame = image::RgbaImage::from_raw(image.width, image.height, image.bgra)
                    .ok_or("文字显示图尺寸不一致")?;
                Ok(TextPreview {
                    spec,
                    annotation: Arc::new(a.clone()),
                    image: Arc::new(RenderImage::new(vec![image::Frame::new(frame)])),
                })
            });
            match result {
                Ok(preview) => next.push(preview),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        Ok((next, first_error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Point, Style};

    fn annotation() -> Annotation {
        let mut a = Annotation::new(
            Tool::Text,
            Point { x: 20., y: 15. },
            Style {
                color: 0,
                font_size: 28.,
                width: 4.,
                ..Default::default()
            },
        );
        a.update(Point { x: 190., y: 90. });
        a.text = "H H H".into();
        a
    }

    fn spec(rect: Rect, density: f32) -> PreviewSpec {
        preview_specs(&[(0, rect)], density, 1.).unwrap()[0]
    }

    fn preview(a: &Annotation, spec: PreviewSpec) -> TextPreview {
        TextPreview {
            annotation: Arc::new(a.clone()),
            spec,
            image: Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::new(1, 1),
            )])),
        }
    }

    #[test]
    fn text_display_cache_local_roi_and_fivefold_density_fit_without_full_canvas() {
        let mut a = annotation();
        a.points = vec![Point { x: 35.25, y: 40.5 }, Point { x: 135.25, y: 80.5 }];
        assert_eq!(
            local_region(
                &a,
                Rect::full(32_000, 32_000),
                Rect::full(32_000, 32_000),
                4.
            )
            .unwrap(),
            Some(Rect {
                x: 27,
                y: 33,
                width: 116,
                height: 55
            })
        );
        assert_eq!(
            local_region(
                &a,
                Rect::full(32_000, 32_000),
                Rect {
                    x: 100,
                    y: 20,
                    width: 300,
                    height: 100
                },
                4.,
            )
            .unwrap(),
            Some(Rect {
                x: 100,
                y: 33,
                width: 43,
                height: 55
            })
        );
        assert_eq!(
            local_region(
                &a,
                Rect::full(32_000, 32_000),
                Rect {
                    x: 500,
                    y: 500,
                    width: 10,
                    height: 10
                },
                4.
            )
            .unwrap(),
            None
        );
        let regions = [(0, Rect::full(1000, 1000)), (1, Rect::full(40, 20))];
        let specs = preview_specs(&regions, 5., 2.).unwrap();
        assert_eq!(specs[0].dimensions, (5000, 5000));
        assert_eq!(specs[1].dimensions, (200, 100));
        // DPI只进入缓存身份；zoom已经是物理密度，不能再乘2。
        assert_eq!(specs[0].density_bits, 5f32.to_bits());
        assert_eq!(specs[0].dpi_bits, 2f32.to_bits());
    }

    #[test]
    fn text_display_cache_bounds_aggregate_pixels_sides_and_invalid_density() {
        let regions = [
            (0, Rect::full(32_000, 32_000)),
            (1, Rect::full(32_000, 1)),
            (2, Rect::full(1, 32_000)),
        ];
        for density in [0.05, 1., 5., 1000.] {
            let specs = preview_specs(&regions, density, 1.5).unwrap();
            let pixels: u64 = specs
                .iter()
                .map(|s| u64::from(s.dimensions.0) * u64::from(s.dimensions.1))
                .sum();
            assert!(pixels <= PIXEL_BUDGET);
            assert!(specs.iter().all(|s| s.dimensions.0 > 0
                && s.dimensions.1 > 0
                && s.dimensions.0 <= MAX_SIDE
                && s.dimensions.1 <= MAX_SIDE));
        }
        for density in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(preview_specs(&regions, density, 1.).is_err());
        }
        assert!(preview_specs(&regions, 1., 0.).is_err());
        assert!(preview_specs(&[(0, Rect::full(0, 20))], 1., 1.).is_err());
        assert!(preview_specs(&[], 1., 1.).unwrap().is_empty());
    }

    #[test]
    fn text_display_cache_identity_covers_text_style_transform_zoom_dpi_and_crop() {
        let a = annotation();
        let s = spec(Rect::full(200, 100), 4.);
        let cached = preview(&a, s);
        assert!(cached.matches(s, &a));
        let mut changed = a.clone();
        changed.text.push('x');
        assert!(!cached.matches(s, &changed));
        changed = a.clone();
        changed.style.text.outline = true;
        assert!(!cached.matches(s, &changed));
        changed = a.clone();
        changed.style.font_size += 1.;
        assert!(!cached.matches(s, &changed));
        changed = a.clone();
        changed.rotation = 17.;
        assert!(!cached.matches(s, &changed));
        changed = a.clone();
        changed.translate(0.25, -0.5);
        assert!(!cached.matches(s, &changed));
        for changed in [
            PreviewSpec {
                density_bits: 4.01f32.to_bits(),
                ..s
            },
            PreviewSpec {
                dpi_bits: 1.5f32.to_bits(),
                ..s
            },
            PreviewSpec { layer: 1, ..s },
            PreviewSpec {
                rect: Rect {
                    x: 1,
                    width: 199,
                    ..s.rect
                },
                ..s
            },
            PreviewSpec {
                dimensions: (801, 400),
                ..s
            },
        ] {
            assert!(!cached.matches(changed, &a));
        }
    }

    #[test]
    fn text_display_cache_reuse_and_replacement_retire_each_texture_once() {
        let a = annotation();
        let old = preview(&a, spec(Rect::full(200, 100), 1.));
        let mut current = vec![old.clone()];
        let mut retired = Vec::new();
        replace_previews(&mut current, vec![old.clone()], &mut retired);
        assert!(retired.is_empty());
        let new = preview(&a, spec(Rect::full(200, 100), 4.));
        replace_previews(&mut current, vec![new.clone()], &mut retired);
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&retired[0], &old.image));
        replace_previews(&mut current, Vec::new(), &mut retired);
        assert_eq!(retired.len(), 2);
        assert!(Arc::ptr_eq(&retired[1], &new.image));
        current.push(old);
        replace_previews(&mut current, Vec::new(), &mut retired);
        assert_eq!(retired.len(), 2);
    }

    fn on_white(image: &Image) -> Image {
        let mut white = Image {
            width: image.width,
            height: image.height,
            bgra: vec![255; image.bgra.len()],
            readback_ms: 0.,
        };
        for (out, pixel) in white
            .bgra
            .chunks_exact_mut(4)
            .zip(image.bgra.chunks_exact(4))
        {
            crate::raster::over_pixel(out, &pixel[..3], f32::from(pixel[3]) / 255.);
        }
        white
    }

    // 黑字白底的10%→90%水平进入边缘；此标量只描述合成fixture，不外推真实GPUI画面。
    fn horizontal_edge_width(image: &Image) -> (usize, f64) {
        let mut count = 0;
        let mut widths = 0usize;
        for row in image.bgra.chunks_exact(image.width as usize * 4) {
            let ink = |x: usize| 255 - row[x * 4];
            let mut x = 1;
            while x < image.width as usize {
                if ink(x - 1) <= 25 && ink(x) > 25 {
                    let start = x - 1;
                    while x < image.width as usize && ink(x) > 25 && ink(x) < 230 {
                        x += 1;
                    }
                    if x < image.width as usize && ink(x) >= 230 {
                        count += 1;
                        widths += x - start;
                    }
                }
                x += 1;
            }
        }
        (count, widths as f64 / count.max(1) as f64)
    }

    #[test]
    fn text_display_quality_fourfold_redraw_has_sharper_edges_than_stretched_source() {
        let a = annotation();
        let original = a.clone();
        let full = Rect::full(200, 100);
        let (rect, source_text) = crate::raster::text(&a, full).unwrap().unwrap();
        let mut one = Image {
            width: 200,
            height: 100,
            bgra: vec![255; 200 * 100 * 4],
            readback_ms: 0.,
        };
        crate::raster::composite(&mut one, full, &source_text, rect);
        let one_rgba = image::RgbaImage::from_raw(one.width, one.height, one.bgra).unwrap();
        let stretched_rgba =
            image::imageops::resize(&one_rgba, 800, 400, image::imageops::FilterType::Triangle);
        let stretched = Image {
            width: 800,
            height: 400,
            bgra: stretched_rgba.clone().into_raw(),
            readback_ms: 0.,
        };
        let start = std::time::Instant::now();
        let redraw = on_white(&render_text(&a, spec(full, 4.)).unwrap());
        let redraw_ms = start.elapsed().as_secs_f64() * 1000.;
        let redraw_rgba =
            image::RgbaImage::from_raw(redraw.width, redraw.height, redraw.bgra.clone()).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa/pin-text-quality-20261009");
        std::fs::create_dir_all(&dir).unwrap();
        // 断言前保存两图：质量失败也能只查看自造文字图，不需要桌面截图。
        stretched_rgba
            .save(dir.join("source-1x-stretched-4x.png"))
            .unwrap();
        redraw_rgba
            .save(dir.join("target-density-4x-redraw.png"))
            .unwrap();
        let (source_edges, source_width) = horizontal_edge_width(&stretched);
        let (redraw_edges, redraw_width) = horizontal_edge_width(&redraw);
        let changed = stretched
            .bgra
            .chunks_exact(4)
            .zip(redraw.bgra.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        let metadata = format!("{{\"synthetic_only\":true,\"windows_created\":0,\"desktop_pixels_read\":0,\"source_density\":1,\"target_density\":4,\"width\":800,\"height\":400,\"changed_pixels\":{changed},\"source_edge_samples\":{source_edges},\"redraw_edge_samples\":{redraw_edges},\"source_mean_edge_width_px\":{source_width},\"redraw_mean_edge_width_px\":{redraw_width},\"single_redraw_ms\":{redraw_ms},\"stable_latency_claim\":false}}\n");
        std::fs::write(dir.join("quality-metadata.json"), &metadata).unwrap();
        println!("{metadata}");
        assert_eq!(a, original);
        assert!(changed > 200, "目标绘制仍在复用1x像素");
        assert!(
            source_edges > 40 && redraw_edges > 40,
            "合成文字缺少可测的实心边缘"
        );
        assert!(
            redraw_width + 0.5 < source_width,
            "目标重绘的边缘过渡未明显收窄：{redraw_width} vs {source_width}"
        );
    }

    #[test]
    fn text_display_quality_visible_crop_preserves_multiline_layout_and_full_redraw_phase() {
        let mut a = annotation();
        a.points = vec![Point { x: 20., y: 15. }, Point { x: 120., y: 175. }];
        a.style.font_size = 18.;
        a.text = "alpha beta gamma delta epsilon wrap\n第二段 中文字符".into();
        a.style.text.outline = true;
        a.style.text.outline_width = 1.5;
        a.style.text.background = true;
        let layout = || {
            crate::text_layout::TextLayout::new(&a)
                .unwrap()
                .rows
                .iter()
                .map(|r| (r.range.clone(), r.x, r.y, r.width, r.height))
                .collect::<Vec<_>>()
        };
        let before = layout();
        assert!(before.len() > 2);
        assert!(before[0].0.end < a.text.find('\n').unwrap());
        for rotation in [0., 17.] {
            a.rotation = rotation;
            let full = render_text(&a, spec(Rect::full(200, 200), 4.)).unwrap();
            let crop_rect = Rect {
                x: 30,
                y: 40,
                width: 110,
                height: 90,
            };
            let crop = render_text(&a, spec(crop_rect, 4.)).unwrap();
            for row in 0..crop.height as usize {
                let from = ((row + 160) * full.width as usize + 120) * 4;
                let to = row * crop.width as usize * 4;
                assert_eq!(
                    &full.bgra[from..from + crop.width as usize * 4],
                    &crop.bgra[to..to + crop.width as usize * 4],
                    "裁剪改变了源布局/字形相位，rotation={rotation}, row={row}"
                );
            }
            assert!(crop.bgra.chunks_exact(4).any(|p| p[3] > 0));
        }
        let after = crate::text_layout::TextLayout::new(&a)
            .unwrap()
            .rows
            .iter()
            .map(|r| (r.range.clone(), r.x, r.y, r.width, r.height))
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }

    #[test]
    fn text_display_cache_reuses_covering_full_density_preview_when_only_panning() {
        let a = annotation();
        let wide = spec(Rect::full(200, 600), 1.25);
        let cached = preview(&a, wide);
        let inside = spec(
            Rect {
                x: 10,
                y: 100,
                width: 150,
                height: 300,
            },
            1.25,
        );
        assert!(cached.covers(inside, &a));
        let outside = spec(
            Rect {
                x: 10,
                y: 400,
                width: 150,
                height: 300,
            },
            1.25,
        );
        assert!(!cached.covers(outside, &a) && cached.same_density(outside, &a));
        // 倍率、DPI 或对象一变都必须重绘。
        assert!(!cached.covers(spec(inside.rect, 1.5), &a));
        let mut other_dpi = inside;
        other_dpi.dpi_bits = 1.5_f32.to_bits();
        assert!(!cached.covers(other_dpi, &a));
        let mut edited = a.clone();
        edited.text.push('!');
        assert!(!cached.covers(inside, &edited));
        // 因预算或单边上限降过密度的预览不能代替全密度。
        let mut limited = wide;
        limited.dimensions.1 -= 1;
        assert!(!full_density(limited) && !preview(&a, limited).covers(inside, &a));
        let clamped = spec(Rect::full(200, 14_000), 1.25);
        assert_eq!(clamped.dimensions.1, MAX_SIDE);
        assert!(!full_density(clamped));
        assert!(!preview(&a, clamped).covers(inside, &a));
    }

    #[test]
    fn panning_region_grows_half_visible_size_and_stays_inside_source() {
        let full = Rect::full(700, 4000);
        let visible = Rect {
            x: 0,
            y: 1000,
            width: 700,
            height: 1200,
        };
        assert_eq!(
            panning_region(visible, full),
            Rect {
                x: 0,
                y: 400,
                width: 700,
                height: 2400,
            }
        );
        let top = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 1200,
        };
        assert_eq!(
            panning_region(top, full),
            Rect {
                x: 0,
                y: 0,
                width: 700,
                height: 1900,
            }
        );
    }

    /// 计时：典型长文字贴图按显示倍率整块重绘的耗时，各取 5 次最小值（`--ignored --nocapture`）。
    #[test]
    #[ignore]
    fn text_pin_redraw_performance() {
        let line = concat!(
            crate::brand_name!(),
            " 文字贴图渲染计时：The quick brown fox jumps over the lazy dog, 中英混排与标点。"
        );
        let text = (0..60)
            .map(|i| format!("{i:02} {line}"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        let source = crate::pin_text_source::prepare(text).unwrap();
        let a = source.annotation;
        let full = Rect::full(source.size.0, source.size.1);
        let best = |f: &mut dyn FnMut()| {
            (0..5)
                .map(|_| {
                    let started = std::time::Instant::now();
                    f();
                    started.elapsed().as_secs_f64() * 1000.
                })
                .fold(f64::INFINITY, f64::min)
        };
        println!("text pin {}x{}", full.width, full.height);
        println!(
            "scene 1x raster {:.1}ms",
            best(&mut || {
                crate::raster::text(&a, full).unwrap();
            })
        );
        for density in [1.25_f32, 1.5, 1.875, 2.5] {
            let rect = local_region(&a, full, full, density).unwrap().unwrap();
            let spec = preview_specs(&[(0, rect)], density, 1.25).unwrap()[0];
            let ms = best(&mut || {
                render_text(&a, spec).unwrap();
            });
            println!(
                "density {density}: {}x{} {ms:.1}ms",
                spec.dimensions.0, spec.dimensions.1
            );
        }
    }
}
