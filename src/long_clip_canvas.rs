//! 长图片段的窗口投影；所有标注事件仍回到原图坐标。
use crate::{
    annotation::Point,
    capture_view::CaptureView,
    geometry::Rect,
    long_clip::{LongClip, SeamSide},
    long_clip_projection::LongClipProjection,
    stitch::ScrollAxis,
    viewport::Viewport,
    Result,
};
use gpui::{Context, Window};

impl CaptureView {
    /// 识别沿用原图几何，裁掉的区域填空，避免隐藏内容进入文字结果。
    pub(crate) fn recognition_content_image(
        &mut self,
        selected: Rect,
    ) -> Result<crate::capture::Image> {
        let image = self.content_image_in(selected)?;
        match self.editor.long_clip() {
            Some(clip) => recognition_image(image, selected, clip),
            None => Ok(image),
        }
    }

    pub(crate) fn long_clip_complete_session(&mut self, cx: &mut Context<Self>) {
        if let Some(done) = self.long_clip_on_done.take() {
            done(
                if self.long_clip_output_done {
                    crate::long_output::LongOutcome::Finished
                } else {
                    crate::long_output::LongOutcome::Return(None)
                },
                cx,
            );
        }
    }

    pub(crate) fn long_clip_return(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.processing || self.pin_handoff_pending {
            return;
        }
        if let Some(done) = self.long_clip_on_done.take() {
            done(crate::long_output::LongOutcome::Return(None), cx);
        }
        window.remove_window();
    }

    pub(crate) fn effective_long_clip(&self) -> Option<&LongClip> {
        self.long_clip_ui
            .as_ref()
            .and_then(|ui| ui.draft.as_ref())
            .or_else(|| self.editor.long_clip())
    }

    pub(crate) fn long_clip_projection(&self) -> Option<LongClipProjection> {
        self.effective_long_clip().map(|clip| {
            LongClipProjection::new(
                clip,
                self.long_clip_ui
                    .as_ref()
                    .map_or(&[], |ui| ui.expanded.as_slice()),
            )
        })
    }

    /// 传入显示域坐标，不包含窗口平移、缩放或 DPI。
    pub(crate) fn clip_source_point(&self, display: Point) -> Point {
        self.long_clip_projection()
            .map_or(display, |projection| source_point(&projection, display))
    }

    /// 原图坐标到窗口物理像素；被收起的点定位到其接缝。
    pub(crate) fn clip_view_point(&self, source: Point, window: &Window) -> Point {
        let display = self.long_clip_projection().map_or(source, |projection| {
            let (width, height) = projection.source_size();
            let bounded = Point {
                x: source.x.clamp(0., width as f32),
                y: source.y.clamp(0., height as f32),
            };
            let mapped = projection
                .source_to_display_nearest(bounded)
                .unwrap_or(bounded);
            Point {
                x: mapped.x + source.x - bounded.x,
                y: mapped.y + source.y - bounded.y,
            }
        });
        self.canvas_viewport(window).window_point(display)
    }

