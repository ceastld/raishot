//! 长图编辑：浏览、删段、裁边后输出。共享条带进入编辑，不展平成整图；
//! 画面按缩放级别分块渲染并缓存，滚动只补新露出的块，编辑后内容未变的块直接复用。
//!
//! 交互不分模式：在图上沿截图方向拖动选出要删除的一段，拖图片四边裁掉开头、结尾或两侧，
//! 滚轮浏览、Ctrl+滚轮缩放、中键或按住空格拖动平移，右侧（横向长图在下方）总览条定位。
#[path = "long_editor_model.rs"]
mod model;
use crate::{
    annotation::Point,
    geometry::Rect,
    gpui_capture::hwnd,
    long_document::{self, LongDocument},
    stitch::{ScrollAxis, StitchSnapshot},
    toolbar_icons::{self, Icon},
    viewport::Viewport,
    Result,
};
use gpui::{
    div, img, prelude::*, px, rgb, rgba, size, AnyElement, App, AppContext, Bounds, Context,
    CursorStyle, FocusHandle, MouseButton, ObjectFit, Render, RenderImage, ScrollDelta,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
};
use model::{along, compose, Minimap, Side, TileKey};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use windows::Win32::UI::{
    HiDpi::GetDpiForSystem,
    Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
};

/// 顶栏、状态栏、总览条（逻辑像素）。
const TOP: f32 = 48.;
const STATUS: f32 = 32.;
const SIDE: f32 = 128.;
const MINI_PAD: f32 = 10.;
/// 图片四周留白，边缘拖柄始终有地方可按。
const PAD: f32 = 28.;
/// 边线命中半径。
const HIT: f32 = 6.;
/// 删段浮条尺寸。
const BAR_W: f32 = 268.;
const BAR_H: f32 = 36.;
const TILE_BUDGET: usize = 48;
const MAX_INFLIGHT: usize = 4;

