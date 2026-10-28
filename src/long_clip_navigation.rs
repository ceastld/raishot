//! 长图缩略导航按跨轴等比例显示；整长标尺另用线性映射负责远距离定位。
use crate::{
    annotation::Point,
    canvas_source::render_image,
    capture_view::CaptureView,
    gpui_capture::hwnd,
    long_clip::SeamSide,
    long_clip_chrome::{alpha, short, ClipTheme},
    long_clip_projection::LongClipProjection,
    output_projection::OutputProjection,
    stitch::ScrollAxis,
};
use gpui::{
    div, img, prelude::*, px, rgb, rgba, AnyElement, Context, CursorStyle, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, RenderImage, ScrollDelta, Window,
};
use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};

// 停靠布局与前端稿一致：顶栏｜左工具栏、画布、缩略图、全长尺、属性栏｜状态栏。
pub(crate) const TOP_BAR: f32 = 48.;
pub(crate) const STATUS_BAR: f32 = 28.;
pub(crate) const RAIL_WIDTH: f32 = 70.;
pub(crate) const NARROW_RAIL_WIDTH: f32 = 56.;
pub(crate) const SIDEBAR_WIDTH: f32 = 284.;
/// 窄于此宽度时收起缩略图与属性栏，只留全长尺。
pub(crate) const NARROW_WIDTH: f32 = 760.;
const MINI_WIDTH: f32 = 116.;
const RULER_WIDTH: f32 = 14.;
const MIN_THUMB: f32 = 18.;
const MIN_MAIN_HEIGHT: f32 = 64.;
const MAX_PREVIEW_PIXELS: f64 = 1_000_000.;
const MAX_PREVIEW_SIDE: f64 = 8192.;
const SCENE_DEBOUNCE: Duration = Duration::from_millis(120);

#[derive(Default)]
pub(crate) struct NavigationState {
    drag: Option<NavigationDrag>,
    preview: Option<NavigationPreview>,
    seen_stamp: Option<(u64, u64)>,
    changed_at: Option<Instant>,
    wake_pending: bool,
}

impl NavigationState {
    /// 只结束导航手势，保留缩略栅格缓存；原生 capture 由宿主统一释放。
    pub(crate) fn cancel(&mut self) -> bool {
        self.drag.take().is_some()
    }
}

struct NavigationPreview {
    source_id: usize,
    dimensions: (u32, u32),
    original: Arc<RenderImage>,
    composited: Option<Arc<RenderImage>>,
    scene_stamp: Option<(u64, u64)>,
}

#[derive(Clone, Copy)]
enum NavigationPart {
    Mini,
    Ruler,
}

#[derive(Clone, Copy)]
struct NavigationDrag {
    part: NavigationPart,
    grab: f32,
}

/// thumb 的最小可抓尺寸只影响视觉，往返换算始终使用有效拖动距离。
#[derive(Clone, Copy, Debug)]
struct NavigationMap {
    scale: f32,
    /// 内容短于面板时的长轴居中留白；内容更长时为 0，随主画布按比例滚动。
    lead: f32,
    scroll: f32,
    thumb_start: f32,
    thumb_len: f32,
    travel: f32,
    thumb_travel: f32,
    view_len: f32,
}

impl NavigationMap {
    fn new(
        doc_long: f32,
        doc_cross: f32,
        panel_long: f32,
        panel_cross: f32,
        start: f32,
        view: f32,
    ) -> Self {
        let scale = (panel_cross / doc_cross.max(1.)).clamp(0.000001, 1.);
        Self::with_scale(doc_long, panel_long, scale, start, view)
    }

    fn ruler(doc_long: f32, panel_long: f32, start: f32, view: f32) -> Self {
        Self::with_scale(
            doc_long,
            panel_long,
            panel_long / doc_long.max(1.),
            start,
            view,
        )
    }

    fn with_scale(doc_long: f32, panel_long: f32, scale: f32, start: f32, view: f32) -> Self {
        let doc_long = doc_long.max(1.);
        let panel_long = panel_long.max(1.);
        let view_len = view.clamp(0., doc_long);
        let travel = (doc_long - view_len).max(0.);
        let start = start.clamp(0., travel);
        let content = doc_long * scale;
        let lead = ((panel_long - content) / 2.).max(0.);
        let track = content.min(panel_long);
        let thumb_len = (view_len * scale).max(MIN_THUMB.min(track)).min(track);
        let thumb_travel = (track - thumb_len).max(0.);
        let progress = if travel > 0. { start / travel } else { 0. };
        Self {
            scale,
            lead,
            scroll: (content - panel_long).max(0.) * progress - lead,
            thumb_start: lead + thumb_travel * progress,
            thumb_len,
            travel,
            thumb_travel,
            view_len,
        }
    }