    /// 显示用底图、文字和水印共享同一份可见原图范围。
    pub(crate) fn canvas_visible_source_region(&self, window: &Window) -> Result<Option<Rect>> {
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let host = Rect::full(
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        if host.width == 0 || host.height == 0 {
            return Ok(None);
        }
        let viewport = self.canvas_viewport(window);
        match self.long_clip_projection() {
            Some(projection) => visible_source_region(&projection, viewport, host),
            None => crate::image_preview::visible_image_region(self.source.full(), viewport, host),
        }
    }

    /// 主画布留出剪辑轨与四周留白，所有适应/缩放入口使用同一块区域。
    pub(crate) fn long_clip_fit_area(&self, host: (u32, u32), dpi: f32) -> Option<Rect> {
        let axis = self.effective_long_clip()?.axis();
        let area = crate::long_clip_navigation::editing_area(
            axis,
            host.0 as f32 / dpi,
            host.1 as f32 / dpi,
            self.long_clip_side_open(),
        );
        // 与前端稿一致：剪辑轨 46（含 6 间隙）＋两侧各 32 留白。
        let left = (crate::long_clip_chrome::GUTTER + 32.).min(area.width / 4.);
        let inset = 32_f32.min(area.width.min(area.height) / 4.);
        Some(Rect {
            x: ((area.x + left) * dpi).round().max(0.) as u32,
            y: ((area.y + inset) * dpi).round().max(0.) as u32,
            width: ((area.width - left - inset) * dpi).round().max(1.) as u32,
            height: ((area.height - inset * 2.) * dpi).round().max(1.) as u32,
        })
    }

    pub(crate) fn long_clip_fit_width(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.sync_viewport(window);
        let Some(projection) = self.long_clip_projection() else {
            return;
        };
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let (width, height) = projection.display_size();
        let host = (
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        let Some(area) = self.long_clip_fit_area(host, dpi) else {
            return;
        };
        if let Some(state) = &mut self.image_canvas {
            state.auto_fit = false;
            let zoom = long_clip_fit_zoom(projection.axis(), (width, height), area);
            state.viewport.zoom = zoom;
            // 跨轴放得下时居中，与前端稿的适宽一致。
            state.viewport.offset = match projection.axis() {
                ScrollAxis::Vertical => Point {
                    x: area.x as f32 + (area.width as f32 - width as f32 * zoom).max(0.) / 2.,
                    y: area.y as f32,
                },
                ScrollAxis::Horizontal => Point {
                    x: area.x as f32,
                    y: area.y as f32 + (area.height as f32 - height as f32 * zoom).max(0.) / 2.,
                },
            };
        }
        self.invalidate_inline_ocr_projection();
        cx.notify();
    }
}

/// 适宽倍率：跨轴铺满编辑区，不放大超过原图。
pub(crate) fn long_clip_fit_zoom(axis: ScrollAxis, display: (u32, u32), area: Rect) -> f32 {
    match axis {
        ScrollAxis::Vertical => (area.width as f32 / display.0.max(1) as f32).min(1.),
        ScrollAxis::Horizontal => (area.height as f32 / display.1.max(1) as f32).min(1.),
    }
    .max(0.0001)
}

pub(crate) fn source_point(projection: &LongClipProjection, display: Point) -> Point {
    let (width, height) = projection.display_size();
    let bounded = Point {
        x: display.x.clamp(0., width as f32),
        y: display.y.clamp(0., height as f32),
    };
    let mapped = projection
        .display_to_source(bounded, SeamSide::After)
        .unwrap_or(bounded);
    Point {
        x: mapped.x + display.x - bounded.x,
        y: mapped.y + display.y - bounded.y,
    }
}

fn recognition_image(
    image: crate::capture::Image,
    selected: Rect,
    clip: &LongClip,
) -> Result<crate::capture::Image> {
    clip.validate()?;
    if (image.width, image.height) != (selected.width, selected.height)
        || image.bgra.len() != selected.width as usize * selected.height as usize * 4
    {
        return Err("识别图片与原图范围不一致".into());
    }
    let projection = LongClipProjection::new(clip, &[]);
    let mut output = crate::capture::Image {
        width: image.width,
        height: image.height,
        bgra: vec![255; image.bgra.len()],
        readback_ms: image.readback_ms,
    };
    let stride = image.width as usize * 4;
    for piece in projection.pieces() {
        let Some(region) = piece
            .output_source
            .and_then(|rect| rect.intersection(selected))
        else {
            continue;
        };
        for row in region.y..region.y + region.height {
            let start = (row - selected.y) as usize * stride + (region.x - selected.x) as usize * 4;
            let end = start + region.width as usize * 4;
            output.bgra[start..end].copy_from_slice(&image.bgra[start..end]);
        }
    }
    Ok(output)
}

fn visible_source_region(
    projection: &LongClipProjection,
    viewport: Viewport,
    host: Rect,
) -> Result<Option<Rect>> {
    let mut union: Option<Rect> = None;
    for piece in projection.pieces() {
        let Some(visible) =
            crate::image_preview::visible_image_region(piece.display, viewport, host)?
        else {
            continue;
        };
        let source = Rect {
            x: (i64::from(visible.x) - piece.translation.0) as u32,
            y: (i64::from(visible.y) - piece.translation.1) as u32,
            width: visible.width,
            height: visible.height,
        };
        union = Some(match union {
            None => source,
            Some(old) => {
                let x = old.x.min(source.x);
                let y = old.y.min(source.y);
                Rect {
                    x,
                    y,
                    width: (old.x + old.width).max(source.x + source.width) - x,
                    height: (old.y + old.height).max(source.y + source.height) - y,
                }
            }
        });
    }
    Ok(union)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_clip_canvas_visible_roi_keeps_far_source_after_a_large_cut() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let size = axis.dimensions(100, 20_000);
            let mut clip = LongClip::new(size.0, size.1, axis).unwrap();
            clip.delete(100..19_500).unwrap();
            let projection = LongClipProjection::new(&clip, &[]);
            let visible =
                visible_source_region(&projection, Viewport::default(), Rect::full(150, 150))
                    .unwrap()
                    .unwrap();
            let (long_start, long_end) = match axis {
                ScrollAxis::Vertical => (visible.y, visible.y + visible.height),
                ScrollAxis::Horizontal => (visible.x, visible.x + visible.width),
            };
            assert_eq!(long_start, 0);
            assert!(long_end > 19_500);
            let p = match axis {
                ScrollAxis::Vertical => Point { x: 50., y: 120. },
                ScrollAxis::Horizontal => Point { x: 120., y: 50. },
            };
            let source = source_point(&projection, p);
            assert_eq!(
                match axis {
                    ScrollAxis::Vertical => source.y,
                    ScrollAxis::Horizontal => source.x,
                },
                19_520.
            );
        }
    }

    #[test]
    fn long_clip_canvas_pointer_extrapolates_outside_image_without_crossing_a_hidden_band() {
        let mut clip = LongClip::new(200, 1000, ScrollAxis::Vertical).unwrap();
        clip.delete(200..800).unwrap();
        let projection = LongClipProjection::new(&clip, &[]);
        assert_eq!(
            source_point(&projection, Point { x: -5., y: 250. }),
            Point { x: -5., y: 850. }
        );
        assert_eq!(
            source_point(&projection, Point { x: 205., y: 420. }),
            Point { x: 205., y: 1020. }
        );
    }

    #[test]
    fn long_clip_recognition_preserves_source_geometry_and_hides_deleted_pixels() {
        let mut clip = LongClip::new(4, 10, ScrollAxis::Vertical).unwrap();
        clip.delete(3..7).unwrap();
        clip.set_cross(1..3).unwrap();
        let selected = Rect {
            x: 0,
            y: 2,
            width: 4,
            height: 6,
        };
        let image = crate::capture::Image {
            width: 4,
            height: 6,
            bgra: [10, 20, 30, 255].repeat(24),
            readback_ms: 0.,
        };
        let output = recognition_image(image, selected, &clip).unwrap();
        assert_eq!((output.width, output.height), (4, 6));
        for y in 0..6 {
            for x in 0..4 {
                let expected: &[u8] = if (1..3).contains(&x) && (y == 0 || y == 5) {
                    &[10, 20, 30, 255]
                } else {
                    &[255, 255, 255, 255]
                };
                let i = (y * 4 + x) * 4;
                assert_eq!(&output.bgra[i..i + 4], expected);
            }
        }
    }
}
