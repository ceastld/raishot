//! 缩放偏好原生专项：只使用本进程合成贴图、GPUI 帧及独立 QA 配置。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    capture_view::Action,
    document::Object,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_menu::PinMenu,
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    pin_zoom::ZoomAnchor,
    preferences::Preferences,
    settings_view::{SettingsTab, SettingsView},
    viewport::Viewport,
    Result,
};
use gpui::{App, AsyncApp, Window, WindowHandle};
use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::ClientToScreen,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const FOLDER: &str = "target/qa/pin-zoom-preferences";
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const WHEEL_MARKER: usize = 0x4000;
const WHEEL_SUBCLASS: usize = 0x52535a50;
const MK_CONTROL_QA: usize = 0x0008;
const POINTER: Point = Point { x: 83., y: 227. };

fn ensure(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn fixture() -> Image {
    Image {
        width: WIDTH,
        height: HEIGHT,
        bgra: [50, 120, 180, 255].repeat((WIDTH * HEIGHT) as usize),
        readback_ms: 0.,
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(50))
        .await;
}
fn input(w: &Window) -> Input {
    Input {
        handle: hwnd(w).0 as usize,
        scale: w.scale_factor(),
    }
}
fn owned(target: Input) -> Result<()> {
    let h = HWND(target.handle as *mut _);
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut process));
    }
    ensure(
        unsafe { IsWindow(Some(h)) }.as_bool() && process == std::process::id(),
        "缩放偏好 QA 输入目标不是本进程自有窗口",
    )
}

