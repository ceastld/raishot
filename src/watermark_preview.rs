//! 水印显示按可见范围重绘；缓存只持有有界显示图，原图和导出不受影响。
use crate::{
    annotation::{Annotation, Point},
    capture::Image,
    capture_view::CaptureView,
    geometry::Rect,
    output_projection::OutputProjection,
    scene::{Layer, Scene},
    Result,
};
use gpui::{RenderImage, Window};
use std::sync::Arc;

const PIXEL_BUDGET: u64 = 8_000_000;
#[derive(Clone)]
pub(crate) struct WatermarkPreview {
    pub(crate) annotation: Arc<Annotation>,
    pub(crate) rect: Rect,
    pub(crate) image: Arc<RenderImage>,
    pub(crate) dimensions: (u32, u32),
}
impl CaptureView {
    pub(crate) fn prepare_watermark_previews(
        &mut self,
        window: &Window,
        scene: &Scene,
    ) -> Result<()> {
        let annotations: Vec<_> = scene
            .layers
            .iter()
            .filter_map(|layer| match layer {
                Layer::Watermark { annotation } => Some(annotation.clone()),
                _ => None,
            })
            .collect();
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let host = Rect::full(
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        let viewport = self.canvas_viewport(window);
        let region = if annotations.is_empty() || host.width == 0 || host.height == 0 {
            None
        } else if self.pin_mode {
            // 长贴图的窗口可能远高于屏幕；和底图共用屏幕内显示范围。
            // 原生几何读取失败仍由像素总预算限制回退范围。
            Some(crate::pin_annotation::shared_preview_region(self, window))
        } else {
            self.canvas_visible_source_region(window)?
        };
        let mut next = Vec::new();
        if let Some(rect) = region {
            let per_layer = PIXEL_BUDGET / annotations.len().max(1) as u64;
            if per_layer == 0 {
                return Err("水印层数量超过显示预算".into());
            }
            let density = f64::from(viewport.zoom)
                .min((per_layer as f64 / (f64::from(rect.width) * f64::from(rect.height))).sqrt());
            let mut width = (f64::from(rect.width) * density).floor().clamp(1., 16384.) as u32;
            let mut height = (f64::from(rect.height) * density).floor().clamp(1., 16384.) as u32;
            while u64::from(width) * u64::from(height) > per_layer {
                if width >= height {
                    width -= 1
                } else {
                    height -= 1
                }
            }
            for annotation in annotations {
                if let Some(cached) = self.watermark_previews.iter().find(|cached| {
                    Arc::ptr_eq(&cached.annotation, &annotation)
                        && cached.rect == rect
                        && cached.dimensions == (width, height)
                }) {
                    next.push(cached.clone());
                    continue;
                }
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
                    source: rect,
                    width,
                    height,
                    sx: width as f32 / rect.width as f32,
                    sy: height as f32 / rect.height as f32,
                    origin: Point { x: 0., y: 0. },
                };
                crate::projected_text::paint(&annotation, &mut image, &projection)?;
                let frame = image::RgbaImage::from_raw(width, height, image.bgra)
                    .ok_or("水印显示图尺寸不一致")?;
                next.push(WatermarkPreview {
                    annotation,
                    rect,
                    dimensions: (width, height),
                    image: Arc::new(RenderImage::new(vec![image::Frame::new(frame)])),
                });
            }
        }
        for old in std::mem::replace(&mut self.watermark_previews, next) {
            if !self
                .watermark_previews
                .iter()
                .any(|current| Arc::ptr_eq(&current.image, &old.image))
            {
                self.retired_raster_previews.push(old.image);
            }
        }
        Ok(())
    }
}
