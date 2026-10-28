//! 独立的长截图实时预览；图片与位置轨共用后台发布的同一版本，不重新采集桌面。
use crate::{
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    long_capture_runtime::LongPreview,
    pin_model::Placement,
    stitch::ScrollAxis,
    Result,
};
use gpui::{
    canvas, div, fill, outline, point, prelude::*, px, rgb, size, App, AppContext, BorderStyle,
    Bounds, Context, Render, RenderImage, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Weak,
    },
};
use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::EnableWindow,
            WindowsAndMessaging::*,
        },
    },
};

const INSET: f32 = 6.;
const RAIL: f32 = 12.;
const FOOTER: f32 = 18.;

/// 仅保留小型数值计账。持有它不会延长预览图片、源像素或窗口的生命周期。
#[derive(Default)]
pub(crate) struct LongPreviewLifecycleLedger {
    render_images_created: AtomicU64,
    drop_image_successes: AtomicU64,
    drop_image_failures: AtomicU64,
    has_current_image: AtomicBool,
    current_image_id: AtomicUsize,
    current_bgra_used_bytes: AtomicUsize,
    current_bgra_capacity_bytes: AtomicUsize,
    current_source_bgra_capacity_bytes: AtomicUsize,
    retired_images: AtomicUsize,
    images_present_at_view_drop: AtomicUsize,
    view_drop_observed: AtomicBool,
    native_placements: AtomicU64,
    native_moves: AtomicU64,
    native_resizes: AtomicU64,
    paint_successes: AtomicU64,
    paint_failures: AtomicU64,
    painted_generation: AtomicU64,
    painted_revision: AtomicU64,
    retired_after_paint: AtomicU64,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct LongPreviewLifecycleProbe {
    pub(crate) render_images_created: u64,
    /// 只计真实 Window::drop_image 返回 Ok，不等同于 atlas/COM 已同步释放。
    pub(crate) drop_image_successes: u64,
    pub(crate) drop_image_failures: u64,
    pub(crate) current_image_id: Option<usize>,
    pub(crate) current_bgra_used_bytes: usize,
    pub(crate) current_bgra_capacity_bytes: usize,
    pub(crate) current_source_bgra_capacity_bytes: usize,
    pub(crate) retired_images: usize,
    /// Drop 开始时仍在 View 字段中的图片数；实际释放仍需 Weak/renderer 证据。
    pub(crate) images_present_at_view_drop: usize,
    pub(crate) view_drop_observed: bool,
    pub(crate) native_placements: u64,
    pub(crate) native_moves: u64,
    pub(crate) native_resizes: u64,
    pub(crate) paint_successes: u64,
    pub(crate) paint_failures: u64,
    pub(crate) painted_generation: u64,
    pub(crate) painted_revision: u64,
    /// 替换图片已成功加入场景后，下一次 render 才执行的真实 drop_image 次数。
    pub(crate) retired_after_paint: u64,
}

impl LongPreviewLifecycleLedger {
    pub(crate) fn snapshot(&self) -> LongPreviewLifecycleProbe {
        LongPreviewLifecycleProbe {
            render_images_created: self.render_images_created.load(Ordering::Acquire),
            drop_image_successes: self.drop_image_successes.load(Ordering::Acquire),
            drop_image_failures: self.drop_image_failures.load(Ordering::Acquire),
            current_image_id: self
                .has_current_image
                .load(Ordering::Acquire)
                .then(|| self.current_image_id.load(Ordering::Acquire)),
            current_bgra_used_bytes: self.current_bgra_used_bytes.load(Ordering::Acquire),
            current_bgra_capacity_bytes: self.current_bgra_capacity_bytes.load(Ordering::Acquire),
            current_source_bgra_capacity_bytes: self
                .current_source_bgra_capacity_bytes
                .load(Ordering::Acquire),
            retired_images: self.retired_images.load(Ordering::Acquire),
            images_present_at_view_drop: self.images_present_at_view_drop.load(Ordering::Acquire),
            view_drop_observed: self.view_drop_observed.load(Ordering::Acquire),
            native_placements: self.native_placements.load(Ordering::Acquire),
            native_moves: self.native_moves.load(Ordering::Acquire),
            native_resizes: self.native_resizes.load(Ordering::Acquire),
            paint_successes: self.paint_successes.load(Ordering::Acquire),
            paint_failures: self.paint_failures.load(Ordering::Acquire),
            painted_generation: self.painted_generation.load(Ordering::Acquire),
            painted_revision: self.painted_revision.load(Ordering::Acquire),
            retired_after_paint: self.retired_after_paint.load(Ordering::Acquire),
        }
    }
}

pub(crate) struct LongPreviewImageWeaks {
    pub(crate) current: Option<Weak<RenderImage>>,
    pub(crate) source: Option<Weak<crate::capture::Image>>,
    pub(crate) retired: Vec<Weak<RenderImage>>,
}

pub(crate) struct LongCapturePreview {
    pub(crate) ready: bool,
    pub(crate) error: Option<String>,
    pub(crate) displayed: Option<LongPreview>,
    /// 图片的窗口客户区逻辑坐标，由实际 paint 写入，供原生验收定位。
    pub(crate) preview_bounds: Rc<RefCell<Option<Bounds<gpui::Pixels>>>>,
    pub(crate) placement: Placement,
    pub(crate) density: f32,
    region: ScreenRect,
    axis: ScrollAxis,
    toolbar: Placement,
    image: Option<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    lifecycle: Arc<LongPreviewLifecycleLedger>,
    visible: bool,
    native_pending: bool,
    painted_frame: Rc<RefCell<Option<PaintedPreview>>>,
    paint_error: Rc<RefCell<Option<String>>>,
    label_bounds: Rc<RefCell<Option<Bounds<gpui::Pixels>>>>,
}

#[derive(Clone)]
struct PaintedPreview {
    image: Arc<RenderImage>,
    frame: LongPreview,
}

impl LongCapturePreview {
    pub(crate) fn lifecycle_probe(&self) -> LongPreviewLifecycleProbe {
        self.lifecycle.snapshot()
    }