#[derive(Clone, Copy)]
enum Action {
    Delete,
    Keep,
    Cancel,
    Undo,
    Redo,
    Reset,
    FitCross,
    FitAll,
    Actual,
    ZoomIn,
    ZoomOut,
    Copy,
    Save,
    Pin,
    Annotate,
    Return,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ZoomMode {
    FitCross,
    FitAll,
    Manual,
}
#[derive(Clone, Copy, PartialEq)]
enum Gesture {
    Pan {
        start: Point,
        offset: Point,
    },
    Band {
        anchor: f32,
        origin: Point,
        moved: bool,
    },
    BandEdge {
        fixed: u32,
    },
    Trim {
        side: Side,
        rect: Rect,
    },
    Minimap {
        grab: f32,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Outside,
    Margin,
    Doc,
    Side(Side),
    BandEdge { start: bool },
    Minimap,
}
#[derive(Clone, Copy)]
struct Metrics {
    dpi: f32,
    /// 主画布（窗口物理像素）。
    main: Rect,
    /// 总览条外框与可用区。
    panel: Rect,
    mini: Rect,
}
impl Default for Metrics {
    fn default() -> Self {
        let empty = Rect::full(0, 0);
        Self {
            dpi: 1.,
            main: empty,
            panel: empty,
            mini: empty,
        }
    }
}
struct CachedTile {
    image: Arc<RenderImage>,
    used: u64,
}
#[derive(Default)]
struct Plan {
    main: Vec<(TileKey, Rect)>,
    mini: Vec<(TileKey, Rect)>,
    mini_view: Viewport,
    minimap: Option<Minimap>,
}
/// 删段／裁边后新块渲染好之前，按编辑把旧块平移到新位置先画着，避免画面闪空。
struct GhostPiece {
    image: Arc<RenderImage>,
    /// 整块在新文档坐标中的位置（可为负）与实际可见部分。
    rect: (f32, f32, f32, f32),
    clip: (f32, f32, f32, f32),
}
enum Edit {
    Cut(u32, u32),
    Crop(Rect),
}

pub(crate) enum LongEditorOutcome {
    Return,
    Finished,
}
type Completion = Box<dyn FnOnce(LongEditorOutcome, &mut App)>;
struct LongEditorHint(SharedString);
impl Render for LongEditorHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::capture_toolbar::ui_kit::tip_body(crate::theme::palette(cx), &self.0)
    }
}
pub(crate) struct LongEditor {
    pub(crate) document: LongDocument,
    pub(crate) ui_positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
    pub(crate) viewport: Viewport,
    zoom_mode: ZoomMode,
    /// 待删除的一段（长轴原图坐标，左闭右开）。
    pub(crate) band: Option<(u32, u32)>,
    gesture: Option<Gesture>,
    pub(crate) gesture_button: Option<MouseButton>,
    hover: Hit,
    pointer: Option<Point>,
    space: bool,
    autoscroll: bool,
    metrics: Metrics,
    host: (u32, u32),
    focus: FocusHandle,
    revision: u64,
    shared: Arc<StitchSnapshot>,
    tiles: HashMap<TileKey, CachedTile>,
    pending: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    plan: Plan,
    ghost: Vec<GhostPiece>,
    clock: u64,
    pub(crate) retired: Vec<Arc<RenderImage>>,
    preview_created: u64,
    preview_released: u64,
    pub(crate) busy: bool,
    completion: Option<Completion>,
    /// 错误提示；下一次手势清空。
    pub(crate) status: String,
    /// 操作回执（如“已删除 … · Ctrl+Z 撤销”）；下一次操作清空。
    notice: String,
    /// 首次打开的操作引导；第一次选段、裁边或关闭后不再显示。
    onboarding: bool,
    /// 最近各帧 render 耗时（毫秒），供性能验收读取。
    pub(crate) render_ms: Vec<f32>,
}
impl LongEditor {
    /// 纵向长图按图宽、横向按图高估计窗口尺寸，放不下时占屏幕约 86%，居中打开。
    fn initial_bounds(document: &LongDocument, cx: &App) -> Bounds<gpui::Pixels> {
        let scale = (unsafe { GetDpiForSystem() } as f32 / 96.).max(1.);
        let (display_w, display_h) = cx
            .primary_display()
            .map(|d| {
                let b = d.bounds();
                (f32::from(b.size.width), f32::from(b.size.height))
            })
            .unwrap_or((1440., 900.));
        let fit = |value: f32, low: f32, high: f32| value.min(high).max(low);
        let (w, h) = (
            document.width() as f32 / scale,
            document.height() as f32 / scale,
        );
        let size = match document.axis() {
            ScrollAxis::Vertical => size(
                px(fit(w + 2. * PAD + SIDE, 860., display_w * 0.86)),
                px(fit(display_h * 0.86, 520., display_h)),
            ),
            ScrollAxis::Horizontal => size(
                px(fit(display_w * 0.86, 860., display_w)),
                px(fit(
                    h + 2. * PAD + TOP + STATUS + SIDE,
                    520.,
                    display_h * 0.86,
                )),
            ),
        };
        Bounds::centered(None, size, cx)
    }
    pub(crate) fn open(
        snapshot: StitchSnapshot,
        on_done: impl FnOnce(LongEditorOutcome, &mut App) + 'static,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let document = LongDocument::new(snapshot)?;
        let bounds = Self::initial_bounds(&document, cx);
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    appears_transparent: false,
                    ..Default::default()
                }),
                focus: true,
                window_min_size: Some(size(px(760.), px(420.))),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(brand_title!("调整长截图", "Edit long capture"));
                let focus = cx.focus_handle();
                window.focus(&focus);
                let shared = Arc::new(document.snapshot());
                let view = cx.new(|_| Self {
                    document,
                    ui_positions: Rc::new(RefCell::new(HashMap::new())),
                    viewport: Viewport::default(),
                    zoom_mode: ZoomMode::FitCross,
                    band: None,
                    gesture: None,
                    gesture_button: None,
                    hover: Hit::Outside,
                    pointer: None,
                    space: false,
                    autoscroll: false,
                    metrics: Metrics::default(),
                    host: (0, 0),
                    focus,
                    revision: 0,
                    shared,
                    tiles: HashMap::new(),
                    pending: HashSet::new(),
                    failed: HashSet::new(),
                    plan: Plan::default(),
                    ghost: Vec::new(),
                    clock: 0,
                    retired: Vec::new(),
                    preview_created: 0,
                    preview_released: 0,
                    busy: false,
                    completion: Some(Box::new(on_done)),
                    status: String::new(),
                    notice: String::new(),
                    render_ms: Vec::new(),
                    onboarding: true,
                });
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |w, cx| {
                    weak.update(cx, |v, cx| {
                        if v.busy {
                            return false;
                        }
                        v.cancel_pointer(w);
                        v.complete(LongEditorOutcome::Return, w, cx);
                        true
                    })
                    .unwrap_or(true)
                });
                view.update(cx, |_, cx| {
                    cx.observe_window_activation(window, |v, w, cx| {
                        if !w.is_window_active() {
                            v.space = false;
                            if v.gesture_button.is_some() {
                                v.cancel_pointer(w);
                            }
                            cx.notify();
                        }
                    })
                    .detach();
                });
                view
            },
        )?;
        Ok(handle)
    }
    fn complete(
        &mut self,
        outcome: LongEditorOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ghost.clear();
        self.plan = Plan::default();
        self.retired
            .extend(self.tiles.drain().map(|(_, tile)| tile.image));
        // 关闭后不会再绘制旧场景；窗口销毁前回收 atlas key 和 CPU 预览。
        self.release_retired(window);
        if let Some(done) = self.completion.take() {
            cx.defer(move |cx| done(outcome, cx));
        }
    }
    fn release_retired(&mut self, window: &mut Window) {
        for image in self.retired.drain(..) {
            match window.drop_image(image) {
                Ok(()) => self.preview_released = self.preview_released.saturating_add(1),
                Err(error) => self.status = format!("释放旧长图编辑预览纹理失败：{error}"),
            }
        }
    }

    // ---- 布局与视口 ----

    fn measure(window: &Window, axis: ScrollAxis) -> Metrics {
        let dpi = window.scale_factor();
        let viewport = window.viewport_size();
        let (w, h) = (
            (f32::from(viewport.width) * dpi).round() as u32,
            (f32::from(viewport.height) * dpi).round() as u32,
        );
        let scaled = |v: f32| (v * dpi).round() as u32;
        let (top, bottom, side, pad) =
            (scaled(TOP), scaled(STATUS), scaled(SIDE), scaled(MINI_PAD));
        let body = h.saturating_sub(top + bottom).max(1);
        let (main, panel) = match axis {
            ScrollAxis::Vertical => {
                let width = w.saturating_sub(side).max(1);
                (
                    Rect {
                        x: 0,
                        y: top,
                        width,
                        height: body,
                    },
                    Rect {
                        x: width,
                        y: top,
                        width: side,
                        height: body,
                    },
                )
            }
            ScrollAxis::Horizontal => {
                let height = body.saturating_sub(side).max(1);
                (
                    Rect {
                        x: 0,
                        y: top,
                        width: w.max(1),
                        height,
                    },
                    Rect {
                        x: 0,
                        y: top + height,
                        width: w.max(1),
                        height: side,
                    },
                )
            }
        };
        let mini = Rect {
            x: panel.x + pad,
            y: panel.y + pad,
            width: panel.width.saturating_sub(2 * pad).max(1),
            height: panel.height.saturating_sub(2 * pad).max(1),
        };
        Metrics {
            dpi,
            main,
            panel,
            mini,
        }
    }
    fn client(p: gpui::Point<gpui::Pixels>, window: &Window) -> Point {
        Point {
            x: f32::from(p.x) * window.scale_factor(),
            y: f32::from(p.y) * window.scale_factor(),
        }
    }
    fn axis(&self) -> ScrollAxis {
        self.document.axis()
    }
    fn doc_long_cross(&self) -> (u32, u32) {
        along(self.axis(), self.document.width(), self.document.height())
    }
    /// 主画布沿 (长轴, 跨轴) 的起点与长度。
    fn main_spans(&self) -> ((f32, f32), (f32, f32)) {
        let m = self.metrics.main;
        let (start_l, start_c) = along(self.axis(), m.x as f32, m.y as f32);
        let (len_l, len_c) = along(self.axis(), m.width as f32, m.height as f32);
        ((start_l, len_l), (start_c, len_c))
    }
    fn offset_long(&self) -> f32 {
        along(self.axis(), self.viewport.offset.x, self.viewport.offset.y).0
    }
    fn set_offset(&mut self, long: f32, cross: f32) {
        let (x, y) = compose(self.axis(), long, cross);
        self.viewport.offset = Point { x, y };
    }
    fn clamp_view(&mut self) {
        let pad = PAD * self.metrics.dpi.max(1.);
        let zoom = self.viewport.zoom;
        let m = self.metrics.main;
        self.viewport.offset.x = model::clamp_offset(
            self.viewport.offset.x,
            self.document.width() as f32 * zoom,
            m.x as f32,
            m.width as f32,
            pad,
        );
        self.viewport.offset.y = model::clamp_offset(
            self.viewport.offset.y,
            self.document.height() as f32 * zoom,
            m.y as f32,
            m.height as f32,
            pad,
        );
    }
    /// 视口起点（长轴原图坐标）与可见长度。
    fn view_range(&self) -> (f32, f32) {
        let ((start, len), _) = self.main_spans();
        (
            (start - self.offset_long()) / self.viewport.zoom,
            len / self.viewport.zoom,
        )
    }
    fn set_view_start(&mut self, position: f32) {
        let ((start, _), _) = self.main_spans();
        let cross = along(self.axis(), self.viewport.offset.x, self.viewport.offset.y).1;
        self.set_offset(start - position * self.viewport.zoom, cross);
        self.clamp_view();
    }
    fn fit_zoom(&self, all: bool) -> f32 {
        let pad = 2. * PAD * self.metrics.dpi.max(1.);
        let ((_, long), (_, cross)) = self.main_spans();
        let (doc_long, doc_cross) = self.doc_long_cross();
        let by_cross = (cross - pad).max(1.) / doc_cross.max(1) as f32;
        let zoom = if all {
            by_cross.min((long - pad).max(1.) / doc_long.max(1) as f32)
        } else {
            by_cross
        };
        zoom.clamp(0.0001, 1.)
    }
    fn min_zoom(&self) -> f32 {
        self.fit_zoom(true).min(0.25)
    }
    /// 改变倍率时保持视口起点所在原图位置不动。
    fn set_zoom_keep_start(&mut self, zoom: f32) {
        let (start, _) = self.view_range();
        self.viewport.zoom = zoom;
        self.set_view_start(start);
    }
    fn zoom_at(&mut self, zoom: f32, anchor: Point) {
        self.zoom_mode = ZoomMode::Manual;
        let zoom = zoom.clamp(self.min_zoom(), 8.);
        self.viewport.set_zoom(zoom, anchor);
        self.clamp_view();
    }
    fn apply_zoom_mode(&mut self) {
        match self.zoom_mode {
            ZoomMode::FitCross => self.set_zoom_keep_start(self.fit_zoom(false)),
            ZoomMode::FitAll => self.set_zoom_keep_start(self.fit_zoom(true)),
            ZoomMode::Manual => self.clamp_view(),
        }
    }
    fn sync_layout(&mut self, window: &Window) {
        self.metrics = Self::measure(window, self.axis());
        let host = (self.metrics.main.width, self.metrics.main.height);
        if self.host != host {
            let first = self.host == (0, 0);
            self.host = host;
            if first {
                self.viewport.zoom = self.fit_zoom(false);
                self.set_view_start(-PAD * self.metrics.dpi / self.viewport.zoom);
            } else {
                self.apply_zoom_mode();
            }
        }
    }
    fn scroll_long(&mut self, delta: f32) {
        let (long, cross) = along(self.axis(), self.viewport.offset.x, self.viewport.offset.y);
        self.set_offset(long + delta, cross);
        self.clamp_view();
    }

    // ---- 命中与手势 ----

    /// 图片在窗口中的物理像素范围 (x0, y0, x1, y1)。
    fn doc_screen(&self) -> (f32, f32, f32, f32) {
        let o = self.viewport.offset;
        let z = self.viewport.zoom;
        (
            o.x,
            o.y,
            o.x + self.document.width() as f32 * z,
            o.y + self.document.height() as f32 * z,
        )
    }
    fn long_at(&self, p: Point) -> f32 {
        let image = self.viewport.image_point(p);
        along(self.axis(), image.x, image.y).0
    }
    fn in_rect(r: Rect, p: Point) -> bool {
        p.x >= r.x as f32
            && p.y >= r.y as f32
            && p.x < (r.x + r.width) as f32
            && p.y < (r.y + r.height) as f32
    }
    fn hit(&self, p: Point) -> Hit {
        if Self::in_rect(self.metrics.panel, p) {
            return Hit::Minimap;
        }
        if !Self::in_rect(self.metrics.main, p) {
            return Hit::Outside;
        }
        let tol = HIT * self.metrics.dpi.max(1.);
        let (x0, y0, x1, y1) = self.doc_screen();
        let inside_x = p.x >= x0 - tol && p.x <= x1 + tol;
        let inside_y = p.y >= y0 - tol && p.y <= y1 + tol;
        if let Some((a, b)) = self.band {
            let (pl, pc) = along(self.axis(), p.x, p.y);
            let (_, (c0, c1)) = along(self.axis(), (x0, x1), (y0, y1));
            let z = self.viewport.zoom;
            let base = self.offset_long();
            if pc >= c0 && pc <= c1 {
                let (sa, sb) = (base + a as f32 * z, base + b as f32 * z);
                if (pl - sb).abs() <= tol {
                    return Hit::BandEdge { start: false };
                }
                if (pl - sa).abs() <= tol {
                    return Hit::BandEdge { start: true };
                }
            }
        }
        let mut best: Option<(f32, Side)> = None;
        for (distance, side, ok) in [
            ((p.x - x0).abs(), Side::Left, inside_y),
            ((p.x - x1).abs(), Side::Right, inside_y),
            ((p.y - y0).abs(), Side::Top, inside_x),
            ((p.y - y1).abs(), Side::Bottom, inside_x),
        ] {
            if ok && distance <= tol && best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, side));
            }
        }
        if let Some((_, side)) = best {
            return Hit::Side(side);
        }
        if p.x >= x0 && p.x < x1 && p.y >= y0 && p.y < y1 {
            Hit::Doc
        } else {
            Hit::Margin
        }
    }
    fn cursor(&self) -> CursorStyle {
        let long = match self.axis() {
            ScrollAxis::Vertical => CursorStyle::ResizeUpDown,
            ScrollAxis::Horizontal => CursorStyle::ResizeLeftRight,
        };
        let side = |s: Side| match s {
            Side::Left | Side::Right => CursorStyle::ResizeLeftRight,
            Side::Top | Side::Bottom => CursorStyle::ResizeUpDown,
        };
        match self.gesture {
            Some(Gesture::Pan { .. }) => return CursorStyle::ClosedHand,
            Some(Gesture::Band { .. } | Gesture::BandEdge { .. }) => return long,
            Some(Gesture::Trim { side: s, .. }) => return side(s),
            Some(Gesture::Minimap { .. }) => return CursorStyle::Arrow,
            None => {}
        }
        if self.space {
            return CursorStyle::OpenHand;
        }
        match self.hover {
            Hit::Margin => CursorStyle::OpenHand,
            Hit::Doc => CursorStyle::Crosshair,
            Hit::Side(s) => side(s),
            Hit::BandEdge { .. } => long,
            Hit::Minimap | Hit::Outside => CursorStyle::Arrow,
        }
    }
    fn minimap(&self) -> Minimap {
        let (doc_long, doc_cross) = self.doc_long_cross();
        let m = self.metrics.mini;
        let (panel_long, panel_cross) = along(self.axis(), m.width as f32, m.height as f32);
        let (start, len) = self.view_range();
        Minimap::new(
            doc_long as f32,
            doc_cross as f32,
            panel_long,
            panel_cross,
            start,
            len,
        )
    }
    fn mini_long(&self, p: Point) -> f32 {
        let m = self.metrics.mini;
        along(self.axis(), p.x - m.x as f32, p.y - m.y as f32).0
    }
    fn begin(&mut self, gesture: Gesture, button: MouseButton, window: &Window) {
        if matches!(gesture, Gesture::Band { .. } | Gesture::Trim { .. }) {
            self.onboarding = false;
        }
        self.gesture = Some(gesture);
        self.gesture_button = Some(button);
        self.status.clear();
        self.notice.clear();
        unsafe {
            SetCapture(hwnd(window));
        }
    }
    fn down(&mut self, p: Point, button: MouseButton, window: &Window, cx: &mut Context<Self>) {
        if self.busy || self.gesture.is_some() {
            return;
        }
        self.metrics = Self::measure(window, self.axis());
        let hit = self.hit(p);
        if button == MouseButton::Middle || self.space || hit == Hit::Margin {
            if matches!(hit, Hit::Outside | Hit::Minimap) {
                return;
            }
            self.begin(
                Gesture::Pan {
                    start: p,
                    offset: self.viewport.offset,
                },
                button,
                window,
            );
            cx.notify();
            return;
        }
        let gesture = match hit {
            Hit::Outside | Hit::Margin => return,
            Hit::Minimap => {
                let map = self.minimap();
                let at = self.mini_long(p);
                let (start, len) = map.indicator;
                let grab = if at >= start && at <= start + len {
                    at - start
                } else {
                    // 点在指示框外：先把视口中心移到这里，再跟手拖动。
                    let (_, view) = self.view_range();
                    self.set_view_start(map.image_at(at) - view / 2.);
                    let map = self.minimap();
                    (at - map.indicator.0).clamp(0., map.indicator.1)
                };
                Gesture::Minimap { grab }
            }
            Hit::BandEdge { start } => {
                let Some((a, b)) = self.band else { return };
                Gesture::BandEdge {
                    fixed: if start { b } else { a },
                }
            }
            Hit::Side(side) => Gesture::Trim {
                side,
                rect: Rect::full(self.document.width(), self.document.height()),
            },
            Hit::Doc => Gesture::Band {
                anchor: self.long_at(p),
                origin: p,
                moved: false,
            },
        };
        self.begin(gesture, button, window);
        cx.notify();
    }
    /// 把当前手势更新到指针 `p`。
    fn drag_to(&mut self, p: Point) {
        let Some(gesture) = self.gesture else { return };
        let (doc_long, _) = self.doc_long_cross();
        match gesture {
            Gesture::Pan { start, offset } => {
                self.viewport.offset = Point {
                    x: offset.x + p.x - start.x,
                    y: offset.y + p.y - start.y,
                };
                self.clamp_view();
            }
            Gesture::Band {
                anchor,
                origin,
                moved,
            } => {
                if !moved && p.distance(origin) < 3. * self.metrics.dpi.max(1.) {
                    return;
                }
                self.gesture = Some(Gesture::Band {
                    anchor,
                    origin,
                    moved: true,
                });
                self.band = model::band(anchor, self.long_at(p), doc_long);
            }
            Gesture::BandEdge { fixed } => {
                self.band = model::band(fixed as f32, self.long_at(p), doc_long);
            }
            Gesture::Trim { side, .. } => {
                let image = self.viewport.image_point(p);
                let at = match side {
                    Side::Left | Side::Right => image.x,
                    Side::Top | Side::Bottom => image.y,
                };
                self.gesture = Some(Gesture::Trim {
                    side,
                    rect: model::trimmed(self.document.width(), self.document.height(), side, at),
                });
            }
            Gesture::Minimap { grab } => {
                let map = self.minimap();
                self.set_view_start(map.view_start_for(self.mini_long(p) - grab));
            }
        }
    }
    fn moved(&mut self, e: &gpui::MouseMoveEvent, w: &Window, cx: &mut Context<Self>) {
        let p = Self::client(e.position, w);
        self.pointer = Some(p);
        if self.gesture.is_some() {
            self.drag_to(p);
            self.ensure_autoscroll(cx);
            cx.notify();
        } else {
            let hover = self.hit(p);
            // 悬停在图上时引导线跟随指针，需要逐次重绘。
            if hover != self.hover || matches!(hover, Hit::Doc | Hit::Side(_)) {
                self.hover = hover;
                cx.notify();
            }
        }
    }
    fn up(&mut self, e: &gpui::MouseUpEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.gesture_button != Some(e.button) {
            return;
        }
        let p = Self::client(e.position, w);
        self.drag_to(p);
        let gesture = self.gesture.take();
        self.gesture_button = None;
        unsafe {
            if GetCapture() == hwnd(w) {
                let _ = ReleaseCapture();
            }
        }
        match gesture {
            // 单击图片（没有拖动）取消已选的删除段。
            Some(Gesture::Band { moved: false, .. }) => self.band = None,
            Some(Gesture::Trim { rect, .. })
                if rect != Rect::full(self.document.width(), self.document.height()) =>
            {
                self.edit(Edit::Crop(rect), w);
            }
            _ => {}
        }
        self.hover = self.hit(p);
        cx.notify();
    }
    fn cancel_pointer(&mut self, window: &Window) {
        // 正在新建或调整的删除段一并放弃；已完成的段保留。
        if matches!(self.gesture, Some(Gesture::Band { moved: true, .. })) {
            self.band = None;
        }
        self.gesture = None;
        self.gesture_button = None;
        unsafe {
            if GetCapture() == hwnd(window) {
                let _ = ReleaseCapture();
            }
        }
    }
    /// 选段或裁开头结尾时指针越过画布两端，按越过距离自动滚动。
    fn autoscroll_speed(&self) -> f32 {
        let Some(p) = self.pointer else { return 0. };
        let along_axis = match self.gesture {
            Some(Gesture::Band { moved: true, .. } | Gesture::BandEdge { .. }) => true,
            Some(Gesture::Trim { side, .. }) => side.is_long(self.axis()),
            _ => false,
        };
        if !along_axis {
            return 0.;
        }
        let dpi = self.metrics.dpi.max(1.);
        let ((start, len), _) = self.main_spans();
        let zone = 16. * dpi;
        let at = along(self.axis(), p.x, p.y).0;
        let over = if at < start + zone {
            at - (start + zone)
        } else if at > start + len - zone {
            at - (start + len - zone)
        } else {
            0.
        };
        (over * 0.35).clamp(-90. * dpi, 90. * dpi)
    }
    fn ensure_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll || self.autoscroll_speed() == 0. {
            return;
        }
        self.autoscroll = true;
        cx.spawn(async move |view, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let running = view
                .update(cx, |v, cx| {
                    let speed = v.autoscroll_speed();
                    if speed == 0. || v.gesture.is_none() {
                        v.autoscroll = false;
                        return false;
                    }
                    v.scroll_long(-speed);
                    if let Some(p) = v.pointer {
                        v.drag_to(p);
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !running {
                break;
            }
        })
        .detach();
    }

    // ---- 编辑与输出 ----

    /// 提交一次删段或裁剪；保持视口起点内容不跳动，并让旧块在新位置过渡显示。
    fn edit(&mut self, edit: Edit, window: &Window) {
        let (start, _) = self.view_range();
        let cross = along(self.axis(), self.viewport.offset.x, self.viewport.offset.y).1;
        let result = match edit {
            Edit::Cut(a, b) => self.document.cut(a, b),
            Edit::Crop(rect) => self.document.crop(rect),
        };
        match result {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        }
        self.capture_ghost(&edit);
        let (start, cross) = match edit {
            Edit::Cut(a, b) => (model::after_cut(start, a, b), cross),
            Edit::Crop(rect) => {
                let (long0, cross0) = along(self.axis(), rect.x, rect.y);
                (
                    start - long0 as f32,
                    cross + cross0 as f32 * self.viewport.zoom,
                )
            }
        };
        self.band = None;
        self.document_changed(window);
        let ((area_start, _), _) = self.main_spans();
        self.set_offset(area_start - start * self.viewport.zoom, cross);
        self.apply_zoom_mode();
        self.notice = match edit {
            Edit::Cut(a, b) => trf!(
                "已删除 {} 像素 · Ctrl+Z 撤销",
                "Removed {} px · Ctrl+Z undoes",
                model::grouped(b - a)
            ),
            Edit::Crop(..) => tr!("已裁剪 · Ctrl+Z 撤销", "Cropped · Ctrl+Z undoes").into(),
        };
    }
    fn capture_ghost(&mut self, edit: &Edit) {
        let mut ghost = Vec::new();
        let axis = self.axis();
        for (key, rect) in &self.plan.main {
            let Some(tile) = self.tiles.get(key) else {
                continue;
            };
            let r = (
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            );
            match *edit {
                Edit::Cut(a, b) => {
                    let ((lo, len), _) = along(axis, (r.0, r.2), (r.1, r.3));
                    let (hi, shift) = (lo + len, (b - a) as f32);
                    for (from, to, delta) in
                        [(lo, hi.min(a as f32), 0.), (lo.max(b as f32), hi, shift)]
                    {
                        if to <= from {
                            continue;
                        }
                        let shift_rect = |start: f32, length: f32| {
                            let ((x, w), (y, h)) = match axis {
                                ScrollAxis::Vertical => ((r.0, r.2), (start, length)),
                                ScrollAxis::Horizontal => ((start, length), (r.1, r.3)),
                            };
                            (x, y, w, h)
                        };
                        ghost.push(GhostPiece {
                            image: tile.image.clone(),
                            rect: shift_rect(lo - delta, len),
                            clip: shift_rect(from - delta, to - from),
                        });
                    }
                }
                Edit::Crop(c) => {
                    let (cx0, cy0) = (c.x as f32, c.y as f32);
                    let x0 = r.0.max(cx0);
                    let y0 = r.1.max(cy0);
                    let x1 = (r.0 + r.2).min(cx0 + c.width as f32);
                    let y1 = (r.1 + r.3).min(cy0 + c.height as f32);
                    if x1 > x0 && y1 > y0 {
                        ghost.push(GhostPiece {
                            image: tile.image.clone(),
                            rect: (r.0 - cx0, r.1 - cy0, r.2, r.3),
                            clip: (x0 - cx0, y0 - cy0, x1 - x0, y1 - y0),
                        });
                    }
                }
            }
        }
        self.ghost = ghost;
    }
    fn document_changed(&mut self, _window: &Window) {
        self.revision += 1;
        self.shared = Arc::new(self.document.snapshot());
        self.failed.clear();
        self.band = self.band.filter(|&(_, b)| b <= self.doc_long_cross().0);
        if self.zoom_mode != ZoomMode::Manual {
            self.apply_zoom_mode();
        } else {
            self.clamp_view();
        }
    }
    fn act(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !matches!(
            action,
            Action::Copy | Action::Save | Action::Pin | Action::Annotate
        ) {
            self.metrics = Self::measure(window, self.axis());
        }
        self.notice.clear();
        match action {
            Action::Delete => {
                if let Some((a, b)) = self.band {
                    self.edit(Edit::Cut(a, b), window);
                }
            }
            Action::Keep => {
                if let Some((a, b)) = self.band {
                    let (x, y) = compose(self.axis(), a, 0);
                    let (_, cross) = self.doc_long_cross();
                    let (width, height) = compose(self.axis(), b - a, cross);
                    self.edit(
                        Edit::Crop(Rect {
                            x,
                            y,
                            width,
                            height,
                        }),
                        window,
                    );
                }
            }
            Action::Cancel => {
                self.cancel_pointer(window);
                self.band = None;
            }
            Action::Undo | Action::Redo | Action::Reset => {
                self.cancel_pointer(window);
                let changed = match action {
                    Action::Undo => self.document.undo(),
                    Action::Redo => self.document.redo(),
                    _ => self.document.reset(),
                };
                if changed {
                    // 撤销／重做后的几何换算未知；内容不变的块按签名直接命中缓存。
                    self.ghost.clear();
                    self.band = None;
                    self.document_changed(window);
                    self.status.clear();
                }
            }
            Action::FitCross | Action::FitAll => {
                self.zoom_mode = if matches!(action, Action::FitAll) {
                    ZoomMode::FitAll
                } else {
                    ZoomMode::FitCross
                };
                self.apply_zoom_mode();
            }
            Action::Actual | Action::ZoomIn | Action::ZoomOut => {
                let m = self.metrics.main;
                let zoom = match action {
                    Action::Actual => 1.,
                    Action::ZoomIn => self.viewport.zoom * 1.25,
                    _ => self.viewport.zoom / 1.25,
                };
                self.zoom_at(
                    zoom,
                    Point {
                        x: (m.x + m.width / 2) as f32,
                        y: (m.y + m.height / 2) as f32,
                    },
                );
            }
            Action::Return => {
                self.cancel_pointer(window);
                self.complete(LongEditorOutcome::Return, window, cx);
                window.remove_window();
            }
            Action::Copy | Action::Save | Action::Pin | Action::Annotate => {
                self.output(action, window, cx)
            }
        }
        cx.notify();
    }
    /// 输出当前长图；未删除的选段只是选择，不影响输出。
    fn output(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pointer(window);
        // 超限时按钮仍可点，点了明确说明原因和做法，而不是置灰后毫无反应。
        if self.exceeds_output_limit() {
            let (w, h) = (self.document.width(), self.document.height());
            let pixels = w as u64 * h as u64;
            let amount = if crate::i18n::is_english() {
                format!("{:.1} MP", pixels as f64 / 1e6)
            } else {
                format!("{} 万像素", model::grouped(pixels.div_ceil(10_000) as u32))
            };
            self.status = trf!(
                "长图 {} × {}（约 {}）超过输出上限 3200 万像素或单边 65535：请拖选删去不需要的部分，或拖动边缘裁剪后再输出",
                "The image is {} × {} (about {}), over the 32 MP / 65535 px output limit: drag to remove parts or trim the edges first",
                model::grouped(w),
                model::grouped(h),
                amount
            );
            cx.notify();
            return;
        }
        self.busy = true;
        self.status.clear();
        let snapshot = self.document.snapshot();
        let native = hwnd(window);
        let task = cx.background_executor().spawn(async move {
            LongDocument::new(snapshot)
                .and_then(|d| d.export())
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, w, cx| {
                let image = match result {
                    Ok(image) => image,
                    Err(e) => {
                        v.busy = false;
                        v.status = e;
                        cx.notify();
                        return;
                    }
                };
                match action {
                    Action::Save => {
                        let owner = w.window_handle().downcast::<Self>().expect("长图编辑窗口");
                        if let Err(e) = crate::save_panel::SavePanel::open(
                            Arc::new(image),
                            None,
                            tr!("保存长截图", "Save long capture"),
                            move |outcome, cx| {
                                let _ = owner.update(cx, |v, w, cx| {
                                    v.busy = false;
                                    match outcome {
                                        crate::save_panel::SaveOutcome::Cancelled => {
                                            w.focus(&v.focus);
                                            let native=hwnd(w).0 as usize;
                                            let mut app=cx.to_async();
                                            cx.foreground_executor().spawn(async move {
                                                let valid=owner.update(&mut app,|_,w,_|hwnd(w).0 as usize==native).unwrap_or(false);
                                                if valid {unsafe{let _=windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(windows::Win32::Foundation::HWND(native as *mut _));}}
                                            }).detach();
                                        }
                                        crate::save_panel::SaveOutcome::Saved {
                                            path,
                                            warnings,
                                        } => {
                                            v.status =
                                                crate::save_workflow::message(&path, &warnings);
                                            v.complete(LongEditorOutcome::Finished, w, cx);
                                            w.remove_window();
                                        }
                                    }
                                    cx.notify();
                                });
                            },
                            cx,
                        ) {
                            v.busy = false;
                            v.status = e.to_string();
                        }
                    }
                    Action::Copy => match crate::export::copy_image(&image, native) {
                        Ok(()) => {
                            cx.background_executor()
                                .spawn(async move {
                                    // 窗口已关闭，无界面可回执；至少记录到日志，不静默吞掉。
                                    if let Err(e) = crate::history::record(&image) {
                                        eprintln!("长图复制后历史保存失败：{e}");
                                    }
                                })
                                .detach();
                            v.complete(LongEditorOutcome::Finished, w, cx);
                            w.remove_window();
                        }
                        Err(e) => {
                            v.busy = false;
                            v.status = e.to_string();
                        }
                    },
                    Action::Pin => {
                        match crate::pin_view::PinView::open_with_history(image, (100, 100), cx) {
                            Ok(()) => {
                                v.complete(LongEditorOutcome::Finished, w, cx);
                                w.remove_window();
                            }
                            Err(e) => {
                                v.busy = false;
                                v.status = e.to_string();
                            }
                        }
                    }
                    Action::Annotate => match crate::image_editor::open(image, None, cx) {
                        Ok(_) => {
                            v.complete(LongEditorOutcome::Finished, w, cx);
                            w.remove_window();
                        }
                        Err(e) => {
                            v.busy = false;
                            v.status = e.to_string();
                        }
                    },
                    _ => v.busy = false,
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn action_enabled(&self, action: Action) -> bool {
        !self.busy
            && match action {
                Action::Undo => self.document.can_undo(),
                Action::Redo => self.document.can_redo(),
                Action::Reset => self.document.can_undo(),
                Action::Delete | Action::Keep | Action::Cancel => self.band.is_some(),
                _ => true,
            }
    }
    fn exceeds_output_limit(&self) -> bool {
        let (w, h) = (self.document.width(), self.document.height());
        w > 65535 || h > 65535 || w as u64 * h as u64 > 32_000_000
    }

    // ---- 分块预览 ----

    fn plan_tiles(
        snapshot: &StitchSnapshot,
        level: u8,
        visible: Rect,
        center: (f32, f32),
        out: &mut Vec<(TileKey, Rect)>,
    ) {
        let (cols, rows) = model::tile_range(level, visible);
        let start = out.len();
        for row in rows {
            for col in cols.clone() {
                if let Some(rect) =
                    model::tile_rect(level, col, row, snapshot.width, snapshot.height)
                {
                    let sig = model::signature(snapshot, rect);
                    out.push((
                        TileKey {
                            level,
                            col,
                            row,
                            sig,
                        },
                        rect,
                    ));
                }
            }
        }
        let distance = |r: &Rect| {
            let dx = r.x as f32 + r.width as f32 / 2. - center.0;
            let dy = r.y as f32 + r.height as f32 / 2. - center.1;
            dx * dx + dy * dy
        };
        out[start..].sort_by(|a, b| distance(&a.1).total_cmp(&distance(&b.1)));
    }
    fn image_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Rect> {
        let (w, h) = (self.document.width() as f32, self.document.height() as f32);
        let (x0, y0) = (x0.floor().clamp(0., w), y0.floor().clamp(0., h));
        let (x1, y1) = (x1.ceil().clamp(0., w), y1.ceil().clamp(0., h));
        (x1 > x0 && y1 > y0).then(|| Rect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }
    /// 计算主画布、总览条和预取需要的块，按距视口中心排序后补渲染缺失块，并淘汰久未使用的块。
    pub(crate) fn refresh_preview(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.completion.is_none() {
            return;
        }
        self.sync_layout(window);
        self.clock += 1;
        let snapshot = self.shared.clone();
        let axis = self.axis();
        let m = self.metrics.main;
        let a = self.viewport.image_point(Point {
            x: m.x as f32,
            y: m.y as f32,
        });
        let b = self.viewport.image_point(Point {
            x: (m.x + m.width) as f32,
            y: (m.y + m.height) as f32,
        });
        let center = ((a.x + b.x) / 2., (a.y + b.y) / 2.);
        let level = model::level_for(self.viewport.zoom);
        let mut plan = Plan::default();
        if let Some(visible) = self.image_rect(a.x, a.y, b.x, b.y) {
            Self::plan_tiles(&snapshot, level, visible, center, &mut plan.main);
        }
        // 总览条：按跨轴宽度缩放，只取面板里可见的那一段。
        let map = self.minimap();
        let mini = self.metrics.mini;
        let (doc_long, doc_cross) = self.doc_long_cross();
        let (mini_long0, mini_cross0) = along(axis, mini.x as f32, mini.y as f32);
        let (mini_long, mini_cross) = along(axis, mini.width as f32, mini.height as f32);
        let (ox, oy) = compose(
            axis,
            mini_long0 - map.scroll,
            mini_cross0 + (mini_cross - doc_cross as f32 * map.zoom).max(0.) / 2.,
        );
        plan.mini_view = Viewport {
            zoom: map.zoom,
            offset: Point { x: ox, y: oy },
        };
        plan.minimap = Some(map);
        let from = map.scroll / map.zoom;
        let to = (map.scroll + mini_long) / map.zoom;
        let (x0, y0) = compose(axis, from, 0.);
        let (x1, y1) = compose(axis, to, doc_cross as f32);
        if let Some(visible) = self.image_rect(x0, y0, x1, y1) {
            let (cl, cc) = ((from + to) / 2., doc_cross as f32 / 2.);
            Self::plan_tiles(
                &snapshot,
                model::level_for(map.zoom),
                visible,
                compose(axis, cl, cc),
                &mut plan.mini,
            );
        }
        // 沿滚动方向前后各预取半屏。
        let mut prefetch = Vec::new();
        let (view_start, view_len) = self.view_range();
        let (c0, c1) = match axis {
            ScrollAxis::Vertical => (a.x, b.x),
            ScrollAxis::Horizontal => (a.y, b.y),
        };
        for (from, to) in [
            (view_start - view_len / 2., view_start),
            (view_start + view_len, view_start + view_len * 1.5),
        ] {
            if to <= 0. || from >= doc_long as f32 {
                continue;
            }
            let (x0, y0) = compose(axis, from, c0);
            let (x1, y1) = compose(axis, to, c1);
            if let Some(visible) = self.image_rect(x0, y0, x1, y1) {
                Self::plan_tiles(&snapshot, level, visible, center, &mut prefetch);
            }
        }
        let mut wanted: Vec<(TileKey, Rect)> = Vec::new();
        let mut seen = HashSet::new();
        for item in plan.main.iter().chain(&plan.mini).chain(&prefetch) {
            if seen.insert(item.0) {
                wanted.push(*item);
            }
        }
        for (key, rect) in &wanted {
            if let Some(tile) = self.tiles.get_mut(key) {
                tile.used = self.clock;
                continue;
            }
            if self.pending.len() >= MAX_INFLIGHT
                || self.pending.contains(key)
                || self.failed.contains(key)
            {
                continue;
            }
            self.spawn_tile(*key, *rect, snapshot.clone(), cx);
        }
        // 主画布的块都齐了，过渡旧块就不再需要。
        if !self.ghost.is_empty()
            && plan
                .main
                .iter()
                .all(|(key, _)| self.tiles.contains_key(key) || self.failed.contains(key))
        {
            self.ghost.clear();
        }
        // 过渡期间旧块可能还在画，暂不淘汰。
        let budget = TILE_BUDGET.max(wanted.len() + 8);
        if self.ghost.is_empty() && self.tiles.len() > budget {
            let mut old: Vec<_> = self
                .tiles
                .iter()
                .filter(|(_, tile)| tile.used != self.clock)
                .map(|(key, tile)| (tile.used, *key))
                .collect();
            old.sort_by_key(|(used, _)| *used);
            for (_, key) in old.into_iter().take(self.tiles.len() - budget) {
                if let Some(tile) = self.tiles.remove(&key) {
                    self.retired.push(tile.image);
                }
            }
        }
        self.plan = plan;
    }
    fn spawn_tile(
        &mut self,
        key: TileKey,
        rect: Rect,
        snapshot: Arc<StitchSnapshot>,
        cx: &mut Context<Self>,
    ) {
        self.pending.insert(key);
        let task = cx.background_executor().spawn(async move {
            let (width, height) = model::tile_output(key.level, rect);
            let image = if key.level == 0 {
                long_document::render_region(&snapshot, rect, width, height)
            } else {
                long_document::render_preview_region(&snapshot, rect, width, height)
            }
            .map_err(|e| e.to_string())?;
            image::RgbaImage::from_raw(image.width, image.height, image.bgra)
                .map(|frame| Arc::new(RenderImage::new(vec![image::Frame::new(frame)])))
                .ok_or_else(|| "长图预览块尺寸不符".to_owned())
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |v, cx| {
                v.pending.remove(&key);
                if v.completion.is_none() {
                    return;
                }
                match result {
                    Ok(image) => {
                        v.preview_created = v.preview_created.saturating_add(1);
                        v.tiles.insert(
                            key,
                            CachedTile {
                                image,
                                used: v.clock,
                            },
                        );
                    }
                    Err(error) => {
                        v.failed.insert(key);
                        v.status = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- 绘制 ----

    /// 原图矩形 → 相对 `origin`（物理像素）的逻辑像素 (左, 上, 宽, 高)，边缘按物理像素取整避免块间缝隙。
    fn screen(
        view: Viewport,
        r: (f32, f32, f32, f32),
        origin: (f32, f32),
        dpi: f32,
    ) -> (f32, f32, f32, f32) {
        let x0 = (view.offset.x + r.0 * view.zoom).round();
        let y0 = (view.offset.y + r.1 * view.zoom).round();
        let x1 = (view.offset.x + (r.0 + r.2) * view.zoom).round();
        let y1 = (view.offset.y + (r.1 + r.3) * view.zoom).round();
        (
            (x0 - origin.0) / dpi,
            (y0 - origin.1) / dpi,
            (x1 - x0) / dpi,
            (y1 - y0) / dpi,
        )
    }
    fn rect_f(r: Rect) -> (f32, f32, f32, f32) {
        (r.x as f32, r.y as f32, r.width as f32, r.height as f32)
    }
    fn tile_image(image: Arc<RenderImage>, at: (f32, f32, f32, f32)) -> AnyElement {
        img(image)
            .absolute()
            .left(px(at.0))
            .top(px(at.1))
            .w(px(at.2))
            .h(px(at.3))
            .object_fit(ObjectFit::Fill)
            .into_any_element()
    }
    /// 画一层块；缺失的块先用已缓存的更粗级别父块顶上。
    fn tile_layer(
        &self,
        tiles: &[(TileKey, Rect)],
        view: Viewport,
        origin: (f32, f32),
        out: &mut Vec<AnyElement>,
    ) {
        let dpi = self.metrics.dpi.max(0.1);
        let mut parents = HashSet::new();
        let mut parent_elements = Vec::new();
        let mut present = Vec::new();
        for (key, rect) in tiles {
            if let Some(tile) = self.tiles.get(key) {
                present.push(Self::tile_image(
                    tile.image.clone(),
                    Self::screen(view, Self::rect_f(*rect), origin, dpi),
                ));
                continue;
            }
            for k in 1..=8u8 {
                let level = key.level.saturating_add(k);
                let (col, row) = (key.col >> k, key.row >> k);
                let Some(rect) =
                    model::tile_rect(level, col, row, self.shared.width, self.shared.height)
                else {
                    break;
                };
                let parent = TileKey {
                    level,
                    col,
                    row,
                    sig: model::signature(&self.shared, rect),
                };
                if let Some(tile) = self.tiles.get(&parent) {
                    if parents.insert(parent) {
                        parent_elements.push(Self::tile_image(
                            tile.image.clone(),
                            Self::screen(view, Self::rect_f(rect), origin, dpi),
                        ));
                    }
                    break;
                }
            }
        }
        out.extend(parent_elements);
        out.extend(present);
    }
    fn ghost_layer(&self, origin: (f32, f32), out: &mut Vec<AnyElement>) {
        let dpi = self.metrics.dpi.max(0.1);
        for piece in &self.ghost {
            let clip = Self::screen(self.viewport, piece.clip, origin, dpi);
            let full = Self::screen(self.viewport, piece.rect, origin, dpi);
            out.push(
                div()
                    .absolute()
                    .left(px(clip.0))
                    .top(px(clip.1))
                    .w(px(clip.2))
                    .h(px(clip.3))
                    .overflow_hidden()
                    .child(Self::tile_image(
                        piece.image.clone(),
                        (full.0 - clip.0, full.1 - clip.1, full.2, full.3),
                    ))
                    .into_any_element(),
            );
        }
    }
    fn block(at: (f32, f32, f32, f32)) -> gpui::Div {
        div()
            .absolute()
            .left(px(at.0))
            .top(px(at.1))
            .w(px(at.2.max(0.)))
            .h(px(at.3.max(0.)))
    }
    fn pill(p: crate::capture_toolbar::Palette, text: String) -> gpui::Div {
        div()
            .px(px(8.))
            .h(px(22.))
            .flex()
            .items_center()
            .rounded(px(11.))
            .bg(rgba(p.hint_bg))
            .border_1()
            .border_color(rgba(p.hint_border))
            .text_color(rgba(p.hint_fg))
            .text_size(px(12.))
            .child(text)
    }
    fn canvas_bg(light: bool) -> u32 {
        if light {
            0xdde2e7
        } else {
            0x14181d
        }
    }
    /// 图片阴影、边框、裁边拖柄、删段与裁边预览、删段浮条。
    fn overlays(&self, cx: &mut Context<Self>, out: &mut Vec<AnyElement>) {
        let p = crate::theme::palette(cx);
        let light = crate::theme::is_light(cx);
        let dpi = self.metrics.dpi.max(0.1);
        let m = self.metrics.main;
        let origin = (m.x as f32, m.y as f32);
        let axis = self.axis();
        let (w, h) = (self.document.width() as f32, self.document.height() as f32);
        let doc = Self::screen(self.viewport, (0., 0., w, h), origin, dpi);
        let (canvas_w, canvas_h) = (m.width as f32 / dpi, m.height as f32 / dpi);
        out.push(
            Self::block(doc)
                .border_1()
                .border_color(rgba(if light { 0x0000_0026 } else { 0xffff_ff1f }))
                .into_any_element(),
        );
        // 裁边预览：被裁掉的部分压暗，新边线高亮。
        if let Some(Gesture::Trim { side, rect }) = self.gesture {
            let removed = match side {
                Side::Left => (0., 0., rect.x as f32, h),
                Side::Right => ((rect.x + rect.width) as f32, 0., w - rect.width as f32, h),
                Side::Top => (0., 0., w, rect.y as f32),
                Side::Bottom => (0., (rect.y + rect.height) as f32, w, h - rect.height as f32),
            };
            let at = Self::screen(self.viewport, removed, origin, dpi);
            let shade = (Self::canvas_bg(light) << 8) | 0xd8;
            out.push(Self::block(at).bg(rgba(shade)).into_any_element());
            let kept = Self::screen(self.viewport, Self::rect_f(rect), origin, dpi);
            out.push(
                Self::block(kept)
                    .border_2()
                    .border_color(rgb(p.accent))
                    .into_any_element(),
            );
            let amount = match side {
                Side::Left | Side::Right => self.document.width() - rect.width,
                Side::Top | Side::Bottom => self.document.height() - rect.height,
            };
            let (lx, ly) = match side {
                Side::Left => (kept.0 + 8., kept.1.max(0.) + 8.),
                Side::Right => (kept.0 + kept.2 - 120., kept.1.max(0.) + 8.),
                Side::Top => (kept.0.max(0.) + 8., kept.1 + 8.),
                Side::Bottom => (kept.0.max(0.) + 8., kept.1 + kept.3 - 30.),
            };
            out.push(
                div()
                    .absolute()
                    .left(px(lx.clamp(4., (canvas_w - 140.).max(4.))))
                    .top(px(ly.clamp(4., (canvas_h - 26.).max(4.))))
                    .child(Self::pill(
                        p,
                        trf!("裁掉 {} 像素", "Trim {} px", model::grouped(amount)),
                    ))
                    .into_any_element(),
            );
        } else {
            // 四边拖柄：画在每条边可见部分的中点，悬停时整条边高亮。
            let clip_x = (doc.0.max(0.), (doc.0 + doc.2).min(canvas_w));
            let clip_y = (doc.1.max(0.), (doc.1 + doc.3).min(canvas_h));
            for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
                let vertical = matches!(side, Side::Left | Side::Right);
                let (line, range) = match side {
                    Side::Left => (doc.0, clip_y),
                    Side::Right => (doc.0 + doc.2, clip_y),
                    Side::Top => (doc.1, clip_x),
                    Side::Bottom => (doc.1 + doc.3, clip_x),
                };
                let limit = if vertical { canvas_w } else { canvas_h };
                if range.1 - range.0 < 24. || line < -4. || line > limit + 4. {
                    continue;
                }
                let hot = self.hover == Hit::Side(side) && self.gesture.is_none();
                if hot {
                    let edge = if vertical {
                        (line - 1., range.0, 2., range.1 - range.0)
                    } else {
                        (range.0, line - 1., range.1 - range.0, 2.)
                    };
                    out.push(Self::block(edge).bg(rgb(p.accent)).into_any_element());
                }
                let mid = (range.0 + range.1) / 2.;
                let grip = if vertical {
                    (line - 3., mid - 18., 6., 36.)
                } else {
                    (mid - 18., line - 3., 36., 6.)
                };
                out.push(
                    Self::block(grip)
                        .rounded(px(3.))
                        .bg(rgb(if hot { p.accent } else { p.background }))
                        .border_1()
                        .border_color(rgb(if hot { p.accent } else { p.border }))
                        .shadow_sm()
                        .into_any_element(),
                );
            }
        }
        self.hover_hint(cx, out);
        // 待删除段：整段跨轴着色，两条边线可拖动调整，旁边浮条给出操作。
        if let Some((a, b)) = self.band {
            let (l, c) = ((a as f32, (b - a) as f32), (0., along(axis, w, h).1));
            let (x, y) = compose(axis, l, c);
            let at = Self::screen(self.viewport, (x.0, y.0, x.1, y.1), origin, dpi);
            out.push(
                Self::block(at)
                    .bg(rgba((p.danger << 8) | 0x30))
                    .into_any_element(),
            );
            for (edge, start) in [(a, true), (b, false)] {
                let hot = self.hover == Hit::BandEdge { start }
                    || matches!(
                        self.gesture,
                        Some(Gesture::Band { .. } | Gesture::BandEdge { .. })
                    );
                let thick = if hot { 3. } else { 2. };
                let line = match axis {
                    ScrollAxis::Vertical => {
                        let y = Self::screen(self.viewport, (0., edge as f32, w, 0.), origin, dpi);
                        (y.0, y.1 - thick / 2., y.2, thick)
                    }
                    ScrollAxis::Horizontal => {
                        let x = Self::screen(self.viewport, (edge as f32, 0., 0., h), origin, dpi);
                        (x.0 - thick / 2., x.1, thick, x.3)
                    }
                };
                out.push(Self::block(line).bg(rgb(p.danger)).into_any_element());
            }
            if !matches!(
                self.gesture,
                Some(Gesture::Band { .. } | Gesture::BandEdge { .. })
            ) {
                out.push(self.band_bar(at, canvas_w, canvas_h, cx));
            } else {
                let label = trf!("{} 像素", "{} px", model::grouped(b - a));
                let (lx, ly) = match axis {
                    ScrollAxis::Vertical => (at.0 + at.2 - 110., at.1 + at.3 + 6.),
                    ScrollAxis::Horizontal => (at.0 + at.2 + 6., at.1 + at.3 - 30.),
                };
                out.push(
                    div()
                        .absolute()
                        .left(px(lx.clamp(4., (canvas_w - 110.).max(4.))))
                        .top(px(ly.clamp(4., (canvas_h - 26.).max(4.))))
                        .child(Self::pill(p, label))
                        .into_any_element(),
                );
            }
        }
    }
    /// 悬停提示：图上显示一条跟随指针的引导线并说明“按住拖动选择要删除的部分”，
    /// 边缘拖柄上说明会裁掉哪一侧；首次打开时画布顶端另有一条操作引导。
    fn hover_hint(&self, cx: &mut Context<Self>, out: &mut Vec<AnyElement>) {
        let p = crate::theme::palette(cx);
        let dpi = self.metrics.dpi.max(0.1);
        let m = self.metrics.main;
        let (canvas_w, canvas_h) = (m.width as f32 / dpi, m.height as f32 / dpi);
        // 错误（如超出输出上限）放在画布顶部醒目处，状态栏同时保留。
        if !self.status.is_empty() && self.gesture.is_none() {
            out.push(
                div()
                    .absolute()
                    .top(px(12.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .max_w(px((canvas_w - 24.).max(120.)))
                            .px(px(14.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(rgb(p.background))
                            .border_1()
                            .border_color(rgb(p.danger))
                            .shadow_md()
                            .text_color(rgb(p.danger))
                            .text_size(px(13.))
                            .child(self.status.clone()),
                    )
                    .into_any_element(),
            );
        } else if self.onboarding && self.band.is_none() && self.gesture.is_none() {
            let text = match self.axis() {
                ScrollAxis::Vertical => tr!(
                    "① 在图上上下拖动，选中不要的部分后删除　② 拖动图片四边的把手裁边　③ 右侧缩略图点击或拖动快速定位",
                    "① Drag up/down on the image to select a part, then delete it　② Drag the edge handles to trim　③ Click or drag the overview on the right to jump"
                ),
                ScrollAxis::Horizontal => tr!(
                    "① 在图上左右拖动，选中不要的部分后删除　② 拖动图片四边的把手裁边　③ 下方缩略图点击或拖动快速定位",
                    "① Drag left/right on the image to select a part, then delete it　② Drag the edge handles to trim　③ Click or drag the overview below to jump"
                ),
            };
            out.push(
                div()
                    .absolute()
                    .top(px(12.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .max_w(px((canvas_w - 24.).max(120.)))
                            .px(px(14.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(rgba(p.hint_bg))
                            .border_1()
                            .border_color(rgba(p.hint_border))
                            .shadow_md()
                            .text_color(rgba(p.hint_fg))
                            .text_size(px(13.))
                            .child(text),
                    )
                    .into_any_element(),
            );
        }
        if self.gesture.is_some() || self.space || self.busy {
            return;
        }
        let Some(pointer) = self.pointer else { return };
        let (px_l, py_l) = (
            (pointer.x - m.x as f32) / dpi,
            (pointer.y - m.y as f32) / dpi,
        );
        let label = match self.hover {
            Hit::Doc => {
                let (w, h) = (self.document.width() as f32, self.document.height() as f32);
                let doc =
                    Self::screen(self.viewport, (0., 0., w, h), (m.x as f32, m.y as f32), dpi);
                let line = match self.axis() {
                    ScrollAxis::Vertical => (doc.0, py_l, doc.2, 1.),
                    ScrollAxis::Horizontal => (px_l, doc.1, 1., doc.3),
                };
                out.push(
                    Self::block(line)
                        .bg(rgba((p.danger << 8) | 0x99))
                        .into_any_element(),
                );
                if self.band.is_some() {
                    tr!(
                        "按住拖动重新选择；单击取消选择",
                        "Drag to reselect · click to clear"
                    )
                } else {
                    tr!(
                        "按住拖动，选择要删除的部分",
                        "Drag to select a part to remove"
                    )
                }
            }
            Hit::Side(Side::Top) => tr!("拖动裁掉顶部", "Drag to trim the top"),
            Hit::Side(Side::Bottom) => tr!("拖动裁掉底部", "Drag to trim the bottom"),
            Hit::Side(Side::Left) => tr!("拖动裁掉左侧", "Drag to trim the left side"),
            Hit::Side(Side::Right) => tr!("拖动裁掉右侧", "Drag to trim the right side"),
            Hit::BandEdge { .. } => tr!("拖动调整删除范围", "Drag to adjust the selection"),
            _ => return,
        };
        let width = 230.;
        out.push(
            div()
                .absolute()
                .left(px((px_l + 14.).min(canvas_w - width).max(4.)))
                .top(px((py_l + 16.).min(canvas_h - 28.).max(4.)))
                .child(Self::pill(p, label.into()))
                .into_any_element(),
        );
    }
    /// 删段浮条：放在段尾外侧，放不下就放到段首外侧，再限在画布内。
    fn band_bar(
        &self,
        band: (f32, f32, f32, f32),
        canvas_w: f32,
        canvas_h: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = crate::theme::palette(cx);
        let (x, y) = match self.axis() {
            ScrollAxis::Vertical => {
                let below = band.1 + band.3 + 8.;
                let y = if below + BAR_H <= canvas_h - 8. {
                    below
                } else {
                    band.1 - 8. - BAR_H
                };
                ((band.0 + band.2).min(canvas_w) - BAR_W - 12., y)
            }
            ScrollAxis::Horizontal => {
                let after = band.0 + band.2 + 8.;
                let x = if after + BAR_W <= canvas_w - 8. {
                    after
                } else {
                    band.0 - 8. - BAR_W
                };
                (x, (band.1 + band.3).min(canvas_h) - BAR_H - 12.)
            }
        };
        let (x, y) = (
            x.clamp(8., (canvas_w - BAR_W - 8.).max(8.)),
            y.clamp(8., (canvas_h - BAR_H - 8.).max(8.)),
        );
        let (a, b) = self.band.unwrap_or((0, 0));
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(BAR_W))
            .h(px(BAR_H))
            .px(px(4.))
            .flex()
            .items_center()
            .gap(px(2.))
            .rounded(px(8.))
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(6.))
                    .truncate()
                    .text_color(rgb(p.muted))
                    .child(trf!("已选 {} 像素", "{} px", model::grouped(b - a))),
            )
            .child(self.text_button(
                "long-edit-delete",
                tr!("删除", "Delete"),
                Some(Icon::Trash),
                tr!(
                    "删除选中的一段 · Delete / Enter",
                    "Remove the selected part · Delete / Enter"
                ),
                Action::Delete,
                Tone::Danger,
                cx,
            ))
            .child(self.text_button(
                "long-edit-keep",
                tr!("仅保留", "Keep"),
                None,
                tr!(
                    "只保留选中的一段，裁掉其余部分",
                    "Keep only the selected part and trim the rest"
                ),
                Action::Keep,
                Tone::Normal,
                cx,
            ))
            .child(self.icon_button(
                "long-edit-cancel",
                tr!("取消选择 · Esc", "Clear selection · Esc"),
                Icon::Close,
                Action::Cancel,
                cx,
            ))
            .into_any_element()
    }
    fn minimap_layer(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        let light = crate::theme::is_light(cx);
        let dpi = self.metrics.dpi.max(0.1);
        let panel = self.metrics.panel;
        let origin = (panel.x as f32, panel.y as f32);
        let mut children = Vec::new();
        self.tile_layer(&self.plan.mini, self.plan.mini_view, origin, &mut children);
        let view = self.plan.mini_view;
        let axis = self.axis();
        let (w, h) = (self.document.width() as f32, self.document.height() as f32);
        let doc = Self::screen(view, (0., 0., w, h), origin, dpi);
        children.push(
            Self::block(doc)
                .border_1()
                .border_color(rgba(if light { 0x0000_0026 } else { 0xffff_ff1f }))
                .into_any_element(),
        );
        let doc_cross = along(axis, w, h).1;
        if let Some((a, b)) = self.band {
            let (x, y) = compose(axis, (a as f32, (b - a) as f32), (0., doc_cross));
            let at = Self::screen(view, (x.0, y.0, x.1, y.1), origin, dpi);
            children.push(
                Self::block(at)
                    .bg(rgba((p.danger << 8) | 0x66))
                    .into_any_element(),
            );
        }
        if self.plan.minimap.is_some() {
            let (start, len) = self.view_range();
            let start = start.clamp(0., along(axis, w, h).0);
            let len = len.min(along(axis, w, h).0 - start);
            let (x, y) = compose(axis, (start, len.max(0.)), (0., doc_cross));
            let at = Self::screen(view, (x.0, y.0, x.1, y.1), origin, dpi);
            children.push(
                Self::block((at.0 - 2., at.1 - 2., at.2 + 4., at.3 + 4.))
                    .rounded(px(3.))
                    .border_2()
                    .border_color(rgb(p.accent))
                    .bg(rgba((p.accent << 8) | 0x1c))
                    .into_any_element(),
            );
        }
        let base = div()
            .absolute()
            .overflow_hidden()
            .bg(rgb(p.track))
            .children(children);
        match axis {
            ScrollAxis::Vertical => base
                .top(px(TOP))
                .bottom(px(STATUS))
                .right_0()
                .w(px(SIDE))
                .border_l_1()
                .border_color(rgb(p.border)),
            ScrollAxis::Horizontal => base
                .left_0()
                .right_0()
                .bottom(px(STATUS))
                .h(px(SIDE))
                .border_t_1()
                .border_color(rgb(p.border)),
        }
    }
    fn icon_button(
        &self,
        id: &'static str,
        hint: &'static str,
        icon: Icon,
        action: Action,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let enabled = self.action_enabled(action);
        div()
            .id(id)
            .relative()
            .size(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.35 })
            .when(enabled, |el| {
                el.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
            })
            .child(toolbar_icons::action(icon, p.foreground).size(px(18.)))
            .child(crate::save_panel::probe(
                self.ui_positions.clone(),
                id.into(),
            ))
            .tooltip(move |_, cx| cx.new(|_| LongEditorHint(hint.into())).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.act(action, w, cx)
                }
            }))
    }
    #[allow(clippy::too_many_arguments)]
    fn text_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: Option<Icon>,
        hint: &'static str,
        action: Action,
        tone: Tone,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let enabled = self.action_enabled(action);
        let (bg, fg, hover) = match tone {
            Tone::Primary => (Some(p.accent), p.on_accent, rgb(p.accent_hover)),
            Tone::Danger => (None, p.danger, rgba(p.danger_bg)),
            Tone::Selected => (Some(p.selected), p.accent, rgb(p.selected)),
            Tone::Normal => (None, p.foreground, rgb(p.hover)),
        };
        div()
            .id(id)
            .relative()
            .h(px(30.))
            .px(px(10.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .rounded_md()
            .when_some(bg, |el, bg| el.bg(rgb(bg)))
            .text_color(rgb(fg))
            .opacity(if enabled { 1. } else { 0.35 })
            .when(enabled, |el| {
                el.cursor_pointer().hover(move |s| s.bg(hover))
            })
            .when_some(icon, |el, icon| {
                el.child(toolbar_icons::action(icon, fg).size(px(16.)))
            })
            .child(label)
            .child(crate::save_panel::probe(
                self.ui_positions.clone(),
                id.into(),
            ))
            .tooltip(move |_, cx| cx.new(|_| LongEditorHint(hint.into())).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.act(action, w, cx)
                }
            }))
    }
    fn separator(cx: &App) -> gpui::Div {
        div()
            .w(px(1.))
            .h(px(18.))
            .mx(px(4.))
            .flex_shrink_0()
            .bg(rgb(crate::theme::palette(cx).border))
    }
    fn hint_text(&self) -> (String, bool) {
        if self.busy {
            return (
                tr!("正在生成长图…", "Building the long image…").into(),
                false,
            );
        }
        if !self.status.is_empty() {
            return (self.status.clone(), true);
        }
        if !self.notice.is_empty() && self.gesture.is_none() {
            return (self.notice.clone(), false);
        }
        let text = match (self.gesture, self.band) {
            (Some(Gesture::Trim { .. }), _) => tr!(
                "松开鼠标裁掉压暗部分 · Esc 放弃",
                "Release to trim the shaded part · Esc cancels"
            ),
            (Some(Gesture::Band { .. } | Gesture::BandEdge { .. }), _) => tr!(
                "拖到画布边缘可自动滚动 · Esc 放弃",
                "Drag past the edge to auto-scroll · Esc cancels"
            ),
            (_, Some(_)) => tr!(
                "Delete 或 Enter 删除选中段 · 拖动红线调整 · 单击图片或 Esc 取消",
                "Delete or Enter removes it · drag the red lines to adjust · click or Esc clears"
            ),
            _ => match self.axis() {
                ScrollAxis::Vertical => tr!(
                    "上下拖动选择要删除的部分 · 拖动图片边缘裁掉开头、结尾或两侧 · 滚轮浏览 · Ctrl+滚轮缩放 · 中键或空格拖动平移",
                    "Drag up/down to select a part to remove · drag the image edges to trim · wheel scrolls · Ctrl+wheel zooms · middle-drag or Space pans"
                ),
                ScrollAxis::Horizontal => tr!(
                    "左右拖动选择要删除的部分 · 拖动图片边缘裁掉开头、结尾或上下 · 滚轮浏览 · Ctrl+滚轮缩放 · 中键或空格拖动平移",
                    "Drag left/right to select a part to remove · drag the image edges to trim · wheel scrolls · Ctrl+wheel zooms · middle-drag or Space pans"
                ),
            },
        };
        (text.into(), false)
    }
    fn top_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TOP))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(2.))
            .bg(rgb(p.background))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(self.icon_button(
                "long-edit-undo",
                tr!("撤销 · Ctrl+Z", "Undo · Ctrl+Z"),
                Icon::Undo,
                Action::Undo,
                cx,
            ))
            .child(self.icon_button(
                "long-edit-redo",
                tr!(
                    "重做 · Ctrl+Y / Ctrl+Shift+Z",
                    "Redo · Ctrl+Y / Ctrl+Shift+Z"
                ),
                Icon::Redo,
                Action::Redo,
                cx,
            ))
            .child(self.text_button(
                "long-edit-reset",
                tr!("还原", "Revert"),
                None,
                tr!(
                    "还原为刚采集的长图 · 可撤销",
                    "Revert to the captured long image · undoable"
                ),
                Action::Reset,
                Tone::Normal,
                cx,
            ))
            .child(Self::separator(cx))
            .child(self.text_button(
                "long-edit-return",
                tr!("继续采集", "Keep capturing"),
                Some(Icon::LongCapture),
                tr!(
                    "回到长截图继续滚动采集 · Esc",
                    "Go back and keep scroll-capturing · Esc"
                ),
                Action::Return,
                Tone::Normal,
                cx,
            ))
            .child(div().flex_1())
            .child(self.text_button(
                "long-edit-annotate",
                tr!("标注", "Annotate"),
                Some(Icon::Pen),
                tr!("在图片编辑器中标注长图", "Annotate in the image editor"),
                Action::Annotate,
                Tone::Normal,
                cx,
            ))
            .child(self.text_button(
                "long-edit-pin",
                tr!("贴图", "Pin"),
                Some(Icon::Pin),
                tr!("贴到屏幕上", "Pin to the screen"),
                Action::Pin,
                Tone::Normal,
                cx,
            ))
            .child(self.text_button(
                "long-edit-save",
                tr!("保存", "Save"),
                Some(Icon::Save),
                tr!("保存 · Ctrl+S", "Save · Ctrl+S"),
                Action::Save,
                Tone::Normal,
                cx,
            ))
            .child(div().w(px(4.)))
            .child(self.text_button(
                "long-edit-copy",
                tr!("复制", "Copy"),
                Some(Icon::Copy),
                tr!("复制长图并关闭 · Ctrl+C", "Copy and close · Ctrl+C"),
                Action::Copy,
                Tone::Primary,
                cx,
            ))
    }
    fn status_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::palette(cx);
        let (hint, error) = self.hint_text();
        let over = self.exceeds_output_limit();
        let mode = |m: ZoomMode| {
            if self.zoom_mode == m {
                Tone::Selected
            } else {
                Tone::Normal
            }
        };
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(STATUS))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(2.))
            .bg(rgb(p.background))
            .border_t_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(if error { p.danger } else { p.muted }))
                    .child(hint),
            )
            .child(
                div()
                    .id("long-edit-size")
                    .flex_shrink_0()
                    .px(px(8.))
                    .text_color(rgb(if over { p.danger } else { p.muted }))
                    .child(format!(
                        "{} × {}",
                        model::grouped(self.document.width()),
                        model::grouped(self.document.height())
                    ))
                    .when(over, |el| {
                        el.tooltip(|_, cx| {
                            cx.new(|_| {
                                LongEditorHint(
                                    tr!(
                                        "超过 65535 单边或 3200 万像素，裁剪或删段后才能输出",
                                        "Over 65535 px per side or 32 MP; trim or remove parts before exporting"
                                    )
                                    .into(),
                                )
                            })
                            .into()
                        })
                    }),
            )
            .child(Self::separator(cx))
            .child(self.text_button(
                "long-edit-cross",
                match self.axis() {
                    ScrollAxis::Vertical => tr!("适合宽度", "Fit width"),
                    ScrollAxis::Horizontal => tr!("适合高度", "Fit height"),
                },
                None,
                tr!("按窗口宽度（横向长图按高度）显示 · Ctrl+0", "Fit the cross direction · Ctrl+0"),
                Action::FitCross,
                mode(ZoomMode::FitCross),
                cx,
            ))
            .child(self.text_button(
                "long-edit-all",
                tr!("全图", "Whole"),
                None,
                tr!("缩小到能看到整张长图", "Zoom out to see the whole image"),
                Action::FitAll,
                mode(ZoomMode::FitAll),
                cx,
            ))
            .child(self.text_button(
                "long-edit-actual",
                "1:1",
                None,
                tr!("原始大小 · Ctrl+1", "Actual size · Ctrl+1"),
                Action::Actual,
                Tone::Normal,
                cx,
            ))
            .child(Self::separator(cx))
            .child(self.text_button(
                "long-edit-minus",
                "−",
                None,
                tr!("缩小 · Ctrl+滚轮 / Ctrl+-", "Zoom out · Ctrl+wheel / Ctrl+-"),
                Action::ZoomOut,
                Tone::Normal,
                cx,
            ))
            .child(
                div()
                    .w(px(48.))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .text_color(rgb(p.foreground))
                    .child(format!("{:.0}%", self.viewport.zoom * 100.)),
            )
            .child(self.text_button(
                "long-edit-plus",
                "+",
                None,
                tr!("放大 · Ctrl+滚轮 / Ctrl+=", "Zoom in · Ctrl+wheel / Ctrl+="),
                Action::ZoomIn,
                Tone::Normal,
                cx,
            ))
    }
    fn key(&mut self, e: &gpui::KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        let m = e.keystroke.modifiers;
        let plain = !m.control && !m.alt && !m.shift && !m.platform && !m.function;
        let ctrl = m.control && !m.alt && !m.platform && !m.function;
        let only_ctrl = ctrl && !m.shift;
        let key = e.keystroke.key.as_str();
        if key == "space" && plain {
            if !self.space {
                self.space = true;
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        let step = 60. * w.scale_factor();
        let page = along(
            self.axis(),
            self.metrics.main.width as f32,
            self.metrics.main.height as f32,
        )
        .0 * 0.9;
        let scroll = match key {
            "up" | "left" if plain => Some(step),
            "down" | "right" if plain => Some(-step),
            "pageup" if plain => Some(page),
            "pagedown" if plain => Some(-page),
            _ => None,
        };
        if let Some(delta) = scroll {
            self.scroll_long(delta);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if plain && matches!(key, "home" | "end") {
            let (doc_long, _) = self.doc_long_cross();
            self.set_view_start(if key == "home" {
                -1e9
            } else {
                doc_long as f32 + 1e9
            });
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let band = self.band.is_some();
        let action = match key {
            "escape" if plain && (self.gesture.is_some() || band) => Some(Action::Cancel),
            "escape" if plain => Some(Action::Return),
            "enter" | "delete" | "backspace" if plain && band => Some(Action::Delete),
            "z" if ctrl => Some(if m.shift { Action::Redo } else { Action::Undo }),
            "y" if only_ctrl => Some(Action::Redo),
            "s" if only_ctrl => Some(Action::Save),
            "c" if only_ctrl => Some(Action::Copy),
            "0" if only_ctrl => Some(Action::FitCross),
            "1" if only_ctrl => Some(Action::Actual),
            "=" | "+" if ctrl => Some(Action::ZoomIn),
            "-" if only_ctrl => Some(Action::ZoomOut),
            _ => None,
        };
        // 长按只消费，不重复触发（否则按住 Esc 会先取消选段再关闭编辑器）。
        let held_blocked = e.is_held
            && matches!(
                action,
                Some(
                    Action::Cancel | Action::Return | Action::Delete | Action::Save | Action::Copy
                )
            );
        if held_blocked {
            cx.stop_propagation();
            return;
        }
        if let Some(action) = action {
            if self.action_enabled(action) || matches!(action, Action::Return) {
                self.act(action, w, cx);
            }
            cx.stop_propagation();
        }
    }
}
#[derive(Clone, Copy)]
enum Tone {
    Normal,
    Primary,
    Danger,
    Selected,
}
impl Render for LongEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let started = std::time::Instant::now();
        crate::theme::sync_window(window, cx);
        if self.gesture_button.is_some() && unsafe { GetCapture() != hwnd(window) } {
            self.cancel_pointer(window);
        }
        self.ui_positions.borrow_mut().clear();
        // 上一帧淘汰的块此时已不在待提交场景里，先释放再规划本帧。
        self.release_retired(window);
        self.refresh_preview(window, cx);
        let p = crate::theme::palette(cx);
        let light = crate::theme::is_light(cx);
        let weak = cx.entity().downgrade();
        let capture_events = gpui::canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let moved = weak.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                    if phase == gpui::DispatchPhase::Capture && unsafe { GetCapture() == hwnd(w) } {
                        let _ = moved.update(cx, |v, cx| {
                            if v.gesture_button.is_some() {
                                v.moved(e, w, cx);
                                cx.stop_propagation();
                            }
                        });
                    }
                });
                let up = weak.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, w, cx| {
                    if phase == gpui::DispatchPhase::Capture && unsafe { GetCapture() == hwnd(w) } {
                        let _ = up.update(cx, |v, cx| {
                            if v.gesture_button == Some(e.button) {
                                v.up(e, w, cx);
                                cx.stop_propagation();
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let m = self.metrics.main;
        let origin = (m.x as f32, m.y as f32);
        let mut layers = Vec::new();
        let dpi = self.metrics.dpi.max(0.1);
        let (w, h) = (self.document.width() as f32, self.document.height() as f32);
        let doc = Self::screen(self.viewport, (0., 0., w, h), origin, dpi);
        layers.push(
            Self::block(doc)
                .bg(rgb(if light { 0xffffff } else { 0x000000 }))
                .shadow_lg()
                .into_any_element(),
        );
        self.ghost_layer(origin, &mut layers);
        self.tile_layer(&self.plan.main, self.viewport, origin, &mut layers);
        self.overlays(cx, &mut layers);
        let canvas = div()
            .absolute()
            .top(px(TOP))
            .left_0()
            .w(px(m.width as f32 / dpi))
            .h(px(m.height as f32 / dpi))
            .overflow_hidden()
            .bg(rgb(Self::canvas_bg(light)))
            .cursor(self.cursor())
            .children(layers);
        let root = div()
            .relative()
            .size_full()
            .bg(rgb(p.background))
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .font_family("Microsoft YaHei UI")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_key_up(cx.listener(|v, e: &gpui::KeyUpEvent, _, cx| {
                if e.keystroke.key == "space" && v.space {
                    v.space = false;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    w.focus(&v.focus);
                    v.down(Self::client(e.position, w), MouseButton::Left, w, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    v.down(Self::client(e.position, w), MouseButton::Middle, w, cx);
                }),
            )
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, w, cx| {
                v.moved(e, w, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseUpEvent, w, cx| v.up(e, w, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|v, e: &gpui::MouseUpEvent, w, cx| v.up(e, w, cx)),
            )
            .on_scroll_wheel(cx.listener(|v, e: &gpui::ScrollWheelEvent, w, cx| {
                if v.busy {
                    return;
                }
                let dpi = w.scale_factor();
                let (x, y) = match e.delta {
                    ScrollDelta::Lines(p) => (p.x * 48. * dpi, p.y * 48. * dpi),
                    ScrollDelta::Pixels(p) => (f32::from(p.x) * dpi, f32::from(p.y) * dpi),
                };
                if e.modifiers.control {
                    v.zoom_at(
                        v.viewport.zoom * (y / dpi / 400.).exp(),
                        Self::client(e.position, w),
                    );
                } else {
                    // 竖向滚轮沿截图方向滚动；Shift+滚轮滚动另一方向，横向滚轮照常横向。
                    let (dx, dy) = match (v.axis(), e.modifiers.shift) {
                        (ScrollAxis::Vertical, false) => (x, y),
                        (ScrollAxis::Vertical, true) => (x + y, 0.),
                        (ScrollAxis::Horizontal, false) => (x + y, 0.),
                        (ScrollAxis::Horizontal, true) => (0., x + y),
                    };
                    v.viewport.offset.x += dx;
                    v.viewport.offset.y += dy;
                    v.clamp_view();
                }
                if let Some(p) = v.pointer {
                    if v.gesture.is_some() {
                        v.drag_to(p);
                    }
                }
                cx.notify();
                cx.stop_propagation();
            }))
            .child(canvas)
            .child(self.minimap_layer(cx))
            .child(capture_events)
            .child(self.top_bar(cx))
            .child(self.status_bar(cx));
        if self.render_ms.len() >= 512 {
            self.render_ms.drain(..256);
        }
        self.render_ms.push(started.elapsed().as_secs_f32() * 1000.);
        root
    }
}
