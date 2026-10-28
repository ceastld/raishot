use crate::{
    annotation::{Bounds as ShapeBounds, Tool},
    capture_ocr_result_click::{self, ResultInputLeaseHandler, ResultIntent},
    capture_view::CaptureView,
    scene::Layer,
};
use gpui::{
    canvas, div, fill, img, point, prelude::*, px, rgb, rgba, size, Bounds, Context, MouseButton,
    ObjectFit, PathBuilder, Render, Window,
};
use std::{rc::Rc, sync::Arc};

/// 非空Scene网格不能因路径准备失败悄悄消失；renderer仍核验实际GPU绘制。
fn validated_content_path(
    path: Option<gpui::Path<gpui::Pixels>>,
    mesh: &crate::vector_mesh::VectorMesh,
    complete: &mut bool,
) -> Option<gpui::Path<gpui::Pixels>> {
    if !mesh.vertices.is_empty() && path.is_none() {
        *complete = false;
    }
    path
}

const MAGNIFIER_CELL: f32 = 12.;
const MAGNIFIER_COLUMNS: i32 = 15;
const MAGNIFIER_ROWS: i32 = 11;
const MAGNIFIER_GRID_WIDTH: f32 = MAGNIFIER_CELL * MAGNIFIER_COLUMNS as f32;
const MAGNIFIER_GRID_HEIGHT: f32 = MAGNIFIER_CELL * MAGNIFIER_ROWS as f32;
const MAGNIFIER_PADDING: f32 = 8.;
const MAGNIFIER_CARD_WIDTH: f32 = MAGNIFIER_GRID_WIDTH + MAGNIFIER_PADDING * 2. + 2.;
/// 说明各一行时的卡片高度；多出的行由 `wrapped_lines` 估算后追加。
const MAGNIFIER_CARD_HEIGHT: f32 = MAGNIFIER_GRID_HEIGHT + 112.;

/// 11px 说明文字的行数估算：ASCII 约 6px、中文约 11px。
fn wrapped_lines(text: &str, width: f32) -> f32 {
    let advance: f32 = text
        .chars()
        .map(|c| if c.is_ascii() { 6. } else { 11. })
        .sum();
    (advance / width).ceil().max(1.)
}

/// 以取色中心为正中逐格绘制冻结小片：片外为暗底，十字行列淡蓝高亮，中心格黑白双框。
fn magnifier_grid(
    pixels: std::sync::Arc<crate::capture::Image>,
    center: (u32, u32),
) -> gpui::Canvas<()> {
    canvas(
        |_, _, _| (),
        move |bounds, _, w, _| {
            let half = (MAGNIFIER_COLUMNS / 2, MAGNIFIER_ROWS / 2);
            let cell = |gx: i32, gy: i32| {
                Bounds::new(
                    bounds.origin
                        + point(
                            px(gx as f32 * MAGNIFIER_CELL),
                            px(gy as f32 * MAGNIFIER_CELL),
                        ),
                    size(px(MAGNIFIER_CELL), px(MAGNIFIER_CELL)),
                )
            };
            let grid = Bounds::new(
                bounds.origin,
                size(px(MAGNIFIER_GRID_WIDTH), px(MAGNIFIER_GRID_HEIGHT)),
            );
            w.paint_quad(fill(grid, rgb(0x16191e)));
            for gy in 0..MAGNIFIER_ROWS {
                for gx in 0..MAGNIFIER_COLUMNS {
                    let sx = i64::from(center.0) + i64::from(gx - half.0);
                    let sy = i64::from(center.1) + i64::from(gy - half.1);
                    if sx < 0
                        || sy < 0
                        || sx >= i64::from(pixels.width)
                        || sy >= i64::from(pixels.height)
                    {
                        continue;
                    }
                    let i = (sy as usize * pixels.width as usize + sx as usize) * 4;
                    let Some(p) = pixels.bgra.get(i..i + 4) else {
                        continue;
                    };
                    w.paint_quad(fill(
                        cell(gx, gy),
                        rgb(u32::from(p[2]) << 16 | u32::from(p[1]) << 8 | u32::from(p[0])),
                    ));
                }
            }
            for gx in 1..MAGNIFIER_COLUMNS {
                w.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(gx as f32 * MAGNIFIER_CELL), px(0.)),
                        size(px(1.), px(MAGNIFIER_GRID_HEIGHT)),
                    ),
                    rgba(0x80808026),
                ));
            }
            for gy in 1..MAGNIFIER_ROWS {
                w.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(0.), px(gy as f32 * MAGNIFIER_CELL)),
                        size(px(MAGNIFIER_GRID_WIDTH), px(1.)),
                    ),
                    rgba(0x80808026),
                ));
            }
            let tint = rgba(0x6c9ffc40);
            for gx in (0..MAGNIFIER_COLUMNS).filter(|gx| *gx != half.0) {
                w.paint_quad(fill(cell(gx, half.1), tint));
            }
            for gy in (0..MAGNIFIER_ROWS).filter(|gy| *gy != half.1) {
                w.paint_quad(fill(cell(half.0, gy), tint));
            }
            let target = cell(half.0, half.1);
            w.paint_quad(gpui::outline(
                Bounds::new(
                    target.origin - point(px(1.), px(1.)),
                    target.size + size(px(2.), px(2.)),
                ),
                rgb(0x000000),
                gpui::BorderStyle::Solid,
            ));
            w.paint_quad(gpui::outline(
                target,
                rgb(0xffffff),
                gpui::BorderStyle::Solid,
            ));
        },
    )
    .size_full()
}

fn outline(b: ShapeBounds, scale: f32, color: u32) -> gpui::Div {
    div()
        .absolute()
        .left(px(b.x / scale))
        .top(px(b.y / scale))
        .w(px((b.width / scale).max(1.)))
        .h(px((b.height / scale).max(1.)))
        .border_1()
        .border_color(rgb(color))
}
fn annotation_outline(a: &crate::annotation::Annotation, scale: f32) -> gpui::AnyElement {
    if matches!(a.tool, Tool::Mosaic | Tool::Blur) && a.supports_radius() && a.corner_radius() > 0.
    {
        let segments = a.segments();
        return canvas(
            |_, _, _| (),
            move |bounds, _, w, _| {
                if let Some((first, _)) = segments.first() {
                    let mut path = PathBuilder::stroke(px(1.));
                    path.move_to(bounds.origin + point(px(first.x / scale), px(first.y / scale)));
                    for (_, end) in &segments {
                        path.line_to(bounds.origin + point(px(end.x / scale), px(end.y / scale)));
                    }
                    path.close();
                    if let Ok(path) = path.build() {
                        w.paint_path(path, rgb(0x6c9ffc));
                    }
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();
    }
    if a.rotation == 0. {
        return outline(a.bounds(), scale, 0x6c9ffc).into_any_element();
    }
    let t = a.rotation_transform();
    let corners = crate::transform::corners(a.local_bounds()).map(|p| t.map(p));
    canvas(
        |_, _, _| (),
        move |bounds, _, w, _| {
            let mut path = PathBuilder::stroke(px(1.));
            path.move_to(bounds.origin + point(px(corners[0].x / scale), px(corners[0].y / scale)));
            for p in &corners[1..] {
                path.line_to(bounds.origin + point(px(p.x / scale), px(p.y / scale)));
            }
            path.close();
            if let Ok(path) = path.build() {
                w.paint_path(path, rgb(0x6c9ffc));
            }
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}
fn handle(x: f32, y: f32, color: u32) -> gpui::Div {
    div()
        .absolute()
        .left(px(x - 3.))
        .top(px(y - 3.))
        .w(px(6.))
        .h(px(6.))
        .bg(rgb(color))
}
/// 选区／自动选区外框。`radius`（选区像素）大于 0 时外框沿圆角走，
/// `mask_corners` 再压暗圆角外的四个小角，让框内正好是导出的圆角图；
/// `frame` 只控制轮廓与圆弧高亮，不影响缩放控制点和圆角遮罩。
fn crop_chrome(
    rect: crate::geometry::Rect,
    ui_scale: f32,
    viewport: crate::viewport::Viewport,
    frame: bool,
    handles: bool,
    radius: f32,
    mask_corners: bool,
    highlight: bool,
) -> impl IntoElement {
    let chrome = crate::selection_chrome::SelectionChrome::projected(
        rect,
        f64::from(viewport.zoom),
        (f64::from(viewport.offset.x), f64::from(viewport.offset.y)),
    );
    canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            let mut paint = |r: crate::selection_chrome::PixelBox, color| {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(r.left as f32 / ui_scale), px(r.top as f32 / ui_scale)),
                        size(
                            px((r.right - r.left) as f32 / ui_scale),
                            px((r.bottom - r.top) as f32 / ui_scale),
                        ),
                    ),
                    rgb(color),
                ));
            };
            if radius > 0. {
                let zoom = viewport.zoom;
                let left = viewport.offset.x + rect.x as f32 * zoom;
                let top = viewport.offset.y + rect.y as f32 * zoom;
                let right = left + rect.width as f32 * zoom;
                let bottom = top + rect.height as f32 * zoom;
                let r = (radius * zoom).min((right - left).min(bottom - top) / 2.);
                let at = |x: f32, y: f32| point(px(x / ui_scale), px(y / ui_scale));
                let k = 0.552_284_8;
                // 圆角矩形轮廓：外扩 `grow` 后的四段直线 + 四段三次贝塞尔。
                let outline = |path: &mut PathBuilder, grow: f32| {
                    let (l, t, rr, b) = (left - grow, top - grow, right + grow, bottom + grow);
                    let c = r + grow;
                    path.move_to(at(l + c, t));
                    path.line_to(at(rr - c, t));
                    path.cubic_bezier_to(
                        at(rr, t + c),
                        at(rr - c * (1. - k), t),
                        at(rr, t + c * (1. - k)),
                    );
                    path.line_to(at(rr, b - c));
                    path.cubic_bezier_to(
                        at(rr - c, b),
                        at(rr, b - c * (1. - k)),
                        at(rr - c * (1. - k), b),
                    );
                    path.line_to(at(l + c, b));
                    path.cubic_bezier_to(
                        at(l, b - c),
                        at(l + c * (1. - k), b),
                        at(l, b - c * (1. - k)),
                    );
                    path.line_to(at(l, t + c));
                    path.cubic_bezier_to(
                        at(l + c, t),
                        at(l, t + c * (1. - k)),
                        at(l + c * (1. - k), t),
                    );
                    path.close();
                };
                if mask_corners {
                    for (x, y, sx, sy) in [
                        (left, top, 1., 1.),
                        (right, top, -1., 1.),
                        (right, bottom, -1., -1.),
                        (left, bottom, 1., -1.),
                    ] {
                        let mut path = PathBuilder::fill();
                        path.move_to(at(x, y));
                        path.line_to(at(x + sx * r, y));
                        path.cubic_bezier_to(
                            at(x, y + sy * r),
                            at(x + sx * r * (1. - k), y),
                            at(x, y + sy * r * (1. - k)),
                        );
                        path.close();
                        if let Ok(path) = path.build() {
                            window.paint_path(path, rgba(0x00000077));
                        }
                    }
                }
                let width = crate::selection_chrome::FRAME_WIDTH as f32;
                if frame {
                    let mut path = PathBuilder::stroke(px(width / ui_scale));
                    outline(&mut path, width / 2.);
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(crate::selection_chrome::COLOR));
                    }
                }
                if frame && highlight {
                    let (l, t, rr, b) = (
                        left - width / 2.,
                        top - width / 2.,
                        right + width / 2.,
                        bottom + width / 2.,
                    );
                    let c = r + width / 2.;
                    let arcs = [
                        (l, t, 1., 1.),
                        (rr, t, -1., 1.),
                        (rr, b, -1., -1.),
                        (l, b, 1., -1.),
                    ]
                    .map(|(x, y, sx, sy)| {
                        [
                            at(x, y + sy * c),
                            at(x, y + sy * c * (1. - k)),
                            at(x + sx * c * (1. - k), y),
                            at(x + sx * c, y),
                        ]
                    });
                    stroke_radius_arcs(window, &arcs, crate::selection_chrome::COLOR);
                }
            } else if frame {
                for r in chrome.frame() {
                    paint(r, crate::selection_chrome::COLOR);
                }
            }
            if handles {
                for circle in chrome.handles() {
                    for (inset, color) in [
                        (0, crate::selection_chrome::COLOR),
                        (crate::selection_chrome::HANDLE_BORDER, 0xffffff),
                    ] {
                        let radius =
                            crate::selection_chrome::HANDLE_SIZE as f64 / 2. - inset as f64;
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(
                                        px((circle.center.0 - radius) as f32 / ui_scale),
                                        px((circle.center.1 - radius) as f32 / ui_scale),
                                    ),
                                    size(
                                        px((radius * 2.) as f32 / ui_scale),
                                        px((radius * 2.) as f32 / ui_scale),
                                    ),
                                ),
                                rgb(color),
                            )
                            .corner_radii(px(radius as f32 / ui_scale)),
                        );
                    }
                }
            }
        },
    )
    .absolute()
    .size_full()
}
/// 圆角控制点的显示状态：悬停放大并反色，拖动再加一圈浅光晕。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RadiusKnob {
    Idle,
    Hover,
    Drag,
}