    fn image_at(self, panel: f32) -> f32 {
        (panel + self.scroll) / self.scale
    }
    fn view_start_for_thumb(self, start: f32) -> f32 {
        if self.thumb_travel > 0. {
            (start - self.lead).clamp(0., self.thumb_travel) / self.thumb_travel * self.travel
        } else {
            0.
        }
    }
    fn contains_thumb(self, position: f32) -> bool {
        position >= self.thumb_start && position <= self.thumb_start + self.thumb_len
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NavRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl NavRect {
    fn relative(self, parent: Self) -> Self {
        Self {
            x: self.x - parent.x,
            y: self.y - parent.y,
            ..self
        }
    }
    fn long(self, axis: ScrollAxis) -> f32 {
        along(axis, self.width, self.height).0
    }
    fn cross(self, axis: ScrollAxis) -> f32 {
        along(axis, self.width, self.height).1
    }
    fn origin_long(self, axis: ScrollAxis) -> f32 {
        along(axis, self.x, self.y).0
    }
}

/// 窗口逻辑像素下的停靠分区；纵向长图的缩略图与全长尺在画布右侧，横向时在画布下方。
#[derive(Clone, Copy, Debug)]
pub(crate) struct NavigationLayout {
    pub(crate) top: NavRect,
    pub(crate) status: NavRect,
    pub(crate) rail: NavRect,
    /// 属性栏；收起或窄窗口时宽度为 0。
    pub(crate) side: NavRect,
    /// 缩略图与全长尺的外框。
    pub(crate) panel: NavRect,
    pub(crate) mini: NavRect,
    pub(crate) ruler: NavRect,
    pub(crate) main: NavRect,
}

impl NavigationLayout {
    pub(crate) fn new(axis: ScrollAxis, width: f32, height: f32, side: bool) -> Self {
        let width = width.max(1.);
        let height = height.max(1.);
        let narrow = width < NARROW_WIDTH;
        let rail_w = if narrow {
            NARROW_RAIL_WIDTH
        } else {
            RAIL_WIDTH
        }
        .min(width / 3.);
        let side_w = if side && !narrow { SIDEBAR_WIDTH } else { 0. };
        let mini_w = if narrow { 0. } else { MINI_WIDTH };
        // 小窗口先压状态栏和顶栏之间的画布，至少保留一小段可操作区域。
        let top = TOP_BAR.min((height - MIN_MAIN_HEIGHT) / 2.).max(0.);
        let bottom = (height - STATUS_BAR).max(top + 1.);
        let body = bottom - top;
        let right = (width - side_w).max(rail_w + 1.);
        let (panel, mini, ruler, main) = match axis {
            ScrollAxis::Vertical => {
                let ruler = NavRect {
                    x: right - RULER_WIDTH,
                    y: top,
                    width: RULER_WIDTH,
                    height: body,
                };
                let mini = NavRect {
                    x: ruler.x - mini_w,
                    y: top,
                    width: mini_w,
                    height: body,
                };
                let panel = NavRect {
                    width: mini_w + RULER_WIDTH,
                    ..mini
                };
                let main = NavRect {
                    x: rail_w,
                    y: top,
                    width: (mini.x - rail_w).max(1.),
                    height: body,
                };
                (panel, mini, ruler, main)
            }
            ScrollAxis::Horizontal => {
                let mini_h = mini_w.min((body - MIN_MAIN_HEIGHT - RULER_WIDTH).max(0.));
                let ruler = NavRect {
                    x: rail_w,
                    y: bottom - RULER_WIDTH,
                    width: right - rail_w,
                    height: RULER_WIDTH,
                };
                let mini = NavRect {
                    x: rail_w,
                    y: ruler.y - mini_h,
                    width: right - rail_w,
                    height: mini_h,
                };
                let panel = NavRect {
                    height: mini_h + RULER_WIDTH,
                    ..mini
                };
                let main = NavRect {
                    x: rail_w,
                    y: top,
                    width: right - rail_w,
                    height: (mini.y - top).max(1.),
                };
                (panel, mini, ruler, main)
            }
        };
        Self {
            top: NavRect {
                x: 0.,
                y: 0.,
                width,
                height: top,
            },
            status: NavRect {
                x: 0.,
                y: bottom,
                width,
                height: height - bottom,
            },
            rail: NavRect {
                x: 0.,
                y: top,
                width: rail_w,
                height: body,
            },
            side: NavRect {
                x: width - side_w,
                y: top,
                width: side_w,
                height: body,
            },
            panel,
            mini,
            ruler,
            main,
        }
    }
}

/// 逻辑像素编辑区，与主视口导航使用完全相同的停靠分区。
pub(crate) fn editing_area(
    axis: ScrollAxis,
    width: f32,
    height: f32,
    side: bool,
) -> crate::toolbar_layout::PanelRect {
    let main = NavigationLayout::new(axis, width, height, side).main;
    crate::toolbar_layout::PanelRect {
        x: main.x,
        y: main.y,
        width: main.width,
        height: main.height,
    }
}

impl CaptureView {
    pub(crate) fn long_clip_side_open(&self) -> bool {
        self.long_clip_ui.as_ref().is_some_and(|ui| ui.panel)
    }

    /// 当前窗口的停靠分区；没有剪辑文档时为空。
    pub(crate) fn long_clip_layout(&self, window: &Window) -> Option<NavigationLayout> {
        let axis = self.effective_long_clip()?.axis();
        let size = window.viewport_size();
        Some(NavigationLayout::new(
            axis,
            f32::from(size.width),
            f32::from(size.height),
            self.long_clip_side_open(),
        ))
    }

    fn navigation_maps(
        &self,
        window: &Window,
        projection: &LongClipProjection,
    ) -> (NavigationLayout, NavigationMap, NavigationMap) {
        let size = window.viewport_size();
        let axis = projection.axis();
        let layout = NavigationLayout::new(
            axis,
            f32::from(size.width),
            f32::from(size.height),
            self.long_clip_side_open(),
        );
        let (long, cross) = along(
            axis,
            projection.display_size().0 as f32,
            projection.display_size().1 as f32,
        );
        let viewport = self.canvas_viewport(window);
        let offset = along(axis, viewport.offset.x, viewport.offset.y).0;
        let view_len = (layout.main.long(axis) * window.scale_factor() / viewport.zoom).min(long);
        let start =
            (layout.main.origin_long(axis) * window.scale_factor() - offset) / viewport.zoom;
        (
            layout,
            NavigationMap::new(
                long,
                cross,
                layout.mini.long(axis),
                layout.mini.cross(axis),
                start,
                view_len,
            ),
            NavigationMap::ruler(long, layout.ruler.long(axis), start, view_len),
        )
    }

    fn navigation_set_start(
        &mut self,
        start: f32,
        window: &Window,
        projection: &LongClipProjection,
    ) {
        let (layout, map, _) = self.navigation_maps(window, projection);
        let Some(canvas) = &mut self.image_canvas else {
            return;
        };
        let offset = layout.main.origin_long(projection.axis()) * window.scale_factor()
            - start.clamp(0., map.travel) * canvas.viewport.zoom;
        match projection.axis() {
            ScrollAxis::Vertical => canvas.viewport.offset.y = offset,
            ScrollAxis::Horizontal => canvas.viewport.offset.x = offset,
        }
        canvas.auto_fit = false;
        self.invalidate_inline_ocr_projection();
    }

    fn navigation_down(
        &mut self,
        event: &MouseDownEvent,
        part: NavigationPart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.long_clip_ui.as_ref().is_some_and(|ui| ui.space) {
            return;
        }
        let Some(projection) = self.long_clip_projection() else {
            return;
        };
        let (layout, mini, ruler) = self.navigation_maps(window, &projection);
        let (map, rect) = match part {
            NavigationPart::Mini => (mini, layout.mini),
            NavigationPart::Ruler => (ruler, layout.ruler),
        };
        let position = along(
            projection.axis(),
            f32::from(event.position.x) - rect.x,
            f32::from(event.position.y) - rect.y,
        )
        .0
        .clamp(0., rect.long(projection.axis()));
        if event.modifiers.shift {
            let display_long = map.image_at(position).clamp(
                0.,
                along(
                    projection.axis(),
                    projection.display_size().0,
                    projection.display_size().1,
                )
                .0 as f32,
            );
            let cross = along(
                projection.axis(),
                projection.source_size().0,
                projection.source_size().1,
            )
            .1 as f32
                / 2.;
            let (x, y) = compose(projection.axis(), display_long, cross);
            if let Some(source) = projection.display_to_source(Point { x, y }, SeamSide::After) {
                let long = along(projection.axis(), source.x, source.y).0;
                if let Some(ui) = &mut self.long_clip_ui {
                    let limit = along(
                        projection.axis(),
                        projection.source_size().0,
                        projection.source_size().1,
                    )
                    .0;
                    let end = ui
                        .snap_position(long, 6. / map.scale.max(0.000001), event.modifiers.alt)
                        .min(limit);
                    if let Some(anchor) = ui.anchor {
                        ui.set_selection(anchor, end);
                    } else {
                        ui.anchor = Some(end);
                    }
                }
            }
        } else {
            let mut current = map;
            if !current.contains_thumb(position) {
                self.navigation_set_start(
                    current.image_at(position) - current.view_len / 2.,
                    window,
                    &projection,
                );
                let (_, mini, ruler) = self.navigation_maps(window, &projection);
                current = match part {
                    NavigationPart::Mini => mini,
                    NavigationPart::Ruler => ruler,
                };
            }
            if let Some(ui) = &mut self.long_clip_ui {
                ui.navigation.drag = Some(NavigationDrag {
                    part,
                    grab: (position - current.thumb_start).clamp(0., current.thumb_len),
                });
            }
            unsafe {
                SetCapture(hwnd(window));
            }
        }
        window.focus(&self.focus);
        cx.stop_propagation();
        cx.notify();
    }

    fn navigation_drag_to(&mut self, position: gpui::Point<gpui::Pixels>, window: &Window) -> bool {
        let Some(drag) = self.long_clip_ui.as_ref().and_then(|ui| ui.navigation.drag) else {
            return false;
        };
        let Some(projection) = self.long_clip_projection() else {
            return false;
        };
        let (layout, mini, ruler) = self.navigation_maps(window, &projection);
        let (map, rect) = match drag.part {
            NavigationPart::Mini => (mini, layout.mini),
            NavigationPart::Ruler => (ruler, layout.ruler),
        };
        let at = along(
            projection.axis(),
            f32::from(position.x) - rect.x,
            f32::from(position.y) - rect.y,
        )
        .0;
        self.navigation_set_start(
            map.view_start_for_thumb(at - drag.grab),
            window,
            &projection,
        );
        true
    }

    pub(crate) fn long_clip_navigation_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.navigation_drag_to(event.position, window) {
            return false;
        }
        if !event
            .pressed_button
            .is_some_and(|button| button == MouseButton::Left)
        {
            self.long_clip_navigation_cancel(window);
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn long_clip_navigation_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.button != MouseButton::Left || !self.navigation_drag_to(event.position, window) {
            return false;
        }
        self.long_clip_navigation_cancel(window);
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn long_clip_navigation_cancel(&mut self, window: &Window) -> bool {
        let active = self
            .long_clip_ui
            .as_mut()
            .is_some_and(|ui| ui.navigation.cancel());
        if active {
            unsafe {
                if GetCapture() == hwnd(window) {
                    let _ = ReleaseCapture();
                }
            }
        }
        active
    }

    /// 仅缓存有界整图缩略栅格；滚动、展开与删段只重放现有纹理中的片段。
    fn navigation_preview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let full = self.source.full();
        let dimensions = thumbnail_dimensions(
            full.width,
            full.height,
            self.effective_long_clip()?.axis(),
            window.scale_factor(),
        );
        let source_id = self.previews.first().map_or(0, |image| {
            self.source.original(image) as *const RenderImage as usize
        });
        let needs_source = self
            .long_clip_ui
            .as_ref()?
            .navigation
            .preview
            .as_ref()
            .is_none_or(|cache| cache.dimensions != dimensions || cache.source_id != source_id);
        let projection = OutputProjection {
            source: full,
            width: dimensions.0,
            height: dimensions.1,
            sx: dimensions.0 as f32 / full.width as f32,
            sy: dimensions.1 as f32 / full.height as f32,
            origin: Point { x: 0., y: 0. },
        };
        if needs_source {
            let image = projection.background(&self.source_pixels()).ok()?;
            let original = render_image(image.width, image.height, image.bgra);
            let old = self
                .long_clip_ui
                .as_mut()?
                .navigation
                .preview
                .replace(NavigationPreview {
                    source_id,
                    dimensions,
                    original,
                    composited: None,
                    scene_stamp: None,
                });
            if let Some(old) = old {
                self.retired_raster_previews.push(old.original);
                if let Some(image) = old.composited {
                    self.retired_raster_previews.push(image);
                }
            }
        }
        let stamp = self.editor.scene_stamp();
        let now = Instant::now();
        let state = &mut self.long_clip_ui.as_mut()?.navigation;
        if state.seen_stamp != Some(stamp) {
            state.seen_stamp = Some(stamp);
            state.changed_at = Some(now);
        }
        let stale = state
            .preview
            .as_ref()
            .is_some_and(|cache| cache.scene_stamp != Some(stamp));
        let settled = state
            .changed_at
            .is_some_and(|at| now.duration_since(at) >= SCENE_DEBOUNCE);
        if stale && settled && !self.editor.is_dragging() && !self.editor.has_pending_drawing() {
            let scene = self.prepared_scene();
            let image = projection
                .background(&self.source_pixels())
                .and_then(|mut image| {
                    scene.apply_display(&mut image, &self.source_pixels(), &projection)?;
                    if let Some(mask) = self.editor.lasso_draft.as_ref().or(self
                        .editor
                        .document
                        .crop_mask
                        .as_ref())
                    {
                        mask.apply_projected(&mut image, &projection);
                    }
                    Ok(render_image(image.width, image.height, image.bgra))
                });
            let cache = self.long_clip_ui.as_mut()?.navigation.preview.as_mut()?;
            if let Ok(image) = image {
                // 失败时保留旧图及旧 stamp；下次仍需重试本次 Scene 和套索。
                cache.scene_stamp = Some(stamp);
                if let Some(old) = cache.composited.replace(image) {
                    self.retired_raster_previews.push(old);
                }
            }
        } else if stale && !self.long_clip_ui.as_ref()?.navigation.wake_pending {
            self.long_clip_ui.as_mut()?.navigation.wake_pending = true;
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(SCENE_DEBOUNCE).await;
                let _ = view.update(cx, |view, cx| {
                    if let Some(ui) = &mut view.long_clip_ui {
                        ui.navigation.wake_pending = false;
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        let cache = self.long_clip_ui.as_ref()?.navigation.preview.as_ref()?;
        Some(cache.composited.as_ref().unwrap_or(&cache.original).clone())
    }

    pub(crate) fn long_clip_navigation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(projection) = self.long_clip_projection() else {
            return div().into_any_element();
        };
        if self.long_clip_ui.is_none() {
            return div().into_any_element();
        }
        let (layout, mini_map, ruler_map) = self.navigation_maps(window, &projection);
        let image = self.navigation_preview(window, cx);
        let theme = ClipTheme::new(self.light_theme);
        let dragging = self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.navigation.drag.is_some());
        let selection = self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.selection.clone());
        let anchor = self.long_clip_ui.as_ref().and_then(|ui| ui.anchor);
        let markers: Vec<_> = self
            .editor
            .document
            .items()
            .iter()
            .take(512)
            .map(|item| {
                let bounds = item.annotation.bounds();
                (
                    Point {
                        x: bounds.x + bounds.width / 2.,
                        y: bounds.y + bounds.height / 2.,
                    },
                    item.annotation.style.color,
                )
            })
            .collect();
        let mini = strip(
            &projection,
            mini_map,
            layout.mini,
            layout.panel,
            image.clone(),
            theme,
            selection.as_ref(),
            anchor,
            &markers,
            false,
            dragging,
        )
        .id("long-clip-readable-map")
        .child(self.probe("long-clip-readable-map"))
        .cursor(CursorStyle::OpenHand)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.navigation_down(event, NavigationPart::Mini, window, cx)
            }),
        )
        .on_mouse_move(cx.listener(|view, event, window, cx| {
            view.long_clip_navigation_move(event, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.long_clip_navigation_up(event, window, cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.long_clip_navigation_up(event, window, cx);
            }),
        )
        .on_scroll_wheel(cx.listener(
            |view, event: &gpui::ScrollWheelEvent, window, cx| {
                let Some(projection) = view.long_clip_projection() else {
                    return;
                };
                let (_, map, _) = view.navigation_maps(window, &projection);
                let delta = match event.delta {
                    ScrollDelta::Lines(p) => (p.x + p.y) * 48.,
                    ScrollDelta::Pixels(p) => f32::from(p.x) + f32::from(p.y),
                };
                let current = map.view_start_for_thumb(map.thumb_start);
                view.navigation_set_start(current - delta / map.scale, window, &projection);
                cx.stop_propagation();
                cx.notify();
            },
        ));
        let ruler = strip(
            &projection,
            ruler_map,
            layout.ruler,
            layout.panel,
            image,
            theme,
            selection.as_ref(),
            anchor,
            &markers,
            true,
            dragging,
        )
        .id("long-clip-full-ruler")
        .child(self.probe("long-clip-full-ruler"))
        .cursor(CursorStyle::OpenHand)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.navigation_down(event, NavigationPart::Ruler, window, cx)
            }),
        )
        .on_mouse_move(cx.listener(|view, event, window, cx| {
            view.long_clip_navigation_move(event, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.long_clip_navigation_up(event, window, cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|view, event, window, cx| {
                view.long_clip_navigation_up(event, window, cx);
            }),
        );
        // 停靠在画布与属性栏之间：缩略图在井色底上，全长尺在面板底上，各带一条分隔线。
        block(layout.panel)
            .id("long-clip-navigation")
            .bg(rgb(theme.well))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    if !view.long_clip_ui.as_ref().is_some_and(|ui| ui.space) {
                        cx.stop_propagation();
                    }
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(mini)
            .child(ruler)
            .into_any_element()
    }
}

#[allow(clippy::too_many_arguments)]
fn strip(
    projection: &LongClipProjection,
    map: NavigationMap,
    rect: NavRect,
    parent: NavRect,
    image: Option<Arc<RenderImage>>,
    t: ClipTheme,
    selection: Option<&Range<u32>>,
    anchor: Option<u32>,
    markers: &[(Point, u32)],
    ruler: bool,
    dragging: bool,
) -> gpui::Div {
    let axis = projection.axis();
    let (_, doc_cross) = along(
        axis,
        projection.source_size().0 as f32,
        projection.source_size().1 as f32,
    );
    let panel_long = rect.long(axis);
    let breadth = rect.cross(axis);
    let cross = if ruler {
        breadth
    } else {
        doc_cross * map.scale
    };
    let cross_offset = (breadth - cross) / 2.;
    let cross_scale = cross / doc_cross.max(1.);
    let source_long = along(
        axis,
        projection.source_size().0 as f32,
        projection.source_size().1 as f32,
    )
    .0 as u32;
    let mut children = Vec::new();
    for piece in projection.pieces() {
        let (source_start, source_cross) =
            along(axis, piece.source.x as f32, piece.source.y as f32);
        let display_start = along(axis, piece.display.x as f32, piece.display.y as f32).0;
        let length = along(
            axis,
            piece.display.width as f32,
            piece.display.height as f32,
        )
        .0;
        let start = display_start * map.scale - map.scroll;
        let len = length * map.scale;
        if start + len < 0. || start > panel_long {
            continue;
        }
        if ruler {
            // 全长尺不画图像：展开的删除段为半透明红块。
            if piece.removed {
                children.push(
                    block(axis_rect(
                        axis,
                        start,
                        len.max(2.),
                        1.,
                        (breadth - 6.).max(1.),
                    ))
                    .bg(alpha(t.cut, 0.55))
                    .into_any_element(),
                );
            }
        } else {
            let at = axis_rect(axis, start, len, cross_offset, cross);
            let mut layer = block(at).overflow_hidden();
            if let Some(image) = &image {
                let (w, h) = compose(
                    axis,
                    along(
                        axis,
                        projection.source_size().0 as f32,
                        projection.source_size().1 as f32,
                    )
                    .0 * map.scale,
                    doc_cross * cross_scale,
                );
                let (x, y) = compose(axis, -source_start * map.scale, -source_cross * cross_scale);
                layer = layer.child(
                    img(image.clone())
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(w))
                        .h(px(h))
                        .object_fit(ObjectFit::Fill),
                );
            }
            if piece.removed {
                layer = layer
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .bg(rgba(0x080a0d8c)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .bg(alpha(t.cut, 0.35)),
                    );
            }
            children.push(layer.into_any_element());
        }
        if let Some(selection) = selection {
            let a = (source_start as u32).max(selection.start);
            let b = (source_start as u32 + length as u32).min(selection.end);
            if a < b {
                let (c, b_len) = if ruler {
                    (1., (breadth - 6.).max(1.))
                } else {
                    (cross_offset, cross)
                };
                children.push(
                    block(axis_rect(
                        axis,
                        start + (a as f32 - source_start) * map.scale,
                        ((b - a) as f32 * map.scale).max(1.5),
                        c,
                        b_len,
                    ))
                    .bg(alpha(t.cut, if ruler { 0.85 } else { 0.4 }))
                    .into_any_element(),
                );
            }
        }
    }
    let kept = projection.cross_range();
    if !ruler {
        for (start, width) in [
            (0., kept.start as f32 * cross_scale),
            (
                kept.end as f32 * cross_scale,
                (doc_cross - kept.end as f32) * cross_scale,
            ),
        ] {
            if width > 0. {
                children.push(
                    block(axis_rect(axis, 0., panel_long, cross_offset + start, width))
                        .bg(rgba(0x06080a99))
                        .into_any_element(),
                );
            }
        }
    }
    for seam in projection.seams() {
        let at = seam.display_at as f32 * map.scale - map.scroll;
        if at < -2. || at > panel_long + 2. {
            continue;
        }
        let (c, b_len) = if ruler {
            (1., (breadth - 6.).max(1.))
        } else {
            (0., breadth)
        };
        children.push(
            block(axis_rect(axis, at - 1., 2., c, b_len))
                .bg(rgb(t.cut))
                .into_any_element(),
        );
        // 缩略图在内部接缝旁标删除量。
        if !ruler
            && axis == ScrollAxis::Vertical
            && seam.source.start > 0
            && seam.source.end < source_long
        {
            children.push(
                div()
                    .absolute()
                    .right(px(3.))
                    .top(px(at - 14.))
                    .px(px(2.))
                    .bg(rgba(0x00000099))
                    .text_size(px(9.5))
                    .line_height(px(12.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(t.cut))
                    .child(format!("−{}", short(seam.source.end - seam.source.start)))
                    .into_any_element(),
            );
        }
    }
    if ruler {
        for &(point, color) in markers {
            if let Some(point) = projection.source_to_display_nearest(point) {
                let at = along(axis, point.x, point.y).0 * map.scale;
                children.push(
                    block(axis_rect(axis, at - 1., 2., breadth - 5., 4.))
                        .bg(rgb(color))
                        .into_any_element(),
                );
            }
        }
    }
    if let Some(anchor) = anchor {
        let (x, y) = compose(axis, anchor as f32, doc_cross / 2.);
        if let Some(point) = projection.source_to_display_nearest(Point { x, y }) {
            let at = along(axis, point.x, point.y).0 * map.scale - map.scroll;
            if at >= 0. && at <= panel_long {
                children.push(
                    block(axis_rect(
                        axis,
                        at - if ruler { 1. } else { 0.5 },
                        if ruler { 2. } else { 1. },
                        0.,
                        breadth,
                    ))
                    .bg(rgb(t.blue))
                    .into_any_element(),
                );
            }
        }
    }
    // 视口滑块：缩略图为浅色罩加上下两条线，全长尺为整条浅色块。
    if ruler {
        children.push(
            block(axis_rect(
                axis,
                map.thumb_start,
                map.thumb_len.max(6.),
                1.,
                (breadth - 2.).max(1.),
            ))
            .bg(alpha(t.fg, 0.16))
            .into_any_element(),
        );
    } else {
        let line = alpha(t.fg, if dragging { 0.55 } else { 0.3 });
        children.push(
            block(axis_rect(axis, map.thumb_start, map.thumb_len, 0., breadth))
                .bg(alpha(t.fg, if dragging { 0.24 } else { 0.09 }))
                .into_any_element(),
        );
        for at in [map.thumb_start, map.thumb_start + map.thumb_len - 1.] {
            children.push(
                block(axis_rect(axis, at, 1., 0., breadth))
                    .bg(line)
                    .into_any_element(),
            );
        }
    }
    block(rect.relative(parent))
        .overflow_hidden()
        .bg(rgb(if ruler { t.panel } else { t.well }))
        .map(|d| match axis {
            ScrollAxis::Vertical => d.border_l_1(),
            ScrollAxis::Horizontal => d.border_t_1(),
        })
        .border_color(rgb(t.border))
        .children(children)
}

fn block(rect: NavRect) -> gpui::Div {
    div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width.max(0.)))
        .h(px(rect.height.max(0.)))
}
fn along<T>(axis: ScrollAxis, x: T, y: T) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (y, x),
        ScrollAxis::Horizontal => (x, y),
    }
}
fn compose<T>(axis: ScrollAxis, long: T, cross: T) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (cross, long),
        ScrollAxis::Horizontal => (long, cross),
    }
}
fn axis_rect(axis: ScrollAxis, start: f32, length: f32, cross: f32, breadth: f32) -> NavRect {
    let (x, y) = compose(axis, start, cross);
    let (width, height) = compose(axis, length, breadth);
    NavRect {
        x,
        y,
        width,
        height,
    }
}
fn thumbnail_dimensions(width: u32, height: u32, axis: ScrollAxis, dpi: f32) -> (u32, u32) {
    let cross = along(axis, width, height).1.max(1) as f64;
    let width = f64::from(width.max(1));
    let height = f64::from(height.max(1));
    let scale = (f64::from(MINI_WIDTH * dpi.max(0.1)) / cross)
        .min(1.)
        .min(MAX_PREVIEW_SIDE / width.max(height))
        .min((MAX_PREVIEW_PIXELS / (width * height)).sqrt());
    (
        (width * scale).floor().max(1.) as u32,
        (height * scale).floor().max(1.) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_clip_navigation_map_keeps_readable_width_and_reaches_both_ends() {
        let first = NavigationMap::new(4320., 1080., 600., 180., 0., 1200.);
        let last = NavigationMap::new(4320., 1080., 600., 180., 3120., 1200.);
        assert_eq!(first.scale, 1. / 6.);
        assert_eq!(first.scroll, 0.);
        assert!((last.scroll - 120.).abs() < 0.001);
        assert_eq!(first.view_start_for_thumb(-50.), 0.);
        assert!((last.view_start_for_thumb(1e6) - 3120.).abs() < 0.001);
        assert!((last.thumb_start + last.thumb_len - 600.).abs() < 0.001);
        assert!((last.image_at(0.) - 720.).abs() < 0.001);
    }
    #[test]
    fn long_clip_navigation_minimum_thumb_uses_effective_travel() {
        let map = NavigationMap::new(32000., 1080., 600., 180., 31925., 75.);
        assert_eq!(map.thumb_len, MIN_THUMB);
        assert!((map.thumb_start - 582.).abs() < 0.001);
        assert!((map.view_start_for_thumb(582.) - 31925.).abs() < 0.001);
        let global = NavigationMap::ruler(32000., 600., 31925., 75.);
        assert_eq!(global.thumb_len, MIN_THUMB);
        assert!((global.view_start_for_thumb(582.) - 31925.).abs() < 0.001);
    }
    #[test]
    fn long_clip_navigation_short_image_and_full_view_do_not_divide_by_zero() {
        // 54 长的内容在 600 面板中居中：前后各留 273。
        let map = NavigationMap::new(300., 1000., 600., 180., 280., 20.);
        assert!((map.scroll + 273.).abs() < 0.001);
        assert!((map.thumb_start + map.thumb_len - 327.).abs() < 0.001);
        assert!((map.image_at(273.) - 0.).abs() < 0.001);
        assert!((map.view_start_for_thumb(1e6) - 280.).abs() < 0.001);
        let full = NavigationMap::new(300., 1000., 600., 180., 0., 300.);
        assert_eq!(full.view_start_for_thumb(100.), 0.);
        assert!((full.thumb_len - 54.).abs() < 0.001);
    }
    #[test]
    fn long_clip_navigation_thumbnail_budget_is_bounded_for_both_axes() {
        for (width, height, axis) in [
            (1080, 4320, ScrollAxis::Vertical),
            (1080, 32000, ScrollAxis::Vertical),
            (32000, 1080, ScrollAxis::Horizontal),
            (1, 65535, ScrollAxis::Vertical),
        ] {
            let (w, h) = thumbnail_dimensions(width, height, axis, 2.);
            assert!(w > 0 && h > 0 && w <= 8192 && h <= 8192);
            assert!(u64::from(w) * u64::from(h) <= MAX_PREVIEW_PIXELS as u64);
        }
        let vertical = NavigationLayout::new(ScrollAxis::Vertical, 1280., 800., true);
        let horizontal = NavigationLayout::new(ScrollAxis::Horizontal, 1280., 800., true);
        assert_eq!(vertical.mini.width, MINI_WIDTH);
        assert_eq!(horizontal.mini.height, MINI_WIDTH);
        assert!(vertical.main.width > 0. && horizontal.main.height > 0.);
    }

    #[test]
    fn long_clip_navigation_docks_match_design_and_stay_disjoint() {
        for (width, height) in [(860., 680.), (1280., 800.)] {
            for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
                for side in [true, false] {
                    let layout = NavigationLayout::new(axis, width, height, side);
                    let main = editing_area(axis, width, height, side);
                    assert_eq!(
                        main,
                        crate::toolbar_layout::PanelRect {
                            x: layout.main.x,
                            y: layout.main.y,
                            width: layout.main.width,
                            height: layout.main.height,
                        }
                    );
                    assert_eq!(layout.top.height, TOP_BAR);
                    assert_eq!(layout.status.y, height - STATUS_BAR);
                    assert_eq!(layout.rail.width, RAIL_WIDTH);
                    assert_eq!(layout.main.x, RAIL_WIDTH);
                    assert_eq!(layout.main.y, TOP_BAR);
                    assert_eq!(layout.side.width, if side { SIDEBAR_WIDTH } else { 0. });
                    assert_eq!(layout.side.x + layout.side.width, width);
                    assert_eq!(layout.mini.cross(axis), MINI_WIDTH);
                    assert_eq!(layout.ruler.cross(axis), RULER_WIDTH);
                    match axis {
                        ScrollAxis::Vertical => {
                            assert_eq!(layout.main.x + layout.main.width, layout.mini.x);
                            assert_eq!(layout.mini.x + MINI_WIDTH, layout.ruler.x);
                            assert_eq!(layout.ruler.x + RULER_WIDTH, layout.side.x);
                            assert_eq!(layout.main.height, height - TOP_BAR - STATUS_BAR);
                        }
                        ScrollAxis::Horizontal => {
                            assert_eq!(layout.main.y + layout.main.height, layout.mini.y);
                            assert_eq!(layout.mini.y + MINI_WIDTH, layout.ruler.y);
                            assert_eq!(layout.ruler.y + RULER_WIDTH, layout.status.y);
                            assert_eq!(layout.main.x + layout.main.width, layout.side.x);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn long_clip_navigation_narrow_window_hides_overview_and_sidebar() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let layout = NavigationLayout::new(axis, 480., 480., true);
            assert_eq!(layout.side.width, 0.);
            assert_eq!(layout.mini.cross(axis), 0.);
            assert_eq!(layout.ruler.cross(axis), RULER_WIDTH);
            assert_eq!(layout.rail.width, NARROW_RAIL_WIDTH);
            assert!(layout.main.height >= MIN_MAIN_HEIGHT);
            assert!(layout.main.width > 0.);
        }
    }
}
