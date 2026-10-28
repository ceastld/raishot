//! 贴图 OCR 的临时范围反馈，仅 GPUI paint；不进入文档、Scene 或输出。
use super::{ocr_task::Request, PinView, TextSelectionStamp};
use crate::geometry::Rect;
use gpui::{
    canvas, fill, point, prelude::*, px, rgb, rgba, size, AnyElement, App, Bounds, Context, Window,
};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

#[path = "pin_ocr_progress_model.rs"]
mod model;
use model::{DisplayRect, ProgressClock, ScanFrame, TaskKey, TaskKind};

struct OrdinaryScope {
    request: Arc<()>,
    source: TextSelectionStamp,
    generation: u64,
    roi: Rect,
    qr: bool,
}

#[derive(Default)]
pub(super) struct ProgressState {
    ordinary: Option<OrdinaryScope>,
    clock: Rc<RefCell<ProgressClock>>,
}

impl PinView {
    pub(super) fn clear_pin_ocr_progress_clock(&self) {
        self.ocr_progress
            .clock
            .borrow_mut()
            .observe(None, Instant::now());
    }

    pub(super) fn clear_pin_ocr_progress(&mut self) {
        self.ocr_progress.ordinary = None;
        self.clear_pin_ocr_progress_clock();
    }

    pub(super) fn begin_pin_ocr_progress(
        &mut self,
        request: &Request<TextSelectionStamp>,
        roi: Rect,
        qr: bool,
    ) {
        self.clear_pin_ocr_progress();
        // 只登记已经 begin 的实际 OCR 输入；QR 不产生此反馈资格。
        if !qr && roi.valid(self.image_size.0, self.image_size.1) {
            self.ocr_progress.ordinary = Some(OrdinaryScope {
                request: Arc::clone(request.identity()),
                source: request.source,
                generation: request.generation,
                roi,
                qr,
            });
        }
    }

    fn pin_ocr_progress_task(&self, cx: &App) -> Option<(TaskKey, Rect)> {
        if self.presentation_geometry_adjusting || self.processing_image_unavailable {
            return None;
        }
        if let Some((identity, id, roi)) = self.pin_local_progress(cx) {
            return Some((
                TaskKey {
                    kind: TaskKind::Local,
                    identity,
                    id,
                },
                roi,
            ));
        }
        let scope = self.ocr_progress.ordinary.as_ref()?;
        let source = self.text_selection_source(cx);
        let request = self
            .ocr_task
            .active_request(source, self.copy_layout_generation)?;
        if !self.processing
            || scope.qr
            || scope.source != source
            || scope.generation != self.copy_layout_generation
            || !Arc::ptr_eq(&scope.request, request.identity())
            || !scope.roi.valid(self.image_size.0, self.image_size.1)
        {
            return None;
        }
        Some((
            TaskKey {
                kind: TaskKind::Ordinary,
                identity: Arc::clone(&scope.request),
                id: scope.generation,
            },
            scope.roi,
        ))
    }