/// 选区与矩形标注共用的圆角控制点：屏幕尺寸固定的白底菱形，与缩放圆点和旋转圆环区分；
/// 外圈 1px 半透明黑线让它在白色截图内容上也分得清。以元素中心绘制，可超出元素边界。
pub(crate) fn radius_knob(state: RadiusKnob, color: u32) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let hot = state != RadiusKnob::Idle;
            let half = if hot { 5.5 } else { 4.5 };
            let diamond = |d: f32, stroke: Option<f32>| {
                let mut path =
                    stroke.map_or_else(PathBuilder::fill, |w| PathBuilder::stroke(px(w)));
                path.move_to(center + point(px(0.), px(-d)));
                path.line_to(center + point(px(d), px(0.)));
                path.line_to(center + point(px(0.), px(d)));
                path.line_to(center + point(px(-d), px(0.)));
                path.close();
                path.build().ok()
            };
            let layers = [
                (
                    state == RadiusKnob::Drag,
                    half + 5.,
                    None,
                    rgba(color << 8 | 0x3d),
                ),
                (true, half + 1.8, Some(1.), rgba(0x00000061)),
                (true, half, None, rgb(if hot { color } else { 0xffffff })),
                (
                    true,
                    half,
                    Some(1.5),
                    rgb(if hot { 0xffffff } else { color }),
                ),
            ];
            for (show, d, stroke, color) in layers {
                if let Some(path) = show.then(|| diamond(d, stroke)).flatten() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size_full()
}

/// 拖动圆角时加亮四段圆弧：先铺白色衬底再压一道粗色线。每段是 [起点, 控制点1, 控制点2, 终点]。
fn stroke_radius_arcs(window: &mut Window, arcs: &[[gpui::Point<gpui::Pixels>; 4]], color: u32) {
    for (width, paint) in [(6., rgba(0xffffffeb)), (3., rgb(color))] {
        let mut path = PathBuilder::stroke(px(width));
        for [from, a, b, to] in arcs {
            path.move_to(*from);
            path.cubic_bezier_to(*to, *a, *b);
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, paint);
        }
    }
}

