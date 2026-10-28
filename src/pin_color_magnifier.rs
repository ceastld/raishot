//! 贴图取色的只读浮窗：仅持有最多 15×9 的 CPU 小片，不上传图片纹理。
use crate::{gpui_capture::hwnd, pin_color_sample::ColorPatch, pin_model::Placement, Result};
use gpui::{
    canvas, div, fill, outline, point, prelude::*, px, rgb, rgba, size, App, BorderStyle, Bounds,
    Context, Render, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        WindowsAndMessaging::*,
    },
};

const CELL: f32 = 12.;
const GRID_WIDTH: f32 = 180.;
const GRID_HEIGHT: f32 = 108.;
const CARD_HEIGHT: f32 = 244.;

pub(crate) struct Packet {
    pub patch: ColorPatch,
    pub color: String,
    pub coords: String,
    pub cursor: (i32, i32),
}

pub(crate) struct PinColorMagnifier {
    packet: Arc<Packet>,
    owner: usize,
    placement: Placement,
    generation: u64,
    painted: Rc<Cell<u64>>,
    visible: bool,
    native_pending: bool,
    native_ready: bool,
    native_visible: bool,
    native_task: Option<gpui::Task<()>>,
}

fn dimensions(packet: &Packet) -> (f32, f32) {
    let width = (GRID_WIDTH + 4.)
        .max(packet.coords.chars().count() as f32 * 7. + 24.)
        .max(packet.color.chars().count() as f32 * 7. + 46.);
    (width, CARD_HEIGHT)
}

fn validate(packet: &Packet) -> Result<()> {
    let image = &packet.patch.image;
    let (width, height) = (image.width, image.height);
    if width == 0
        || height == 0
        || width > 15
        || height > 9
        || packet.patch.center.0 >= width
        || packet.patch.center.1 >= height
        || image.bgra.len() != width as usize * height as usize * 4
        || packet.color.len() > 128
        || packet.coords.len() > 128
    {
        return Err("取色放大镜数据不完整".into());
    }
    Ok(())
}