    pub(crate) fn lifecycle_ledger(&self) -> Arc<LongPreviewLifecycleLedger> {
        self.lifecycle.clone()
    }

    pub(crate) fn image_weaks(&self) -> LongPreviewImageWeaks {
        LongPreviewImageWeaks {
            current: self.image.as_ref().map(Arc::downgrade),
            source: self
                .displayed
                .as_ref()
                .map(|frame| Arc::downgrade(&frame.image)),
            retired: self.retired.iter().map(Arc::downgrade).collect(),
        }
    }

    /// 选区移动（含拖动中的草稿选区）时直接跟随，不做过渡；`toolbar` 为操作条的新位置。
    pub(crate) fn set_region(
        &mut self,
        region: ScreenRect,
        toolbar: Option<Placement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let toolbar_changed = toolbar.is_some_and(|toolbar| toolbar != self.toolbar);
        if self.region == region && !toolbar_changed {
            return Ok(());
        }
        if let Some(toolbar) = toolbar {
            self.toolbar = toolbar;
        }
        let placement = if let Some(frame) = &self.displayed {
            resolve_frame(region, self.toolbar, frame, &work_areas())?
        } else {
            resolve(
                region,
                self.axis,
                self.toolbar,
                (region.width, region.height),
                &work_areas(),
            )?
        };
        self.region = region;
        self.placement = placement;
        self.schedule_native(window, cx);
        cx.notify();
        Ok(())
    }
    pub(crate) fn open(
        region: ScreenRect,
        axis: ScrollAxis,
        toolbar: Placement,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let placement = resolve(
            region,
            axis,
            toolbar,
            (region.width, region.height),
            &work_areas(),
        )?;
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                is_resizable: false,
                is_minimizable: false,
                focus: false,
                show: false,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(brand_title!("长截图实时预览", "Long capture preview"));
                let density = window.scale_factor();
                let view = cx.new(|_| Self {
                    ready: false,
                    error: None,
                    displayed: None,
                    preview_bounds: Rc::new(RefCell::new(None)),
                    placement,
                    density,
                    region,
                    axis,
                    toolbar,
                    image: None,
                    retired: Vec::new(),
                    lifecycle: Arc::new(LongPreviewLifecycleLedger::default()),
                    visible: true,
                    native_pending: false,
                    painted_frame: Rc::new(RefCell::new(None)),
                    paint_error: Rc::new(RefCell::new(None)),
                    label_bounds: Rc::new(RefCell::new(None)),
                });
                // 预览由父采集会话回收，不能单独关闭而留下一条失去反馈的采集任务。
                window.on_window_should_close(cx, |_, _| false);
                view.update(cx, |v, cx| v.schedule_native(window, cx));
                view
            },
        )?)
    }

    pub(crate) fn set_frame(
        &mut self,
        frame: LongPreview,
        toolbar: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if !frame.source_rect.valid(frame.width, frame.height)
            || !frame.viewport.valid(frame.width, frame.height)
            || frame.image.width == 0
            || frame.image.height == 0
            || frame.image.width > 2048
            || frame.image.height > 2048
            || u64::from(frame.image.width) * u64::from(frame.image.height) > 1_000_000
        {
            return Err("长截图预览的投影或缩略图尺寸无效".into());
        }
        let changed = self.displayed.as_ref().is_none_or(|last| {
            (last.generation, last.revision) != (frame.generation, frame.revision)
                || !Arc::ptr_eq(&last.image, &frame.image)
        });
        let placement = if !changed && self.axis == frame.axis && self.toolbar == toolbar {
            self.placement
        } else {
            resolve_frame(self.region, toolbar, &frame, &work_areas())?
        };
        if self
            .displayed
            .as_ref()
            .is_none_or(|last| !Arc::ptr_eq(&last.image, &frame.image))
        {
            let pixels = image::RgbaImage::from_raw(
                frame.image.width,
                frame.image.height,
                frame.image.bgra.clone(),
            )
            .ok_or("长截图预览像素长度不正确")?;
            // 记录真正被移动进 Frame 的 Vec 容量，不能用宽高或源 Vec 容量代替。
            let capacity = pixels.as_raw().capacity();
            let next = Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]));
            self.lifecycle
                .render_images_created
                .fetch_add(1, Ordering::Release);
            self.lifecycle
                .current_image_id
                .store(next.id.0, Ordering::Release);
            self.lifecycle
                .current_bgra_used_bytes
                .store(next.as_bytes(0).map_or(0, <[u8]>::len), Ordering::Release);
            self.lifecycle
                .current_bgra_capacity_bytes
                .store(capacity, Ordering::Release);
            self.lifecycle
                .has_current_image
                .store(true, Ordering::Release);
            if let Some(previous) = self.image.replace(next) {
                // 在下一次 render 中回收，不能提前使已绘制场景引用的 atlas tile 失效。
                self.retired.push(previous);
            }
            self.lifecycle
                .retired_images
                .store(self.retired.len(), Ordering::Release);
        }
        self.axis = frame.axis;
        self.toolbar = toolbar;
        self.displayed = Some(frame);
        self.lifecycle.current_source_bgra_capacity_bytes.store(
            self.displayed
                .as_ref()
                .map_or(0, |frame| frame.image.bgra.capacity()),
            Ordering::Release,
        );
        if self.placement != placement {
            self.placement = placement;
            self.schedule_native(window, cx);
        }
        cx.notify();
        Ok(())
    }

    pub(crate) fn set_visible(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if self.visible != visible {
            self.visible = visible;
            self.schedule_native(window, cx);
        }
        Ok(())
    }

    /// 远程画面兼容时预览不排除采集；只有落在采集区外才显示，避免被拼进成品。
    fn clear_of_capture(&self) -> bool {
        let capture = Placement {
            x: self.region.left,
            y: self.region.top,
            width: self.region.width,
            height: self.region.height,
        };
        !crate::remote_display::compatible() || !overlaps(self.placement, capture, 0)
    }

    fn schedule_native(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.native_pending || self.error.is_some() {
            return;
        }
        self.native_pending = true;
        let own = window
            .window_handle()
            .downcast::<Self>()
            .expect("长截图预览窗口类型");
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let request = own.update(&mut async_cx, |v, w, _| {
                    let visible = v.visible && v.clear_of_capture();
                    (hwnd(w).0 as usize, v.placement, visible, !v.ready, v.region)
                });
                let Ok((native, placement, visible, initialize, region)) = request else {
                    return;
                };
                // App/Window 借用已结束；SetWindowPos 的同步尺寸/DPI 回调可以安全更新 GPUI。
                let result =
                    native_place(HWND(native as *mut _), placement, visible, initialize, region);
                let _ = own.update(&mut async_cx, |v, w, cx| {
                    v.native_pending = false;
                    match result {
                        Ok((moved, resized)) => {
                            v.lifecycle
                                .native_placements
                                .fetch_add(1, Ordering::Release);
                            if moved {
                                v.lifecycle.native_moves.fetch_add(1, Ordering::Release);
                            }
                            if resized {
                                v.lifecycle.native_resizes.fetch_add(1, Ordering::Release);
                            }
                            v.ready = true;
                            if v.placement != placement || v.visible != visible {
                                v.schedule_native(w, cx);
                            }
                        }
                        Err(error) => {
                            v.ready = false;
                            v.error = Some(error.to_string());
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
    }
}

impl Drop for LongCapturePreview {
    fn drop(&mut self) {
        self.lifecycle.images_present_at_view_drop.store(
            usize::from(self.image.is_some()) + self.retired.len(),
            Ordering::Release,
        );
        // 仅清掉 View 的 live 数值。闭窗自动 Drop 不能冒充成功 drop_image 调用。
        self.lifecycle
            .has_current_image
            .store(false, Ordering::Release);
        self.lifecycle
            .current_bgra_used_bytes
            .store(0, Ordering::Release);
        self.lifecycle
            .current_bgra_capacity_bytes
            .store(0, Ordering::Release);
        self.lifecycle
            .current_source_bgra_capacity_bytes
            .store(0, Ordering::Release);
        self.lifecycle.retired_images.store(0, Ordering::Release);
        self.lifecycle
            .view_drop_observed
            .store(true, Ordering::Release);
    }
}

impl Render for LongCapturePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.density = window.scale_factor();
        if let Some(error) = self.paint_error.borrow_mut().take() {
            self.error = Some(error);
        }
        // 上一场景仍可能使用旧 atlas tile。新图成功 paint 之后的下一帧才退休旧图。
        let replacement_painted = self.painted_frame.borrow().as_ref().is_some_and(|painted| {
            self.image
                .as_ref()
                .is_some_and(|image| Arc::ptr_eq(image, &painted.image))
        });
        let retire_count = if replacement_painted {
            self.retired.len()
        } else {
            0
        };
        for retired in self.retired.drain(..retire_count) {
            match window.drop_image(retired) {
                Ok(()) => {
                    self.lifecycle
                        .drop_image_successes
                        .fetch_add(1, Ordering::Release);
                    self.lifecycle
                        .retired_after_paint
                        .fetch_add(1, Ordering::Release);
                }
                Err(error) => {
                    self.lifecycle
                        .drop_image_failures
                        .fetch_add(1, Ordering::Release);
                    self.error = Some(format!("释放旧长截图预览纹理失败：{error}"));
                }
            }
        }
        self.lifecycle
            .retired_images
            .store(self.retired.len(), Ordering::Release);
        let image = self.image.clone();
        let displayed = self.displayed.clone();
        let painted = self.preview_bounds.clone();
        let painted_frame = self.painted_frame.clone();
        let paint_error = self.paint_error.clone();
        let lifecycle = self.lifecycle.clone();
        let axis = self.axis;
        let label_bounds = self.label_bounds.clone();
        let center = long_center(self.region, self.placement, axis, window.scale_factor());
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .rounded_md()
            .text_color(rgb(crate::theme::palette(cx).foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(10.))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        *label_bounds.borrow_mut() = None;
                        let (Some(image), Some(frame)) = (&image, &displayed) else {
                            return;
                        };
                        // 预览随后台逐帧发布的缩略图直接重绘，不叠加过渡动画，保持与采集同步。
                        let image_bounds = preview_image_bounds(
                            bounds,
                            (frame.image.width, frame.image.height),
                            axis,
                            window.scale_factor(),
                            center,
                        );
                        let result = window.paint_image_exact(
                            image_bounds,
                            Default::default(),
                            image.clone(),
                            0,
                            false,
                        );
                        let (image_bounds, frame) = match result {
                            Ok(()) => {
                                let changed = painted_frame.borrow().as_ref().is_none_or(|last| {
                                    !Arc::ptr_eq(&last.image, image)
                                        || (last.frame.generation, last.frame.revision)
                                            != (frame.generation, frame.revision)
                                });
                                *painted_frame.borrow_mut() = Some(PaintedPreview {
                                    image: image.clone(),
                                    frame: frame.clone(),
                                });
                                lifecycle.paint_successes.fetch_add(1, Ordering::Release);
                                lifecycle
                                    .painted_generation
                                    .store(frame.generation, Ordering::Release);
                                lifecycle
                                    .painted_revision
                                    .store(frame.revision, Ordering::Release);
                                if changed {
                                    // 同时更新已显示尺寸标签，并在下一帧回收旧纹理。
                                    window.request_animation_frame();
                                }
                                (image_bounds, frame.clone())
                            }
                            Err(error) => {
                                lifecycle.paint_failures.fetch_add(1, Ordering::Release);
                                *paint_error.borrow_mut() =
                                    Some(format!("绘制长截图预览失败：{error}"));
                                window.request_animation_frame();
                                let Some(previous) = painted_frame.borrow().clone() else {
                                    return;
                                };
                                let previous_bounds = preview_image_bounds(
                                    bounds,
                                    (previous.frame.image.width, previous.frame.image.height),
                                    previous.frame.axis,
                                    window.scale_factor(),
                                    center,
                                );
                                if window
                                    .paint_image_exact(
                                        previous_bounds,
                                        Default::default(),
                                        previous.image,
                                        0,
                                        false,
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                                (previous_bounds, previous.frame)
                            }
                        };
                        *painted.borrow_mut() = Some(image_bounds);
                        let palette = crate::theme::palette(cx);
                        window.paint_quad(outline(
                            image_bounds,
                            rgb(palette.muted),
                            BorderStyle::Solid,
                        ));
                        let source = frame.source_rect;
                        if let Some(visible) = frame.viewport.intersection(source) {
                            let (w, h) = (
                                f32::from(image_bounds.size.width),
                                f32::from(image_bounds.size.height),
                            );
                            let current = Bounds::new(
                                image_bounds.origin
                                    + point(
                                        px((visible.x - source.x) as f32 / source.width as f32 * w),
                                        px((visible.y - source.y) as f32 / source.height as f32
                                            * h),
                                    ),
                                size(
                                    px(visible.width as f32 / source.width as f32 * w),
                                    px(visible.height as f32 / source.height as f32 * h),
                                ),
                            );
                            window.paint_quad(outline(current, rgb(0x32be69), BorderStyle::Solid));
                        }
                        paint_rail(
                            window,
                            image_bounds,
                            frame.axis,
                            rail_intervals(&frame),
                            palette,
                        );
                        paint_omissions(
                            window,
                            palette,
                            image_bounds,
                            source,
                            frame.width,
                            frame.height,
                            frame.axis,
                        );
                        // 外壳保持稳定，未使用的区域完全透明；标签只贴实际成功绘制的图。
                        match paint_dimensions(window, image_bounds, bounds, &frame, cx) {
                            Ok(bounds) => *label_bounds.borrow_mut() = Some(bounds),
                            Err(error) => eprintln!("长截图尺寸标签绘制失败：{error}"),
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl LongCapturePreview {
    pub(crate) fn label_bounds_for_qa(&self) -> Option<Bounds<gpui::Pixels>> {
        *self.label_bounds.borrow()
    }
}

fn paint_dimensions(
    window: &mut Window,
    image: Bounds<gpui::Pixels>,
    shell: Bounds<gpui::Pixels>,
    frame: &LongPreview,
    cx: &mut App,
) -> Result<Bounds<gpui::Pixels>> {
    let text: gpui::SharedString = format!("{} × {}", frame.width, frame.height).into();
    let line = window.text_system().shape_line(
        text.clone(),
        px(10.),
        &[gpui::TextRun {
            len: text.len(),
            font: window.text_style().font(),
            color: rgb(crate::theme::palette(cx).foreground).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let width = (line.width + px(8.)).min((shell.right() - image.left() - px(INSET)).max(px(1.)));
    let label = Bounds::new(
        image.bottom_left()
            + point(
                px(0.),
                px(4.
                    + if frame.axis == ScrollAxis::Horizontal {
                        RAIL
                    } else {
                        0.
                    }),
            ),
        size(width, px(FOOTER - 4.)),
    );
    window.paint_quad(fill(label, rgb(crate::theme::palette(cx).background)).corner_radii(px(3.)));
    window
        .with_content_mask(Some(gpui::ContentMask { bounds: label }), |window| {
            line.paint(
                label.origin + point(px(4.), px(0.)),
                label.size.height,
                window,
                cx,
            )
        })
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?;
    Ok(label)
}

/// 图像可用区域（逻辑像素），不含边距、位置轨与尺寸标签。
fn content_area(bounds: Bounds<gpui::Pixels>, axis: ScrollAxis) -> (f32, f32) {
    let vertical = axis == ScrollAxis::Vertical;
    (
        (f32::from(bounds.size.width) - INSET * 2. - if vertical { RAIL } else { 0. }).max(1.),
        (f32::from(bounds.size.height) - INSET * 2. - FOOTER - if vertical { 0. } else { RAIL })
            .max(1.),
    )
}

/// 选区长轴中点相对预览窗口的逻辑坐标；预览以它为中心向两侧展开。
fn long_center(region: ScreenRect, placement: Placement, axis: ScrollAxis, dpi: f32) -> f32 {
    let (start, length, origin) = if axis == ScrollAxis::Vertical {
        (region.top, region.height, placement.y)
    } else {
        (region.left, region.width, placement.x)
    };
    ((f64::from(start) + f64::from(length) / 2. - f64::from(origin)) / f64::from(dpi.max(0.1)))
        as f32
}

/// 长度为 `length` 的内容在长轴上的起点：中心对齐选区中点，靠近外壳边缘时夹回可用区域。
fn long_start(available: f32, center: f32, length: f32) -> f32 {
    (center - length / 2.)
        .min(INSET + available - length)
        .max(INSET)
}

fn snap(value: f32, dpi: f32) -> f32 {
    (value * dpi).round() / dpi
}

fn preview_image_bounds(
    bounds: Bounds<gpui::Pixels>,
    image: (u32, u32),
    axis: ScrollAxis,
    dpi: f32,
    center: f32,
) -> Bounds<gpui::Pixels> {
    let vertical = axis == ScrollAxis::Vertical;
    let (width, height) = content_area(bounds, axis);
    let factor = ((width * dpi).floor().max(1.) / image.0.max(1) as f32)
        .min((height * dpi).floor().max(1.) / image.1.max(1) as f32)
        .min(1.);
    let image_width = (image.0 as f32 * factor).round().max(1.) / dpi;
    let image_height = (image.1 as f32 * factor).round().max(1.) / dpi;
    // 长轴以选区中点为中心，追加内容时向两侧展开；交叉轴居中。
    let (x, y) = if vertical {
        (
            INSET + (width - image_width) / 2.,
            long_start(height, center, image_height),
        )
    } else {
        (
            long_start(width, center, image_width),
            INSET + (height - image_height) / 2.,
        )
    };
    Bounds::new(
        point(
            px(snap(f32::from(bounds.origin.x) + x, dpi)),
            px(snap(f32::from(bounds.origin.y) + y, dpi)),
        ),
        size(px(image_width), px(image_height)),
    )
}

/// 位置轨比例区间：预览投影的起点、长度，当前视口的起点、长度。
fn rail_intervals(frame: &LongPreview) -> [f32; 4] {
    let ((projected_start, projected_length), (current_start, current_length)) =
        if frame.axis == ScrollAxis::Vertical {
            (
                rail_interval(frame.source_rect.y, frame.source_rect.height, frame.height),
                rail_interval(frame.viewport.y, frame.viewport.height, frame.height),
            )
        } else {
            (
                rail_interval(frame.source_rect.x, frame.source_rect.width, frame.width),
                rail_interval(frame.viewport.x, frame.viewport.width, frame.width),
            )
        };
    [
        projected_start,
        projected_length,
        current_start,
        current_length,
    ]
}

fn paint_rail(
    window: &mut Window,
    image: Bounds<gpui::Pixels>,
    axis: ScrollAxis,
    intervals: [f32; 4],
    palette: crate::capture_toolbar::Palette,
) {
    let vertical = axis == ScrollAxis::Vertical;
    let rail = if vertical {
        Bounds::new(
            image.top_right() + point(px(5.), px(0.)),
            size(px(4.), image.size.height),
        )
    } else {
        Bounds::new(
            image.bottom_left() + point(px(0.), px(5.)),
            size(image.size.width, px(4.)),
        )
    };
    window.paint_quad(fill(rail, rgb(palette.border)));
    let [projected_start, projected_length, current_start, current_length] = intervals;
    for (start, length, color) in [
        (projected_start, projected_length, palette.muted),
        (current_start, current_length, 0x32be69),
    ] {
        let segment = if vertical {
            let rail_length = f32::from(rail.size.height);
            Bounds::new(
                rail.origin + point(px(0.), px(start * rail_length)),
                size(rail.size.width, px((length * rail_length).max(1.))),
            )
        } else {
            let rail_length = f32::from(rail.size.width);
            Bounds::new(
                rail.origin + point(px(start * rail_length), px(0.)),
                size(px((length * rail_length).max(1.)), rail.size.height),
            )
        };
        window.paint_quad(fill(segment, rgb(color)));
    }
}

fn rail_interval(start: u32, length: u32, total: u32) -> (f32, f32) {
    let total = total.max(1) as f32;
    let start = (start as f32 / total).clamp(0., 1.);
    (start, (length as f32 / total).clamp(0., 1. - start))
}

fn paint_omissions(
    window: &mut Window,
    palette: crate::capture_toolbar::Palette,
    image: Bounds<gpui::Pixels>,
    source: Rect,
    width: u32,
    height: u32,
    axis: ScrollAxis,
) {
    let (leading, trailing) = if axis == ScrollAxis::Vertical {
        (
            source.y > 0,
            source.y.saturating_add(source.height) < height,
        )
    } else {
        (source.x > 0, source.x.saturating_add(source.width) < width)
    };
    for (show, trailing) in [(leading, false), (trailing, true)] {
        if !show {
            continue;
        }
        for index in 0..3 {
            let origin = if axis == ScrollAxis::Vertical {
                image.origin
                    + point(
                        px(f32::from(image.size.width) / 2. - 5. + index as f32 * 4.),
                        px(if trailing {
                            f32::from(image.size.height) - 5.
                        } else {
                            3.
                        }),
                    )
            } else {
                image.origin
                    + point(
                        px(if trailing {
                            f32::from(image.size.width) - 5.
                        } else {
                            3.
                        }),
                        px(f32::from(image.size.height) / 2. - 5. + index as f32 * 4.),
                    )
            };
            window.paint_quad(fill(
                Bounds::new(origin, size(px(2.), px(2.))),
                rgb(palette.foreground),
            ));
        }
    }
}

fn native_place(
    h: HWND,
    p: Placement,
    visible: bool,
    initialize: bool,
    capture: ScreenRect,
) -> Result<(bool, bool)> {
    let previous = crate::pin_native::rect(h)?;
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid != std::process::id() || !IsWindow(Some(h)).as_bool() {
            return Err("长截图实时预览已关闭".into());
        }
        if initialize {
            crate::remote_display::exclude_from_capture(h)
                .map_err(|error| format!("无法排除长截图实时预览：{error}"))?;
            SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
            SetWindowLongPtrW(
                h,
                GWL_EXSTYLE,
                GetWindowLongPtrW(h, GWL_EXSTYLE)
                    | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0 as isize,
            );
            // 只读窗口没有控件。禁用命中可穿透目标页，保持 DComp，不强行添加 WS_EX_LAYERED。
            let _ = EnableWindow(h, false);
        }
        let mut flags = SWP_NOACTIVATE
            | if visible {
                SWP_SHOWWINDOW
            } else {
                SWP_HIDEWINDOW
            };
        if initialize {
            flags |= SWP_FRAMECHANGED;
        }
        let target = ScreenRect {
            left: p.x,
            top: p.y,
            width: p.width,
            height: p.height,
        };
        crate::remote_display::guard_tool_move(h, target, capture);
        SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            p.x,
            p.y,
            p.width as i32,
            p.height as i32,
            flags,
        )?;
        crate::remote_display::sync_tool_window(h, &[capture]);
    }
    Ok((
        previous.x != p.x || previous.y != p.y,
        previous.width != p.width || previous.height != p.height,
    ))
}

#[derive(Clone, Copy)]
struct WorkArea {
    bounds: Placement,
    dpi: f32,
}
fn work_areas() -> Vec<WorkArea> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let areas = unsafe { &mut *(data.0 as *mut Vec<WorkArea>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let mut x = 96;
            let mut y = 96;
            let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }
                .is_ok()
                && x > 0
            {
                x as f32 / 96.
            } else {
                1.
            };
            let r = info.rcWork;
            if r.right > r.left && r.bottom > r.top {
                areas.push(WorkArea {
                    bounds: Placement {
                        x: r.left,
                        y: r.top,
                        width: (r.right - r.left) as u32,
                        height: (r.bottom - r.top) as u32,
                    },
                    dpi,
                });
            }
        }
        BOOL(1)
    }
    let mut areas = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&mut areas as *mut Vec<WorkArea>) as isize),
        );
    }
    areas
}

fn dimensions(_image: (u32, u32), axis: ScrollAxis, work: WorkArea) -> (u32, u32) {
    let (max_width, max_height) = if axis == ScrollAxis::Vertical {
        (200., 700.)
    } else {
        (700., 200.)
    };
    // 会话外壳只随轴向、目标屏 DPI/工作区变化；追加缩略图不再 resize DComp swapchain。
    let width = max_width
        + INSET * 2.
        + if axis == ScrollAxis::Vertical {
            RAIL
        } else {
            0.
        };
    let height = max_height
        + INSET * 2.
        + FOOTER
        + if axis == ScrollAxis::Horizontal {
            RAIL
        } else {
            0.
        };
    let margin = 20. * work.dpi;
    let scale = ((work.bounds.width as f32 - margin).max(1.) / (width * work.dpi))
        .min((work.bounds.height as f32 - margin).max(1.) / (height * work.dpi))
        .min(1.);
    (
        (width * work.dpi * scale).round().max(1.) as u32,
        (height * work.dpi * scale).round().max(1.) as u32,
    )
}

fn resolve_frame(
    region: ScreenRect,
    toolbar: Placement,
    frame: &LongPreview,
    works: &[WorkArea],
) -> Result<Placement> {
    // 图像尺寸只用于校验；外壳布局不跟随不断增长的长图或纹理密度。
    resolve(
        region,
        frame.axis,
        toolbar,
        (frame.source_rect.width, frame.source_rect.height),
        works,
    )
}

fn contains(work: Placement, p: Placement) -> bool {
    p.x >= work.x
        && p.y >= work.y
        && i64::from(p.x) + i64::from(p.width) <= i64::from(work.x) + i64::from(work.width)
        && i64::from(p.y) + i64::from(p.height) <= i64::from(work.y) + i64::from(work.height)
}
fn overlaps(a: Placement, b: Placement, gap: i64) -> bool {
    i64::from(a.x) < i64::from(b.x) + i64::from(b.width) + gap
        && i64::from(a.x) + i64::from(a.width) > i64::from(b.x) - gap
        && i64::from(a.y) < i64::from(b.y) + i64::from(b.height) + gap
        && i64::from(a.y) + i64::from(a.height) > i64::from(b.y) - gap
}
fn at(x: i64, y: i64, size: (u32, u32)) -> Option<Placement> {
    Some(Placement {
        x: i32::try_from(x).ok()?,
        y: i32::try_from(y).ok()?,
        width: size.0,
        height: size.1,
    })
}
fn resolve(
    region: ScreenRect,
    axis: ScrollAxis,
    toolbar: Placement,
    image: (u32, u32),
    areas: &[WorkArea],
) -> Result<Placement> {
    if region.width == 0 || region.height == 0 || image.0 == 0 || image.1 == 0 || areas.is_empty() {
        return Err("无法确定长截图预览的屏幕位置".into());
    }
    let capture = Placement {
        x: region.left,
        y: region.top,
        width: region.width,
        height: region.height,
    };
    let right = i64::from(region.left) + i64::from(region.width);
    let bottom = i64::from(region.top) + i64::from(region.height);
    // 优先选区所在屏，再看其它屏；每个候选使用其自己的 DPI 和物理工作区。
    let mut areas = areas.to_vec();
    areas.sort_by_key(|a| !overlaps(a.bounds, capture, 0));
    for work in &areas {
        let size = dimensions(image, axis, *work);
        let gap = (10. * work.dpi).round() as i64;
        let (width, height) = (i64::from(size.0), i64::from(size.1));
        let vertical = axis == ScrollAxis::Vertical;
        // 预览图以选区中点为中心向两侧展开，因此外壳与选区中线对齐（竖向垂直居中、横向水平居中）；
        // 靠近屏幕边缘时沿交叉轴夹回工作区，仍贴着选区，图像在外壳内再夹到可用范围。
        let (lo, hi, length, start) = if vertical {
            (
                i64::from(work.bounds.y) + gap,
                i64::from(work.bounds.y) + i64::from(work.bounds.height) - gap,
                height,
                i64::from(region.top) + i64::from(region.height) / 2 - height / 2,
            )
        } else {
            (
                i64::from(work.bounds.x) + gap,
                i64::from(work.bounds.x) + i64::from(work.bounds.width) - gap,
                width,
                i64::from(region.left) + i64::from(region.width) / 2 - width / 2,
            )
        };
        let sides = if vertical {
            [right + gap, i64::from(region.left) - gap - width]
        } else {
            [bottom + gap, i64::from(region.top) - gap - height]
        };
        let (toolbar_start, toolbar_end) = if vertical {
            (
                i64::from(toolbar.y),
                i64::from(toolbar.y) + i64::from(toolbar.height),
            )
        } else {
            (
                i64::from(toolbar.x),
                i64::from(toolbar.x) + i64::from(toolbar.width),
            )
        };
        for side in sides {
            // 首选与选区中线对齐；压住操作条时沿交叉轴滑到操作条另一侧。
            let along = [start, toolbar_end + gap, toolbar_start - gap - length];
            for cross in along {
                let cross = cross.min(hi - length).max(lo);
                let (x, y) = if vertical {
                    (side, cross)
                } else {
                    (cross, side)
                };
                if let Some(p) = at(x, y, size) {
                    if contains(work.bounds, p)
                        && !overlaps(p, capture, 2)
                        && !overlaps(p, toolbar, gap)
                    {
                        return Ok(p);
                    }
                }
            }
        }
        // 其它显示器无需贴着选区，但不能压住控制条。
        if !overlaps(work.bounds, capture, 0) {
            for (x, y) in corners(work.bounds, size, gap) {
                if let Some(p) = at(x, y, size) {
                    if contains(work.bounds, p) && !overlaps(p, toolbar, gap) {
                        return Ok(p);
                    }
                }
            }
        }
    }
    // 最大化目标无外侧空白时靠屏幕角落；独立 HWND 已排除采集并禁用输入。
    for work in &areas {
        let size = dimensions(image, axis, *work);
        let gap = (10. * work.dpi).round() as i64;
        for (x, y) in corners(work.bounds, size, gap) {
            if let Some(p) = at(x, y, size) {
                if contains(work.bounds, p) && !overlaps(p, toolbar, gap) {
                    return Ok(p);
                }
            }
        }
    }
    Err("工作区没有可避开操作条的长截图预览位置".into())
}
fn corners(work: Placement, size: (u32, u32), gap: i64) -> [(i64, i64); 4] {
    let left = i64::from(work.x) + gap;
    let top = i64::from(work.y) + gap;
    let right = i64::from(work.x) + i64::from(work.width) - i64::from(size.0) - gap;
    let bottom = i64::from(work.y) + i64::from(work.height) - i64::from(size.1) - gap;
    [(right, top), (right, bottom), (left, top), (left, bottom)]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_preview_stability_growth_keeps_shell_and_anchor_on_both_axes() {
        let works = [WorkArea {
            bounds: Placement {
                x: -1920,
                y: -240,
                width: 1920,
                height: 1040,
            },
            dpi: 1.25,
        }];
        let region = ScreenRect {
            left: -1800,
            top: -120,
            width: 480,
            height: 300,
        };
        let toolbar = Placement {
            x: -1780,
            y: 550,
            width: 560,
            height: 90,
        };
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let mut shell = None;
            for long in [240, 480, 960, 1400, 3200, 32_000] {
                let source = if axis == ScrollAxis::Vertical {
                    (400, long)
                } else {
                    (long, 400)
                };
                let p = resolve(region, axis, toolbar, source, &works).unwrap();
                assert!(contains(works[0].bounds, p) && !overlaps(p, toolbar, 10));
                if let Some(previous) = shell {
                    assert_eq!(previous, p);
                }
                shell = Some(p);
                let projection_long = long.min(1400);
                let image = if axis == ScrollAxis::Vertical {
                    (250, projection_long * 250 / 400)
                } else {
                    (projection_long * 250 / 400, 250)
                };
                let shell_bounds = Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(p.width as f32 / 1.25), px(p.height as f32 / 1.25)),
                );
                let center = long_center(region, p, axis, 1.25);
                let b = preview_image_bounds(shell_bounds, image, axis, 1.25, center);
                // 图像中点贴着选区中点，只有触到外壳边缘才夹回，始终留在可用区域内。
                let (available, start, length) = if axis == ScrollAxis::Vertical {
                    (
                        content_area(shell_bounds, axis).1,
                        b.origin.y,
                        b.size.height,
                    )
                } else {
                    (content_area(shell_bounds, axis).0, b.origin.x, b.size.width)
                };
                let (start, length) = (f32::from(start), f32::from(length));
                let expected = center.clamp(
                    INSET + length / 2.,
                    (INSET + available - length / 2.).max(INSET + length / 2.),
                );
                assert!((start + length / 2. - expected).abs() <= 1.);
                assert!(start >= INSET - 0.5 && start + length <= INSET + available + 0.5);
                assert!((f32::from(b.origin.x) * 1.25).fract().abs() < 0.001);
                assert!((f32::from(b.origin.y) * 1.25).fract().abs() < 0.001);
                assert!((f32::from(b.size.width) * 1.25).round() <= image.0 as f32);
                assert!((f32::from(b.size.height) * 1.25).round() <= image.1 as f32);
            }
        }
    }
    #[test]
    fn long_preview_stability_shell_clamps_small_workarea_and_preserves_rail_space() {
        let work = WorkArea {
            bounds: Placement {
                x: 0,
                y: 0,
                width: 500,
                height: 420,
            },
            dpi: 2.,
        };
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let initial = dimensions((400, 240), axis, work);
            assert_eq!(initial, dimensions((400, 32_000), axis, work));
            assert!(initial.0 <= work.bounds.width - 40 && initial.1 <= work.bounds.height - 40);
            let image = if axis == ScrollAxis::Vertical {
                (400, 1400)
            } else {
                (1400, 400)
            };
            let bounds = preview_image_bounds(
                Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(initial.0 as f32 / 2.), px(initial.1 as f32 / 2.)),
                ),
                image,
                axis,
                2.,
                initial.1.max(initial.0) as f32 / 4.,
            );
            let rail_end = if axis == ScrollAxis::Vertical {
                f32::from(bounds.origin.x + bounds.size.width) + 9.
            } else {
                f32::from(bounds.origin.y + bounds.size.height) + 9.
            };
            let client_end = if axis == ScrollAxis::Vertical {
                initial.0 as f32 / 2.
            } else {
                initial.1 as f32 / 2. - FOOTER
            };
            assert!(rail_end <= client_end);
        }
    }
    #[test]
    fn long_preview_quality_layout_does_not_follow_texture_density() {
        let works = [
            WorkArea {
                bounds: Placement {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 1000,
                },
                dpi: 1.25,
            },
            WorkArea {
                bounds: Placement {
                    x: 800,
                    y: 0,
                    width: 1200,
                    height: 1000,
                },
                dpi: 1.,
            },
        ];
        let region = ScreenRect {
            left: 30,
            top: 200,
            width: 480,
            height: 392,
        };
        let toolbar = Placement {
            x: 30,
            y: 610,
            width: 560,
            height: 90,
        };
        let mut previous = None;
        for (width, height) in [(200, 163), (250, 204), (400, 327), (500, 408)] {
            let frame = LongPreview {
                image: Arc::new(crate::capture::Image {
                    width,
                    height,
                    bgra: vec![255; width as usize * height as usize * 4],
                    readback_ms: 0.,
                }),
                revision: 1,
                generation: 0,
                width: region.width,
                height: region.height,
                viewport: Rect::full(region.width, region.height),
                axis: ScrollAxis::Vertical,
                source_rect: Rect::full(region.width, region.height),
            };
            let placement = resolve_frame(region, toolbar, &frame, &works).unwrap();
            // 主屏外侧空间只能装下低密度小图；布局固定在副屏，避免改变 DPI 后又跳回。
            assert!(placement.x >= 800);
            if let Some(previous) = previous {
                assert_eq!(placement, previous);
            }
            previous = Some(placement);
        }
    }
    #[test]
    fn long_preview_placement_uses_right_side_and_target_dpi_without_toolbar_overlap() {
        let work = WorkArea {
            bounds: Placement {
                x: -1920,
                y: -240,
                width: 1920,
                height: 1040,
            },
            dpi: 1.25,
        };
        let region = ScreenRect {
            left: -1680,
            top: -100,
            width: 640,
            height: 600,
        };
        let toolbar = Placement {
            x: -1630,
            y: 550,
            width: 560,
            height: 90,
        };
        let p = resolve(
            region,
            ScrollAxis::Vertical,
            toolbar,
            (region.width, region.height),
            &[work],
        )
        .unwrap();
        assert!(p.x >= region.left + region.width as i32);
        assert!(contains(work.bounds, p) && !overlaps(p, toolbar, 10));
        assert_eq!(p.width, 280);
    }
    #[test]
    fn long_preview_placement_centers_on_capture_and_clamps_near_screen_edges() {
        let work = WorkArea {
            bounds: Placement {
                x: 0,
                y: 0,
                width: 1920,
                height: 1040,
            },
            dpi: 1.,
        };
        let far = Placement {
            x: 0,
            y: 2000,
            width: 1,
            height: 1,
        };
        // 高选区：外壳中线与选区中线对齐，图像从选区右侧中间向上下展开。
        let tall = ScreenRect {
            left: 300,
            top: 100,
            width: 800,
            height: 800,
        };
        let p = resolve(tall, ScrollAxis::Vertical, far, (800, 800), &[work]).unwrap();
        assert_eq!((p.x, p.y), (1110, 500 - 365));
        assert_eq!(long_center(tall, p, ScrollAxis::Vertical, 1.), 365.);
        // 矮选区靠近屏幕顶部：外壳夹回工作区，图像中心仍对准选区中点。
        let region = ScreenRect {
            left: 300,
            top: 200,
            width: 800,
            height: 240,
        };
        let p = resolve(region, ScrollAxis::Vertical, far, (800, 240), &[work]).unwrap();
        assert_eq!((p.x, p.y), (1110, 10));
        assert_eq!(long_center(region, p, ScrollAxis::Vertical, 1.), 310.);
        // 贴近屏幕底部：沿交叉轴夹回工作区，仍贴在选区右侧。
        let low = ScreenRect { top: 900, ..region };
        let p = resolve(low, ScrollAxis::Vertical, far, (800, 240), &[work]).unwrap();
        assert_eq!(p.x, 1110);
        assert!(contains(work.bounds, p));
        // 横向：水平居中于选区，贴在下方，靠右边缘时夹回工作区。
        let wide = ScreenRect {
            left: 1500,
            top: 200,
            width: 400,
            height: 200,
        };
        let p = resolve(wide, ScrollAxis::Horizontal, far, (400, 200), &[work]).unwrap();
        assert_eq!(p.y, 410);
        assert!(contains(work.bounds, p));
        // 操作条在右侧同高时滑到其下方，不压住操作条。
        let toolbar = Placement {
            x: 1110,
            y: 180,
            width: 400,
            height: 60,
        };
        let p = resolve(region, ScrollAxis::Vertical, toolbar, (800, 240), &[work]).unwrap();
        assert!(p.x == 1110 && !overlaps(p, toolbar, 10));
    }
    #[test]
    fn long_preview_placement_horizontal_below_and_fullscreen_inset_avoid_toolbar() {
        let work = WorkArea {
            bounds: Placement {
                x: 0,
                y: 0,
                width: 1920,
                height: 1040,
            },
            dpi: 1.,
        };
        let toolbar = Placement {
            x: 660,
            y: 80,
            width: 560,
            height: 90,
        };
        let region = ScreenRect {
            left: 400,
            top: 220,
            width: 1000,
            height: 200,
        };
        let p = resolve(region, ScrollAxis::Horizontal, toolbar, (700, 200), &[work]).unwrap();
        assert!(p.y >= region.top + region.height as i32);
        let full = ScreenRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1040,
        };
        let inset = resolve(full, ScrollAxis::Vertical, toolbar, (200, 700), &[work]).unwrap();
        assert!(contains(work.bounds, inset) && !overlaps(inset, toolbar, 10));
    }
    #[test]
    fn long_preview_global_rail_retains_window_projection_and_current_position() {
        assert_eq!(rail_interval(30_000, 4000, 60_000), (0.5, 1. / 15.));
        assert_eq!(rail_interval(31_000, 1000, 60_000), (31. / 60., 1. / 60.));
        assert_eq!(rail_interval(70_000, 1000, 60_000), (1., 0.));
    }
}