/// GPUI Windows 从 GetKeyState 而非 WM_MOUSEWHEEL.wParam 读取 Ctrl。
/// 仅在带 QA 标记的自有窗口消息分发期间覆盖本线程键盘状态；不使用 SendInput，
/// 不改变全局异步键盘状态，调用结束立即恢复原值。
struct KeyboardRestore([u8; 256]);
impl Drop for KeyboardRestore {
    fn drop(&mut self) {
        unsafe {
            let _ = SetKeyboardState(&self.0);
        }
    }
}
unsafe extern "system" fn wheel_modifiers(
    h: HWND,
    message: u32,
    mut wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(h, Some(wheel_modifiers), id);
    }
    if message != WM_MOUSEWHEEL || wp.0 & WHEEL_MARKER == 0 {
        return DefSubclassProc(h, message, wp, lp);
    }
    let mut original = [0_u8; 256];
    if GetKeyboardState(&mut original).is_err() {
        // 不可静默把 Ctrl 专项当成普通滚轮通过；随后透明度契约会失败。
        return LRESULT(0);
    }
    let mut keys = original;
    for key in [
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ] {
        keys[key.0 as usize] &= 0x7f;
    }
    if wp.0 & MK_CONTROL_QA != 0 {
        keys[VK_CONTROL.0 as usize] |= 0x80;
        keys[VK_LCONTROL.0 as usize] |= 0x80;
    }
    if SetKeyboardState(&keys).is_err() {
        return LRESULT(0);
    }
    let _restore = KeyboardRestore(original);
    wp.0 &= !WHEEL_MARKER;
    DefSubclassProc(h, message, wp, lp)
}
fn isolate(target: Input) -> Result<()> {
    owned(target)?;
    let h = HWND(target.handle as *mut _);
    gpui_smoke::isolate_input(h)?;
    unsafe {
        SetWindowSubclass(h, Some(wheel_modifiers), WHEEL_SUBCLASS, 0).ok()?;
    }
    Ok(())
}
fn wheel(target: Input, local_physical: Point, delta: i16, control: bool) -> Result<()> {
    owned(target)?;
    let h = HWND(target.handle as *mut _);
    let mut at = POINT {
        x: local_physical.x.round() as i32,
        y: local_physical.y.round() as i32,
    };
    unsafe {
        ClientToScreen(h, &mut at).ok()?;
        PostMessageW(
            Some(h),
            WM_MOUSEWHEEL,
            WPARAM(
                ((delta as u16 as usize) << 16)
                    | WHEEL_MARKER
                    | if control { MK_CONTROL_QA } else { 0 },
            ),
            LPARAM((((at.y as u16 as u32) << 16) | at.x as u16 as u32) as isize),
        )?;
    }
    Ok(())
}
fn pin_input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    let target = pin
        .update(cx, |_, w, _| input(w))
        .map_err(|e| e.to_string())?;
    owned(target)?;
    Ok(target)
}
fn action(pin: WindowHandle<PinView>, next: PinAction, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| v.perform(next, w, cx))
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Clone)]
struct Meta {
    placement: Placement,
    viewport: Viewport,
    zoom: f32,
    opacity: u8,
    anchor: ZoomAnchor,
    dpi: f32,
    physical: (u32, u32),
}
fn meta(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Meta> {
    pin.update(cx, |v, w, cx| -> Result<_> {
        let viewport = v
            .canvas
            .as_ref()
            .ok_or("专项画布不存在")?
            .read(cx)
            .image_canvas
            .as_ref()
            .ok_or("专项视口不存在")?
            .viewport;
        let dpi = w.scale_factor();
        let physical = (
            (f32::from(w.viewport_size().width) * dpi).round() as u32,
            (f32::from(w.viewport_size().height) * dpi).round() as u32,
        );
        let mut client = RECT::default();
        unsafe {
            GetClientRect(hwnd(w), &mut client)?;
        }
        ensure(
            physical
                == (
                    (client.right - client.left) as u32,
                    (client.bottom - client.top) as u32,
                )
                && (unsafe { GetDpiForWindow(hwnd(w)) } as f32 / 96. - dpi).abs() < 0.01,
            "原生客户区与 GPUI 的 DPI/物理尺寸不一致",
        )?;
        Ok(Meta {
            placement: native::rect(hwnd(w))?,
            viewport,
            zoom: v.state.zoom,
            opacity: v.state.opacity,
            anchor: v.pin_zoom_anchor,
            dpi,
            physical,
        })
    })
    .map_err(|e| e.to_string())?
}
fn meta_log(m: &Meta) -> serde_json::Value {
    serde_json::json!({"placement":[m.placement.x,m.placement.y,m.placement.width,m.placement.height],
        "physical_client":m.physical,"zoom":m.zoom,"viewport_zoom":m.viewport.zoom,
        "viewport_offset":[m.viewport.offset.x,m.viewport.offset.y],"dpi":m.dpi,
        "opacity":m.opacity,"wheel_anchor":m.anchor.wire_name()})
}
async fn settle(pin: WindowHandle<PinView>, zoom: f32, cx: &mut AsyncApp) -> Result<Meta> {
    let expected = (
        (WIDTH as f32 * zoom).round() as u32,
        (HEIGHT as f32 * zoom).round() as u32,
    );
    let mut last = None;
    for _ in 0..50 {
        tick(cx).await;
        let m = meta(pin, cx)?;
        if m.physical == expected
            && (m.placement.width, m.placement.height) == expected
            && (m.zoom - zoom).abs() < 0.001
            && (m.viewport.zoom - zoom).abs() < 0.001
            && m.viewport.offset.distance(p(0., 0.)) < 0.01
        {
            return Ok(m);
        }
        last = Some(meta_log(&m));
    }
    Err(format!("缩放偏好原生尺寸未收敛：expected={zoom} last={last:?}").into())
}
fn anchor_error(before: &Meta, after: &Meta, local: Point) -> f32 {
    let image = before.viewport.image_point(local);
    let mapped = after.viewport.window_point(image);
    p(
        before.placement.x as f32 + local.x,
        before.placement.y as f32 + local.y,
    )
    .distance(p(
        after.placement.x as f32 + mapped.x,
        after.placement.y as f32 + mapped.y,
    ))
}

#[derive(Clone, PartialEq)]
struct DocumentState {
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    undo: bool,
    redo: bool,
    undo_edits: usize,
    redo_edits: usize,
    adjustments: crate::pin_model::Adjustments,
}
fn document(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<DocumentState> {
    pin.update(cx, |v, _, cx| -> Result<_> {
        let canvas = v.canvas.as_ref().ok_or("专项文档不存在")?.read(cx);
        let d = &canvas.editor.document;
        Ok(DocumentState {
            objects: d.items().to_vec(),
            revision: d.revision(),
            edit_revision: d.edit_revision(),
            undo: d.can_undo(),
            redo: d.can_redo(),
            undo_edits: v.undo_edits.len(),
            redo_edits: v.redo_edits.len(),
            adjustments: v.state.adjustments,
        })
    })
    .map_err(|e| e.to_string())?
}
fn invariant(pin: WindowHandle<PinView>, before: &DocumentState, cx: &mut AsyncApp) -> Result<()> {
    ensure(
        &document(pin, cx)? == before,
        "缩放/偏好/透明度改写了文档或撤销重做状态",
    )
}
fn exported(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Image> {
    pin.update(cx, |v, _, cx| v.output(cx))
        .map_err(|e| e.to_string())?
}
fn same_output(actual: &Image, baseline: &Image) -> Result<()> {
    ensure(
        (actual.width, actual.height) == (baseline.width, baseline.height)
            && actual.bgra == baseline.bgra,
        "缩放偏好改变了原图导出像素",
    )
}

async fn probe_settings(
    settings: WindowHandle<SettingsView>,
    id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let requested_tab = SettingsTab::for_control(id);
    for _ in 0..28 {
        tick(cx).await;
        let (target, size, at, tab_matches, tab_position, content) = settings
            .update(cx, |v, w, _| {
                let positions = v.ui_positions.borrow();
                (
                    input(w),
                    (
                        f32::from(w.viewport_size().width),
                        f32::from(w.viewport_size().height),
                    ),
                    positions.get(id).copied(),
                    requested_tab.is_none_or(|tab| v.tab == tab),
                    requested_tab.and_then(|tab| positions.get(tab.id()).copied()),
                    positions
                        .get("settings-content-top-left")
                        .copied()
                        .zip(positions.get("settings-content-bottom-right").copied())
                        .zip(positions.get("settings-content").copied()),
                )
            })
            .map_err(|e| e.to_string())?;
        owned(target)?;
        let in_window =
            |at: (f32, f32)| at.0 > 8. && at.0 < size.0 - 8. && at.1 > 8. && at.1 < size.1 - 8.;
        if !tab_matches {
            if let Some(at) = tab_position.filter(|at| in_window(*at)) {
                target.click(at);
            }
            tick(cx).await;
            continue;
        }
        let Some(((top_left, bottom_right), wheel_at)) = content else {
            continue;
        };
        let in_content = |at: (f32, f32)| {
            in_window(at)
                && at.0 > top_left.0 + 8.
                && at.0 < bottom_right.0 - 8.
                && at.1 > top_left.1 + 8.
                && at.1 < bottom_right.1 - 8.
        };
        ensure(in_content(wheel_at), "设置内容视口探针越界")?;
        if let Some(at) = at.filter(|at| in_content(*at)) {
            target.click(at);
            tick(cx).await;
            tick(cx).await;
            return Ok(());
        }
        let delta = if at.is_some_and(|at| at.1 <= top_left.1 + 8.) {
            360
        } else {
            -360
        };
        wheel(
            target,
            p(wheel_at.0 * target.scale, wheel_at.1 * target.scale),
            delta,
            false,
        )?;
    }
    Err(format!("设置真实入口滚动后仍不可见：{id}").into())
}
async fn open_menu(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<WindowHandle<PinMenu>> {
    let target = pin_input(pin, cx)?;
    let at = LPARAM(((24_u32 << 16) | 24) as isize);
    unsafe {
        PostMessageW(
            Some(HWND(target.handle as *mut _)),
            WM_RBUTTONDOWN,
            WPARAM(2),
            at,
        )?;
        PostMessageW(
            Some(HWND(target.handle as *mut _)),
            WM_RBUTTONUP,
            WPARAM(0),
            at,
        )?;
    }
    for _ in 0..30 {
        tick(cx).await;
        if let Some(menu) = pin
            .update(cx, |v, _, _| v.context_menu)
            .map_err(|e| e.to_string())?
        {
            let visible = menu
                .update(cx, |v, w, _| {
                    v.owner_matches(pin)
                        && v.is_open()
                        && unsafe { IsWindowVisible(hwnd(w)).as_bool() }
                })
                .unwrap_or(false);
            if !visible {
                continue;
            }
            let target = menu
                .update(cx, |_, w, _| input(w))
                .map_err(|e| e.to_string())?;
            isolate(target)?;
            probe_menu(menu, "tab-2", false, cx).await?;
            ensure(
                menu.update(cx, |v, _, _| v.tab == 2)
                    .map_err(|e| e.to_string())?,
                "贴图菜单显示页签的原生点击无效",
            )?;
            return Ok(menu);
        }
    }
    Err("原生右键没有打开自有贴图菜单".into())
}
async fn probe_menu(
    menu: WindowHandle<PinMenu>,
    id: &str,
    scroll: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..28 {
        tick(cx).await;
        let (target, size, at) = menu
            .update(cx, |v, w, _| {
                (
                    input(w),
                    (
                        f32::from(w.viewport_size().width),
                        f32::from(w.viewport_size().height),
                    ),
                    v.positions.borrow().get(id).copied(),
                )
            })
            .map_err(|e| e.to_string())?;
        owned(target)?;
        let top = if scroll { 64. } else { 4. };
        let bottom = if scroll { size.1 - 62. } else { size.1 - 4. };
        if let Some(at) =
            at.filter(|at| at.0 > 4. && at.0 < size.0 - 4. && at.1 > top && at.1 < bottom)
        {
            target.click(at);
            tick(cx).await;
            tick(cx).await;
            return Ok(());
        }
        ensure(scroll, format!("菜单页签入口未绘制：{id}"))?;
        let delta = if at.is_some_and(|at| at.1 < top) {
            240
        } else {
            -240
        };
        wheel(
            target,
            p(size.0 * target.scale / 2., size.1 * target.scale / 2.),
            delta,
            false,
        )?;
    }
    Err(format!("菜单真实入口滚动后仍不可见：{id}").into())
}

async fn preference_sync(
    pin: WindowHandle<PinView>,
    settings: WindowHandle<SettingsView>,
    anchor: ZoomAnchor,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..40 {
        tick(cx).await;
        if Preferences::load().pin_zoom_anchor == anchor
            && meta(pin, cx)?.anchor == anchor
            && settings
                .update(cx, |v, _, _| v.preferences.pin_zoom_anchor)
                .map_err(|e| e.to_string())?
                == anchor
        {
            return Ok(());
        }
    }
    Err(format!(
        "持久化/已有贴图/设置窗缩放偏好不同步：{}",
        anchor.wire_name()
    )
    .into())
}
async fn fresh_pin(anchor: ZoomAnchor, cx: &mut AsyncApp) -> Result<()> {
    let pin = cx
        .update(|cx| PinView::open_state(fixture(), (120, 120), PinState::default(), cx))
        .map_err(|e| e.to_string())??;
    let result = pin
        .update(cx, |v, w, _| -> Result<()> {
            owned(input(w))?;
            ensure(v.pin_zoom_anchor == anchor, "新建贴图没有读取存盘缩放偏好")
        })
        .map_err(|e| e.to_string())?;
    // 不走“关闭并保留贴图”的产品行为，测试夹具本身不产生工作区文件。
    let _ = pin.update(cx, |_, w, _| w.remove_window());
    result
}

#[derive(Clone)]
struct Bars {
    handles: [usize; 2],
    sizes: [(u32, u32); 2],
}
fn bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("一级工具栏消失")?;
    let (first, rect, secondary) = primary
        .update(cx, |v, w, _| -> Result<_> {
            Ok((
                hwnd(w).0 as usize,
                native::rect(hwnd(w))?,
                v.properties.ok_or("二级工具栏消失")?,
            ))
        })
        .map_err(|e| e.to_string())??;
    let (second, r) = secondary
        .update(cx, |_, w, _| -> Result<_> {
            Ok((hwnd(w).0 as usize, native::rect(hwnd(w))?))
        })
        .map_err(|e| e.to_string())??;
    Ok(Bars {
        handles: [first, second],
        sizes: [(rect.width, rect.height), (r.width, r.height)],
    })
}
async fn same_bars(pin: WindowHandle<PinView>, before: &Bars, cx: &mut AsyncApp) -> Result<()> {
    for _ in 0..40 {
        tick(cx).await;
        if let Ok(after) = bars(pin, cx) {
            ensure(after.handles == before.handles, "缩放重建了两级工具栏 HWND")?;
            if after.sizes == before.sizes {
                return Ok(());
            }
        }
    }
    Err("两级工具栏尺寸随贴图缩放发生变化".into())
}
async fn frame_pixel(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    let m = meta(pin, cx)?;
    let point = m.viewport.window_point(p(24.5, 24.5));
    let at = [point.x.floor() as u32 - 2, point.y.floor() as u32 - 2, 5, 5];
    pin.update(cx, |v, w, cx| {
        v.hovered = false;
        if let Some(c) = &v.canvas {
            c.update(cx, |c, cx| {
                c.pointer = (-1000, -1000);
                c.pointer_sample = p(-1000., -1000.);
                cx.notify();
            });
        }
        w.request_frame_capture(at)
    })
    .map_err(|e| e.to_string())??;
    for _ in 0..40 {
        tick(cx).await;
        if let Some(bytes) = pin
            .update(cx, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?
        {
            let bytes = bytes.map_err(|e| e.to_string())?;
            ensure(bytes.len() == 100, "自有 GPUI 帧读回尺寸错误")?;
            let expected = [50_u8, 120, 180, 255];
            ensure(
                bytes[48..52]
                    .iter()
                    .zip(expected)
                    .all(|(&actual, expected)| actual.abs_diff(expected) <= 4),
                format!("自有合成图的 GPU 帧像素错误：{:?}", &bytes[48..52]),
            )?;
            return Ok(
                serde_json::json!({"physical_rect":at,"actual_bgra":&bytes[48..52],
                "expected_bgra":expected,"source":"owned-gpui-framebuffer"}),
            );
        }
    }
    Err("自有 GPUI 帧读回超时".into())
}

/// 占住 settings.json 的跨进程配置锁，让 Preferences 保存失败；原文件始终保持可读。
/// 锁在析构时随文件句柄释放。
struct SaveBlock(#[allow(dead_code)] std::fs::File);
impl SaveBlock {
    fn new() -> Result<Self> {
        Ok(Self(crate::config_recovery::lock(
            &crate::preferences::data_dir().join("settings.json"),
        )?))
    }
}

async fn verify(
    pin: WindowHandle<PinView>,
    settings: WindowHandle<SettingsView>,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let started = Instant::now();
    settle(pin, 1., cx).await?;
    let original = document(pin, cx)?;
    ensure(
        original.undo && original.redo,
        "文档夹具必须同时有撤销与重做历史",
    )?;
    let baseline = exported(pin, cx)?;
    let mut modes = Vec::new();
    for (anchor, id) in [
        (ZoomAnchor::TopLeft, "settings-pin-anchor-top-left"),
        (ZoomAnchor::Pointer, "settings-pin-anchor-pointer"),
        (ZoomAnchor::Center, "settings-pin-anchor-center"),
    ] {
        probe_settings(settings, id, cx).await?;
        preference_sync(pin, settings, anchor, cx).await?;
        fresh_pin(anchor, cx).await?;
        action(pin, PinAction::Original, cx)?;
        let before = settle(pin, 1., cx).await?;
        let local = match anchor {
            ZoomAnchor::TopLeft => before.viewport.offset,
            ZoomAnchor::Pointer => POINTER,
            ZoomAnchor::Center => p(before.physical.0 as f32 / 2., before.physical.1 as f32 / 2.),
        };
        wheel(pin_input(pin, cx)?, POINTER, 120, false)?;
        let after = settle(pin, 1.1, cx).await?;
        let error = anchor_error(&before, &after, local);
        ensure(
            error <= 1.5,
            format!("普通滚轮{}锚点漂移 {error}px", anchor.wire_name()),
        )?;
        invariant(pin, &original, cx)?;
        same_output(&exported(pin, cx)?, &baseline)?;
        let pixels = frame_pixel(pin, cx).await?;
        modes.push(
            serde_json::json!({"mode":anchor.wire_name(),"settings_probe":id,
            "before":meta_log(&before),"after":meta_log(&after),"anchor_error_physical_px":error,
            "existing_pin_updated":true,"new_pin_reads_disk":true,"pixels":pixels}),
        );
    }
    action(pin, PinAction::Original, cx)?;
    let before = settle(pin, 1., cx).await?;
    let mut external = Preferences::load();
    external.pin_zoom_anchor = ZoomAnchor::Pointer;
    external.save()?;
    let (target, active) = pin
        .update(cx, |v, w, _| {
            v.pin_zoom_anchor = ZoomAnchor::Center;
            v.reset_zoom_wheel();
            (input(w), w.is_window_active())
        })
        .map_err(|e| e.to_string())?;
    // 强制旧缓存后直接投递轮消息，不插入 activation/hover 或同步产品 action。
    wheel(target, POINTER, 120, false)?;
    let after = settle(pin, 1.1, cx).await?;
    let error = anchor_error(&before, &after, POINTER);
    ensure(
        after.anchor == ZoomAnchor::Pointer && error <= 1.5,
        "滚轮输入没有即时读取另一个进程已写入的缩放锚点",
    )?;
    invariant(pin, &original, cx)?;
    modes.push(
        serde_json::json!({"mode":"pointer-from-external-owned-ini","old_cached_anchor":"center",
        "pin_active_before_input":active,"activation_or_hover_injected":false,
        "after":meta_log(&after),"anchor_error_physical_px":error}),
    );
    logs.push(serde_json::json!({"group":"native-wheel-three-anchors-settings-disk-live-new","ok":true,"modes":modes}));

    let stale = settings
        .update(cx, |v, _, _| v.preferences.clone())
        .map_err(|e| e.to_string())?;
    let menu = open_menu(pin, cx).await?;
    probe_menu(menu, "pin-menu-anchor-pointer", true, cx).await?;
    preference_sync(pin, settings, ZoomAnchor::Pointer, cx).await?;
    let saved = settings
        .update(cx, |v, _, cx| {
            // 回放这个真实旧窗持有的快照，使 save() 的读盘合并契约确实被覆盖。
            v.preferences = stale.clone();
            v.preferences.guides = !stale.guides;
            let result = v.save();
            cx.notify();
            result
        })
        .map_err(|e| e.to_string())?;
    ensure(saved, "旧设置窗保存其它项失败")?;
    preference_sync(pin, settings, ZoomAnchor::Pointer, cx).await?;
    ensure(
        Preferences::load().guides != stale.guides,
        "旧设置窗其它项没有保存",
    )?;
    // 模拟另一个设置入口已经改好其它字段，而这个已打开的窗仍持旧快照。
    // 显式锚点 radio 只能覆盖 pinZoomAnchor，不能回写旧 theme/hotkey。
    let old_fields = settings
        .update(cx, |v, _, _| v.preferences.clone())
        .map_err(|e| e.to_string())?;
    let mut latest = Preferences::load();
    latest.theme = old_fields.theme.next();
    latest.hotkey = 2;
    latest.save()?;
    settings
        .update(cx, |v, _, cx| {
            v.preferences = old_fields;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    probe_settings(settings, "settings-pin-anchor-top-left", cx).await?;
    preference_sync(pin, settings, ZoomAnchor::TopLeft, cx).await?;
    let explicit = Preferences::load();
    ensure(
        explicit.theme == latest.theme
            && explicit.hotkey == latest.hotkey
            && explicit.guides == latest.guides,
        "陈旧设置窗显式改锚点覆盖了磁盘上最新的其它字段",
    )?;
    let menu = open_menu(pin, cx).await?;
    probe_menu(menu, "pin-menu-anchor-pointer", true, cx).await?;
    preference_sync(pin, settings, ZoomAnchor::Pointer, cx).await?;
    logs.push(
        serde_json::json!({"group":"native-menu-preference-stale-settings-merge","ok":true,
        "menu_probe":"pin-menu-anchor-pointer","stale_anchor":stale.pin_zoom_anchor.wire_name(),
        "stored_anchor":Preferences::load().pin_zoom_anchor.wire_name(),"other_setting_saved":true,
        "explicit_anchor_preserves_latest_theme_hotkey":true,
        "latest_theme":explicit.theme.wire_name(),"latest_hotkey_mode":explicit.hotkey}),
    );

    let stored = std::fs::read(crate::preferences::data_dir().join("settings.json"))?;
    {
        let _block = SaveBlock::new()?;
        probe_settings(settings, "settings-pin-anchor-center", cx).await?;
        ensure(
            settings
                .update(cx, |v, _, _| {
                    v.preferences.pin_zoom_anchor == ZoomAnchor::Pointer
                        && !v.status.is_empty()
                        && !v.status.starts_with("已保存")
                })
                .map_err(|e| e.to_string())?,
            "设置保存失败仍改变了选中项或显示成功",
        )?;
        ensure(
            meta(pin, cx)?.anchor == ZoomAnchor::Pointer,
            "设置保存失败更新了已有贴图",
        )?;
        let menu = open_menu(pin, cx).await?;
        probe_menu(menu, "pin-menu-anchor-top-left", true, cx).await?;
        ensure(
            meta(pin, cx)?.anchor == ZoomAnchor::Pointer,
            "菜单保存失败仍改变了缩放偏好",
        )?;
        ensure(
            std::fs::read(crate::preferences::data_dir().join("settings.json"))? == stored,
            "保存失败改写了原有 QA 配置",
        )?;
    }
    preference_sync(pin, settings, ZoomAnchor::Pointer, cx).await?;
    logs.push(
        serde_json::json!({"group":"native-settings-menu-save-failure-rollback","ok":true,
        "blocked":"owned-settings-pid-tmp-directory","settings_bytes_unchanged":true,
        "selected_anchor_unchanged":true,"resident_notify_disabled":true}),
    );

    let menu = open_menu(pin, cx).await?;
    probe_menu(menu, "pin-menu-anchor-center", true, cx).await?;
    preference_sync(pin, settings, ZoomAnchor::Center, cx).await?;
    pin.update(cx, |v, w, cx| {
        v.perform(PinAction::Chrome, w, cx);
        v.annotation(Action::Tool(Tool::Rectangle), w, cx);
    })
    .map_err(|e| e.to_string())?;
    action(pin, PinAction::Original, cx)?;
    let before = settle(pin, 1., cx).await?;
    let mut before_bars = None;
    for _ in 0..40 {
        tick(cx).await;
        if let Ok(value) = bars(pin, cx) {
            before_bars = Some(value);
            break;
        }
    }
    let before_bars = before_bars.ok_or("初始两级工具栏未绘制")?;
    let menu = open_menu(pin, cx).await?;
    probe_menu(menu, "pin-menu-zoom-150", true, cx).await?;
    let enlarged = settle(pin, 1.5, cx).await?;
    let tl_error = anchor_error(&before, &enlarged, before.viewport.offset);
    ensure(
        tl_error <= 1.5,
        "比例菜单被普通滚轮指针偏好覆盖，图片左上角漂移",
    )?;
    same_bars(pin, &before_bars, cx).await?;
    let target = pin_input(pin, cx)?;
    let at = p(230., 260.);
    let lp = LPARAM((((at.y as u32) << 16) | at.x as u32) as isize);
    unsafe {
        PostMessageW(
            Some(HWND(target.handle as *mut _)),
            WM_MBUTTONDOWN,
            WPARAM(0x10),
            lp,
        )?;
        PostMessageW(
            Some(HWND(target.handle as *mut _)),
            WM_MBUTTONUP,
            WPARAM(0),
            lp,
        )?;
    }
    let restored = settle(pin, 1., cx).await?;
    let pointer_error = anchor_error(&enlarged, &restored, at);
    ensure(pointer_error <= 1.5, "中键恢复100%没有保持指针下的图像点")?;
    same_bars(pin, &before_bars, cx).await?;
    invariant(pin, &original, cx)?;
    logs.push(serde_json::json!({"group":"native-ratio-menu-top-left-middle-pointer-stable-toolbars","ok":true,
        "ratio_menu_probe":"pin-menu-zoom-150","wheel_preference":"center",
        "menu_anchor_error_physical_px":tl_error,"middle_anchor_error_physical_px":pointer_error,
        "toolbar_handles":before_bars.handles,"toolbar_sizes":before_bars.sizes}));

    let before = meta(pin, cx)?;
    let mut opacity = Vec::new();
    for (delta, expected) in [(-120, 230), (-120, 204), (240, 255)] {
        wheel(pin_input(pin, cx)?, POINTER, delta, true)?;
        let mut observed = None;
        for _ in 0..40 {
            tick(cx).await;
            let m = meta(pin, cx)?;
            if m.opacity == expected {
                observed = Some(m);
                break;
            }
        }
        let m = observed.ok_or_else(|| {
            format!("Ctrl轮每格10个百分点失败：delta={delta} expected={expected}")
        })?;
        ensure(
            m.placement == before.placement && m.zoom == before.zoom,
            "Ctrl滚轮改变了图像缩放或原生窗口位置",
        )?;
        opacity.push(serde_json::json!({"wheel_delta":delta,"opacity_byte":m.opacity}));
    }
    same_bars(pin, &before_bars, cx).await?;
    action(pin, PinAction::Lock, cx)?;
    tick(cx).await;
    let locked = meta(pin, cx)?;
    wheel(pin_input(pin, cx)?, POINTER, 120, false)?;
    wheel(pin_input(pin, cx)?, POINTER, -120, true)?;
    for _ in 0..6 {
        tick(cx).await;
    }
    let after = meta(pin, cx)?;
    ensure(
        after.placement == locked.placement
            && after.zoom == locked.zoom
            && after.opacity == locked.opacity,
        "锁定后的普通/Ctrl滚轮仍改变了贴图",
    )?;
    action(pin, PinAction::Lock, cx)?;
    invariant(pin, &original, cx)?;
    same_output(&exported(pin, cx)?, &baseline)?;
    let pixels = frame_pixel(pin, cx).await?;
    ensure(
        started.elapsed() < Duration::from_secs(60),
        "贴图缩放偏好专项超过60秒",
    )?;
    logs.push(serde_json::json!({"group":"native-ctrl-wheel-ten-percent-locked-document-export","ok":true,
        "opacity_steps":opacity,"locked_zoom_opacity_unchanged":true,"document_undo_redo_unchanged":true,
        "export_pixels_exact":true,"toolbar_handles_unchanged_by_zoom_middle_ctrl":true,
        "lock_hides_editing_properties_by_product_contract":true,"pixels":pixels}));
    Ok(())
}

pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("贴图缩放偏好专项未完成".into());
    let foreground = unsafe { GetForegroundWindow() }.0 as usize;
    let initial = (|| -> Result<_> {
        std::fs::create_dir_all(FOLDER)?;
        let settings_path = std::env::current_dir()?.join(FOLDER).join("settings");
        ensure(
            crate::preferences::data_dir() == settings_path
                && std::env::var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").as_deref() == Ok("1"),
            "缩放偏好专项未隔离用户配置或禁用托盘通知",
        )?;
        let seed = Preferences {
            pin_restore: false,
            history: false,
            hotkey: 4,
            quick_dir: settings_path.join("output"),
            ..Default::default()
        };
        seed.save()?;
        std::fs::write(settings_path.join("tool-presets.json"), "{}\n")?;
        let work = native::work_area((100, 100));
        ensure(
            work.width >= WIDTH * 2 && work.height >= HEIGHT * 2,
            "缩放偏好专项工作区至少需要960×640",
        )?;
        let origin = (
            work.x + (work.width as i32 - WIDTH as i32) / 2,
            work.y + (work.height as i32 - HEIGHT as i32) / 2,
        );
        let pin = PinView::open_state(fixture(), origin, PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            isolate(input(w))?;
            v.ensure_canvas(w, cx)?;
            v.canvas.as_ref().unwrap().update(cx, |canvas, cx| {
                let mut rectangle = Annotation::new(Tool::Rectangle, p(40., 50.), Style::default());
                rectangle.update(p(120., 100.));
                canvas.editor.document.insert(rectangle.clone());
                rectangle.translate(180., 80.);
                canvas.editor.document.insert(rectangle);
                canvas.editor.document.undo();
                canvas.editor.select_tool(Tool::Edit);
                canvas.pin_editing = false;
                cx.notify();
            });
            Ok(())
        })??;
        let settings = SettingsView::open_window(cx)?;
        settings.update(cx, |_, w, _| isolate(input(w)))??;
        Ok((pin, settings))
    })();
    let (pin, settings) = match initial {
        Ok(value) => value,
        Err(error) => {
            let error = error.to_string();
            let _ = std::fs::write(
                Path::new(FOLDER).join("report.json"),
                serde_json::json!({"ok":false,"error":error,"groups":[]}).to_string(),
            );
            *errors.borrow_mut() = Some(error);
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let mut logs=Vec::new();
        let error=verify(pin,settings,&mut logs,cx).await.err().map(|e|e.to_string());
        let report=serde_json::json!({"test":"native-pin-zoom-preferences","ok":error.is_none(),
            "error":error,"groups":logs,"source":"owned-synthetic-pin-and-gpui-framebuffer",
            "native_input":"PostMessageW mouse-wheel/button through GPUI WndProc",
            "ctrl_modifier_scope":"temporary current-thread keyboard state inside owned-window subclass",
            "clipboard_read":false,"clipboard_written":false,"global_keyboard_input":false,
            "global_pointer_moved":false,"settings_scope":crate::preferences::data_dir()});
        let saved=std::fs::write(Path::new(FOLDER).join("report.json"),serde_json::to_vec_pretty(&report).unwrap_or_default());
        println!("{report}");
        *errors.borrow_mut()=error.or_else(||saved.err().map(|e|e.to_string()));
        let _=settings.update(cx,|_,w,_|w.remove_window());
        let _=pin.update(cx,|v,w,cx|{v.close_toolbar(cx);w.remove_window();});
        unsafe {
            let mut process=0;GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut process));
            if process==std::process::id() && foreground!=0 {let _=SetForegroundWindow(HWND(foreground as *mut _));}
        }
        let _=cx.update(|cx|cx.quit());
    }).detach();
}