/// 圆角控制点旁的数值标签：从控制点沿对角线向内偏 14 逻辑像素，按该角一侧对齐。
/// `knob`、`inward` 为根画布逻辑坐标与指向内部的符号；`zero_hint` 是拖到 0 时的补充说明。
pub(crate) fn radius_value_pill(
    knob: (f32, f32),
    inward: (f32, f32),
    value: f32,
    limit: f32,
    hovering: bool,
    zero_hint: Option<&'static str>,
    palette: crate::capture_toolbar::Palette,
) -> gpui::Div {
    const BOX_WIDTH: f32 = 240.;
    const HEIGHT: f32 = 22.;
    let value = value.max(0.).round();
    let note =
        |text: gpui::SharedString, color: u32| div().flex_none().text_color(rgb(color)).child(text);
    let mut pill = div()
        .flex_none()
        .h(px(HEIGHT))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded(px(6.))
        .bg(rgb(palette.background))
        .border_1()
        .border_color(rgb(palette.border))
        .shadow_md()
        .text_size(px(12.))
        .whitespace_nowrap()
        .text_color(rgb(palette.foreground));
    if value <= 0. {
        pill = pill.child(note(tr!("直角", "Square").into(), palette.foreground));
    } else {
        pill = pill.child(note(
            trf!("圆角 {}", "Radius {}", value).into(),
            palette.foreground,
        ));
        if value >= limit.round() {
            pill = pill.child(note(tr!("最大", "Max").into(), palette.accent));
        }
    }
    if hovering {
        pill = pill.child(note(tr!("拖动调整", "Drag").into(), palette.muted));
    } else if let Some(hint) = zero_hint.filter(|_| value <= 0.) {
        pill = pill.child(note(hint.into(), palette.muted));
    }
    let (x, y) = (knob.0 + inward.0 * 14., knob.1 + inward.1 * 14.);
    // 固定宽高的定位盒按方向贴到锚点，标签在盒内靠锚点一侧对齐，不必预估文字宽度。
    div()
        .absolute()
        .left(px(if inward.0 < 0. { x - BOX_WIDTH } else { x }))
        .top(px(if inward.1 < 0. { y - HEIGHT } else { y }))
        .w(px(BOX_WIDTH))
        .h(px(HEIGHT))
        .flex()
        .when(inward.0 < 0., |d| d.justify_end())
        .child(pill)
}
impl Render for CaptureView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.quick_action.recipe.is_some() {
            cx.defer_in(window, |view, window, cx| {
                view.maybe_start_quick_action(window, cx)
            });
        }
        // 新Capture源每次重绘都重新取得真实内容资格；旧成功marker不可借给新帧。
        let presentation_completion = (self.pin_mode && self.pin_content_rect.is_some())
            .then(|| self.pin_content_drawn_size.clone());
        if let Some(completion) = &presentation_completion {
            completion.store(0, std::sync::atomic::Ordering::Release);
        }
        let mut content_resources_ready = true;
        self.smart_candidate_sync(window, cx);
        self.sync_qr_scan(cx);
        self.light_theme = crate::theme::is_light(cx);
        crate::theme::sync_window(window, cx);
        let palette = crate::theme::palette(cx);
        self.validate_ocr_overlay();
        self.prepare_capture_magnifier(window, cx);
        self.request_control_probe(cx);
        if !self.pin_mode {
            self.ensure_watermark_field(window, cx);
        }
        // 与贴图宿主一样，窗口级捕获监听只能在 canvas 的 paint 回调中注册。
        let view = cx.entity().downgrade();
        let capture_events = (self.pin_mode
            || self.long_clip_ui.is_some()
            || (self.inline_ocr_host() && self.recognition_result.is_some()))
        .then(|| {
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let weak = view.clone();
                    window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = weak.update(cx, |view, _| {
                                view.cancel_ocr_typed_pointer_clicks();
                                view.cancel_ocr_sensitive_pointer_click();
                                view.cancel_ocr_local_retry_pointer_click();
                            });
                        }
                    });
                    let weak = view.clone();
                    window.on_mouse_event(move |_: &gpui::ScrollWheelEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = weak.update(cx, |view, _| {
                                view.cancel_ocr_typed_pointer_clicks();
                                view.cancel_ocr_sensitive_pointer_click();
                                view.cancel_ocr_local_retry_pointer_click();
                            });
                        }
                    });
                    let weak = view.clone();
                    window.on_key_event(move |_: &gpui::KeyDownEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = weak.update(cx, |view, _| {
                                view.cancel_ocr_typed_pointer_clicks();
                                view.cancel_ocr_sensitive_pointer_click();
                                view.cancel_ocr_local_retry_pointer_click();
                            });
                        }
                    });
                    let weak = view.clone();
                    window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = weak.update(cx, |v, cx| {
                                if v.long_clip_moved(e, w, cx) {
                                    return;
                                }
                                if v.ocr_overlay_dragging() {
                                    v.ocr_overlay_moved(e, w, cx);
                                    return;
                                }
                                if v.pointer_down_start.is_some() {
                                    v.moved(e, w, cx);
                                }
                            });
                        }
                    });
                    let weak = view.clone();
                    window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, w, cx| {
                        if phase == gpui::DispatchPhase::Capture {
                            let _ = weak.update(cx, |v, cx| {
                                // 剪辑与中键平移可在任何子控件上松手；先完成已认领手势。
                                if v.long_clip_up(e, w, cx) {
                                    return;
                                }
                                if e.button != MouseButton::Left {
                                    return;
                                }
                                if v.ocr_overlay_dragging() {
                                    v.ocr_overlay_up(e, w, cx);
                                    return;
                                }
                                if v.pointer_down_start.is_some() {
                                    v.up(e, w, cx);
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
        });
        self.sync_viewport(window);
        if let Err(error) = self.refresh_editor_preview(window) {
            content_resources_ready = false;
            self.status = error.to_string();
        }
        let scale = self.canvas_scale(window);
        let canvas_offset = self.canvas_offset(window);
        let (w, h) = (
            self.source.bounds.width as f32 / scale,
            self.source.bounds.height as f32 / scale,
        );
        let mut scene = self.prepared_scene();
        let mut annotations_visible = !self.pin_mode
            || self
                .pin_annotation_preview
                .as_ref()
                .is_none_or(|preview| preview.visible());
        let mut retained_rasters = None;
        let mut presentation_current = false;
        let presentation_plan =
            match self.prepare_pin_presentation_preview(window, &scene, annotations_visible) {
                Ok(plan) => {
                    presentation_current = plan.is_some();
                    plan
                }
                Err(error) => {
                    self.status = error.to_string();
                    let current_plan = self
                        .pin_presentation_style
                        .and_then(|style| self.pin_presentation_preview_plan(window, style).ok());
                    let retained = current_plan
                        .and_then(|plan| self.pin_presentation_retained_group_frame(window, plan));
                    if let Some(frame) = retained {
                        // 仅完整来源/几何一致的旧帧可继续显示，不能把旧角配新Scene。
                        scene = frame.scene;
                        retained_rasters = Some(frame.rasters);
                        annotations_visible = frame.annotations_visible;
                        Some(frame.plan)
                    } else {
                        None
                    }
                }
            };
        let presentation_body: Vec<_> = if presentation_plan.is_some() {
            self.pin_presentation_preview
                .body_tiles()
                .iter()
                .map(|tile| (tile.region, tile.image.clone()))
                .collect()
        } else {
            Vec::new()
        };
        if let Err(error) = self.prepare_watermark_previews(window, &scene) {
            content_resources_ready = false;
            self.status = error.to_string();
        }
        if let Err(error) = self.prepare_text_previews(window, &scene) {
            content_resources_ready = false;
            self.status = error.to_string();
        }
        let mut presentation_mask = match self.pin_presentation_group_path(window) {
            Ok(mask) => mask,
            Err(error) => {
                content_resources_ready = false;
                self.status = error.to_string();
                None
            }
        };
        let presentation_bound = presentation_current
            && content_resources_ready
            && presentation_plan.is_some_and(|plan| {
                self.pin_presentation_group_frame_matches(window, plan, &scene, annotations_visible)
            });
        let protected_raster_ids: std::collections::HashSet<_> = self
            .raster_previews
            .iter()
            .chain(self.pin_presentation_preview.raster_previews().iter())
            .map(|(_, preview)| preview.id.0)
            .collect();
        let mut deferred_rasters = Vec::new();
        let mut retiring_ids = std::collections::HashSet::new();
        for preview in self.retired_raster_previews.drain(..) {
            let id = preview.id.0;
            if !retiring_ids.insert(id) {
                continue;
            }
            if protected_raster_ids.contains(&id) {
                deferred_rasters.push(preview);
                continue;
            }
            match window.drop_image(preview) {
                Ok(()) => {
                    if self.retired_editor_preview_ids.contains(&id) {
                        self.retired_editor_preview_ids.retain(|old| *old != id);
                        self.editor_preview_releases =
                            self.editor_preview_releases.saturating_add(1);
                    }
                }
                Err(error) => {
                    self.status = trf!(
                        "释放旧画布纹理失败：{error}",
                        "Failed to release the old canvas texture: {error}"
                    )
                }
            }
        }
        self.retired_raster_previews.extend(deferred_rasters);
        let rasters = retained_rasters.unwrap_or_else(|| self.raster_previews.clone());
        let watermark_previews = self.watermark_previews.clone();
        let text_previews = self.text_previews.clone();
        let retained_annotations = self.retained_annotations.clone();
        let toolbar_visible = self.screenshot_toolbar_visible();
        let selected = self.selected().ok();
        // 自动吸附只是展示范围，不提交 crop；遮罩、边框和提示共用同次有效目标。
        let hover = if selected.is_none() && !self.editor.is_dragging() {
            self.display_control_selection(window, cx).filter(|target| {
                target
                    .rect
                    .valid(self.source.bounds.width, self.source.bounds.height)
            })
        } else {
            None
        };
        let clipped = self.editor.long_clip().is_some();
        if let Some(rect) = selected.filter(|_| !self.pin_mode && !clipped) {
            if let Err(error) = self.prepare_finish_preview(rect) {
                self.finish_preview = None;
                self.status = error.to_string();
            }
        } else {
            self.finish_preview = None;
        }
        let mut outer = div()
            .relative()
            .size_full()
            .overflow_hidden()
            .when(!self.pin_mode, |outer| {
                outer.bg(rgb(if clipped {
                    crate::long_clip_chrome::ClipTheme::new(self.light_theme).canvas
                } else if self.image_canvas.is_some() {
                    0x30343a
                } else {
                    0x000000
                }))
            })
            .font_family("Microsoft YaHei UI")
            .text_color(rgb(palette.foreground))
            .text_size(px(12.))
            .when(!self.pin_mode || self.pin_editing, |canvas| {
                canvas.cursor(self.canvas_cursor(scale, window.scale_factor(), window.modifiers()))
            })
            .track_focus(&self.focus)
            .children(capture_events)
            .capture_any_mouse_down(cx.listener(|view, _, _, _| {
                // 新 Down（包括工具栏/参数）不能接续已漏 Up 的正文拖选。
                view.result_selection_end(view.result_selection_generation());
                view.end_ocr_overlay_drag();
            }))
            .capture_any_mouse_down(cx.listener(|view, _, _, cx| view.qr_pointer_down_capture(cx)))
            .capture_any_mouse_down(cx.listener(
                |view, event: &gpui::MouseDownEvent, window, cx| {
                    // 外观面板的子控件会消费中键；在捕获阶段交给同一个贴图入口。
                    if event.button == MouseButton::Middle && view.middle_pin_finish_panel_open() {
                        view.middle_down(event, window, cx);
                    }
                },
            ))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::middle_down))
            .on_mouse_move(cx.listener(Self::moved))
            .on_scroll_wheel(cx.listener(Self::wheel))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::middle_up))
            .capture_key_down(cx.listener(|v, e, _, _| {
                v.reset_guide_toggle();
                v.preview_cursor_key_down(e);
            }))
            .capture_key_up(cx.listener(|v, e, window, cx| {
                v.long_clip_key_up(e, window, cx);
                v.reset_guide_toggle();
                v.preview_cursor_key_up(e);
                v.capture_color_key_up(e, window, cx);
            }))
            .on_key_down(cx.listener(Self::key))
            .on_modifiers_changed(
                cx.listener(|v, e: &gpui::ModifiersChangedEvent, window, cx| {
                    v.canvas_click_modifiers_changed(e.modifiers);
                    v.guide_modifiers_changed(e);
                    if (v.editor.is_dragging() || v.editor.has_pending_drawing())
                        && !(v.editor.local_fill_candidate().is_some()
                            && v.pointer_down_start.is_none())
                    {
                        let p = v.drawing_pointer(e.modifiers.shift);
                        v.editor.pointer_move_at(p);
                    }
                    v.capture_color_modifiers_changed(e.modifiers.shift, window, cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    v.invalidate_click_sequence();
                    if v.pin_mode {
                        return;
                    }
                    if v.screenshot_toolbar_menu.dismiss() {
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    v.wheel_input.reset();
                    if v.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None {
                        v.toolbar_panel = crate::toolbar_layout::ToolbarPanel::None;
                        cx.notify();
                        return;
                    }
                    if v.result_selection_pointer_ready() {
                        cx.stop_propagation();
                        return;
                    }
                    if v.manual_preview.is_some() {
                        v.set_manual_preview(None);
                        v.cancel_pointer();
                        v.pointer = v.physical(e.position, w);
                        v.pointer_sample = v.world_point(e.position, w);
                        v.request_control_probe(cx);
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    if v.smart_mode() {
                        let resizing = v.editor.is_changing_crop();
                        v.editor.cancel_gesture();
                        v.cancel_pointer();
                        if !resizing {
                            v.perform(crate::capture_view::Action::Tool(Tool::Edit), w, cx);
                        }
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    if v.editor.tool == Tool::Lasso {
                        if !v.editor.cancel_gesture() {
                            v.editor.select_tool(Tool::Edit);
                        }
                        v.clear_pointer_tracking();
                        v.release_capture();
                        cx.notify();
                        return;
                    }
                    if v.editor.is_dragging() {
                        v.editor.cancel_gesture();
                        v.pointer_down_start = None;
                        v.release_capture();
                        cx.notify();
                        return;
                    }
                    if if v.image_canvas.is_some() {
                        v.editor.straighten_arrow_point(
                            v.world_point(e.position, w),
                            6. * v.canvas_scale(w),
                        )
                    } else {
                        v.editor
                            .straighten_arrow_at(v.physical(e.position, w), 6. * v.canvas_scale(w))
                    } {
                        v.release_capture();
                        cx.notify();
                        return;
                    }
                    if v.editor.has_arrow_draft() {
                        v.editor.cancel_gesture();
                        v.release_capture();
                        cx.notify();
                        return;
                    }
                    if v.editor.has_pending_drawing() {
                        v.finish_content_or_status();
                        cx.notify();
                        return;
                    }
                    if v.editor.text_draft.is_some() {
                        v.finish_content_or_status();
                        cx.notify();
                        return;
                    }
                    if v.image_canvas.is_some() {
                        v.perform(crate::capture_view::Action::Tool(Tool::Edit), w, cx);
                        cx.notify();
                        return;
                    }
                    if v.open_screenshot_toolbar_menu(e, w) {
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    if let Some(action) = crate::capture_interaction::screenshot_right_click_action(
                        &v.editor,
                        v.physical(e.position, w),
                    ) {
                        v.perform(action, w, cx);
                    }
                }),
            );
        // 同一原图、Scene、对象控件按可见片段重放，只有投影平移和裁切不同。
        let clip_projection = self.long_clip_projection();
        let pieces: Vec<_> = clip_projection.as_ref().map_or_else(
            || vec![None],
            |projection| projection.pieces().iter().cloned().map(Some).collect(),
        );
        for clip_piece in pieces {
        let original_offset = canvas_offset;
        let canvas_offset = clip_piece.as_ref().map_or(original_offset, |piece| {
            original_offset + point(px(piece.translation.0 as f32 / scale), px(piece.translation.1 as f32 / scale))
        });
        let piece_mask = clip_piece.as_ref().map(|piece| Bounds::new(
            original_offset + point(px(piece.display.x as f32 / scale), px(piece.display.y as f32 / scale)),
            size(px(piece.display.width as f32 / scale), px(piece.display.height as f32 / scale)),
        ));
        if piece_mask.is_some_and(|mask| {
            let view = window.viewport_size();
            mask.origin.x >= view.width || mask.origin.y >= view.height
                || mask.origin.x + mask.size.width <= px(0.) || mask.origin.y + mask.size.height <= px(0.)
        }) { continue; }
        let scene = scene.clone();
        let rasters = rasters.clone();
        let watermark_previews = watermark_previews.clone();
        let text_previews = text_previews.clone();
        let retained_annotations = retained_annotations.clone();
        let presentation_body = presentation_body.clone();
        let presentation_completion = presentation_completion.clone();
        let presentation_mask = presentation_mask.take();
        let ime_piece = clip_piece.as_ref().map(|piece| piece.source);
        let mut root = div()
            .absolute()
            .left(canvas_offset.x)
            .top(canvas_offset.y)
            .w(px(w))
            .h(px(h));
        if let Some([b, g, r, _]) = self.source.solid_fill() {
            // 矢量纯色底直接绘制，不上传整幅底图纹理。
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(gpui::rgb(u32::from_be_bytes([0, r, g, b]))),
            );
        }
        let image_previews: Vec<_> = if let Some(piece) = clip_piece.as_ref() {
            self.long_clip_piece_previews(piece.source).into_iter().map(|(rect, image)| {
                (ShapeBounds { x: rect.x as f32, y: rect.y as f32, width: rect.width as f32, height: rect.height as f32 }, image)
            }).collect()
        } else {
            self.previews.iter().enumerate().map(|(index, image)| (self.source.preview_bounds(index), image.clone())).collect()
        };
        for (r, preview) in image_previews {
            if self.pin_mode && self.pin_presentation_style.is_some() {
                // 所有截图底图统一使用目标缓存，透明像素插值与圆角一致。
                continue;
            }
            // 超长图片按块显示，只让可见块进入GPUI的纹理缓存。
            if self.source.is_image() {
                let view = window.viewport_size();
                let left = f32::from(canvas_offset.x) + r.x / scale;
                let top = f32::from(canvas_offset.y) + r.y / scale;
                if left >= f32::from(view.width)
                    || top >= f32::from(view.height)
                    || left + r.width / scale <= 0.
                    || top + r.height / scale <= 0.
                {
                    continue;
                }
            }
            root = root.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        // 布局引擎会圆整物理像素；颜色采样必须与矢量画刷共享精确视口坐标。
                        let bounds = Bounds::new(
                            canvas_offset + point(px(r.x / scale), px(r.y / scale)),
                            size(px(r.width / scale), px(r.height / scale)),
                        );
                        crate::pin_presentation::paint_content_regions(
                            presentation_plan,
                            point(px(0.), px(0.)),
                            window,
                            |window| {
                                // 最后一列/行的采样格可以略超原图；显示仍裁到原图边界。
                                window.with_content_mask(
                                    Some(gpui::ContentMask {
                                        bounds: Bounds::new(canvas_offset, size(px(w), px(h))),
                                    }),
                                    |window| {
                                        let _ = window.paint_image_exact(
                                            bounds,
                                            Default::default(),
                                            preview.clone(),
                                            0,
                                            false,
                                        );
                                    },
                                );
                            },
                        );
                    },
                )
                .absolute()
                .left(px(r.x / scale))
                .top(px(r.y / scale))
                .w(px(r.width / scale))
                .h(px(r.height / scale)),
            );
        }
        // styled底图已经从Source.overlay采样光标；不能重复画在整体mask之外。
        if self.include_cursor && !(self.pin_mode && self.pin_presentation_style.is_some()) {
            if let (Some(cursor), Some(preview)) = (&self.cursor, &self.cursor_preview) {
                let r = cursor.rect;
                root = root.child(
                    img(preview.clone())
                        .absolute()
                        .left(px(r.x as f32 / scale))
                        .top(px(r.y as f32 / scale))
                        .w(px(r.width as f32 / scale))
                        .h(px(r.height as f32 / scale))
                        .object_fit(ObjectFit::Fill),
                );
            }
        }
        // 整个虚拟桌面的绘制层。标注几何可以伸出画布，这里只显示画布矩形内的像素。
        // 选区仅影响下面的遮罩和最终导出裁剪。
        root = root.child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .when(presentation_plan.is_none(), |layer| layer.overflow_hidden())
                .child(
                    canvas(
                        move |_, _, _| (),
                        move |_, _, window, cx| {
                            let mut paint_content = |window: &mut Window| -> bool {
                                let mut content_complete = presentation_bound;
                                // 物理目标底图仍采用预乘alpha采样；不含Scene或圆角覆盖。
                                let dpi = window.scale_factor();
                                for (region, image) in &presentation_body {
                                    let bounds = Bounds::new(
                                        point(
                                            px(region.left as f32 / dpi),
                                            px(region.top as f32 / dpi),
                                        ),
                                        size(
                                            px(region.width as f32 / dpi),
                                            px(region.height as f32 / dpi),
                                        ),
                                    );
                                    if let Err(error) = window.paint_image_nearest_exact(
                                        bounds,
                                        Default::default(),
                                        image.clone(),
                                        0,
                                        false,
                                    ) {
                                        content_complete = false;
                                        eprintln!("绘制贴图目标底图失败：{error}");
                                    }
                                }
                            if !annotations_visible {
                                return content_complete;
                            }
                            // 同一 Scene 在圆角主体 mask 中重放；每层几何只准备一次。
                            // 懒准备也避免为不可见的荧光笔画刷 tile 构造路径。
                            let mut paths = crate::vector_paint::FramePaths::new(
                                scene.layers.len(),
                                canvas_offset,
                                scale,
                                window.scale_factor(),
                            );
                            let repeated_regions = false;
                            crate::pin_presentation::paint_content_regions(
                                presentation_plan,
                                point(px(0.), px(0.)),
                                window,
                                |window| {
                                    let bounds = Bounds::new(canvas_offset, size(px(w), px(h)));
                                    let viewport = window.viewport_size();
                                    let visible_shape = |r: ShapeBounds| {
                                        let left = f32::from(bounds.origin.x) + r.x / scale;
                                        let top = f32::from(bounds.origin.y) + r.y / scale;
                                        left < f32::from(viewport.width) && top < f32::from(viewport.height)
                                            && left + r.width / scale > 0. && top + r.height / scale > 0.
                                    };
                                    let visible_rect = |r: crate::geometry::Rect| visible_shape(ShapeBounds {
                                        x: r.x as f32, y: r.y as f32,
                                        width: r.width as f32, height: r.height as f32,
                                    });
                                    // 物理像素矩形换算到窗口逻辑坐标（画布原点为 bounds.origin）。
                                    let scaled = |r: crate::geometry::Rect| Bounds::new(
                                        bounds.origin + point(px(r.x as f32 / scale), px(r.y as f32 / scale)),
                                        size(px(r.width as f32 / scale), px(r.height as f32 / scale)),
                                    );
                                    // 文字/水印层实际使用的预览图也是绘制身份的一部分。
                                    let signatures = scene.layers.iter().enumerate().map(|(layer_index, layer)| {
                                        let preview = match layer {
                                            Layer::Raster { .. } | Layer::Restore { .. } => text_previews
                                                .iter()
                                                .find(|preview| preview.layer() == layer_index)
                                                .map(|preview| (std::sync::Arc::as_ptr(&preview.image) as usize, preview.rect())),
                                            Layer::Watermark { annotation } => watermark_previews
                                                .iter()
                                                .find(|p| std::sync::Arc::ptr_eq(&p.annotation, annotation))
                                                .map(|p| (std::sync::Arc::as_ptr(&p.image) as usize, p.rect)),
                                            _ => None,
                                        };
                                        crate::vector_paint::LayerSignature::new(layer, preview)
                                    }).collect();
                                    let group = presentation_plan.map_or([0.; 4], |plan| {
                                        let b = plan.contour(0.).bounds;
                                        [b.left as f32, b.top as f32, b.width as f32, b.height as f32]
                                    });
                                    let plan = if clipped {
                                        // 各片段的origin不同，不能共用同一对保留纹理。
                                        crate::vector_paint::RetainedPlan { below: None, live: 0..scene.layers.len(), above: None }
                                    } else { retained_annotations.borrow_mut().plan(signatures, [
                                        f32::from(bounds.origin.x), f32::from(bounds.origin.y),
                                        scale, window.scale_factor(), w, h,
                                        f32::from(viewport.width), f32::from(viewport.height),
                                        group[0], group[1], group[2], group[3],
                                    ]) };
                                    let mut paint_layers = |window: &mut Window, range: std::ops::Range<usize>| -> bool {
                                    let mut content_complete = true;
                                    for layer_index in range {
                                        let layer = &scene.layers[layer_index];
                                        match layer {
                                            Layer::Empty => {}
                                            Layer::Watermark { annotation } => {
                                                if let Some(preview) =
                                                    watermark_previews.iter().find(|p| {
                                                        std::sync::Arc::ptr_eq(
                                                            &p.annotation,
                                                            annotation,
                                                        )
                                                    })
                                                {
                                                    let rect = preview.rect;
                                                    content_complete &= window.paint_image_exact(
                                                        scaled(rect),
                                                        Default::default(),
                                                        preview.image.clone(),
                                                        0,
                                                        false,
                                                    ).is_ok();
                                                } else if visible_shape(annotation.bounds()) {
                                                    content_complete = false;
                                                }
                                            }
                                            Layer::LocalFill { patches } => {
                                                let viewport = window.viewport_size();
                                                for patch in patches.iter() {
                                                    let rect = patch.rect;
                                                    let left = f32::from(bounds.origin.x)
                                                        + rect.x as f32 / scale;
                                                    let top = f32::from(bounds.origin.y)
                                                        + rect.y as f32 / scale;
                                                    if left >= f32::from(viewport.width)
                                                        || top >= f32::from(viewport.height)
                                                        || left + rect.width as f32 / scale <= 0.
                                                        || top + rect.height as f32 / scale <= 0.
                                                    {
                                                        continue;
                                                    }
                                                    if let Some((_, preview)) =
                                                        rasters.iter().find(|(cached, _)| {
                                                            std::sync::Arc::ptr_eq(
                                                                cached,
                                                                &patch.image,
                                                            )
                                                        })
                                                    {
                                                        content_complete &= window.paint_image_nearest_exact(
                                                            Bounds::new(
                                                                point(px(left), px(top)),
                                                                size(
                                                                    px(rect.width as f32 / scale),
                                                                    px(rect.height as f32 / scale),
                                                                ),
                                                            ),
                                                            Default::default(),
                                                            preview.clone(),
                                                            0,
                                                            false,
                                                        ).is_ok();
                                                    } else if visible_rect(patch.rect) {
                                                        content_complete = false;
                                                    }
                                                }
                                            }
                                            Layer::EffectRegions { patches } => {
                                                for patch in patches.iter() {
                                                    if let Some((_, preview)) =
                                                        rasters.iter().find(|(cached, _)| {
                                                            std::sync::Arc::ptr_eq(
                                                                cached,
                                                                &patch.image,
                                                            )
                                                        })
                                                    {
                                                        let rect = patch.rect;
                                                        content_complete &= window.paint_image_nearest_exact(
                                                            scaled(rect),
                                                            Default::default(),
                                                            preview.clone(),
                                                            0,
                                                            false,
                                                        ).is_ok();
                                                    } else if visible_rect(patch.rect) {
                                                        content_complete = false;
                                                    }
                                                }
                                            }
                                            Layer::Raster { rect, image }
                                            | Layer::Restore { rect, image } => {
                                                if let Some(preview) = text_previews
                                                    .iter()
                                                    .find(|preview| preview.layer() == layer_index)
                                                {
                                                    let rect = preview.rect();
                                                    content_complete &= window.paint_image_exact(
                                                        scaled(rect),
                                                        Default::default(),
                                                        preview.image.clone(),
                                                        0,
                                                        false,
                                                    ).is_ok();
                                                    continue;
                                                }
                                                if let Some((_, preview)) =
                                                    rasters.iter().find(|(cached, _)| {
                                                        std::sync::Arc::ptr_eq(cached, image)
                                                    })
                                                {
                                                    content_complete &= window.paint_image(
                                                        scaled(*rect),
                                                        Default::default(),
                                                        preview.clone(),
                                                        0,
                                                        false,
                                                    ).is_ok();
                                                } else if visible_rect(*rect) {
                                                    content_complete = false;
                                                }
                                            }
                                            Layer::Spotlight { geometry } => {
                                                crate::vector_paint::paint(
                                                    validated_content_path(paths.get(
                                                        layer_index,
                                                        crate::vector_paint::PathPart::Main,
                                                        &geometry.mask,
                                                        geometry.mask_style(),
                                                        repeated_regions,
                                                    ), &geometry.mask, &mut content_complete),
                                                    geometry.mask_style(),
                                                    window,
                                                );
                                                crate::vector_paint::paint(
                                                    validated_content_path(paths.get(
                                                        layer_index,
                                                        crate::vector_paint::PathPart::Border,
                                                        &geometry.border,
                                                        geometry.border_style(),
                                                        repeated_regions,
                                                    ), &geometry.border, &mut content_complete),
                                                    geometry.border_style(),
                                                    window,
                                                );
                                            }
                                            Layer::Vector { mesh, style, .. } => {
                                                crate::vector_paint::paint(
                                                    validated_content_path(paths.get(
                                                        layer_index,
                                                        crate::vector_paint::PathPart::Main,
                                                        mesh,
                                                        *style,
                                                        repeated_regions,
                                                    ), mesh, &mut content_complete),
                                                    *style,
                                                    window,
                                                );
                                            }
                                            Layer::Highlighter {
                                                mesh, style, tiles, ..
                                            } => {
                                                let viewport = window.viewport_size();
                                                for tile in tiles.iter() {
                                                    let rect = tile.rect;
                                                    let left = f32::from(bounds.origin.x)
                                                        + rect.x as f32 / scale;
                                                    let top = f32::from(bounds.origin.y)
                                                        + rect.y as f32 / scale;
                                                    if left >= f32::from(viewport.width)
                                                        || top >= f32::from(viewport.height)
                                                        || left + rect.width as f32 / scale <= 0.
                                                        || top + rect.height as f32 / scale <= 0.
                                                    {
                                                        continue;
                                                    }
                                                    if let Some((_, preview)) =
                                                        rasters.iter().find(|(cached, _)| {
                                                            std::sync::Arc::ptr_eq(
                                                                cached,
                                                                &tile.image,
                                                            )
                                                        })
                                                    {
                                                        content_complete &= crate::vector_paint::paint_image(
                                                            validated_content_path(paths.get(
                                                                layer_index,
                                                                crate::vector_paint::PathPart::Main,
                                                                mesh,
                                                                *style,
                                                                repeated_regions || tiles.len() > 1,
                                                            ), mesh, &mut content_complete),
                                                            *style,
                                                            bounds.origin,
                                                            scale,
                                                            rect,
                                                            tile.image_rect,
                                                            preview,
                                                            window,
                                                            cx,
                                                        ).is_ok();
                                                    } else if visible_rect(tile.rect) {
                                                        content_complete = false;
                                                    }
                                                }
                                            }
                                            Layer::Stroke { segments, style } => {
                                                for (a, b) in segments.iter() {
                                                    let mut path = PathBuilder::stroke(px(style
                                                        .width
                                                        / scale));
                                                    path.move_to(
                                                        bounds.origin
                                                            + point(
                                                                px(a.x / scale),
                                                                px(a.y / scale),
                                                            ),
                                                    );
                                                    path.line_to(
                                                        bounds.origin
                                                            + point(
                                                                px(b.x / scale),
                                                                px(b.y / scale),
                                                            ),
                                                    );
                                                    match path.build() {
                                                        Ok(path) => window.paint_path(path, rgb(style.color)),
                                                        Err(_) => content_complete = false,
                                                    }
                                                    // 共享分段的端点是圆头；与导出的距离场覆盖保持一致。
                                                    let radius = style.width / scale / 2.;
                                                    for p in [a, b] {
                                                        window.paint_quad(
                                                            fill(
                                                                Bounds::new(
                                                                    bounds.origin
                                                                        + point(
                                                                            px(p.x / scale
                                                                                - radius),
                                                                            px(p.y / scale
                                                                                - radius),
                                                                        ),
                                                                    size(
                                                                        px(radius * 2.),
                                                                        px(radius * 2.),
                                                                    ),
                                                                ),
                                                                rgb(style.color),
                                                            )
                                                            .corner_radii(px(radius)),
                                                        );
                                                    }
                                                }
                                            }
                                            Layer::Mosaic { blocks } => {
                                                for block in blocks.iter() {
                                                    let r = block.rect;
                                                    window.paint_quad(fill(
                                                        scaled(r),
                                                        rgb(block.color),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    content_complete
                                    };
                                    // 静态下层、实时层、静态上层依次合成；缓存层只在内容变化时重画。
                                    if let Some(part) = plan.below {
                                        content_complete &= window.paint_retained_layer(
                                            &part.layer, part.key, part.mode, bounds,
                                            |window| paint_layers(window, part.range.clone()),
                                        );
                                    }
                                    content_complete &= paint_layers(window, plan.live);
                                    if let Some(part) = plan.above {
                                        content_complete &= window.paint_retained_layer(
                                            &part.layer, part.key, part.mode, bounds,
                                            |window| paint_layers(window, part.range.clone()),
                                        );
                                    }
                                },
                            );
                            content_complete
                            };
                            if let Some(plan) = presentation_plan {
                                let contour = plan.contour(0.);
                                let dpi = f64::from(window.scale_factor());
                                let b = contour.bounds;
                                let bounds = Bounds::new(
                                    point(px((b.left / dpi) as f32), px((b.top / dpi) as f32)),
                                    size(px((b.width / dpi) as f32), px((b.height / dpi) as f32)),
                                );
                                if let Some(completion) = presentation_completion {
                                    // marker由renderer在完整全帧和实际Present成功后写，记录命令不是成功。
                                    window.with_rounded_content_group_masked_tracked(
                                        bounds,
                                        px((contour.radius_x / dpi) as f32).into(),
                                        px((contour.radius_y / dpi) as f32).into(),
                                        presentation_mask,
                                        completion,
                                        &mut paint_content,
                                    );
                                } else {
                                    window.with_rounded_content_group(
                                        bounds,
                                        px((contour.radius_x / dpi) as f32).into(),
                                        px((contour.radius_y / dpi) as f32).into(),
                                        &mut paint_content,
                                    );
                                }
                            } else if let Some(completion) = presentation_completion {
                                // 无完整准备帧也插入实际拒绝门，不能让边框/占位Present授予首显。
                                window.with_rounded_content_group_masked_tracked(
                                    Bounds::new(point(px(0.), px(0.)), window.viewport_size()),
                                    Default::default(), Default::default(), None, completion,
                                    |_| false,
                                );
                            } else {
                                paint_content(window);
                            }
                        },
                    )
                    .size_full(),
                ),
        );
        let shade = |x: f32, y: f32, width: f32, height: f32| {
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(width.max(0.)))
                .h(px(height.max(0.)))
                .bg(rgba(0x00000077))
        };
        let display_hole = selected.or_else(|| hover.as_ref().map(|target| target.rect));
        if let Some(r) = display_hole.filter(|_| !self.pin_mode && !clipped) {
            let (x, y, rw, rh) = (
                r.x as f32 / scale,
                r.y as f32 / scale,
                r.width as f32 / scale,
                r.height as f32 / scale,
            );
            root = root.children([
                shade(0., 0., w, y),
                shade(0., y, x, rh),
                shade(x + rw, y, w - x - rw, rh),
                shade(0., y + rh, w, h - y - rh),
            ]);
        } else if !self.pin_mode && !clipped {
            root = root.child(shade(0., 0., w, h));
        }
        if let Some(r) = selected.filter(|_| !self.pin_mode && !clipped) {
            root = root.child(self.finish_chrome(r, scale, canvas_offset));
            root = root.child(crop_chrome(
                r,
                window.scale_factor(),
                self.canvas_viewport(window),
                !self.finish_has_visible_border(),
                // 拖动圆角时暂隐 8 个缩放点，保留圆角柄和数值。
                self.editor.tool == Tool::Crop && self.finish_radius_drag.is_none(),
                self.finish_frame_radius(r),
                false,
                self.finish_radius_drag.is_some(),
            ));
            root = root.child(self.finish_radius_knobs(r, window, cx));
            if self.image_canvas.is_some() {
                let output_size = self.shown_finish_style().layout(r.width, r.height).ok();
                let extra = output_size
                    .filter(|layout| {
                        layout.output_width != r.width || layout.output_height != r.height
                    })
                    .map_or_else(String::new, |layout| {
                        trf!(
                            " · 输出 {} × {}",
                            " · Output {} × {}",
                            layout.output_width,
                            layout.output_height
                        )
                    });
                let ui_scale = window.scale_factor();
                let viewport = self.canvas_viewport(window);
                let projected = crate::selection_chrome::SelectionChrome::projected(
                    r,
                    f64::from(viewport.zoom),
                    (f64::from(viewport.offset.x), f64::from(viewport.offset.y)),
                )
                .0;
                let host = window.viewport_size();
                let label_height = 24.;
                // 固定高度并给外置控制点留下 8px：不能靠字体自然高度或 top=0 把标签压进选区。
                let above = projected.top as f32 / ui_scale - label_height - 8. / ui_scale;
                let below = projected.bottom as f32 / ui_scale + 8. / ui_scale;
                let label_y = if above >= 0. {
                    Some(above)
                } else if below + label_height <= f32::from(host.height) {
                    Some(below)
                } else {
                    None
                };
                let label_width = f32::from(host.width).min(300.);
                let label_x = (projected.left as f32 / ui_scale)
                    .clamp(0., (f32::from(host.width) - label_width).max(0.));
                if let Some(label_y) = label_y {
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(label_x - f32::from(canvas_offset.x)))
                            .top(px(label_y - f32::from(canvas_offset.y)))
                            .max_w(px(label_width))
                            .h(px(label_height))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .bg(rgb(palette.background))
                            .child(format!(
                                "{} × {} · ({}, {}){}",
                                r.width,
                                r.height,
                                self.source.bounds.left + r.x as i32,
                                self.source.bounds.top + r.y as i32,
                                extra,
                            )),
                    );
                }
            }
        }
        if let Some(chrome) = self.lasso_chrome(scale) {
            root = root.child(chrome);
        }
        if self.smart_mode()
            && self.smart_candidates.core.is_active()
            && !self.editor.is_changing_crop()
        {
            let applied = self.editor.smart_applied_regions();
            let hovered = self.smart_candidates.core.candidate_at(self.pointer_sample);
            for (index, region) in self
                .smart_candidates
                .core
                .candidates()
                .iter()
                .copied()
                .enumerate()
            {
                let selected = applied.iter().any(|other| {
                    crate::smart_mosaic::regions_match(
                        *other,
                        region,
                        self.smart_candidates.core.region_tolerance(),
                    )
                });
                let color = if selected {
                    0x35b77c
                } else if hovered == Some(index) {
                    0xffc35c
                } else {
                    0x69a9ff
                };
                root = root.child(outline(region, scale, color));
            }
            if let Some(marquee) = self.smart_candidates.core.marquee() {
                root = root.child(outline(marquee, scale, 0x69a9ff).bg(rgba(0x69a9ff18)));
            }
            if self.smart_candidates.core.is_busy() {
                if let Some(rect) = self.editor.crop {
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(rect.x as f32 / scale + 8.))
                            .top(px(rect.y as f32 / scale + 8.))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(palette.background))
                            .text_color(rgb(palette.foreground))
                            .text_size(px(12.))
                            .child(tr!("正在识别文字区域…", "Detecting text regions…")),
                    );
                }
            }
        }
        if self.show_guides {
            let x = self.pointer.0 as f32 / scale;
            let y = self.pointer.1 as f32 / scale;
            root = root
                .child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top_0()
                        .w(px(1.))
                        .h(px(h))
                        .bg(rgba(0xffffff60)),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(y))
                        .w(px(w))
                        .h(px(1.))
                        .bg(rgba(0xffffff60)),
                );
        }
        if selected.is_none() && !self.editor.is_dragging() {
            if let Some(target) = &hover {
                // 自动选区框也按当前圆角画，框内即单击后导出的圆角图。
                root = root.child(crop_chrome(
                    target.rect,
                    window.scale_factor(),
                    self.canvas_viewport(window),
                    true,
                    false,
                    self.finish_frame_radius(target.rect),
                    true,
                    false,
                ));
            }
            if let (Some(preview), Some(labels)) = (
                &self.magnifier_image,
                self.capture_color_labels(hover.as_ref().map(|target| target.rect), window),
            ) {
                let monitor = self
                    .source
                    .screen_rect(self.source.monitor_at(self.pointer));
                let card_width = MAGNIFIER_CARD_WIDTH;
                let keys_hint = tr!(
                    "Shift 切换格式 · P 切换坐标",
                    "Shift: format · P: coordinates"
                );
                // 卡片高度随说明行数自适应；先按字宽估算换行，供贴边翻转定位使用。
                let card_height = MAGNIFIER_CARD_HEIGHT
                    + (wrapped_lines(keys_hint, MAGNIFIER_GRID_WIDTH) - 1.) * 15.;
                let place = |pointer: i32, start: u32, length: u32, side: f32| {
                    let pointer = pointer as f32 / scale;
                    let start = start as f32 / scale;
                    let end = start + length as f32 / scale;
                    let position = if pointer + 22. + side > end {
                        pointer - 22. - side
                    } else {
                        pointer + 22.
                    };
                    position.clamp(start, (end - side).max(start))
                };
                let x = place(self.pointer.0, monitor.x, monitor.width, card_width);
                let y = place(self.pointer.1, monitor.y, monitor.height, card_height);
                let swatch = self.magnifier_color.unwrap_or(0);
                let hint = hover.as_ref().map_or_else(
                    || {
                        tr!(
                            "拖动框选 · 滚轮切换控件层级",
                            "Drag to select · wheel: layer"
                        )
                        .to_owned()
                    },
                    |target| {
                        trf!(
                            "{} · 滚轮切换父子范围",
                            "{} · wheel: parent/child",
                            target.title
                        )
                    },
                );
                root = root.child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(card_width))
                        .p(px(MAGNIFIER_PADDING))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .overflow_hidden()
                        .bg(rgb(palette.background))
                        .text_color(rgb(palette.foreground))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(rgb(palette.border))
                        .shadow_lg()
                        .child(
                            div()
                                .flex_none()
                                .w(px(MAGNIFIER_GRID_WIDTH))
                                .h(px(MAGNIFIER_GRID_HEIGHT))
                                .rounded(px(4.))
                                .overflow_hidden()
                                .child(magnifier_grid(preview.clone(), self.magnifier_cursor)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(16.))
                                        .rounded(px(3.))
                                        .border_1()
                                        .border_color(rgb(palette.border))
                                        .bg(rgb(swatch)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .font_family("Consolas")
                                        .text_size(px(13.))
                                        .child(labels.color),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .h(px(16.))
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(12.))
                                .text_color(rgb(palette.muted))
                                .child(
                                    div()
                                        .font_family("Consolas")
                                        .truncate()
                                        .child(labels.coords),
                                )
                                .child(div().flex_none().child(tr!("C 复制", "C copy"))),
                        )
                        .child(div().flex_none().h(px(1.)).bg(rgb(palette.border)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .text_size(px(11.))
                                .line_height(px(15.))
                                .text_color(rgb(palette.muted))
                                .child(keys_hint)
                                .child(div().truncate().child(hint)),
                        ),
                );
            }
        }
        if !self.pin_mode || (self.pin_editing && annotations_visible) {
            if let Some(b) = self.editor.marquee() {
                root = root.child(outline(b, scale, 0x6c9ffc).bg(rgba(0x6c9ffc22)));
            }
            let multiple = self.editor.selected_ids().len() > 1;
            if !multiple {
                for id in &self.editor.selection {
                    if Some(*id) != self.editor.selected {
                        if let Some(a) = self.editor.document.get(*id) {
                            root = root.child(annotation_outline(a, scale));
                        }
                    }
                }
                // 圆角拖动中只留加亮圆弧、控制点和数值，缩放点与旋转柄暂隐。
                let radius_drag = self.editor.dragging_radius_corner();
                if let Some(a) = self.editor.selected_annotation() {
                    root = root.child(annotation_outline(a, scale));
                    let handles = if radius_drag.is_some() {
                        Vec::new()
                    } else {
                        a.handles()
                    };
                    for (i, p) in handles.into_iter().enumerate() {
                        let active = (a.tool == Tool::Arrow && i == 2)
                            || (a.tool == Tool::Polyline && self.editor.active_node() == Some(i));
                        root = root.child(
                            handle(
                                p.x / scale,
                                p.y / scale,
                                if active { 0xffc56b } else { 0x6c9ffc },
                            )
                            .rounded_full(),
                        );
                    }
                }
                let controls = self.editor.radius_controls(6. * scale);
                let shape = self
                    .editor
                    .selected_annotation()
                    .filter(|_| !controls.is_empty());
                let hovered = controls
                    .iter()
                    .find(|c| radius_drag.is_none() && c.hit(self.pointer_sample))
                    .map(|c| c.corner);
                if let (Some(a), Some(_)) = (shape, radius_drag) {
                    let local = a.local_bounds();
                    let rotation = a.rotation_transform();
                    let k = 0.552_284_8;
                    let corners = [
                        (local.x, local.y, 1., 1.),
                        (local.x + local.width, local.y, -1., 1.),
                        (local.x + local.width, local.y + local.height, -1., -1.),
                        (local.x, local.y + local.height, 1., -1.),
                    ];
                    let arcs: Vec<_> = corners
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, (x, y, sx, sy))| {
                            let r = a
                                .radius_at_corner(i)
                                .max(0.)
                                .min(local.width.min(local.height) / 2.);
                            (r > 0.).then(|| {
                                [
                                    (x, y + sy * r),
                                    (x, y + sy * r * (1. - k)),
                                    (x + sx * r * (1. - k), y),
                                    (x + sx * r, y),
                                ]
                                .map(|(x, y)| rotation.map(crate::annotation::Point { x, y }))
                            })
                        })
                        .collect();
                    root = root.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, w, _| {
                                let arcs: Vec<_> = arcs
                                    .iter()
                                    .map(|arc| {
                                        arc.map(|p| {
                                            bounds.origin + point(px(p.x / scale), px(p.y / scale))
                                        })
                                    })
                                    .collect();
                                stroke_radius_arcs(w, &arcs, 0x6c9ffc);
                            },
                        )
                        .absolute()
                        .size_full(),
                    );
                }
                let center = shape.map(|a| crate::transform::center(a.local_bounds()));
                for control in &controls {
                    let p = control.position;
                    let id = match control.corner {
                        0 => "radius-top-left",
                        1 => "radius-top-right",
                        2 => "radius-bottom-right",
                        3 => "radius-bottom-left",
                        _ => continue,
                    };
                    let state = if radius_drag == Some(control.corner) {
                        RadiusKnob::Drag
                    } else if hovered == Some(control.corner) {
                        RadiusKnob::Hover
                    } else {
                        RadiusKnob::Idle
                    };
                    // 光标与该角同向（旋转后按柄相对中心的屏幕方向），与编辑器悬停判定一致。
                    let down = center.is_none_or(|c| (p.x - c.x) * (p.y - c.y) >= 0.);
                    root = root.child(
                        div()
                            .id(id)
                            .relative()
                            .absolute()
                            .left(px(p.x / scale - 6.))
                            .top(px(p.y / scale - 6.))
                            .size(px(12.))
                            .cursor(if down {
                                gpui::CursorStyle::ResizeUpLeftDownRight
                            } else {
                                gpui::CursorStyle::ResizeUpRightDownLeft
                            })
                            .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
                            .child(radius_knob(state, 0x6c9ffc))
                            .child(self.probe(id))
                            .when(control.corner == 0, |knob| {
                                knob.child(self.probe("radius-handle"))
                            }),
                    );
                }
                let active = radius_drag.or(hovered);
                if let (Some(a), Some(c), Some(control)) = (
                    shape,
                    center,
                    active.and_then(|corner| controls.iter().find(|k| k.corner == corner)),
                ) {
                    let p = control.position;
                    let value = self
                        .editor
                        .dragging_radius_value()
                        .unwrap_or_else(|| a.radius_at_corner(control.corner));
                    let local = a.local_bounds();
                    let inward = |d: f32| if d < 0. { -1. } else { 1. };
                    let pill = radius_value_pill(
                        (p.x / scale, p.y / scale),
                        (inward(c.x - p.x), inward(c.y - p.y)),
                        value,
                        (local.width.min(local.height) / 2.).min(50.),
                        radius_drag.is_none(),
                        None,
                        palette,
                    );
                    root = root.child(if radius_drag.is_some() {
                        pill.child(self.probe("radius-value-hint"))
                    } else {
                        pill
                    });
                }
            }
            if let Some(b) = self.editor.selection_bounds() {
                if multiple {
                    root = root.child(outline(b, scale, 0x6c9ffc));
                    for i in [0, 2, 5, 7] {
                        let p = b.handles()[i];
                        root =
                            root.child(handle(p.x / scale, p.y / scale, 0x6c9ffc).rounded_full());
                    }
                }
                if let Some(control) = self
                    .editor
                    .rotation_control(6. * scale)
                    .filter(|_| !self.editor.is_dragging_radius())
                {
                    let knob = control.position;
                    let line_from = control.anchor;
                    root = root.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, w, _| {
                                let mut path = PathBuilder::stroke(px(1.));
                                path.move_to(
                                    bounds.origin
                                        + point(px(line_from.x / scale), px(line_from.y / scale)),
                                );
                                path.line_to(
                                    bounds.origin + point(px(knob.x / scale), px(knob.y / scale)),
                                );
                                if let Ok(path) = path.build() {
                                    w.paint_path(path, rgb(0x6c9ffc));
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    );
                    root = root.child(
                        div()
                            .relative()
                            .id("rotation-handle")
                            .absolute()
                            .left(px(knob.x / scale - 5.))
                            .top(px(knob.y / scale - 5.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .border_1()
                            .border_color(rgb(0x6c9ffc))
                            .child(self.probe("rotation-handle")),
                    );
                }
            }
            if let Some(draft) = &self.editor.text_draft {
                let b = draft.annotation.bounds();
                let local = draft.annotation.local_bounds();
                let rotation = draft.annotation.rotation_transform();
                let view = cx.entity();
                let focus = self.focus.clone();
                let geometry =
                    crate::text_geometry::TextGeometry::new(&draft.annotation, scale).ok();
                let selection = self.input.selection.clone();
                let caret_index = self.input.caret();
                root = root.child(outline(b, scale, 0x6c9ffc)).child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let origin = point(px(local.x / scale), px(local.y / scale));
                            let map_rect = |rect: Bounds<gpui::Pixels>| {
                                let shape = ShapeBounds {
                                    x: f32::from(rect.origin.x) * scale,
                                    y: f32::from(rect.origin.y) * scale,
                                    width: f32::from(rect.size.width) * scale,
                                    height: f32::from(rect.size.height) * scale,
                                };
                                crate::transform::corners(shape).map(|p| rotation.map(p))
                            };
                            let mut paint = |rect: Bounds<gpui::Pixels>, color: gpui::Hsla| {
                                let corners = map_rect(rect);
                                let mut path = PathBuilder::fill();
                                path.move_to(
                                    bounds.origin
                                        + point(px(corners[0].x / scale), px(corners[0].y / scale)),
                                );
                                for p in &corners[1..] {
                                    path.line_to(
                                        bounds.origin + point(px(p.x / scale), px(p.y / scale)),
                                    );
                                }
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, color);
                                }
                            };
                            let caret = if let Some(geometry) = &geometry {
                                for rect in geometry.selection(selection.clone(), origin) {
                                    paint(rect, rgba(0x5796ff55).into());
                                }
                                geometry.caret(caret_index, origin)
                            } else {
                                Bounds::new(origin, size(px(1.5), px(24.)))
                            };
                            paint(caret, rgb(0x6c9ffc).into());
                            let b = crate::transform::bounds(map_rect(caret));
                            let caret = Bounds::new(
                                bounds.origin + point(px(b.x / scale), px(b.y / scale)),
                                size(px(b.width / scale), px(b.height / scale)),
                            );
                            // IME仅注册实际光标所在片段，不能被后面的片段覆盖位置。
                            if ime_piece.is_none_or(|piece| piece.contains(((b.x + b.width / 2.).floor() as i32, (b.y + b.height / 2.).floor() as i32))) {
                                window.handle_input(
                                    &focus,
                                    gpui::ElementInputHandler::new(caret, view.clone()),
                                    cx,
                                );
                            }
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
            }
        }
        outer = if let Some(mask) = piece_mask {
            let source = clip_piece.as_ref().expect("剪辑片段").source;
            outer.child(div().id(("long-clip-piece", (u64::from(source.x) << 32) | u64::from(source.y)))
                .absolute().left(mask.origin.x).top(mask.origin.y)
                .w(mask.size.width).h(mask.size.height).overflow_hidden()
                .child(root.left(canvas_offset.x - mask.origin.x).top(canvas_offset.y - mask.origin.y)))
        } else {
            outer.child(root)
        };
        }
        let mut root = outer;
        self.sync_markdown_translation(window, cx);
        if let Some(panel) = self.render_formula_panel(window, cx) {
            root = root.child(panel);
        }
        let ocr_progress = self.render_ocr_progress(window, cx);
        let selection_ocr_busy = ocr_progress.is_some();
        if let Some(progress) = ocr_progress {
            root = root.child(progress);
        }
        // 窗口工具栏/弹窗与原图视口分开，不随缩放变小或随平移离开窗口。
        let scale = window.scale_factor();
        if selection_ocr_busy && self.ocr_panel_busy() && self.current_result_text().is_none() {
            // 识别中先在结果面板的位置显示骨架，完成后原位替换。
            if let Some(panel) = self.render_ocr_busy_panel(window, cx) {
                root = root.child(panel);
            }
        }
        if let Some(overlay) = self.render_ocr_overlay(scale, window, cx) {
            root = root.child(overlay);
            if self.auto_copy_recovery_is_current() {
                // 自动复制失败的原位层不显示全文面板，仍必须实际paint同代结果后才可重试。
                let result_frame = self.result_output_frame();
                let painted_result_output = self.painted_result_output.clone();
                let painted_result_projection = Rc::clone(&self.painted_result_projection);
                let projection_epoch = Arc::clone(&self.ocr_projection_epoch);
                let result_projection = self.ocr_projection(window);
                let result_body = self.current_result_body_identity();
                let result_owner = cx.entity().downgrade();
                root = root.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, cx| {
                            let Some(owner) = result_owner.upgrade() else {
                                return;
                            };
                            let view = owner.read(cx);
                            if !view.auto_copy_recovery_is_current()
                                || view.ocr_overlay.is_none()
                                || view.result_output_frame() != result_frame
                                || !Arc::ptr_eq(&view.ocr_projection_epoch, &projection_epoch)
                                || view.ocr_projection(window) != result_projection
                                || !Arc::ptr_eq(&view.current_result_body_identity(), &result_body)
                            {
                                return;
                            }
                            let Some(projection) = result_projection else {
                                return;
                            };
                            *painted_result_projection.borrow_mut() =
                                Some(crate::ocr_projection::ProjectionPaint {
                                    epoch: Arc::clone(&projection_epoch),
                                    projection,
                                });
                            painted_result_output.set(
                                (window.is_window_active() && view.focus.is_focused(window))
                                    .then_some(result_frame),
                            );
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
            }
        }
        // 二维码按钮在工具栏之下，避免全屏选区时盖住工具栏。
        let (qr_buttons, qr_popup) = self.render_qr_overlay(window, cx);
        root = root.children(qr_buttons);
        if clipped {
            // 停靠面板盖住画布溢出部分；原生属性弹层随后绘制，仍在面板之上。
            root = root.child(self.long_clip_navigation(window, cx));
            root = root.child(self.long_clip_chrome(window, cx));
        }
        if let Some(r) = selected.filter(|_| !self.pin_mode) {
            // 仅调整截图范围时收起工具栏；绘制和对象编辑保持显示，避免每次落笔闪烁。
            if toolbar_visible && !self.editor.is_changing_crop() {
                root = root.child(self.toolbar(r, window, cx));
            }
        }
        if let Some(menu) = self.screenshot_toolbar_menu_overlay(cx) {
            root = root.child(menu);
        }
        if let Some(popup) = qr_popup {
            root = root.child(popup);
        }
        if let Some(text) = self
            .current_result_text()
            .filter(|_| self.ocr_overlay.is_none() || self.result_editor.is_some())
            .map(str::to_owned)
        {
            self.refresh_ocr_result_click(window);
            let editing = self.result_editor.is_some();
            let result_frame = self.result_output_frame();
            let painted_result_output = self.painted_result_output.clone();
            let painted_result_projection = Rc::clone(&self.painted_result_projection);
            let projection_epoch = Arc::clone(&self.ocr_projection_epoch);
            let result_projection = self.ocr_projection(window);
            let result_body = self.current_result_body_identity();
            let result_owner = cx.entity().downgrade();
            let current = self.ui_monitor(self.toolbar_monitor);
            let work = self
                .ocr_ui_work(window)
                .unwrap_or(crate::toolbar_layout::PanelRect {
                    x: current.x as f32 / scale,
                    y: current.y as f32 / scale,
                    width: current.width as f32 / scale,
                    height: current.height as f32 / scale,
                });
            let monitor_width = work.width;
            let monitor_height = work.height;
            // 截图/图片沿已投影的 OCR 面板位置；无可停靠选区时退回客户区。
            let (panel_x, panel_y, panel_width, panel_height) =
                self.ocr_panel_frame(window).map_or_else(
                    || {
                        let left = 24_f32.min(monitor_width / 4.);
                        let top = 60_f32.min(monitor_height / 4.);
                        (
                            work.x + left,
                            work.y + top,
                            (monitor_width - left * 2.).clamp(1., 520.),
                            (monitor_height - top - 24_f32.min(monitor_height / 4.))
                                .clamp(1., 600.),
                        )
                    },
                    |layout| {
                        let r = layout.panel;
                        (r.x, r.y, r.width, r.height)
                    },
                );
            // 保留标题、编辑提示与按钮空间；高 DPI 下正文滚动，不把操作行挤出屏幕。
            let body_height = (panel_height - if editing { 120. } else { 112. }).clamp(1., 460.);
            // 停靠在选区旁时与 OCR 结果面板同一外观：标题栏、正文、底部操作栏。
            let docked = self.ocr_panel_frame(window).is_some();
            let chip = move |button: gpui::Stateful<gpui::Div>| {
                button
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(palette.border))
                    .text_size(px(12.5))
                    .whitespace_nowrap()
                    .hover(move |s| s.bg(rgb(palette.hover)))
            };
            let primary_chip = move |button: gpui::Stateful<gpui::Div>| {
                button
                    .h(px(28.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .rounded_md()
                    .bg(rgb(palette.accent))
                    .text_color(rgb(palette.background))
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .whitespace_nowrap()
            };
            let line_count = text.lines().filter(|line| !line.trim().is_empty()).count();
            let body = div()
                .id("ocr-result-text")
                .relative()
                .max_h(px(body_height))
                .flex_shrink_0()
                .overflow_y_scroll()
                .when(editing, |body| {
                    body.track_scroll(&self.result_editor_scroll)
                });
            let body = if self.inline_ocr_host() {
                let selected = self.result_selection_range();
                let marked = self
                    .result_editor
                    .as_ref()
                    .and_then(|editor| editor.input.marked.clone());
                // 合并非重叠 style runs；选择背景和 IME 下划线共用正文布局。
                let mut stops = vec![0, text.len(), selected.start, selected.end];
                if let Some(marked) = &marked {
                    stops.extend([marked.start, marked.end]);
                }
                stops.sort_unstable();
                stops.dedup();
                let highlights = stops
                    .windows(2)
                    .filter_map(|pair| {
                        let range = pair[0]..pair[1];
                        let selection = range.start < selected.end && range.end > selected.start;
                        let composition = marked.as_ref().is_some_and(|marked| {
                            range.start < marked.end && range.end > marked.start
                        });
                        (selection || composition).then_some((
                            range,
                            gpui::HighlightStyle {
                                background_color: selection.then_some(rgba(0x5796ff66).into()),
                                underline: composition.then_some(gpui::UnderlineStyle {
                                    thickness: px(1.),
                                    color: Some(rgb(palette.accent).into()),
                                    wavy: false,
                                }),
                                ..Default::default()
                            },
                        ))
                    })
                    .collect::<Vec<_>>();
                // 占位只用于空串和尾换行的真实行布局，不进入编辑内容或复制。
                let display = if editing {
                    format!("{text}\u{200b}")
                } else if text.trim().is_empty() {
                    tr!("未识别到文字", "No text recognized").into()
                } else {
                    text.clone()
                };
                // 空状态提示不是正文，原空白文本的字节范围不能套到本地化提示上。
                let text = gpui::StyledText::new(display).with_highlights(
                    if !editing && text.trim().is_empty() {
                        Vec::new()
                    } else {
                        highlights
                    },
                );
                // Listener 在 paint 注册后才接收输入；与实际绘字共用布局，含滚动偏移。
                // 不另算行高、DPI、正文宽度或字符宽度，也不保留跨结果的布局。
                let down_layout = text.layout().clone();
                let move_layout = down_layout.clone();
                let up_layout = down_layout.clone();
                let up_out_layout = down_layout.clone();
                let paint_layout = down_layout.clone();
                let generation = self.result_selection_generation();
                let revision = self.result_editor.as_ref().map(|editor| editor.revision);
                let caret = self.result_editor.as_ref().map(|editor| editor.caret());
                let input_handler = self.result_input_handler.clone();
                let input_layout = self.result_input_layout.clone();
                let reveal_caret = self.result_reveal_caret.clone();
                let editor_scroll = self.result_editor_scroll.clone();
                let focus = self.focus.clone();
                let owner = cx.entity().downgrade();
                let prepaint_owner = owner.clone();
                let text_source = self.result_body_source(window);
                let prepaint_source = text_source.clone();
                let paint_input = self
                    .result_editor
                    .as_ref()
                    .map(|editor| editor.input.clone());
                let prepaint_input = paint_input.clone();
                let down_source = text_source.clone();
                let move_source = text_source.clone();
                let up_source = text_source.clone();
                let up_out_source = text_source.clone();
                let wheel_source = text_source.clone();
                let layout_token = Arc::new(());
                let down_token = Arc::clone(&layout_token);
                let move_token = Arc::clone(&layout_token);
                let up_token = Arc::clone(&layout_token);
                let up_out_token = Arc::clone(&layout_token);
                body.cursor(gpui::CursorStyle::IBeam)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                            view.cancel_ocr_result_click();
                            if !event.first_mouse
                                && view.result_body_event_current(
                                    down_source.as_ref(),
                                    &down_token,
                                    window,
                                )
                            {
                                let command = view.begin_result_pointer_command(window, cx);
                                if view.result_editor.is_some() && command.is_none() {
                                    cx.stop_propagation();
                                    return;
                                }
                                let changed = view.result_selection_down(
                                    generation,
                                    revision,
                                    &down_layout,
                                    event.position,
                                );
                                if let Some(command) = command {
                                    view.finish_result_key_command(command, window, cx);
                                }
                                if changed {
                                    window.focus(&view.focus);
                                    cx.notify();
                                }
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        move |view, event: &gpui::MouseMoveEvent, window, cx| {
                            if view.result_selection_dragging(generation) {
                                if view.result_body_event_current(
                                    move_source.as_ref(),
                                    &move_token,
                                    window,
                                ) {
                                    let command = view.begin_result_pointer_command(window, cx);
                                    if view.result_editor.is_some() && command.is_none() {
                                        if view.result_selection_end(generation) {
                                            cx.notify();
                                        }
                                        cx.stop_propagation();
                                        return;
                                    }
                                    let changed = view.result_selection_move(
                                        generation,
                                        revision,
                                        &move_layout,
                                        event,
                                    );
                                    if let Some(command) = command {
                                        view.finish_result_key_command(command, window, cx);
                                    }
                                    if changed {
                                        cx.notify();
                                    }
                                }
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseUpEvent, window, cx| {
                            if view.result_body_event_current(up_source.as_ref(), &up_token, window)
                            {
                                let command = view.begin_result_pointer_command(window, cx);
                                if view.result_editor.is_some() && command.is_none() {
                                    if view.result_selection_end(generation) {
                                        cx.notify();
                                    }
                                    cx.stop_propagation();
                                    return;
                                }
                                let changed = view.result_selection_up(
                                    generation,
                                    revision,
                                    &up_layout,
                                    event.position,
                                );
                                if let Some(command) = command {
                                    view.finish_result_key_command(command, window, cx);
                                }
                                if changed {
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                            }
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseUpEvent, window, cx| {
                            if view.result_body_event_current(
                                up_out_source.as_ref(),
                                &up_out_token,
                                window,
                            ) {
                                let command = view.begin_result_pointer_command(window, cx);
                                if view.result_editor.is_some() && command.is_none() {
                                    if view.result_selection_end(generation) {
                                        cx.notify();
                                    }
                                    cx.stop_propagation();
                                    return;
                                }
                                let changed = view.result_selection_up(
                                    generation,
                                    revision,
                                    &up_out_layout,
                                    event.position,
                                );
                                if let Some(command) = command {
                                    view.finish_result_key_command(command, window, cx);
                                }
                                if changed {
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                            }
                        }),
                    )
                    .on_scroll_wheel(cx.listener(move |view, _, window, _| {
                        view.cancel_ocr_result_click();
                        if wheel_source
                            .as_ref()
                            .is_some_and(|source| source.current_in(view, window))
                        {
                            view.result_selection_end(generation);
                            view.result_reveal_caret.set(false);
                            // 保留 GPUI 原正文滚动；识别面板的宿主 wheel 已拒绝改图像/选区。
                            view.invalidate_click_sequence();
                        }
                    }))
                    .child(text)
                    .child(
                        canvas(
                            move |_, window, cx| {
                                prepaint_owner.upgrade().is_some_and(|owner| {
                                    prepaint_source.as_ref().is_some_and(|source| {
                                        let view = owner.read(cx);
                                        source.current_in(view, window)
                                            && view
                                                .result_editor
                                                .as_ref()
                                                .map(|editor| editor.input.clone())
                                                == prepaint_input
                                    })
                                })
                            },
                            move |_, prepaint_current, window, cx| {
                                if !prepaint_current {
                                    return;
                                }
                                let Some(paint_owner) = owner.upgrade() else {
                                    return;
                                };
                                let view = paint_owner.read(cx);
                                if view
                                    .result_editor
                                    .as_ref()
                                    .map(|editor| editor.input.clone())
                                    != paint_input
                                {
                                    return;
                                }
                                let scroll = editor_scroll.offset();
                                if !view.publish_result_body_layout(
                                    text_source.as_ref(),
                                    &layout_token,
                                    paint_layout.bounds(),
                                    [f32::from(scroll.x), f32::from(scroll.y)],
                                    window,
                                ) {
                                    return;
                                }
                                if !editing {
                                    return;
                                }
                                let ready = window.is_window_active()
                                    && focus.is_focused(window)
                                    && view.result_editor_ready()
                                    && view.result_selection_generation() == generation
                                    && view.result_editor.as_ref().map(|editor| editor.revision)
                                        == revision;
                                if !ready {
                                    return;
                                }
                                // 仅输入或键导航请求显示光标；手动滚动不被逐帧拉回。
                                if reveal_caret.replace(false) {
                                    if let Some(position) = caret
                                        .and_then(|caret| paint_layout.position_for_index(caret))
                                    {
                                        let viewport = editor_scroll.bounds();
                                        let offset = editor_scroll.offset();
                                        let bottom = position.y + paint_layout.line_height();
                                        let delta = if position.y < viewport.top() {
                                            viewport.top() - position.y
                                        } else if bottom > viewport.bottom() {
                                            viewport.bottom() - bottom
                                        } else {
                                            px(0.)
                                        };
                                        let y = (offset.y + delta)
                                            .clamp(-editor_scroll.max_offset().height, px(0.));
                                        if y != offset.y {
                                            editor_scroll.set_offset(gpui::point(offset.x, y));
                                            // 偏移下一帧生效；本帧回执仍对应实际绘字位置。
                                            window.defer(cx, |window, _| window.refresh());
                                        }
                                    }
                                }
                                let Some(revision) = revision else {
                                    return;
                                };
                                *input_layout.borrow_mut() =
                                    Some(crate::text_input::ResultInputLayout {
                                        generation,
                                        revision,
                                        layout: paint_layout.clone(),
                                    });
                                if let Some(position) =
                                    caret.and_then(|caret| paint_layout.position_for_index(caret))
                                {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            position,
                                            size(px(1.5), paint_layout.line_height()),
                                        ),
                                        rgb(palette.accent),
                                    ));
                                }
                                if let Some(handler) = &input_handler {
                                    let Some(source) = text_source.clone() else {
                                        return;
                                    };
                                    let token = Arc::clone(&layout_token);
                                    let inner = handler.clone();
                                    let input_owner = owner.clone();
                                    let proxy = cx.new(|cx| {
                                        ResultInputLeaseHandler::new(
                                            inner,
                                            input_owner,
                                            source,
                                            token,
                                            window,
                                            cx,
                                        )
                                    });
                                    window.handle_input(
                                        &focus,
                                        gpui::ElementInputHandler::new(
                                            paint_layout.bounds(),
                                            proxy,
                                        ),
                                        cx,
                                    );
                                }
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
            } else {
                // 贴图继续使用专用宿主的原正文和输入合同。
                body.child(if text.trim().is_empty() {
                    tr!("未识别到文字", "No text recognized").to_owned()
                } else {
                    text.clone()
                })
            };
            root = root.child(
                div()
                    .id("ocr-result-panel")
                    .absolute()
                    .left(px(panel_x))
                    .top(px(panel_y))
                    .w(px(panel_width))
                    .max_h(px(panel_height))
                    .overflow_y_scroll()
                    .when(!docked, |panel| panel.p_4().gap_2())
                    .when(docked, |panel| {
                        panel
                            .h(px(panel_height))
                            .shadow_lg()
                            .text_color(rgb(palette.foreground))
                            .text_size(px(13.))
                    })
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .bg(rgb(palette.background))
                    .border_1()
                    .border_color(rgb(palette.border))
                    .capture_any_mouse_down(cx.listener(|view, _, _, _| {
                        view.invalidate_click_sequence();
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when(self.inline_ocr_host(), |panel| {
                        panel.on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    })
                    .when(!docked, |panel| panel.child(self.result_title.clone()))
                    .when(docked, |panel| {
                        panel.child(
                            div()
                                .h(px(44.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .px(px(14.))
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .text_size(px(14.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(self.result_title.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .text_color(rgb(palette.muted))
                                        .child(trf!("{} 行", "{} lines", line_count)),
                                ),
                        )
                    })
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, cx| {
                                let Some(owner) = result_owner.upgrade() else {
                                    return;
                                };
                                let view = owner.read(cx);
                                if view.result_output_frame() != result_frame {
                                    return;
                                }
                                if !Arc::ptr_eq(&view.ocr_projection_epoch, &projection_epoch)
                                    || view.ocr_projection(window) != result_projection
                                    || !Arc::ptr_eq(&view.current_result_body_identity(), &result_body) {
                                    return;
                                }
                                let Some(projection) = result_projection else { return; };
                                if view.inline_ocr_host() {
                                    capture_ocr_result_click::observe_result_input(view, window);
                                }
                                *painted_result_projection.borrow_mut() = Some(crate::ocr_projection::ProjectionPaint {
                                    epoch: Arc::clone(&projection_epoch), projection,
                                });
                                painted_result_output.set(
                                    (window.is_window_active() && view.focus.is_focused(window))
                                        .then_some(result_frame),
                                );
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .when(editing, |panel| {
                        panel.child(
                            div()
                                .when(docked, |hint| hint.px(px(14.)))
                                .text_size(px(11.))
                                .text_color(rgb(palette.muted))
                                .child(tr!(
                                    "编辑全文 · 复制全文使用修改后的文字",
                                    "Editing full text · Copy all uses the edited text"
                                )),
                        )
                    })
                    .when(
                        !editing
                            && self.inline_ocr_host()
                            && !text.trim().is_empty(),
                        |panel| {
                            panel.child(
                                div()
                                    .when(docked, |hint| hint.px(px(14.)))
                                    .text_size(px(11.))
                                    .text_color(rgb(palette.muted))
                                    .child(tr!(
                                        "Enter / Ctrl+C 复制所选或全文",
                                        "Enter / Ctrl+C: copy text"
                                    )),
                            )
                        },
                    )
                    .child(if docked {
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .px(px(14.))
                            .py(px(6.))
                            .child(body)
                            .into_any_element()
                    } else {
                        body.into_any_element()
                    })
                    .when_some(self.translation_section(palette), |panel, section| {
                        panel.child(
                            section
                                .pt_2()
                                .border_t_1()
                                .border_color(rgb(palette.border))
                                .when(docked, |section| section.mx(px(14.)).pb(px(8.))),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .flex_shrink_0()
                            .when(!docked, |row| row.gap_4())
                            // 主操作“复制”排在最右，其余次要操作依次向左。
                            .when(docked, |row| {
                                row.flex_row_reverse()
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(12.))
                                    .py(px(10.))
                                    .border_t_1()
                                    .border_color(rgb(palette.border))
                            })
                            .child(
                                div()
                                    .id("ocr-copy")
                                    .when(docked, primary_chip)
                                    .cursor_pointer()
                                    .child(if self.inline_ocr_host() {
                                        tr!("复制全文", "Copy all")
                                    } else {
                                        tr!("复制文字", "Copy text")
                                    })
                                    .when(self.inline_ocr_host(), |button| self.bind_result_click(
                                        button, "ocr-copy", ResultIntent::Copy, window, cx,
                                    ))
                                    .when(!self.inline_ocr_host(), |button| button.on_click(cx.listener(move |v, _, window, cx| {
                                        v.copy_recognition_result(result_frame, window, cx);
                                    }))),
                            )
                            .child(
                                div()
                                    .id("ocr-close")
                                        .when(docked, chip)
                                    .cursor_pointer()
                                    .child(if self.image_canvas.is_some() && !self.pin_mode {
                                        tr!("返回图片", "Back to image")
                                    } else { tr!("返回截图", "Back to capture") })
                                    .when(self.inline_ocr_host(), |button| self.bind_result_click(
                                        button, "ocr-close", ResultIntent::Close, window, cx,
                                    ))
                                    .when(!self.inline_ocr_host(), |button| button.on_click(cx.listener(|v, _, _, cx| {
                                        v.dismiss_recognition_result();
                                        cx.notify();
                                    }))),
                            )
                            .when(self.has_ocr_result(), |row| {
                                let table = self.render_table_copy_button(result_frame, false, cx);
                                row.child(if docked {
                                    chip(div().id("ocr-copy-table-chip")).child(table).into_any_element()
                                } else {
                                    table
                                })
                            })
                            .when(!text.trim().is_empty(), |row| {
                                let pending = self.translation_pending();
                                row.child(
                                    div()
                                        .id("ocr-translate")
                                        .when(docked, chip)
                                        .opacity(if pending { 0.45 } else { 1. })
                                        .when(!pending, |button| button.cursor_pointer())
                                        .child(if pending {
                                            tr!("正在翻译…", "Translating…")
                                        } else {
                                            tr!("翻译", "Translate")
                                        })
                                        .tooltip(|_, cx| {
                                            cx.new(|_| {
                                                crate::pin_view::TextToolHint(tr!(
                                                    "将所选文字（未选择时为全文）发送到在线翻译服务",
                                                    "Send the selected text (or all text) to the online translation service"
                                                ))
                                            })
                                            .into()
                                        })
                                        .when(self.inline_ocr_host() && !pending, |button| self.bind_result_click(
                                            button, "ocr-translate", ResultIntent::Translate, window, cx,
                                        ))
                                        .when(!self.inline_ocr_host(), |button| button.on_click(cx.listener(move |view, _, _, cx| {
                                            view.translate_recognition_result(result_frame, cx);
                                        }))),
                                )
                            })
                            .when(
                                self.current_translation().is_some_and(|translation| {
                                    matches!(
                                        translation.state,
                                        crate::translate::TranslationState::Done(_)
                                    )
                                }),
                                |row| {
                                    let ticket = self.translation_copy_ticket();
                                    row.child(
                                        div()
                                            .id("ocr-copy-translation")
                                        .when(docked, chip)
                                            .cursor_pointer()
                                            .child(tr!("复制译文", "Copy translation"))
                                            .on_mouse_down(MouseButton::Left, cx.listener(move |view, event, window, cx| {
                                                view.begin_translation_copy_pointer(ticket.clone(), event, window, cx);
                                                cx.stop_propagation();
                                            }))
                                            .on_click(cx.listener(|view, event, window, cx| {
                                                view.copy_translation_pointer(event, window, cx);
                                            })),
                                    )
                                },
                            )
                            .when(self.has_ocr_result(), |row| {
                                let enabled = self.result_retry_available();
                                row.child(
                                    div()
                                        .id("ocr-retry")
                                        .when(docked, chip)
                                        .cursor_pointer()
                                        .opacity(if enabled { 1. } else { 0.3 })
                                        .child(if self.result_retry_busy() {
                                            tr!("正在重新识别…", "Recognizing again…")
                                        } else {
                                            tr!("重新识别", "Recognize again")
                                        })
                                        .when(self.inline_ocr_host() && enabled, |button| self.bind_result_click(
                                            button, "ocr-retry", ResultIntent::Retry, window, cx,
                                        ))
                                        .when(!self.inline_ocr_host(), |button| button.on_click(cx.listener(move |view, _, window, cx| {
                                            if enabled {
                                                view.retry_recognition_result(window, cx);
                                            }
                                        }))),
                                )
                            })
                            .when(self.inline_ocr_host(), |row| {
                                row.child(
                                    self.bind_result_click(div()
                                        .id("ocr-edit-fulltext")
                                        .when(docked, chip)
                                        .cursor_pointer()
                                        .child(if editing {
                                            tr!("收起全文", "Collapse full text")
                                        } else {
                                            tr!("编辑全文", "Edit full text")
                                        }), "ocr-edit-fulltext", ResultIntent::Edit(editing), window, cx),
                                )
                            }),
                    ),
            );
        }
        let idle_hint = self.status.is_empty()
            && self.image_canvas.is_none()
            && !self.editor.has_arrow_draft()
            && !self.editor.has_polyline()
            && !self.auto_copy_recovery_is_current();
        if idle_hint
            && selected.is_none()
            && self.pointer_down_start.is_none()
            && self.key_hints_visible()
        {
            // 空闲快捷键卡片放当前屏左下角；指针靠近或吸附框相交时先移到左上，两处都被占才隐藏。
            // 只列框选前此刻有效的键，完整列表与版本号在 Shift+F1 帮助里；F1 显隐并记住。
            let current = self.ui_monitor(self.source.current);
            let rows: [(&[&str], &str, bool); 10] = [
                (
                    &[tr!("拖动", "Drag")],
                    tr!("框选区域（可跨屏）", "Select area (across screens)"),
                    true,
                ),
                (
                    &[tr!("单击", "Click")],
                    tr!("选择吸附窗口", "Pick snapped window"),
                    true,
                ),
                (
                    &["Ctrl+A"],
                    tr!("当前屏 / 全部屏幕", "Current / all screens"),
                    false,
                ),
                (
                    &["Alt+A"],
                    tr!("指针所在屏幕", "Screen under pointer"),
                    false,
                ),
                (
                    &[tr!("方向键", "Arrows")],
                    tr!("微调指针 1 像素", "Nudge pointer 1 px"),
                    false,
                ),
                (
                    &["Shift"],
                    tr!("切换颜色格式", "Switch color format"),
                    false,
                ),
                (&["C"], tr!("复制放大镜颜色", "Copy magnifier color"), false),
                (&["R"], tr!("识别悬停区域文字", "Read hovered text"), false),
                (&["F5"], tr!("刷新底图", "Refresh desktop"), false),
                (&["Esc"], tr!("取消截图", "Cancel capture"), false),
            ];
            let recipe = self
                .quick_action
                .recipe
                .as_ref()
                .map(|recipe| recipe.summary());
            let (w, margin) = (288., 20.);
            // 行高 22、行距 4、上下内边距 8＋分隔线；仅用于避让判断。
            let h = 26. * (rows.len() + 2) as f32 + 18. + if recipe.is_some() { 22. } else { 0. };
            let at = |bottom: bool| crate::geometry::Rect {
                x: current.x + (margin * scale) as u32,
                y: if bottom {
                    (current.y + current.height).saturating_sub(((margin + h) * scale) as u32)
                } else {
                    current.y + (margin * scale) as u32
                },
                width: (w * scale) as u32,
                height: (h * scale) as u32,
            };
            let blocked = |rect: crate::geometry::Rect| {
                let guard = crate::geometry::Rect {
                    x: rect.x.saturating_sub((24. * scale) as u32),
                    y: rect.y.saturating_sub((24. * scale) as u32),
                    width: rect.width + (48. * scale) as u32,
                    height: rect.height + (48. * scale) as u32,
                };
                guard.contains(self.pointer)
                    || hover
                        .as_ref()
                        .is_some_and(|t| t.rect.intersection(rect).is_some())
            };
            let place = [at(true), at(false)]
                .into_iter()
                .find(|rect| !blocked(*rect));
            if let Some(px_rect) = place {
                let chip = |key: &str, mouse: bool| {
                    let chip = div()
                        .h(px(20.))
                        .px_1p5()
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .text_xs()
                        .child(key.to_string());
                    if mouse {
                        chip.border_1()
                            .border_dashed()
                            .border_color(rgba(palette.hint_muted))
                    } else {
                        chip.bg(rgba(palette.hint_chip))
                    }
                };
                let row = |keys: &[&str], label: &str, mouse: bool| {
                    div()
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .children(keys.iter().map(|k| chip(k, mouse))),
                        )
                        .child(label.to_string())
                };
                root = root.child(
                    div()
                        .absolute()
                        .left(px(px_rect.x as f32 / scale))
                        .top(px(px_rect.y as f32 / scale))
                        .w(px(w))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgba(palette.hint_border))
                        .bg(rgba(palette.hint_bg))
                        .text_color(rgba(palette.hint_fg))
                        .text_sm()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(
                            rows.iter()
                                .map(|(keys, label, mouse)| row(keys, label, *mouse)),
                        )
                        .child(
                            div()
                                .pt_1()
                                .border_t_1()
                                .border_color(rgba(palette.hint_border))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(row(
                                    &["F1"],
                                    tr!("显示或隐藏快捷键提示", "Show or hide shortcut hints"),
                                    false,
                                ))
                                .child(row(
                                    &["Shift+F1"],
                                    tr!("完整快捷键帮助", "All shortcuts"),
                                    false,
                                )),
                        )
                        .when_some(recipe, |hint, recipe| {
                            hint.child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(palette.hint_muted))
                                    .child(recipe),
                            )
                        }),
                );
            }
        }
        // 长图剪辑的状态写在底部状态栏。
        if !clipped
            && ((selected.is_none() && !idle_hint)
                || !(self.status.is_empty()
                    || selection_ocr_busy && self.ocr_progress_replaces_status())
                || self.editor.has_pending_drawing()
                || self.auto_copy_recovery_is_current())
        {
            let current = self.ui_monitor(self.source.current);
            // 截图提示放当前屏左下角；左下角带与选区／鼠标重叠时回到左上角，避免遮挡目标。
            let band = crate::geometry::Rect {
                x: current.x,
                y: (current.y + current.height).saturating_sub((140. * scale) as u32),
                width: ((760. * scale) as u32).min(current.width),
                height: ((140. * scale) as u32).min(current.height),
            };
            let bottom = self.image_canvas.is_none()
                && !band.contains(self.pointer)
                && selected.is_none_or(|rect| rect.intersection(band).is_none());
            let notice = div()
                .px_4()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgba(palette.hint_border))
                    .bg(rgba(palette.hint_bg))
                    .text_color(rgba(palette.hint_fg))
                    .child(if self.status.is_empty() && self.editor.has_arrow_draft() {
                        tr!("单击确定箭头终点 · Shift 约束角度 · Esc / 右键取消", "Click to set the arrow end · Shift constrains angle · Esc / right-click cancels").to_string()
                    } else if self.status.is_empty() && self.editor.has_polyline() {
                        tr!("单击增加折线节点 · Shift 约束角度 · Enter / 右键完成", "Click to add polyline points · Shift constrains angle · Enter / right-click finishes").to_string()
                    } else if self.status.is_empty() {
                        trf!("{} {} · {} 屏幕    跨屏拖拽 · Ctrl+A 当前/全部 · Esc 取消", "{} {} · {} screens    drag across screens · Ctrl+A current/all · Esc cancels",
                            crate::brand::NAME,
                            env!("CARGO_PKG_VERSION"),
                            self.source.screen_count()
                        )
                    } else {
                        self.status.clone()
                    })
                    .when(self.auto_copy_recovery_is_current(), |notice| {
                        // 保持失败态重试语义可见，颜色等其它反馈不能静默改写 Enter。
                        notice.child(div().mt_1().child(tr!(
                            "Enter 重试全文 · Ctrl+C 复制文字",
                            "Enter retries all · Ctrl+C copies text"
                        )))
                    });
            let left = current.x as f32 / scale + 20.;
            root = root.child(if bottom {
                // 用当前屏大小的透明容器底对齐，提示行数变化时无需估算高度。
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(current.y as f32 / scale))
                    .h(px(current.height as f32 / scale - 20.))
                    .max_w(px(current.width as f32 / scale - 40.))
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(notice)
            } else {
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(if self.image_canvas.is_some() {
                        64.
                    } else {
                        current.y as f32 / scale + 20.
                    }))
                    .child(notice)
            });
        }
        if self.image_canvas.is_some() && !self.pin_mode && !clipped {
            root = root.child(self.image_controls(cx));
        }
        if !self.pin_mode && self.image_canvas.is_none() {
            // 同一 render 的遮罩孔、框和此 receipt 共用 hover；paint 后仍需 successful Present。
            let painted_preview = self.painted_preview.clone();
            let painted_selection = self.painted_selection.clone();
            let shown_selection = selected.or_else(|| hover.as_ref().map(|target| target.rect));
            let shown_preview = hover.as_ref().map(|target| target.rect).filter(|rect| {
                self.screenshot_preview_confirm_ready(*rect)
                    || (self.manual_preview == Some(*rect)
                        && self.manual_preview_click == Some(*rect))
            });
            let preview_canvas = self.source.full();
            root = root.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let h = crate::gpui_capture::hwnd(window);
                        let before_present = unsafe {
                            windows::Win32::UI::WindowsAndMessaging::GetPropW(
                                h,
                                windows::core::w!("RustShot.GpuiPresentSerial"),
                            )
                        }
                        .0 as usize;
                        let size = unsafe {
                            windows::Win32::UI::WindowsAndMessaging::GetPropW(
                                h,
                                windows::core::w!("RustShot.GpuiPresentSize"),
                            )
                        }
                        .0 as usize as u32;
                        let previous_presented = painted_preview
                            .get()
                            .and_then(|old| old.presented(preview_canvas, before_present, size));
                        painted_preview.set(Some(crate::preview_middle_pin::PaintedPreview {
                            rect: shown_preview,
                            previous_presented,
                            before_present,
                        }));
                        let previous_selection = painted_selection
                            .get()
                            .and_then(|old| old.presented(preview_canvas, before_present, size));
                        painted_selection.set(Some(crate::preview_middle_pin::PaintedPreview {
                            rect: shown_selection,
                            previous_presented: previous_selection,
                            before_present,
                        }));
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        }
        if self.toolbar_panel == crate::toolbar_layout::ToolbarPanel::Help
            && (selected.is_none() || !toolbar_visible)
        {
            let monitor = crate::toolbar_layout::PanelRect::from_physical(
                self.ui_monitor(self.toolbar_monitor),
                window.scale_factor(),
            );
            let width = (monitor.width - 24.).clamp(1., 520.);
            let height = (monitor.height - 24.).clamp(1., 520.);
            let popup = crate::toolbar_layout::PanelRect {
                x: monitor.x + (monitor.width - width) / 2.,
                y: monitor.y + (monitor.height - height) / 2.,
                width,
                height,
            };
            root = root.child(self.help_popup(popup, cx));
        }
        root
    }
}

impl CaptureView {
    /// 结果面板与原位译文卡片共用的“译文”标题和正文；没有翻译时为 None。
    pub(crate) fn translation_section(
        &self,
        palette: crate::capture_toolbar::Palette,
    ) -> Option<gpui::Div> {
        use crate::translate::TranslationState;
        let translation = self.current_translation()?;
        let (content, muted) = match &translation.state {
            TranslationState::Pending if translation.partial().is_empty() => {
                (tr!("正在翻译…", "Translating…").to_owned(), true)
            }
            TranslationState::Pending => (format!("{}…", translation.partial()), false),
            TranslationState::Done(text) => (text.clone(), false),
            TranslationState::Failed(error) => (error.clone(), true),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(tr!("译文", "Translation")),
                )
                .child(
                    div()
                        .when(muted, |text| text.text_color(rgb(palette.muted)))
                        .child(content),
                ),
        )
    }
}