    pub(super) fn render_pin_ocr_progress(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let viewport = self.content_viewport(window);
        let dpi = window.scale_factor();
        let window_size = window.viewport_size();
        let visible_window = DisplayRect {
            x: 0.,
            y: 0.,
            width: f32::from(window_size.width),
            height: f32::from(window_size.height),
        };
        let image_size = self.image_size;
        let project = |roi| {
            DisplayRect::project(
                roi,
                image_size,
                viewport.zoom,
                dpi,
                (viewport.offset.x, viewport.offset.y),
            )
        };
        let projected = window
            .is_window_active()
            .then(|| self.pin_ocr_progress_task(cx))
            .flatten()
            .and_then(|(key, source_roi)| {
                let roi = project(source_roi)?;
                let content = project(self.content_rect())?.intersection(visible_window)?;
                let visible_roi = roi.intersection(content)?;
                Some((key, source_roi, roi, content, visible_roi))
            });
        let clock = self.ocr_progress.clock.clone();
        let (elapsed, lease) = clock.borrow_mut().begin_render(
            projected.as_ref().map(|(key, ..)| key.clone()),
            Instant::now(),
        )?;
        let (key, source_roi, roi, content, visible_roi) = projected?;
        let scan = ScanFrame::new(roi, content, elapsed)?;
        let owner = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, window, cx| {
                    // 延后 paint 仍须属于这一任务、这一 render、当前来源和当前投影。
                    let current = owner.upgrade().and_then(|owner| {
                        let view = owner.read(cx);
                        (window.is_window_active()
                            && view.image_size == image_size
                            && view.content_viewport(window) == viewport
                            && window.scale_factor() == dpi
                            && window.viewport_size() == window_size)
                            .then(|| view.pin_ocr_progress_task(cx))
                            .flatten()
                    });
                    if !current.as_ref().is_some_and(|(actual, actual_roi)| {
                        actual == &key && *actual_roi == source_roi
                    }) || !clock
                        .borrow()
                        .lease_is_current(&lease, current.as_ref().map(|(actual, _)| actual))
                    {
                        return;
                    }
                    let p = crate::theme::palette(cx);
                    let bounds = display_bounds(visible_roi);
                    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                        let inset = 1_f32.min(roi.width / 4.).min(roi.height / 4.);
                        let edge = Bounds::new(
                            point(px(roi.x + inset), px(roi.y + inset)),
                            size(px(roi.width - inset * 2.), px(roi.height - inset * 2.)),
                        );
                        window.paint_quad(gpui::outline(
                            edge,
                            rgba((p.accent << 8) | u32::from(scan.edge_alpha)),
                            gpui::BorderStyle::default(),
                        ));
                        window.paint_quad(fill(
                            display_bounds(scan.band),
                            rgba((p.accent << 8) | 0x14),
                        ));
                        window.paint_quad(fill(
                            Bounds::new(
                                point(px(scan.band.x), px(scan.band.y + scan.band.height / 2.)),
                                size(px(scan.band.width), px(1_f32.min(scan.band.height))),
                            ),
                            rgba((p.accent << 8) | 0x90),
                        ));
                    });
                    // 缺依赖时先联网下载，徽标改报下载百分比；放不下完整文案时退到短文案。
                    let labels = match crate::ppocr::download_progress::current() {
                        Some(progress) => progress.labels().map(gpui::SharedString::from),
                        None => [
                            match key.kind {
                                TaskKind::Ordinary => tr!("正在识别…", "Recognizing…"),
                                TaskKind::Local => tr!("正在重识别…", "Retrying…"),
                            }
                            .into(),
                            "".into(),
                        ],
                    };
                    let fitted =
                        labels
                            .into_iter()
                            .filter(|text| !text.is_empty())
                            .find_map(|text| {
                                let line = window.text_system().shape_line(
                                    text.clone(),
                                    px(12.),
                                    &[gpui::TextRun {
                                        len: text.len(),
                                        font: window.text_style().font(),
                                        color: rgb(p.foreground).into(),
                                        background_color: None,
                                        underline: None,
                                        strikethrough: None,
                                    }],
                                    None,
                                );
                                // 窄 ROI 的短状态退到可见内容上缘，不铺面板，也不占用任何鼠标命中区。
                                let width = f32::from(line.width);
                                visible_roi
                                    .badge(width)
                                    .or_else(|| {
                                        content.badge(width).map(|mut badge| {
                                            badge.y = content.y + 6.;
                                            badge
                                        })
                                    })
                                    .map(|badge| (line, badge))
                            });
                    if let Some((line, badge)) = fitted {
                        window.with_content_mask(
                            Some(gpui::ContentMask {
                                bounds: display_bounds(content),
                            }),
                            |window| {
                                window.paint_quad(
                                    fill(display_bounds(badge), rgba((p.background << 8) | 0xdb))
                                        .corner_radii(px(13.))
                                        .border_widths(px(1.))
                                        .border_color(rgba((p.accent << 8) | 0x50)),
                                );
                                window.paint_quad(
                                    fill(
                                        Bounds::new(
                                            point(px(badge.x + 8.), px(badge.y + 11.)),
                                            size(px(4.), px(4.)),
                                        ),
                                        rgb(p.accent),
                                    )
                                    .corner_radii(px(2.)),
                                );
                                let _ = line.paint(
                                    point(px(badge.x + 18.), px(badge.y)),
                                    px(26.),
                                    window,
                                    cx,
                                );
                            },
                        );
                    }
                    // 已完成的当前 paint 才续约；旧 paint 不为后继任务请求动画。
                    window.request_animation_frame();
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element(),
        )
    }
}

fn display_bounds(rect: DisplayRect) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(px(rect.x), px(rect.y)),
        size(px(rect.width), px(rect.height)),
    )
}