/// 输入输出均为物理屏幕坐标；布局尺寸和间隔才按目标显示器 DPI 转换。
fn placement(cursor: (i32, i32), work: Placement, dpi: f32, logical: (f32, f32)) -> Placement {
    let dpi = if dpi.is_finite() && dpi > 0. {
        f64::from(dpi)
    } else {
        1.
    };
    let side = |logical: f32, available: u32| {
        ((f64::from(logical) * dpi)
            .round()
            .clamp(1., i32::MAX as f64) as u32)
            .min(available.max(1))
    };
    let width = side(logical.0, work.width);
    let height = side(logical.1, work.height);
    let gap = (20. * dpi).round().clamp(1., i32::MAX as f64) as i64;
    let resolve = |point: i32, start: i32, available: u32, side: u32| {
        let start = i64::from(start);
        let end = start + i64::from(available.max(1));
        let point = i64::from(point);
        let mut result = point + gap;
        if result + i64::from(side) > end {
            result = point - gap - i64::from(side);
        }
        result
            .clamp(start, end - i64::from(side))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    Placement {
        x: resolve(cursor.0, work.x, work.width, width),
        y: resolve(cursor.1, work.y, work.height, height),
        width,
        height,
    }
}

fn screen_placement(packet: &Packet) -> Placement {
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    let monitor = unsafe {
        MonitorFromPoint(
            windows::Win32::Foundation::POINT {
                x: packet.cursor.0,
                y: packet.cursor.1,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / 96.
    } else {
        1.
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Placement {
            x: info.rcWork.left,
            y: info.rcWork.top,
            width: (info.rcWork.right - info.rcWork.left).max(1) as u32,
            height: (info.rcWork.bottom - info.rcWork.top).max(1) as u32,
        }
    } else {
        crate::pin_native::work_area(packet.cursor)
    };
    placement(packet.cursor, work, dpi, dimensions(packet))
}

fn owner_alive(owner: HWND) -> bool {
    let mut pid = 0;
    unsafe {
        IsWindow(Some(owner)).as_bool()
            && IsWindowVisible(owner).as_bool()
            && GetWindowThreadProcessId(owner, Some(&mut pid)) != 0
            && pid == std::process::id()
    }
}

fn bound_window(h: HWND, owner: HWND) -> bool {
    let mut pid = 0;
    let required_style = (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0 as isize;
    unsafe {
        owner_alive(owner)
            && IsWindow(Some(h)).as_bool()
            && GetWindowThreadProcessId(h, Some(&mut pid)) != 0
            && pid == std::process::id()
            && GetWindowLongPtrW(h, GWLP_HWNDPARENT) == owner.0 as isize
            && !IsWindowEnabled(h).as_bool()
            && GetWindowLongPtrW(h, GWL_EXSTYLE) & required_style == required_style
    }
}

pub(crate) fn show(
    existing: Option<WindowHandle<PinColorMagnifier>>,
    packet: Packet,
    owner: HWND,
    cx: &mut App,
) -> Result<WindowHandle<PinColorMagnifier>> {
    let result = show_inner(existing, packet, owner, cx);
    if result.is_err() {
        close(existing, cx);
    }
    result
}

fn show_inner(
    existing: Option<WindowHandle<PinColorMagnifier>>,
    packet: Packet,
    owner: HWND,
    cx: &mut App,
) -> Result<WindowHandle<PinColorMagnifier>> {
    validate(&packet)?;
    if !owner_alive(owner) {
        return Err("取色放大镜宿主已关闭或隐藏".into());
    }
    let p = screen_placement(&packet);
    let reusable = existing.filter(|handle| {
        handle
            .update(cx, |v, _, _| v.visible && v.owner == owner.0 as usize)
            .unwrap_or(false)
    });
    if let Some(handle) = reusable {
        handle.update(cx, |v, w, cx| {
            v.packet = Arc::new(packet);
            v.placement = p;
            v.generation = v.generation.wrapping_add(1);
            v.schedule_native(w, cx);
            cx.notify();
        })?;
        return Ok(handle);
    }
    close(existing, cx);
    let (display_id, bounds) = crate::pin_native::initial_pin_bounds(p, cx);
    let opened = Rc::new(Cell::new(None));
    let opened_for_constructor = opened.clone();
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id,
            kind: gpui::WindowKind::PopUp,
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            focus: false,
            show: false,
            is_resizable: false,
            is_minimizable: false,
            is_movable: false,
            ..Default::default()
        },
        move |w, cx| {
            opened_for_constructor.set(w.window_handle().downcast::<PinColorMagnifier>());
            w.set_window_title(brand_title!("取色放大镜", "Color magnifier"));
            w.disable_focus();
            let view = cx.new(|_| PinColorMagnifier {
                packet: Arc::new(packet),
                owner: owner.0 as usize,
                placement: p,
                generation: 1,
                painted: Rc::new(Cell::new(0)),
                visible: true,
                native_pending: false,
                native_ready: false,
                native_visible: false,
                native_task: None,
            });
            w.on_window_should_close(cx, |_, _| false);
            view.update(cx, |v, cx| v.schedule_native(w, cx));
            view
        },
    );
    match result {
        Ok(handle) => Ok(handle),
        Err(error) => {
            close(opened.get(), cx);
            Err(error.into())
        }
    }
}

pub(crate) fn close(handle: Option<WindowHandle<PinColorMagnifier>>, cx: &mut App) {
    if let Some(handle) = handle {
        let _ = handle.update(cx, |v, w, _| {
            v.visible = false;
            v.generation = v.generation.wrapping_add(1);
            v.native_pending = false;
            drop(v.native_task.take());
            w.remove_window();
        });
    }
}

impl PinColorMagnifier {
    fn schedule_native(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.native_pending || !self.visible {
            return;
        }
        self.native_pending = true;
        let handle = window
            .window_handle()
            .downcast::<Self>()
            .expect("取色放大镜窗口类型");
        let mut async_cx = cx.to_async();
        self.native_task = Some(cx.foreground_executor().spawn(async move {
            let outcome: Result<()> = async {
                loop {
                    let request = handle.update(&mut async_cx, |v, w, _| {
                        v.visible.then_some((
                            hwnd(w).0 as usize,
                            v.owner,
                            v.generation,
                            v.placement,
                            !v.native_ready,
                            v.native_visible,
                        ))
                    })?;
                    let Some((native, owner, generation, p, initialize, was_visible)) = request
                    else {
                        return Ok(());
                    };
                    let h = HWND(native as *mut _);
                    let owner = HWND(owner as *mut _);
                    if !owner_alive(owner) {
                        return Err("取色放大镜宿主不可用".into());
                    }
                    if initialize {
                        unsafe {
                            crate::remote_display::exclude_from_capture(h)?;
                            SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                            SetWindowLongPtrW(
                                h,
                                GWL_EXSTYLE,
                                GetWindowLongPtrW(h, GWL_EXSTYLE)
                                    | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0
                                        as isize,
                            );
                            SetWindowLongPtrW(h, GWLP_HWNDPARENT, owner.0 as isize);
                            let _ = EnableWindow(h, false);
                        }
                    }
                    if !bound_window(h, owner) {
                        return Err("取色放大镜窗口身份改变".into());
                    }
                    let baseline =
                        unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSerial")) }.0
                            as usize;
                    let owner_topmost = unsafe { GetWindowLongPtrW(owner, GWL_EXSTYLE) }
                        & WS_EX_TOPMOST.0 as isize
                        != 0;
                    let popup_topmost = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) }
                        & WS_EX_TOPMOST.0 as isize
                        != 0;
                    if initialize
                        || owner_topmost != popup_topmost
                        || !crate::pin_native::rect(h).is_ok_and(|rect| rect == p)
                    {
                        // 此处不在 App/Window 借用内；尺寸与 DPI 回调可安全更新 GPUI。
                        unsafe {
                            SetWindowPos(
                                h,
                                Some(if owner_topmost {
                                    HWND_TOPMOST
                                } else {
                                    HWND_NOTOPMOST
                                }),
                                p.x,
                                p.y,
                                p.width as i32,
                                p.height as i32,
                                SWP_NOACTIVATE
                                    | SWP_NOOWNERZORDER
                                    | if initialize {
                                        SWP_HIDEWINDOW | SWP_FRAMECHANGED
                                    } else {
                                        SET_WINDOW_POS_FLAGS(0)
                                    },
                            )?;
                            // WM_DPICHANGED 的推荐位置可能覆盖目标物理范围，只校正一次。
                            if !crate::pin_native::rect(h).is_ok_and(|rect| rect == p) {
                                SetWindowPos(
                                    h,
                                    None,
                                    p.x,
                                    p.y,
                                    p.width as i32,
                                    p.height as i32,
                                    SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
                                )?;
                            }
                        }
                    }
                    handle.update(&mut async_cx, |v, w, cx| {
                        v.native_ready = true;
                        w.refresh();
                        cx.notify();
                    })?;
                    let started = Instant::now();
                    loop {
                        let ready = handle.update(&mut async_cx, |v, w, _| {
                            if !v.visible || hwnd(w) != h || v.owner != owner.0 as usize {
                                return None;
                            }
                            if v.generation != generation {
                                return Some((false, true));
                            }
                            let serial = unsafe {
                                GetPropW(h, windows::core::w!("RustShot.GpuiPresentSerial"))
                            }
                            .0 as usize;
                            let packed = unsafe {
                                GetPropW(h, windows::core::w!("RustShot.GpuiPresentSize"))
                            }
                            .0 as usize as u32;
                            let viewport = w.viewport_size();
                            let mut client = RECT::default();
                            let matches = unsafe { GetClientRect(h, &mut client) }.is_ok()
                                && client.right - client.left == p.width as i32
                                && client.bottom - client.top == p.height as i32;
                            Some((
                                v.painted.get() == generation
                                    && serial != 0
                                    && serial != baseline
                                    && packed == ((p.width << 16) | p.height)
                                    && matches
                                    && (f32::from(viewport.width) * w.scale_factor()
                                        - p.width as f32)
                                        .abs()
                                        < 1.
                                    && (f32::from(viewport.height) * w.scale_factor()
                                        - p.height as f32)
                                        .abs()
                                        < 1.
                                    && crate::pin_native::rect(h).is_ok_and(|rect| rect == p),
                                false,
                            ))
                        })?;
                        match ready {
                            None => return Ok(()),
                            Some((_, true)) => break,
                            Some((true, false)) => {
                                if !bound_window(h, owner) {
                                    return Err("取色放大镜显示前宿主已失效".into());
                                }
                                let current = handle.update(&mut async_cx, |v, w, _| {
                                    v.visible
                                        && v.generation == generation
                                        && v.owner == owner.0 as usize
                                        && hwnd(w) == h
                                })?;
                                if !current {
                                    break;
                                }
                                if !was_visible {
                                    unsafe {
                                        SetWindowPos(
                                            h,
                                            None,
                                            0,
                                            0,
                                            0,
                                            0,
                                            SWP_NOACTIVATE
                                                | SWP_NOZORDER
                                                | SWP_NOOWNERZORDER
                                                | SWP_NOMOVE
                                                | SWP_NOSIZE
                                                | SWP_SHOWWINDOW,
                                        )?;
                                    }
                                }
                                let repeat = handle.update(&mut async_cx, |v, _, _| {
                                    v.native_visible = true;
                                    let repeat = v.generation != generation;
                                    if !repeat {
                                        v.native_pending = false;
                                    }
                                    repeat
                                })?;
                                if repeat {
                                    break;
                                }
                                return Ok(());
                            }
                            Some((false, false)) if started.elapsed() >= Duration::from_secs(2) => {
                                return Err("取色放大镜未获得正确尺寸的新首帧".into());
                            }
                            Some((false, false)) => {}
                        }
                        unsafe {
                            PostMessageW(Some(h), WM_PAINT, WPARAM(0), LPARAM(0))?;
                        }
                        async_cx
                            .background_executor()
                            .timer(Duration::from_millis(8))
                            .await;
                    }
                }
            }
            .await;
            if let Err(error) = outcome {
                let _ = handle.update(&mut async_cx, |v, w, _| {
                    if v.visible {
                        eprintln!("取色放大镜：{error}");
                    }
                    v.visible = false;
                    v.native_pending = false;
                    w.remove_window();
                });
            }
        }));
    }
}

impl Render for PinColorMagnifier {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(w, cx);
        let palette = crate::theme::palette(cx);
        let packet = self.packet.clone();
        let width = dimensions(&packet).0;
        let painted = self.painted.clone();
        let generation = self.generation;
        let i = ((self.packet.patch.center.1 * self.packet.patch.image.width
            + self.packet.patch.center.0)
            * 4) as usize;
        let pixel = &self.packet.patch.image.bgra[i..i + 4];
        let swatch = rgba(
            (u32::from(pixel[2]) << 24)
                | (u32::from(pixel[1]) << 16)
                | (u32::from(pixel[0]) << 8)
                | u32::from(pixel[3]),
        );
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .font_family("Consolas")
            .text_size(px(12.))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, w, _| {
                        let image = &packet.patch.image;
                        let patch_width = image.width as f32 * CELL;
                        let patch_height = image.height as f32 * CELL;
                        let start = bounds.origin
                            + point(
                                px((width - patch_width) / 2.),
                                px(2. + (GRID_HEIGHT - patch_height) / 2.),
                            );
                        for y in 0..image.height {
                            for x in 0..image.width {
                                let b = Bounds::new(
                                    start + point(px(x as f32 * CELL), px(y as f32 * CELL)),
                                    size(px(CELL), px(CELL)),
                                );
                                w.paint_quad(fill(
                                    b,
                                    rgb(if (x + y) % 2 == 0 { 0xe0e0e0 } else { 0xb8b8b8 }),
                                ));
                                let i = ((y * image.width + x) * 4) as usize;
                                let p = &image.bgra[i..i + 4];
                                w.paint_quad(fill(
                                    b,
                                    rgba(
                                        (u32::from(p[2]) << 24)
                                            | (u32::from(p[1]) << 16)
                                            | (u32::from(p[0]) << 8)
                                            | u32::from(p[3]),
                                    ),
                                ));
                            }
                        }
                        let grid = Bounds::new(start, size(px(patch_width), px(patch_height)));
                        for x in 1..image.width {
                            w.paint_quad(fill(
                                Bounds::new(
                                    start + point(px(x as f32 * CELL), px(0.)),
                                    size(px(1.), px(patch_height)),
                                ),
                                rgba(0x80808066),
                            ));
                        }
                        for y in 1..image.height {
                            w.paint_quad(fill(
                                Bounds::new(
                                    start + point(px(0.), px(y as f32 * CELL)),
                                    size(px(patch_width), px(1.)),
                                ),
                                rgba(0x80808066),
                            ));
                        }
                        w.paint_quad(outline(grid, rgb(0x808080), BorderStyle::Solid));
                        let center = Bounds::new(
                            start
                                + point(
                                    px(packet.patch.center.0 as f32 * CELL),
                                    px(packet.patch.center.1 as f32 * CELL),
                                ),
                            size(px(CELL), px(CELL)),
                        );
                        w.paint_quad(outline(
                            Bounds::new(
                                center.origin - point(px(1.), px(1.)),
                                center.size + size(px(2.), px(2.)),
                            ),
                            rgb(0x000000),
                            BorderStyle::Solid,
                        ));
                        w.paint_quad(outline(center, rgb(0xffffff), BorderStyle::Solid));
                        painted.set(generation);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .w(px(width))
                .h(px(GRID_HEIGHT + 4.)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(GRID_HEIGHT + 10.))
                    .left_0()
                    .w(px(width))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.))
                    .child(self.packet.coords.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .child(
                                div()
                                    .w(px(14.))
                                    .h(px(14.))
                                    .border_1()
                                    .border_color(rgb(palette.border))
                                    .bg(rgb(0xe0e0e0))
                                    .child(div().size_full().bg(swatch)),
                            )
                            .child(self.packet.color.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(tr!("Shift 格式 · P 坐标", "Shift format · P coordinates")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(tr!("C 复制 · Alt/Esc 退出", "C copy · Alt/Esc exit")),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::placement;
    use crate::pin_model::Placement;

    fn contained(p: Placement, work: Placement) {
        assert!(p.x >= work.x && p.y >= work.y);
        assert!(i64::from(p.x) + i64::from(p.width) <= i64::from(work.x) + i64::from(work.width));
        assert!(i64::from(p.y) + i64::from(p.height) <= i64::from(work.y) + i64::from(work.height));
    }

    #[test]
    fn four_corners_flip_and_clamp_in_negative_monitor_work_area() {
        let work = Placement {
            x: -1600,
            y: -900,
            width: 1600,
            height: 900,
        };
        for cursor in [(-1600, -900), (-1, -900), (-1600, -1), (-1, -1)] {
            let p = placement(cursor, work, 1., (184., 244.));
            assert_eq!((p.width, p.height), (184, 244));
            contained(p, work);
        }
        assert_eq!(placement((-1, -1), work, 1., (184., 244.)).x, -205);
    }

    #[test]
    fn target_dpi_scales_card_and_gap_without_scaling_screen_origin() {
        let work = Placement {
            x: -2560,
            y: 120,
            width: 2560,
            height: 1440,
        };
        let p = placement((-2500, 160), work, 1.5, (184., 244.));
        assert_eq!((p.x, p.y, p.width, p.height), (-2470, 190, 276, 366));
        contained(p, work);
        let large = placement((-1, 1559), work, 2., (184., 244.));
        assert_eq!((large.width, large.height), (368, 488));
        contained(large, work);
    }

    #[test]
    fn tiny_work_area_and_invalid_dpi_keep_positive_contained_bounds() {
        let work = Placement {
            x: -12,
            y: -7,
            width: 9,
            height: 5,
        };
        for dpi in [0., -1., f32::NAN, f32::INFINITY, 4.] {
            let p = placement((i32::MAX, i32::MIN), work, dpi, (184., 244.));
            assert_eq!(p, work);
            contained(p, work);
        }
    }
}
