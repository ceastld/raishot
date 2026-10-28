//! 真实 GPUI 交互验收：按实际绘制后的按钮位置点击，不移动用户鼠标或改写剪贴板。
use crate::{
    annotation::Tool, capture::memory_mib, capture_view::CaptureView, desktop::DesktopCapture,
    export, geometry::Rect, gpui_capture::hwnd, Result,
};
use gpui::{App, AppContext, Entity, Window};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};
fn curve_owner_qpc() -> Option<i64> {
    let mut value = 0;
    unsafe {
        windows::Win32::System::Performance::QueryPerformanceCounter(&mut value)
            .is_ok()
            .then_some(value)
    }
}
// 仅 DIAG=1 的自有成功 draw 属性观察；不证明该 draw 对应本次曲线。
fn curve_owner_present(input: Input) -> serde_json::Value {
    unsafe {
        let handle = HWND(input.handle as *mut _);
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(handle, Some(&mut pid));
        let owner_verified =
            tid > 0 && pid == std::process::id() && IsWindow(Some(handle)).as_bool();
        let verify_own = || {
            let mut observed_pid = 0;
            let observed_tid = GetWindowThreadProcessId(handle, Some(&mut observed_pid));
            IsWindow(Some(handle)).as_bool()
                && observed_pid == pid
                && observed_pid == std::process::id()
                && observed_tid == tid
                && observed_tid > 0
        };
        let read = |name: windows::core::PCWSTR| {
            let before = owner_verified && verify_own();
            let raw = before.then(|| GetPropW(handle, name).0 as usize);
            let after = before.then(verify_own);
            let value = raw.filter(|value| *value != 0 && after == Some(true));
            serde_json::json!({
                "owner_verified_before":before,"owner_verified_after":after,
                "property_called":before,"raw":raw,"has_nonzero_raw":raw.map(|value|value!=0),
                "value":value,"query_qualified_nonzero":value.is_some(),
                "zero_scope":"zero is absent/unknown; not an API success receipt"
            })
        };
        serde_json::json!({
            "owner_hwnd":input.handle,"owner_pid":pid,"owner_tid":tid,"owner_verified":owner_verified,
            "serial":read(w!("RustShot.GpuiPresentSerial")),
            "size_packed":read(w!("RustShot.GpuiPresentSize")),
            "size_encoding":"width<<16|height",
            "scope":"own successful renderer.draw metadata only; not bound to this curve or DWM visibility"
        })
    }
}
fn owned_smoke_window(handle: HWND) -> Result<HWND> {
    unsafe {
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(handle, Some(&mut pid));
        if !IsWindow(Some(handle)).as_bool()
            || pid != std::process::id()
            || tid != windows::Win32::System::Threading::GetCurrentThreadId()
        {
            return Err("自动检查窗口不属于本轮 GPUI，未向该窗口发送操作".into());
        }
    }
    Ok(handle)
}
// 仅显式 QA 开关调用；所有非自身 HWND 只记录 opaque 数值，不查询其属性。
fn curve_owner_snapshot(
    input: Input,
    bounds: crate::geometry::ScreenRect,
    points: &[(u32, u32)],
) -> serde_json::Value {
    use windows::Win32::{
        Graphics::Gdi::ClientToScreen,
        System::{Performance::QueryPerformanceFrequency, Threading::GetCurrentThreadId},
        UI::{
            HiDpi::*,
            Input::KeyboardAndMouse::{GetActiveWindow, GetCapture, GetFocus, IsWindowEnabled},
        },
    };
    let begin_qpc = curve_owner_qpc();
    unsafe {
        let handle = HWND(input.handle as *mut _);
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(handle, Some(&mut pid));
        let owner_verified =
            tid > 0 && pid == std::process::id() && IsWindow(Some(handle)).as_bool();
        // 仅 DIAG=1 的既有两次 snapshot；每项调用前后核同一自有 HWND/PID/TID。
        let own_window_status = owner_verified.then(|| {
            #[link(name = "dwmapi")]
            unsafe extern "system" {
                #[link_name = "DwmGetWindowAttribute"]
                fn curve_owner_dwm_get_window_attribute(
                    hwnd: HWND,
                    attribute: u32,
                    value: *mut std::ffi::c_void,
                    bytes: u32,
                ) -> i32;
            }
            let verify_own = || {
                let mut observed_pid = 0;
                let observed_tid = GetWindowThreadProcessId(handle, Some(&mut observed_pid));
                IsWindow(Some(handle)).as_bool()
                    && observed_pid == pid
                    && observed_pid == std::process::id()
                    && observed_tid == tid
                    && observed_tid > 0
            };
            let read_style = |index: WINDOW_LONG_PTR_INDEX| {
                let before = verify_own();
                let result = before.then(|| {
                    SetLastError(WIN32_ERROR(0));
                    let raw = GetWindowLongPtrW(handle, index);
                    let error = GetLastError().0;
                    (raw as u32, raw != 0 || error == 0, error)
                });
                let after = before.then(verify_own);
                let succeeded = result.map(|(_, ok, _)| ok);
                let qualified = succeeded == Some(true) && after == Some(true);
                serde_json::json!({
                    "owner_verified_before":before, "owner_verified_after":after,
                    "api_called":before, "api_succeeded":succeeded,
                    "last_error_raw":result.map(|(_,_,error)|error),
                    "value":result.filter(|_|qualified).map(|(value,_,_)|value)
                })
            };
            let style = read_style(GWL_STYLE);
            let exstyle = read_style(GWL_EXSTYLE);
            let visible_before = verify_own();
            let visible_raw = visible_before.then(|| IsWindowVisible(handle).as_bool());
            let visible_after = visible_before.then(verify_own);
            let visible = visible_raw.filter(|_| visible_after == Some(true));
            let enabled_before = verify_own();
            let enabled_raw = enabled_before.then(|| IsWindowEnabled(handle).as_bool());
            let enabled_after = enabled_before.then(verify_own);
            let enabled = enabled_raw.filter(|_| enabled_after == Some(true));
            let cloak_before = verify_own();
            let mut cloak = 0_u32;
            let cloak_hresult = cloak_before.then(|| {
                curve_owner_dwm_get_window_attribute(
                    handle,
                    14, // DWMWA_CLOAKED；raw HRESULT，未新增 Cargo feature。
                    (&mut cloak as *mut u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                )
            });
            let cloak_after = cloak_before.then(verify_own);
            let cloak_api_succeeded = cloak_hresult.map(|hr| hr >= 0);
            let cloak = (cloak_api_succeeded == Some(true) && cloak_after == Some(true))
                .then_some(cloak);
            serde_json::json!({
                "scope":"own metadata only; each API bracketed by original HWND/PID/TID/IsWindow recheck; no foreign attributes/window mutation/input",
                "style":style, "exstyle":exstyle,
                "visible":{"owner_verified_before":visible_before,"owner_verified_after":visible_after,"api_called":visible_before,"query_qualified":visible.is_some(),"value":visible},
                "enabled":{"owner_verified_before":enabled_before,"owner_verified_after":enabled_after,"api_called":enabled_before,"query_qualified":enabled.is_some(),"value":enabled},
                "dwm_cloak":{"owner_verified_before":cloak_before,"owner_verified_after":cloak_after,"api_called":cloak_before,"api_succeeded":cloak_api_succeeded,"hresult":cloak_hresult,"value":cloak},
                "window_region_observed":false
            })
        });
        let context = GetThreadDpiAwarenessContext();
        let context_known = !context.0.is_null();
        let pmv2 = context_known.then(|| {
            AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                .as_bool()
        });
        let mut frequency = 0;
        let frequency = QueryPerformanceFrequency(&mut frequency)
            .is_ok()
            .then_some(frequency)
            .filter(|value| *value > 0);
        let rect = |r: RECT| [r.left, r.top, r.right, r.bottom];
        let mut outer = RECT::default();
        let outer = (owner_verified && GetWindowRect(handle, &mut outer).is_ok())
            .then_some(outer)
            .map(rect);
        let mut client = RECT::default();
        let client = (owner_verified && GetClientRect(handle, &mut client).is_ok())
            .then_some(client)
            .map(rect);
        let mut origin = POINT::default();
        let origin = (owner_verified && ClientToScreen(handle, &mut origin).as_bool())
            .then_some([origin.x, origin.y]);
        let dpi = owner_verified
            .then(|| GetDpiForWindow(handle))
            .filter(|value| *value > 0);
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let gui_ok = owner_verified && GetGUIThreadInfo(tid, &mut gui).is_ok();
        let own_gui = gui_ok.then(|| {
            serde_json::json!({
                "active_hwnd":gui.hwndActive.0 as usize, "focus_hwnd":gui.hwndFocus.0 as usize,
                "capture_hwnd":gui.hwndCapture.0 as usize, "flags":gui.flags.0
            })
        });
        let caller_gui = owner_verified.then(|| {
            serde_json::json!({
                "active_hwnd":GetActiveWindow().0 as usize, "focus_hwnd":GetFocus().0 as usize,
                "capture_hwnd":GetCapture().0 as usize
            })
        });
        let foreground = GetForegroundWindow();
        let point_records: Vec<_> = points.iter().enumerate().map(|(slot, &(x, y))| {
            let point_begin_qpc = curve_owner_qpc();
            let global = i32::try_from(i64::from(bounds.left) + i64::from(x)).ok()
                .zip(i32::try_from(i64::from(bounds.top) + i64::from(y)).ok())
                .map(|(x, y)| POINT { x, y });
            let local = i32::try_from(x).ok().zip(i32::try_from(y).ok()).map(|(x, y)| POINT { x, y });
            let mapped = local.and_then(|mut p| (owner_verified && ClientToScreen(handle, &mut p).as_bool()).then_some(p));
            let hits = global.filter(|_| owner_verified).map(|point| (WindowFromPoint(point), WindowFromPhysicalPoint(point)));
            serde_json::json!({
                "curve_index":([18_usize,25,56][slot]), "canvas_point":[x,y],
                "bounds_global_point":global.map(|p|[p.x,p.y]),
                "client_to_screen_raw":mapped.map(|p|[p.x,p.y]),
                "client_mapping_query_succeeded":mapped.is_some(),
                "client_mapping_equals_bounds_global":mapped.zip(global).map(|(a,b)|a.x==b.x && a.y==b.y),
                "window_from_point_raw_hwnd":hits.map(|h|h.0.0 as usize),
                "window_from_point_has_hwnd":hits.map(|h|!h.0.0.is_null()),
                "window_from_point_is_own":hits.and_then(|h|(!h.0.0.is_null()).then_some(h.0==handle)),
                "window_from_physical_point_raw_hwnd":hits.map(|h|h.1.0 as usize),
                "window_from_physical_point_has_hwnd":hits.map(|h|!h.1.0.is_null()),
                "window_from_physical_point_is_own":hits.and_then(|h|(!h.1.0.is_null()).then_some(h.1==handle)),
                "qpc_begin":point_begin_qpc, "qpc_end":curve_owner_qpc()
            })
        }).collect();
        let own_present_properties = curve_owner_present(input);
        serde_json::json!({
            "qpc_begin":begin_qpc, "qpc_end":curve_owner_qpc(), "qpc_frequency":frequency,
            "owner_hwnd":input.handle, "owner_pid":pid, "owner_tid":tid, "caller_tid":GetCurrentThreadId(),
            "owner_verified":owner_verified,
            "own_window_status":own_window_status, "own_present_properties":own_present_properties,
            "geometry_queries_succeeded":outer.is_some() && client.is_some() && origin.is_some(),
            "own_gui_query_succeeded":gui_ok, "outer_rect_raw":outer, "client_rect_raw":client,
            "client_origin_raw":origin, "window_dpi":dpi,
            "thread_dpi_context_raw":context.0 as isize,
            "thread_dpi_awareness":context_known.then(||GetAwarenessFromDpiAwarenessContext(context).0),
            "thread_is_per_monitor_v2":pmv2, "win32_geometry_scope":"caller-context/raw",
            "foreground_raw_hwnd":foreground.0 as usize, "foreground_is_own":owner_verified.then_some(foreground==handle),
            "own_gui_thread":own_gui, "caller_gui_thread":caller_gui,
            "bounds":[bounds.left,bounds.top,bounds.width as i64,bounds.height as i64],
            "point_queries":point_records
        })
    }
}

// 只在 DIAG=1 的原 RGB 失败后使用；结果属于同模型的新重绘，不绑定原采样帧。
fn curve_framebuffer_source(view: &CaptureView) -> Result<crate::scene::SourcePixels<'_>> {
    for preview in &view.previews {
        if view.source.original(preview).as_bytes(0).is_none() {
            return Err("诊断冻结源缺少原始像素帧".into());
        }
    }
    let source = view.source_pixels();
    if source.canvas.x != 0
        || source.canvas.y != 0
        || source.canvas.width == 0
        || source.canvas.height == 0
    {
        return Err("诊断冻结源画布范围无效".into());
    }
    for plane in source.planes.iter().chain(source.overlay.iter()) {
        let pixels = u64::from(plane.rect.width)
            .checked_mul(u64::from(plane.rect.height))
            .ok_or("诊断plane像素溢出")?;
        let bytes = pixels
            .checked_mul(4)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or("诊断plane长度溢出")?;
        if plane.rect.width == 0
            || plane.rect.height == 0
            || plane.rect.x.checked_add(plane.rect.width).is_none()
            || plane.rect.y.checked_add(plane.rect.height).is_none()
            || pixels > u64::from(u32::MAX)
            || plane.bgra.len() != bytes
        {
            return Err("诊断plane矩形/行跨度/完整BGRA长度无效".into());
        }
    }
    Ok(source)
}
fn curve_frozen_probe(
    view: &CaptureView,
    scene: &crate::scene::Scene,
    point: (u32, u32),
) -> serde_json::Value {
    let read = || -> Result<serde_json::Value> {
        let source = curve_framebuffer_source(view)?;
        let rect = Rect {
            x: point.0,
            y: point.1,
            width: 1,
            height: 1,
        };
        if !rect.valid(source.canvas.width, source.canvas.height) {
            return Err("诊断1px超出冻结画布".into());
        }
        let contains = |p: &crate::scene::PixelPlane<'_>| {
            point.0 >= p.rect.x
                && point.1 >= p.rect.y
                && point.0 < p.rect.x + p.rect.width
                && point.1 < p.rect.y + p.rect.height
        };
        if !source.planes.iter().any(contains) {
            return Err("诊断1px没有真实base plane覆盖；不把默认黑色作为底色".into());
        }
        for plane in source
            .planes
            .iter()
            .chain(source.overlay.iter())
            .filter(|p| contains(p))
        {
            let index = u64::from(point.1 - plane.rect.y)
                .checked_mul(u64::from(plane.rect.width))
                .and_then(|n| n.checked_add(u64::from(point.0 - plane.rect.x)))
                .ok_or("诊断1px索引溢出")?;
            let start = index
                .checked_mul(4)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("诊断1px字节索引溢出")?;
            let end = start.checked_add(4).ok_or("诊断1px末端溢出")?;
            if index > u64::from(u32::MAX) || plane.bgra.get(start..end).is_none() {
                return Err("诊断1px索引未落在已核BGRA范围".into());
            }
        }
        let mut image = source.read(rect);
        let frozen: [u8; 4] = image
            .bgra
            .as_slice()
            .try_into()
            .map_err(|_| "诊断冻结1px长度不是4")?;
        let composition = scene.try_apply(&mut image, rect);
        let composite: Option<[u8; 4]> = composition
            .as_ref()
            .ok()
            .and_then(|_| image.bgra.as_slice().try_into().ok());
        Ok(
            serde_json::json!({"canvas_point":[point.0,point.1],"frozen_cpu_bgra":frozen,"shared_scene_cpu_bgra":composite,"diag_error":composition.err().map(|e|e.to_string()),"scope":"frozen_cpu_source_including_existing_overlay; shared Scene 1px composition; no Desktop read and no actual GPU paint proof"}),
        )
    };
    match read() {
        Ok(value) => value,
        Err(error) => {
            serde_json::json!({"canvas_point":[point.0,point.1],"frozen_cpu_bgra":null,"shared_scene_cpu_bgra":null,"diag_error":error.to_string(),"scope":"frozen_cpu_source_including_existing_overlay; qualification failed, no pixel accepted"})
        }
    }
}
struct CurveFramebufferModel {
    bounds: crate::geometry::ScreenRect,
    source_current: usize,
    source_identity: crate::scene::SourceIdentity,
    stamp: (u64, u64),
    crop: Option<Rect>,
    scene: std::sync::Arc<crate::scene::Scene>,
    mesh: std::sync::Arc<crate::vector_mesh::VectorMesh>,
}
impl CurveFramebufferModel {
    fn verify(&self, view: &mut CaptureView) -> Result<()> {
        if view.source.bounds != self.bounds
            || view.source.current != self.source_current
            || view.editor.scene_stamp() != self.stamp
            || view.editor.crop != self.crop
            || view.image_canvas.is_some()
            || view.pin_mode
        {
            return Err("诊断期间source/scene_stamp/crop/Full画布类型改变".into());
        }
        if crate::scene::SourceIdentity::of(&curve_framebuffer_source(view)?)
            != self.source_identity
        {
            return Err("诊断期间冻结源身份改变".into());
        }
        let scene = view.prepared_scene();
        let Some(crate::scene::Layer::Vector { mesh, .. }) = scene.layers.first() else {
            return Err("诊断期间Vector层消失".into());
        };
        if !std::sync::Arc::ptr_eq(&scene, &self.scene) || !std::sync::Arc::ptr_eq(mesh, &self.mesh)
        {
            return Err("诊断期间原SceneArc/mesh身份改变".into());
        }
        Ok(())
    }
}
#[derive(PartialEq)]
struct CurveFramebufferWindow {
    pid: u32,
    tid: u32,
    scale: f32,
    outer: [i32; 4],
    client: [i32; 4],
    origin: [i32; 2],
    physical: [u32; 2],
}
fn curve_framebuffer_window(
    input: Input,
    window: &Window,
    bounds: crate::geometry::ScreenRect,
) -> Result<CurveFramebufferWindow> {
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    let handle = HWND(input.handle as *mut _);
    if hwnd(window) != handle {
        return Err("诊断WindowHandle不是原self HWND".into());
    }
    let verify = || owned_smoke_window(handle).map(|_| ());
    verify()?;
    let mut pid = 0;
    let tid = unsafe { GetWindowThreadProcessId(handle, Some(&mut pid)) };
    verify()?;
    if pid != std::process::id()
        || tid == 0
        || tid != unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    {
        return Err("诊断原self PID/TID读取无效".into());
    }
    let mut outer = RECT::default();
    unsafe {
        GetWindowRect(handle, &mut outer)?;
    }
    verify()?;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(handle, &mut client)?;
    }
    verify()?;
    let mut origin = POINT::default();
    if !unsafe { ClientToScreen(handle, &mut origin).as_bool() } {
        return Err("诊断self client原点查询失败".into());
    }
    verify()?;
    let scale = window.scale_factor();
    let viewport = window.viewport_size();
    let physical = [
        f64::from(f32::from(viewport.width)) * f64::from(scale),
        f64::from(f32::from(viewport.height)) * f64::from(scale),
    ];
    if !scale.is_finite()
        || scale <= 0.
        || scale.to_bits() != input.scale.to_bits()
        || physical
            .iter()
            .any(|v| !v.is_finite() || *v <= 0. || *v > f64::from(u32::MAX))
        || client.left != 0
        || client.top != 0
        || i64::from(client.right) != i64::from(bounds.width)
        || i64::from(client.bottom) != i64::from(bounds.height)
        || origin.x != bounds.left
        || origin.y != bounds.top
        || physical[0].round() != f64::from(bounds.width)
        || physical[1].round() != f64::from(bounds.height)
    {
        return Err("诊断原self客户区/物理映射/scale与冻结源不一致".into());
    }
    verify()?;
    Ok(CurveFramebufferWindow {
        pid,
        tid,
        scale,
        outer: [outer.left, outer.top, outer.right, outer.bottom],
        client: [client.left, client.top, client.right, client.bottom],
        origin: [origin.x, origin.y],
        physical: [physical[0].round() as u32, physical[1].round() as u32],
    })
}
// 只过滤当前自动验收窗口的鼠标移动，不移动/锁定系统鼠标。原生调用仍经过 GPUI WndProc。
pub(crate) const SMOKE_MOVE: usize = 1 << 24;
const SMOKE_SUBCLASS: usize = 0x5253534d;
unsafe extern "system" fn smoke_window_input(
    handle: HWND,
    message: u32,
    mut wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_MOUSEMOVE {
        if wparam.0 & SMOKE_MOVE == 0 {
            return LRESULT(0);
        }
        wparam.0 &= !SMOKE_MOVE;
    } else if message == WM_NCMOUSEMOVE {
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(handle, Some(smoke_window_input), id);
    }
    DefSubclassProc(handle, message, wparam, lparam)
}
pub(crate) fn isolate_input(handle: HWND) -> Result<()> {
    unsafe {
        SetWindowSubclass(handle, Some(smoke_window_input), SMOKE_SUBCLASS, 0).ok()?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
pub(crate) struct Input {
    pub(crate) handle: usize,
    pub(crate) scale: f32,
}
impl Input {
    pub(crate) fn drag(self, a: (u32, u32), b: (u32, u32)) {
        unsafe {
            let h = HWND(self.handle as *mut _);
            let pack = |(x, y): (u32, u32)| LPARAM(((y << 16) | x) as isize);
            let _ = PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(1), pack(a));
            let _ = PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(SMOKE_MOVE | 1), pack(b));
            let _ = PostMessageW(Some(h), WM_LBUTTONUP, WPARAM(0), pack(b));
        }
    }
    pub(crate) fn hover(self, p: (u32, u32)) -> Result<()> {
        unsafe {
            PostMessageW(
                Some(HWND(self.handle as *mut _)),
                WM_MOUSEMOVE,
                WPARAM(SMOKE_MOVE),
                LPARAM(((p.1 << 16) | p.0) as isize),
            )?;
        }
        Ok(())
    }
    fn double_click(self, p: (u32, u32)) -> Result<()> {
        // GPUI Windows 在两次 DOWN 中自行计数，不接收 WM_LBUTTONDBLCLK。
        self.drag(p, p);
        self.drag(p, p);
        Ok(())
    }
    pub(crate) fn stroke(self, points: &[(u32, u32)]) -> Result<()> {
        let Some(&first) = points.first() else {
            return Ok(());
        };
        unsafe {
            let h = HWND(self.handle as *mut _);
            let pack = |(x, y): (u32, u32)| LPARAM(((y << 16) | x) as isize);
            PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(1), pack(first))?;
            for &p in points.iter().skip(1) {
                PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(SMOKE_MOVE | 1), pack(p))?;
            }
            PostMessageW(
                Some(h),
                WM_LBUTTONUP,
                WPARAM(0),
                pack(*points.last().unwrap()),
            )?;
        }
        Ok(())
    }
    pub(crate) fn wheel(self, p: (f32, f32), delta: i16) -> Result<()> {
        unsafe {
            let h = HWND(self.handle as *mut _);
            let mut point = POINT {
                x: (p.0 * self.scale).round() as i32,
                y: (p.1 * self.scale).round() as i32,
            };
            windows::Win32::Graphics::Gdi::ClientToScreen(h, &mut point).ok()?;
            PostMessageW(
                Some(h),
                WM_MOUSEWHEEL,
                WPARAM((delta as u16 as usize) << 16),
                LPARAM((((point.y as u16 as u32) << 16) | point.x as u16 as u32) as isize),
            )?;
        }
        Ok(())
    }
    pub(crate) fn key(self, key: usize) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.handle as *mut _)),
                WM_KEYDOWN,
                WPARAM(key),
                LPARAM(0),
            );
        }
    }
    fn text(self, text: &str) {
        unsafe {
            for unit in text.encode_utf16() {
                let _ = PostMessageW(
                    Some(HWND(self.handle as *mut _)),
                    WM_CHAR,
                    WPARAM(unit as usize),
                    LPARAM(0),
                );
            }
        }
    }
    pub(crate) fn click(self, p: (f32, f32)) {
        let p = (
            (p.0 * self.scale).round() as u32,
            (p.1 * self.scale).round() as u32,
        );
        self.drag(p, p);
    }
}
pub fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _cx: &mut App,
    started: Instant,
    preview_ms: f64,
    errors: Rc<RefCell<Option<String>>>,
) {
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("pin-handoff") {
        crate::pin_handoff_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("control-selection") {
        crate::control_selection_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("crop-expand") {
        crate::crop_expand_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("cursor-controls") {
        cursor_controls::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("highlighter") {
        crate::highlighter_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("toolbar-above") {
        crate::toolbar_above_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("long-workflow") {
        crate::long_workflow_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("pencil-continuity") {
        crate::pencil_continuity_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("toolbar-stability") {
        crate::toolbar_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("pencil-input") {
        crate::pencil_input_smoke::schedule(view, window, _cx, errors);
        return;
    }
    if std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("pencil-perf") {
        crate::pencil_input_smoke::schedule_perf(view, window, _cx, errors);
        return;
    }
    let weak = view.downgrade();
    window.on_next_frame(move|window,cx|{
        let own_window = window.window_handle();
        let activation_ms=started.elapsed().as_secs_f64()*1000.;let input=Input{handle:hwnd(window).0 as usize,scale:window.scale_factor()};
        let current=weak.update(cx,|v,_|->Result<Rect>{
            match std::env::var("RUSTSHOT_QA_GPUI_FULL_SECOND_SCREEN") {
                Err(std::env::VarError::NotPresent) => Ok(v.source.screen_rect(v.source.current)),
                Err(std::env::VarError::NotUnicode(_)) => Err("QA 第二屏开关包含非 Unicode 值；只接受精确字符串 1".into()),
                Ok(value) if value != "1" => Err("QA 第二屏开关只接受精确字符串 1；无效值不回退".into()),
                Ok(_) => {
                    let screen_count=v.source.screen_count();
                    if screen_count <= 1 {
                        return Err("QA 第二屏测试需要实际 screen index 1；不存在时不回退".into());
                    }
                    let rect=v.source.screen_rect(1);
                    if rect.width < 1100 || rect.height < 800 {
                        return Err(format!("QA 第二屏测试的实际 screen index 1 尺寸不足 1100x800：{}x{}；不回退",rect.width,rect.height).into());
                    }
                    let bounds=v.source.bounds;
                    println!("{}",serde_json::json!({
                        "phase":"gpui_full_test_screen","owner_hwnd":input.handle,
                        "owner_pid":std::process::id(),"selected_index":1,
                        "original_source_current":v.source.current,"screen_count":screen_count,
                        "test_rect":[i64::from(rect.x),i64::from(rect.y),i64::from(rect.width),i64::from(rect.height)],
                        "source_bounds":[i64::from(bounds.left),i64::from(bounds.top),i64::from(bounds.width),i64::from(bounds.height)],
                        "qa_only":true,"product_source_current_changed":false
                    }));
                    Ok(rect)
                }
            }
        }).map_err(|e|e.to_string());
        cx.spawn(async move|cx|{
            let result:Result<()>=async{
                let current=current??;
                isolate_input(HWND(input.handle as *mut _))?;
                let pause=Duration::from_millis(240);
                macro_rules! wait {()=>{cx.background_executor().timer(pause).await;}}
                macro_rules! button {($id:expr)=>{{let p=weak.update(cx,|v,_|v.ui_positions.borrow().get($id).copied()).map_err(|e|e.to_string())?.ok_or_else(||format!("找不到工具栏按钮：{}",$id))?;input.click(p);wait!();}}}
                macro_rules! tool {($id:expr)=>{{button!("more");button!($id);}}}
                macro_rules! action {($id:expr)=>{{button!("more");button!("more-actions-tab");button!($id);}}}
                wait!();let(sx,sy)=(current.x+100,current.y+100);
                input.drag((sx,sy),(sx+900,sy+580));wait!();
                weak.update(cx,|v,_|->Result<()>{
                    if !v.ui_positions.borrow().contains_key("property-toolbar") { return Err("选区模式缺少外观属性栏".into()); }
                    Ok(())
                }).map_err(|e|e.to_string())??;
                let canvas_point = ((sx+400) as f32/input.scale, (sy+250) as f32/input.scale);
                let initial_crop = weak.update(cx,|v,_|v.editor.crop.unwrap()).map_err(|e|e.to_string())?;
                input.wheel(canvas_point, 120)?; wait!();
                weak.update(cx,|v,_|->Result<()>{let r=v.editor.crop.unwrap();if r.x+1!=initial_crop.x || r.width!=initial_crop.width+2 {return Err("原生滚轮选区微调失败".into());}Ok(())}).map_err(|e|e.to_string())??;
                input.wheel(canvas_point, -120)?; wait!();
                if weak.update(cx,|v,_|v.editor.crop).map_err(|e|e.to_string())? != Some(initial_crop) {return Err("滚轮选区还原失败".into());}
                button!("rectangle");
                let original = weak.update(cx,|v,_|v.editor.style).map_err(|e|e.to_string())?;
                input.wheel(canvas_point, 120)?; wait!();
                if weak.update(cx,|v,_|v.editor.style.width).map_err(|e|e.to_string())? != original.width+1. {return Err("原生滚轮刻度未按系统设置归一化".into());}
                let chars = weak.update(cx,|v,_|v.wheel_settings.1).map_err(|e|e.to_string())?;
                cx.update_window(own_window, |_,w,cx| {
                    let event = gpui::ScrollWheelEvent {
                        position: gpui::point(gpui::px(canvas_point.0),gpui::px(canvas_point.1)),
                        delta: gpui::ScrollDelta::Lines(gpui::point(chars,0.)), modifiers: gpui::Modifiers {shift:true,..Default::default()}, ..Default::default()
                    };
                    weak.update(cx, |v,cx| v.wheel(&event,w,cx))
                }).map_err(|e|e.to_string())?.map_err(|e|e.to_string())?; wait!();
                if weak.update(cx,|v,_|v.editor.style.radius).map_err(|e|e.to_string())? != original.radius+1. {return Err("Shift 水平滚轮未调整圆角".into());}
                let radius = weak.update(cx,|v,_|v.ui_positions.borrow().get("radius-menu").copied()).map_err(|e|e.to_string())?.ok_or("圆角控件不存在")?;
                input.wheel(radius,120)?;wait!();
                if weak.update(cx,|v,_|v.editor.style.radius).map_err(|e|e.to_string())? != original.radius+2. {return Err("属性控件悬停滚轮失败".into());}
                let width = weak.update(cx,|v,_|v.editor.style.width).map_err(|e|e.to_string())?;
                input.wheel(canvas_point,120)?;wait!();
                if weak.update(cx,|v,_|v.editor.style.width).map_err(|e|e.to_string())? != width+1. {return Err("属性滚轮传播到了画布".into());}
                button!("more");input.wheel(canvas_point,120)?;wait!();
                if weak.update(cx,|v,_|v.editor.style.width).map_err(|e|e.to_string())? != width+1. {return Err("菜单打开时滚轮误修改图形".into());}
                input.key(0x1B);wait!();
                weak.update(cx,|v,cx|{v.editor.set_style(original);v.editor.select_tool(Tool::Pen);cx.notify();}).map_err(|e|e.to_string())?;wait!();
                let pen_style = weak.update(cx,|v,_|v.editor.style).map_err(|e|e.to_string())?;
                let pack = |x:u32,y:u32| LPARAM(((y<<16)|x) as isize);
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONDOWN,WPARAM(1),pack(sx+300,sy+300))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+380,sy+330))?;}wait!();
                input.wheel(((sx+380) as f32/input.scale,(sy+330) as f32/input.scale),120)?;wait!();
                weak.update(cx,|v,_|->Result<()>{if !v.editor.is_dragging() || !v.editor.document.items().is_empty() {return Err("滚轮提前提交正在绘制的笔画".into());}Ok(())}).map_err(|e|e.to_string())??;
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+480,sy+380))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONUP,WPARAM(0),pack(sx+480,sy+380))?;}wait!();
                weak.update(cx,|v,_|->Result<()>{let items=v.editor.document.items();if items.len()!=1 || items[0].annotation.style.width != pen_style.width+1. || items[0].annotation.points.last().unwrap().x != (sx+480) as f32 {return Err("滚轮中绘制没有保持完整笔画".into());}Ok(())}).map_err(|e|e.to_string())??;
                button!("edit");input.drag((sx+380,sy+330),(sx+380,sy+330));wait!();
                input.wheel(canvas_point,120)?;wait!();input.wheel(canvas_point,120)?;wait!();
                button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.width).map_err(|e|e.to_string())? != pen_style.width+1. {return Err("连续滚轮未合并为一次撤销".into());}
                button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.can_undo()).map_err(|e|e.to_string())? {return Err("笔画没有只提交一次".into());}
                weak.update(cx,|v,cx|{v.editor.set_style(pen_style);cx.notify();}).map_err(|e|e.to_string())?;
                println!("{{\"test\":\"native_wheel_crop_shift_properties_popup_draw_and_undo\",\"ok\":true}}");
                button!("pen");
                let corner=(sx+500,sy+240);
                let line:Vec<_>=(0..=100).map(|i|(sx+300+i*2,sy+240)).chain((1..=80).map(|i|(sx+500,sy+240+i*2))).collect();
                input.stroke(&line)?;wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let points=v.editor.document.items()[0].annotation.points.clone();
                    if points.len()<3 || !points.contains(&crate::annotation::Point{x:corner.0 as f32,y:corner.1 as f32}) {return Err("原生铅笔未保留拐角样本".into());}
                    let scene=v.prepared_scene();
                    let crate::scene::Layer::Vector{path,..}=&scene.layers[0] else {return Err("实际铅笔没有使用 GPU 矢量路径".into());};
                    if !path.spans.iter().any(|span|span.end()==crate::annotation::Point{x:corner.0 as f32,y:corner.1 as f32}) {return Err("铅笔平滑丢失直角".into());}
                    let image=v.selected_image()?;let crop=v.selected()?;let offset=(((corner.1-crop.y)*image.width+corner.0-crop.x)*4) as usize;
                    if image.bgra[offset..offset+3] != [0x5c,0x4b,0xff] {return Err("导出未覆盖铅笔原始拐点".into());}
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("undo");
                let curve:Vec<_>=(0..=75).map(|i|{let t=i as f32/75.*std::f32::consts::PI;((sx as f32+650.+90.*t.cos()).round() as u32,(sy as f32+180.+80.*t.sin()).round() as u32)}).collect();
                let curve_present_floor=(std::env::var("RUSTSHOT_QA_GPUI_CURVE_OWNER_DIAG").as_deref()==Ok("1")).then(||curve_owner_present(input));
                input.stroke(&curve)?;wait!();
                let curve_owner_diag=std::env::var("RUSTSHOT_QA_GPUI_CURVE_OWNER_DIAG").as_deref()==Ok("1");
                // 在原实体借用之外读取实际 GPUI scale；失败只保留 None，不改变原 RGB 门。
                let curve_gpui_window_state=if curve_owner_diag {own_window.update(cx,|_,w,_|{
                    let viewport=w.viewport_size();
                    (w.scale_factor(),serde_json::json!({
                        "viewport_logical":[f32::from(viewport.width),f32::from(viewport.height)],
                        "viewport_physical_from_current_scale":[f32::from(viewport.width)*w.scale_factor(),f32::from(viewport.height)*w.scale_factor()],
                        "query_scope":"public window state getter outside paint; not actual curve paint bounds or clip",
                        "content_mask_observed":false,"content_mask":null
                    }))
                }).ok()} else {None};
                let curve_gpui_scale=curve_gpui_window_state.as_ref().map(|(scale,_)|*scale);
                let mut curve_original_rgb_failed=false;
                let mut curve_framebuffer_model=None;
                let curve_rgb_result=weak.update(cx,|v,_|->Result<()>{
                    let scene=v.prepared_scene();let crate::scene::Layer::Vector{path,mesh,style,..}=&scene.layers[0] else {return Err("圆弧未进入矢量层".into());};
                    if !path.spans.iter().any(|span|matches!(span,crate::vector_path::Span::Cubic{..})) {return Err("圆弧没有生成贝塞尔段".into());}
                    let curve_frozen_cpu_probes=curve_owner_diag.then(||[curve[18],curve[25],curve[56]].map(|p|curve_frozen_probe(v,&scene,p)));
                    let curve_framebuffer_model_error=if curve_owner_diag {
                        match curve_framebuffer_source(v) {
                            Ok(source)=>{curve_framebuffer_model=Some(CurveFramebufferModel{bounds:v.source.bounds,source_current:v.source.current,source_identity:crate::scene::SourceIdentity::of(&source),stamp:v.editor.scene_stamp(),crop:v.editor.crop,scene:scene.clone(),mesh:mesh.clone()});None},
                            Err(error)=>Some(error.to_string()),
                        }
                    } else {None};
                    let curve_scene_diagnostic=curve_owner_diag.then(||{
                        let annotation=v.editor.document.items().first().map(|item|&item.annotation);
                        serde_json::json!({
                            "frozen_cpu_original_probe_points":curve_frozen_cpu_probes,"framebuffer_model_qualification_error":curve_framebuffer_model_error,"scene_errors":scene.errors,"scene_layer_count":scene.layers.len(),
                            "vector_mesh_counts":{"vertices":mesh.vertices.len(),"indices":mesh.indices.len(),"edges":mesh.edges.len(),"normals":mesh.normals.len()},
                            "scene_vector_style":{"color_rgb_u32":style.color,"opacity":style.opacity,"width":style.width,"pencil_smooth":style.pencil_smooth,"dashed":style.dashed},
                            "document_annotation_count":v.editor.document.items().len(),
                            "annotation_point_count":annotation.map(|a|a.points.len()),
                            "annotation_style":annotation.map(|a|serde_json::json!({"color_rgb_u32":a.style.color,"opacity":a.style.opacity,"width":a.style.width,"pencil_smooth":a.style.pencil_smooth,"dashed":a.style.dashed})),
                            "original_probe_points_present":annotation.map(|a|[18_usize,25,56].map(|i|a.points.contains(&crate::annotation::Point{x:curve[i].0 as f32,y:curve[i].1 as f32}))),
                            "scope":"current CPU Scene/document observation; not a submitted/presented curve receipt"
                        })
                    });
                    let curve_before=curve_owner_diag.then(||curve_owner_snapshot(input,v.source.bounds,&[curve[18],curve[25],curve[56]]));
                    let curve_capture_begin=curve_owner_diag.then(curve_owner_qpc).flatten();
                    let visible=DesktopCapture::capture()?;let mut matches=0;
                    let curve_capture_end=curve_owner_diag.then(curve_owner_qpc).flatten();
                    let curve_after=curve_owner_diag.then(||curve_owner_snapshot(input,visible.bounds,&[curve[18],curve[25],curve[56]]));
                    let mut curve_rgb_gate_matched=curve_owner_diag.then_some([false;3]);
                    let mut curve_rgb_bgra=curve_owner_diag.then_some([None::<[u8;4]>;3]);
                    for i in [18_usize,25,56] {let (x,y)=curve[i];let pixel=visible.read_region(Rect{x,y,width:1,height:1})?;
                        if let Some(values)=&mut curve_rgb_bgra {values[if i==18 {0} else if i==25 {1} else {2}]=pixel.bgra.get(..4).and_then(|bytes|bytes.try_into().ok());}
                        if (pixel.bgra[0] as i32-0x5c).abs()<24 && (pixel.bgra[1] as i32-0x4b).abs()<24 && (pixel.bgra[2] as i32-0xff).abs()<24 {matches+=1;if let Some(flags)=&mut curve_rgb_gate_matched {flags[if i==18 {0} else if i==25 {1} else {2}]=true;}}
                    }
                    curve_original_rgb_failed=matches<2;
                    if let Some(flags)=curve_rgb_gate_matched {println!("{}",serde_json::json!({
                            "phase":"gpui_curve_owner_diagnostic","diagnostic_only":true,
                            "gpui_scale_before_entity_update":curve_gpui_scale,"gpui_scale_query_succeeded":curve_gpui_scale.is_some(),
                            "gpui_window_state_outside_paint":curve_gpui_window_state.as_ref().map(|(_,state)|state),
                            "stroke_before_present_properties":curve_present_floor,"current_scene_document":curve_scene_diagnostic,
                            "input_initial_gpui_scale":input.scale,"capture_begin_qpc":curve_capture_begin,"capture_end_qpc":curve_capture_end,
                            "before_capture_source_bounds":curve_before,"after_capture_visible_bounds":curve_after,
                            "source_bounds_equal_visible":v.source.bounds==visible.bounds,
                            "rgb_gate_points":[
                                {"curve_index":18,"rgb_gate_matched":flags[0],"captured_bgra":curve_rgb_bgra.as_ref().and_then(|values|values[0])},
                                {"curve_index":25,"rgb_gate_matched":flags[1],"captured_bgra":curve_rgb_bgra.as_ref().and_then(|values|values[1])},
                                {"curve_index":56,"rgb_gate_matched":flags[2],"captured_bgra":curve_rgb_bgra.as_ref().and_then(|values|values[2])}
                            ],
                            "rgb_gate_match_count":matches,"original_rgb_gate_failed":matches<2
                        }));}
                    if matches<2 {
                        return Err("实际 GPU 贝塞尔曲线与输入路径不一致".into());
                    }
                    Ok(())
                }).map_err(|e|e.to_string())?;
                if curve_owner_diag && curve_original_rgb_failed {
                    let diagnostic:Result<serde_json::Value>=async {
                        let model=curve_framebuffer_model.ok_or("原RGB失败，但framebuffer模型资格未建立")?;
                        weak.update(cx,|v,_|model.verify(v)).map_err(|e|e.to_string())??;
                        let baseline=own_window.update(cx,|_,w,_|curve_framebuffer_window(input,w,model.bounds)).map_err(|e|e.to_string())??;
                        let points=[curve[18],curve[25],curve[56]];
                        let min_x=points.iter().map(|p|p.0).min().ok_or("缺少原probe")?;
                        let min_y=points.iter().map(|p|p.1).min().ok_or("缺少原probe")?;
                        let max_x=points.iter().map(|p|p.0).max().ok_or("缺少原probe")?;
                        let max_y=points.iter().map(|p|p.1).max().ok_or("缺少原probe")?;
                        let roi=[min_x,min_y,max_x.checked_sub(min_x).and_then(|v|v.checked_add(1)).ok_or("selfROI宽度溢出")?,max_y.checked_sub(min_y).and_then(|v|v.checked_add(1)).ok_or("selfROI高度溢出")?];
                        let expected_bytes=(roi[2] as usize).checked_mul(roi[3] as usize).and_then(|v|v.checked_mul(4)).ok_or("selfROI长度溢出")?;
                        if !(Rect{x:roi[0],y:roi[1],width:roi[2],height:roi[3]}).valid(baseline.physical[0],baseline.physical[1]) {return Err("selfROI未落在原客户区".into());}
                        let request_begin_qpc=curve_owner_qpc();
                        let before_present=curve_owner_present(input);
                        own_window.update(cx,|_,w,_|->Result<()>{
                            if curve_framebuffer_window(input,w,model.bounds)?!=baseline {return Err("request前原self几何/身份改变".into());}
                            w.request_frame_capture(roi)?;
                            if curve_framebuffer_window(input,w,model.bounds)?!=baseline {return Err("request后原self几何/身份改变".into());}
                            Ok(())
                        }).map_err(|e|e.to_string())??;
                        let request_end_qpc=curve_owner_qpc();
                        for poll in 1..=20_u32 {
                            cx.background_executor().timer(Duration::from_millis(25)).await;
                            weak.update(cx,|v,_|model.verify(v)).map_err(|e|e.to_string())??;
                            let captured=own_window.update(cx,|_,w,_|->Result<_>{
                                if curve_framebuffer_window(input,w,model.bounds)?!=baseline {return Err("take前原self几何/身份改变".into());}
                                let value=w.take_frame_capture();
                                if curve_framebuffer_window(input,w,model.bounds)?!=baseline {return Err("take后原self几何/身份改变".into());}
                                Ok(value)
                            }).map_err(|e|e.to_string())??;
                            if let Some(captured)=captured {
                                let mut bytes=captured?;
                                if bytes.len()!=expected_bytes {bytes.fill(0);return Err("selfROI紧密BGRA长度错误".into());}
                                weak.update(cx,|v,_|model.verify(v)).map_err(|e|e.to_string())??;
                                let mut samples=Vec::with_capacity(3);
                                for (slot,p) in points.iter().enumerate() {
                                    let offset=(p.1-roi[1]) as usize;
                                    let offset=offset.checked_mul(roi[2] as usize).and_then(|v|v.checked_add((p.0-roi[0]) as usize)).and_then(|v|v.checked_mul(4)).ok_or("selfROI probe索引溢出")?;
                                    let end=offset.checked_add(4).ok_or("selfROI probe末端溢出")?;
                                    let bgra:[u8;4]=bytes.get(offset..end).ok_or("selfROI probe越界")?.try_into().map_err(|_|"selfROI probe长度错误")?;
                                    samples.push(serde_json::json!({"curve_index":([18_usize,25,56][slot]),"client_physical_point":[p.0,p.1],"self_backbuffer_bgra":bgra}));
                                }
                                bytes.fill(0);
                                let after_present=curve_owner_present(input);
                                return Ok(serde_json::json!({"owner_hwnd":input.handle,"owner_pid":baseline.pid,"owner_tid":baseline.tid,"original_identity_and_mapping_verified":true,"scale":baseline.scale,"outer":baseline.outer,"client":baseline.client,"client_origin":baseline.origin,"physical_viewport":baseline.physical,"roi_client_physical":roi,"byte_count":expected_bytes,"request_count":1,"completion_poll":poll,"request_begin_qpc":request_begin_qpc,"request_end_qpc":request_end_qpc,"take_completed_qpc":curve_owner_qpc(),"source_current":model.source_current,"scene_stamp":model.stamp,"crop":model.crop.map(|r|[r.x,r.y,r.width,r.height]),"scene_arc_identity":std::sync::Arc::as_ptr(&model.scene) as usize,"mesh_arc_identity":std::sync::Arc::as_ptr(&model.mesh) as usize,"source_scene_mesh_crop_stable":true,"request_before_present_properties":before_present,"take_after_present_properties":after_present,"points":samples}));
                            }
                        }
                        Err("单次selfROI请求在20次completion poll内未完成；不再请求或重试颜色".into())
                    }.await;
                    println!("{}",serde_json::json!({"phase":"gpui_curve_self_framebuffer_diagnostic","diagnostic_only":true,"diagnostic_completed":diagnostic.is_ok(),"diag_error":diagnostic.as_ref().err().map(|e|e.to_string()),"observation":diagnostic.ok(),"original_rgb_gate_failed":true,"original_rgb_error_preserved":true,"capture_curve_epoch_verified":false,"performance_verified":false,"scope":"selfBackbufferROI only; same-model-new-redraw after frozen original RGB FAIL; request adds refresh/redraw; completion polling never retries color; Present properties do not bind curve epoch; frozen desktop imagery may underlie this own framebuffer; no new Desktop capture, PNG, Clipboard or foreign-window pixels"}));
                }
                curve_rgb_result?;
                button!("edit");input.drag(curve[25],curve[25]);wait!();
                button!("pencil-smooth");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.pencil_smooth).map_err(|e|e.to_string())? {return Err("铅笔原始模式切换失败".into());}
                button!("undo");
                if !weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.pencil_smooth).map_err(|e|e.to_string())? {return Err("铅笔模式撤销失败".into());}
                button!("pen");
                weak.update(cx,|v,cx|{let mut style=v.editor.style;style.pencil_smooth=true;v.editor.set_style(style);cx.notify();}).map_err(|e|e.to_string())?;
                button!("undo");
                std::fs::create_dir_all("target/qa/pencil")?;
                let saved_pen_style=weak.update(cx,|v,cx|{
                    let old=v.editor.style;
                    let mut backdrop=crate::annotation::Annotation::new(Tool::Rectangle,
                        crate::annotation::Point{x:(sx+240) as f32,y:(sy+160) as f32},
                        crate::annotation::Style{color:0xffffff,filled:true,..Default::default()});
                    backdrop.update(crate::annotation::Point{x:(sx+800) as f32,y:(sy+500) as f32});
                    v.editor.document.insert(backdrop);
                    v.editor.selected=None;
                    let mut style=old;style.width=1.;style.color=0x25374b;style.pencil_smooth=true;
                    v.editor.set_style(style);cx.notify();old
                }).map_err(|e|e.to_string())?;wait!();
                let diagonal:Vec<_>=(0..320).map(|i|(sx+260+i,sy+190+(i as f32*0.37).round() as u32)).collect();
                input.stroke(&diagonal)?;wait!();
                weak.update(cx,|v,cx|->Result<()>{
                    let a=&v.editor.document.items()[1].annotation;
                    if a.points.len()<200 || a.vector_path().spans.len()>=a.points.len()/3 {return Err("原生斜线采样未减少台阶段".into());}
                    v.editor.selected=None;v.editor.selection.clear();cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;wait!();
                input.hover((sx+50,sy+50))?;wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let rect=Rect{x:sx+248,y:sy+178,width:348,height:140};
                    let native=DesktopCapture::capture()?.read_region(rect)?;
                    let mut exported=v.source.read_region(rect)?;v.prepared_scene().apply(&mut exported,rect);
                    let mut error=0.;let mut count=0;
                    for x in sx+270..sx+570 {
                        let ideal=sy as f32+190.+(x as f32+0.5-(sx+260) as f32)*0.37;
                        let mut weight=0.;let mut moment=0.;
                        for y in (ideal as u32-6)..(ideal as u32+7) {
                            let i=(((y-rect.y)*rect.width+x-rect.x)*4) as usize;
                            let ink=255.-native.bgra[i+1] as f32;
                            weight+=ink;moment+=ink*(y as f32+0.5);
                        }
                        if weight<80. {return Err(format!("GPU 细线出现缺口：x={x}").into());}
                        error+=(moment/weight-ideal).powi(2);count+=1;
                    }
                    let rms=(error/count as f32).sqrt();
                    export::save_png(native,&PathBuf::from("target/qa/pencil/native-thin-stroke.png"))?;
                    export::save_png(exported,&PathBuf::from("target/qa/pencil/export-thin-stroke.png"))?;
                    if rms>0.24 {return Err(format!("GPU 细线仍有明显台阶：RMS={rms}").into());}
                    println!("{{\"test\":\"native_thin_pencil_continuous_round_stroke\",\"rms_px\":{rms:.4},\"ok\":true}}");
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("undo");
                // 浅斜率与陡斜率也走原生消息，防止只把某一条斜线调得好看。
                for (name,slope,phase) in [("shallow",0.08_f32,0.3_f32),("steep",0.68,0.7)] {
                    let (x0,y0)=(sx+260,sy+190);
                    let trace:Vec<_>=(0..320).map(|i|(x0+i,y0+(i as f32*slope+phase).round() as u32)).collect();
                    input.stroke(&trace)?;wait!();
                    weak.update(cx,|v,cx|{v.editor.selected=None;v.editor.selection.clear();cx.notify();}).map_err(|e|e.to_string())?;wait!();
                    input.hover((sx+50,sy+50))?;wait!();
                    weak.update(cx,|v,_|->Result<()>{
                        let rect=Rect{x:x0-12,y:y0-12,width:344,height:(320.*slope+26.).ceil() as u32};
                        let native=DesktopCapture::capture()?.read_region(rect)?;
                        let mut exported=v.source.read_region(rect)?;
                        v.prepared_scene().apply(&mut exported,rect);
                        let mut report=Vec::new();
                        for (label,image) in [("preview",&native),("export",&exported)] {
                            let mut error=0.;let mut count=0;
                            for x in x0+8..x0+312 {
                                let ideal=y0 as f32+(x as f32+0.5-x0 as f32)*slope+phase;
                                let mut weight=0.;let mut moment=0.;
                                for y in (ideal as u32-6)..(ideal as u32+7) {
                                    let i=(((y-rect.y)*rect.width+x-rect.x)*4) as usize;
                                    let ink=255.-image.bgra[i+1] as f32;
                                    weight+=ink;moment+=ink*(y as f32+0.5);
                                }
                                if weight<80. {return Err(format!("{name}/{label} 细线缺口：x={x}").into());}
                                error+=(moment/weight-ideal).powi(2)/(1.+slope*slope);count+=1;
                            }
                            let rms=(error/count as f32).sqrt();
                            if rms>0.26 {return Err(format!("{name}/{label} 细线残余台阶：RMS={rms}").into());}
                            report.push((label,rms));
                        }
                        export::save_png(native,&PathBuf::from(format!("target/qa/pencil/native-thin-{name}.png")))?;
                        export::save_png(exported,&PathBuf::from(format!("target/qa/pencil/export-thin-{name}.png")))?;
                        println!("{{\"test\":\"native_pencil_quantized_{name}_preview_export\",\"preview_rms_px\":{:.4},\"export_rms_px\":{:.4},\"ok\":true}}",report[0].1,report[1].1);
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    button!("undo");
                }
                // 两个直角只隔 2 px；验证原生密集采样没有被转角去重磨成曲线。
                for (name, turn_back) in [("step",false),("hairpin",true)] {
                    let (x0,y0)=(sx+300,sy+260);
                    let mut trace:Vec<_>=(0..=160).map(|i|(x0+i,y0)).collect();
                    trace.extend([(x0+160,y0+1),(x0+160,y0+2)]);
                    if turn_back {
                        trace.extend((0..160).rev().map(|i|(x0+i,y0+2)));
                    } else {
                        trace.extend((161..=320).map(|i|(x0+i,y0+2)));
                    }
                    input.stroke(&trace)?;wait!();
                    weak.update(cx,|v,cx|->Result<()>{
                        let a=&v.editor.document.items()[1].annotation;
                        let path=a.vector_path();
                        for corner in [(x0+160,y0),(x0+160,y0+2)] {
                            let corner=crate::annotation::Point{x:corner.0 as f32,y:corner.1 as f32};
                            if !path.spans.iter().any(|s|s.end()==corner) {
                                return Err("近距离原生铅笔折角丢失顶点".into());
                            }
                        }
                        if path.spans.iter().any(|s|!matches!(s,crate::vector_path::Span::Line{..})) {
                            return Err("近距离台阶/回折仍被磨成贝塞尔曲线".into());
                        }
                        v.editor.selected=None;v.editor.selection.clear();cx.notify();Ok(())
                    }).map_err(|e|e.to_string())??;wait!();
                    input.hover((sx+50,sy+50))?;wait!();
                    weak.update(cx,|v,_|->Result<()>{
                        let rect=Rect{x:x0+145,y:y0-8,width:30,height:20};
                        let native=DesktopCapture::capture()?.read_region(rect)?;
                        let mut exported=v.source.read_region(rect)?;
                        v.prepared_scene().apply(&mut exported,rect);
                        let mut missing=None;
                        for (label,image) in [("GPU",&native),("导出",&exported)] {
                            // 台阶下沿向右延伸；回折下沿向左延伸。取各自线段内侧的像素。
                            let lower_x=x0+160-u32::from(turn_back);
                            for (x,y) in [(x0+159,y0),(lower_x,y0+2)] {
                                let i=(((y-rect.y)*rect.width+x-rect.x)*4) as usize;
                                if image.bgra[i+1]>210 {
                                    missing=Some(format!("近距离折角缺失：{name}/{label} ({x},{y})={:?}",&image.bgra[i..i+4]));
                                }
                            }
                        }
                        export::save_png(native,&PathBuf::from(format!("target/qa/pencil/native-tight-{name}.png")))?;
                        export::save_png(exported,&PathBuf::from(format!("target/qa/pencil/export-tight-{name}.png")))?;
                        if let Some(error)=missing {return Err(error.into());}
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    button!("undo");
                }
                println!("{{\"test\":\"native_pencil_nearby_step_hairpin_preserved_pixels_undo\",\"ok\":true}}");
                button!("undo");
                weak.update(cx,|v,cx|->Result<()>{
                    if !v.editor.document.items().is_empty() || v.editor.document.can_undo(){return Err("细线验收未正确撤销两个对象".into());}
                    v.editor.set_style(saved_pen_style);cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;
                // 仅注入当前 GPUI 窗口的按下修饰键；后续 MOVE/UP/滚轮走原生消息。
                cx.update_window(own_window,|_,w,cx| {
                    let event=gpui::MouseDownEvent{button:gpui::MouseButton::Left,
                        position:gpui::point(gpui::px((sx+300) as f32/input.scale),gpui::px((sy+180) as f32/input.scale)),
                        modifiers:gpui::Modifiers{shift:true,..Default::default()},click_count:1,first_mouse:false};
                    weak.update(cx,|v,cx|v.down(&event,w,cx))
                }).map_err(|e|e.to_string())?.map_err(|e|e.to_string())?;
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+480,sy+240))?;}wait!();
                input.wheel(canvas_point,120)?;wait!();
                if weak.update(cx,|v,_|!v.editor.is_dragging()||!v.editor.document.items().is_empty()).map_err(|e|e.to_string())? {return Err("Shift 直线滚轮提前提交".into());}
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+640,sy+320))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONUP,WPARAM(0),pack(sx+640,sy+320))?;}wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let a=&v.editor.document.items()[0].annotation;let end=*a.points.last().unwrap();
                    let angle=(end.y-a.points[0].y).atan2(end.x-a.points[0].x).to_degrees();
                    if a.points.len()!=2 || (angle-22.5).abs()>0.2 || a.style.width<=saved_pen_style.width {return Err("Shift 铅笔未保持 22.5° 直线或滚轮线宽".into());}
                    let visible=DesktopCapture::capture()?;
                    for t in [0.2,0.4,0.6] {let p=a.vector_path().spans[0].at(t);let pixel=visible.read_region(Rect{x:p.x as u32,y:p.y as u32,width:1,height:1})?;if pixel.bgra[..3]!=[0x5c,0x4b,0xff] {return Err("GPUI 直线铅笔显示像素不一致".into());}}
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("undo");
                weak.update(cx,|v,cx|->Result<()>{if !v.editor.document.items().is_empty()||v.editor.document.can_undo(){return Err("Shift 直线非一次撤销".into());}v.editor.set_style(saved_pen_style);cx.notify();Ok(())}).map_err(|e|e.to_string())??;
                println!("{{\"test\":\"gpui_shift_pen_native_moves_wheel_pixels_atomic_undo\",\"ok\":true}}");
                button!("arrow");
                let saved_arrow_style=weak.update(cx,|v,cx|{
                    let old=v.editor.style;let mut backdrop=crate::annotation::Annotation::new(Tool::Rectangle,crate::annotation::Point{x:(sx+260) as f32,y:(sy+150) as f32},crate::annotation::Style{color:0xffffff,filled:true,..Default::default()});
                    backdrop.update(crate::annotation::Point{x:(sx+830) as f32,y:(sy+465) as f32});v.editor.document.insert(backdrop);v.editor.selected=None;cx.notify();old
                }).map_err(|e|e.to_string())?;wait!();
                for preset in [crate::arrow_geometry::ArrowPreset::FilledSingle,crate::arrow_geometry::ArrowPreset::FilledDouble,crate::arrow_geometry::ArrowPreset::Dart] {
                    // 端点与头型两组分段组合出预设。
                    let (ends,head)=match preset {crate::arrow_geometry::ArrowPreset::FilledSingle=>("arrow-ends-single","arrow-head-triangle"),crate::arrow_geometry::ArrowPreset::FilledDouble=>("arrow-ends-double","arrow-head-triangle"),_=>("arrow-ends-single","arrow-head-dart")};
                    button!(ends);button!(head);
                    input.drag((sx+320,sy+220),(sx+760,sy+220));wait!();
                    input.drag((sx+540,sy+220),(sx+540,sy+390));wait!();
                    weak.update(cx,|v,cx|->Result<()>{
                        let a=&v.editor.document.items()[1].annotation;if crate::arrow_geometry::ArrowPreset::of(a.style)!=preset||a.points.len()!=3||a.vector_path().fills.is_empty(){return Err("箭头样式选择或曲线编辑失败".into());}
                        let scene=v.prepared_scene();if !matches!(&scene.layers[1],crate::scene::Layer::Vector{..}){return Err("实心箭头没有使用 GPU 矢量层".into());}
                        v.editor.selected=None;v.editor.selection.clear();cx.notify();Ok(())
                    }).map_err(|e|e.to_string())??;wait!();
                    input.hover((sx+50,sy+50))?;wait!();
                    weak.update(cx,|v,_|->Result<()>{
                        let rect=Rect{x:sx+275,y:sy+165,width:540,height:280};
                        let native=DesktopCapture::capture()?.read_region(rect)?;let mut exported=v.source.read_region(rect)?;v.prepared_scene().apply(&mut exported,rect);
                        let mut ink=0;for (actual,expected) in native.bgra.chunks_exact(4).zip(exported.bgra.chunks_exact(4)){if expected[..3]==[0x5c,0x4b,0xff]{ink+=1;if (actual[0] as i32-0x5c).abs()>18||(actual[1] as i32-0x4b).abs()>18||(actual[2] as i32-0xff).abs()>18{return Err("GPUI 实心箭头与导出实体像素不一致".into());}}}
                        if ink<1200 {return Err("实心箭头覆盖不足".into());}
                        std::fs::create_dir_all("target/qa/arrow")?;export::save_png(native,&PathBuf::from(format!("target/qa/arrow/native-{}.png",preset.id())))?;export::save_png(exported,&PathBuf::from(format!("target/qa/arrow/export-{}.png",preset.id())))?;
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    // 一次撤销曲线编辑，再撤销创建；保留白色背景。
                    button!("undo");button!("undo");
                }
                button!("undo");
                weak.update(cx,|v,cx|->Result<()>{if !v.editor.document.items().is_empty()||v.editor.document.can_undo(){return Err("实心箭头撤销残留".into());}v.editor.set_style(saved_arrow_style);cx.notify();Ok(())}).map_err(|e|e.to_string())??;
                println!("{{\"test\":\"native_arrow_filled_double_dart_curve_vector_pixels_popup_undo\",\"ok\":true}}");

                // 源图恢复笔迹：实际拖动和滚轮；读取比较像素，但不保存用户原图。
                weak.update(cx,|v,cx|{
                    let mut backdrop=crate::annotation::Annotation::new(Tool::Rectangle,crate::annotation::Point{x:(sx+220) as f32,y:(sy+140) as f32},crate::annotation::Style{color:0xe15b67,filled:true,..Default::default()});
                    backdrop.update(crate::annotation::Point{x:(sx+780) as f32,y:(sy+460) as f32});v.editor.document.insert(backdrop);v.editor.selected=None;v.editor.selection.clear();cx.notify();
                }).map_err(|e|e.to_string())?;wait!();
                tool!("pixel-eraser");
                let restore_width=weak.update(cx,|v,_|v.editor.style.width).map_err(|e|e.to_string())?;
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONDOWN,WPARAM(1),pack(sx+280,sy+270))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+480,sy+270))?;}wait!();
                input.wheel(((sx+480) as f32/input.scale,(sy+270) as f32/input.scale),120)?;wait!();
                if weak.update(cx,|v,_|!v.editor.is_dragging()||v.editor.document.items().len()!=1).map_err(|e|e.to_string())?{return Err("局部擦除滚轮提前提交对象".into());}
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+650,sy+270))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONUP,WPARAM(0),pack(sx+650,sy+270))?;}wait!();
                weak.update(cx,|v,cx|->Result<()>{
                    if v.editor.document.items().len()!=2 {return Err("局部擦除没有保留下层对象".into());}
                    let a=&v.editor.document.items()[1].annotation;
                    if v.editor.selected.is_some() || v.editor.hover(((sx+400) as i32,(sy+270) as i32),6.*input.scale)!=crate::interaction::Hover::Crosshair {return Err("局部擦除抬起后应保持画笔光标".into());}
                    if a.tool!=Tool::PixelEraser||a.style.width!=restore_width+1. {return Err("局部擦除滚轮直径未生效".into());}
                    let scene=v.prepared_scene();if !matches!(&scene.layers[1],crate::scene::Layer::Restore{..}){return Err("擦除笔迹未建立原图恢复层".into());}
                    v.editor.selected=None;v.editor.selection.clear();cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;wait!();
                input.hover((sx+50,sy+50))?;wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let native=DesktopCapture::capture()?;let crop=v.selected()?;let output=v.selected_image()?;
                    for x in [sx+350,sx+400,sx+500] {
                        let rect=Rect{x,y:sy+270,width:1,height:1};let expected=v.source_pixels().read(rect);let actual=native.read_region(rect)?;
                        let i=(((rect.y-crop.y)*output.width+x-crop.x)*4) as usize;
                        if actual.bgra[..3]!=expected.bgra[..3]||output.bgra[i..i+4]!=expected.bgra[..4]{return Err(format!("擦除像素差异 x={x}：原图={:?}，GPU={:?}，输出={:?}，笔迹={:?}",expected.bgra,actual.bgra,&output.bgra[i..i+4],v.editor.document.items()[1].annotation).into());}
                    }
                    let r=&v.editor.document.items()[0].annotation;
                    if r.style.color!=0xe15b67||r.points[0].x!=(sx+220) as f32 {return Err("擦除修改了下层标注数据".into());}Ok(())
                }).map_err(|e|e.to_string())??;
                button!("edit");input.drag((sx+400,sy+270),(sx+410,sy+290));wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let a=v.editor.selected_annotation().ok_or("恢复笔迹无法在编辑工具中选中")?;
                    if a.tool!=Tool::PixelEraser||a.points[0].x!=(sx+290) as f32||a.points[0].y!=(sy+290) as f32||!v.ui_positions.borrow().contains_key("angle-menu"){return Err("恢复笔迹对象移动或变换属性失败".into());}Ok(())
                }).map_err(|e|e.to_string())??;
                button!("angle-menu");button!("rotate-right");button!("angle-menu");button!("undo");button!("undo");
                weak.update(cx,|v,cx|{v.editor.select_all_objects();cx.notify();}).map_err(|e|e.to_string())?;wait!();
                weak.update(cx,|v,cx|->Result<()>{
                    let b=v.toolbar_layout(v.selected()?,input.scale).properties;
                    if v.ui_positions.borrow().contains_key("width-menu"){return Err("多选恢复笔迹应只显示变换属性".into());}
                    for id in ["angle-menu","arrange-menu"] {let (x,y)=v.ui_positions.borrow().get(id).copied().ok_or("恢复笔迹组合操作缺失")?;if x<b.x||x>b.right()||y<b.y||y>b.bottom(){return Err("恢复笔迹组合属性越界".into());}}
                    v.editor.selected=None;v.editor.selection.clear();cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;
                button!("undo");button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.can_undo()).map_err(|e|e.to_string())?{return Err("擦除与下层对象撤销次数不一致".into());}
                println!("{{\"test\":\"native_restore_source_wheel_sampling_atomic_undo\",\"ok\":true}}");
                // 双绕圈轮廓必须仍是实心；导出与实际辅助遮罩共用非零环绕规则。
                weak.update(cx,|v,cx|{
                    let mut backdrop=crate::annotation::Annotation::new(Tool::Rectangle,crate::annotation::Point{x:(sx+200) as f32,y:(sy+120) as f32},crate::annotation::Style{color:0xffffff,filled:true,..Default::default()});
                    backdrop.update(crate::annotation::Point{x:(sx+800) as f32,y:(sy+480) as f32});v.editor.document.insert(backdrop);v.editor.selected=None;v.editor.selection.clear();cx.notify();
                }).map_err(|e|e.to_string())?;wait!();tool!("lasso");
                let loop_points=[(sx+240,sy+140),(sx+740,sy+150),(sx+650,sy+400),(sx+300,sy+450),(sx+240,sy+140),(sx+740,sy+150),(sx+650,sy+400),(sx+300,sy+450),(sx+240,sy+140)];
                input.stroke(&loop_points)?;wait!();
                weak.update(cx,|v,_|->Result<()>{
                    if v.editor.document.crop_mask.is_some()||v.editor.lasso_draft.is_none()||v.editor.document.items().len()!=1 {return Err("套索抬起应只形成草稿".into());}
                    let image=v.selected_image()?;
                    if image.width!=500||image.height!=310||v.editor.document.crop_mask.is_some(){return Err("套索草稿紧裁剪或只读预览有误".into());}
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("apply-lasso");
                let applied_mask=weak.update(cx,|v,_|v.editor.document.crop_mask.clone()).map_err(|e|e.to_string())?;
                weak.update(cx,|v,_|->Result<()>{
                    if v.editor.lasso_draft.is_some()||v.editor.document.crop_mask.is_none()||v.editor.document.items().len()!=1 {return Err("套索应用改变了标注对象".into());}
                    let rect=v.output_rect()?;let native=DesktopCapture::capture()?.read_region(rect)?;let output=v.selected_image()?;
                    let offset=|x:u32,y:u32|(((sy+y-rect.y)*rect.width+sx+x-rect.x)*4) as usize;
                    let inside=offset(500,300);let outside=offset(260,430);
                    if native.bgra[inside..inside+3]!=[255,255,255]||output.bgra[inside..inside+4]!=[255,255,255,255]{return Err("非零套索的双绕圈内部被误挖空".into());}
                    if native.bgra[outside..outside+3].iter().any(|&c|!(148..=158).contains(&c))||output.bgra[outside..outside+4]!=[0,0,0,0]{return Err("套索外部预览遮罩或输出 alpha 有误".into());}
                    if !output.bgra.chunks_exact(4).any(|p|p[3]>0&&p[3]<255){return Err("套索边缘没有抗锯齿 alpha".into());}
                    let layout=v.toolbar_layout(v.selected()?,input.scale);
                    for id in ["apply-lasso","clear-lasso"] {let (x,y)=v.ui_positions.borrow().get(id).copied().ok_or("套索属性按钮缺失")?;let b=layout.properties;if x<b.x||x>b.right()||y<b.y||y>b.bottom(){return Err("套索属性按钮超出工具栏".into());}}
                    std::fs::create_dir_all("target/qa/mask")?;export::save_png(native,&PathBuf::from("target/qa/mask/native-lasso.png"))?;export::save_png(output,&PathBuf::from("target/qa/mask/export-lasso.png"))?;Ok(())
                }).map_err(|e|e.to_string())??;
                let small_loop=[(sx+340,sy+220),(sx+600,sy+220),(sx+600,sy+350),(sx+340,sy+350),(sx+340,sy+220)];
                input.stroke(&small_loop)?;wait!();input.key(0x1B);wait!();
                if weak.update(cx,|v,_|v.editor.lasso_draft.is_some()||v.editor.document.crop_mask!=applied_mask).map_err(|e|e.to_string())?{return Err("Esc 取消套索草稿丢失已应用轮廓".into());}
                input.stroke(&small_loop)?;wait!();
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONDOWN,WPARAM(2),pack(sx+500,sy+280))?;PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONUP,WPARAM(0),pack(sx+500,sy+280))?;}wait!();
                if weak.update(cx,|v,_|v.editor.lasso_draft.is_some()||v.editor.document.crop_mask!=applied_mask).map_err(|e|e.to_string())?{return Err("右键取消套索草稿丢失已应用轮廓".into());}
                input.key(0x1B);wait!();
                if weak.update(cx,|v,_|v.editor.tool!=Tool::Edit||v.editor.document.crop_mask!=applied_mask||v.editor.crop!=Some(initial_crop)).map_err(|e|e.to_string())?{return Err("套索退出未保留裁剪范围".into());}
                input.key(0x4F);wait!();input.stroke(&small_loop)?;wait!();input.key(0x0D);wait!();
                if weak.update(cx,|v,_|v.editor.tool!=Tool::Lasso||v.editor.lasso_draft.is_some()||v.editor.document.crop_mask==applied_mask).map_err(|e|e.to_string())?{return Err("套索 Enter 应用失败".into());}
                button!("undo");button!("clear-lasso");
                if weak.update(cx,|v,_|v.editor.document.crop_mask.is_some()).map_err(|e|e.to_string())?{return Err("清除套索按钮失败".into());}
                button!("undo");
                if weak.update(cx,|v,_|v.editor.document.crop_mask!=applied_mask).map_err(|e|e.to_string())?{return Err("清除套索不可撤销".into());}
                button!("undo");button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.crop_mask.is_some()||v.editor.document.can_undo()).map_err(|e|e.to_string())?{return Err("套索与标注撤销历史残留".into());}
                println!("{{\"test\":\"native_lasso_nonzero_draft_apply_alpha_pixels_clear_undo\",\"ok\":true}}");
                button!("arrow");
                input.drag((sx+400,sy+180),(sx+400,sy+180));wait!();
                if weak.update(cx,|v,_|!v.editor.has_pending_drawing() || !v.editor.document.items().is_empty()).map_err(|e|e.to_string())? {return Err("箭头首击未进入终点预览".into());}
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE),pack(sx+700,sy+330))?;}wait!();
                input.drag((sx+700,sy+330),(sx+700,sy+330));wait!();
                if weak.update(cx,|v,_|v.editor.has_pending_drawing() || v.editor.document.items().len()!=1).map_err(|e|e.to_string())? {return Err("箭头第二击未完成一个对象".into());}
                button!("undo");input.drag((sx+400,sy+180),(sx+400,sy+180));wait!();input.key(0x1B);wait!();
                if weak.update(cx,|v,_|v.editor.has_pending_drawing()||v.editor.document.can_undo()).map_err(|e|e.to_string())? {return Err("取消箭头草稿产生残留或撤销记录".into());}
                input.drag((sx+400,sy+180),(sx+400,sy+180));wait!();
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONDOWN,WPARAM(2),pack(sx+450,sy+200))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONUP,WPARAM(0),pack(sx+450,sy+200))?;}wait!();
                if weak.update(cx,|v,_|v.editor.has_pending_drawing()||v.editor.crop!=Some(initial_crop)).map_err(|e|e.to_string())? {return Err("右键取消箭头误重选截图".into());}
                println!("{{\"test\":\"native_vector_pencil_corner_bezier_pixels_arrow_two_click_cancel\",\"ok\":true}}");

                button!("rectangle");
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_LBUTTONDOWN,WPARAM(1),pack(sx+100,sy+100))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_MOUSEMOVE,WPARAM(SMOKE_MOVE | 1),pack(sx+200,sy+200))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONDOWN,WPARAM(2),pack(sx+200,sy+200))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONUP,WPARAM(0),pack(sx+200,sy+200))?;}wait!();
                if weak.update(cx,|v,_|v.editor.is_dragging()||v.editor.crop!=Some(initial_crop)||v.editor.document.can_undo()).map_err(|e|e.to_string())? {return Err("右键取消当前绘图误提交或重选".into());}
                button!("arrow");
                input.drag((sx+100,sy+100),(sx+400,sy+100));wait!();
                input.drag((sx+250,sy+100),(sx+250,sy+230));wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let a=&v.editor.document.items()[0].annotation;
                    if a.points.len()!=3 || a.arrow_midpoint()!= (crate::annotation::Point{x:(sx+250) as f32,y:(sy+230) as f32}) {return Err("箭头中点未弯曲".into());}
                    let path=a.vector_path();let curve=path.spans[0];let color=a.style.color;let visible=DesktopCapture::capture()?;
                    save_toolbar_preview(v,input.scale,"target/qa/vector/arrow-toolbars.png")?;
                    for t in [0.2,0.3,0.7] {
                        let p=curve.at(t);let pixel=visible.read_region(Rect{x:p.x.round() as u32,y:p.y.round() as u32,width:1,height:1})?;
                        for c in 0..3 { if (pixel.bgra[c] as i32-((color>>(c*8))&255) as i32).abs()>32 {return Err("实际 GPU 曲线箭头与共享路径不符".into());} }
                    }
                    Ok(())
                }).map_err(|e|e.to_string())??;
                input.drag((sx+400,sy+100),(sx+460,sy+100));wait!();
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.arrow_midpoint()).map_err(|e|e.to_string())? != (crate::annotation::Point{x:(sx+280) as f32,y:(sy+256) as f32}) {return Err("拖端点未保留弯曲比例".into());}
                unsafe {PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONDOWN,WPARAM(2),pack(sx+280,sy+256))?;
                    PostMessageW(Some(HWND(input.handle as *mut _)),WM_RBUTTONUP,WPARAM(0),pack(sx+280,sy+256))?;}wait!();
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.points.len()!=2||v.editor.crop!=Some(initial_crop)).map_err(|e|e.to_string())? {return Err("右键中点拉直失败或误重选".into());}
                button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.points.len()).map_err(|e|e.to_string())?!=3 {return Err("拉直无法撤销".into());}
                button!("undo");button!("undo");button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.can_undo()).map_err(|e|e.to_string())? {return Err("曲线编辑并非一次手势一次历史".into());}
                tool!("polyline");
                for pos in [(sx+100,sy+100),(sx+220,sy+240),(sx+360,sy+100)] {input.drag(pos,pos);wait!();}
                input.key(0x0D);wait!();input.drag((sx+220,sy+240),(sx+240,sy+260));wait!();
                input.double_click((sx+170,sy+180))?;wait!();
                let (points,active)=weak.update(cx,|v,_|(v.editor.document.items()[0].annotation.points.clone(),v.editor.active_node())).map_err(|e|e.to_string())?;
                if points.len()!=4 || active!=Some(1) {return Err(format!("双击折线未插入可编辑节点：{points:?}，active={active:?}").into());}
                weak.update(cx,|v,_|save_toolbar_preview(v,input.scale,"target/qa/vector/polyline-toolbars.png")).map_err(|e|e.to_string())??;
                button!("delete-node");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.points.len()).map_err(|e|e.to_string())?!=3 {return Err("属性栏删顶点失败".into());}
                button!("undo");button!("undo");
                input.drag((sx+240,sy+260),(sx+240,sy+260));wait!();
                button!("polyline-smooth");button!("filled");
                weak.update(cx,|v,_|->Result<()>{
                    let a=&v.editor.document.items()[0].annotation;
                    if !a.style.polyline_smooth||!a.style.filled {return Err("闭合平滑属性未生效".into());}
                    if v.editor.document.items().len()!=1||a.tool!=Tool::Polyline||a.points.len()!=3 {return Err("闭合曲线对象或节点数量不符".into());}
                    let expected_path=a.vector_path();let expected_style=a.style;
                    let fill_point=crate::annotation::Point{x:a.points.iter().map(|p|p.x).sum::<f32>()/3.,y:a.points.iter().map(|p|p.y).sum::<f32>()/3.};
                    let scene=v.prepared_scene();
                    if !scene.errors.is_empty()||scene.layers.len()!=1 {return Err(format!("闭合曲线场景错误或层数不符：{:?}",scene.errors).into());}
                    let crate::scene::Layer::Vector{mesh,path,segments,style,arrow}= &scene.layers[0] else {return Err("闭合曲线未保留共享矢量层".into());};
                    if *arrow||*style!=expected_style||**path!=expected_path||**segments!=path.flattened()
                        ||path.spans.len()!=3||path.spans.iter().any(|span|!matches!(span,crate::vector_path::Span::Cubic{..}))
                        ||path.spans.first().map(|span|span.start())!=path.spans.last().map(|span|span.end())
                        ||path.fills.len()!=1||path.fills[0].len()<3||path.fills[0].iter().any(|p|!p.x.is_finite()||!p.y.is_finite())
                    {return Err("闭合曲线的平滑路径、填充轮廓或样式与标注不一致".into());}
                    if mesh.vertices.is_empty()||mesh.indices.is_empty()||!mesh.indices.len().is_multiple_of(3)||mesh.edges.is_empty()
                        ||mesh.normals.len()!=mesh.vertices.len()
                        ||mesh.vertices.iter().chain(mesh.normals.iter()).any(|p|!p.x.is_finite()||!p.y.is_finite())
                        ||mesh.indices.iter().any(|&i|i as usize>=mesh.vertices.len())
                        ||mesh.edges.iter().any(|&(a,b)|a as usize>=mesh.vertices.len()||b as usize>=mesh.vertices.len())
                    {return Err("闭合曲线矢量网格为空或几何索引无效".into());}
                    let crop=v.selected()?;
                    if v.output_rect()?!=crop||v.editor.document.crop_mask.is_some()||v.editor.lasso_draft.is_some() {return Err("闭合曲线验收残留套索或裁剪变化".into());}
                    let probe=Rect{x:fill_point.x.floor() as u32,y:fill_point.y.floor() as u32,width:1,height:1};
                    let center=crate::annotation::Point{x:probe.x as f32+0.5,y:probe.y as f32+0.5};
                    if !fill_point.x.is_finite()||!fill_point.y.is_finite()||!crop.contains((probe.x as i32,probe.y as i32))||!crate::annotation::contains_path(center,segments)
                        ||segments.iter().any(|&(a,b)|crate::annotation::distance_to_segment(center,a,b)<=style.width/2.+1.)
                        ||!style.opacity.is_finite()||style.opacity<=0.||style.opacity>1.
                    {return Err("闭合曲线填充探针不在远离描边的有效内部".into());}
                    let cross=|a:crate::annotation::Point,b:crate::annotation::Point,p:crate::annotation::Point|(b.x-a.x)*(p.y-a.y)-(b.y-a.y)*(p.x-a.x);
                    if !mesh.indices.chunks_exact(3).any(|triangle|{
                        let [a,b,c]=[mesh.vertices[triangle[0] as usize],mesh.vertices[triangle[1] as usize],mesh.vertices[triangle[2] as usize]];
                        let sides=[cross(a,b,center),cross(b,c,center),cross(c,a,center)];
                        cross(a,b,c)!=0.&&(sides.iter().all(|&s|s>=0.)||sides.iter().all(|&s|s<=0.))
                    }) {return Err("闭合曲线 GPU 矢量网格缺少内部填充几何".into());}
                    // 透明的合成探针保证填充确实产生颜色和 alpha，不能让相同底图掩盖空填充。
                    let mut fill=crate::capture::Image{width:1,height:1,bgra:vec![0;4],readback_ms:0.};
                    scene.try_apply(&mut fill,probe)?;
                    let color=[style.color as u8,(style.color>>8) as u8,(style.color>>16) as u8];
                    if fill.bgra[..3]!=color||fill.bgra[3]==0||fill.bgra[3]!=(style.opacity*255.).round() as u8 {return Err("闭合曲线内部的共享填充颜色或透明度缺失".into());}
                    // 使用比选区外扩一像素的 CPU 预览，再逐行比对真实内容导出的裁剪，检查全屏坐标与边缘。
                    let (left,top)=(crop.x.saturating_sub(1),crop.y.saturating_sub(1));
                    let right=(crop.x+crop.width).saturating_add(1).min(v.source.bounds.width);
                    let bottom=(crop.y+crop.height).saturating_add(1).min(v.source.bounds.height);
                    let preview_rect=Rect{x:left,y:top,width:right-left,height:bottom-top};
                    let mut preview=v.source_pixels().read(preview_rect);scene.try_apply(&mut preview,preview_rect)?;
                    let output=v.selected_content_image()?;
                    if output.width!=crop.width||output.height!=crop.height||output.bgra.len()!=crop.width as usize*crop.height as usize*4 {return Err("闭合曲线内容导出的尺寸或像素长度不符".into());}
                    let row_bytes=crop.width as usize*4;
                    for y in 0..crop.height {
                        let src=(((crop.y-preview_rect.y+y) as usize*preview.width as usize)+(crop.x-preview_rect.x) as usize)*4;
                        let dst=y as usize*row_bytes;
                        if preview.bgra[src..src+row_bytes]!=output.bgra[dst..dst+row_bytes] {return Err(format!("闭合曲线共享 CPU 预览与导出裁剪不一致：row={y}").into());}
                    }
                    Ok(())
                }).map_err(|e|e.to_string())??;
                let wheel=((sx+170) as f32/input.scale,(sy+180) as f32/input.scale);
                input.wheel(wheel,120)?;wait!();button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.width!=4.||!v.editor.document.items()[0].annotation.style.filled).map_err(|e|e.to_string())? {return Err("节点编辑后的滚轮撤销越过填充事务".into());}
                button!("undo");button!("undo");button!("undo");button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.can_undo()).map_err(|e|e.to_string())? {return Err("节点操作留下多余历史".into());}
                for pos in [(sx+100,sy+100),(sx+220,sy+240),(sx+360,sy+100)] {input.drag(pos,pos);wait!();}
                input.key(0x08);wait!();input.double_click((sx+300,sy+260))?;wait!();
                if weak.update(cx,|v,_|v.editor.has_pending_drawing()||v.editor.document.items()[0].annotation.points.len()!=3).map_err(|e|e.to_string())? {return Err("折线 Backspace 或双击完成失败".into());}
                button!("undo");
                println!("{{\"test\":\"native_curve_arrow_nodes_backspace_fill_smooth_wheel_undo\",\"ok\":true}}");
                // 实际旋转手柄、第二栏按钮、原生滚轮与组合事务。
                std::fs::create_dir_all("target/qa/transform")?;
                macro_rules! rotate_drag {($degrees:expr)=>{{
                    let (knob,pivot)=weak.update(cx,|v,_|->Result<_>{
                        let &(x,y)=v.ui_positions.borrow().get("rotation-handle").ok_or("找不到实际旋转手柄")?;
                        Ok((crate::annotation::Point{x:x*input.scale,y:y*input.scale},crate::transform::center(v.editor.selection_bounds().ok_or("没有旋转选区")?)))
                    }).map_err(|e|e.to_string())??;
                    let end=crate::transform::Rotation::new(pivot,$degrees).map(knob);
                    input.drag((knob.x.round() as u32,knob.y.round() as u32),(end.x.round() as u32,end.y.round() as u32));wait!();
                }}}
                button!("rectangle");
                let original_rect_style=weak.update(cx,|v,cx|{let old=v.editor.style;v.editor.set_style(crate::annotation::Style::default());cx.notify();old}).map_err(|e|e.to_string())?;
                input.drag((sx+120,sy+180),(sx+330,sy+250));wait!();
                let unrotated=weak.update(cx,|v,_|v.editor.document.items()[0].annotation.clone()).map_err(|e|e.to_string())?;
                rotate_drag!(90.);
                let rotation_probes=weak.update(cx,|v,_|->Result<_>{
                    let a=&v.editor.document.items()[0].annotation;
                    if (a.rotation-90.).abs()>1.||a.points!=unrotated.points{return Err("原生旋转拖动改写了源点或角度错误".into());}
                    let b=v.toolbar_layout(v.selected()?,input.scale).properties;
                    for id in ["angle-menu","arrange-menu"] {
                        let &(x,y)=v.ui_positions.borrow().get(id).ok_or("变换按钮缺失")?;
                        if x<b.x||x>b.right()||y<b.y||y>b.bottom(){return Err(format!("变换按钮 {id} 超出属性栏").into());}
                    }
                    let probes=a.segments().iter().take(2).map(|(a,b)|crate::annotation::Point{x:a.x+(b.x-a.x)*0.32,y:a.y+(b.y-a.y)*0.32}).collect::<Vec<_>>();
                    save_toolbar_preview(v,input.scale,"target/qa/transform/rotation-toolbars.png")?;
                    Ok(probes)
                }).map_err(|e|e.to_string())??;
                button!("edit");
                weak.update(cx,|v,_|verify_scene_pixels(v,&rotation_probes)).map_err(|e|e.to_string())??;
                input.drag((sx+225,sy+215),(sx+225,sy+215));wait!();
                input.wheel(((sx+225) as f32/input.scale,(sy+215) as f32/input.scale),120)?;wait!();
                if weak.update(cx,|v,_|(v.editor.document.items()[0].annotation.rotation-90.).abs()>1.||v.editor.document.items()[0].annotation.style.width!=5.).map_err(|e|e.to_string())?{return Err("旋转后滚轮调参丢失角度".into());}
                button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.width!=4.||(v.editor.document.items()[0].annotation.rotation-90.).abs()>1.).map_err(|e|e.to_string())?{return Err("旋转后滚轮撤销越过旋转事务".into());}
                button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation!=unrotated).map_err(|e|e.to_string())?{return Err("旋转手势无法一次撤销".into());}
                button!("undo");
                button!("text");input.drag((sx+200,sy+260),(sx+200,sy+260));wait!();input.text("旋转文字 Rust");wait!();button!("edit");
                input.drag((sx+215,sy+273),(sx+215,sy+273));wait!();button!("angle-menu");button!("rotate-right");button!("rotate-right");button!("angle-menu");
                let old_text=weak.update(cx,|v,_|v.editor.document.items()[0].annotation.clone()).map_err(|e|e.to_string())?;
                input.key(0x71);wait!();input.text("修改中文🙂");wait!();
                weak.update(cx,|v,_|->Result<()>{
                    let a=&v.editor.text_draft.as_ref().ok_or("旋转文字没有进入编辑")?.annotation;
                    if a.text!="修改中文🙂"||a.rotation!=30.||a.to_world(a.points[0]).distance(old_text.to_world(old_text.points[0]))>0.01{return Err("旋转文字编辑丢失角度或光标锚点".into());}
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("rectangle");button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation!=old_text).map_err(|e|e.to_string())?{return Err("旋转文字编辑撤销失败".into());}
                button!("undo");button!("undo");button!("undo");
                weak.update(cx,|v,cx|{v.editor.set_width(40.);cx.notify();}).map_err(|e|e.to_string())?;wait!();
                input.drag((sx+150,sy+210),(sx+230,sy+270));wait!();
                input.drag((sx+350,sy+220),(sx+440,sy+280));wait!();
                weak.update(cx,|v,cx|{v.editor.select_all_objects();cx.notify();}).map_err(|e|e.to_string())?;wait!();button!("arrange-menu");button!("group-selection");
                weak.update(cx,|v,_|save_toolbar_preview(v,input.scale,"target/qa/transform/group-toolbars.png")).map_err(|e|e.to_string())??;
                let grouped=weak.update(cx,|v,_|v.editor.document.items().to_vec()).map_err(|e|e.to_string())?;
                button!("edit");input.drag((sx+170,sy+235),(sx+190,sy+265));wait!();
                weak.update(cx,|v,_|->Result<()>{
                    if v.editor.selected_ids().len()!=2{return Err("点击组合成员没有选中完整组合".into());}
                    for (a,b) in v.editor.document.items().iter().zip(&grouped){if (a.annotation.bounds().x-b.annotation.bounds().x-20.).abs()>0.01||(a.annotation.bounds().y-b.annotation.bounds().y-30.).abs()>0.01{return Err("组合移动没有保留成员间距".into());}}
                    Ok(())
                }).map_err(|e|e.to_string())??;button!("undo");
                input.drag((sx+170,sy+235),(sx+170,sy+235));wait!();
                input.drag((sx+440,sy+280),(sx+585,sy+315));wait!();
                if weak.update(cx,|v,_|(v.editor.document.items()[0].annotation.bounds().width-120.).abs()>0.1).map_err(|e|e.to_string())?{return Err("组合角点比例缩放失败".into());}
                input.wheel(((sx+210) as f32/input.scale,(sy+245) as f32/input.scale),120)?;wait!();
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.width!=60.).map_err(|e|e.to_string())?{return Err("组合放大后滚轮向上反向跳变".into());}
                input.wheel(((sx+210) as f32/input.scale,(sy+245) as f32/input.scale),-120)?;wait!();
                if weak.update(cx,|v,_|v.editor.document.items()[0].annotation.style.width!=59.).map_err(|e|e.to_string())?{return Err("组合放大后滚轮向下未逐步调整".into());}
                button!("undo");button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()!=grouped).map_err(|e|e.to_string())?{return Err("组合缩放并非一次撤销".into());}
                input.drag((sx+170,sy+235),(sx+170,sy+235));wait!();rotate_drag!(45.);
                if weak.update(cx,|v,_|v.editor.document.items().iter().any(|o|(o.annotation.rotation-45.).abs()>1.)).map_err(|e|e.to_string())?{return Err("组合旋转没有同步成员角度".into());}
                button!("undo");input.drag((sx+170,sy+235),(sx+170,sy+235));wait!();action!("duplicate");
                weak.update(cx,|v,_|->Result<()>{let items=v.editor.document.items();
                    if items.len()!=4||v.editor.selected_ids().len()!=2||items[0].group==items[2].group||items[2].group!=items[3].group{return Err("复制组合混入了原组合".into());}Ok(())
                }).map_err(|e|e.to_string())??;
                button!("arrange-menu");button!("ungroup-selection");
                if weak.update(cx,|v,_|v.editor.document.items()[2..].iter().any(|o|o.group.is_some())).map_err(|e|e.to_string())?{return Err("属性栏拆分没有解除组合".into());}
                button!("undo");button!("undo");button!("undo");button!("undo");button!("undo");
                if weak.update(cx,|v,_|!v.editor.document.items().is_empty()||v.editor.document.can_undo()).map_err(|e|e.to_string())?{return Err("组合编辑留下额外撤销记录".into());}
                for tool in [Tool::Mosaic,Tool::Blur,Tool::Spotlight] {
                    weak.update(cx,|v,cx|{v.editor.select_tool(tool);cx.notify();}).map_err(|e|e.to_string())?;wait!();
                    input.drag((sx+540,sy+300),(sx+740,sy+390));wait!();button!("angle-menu");button!("rotate-right");button!("rotate-right");button!("angle-menu");
                    weak.update(cx,|v,_|->Result<()>{
                        let a=&v.editor.document.items()[0].annotation;let b=a.bounds();
                        let probes=[crate::transform::center(a.local_bounds()),crate::annotation::Point{x:b.x+3.,y:b.y+3.}];
                        verify_scene_pixels(v,&probes)?;
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    button!("undo");button!("undo");button!("undo");
                }
                weak.update(cx,|v,cx|{v.editor.select_tool(Tool::Rectangle);v.editor.set_style(original_rect_style);cx.notify();}).map_err(|e|e.to_string())?;wait!();
                println!("{{\"test\":\"native_rotation_group_scale_text_wheel_masks_undo\",\"ok\":true}}");
                for tool in [Tool::Rectangle,Tool::Ellipse,Tool::Arrow,Tool::Pen,Tool::Text,Tool::Watermark,
                    Tool::Serial,Tool::Highlighter,Tool::HighlighterRect,Tool::Polyline,Tool::Mosaic,Tool::Blur,
                    Tool::MosaicBrush,Tool::BlurBrush,Tool::Spotlight] {
                    let original=weak.update(cx,|v,cx|{
                        v.editor.select_tool(tool);
                        v.toolbar_panel=crate::toolbar_layout::ToolbarPanel::None;
                        cx.notify();
                        v.editor.style
                    }).map_err(|e|e.to_string())?;
                    weak.update(cx,|v,cx|{let mut s=v.editor.style;s.width=48.;s.radius=20.;s.font_size=56.;s.bold=true;s.opacity=1.;s.block_size=32;v.editor.set_style(s);cx.notify();}).map_err(|e|e.to_string())?;
                    wait!();
                    weak.update(cx,|v,_|->Result<()>{
                        let bounds=v.toolbar_layout(v.selected()?,input.scale).properties;
                        let positions=v.ui_positions.borrow();
                        for id in ["custom-color","width-menu","font-menu","font-family-menu","text-format-menu","bold","filled","radius-menu","opacity-menu","arrow-menu","dashed","block-menu","edit-text","pencil-smooth","polyline-smooth","delete-node","straighten-arrow"] {
                            if let Some(&(x,y))=positions.get(id) {
                                if x < bounds.x || x > bounds.right() || y < bounds.y || y > bounds.bottom() {
                                    return Err(format!("{} 属性 {} 被挤出工具栏：({x}, {y})，{bounds:?}",tool.label(),id).into());
                                }
                            }
                        }
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    weak.update(cx,|v,cx|{v.editor.set_style(original);cx.notify();}).map_err(|e|e.to_string())?;
                }
                button!("rectangle");
                let primary = weak.update(cx,|v,_|v.ui_positions.borrow().get("primary-toolbar").copied()).map_err(|e|e.to_string())?.ok_or("找不到主栏")?;
                let properties = weak.update(cx,|v,_|v.ui_positions.borrow().get("property-toolbar").copied()).map_err(|e|e.to_string())?.ok_or("找不到属性栏")?;
                button!("more");
                weak.update(cx,|v,_|->Result<()>{
                    if v.ui_positions.borrow().get("primary-toolbar") != Some(&primary) || v.ui_positions.borrow().get("property-toolbar") != Some(&properties) { return Err("展开更多移动了两个工具栏".into()); }
                    if !v.ui_positions.borrow().contains_key("toolbar-popup") { return Err("更多面板没有独立绘制".into()); }
                    Ok(())
                }).map_err(|e|e.to_string())??;
                input.key(0x1B);wait!();
                if weak.update(cx,|v,_|v.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None).map_err(|e|e.to_string())? { return Err("Esc 未收起更多面板".into()); }
                button!("width-menu"); input.drag((sx+850,sy+550),(sx+850,sy+550)); wait!();
                weak.update(cx,|v,_|->Result<()>{
                    if v.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None || !v.editor.document.items().is_empty() { return Err("点击外部收起面板误创建了对象".into()); }
                    Ok(())
                }).map_err(|e|e.to_string())??;
                input.drag((sx-50,sy-50),(sx+250,sy+200));wait!();
                button!("arrow");input.drag((sx+400,sy+100),(sx+650,sy+300));wait!();
                input.key(0x31);input.drag((sx+450,sy+450),(sx+470,sy+460));wait!();
                weak.update(cx,|v,_|->Result<()>{
                    if v.editor.crop!=Some(Rect{x:sx+20,y:sy+10,width:900,height:580}){return Err("截图范围移动失败".into());}
                    let a=&v.editor.document.items()[0].annotation;
                    if a.bounds().x!=(sx-50) as f32||a.bounds().y!=(sy-50) as f32{return Err("全屏标注随选区移动".into());}
                    Ok(())
                }).map_err(|e|e.to_string())??;
                button!("mosaic");input.drag((sx+180,sy+70),(sx+320,sy+230));wait!();
                button!("edit");input.drag((sx+230,sy+140),(sx+265,sy+160));wait!();
                input.drag((sx+355,sy+250),(sx+395,sy+290));wait!();button!("block-menu");button!("block8");
                weak.update(cx,|v,_|->Result<()>{let a=&v.editor.document.items()[2].annotation;
                    if a.tool!=Tool::Mosaic||a.bounds().width!=180.||a.style.block_size!=8{return Err(format!("马赛克编辑失败：{a:?}").into());}Ok(())
                }).map_err(|e|e.to_string())??;
                button!("undo");if weak.update(cx,|v,_|v.editor.document.items()[2].annotation.style.block_size).map_err(|e|e.to_string())?!=16{return Err("撤销属性失败".into());}
                button!("redo");tool!("highlighter");
                input.drag((sx+500,sy+350),(sx+720,sy+350));wait!();
                tool!("blur");input.drag((sx+470,sy+80),(sx+670,sy+220));wait!();
                tool!("serial");input.drag((sx+780,sy+280),(sx+780,sy+280));wait!();
                button!("text");input.drag((sx+60,sy+390),(sx+60,sy+390));wait!();input.text("RustShot 中文🙂");wait!();
                let text=weak.update(cx,|v,_|v.editor.text_draft.as_ref().map(|d|d.annotation.text.clone())).map_err(|e|e.to_string())?;
                if text.as_deref()!=Some("RustShot 中文🙂"){return Err(format!("原生中文输入失败：{text:?}").into());}
                input.drag((sx+80,sy+405),(sx+145,sy+405));wait!();
                if weak.update(cx,|v,_|v.input.selection.is_empty()).map_err(|e|e.to_string())? { return Err("文字鼠标扩选失败".into()); }
                input.drag((sx+130,sy+405),(sx+130,sy+405));wait!();
                if !weak.update(cx,|v,_|v.input.selection.is_empty()).map_err(|e|e.to_string())? { return Err("文字鼠标定位失败".into()); }
                let text_style=weak.update(cx,|v,_|v.editor.style).map_err(|e|e.to_string())?;
                button!("font-family-menu");button!("font-consolas");button!("italic");
                // 底色、描边的粗细与不透明度在拆分开关右半 ▾ 的细项里；最后停在描边细项供滚轮。
                button!("text-background");button!("text-background-menu");button!("text-background-alpha");button!("background-alpha35");
                button!("text-outline");button!("text-outline-menu");button!("text-outline-width");button!("outline-6");
                let outline=weak.update(cx,|v,_|v.ui_positions.borrow().get("text-outline-width").copied()).map_err(|e|e.to_string())?.ok_or("缺少文字描边滚轮入口")?;
                input.wheel(outline,120)?;wait!();
                weak.update(cx,|v,cx|->Result<()>{
                    let t=v.editor.style.text;
                    if t.font.name()!="Consolas" || !t.italic || !t.outline || !t.background || t.outline_width<=6. || (t.background_opacity-0.35).abs()>0.001 {
                        return Err(format!("文字样式面板与滚轮未进入当前草稿：font={}, style={t:?}, panel={:?}, draft={}",t.font.name(),v.toolbar_panel,v.editor.text_draft.is_some()).into());
                    }
                    v.editor.set_style(text_style);v.toolbar_panel=crate::toolbar_layout::ToolbarPanel::None;cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;wait!();
                println!("{{\"test\":\"native_text_font_outline_background_popup_wheel\",\"ok\":true}}");
                button!("rectangle"); // 换工具提交一次文字事务。
                input.drag((sx+690,sy+400),(sx+830,sy+500));wait!();button!("filled");button!("radius-menu");button!("corner20");
                button!("edit");input.drag((sx+100,sy+403),(sx+100,sy+403));wait!();
                input.key(0x71);wait!();input.text("RustShot 编辑🙂");wait!();
                if weak.update(cx,|v,_|v.editor.text_draft.as_ref().map(|d|d.annotation.text.clone())).map_err(|e|e.to_string())?.as_deref()!=Some("RustShot 编辑🙂") { return Err("F2 二次文字编辑失败".into()); }
                button!("rectangle");button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items()[6].annotation.text.clone()).map_err(|e|e.to_string())? != "RustShot 中文🙂" { return Err("文字二次编辑撤销失败".into()); }
                weak.update(cx,|v,_|->Result<()>{
                    if v.editor.document.items().len()!=8{return Err(format!("对象数量错误：{}",v.editor.document.items().len()).into());}
                    std::fs::create_dir_all("target/qa")?;export::save_png(v.selected_image()?,&PathBuf::from("target/qa/v04-annotated.png"))?;
                    let scene=v.prepared_scene();if !scene.errors.is_empty(){return Err(scene.errors.join(";").into());}
                    let(w,p)=memory_mib();println!("{{\"phase\":\"gpui_v04\",\"screens\":{},\"objects\":8,\"working_set_mib\":{w:.2},\"private_mib\":{p:.2},\"capture_ms\":{:.3},\"preview_ms\":{preview_ms:.3},\"activation_ms\":{activation_ms:.3},\"ok\":true}}",v.source.screen_count(),v.source.capture_ms);
                    Ok(())
                }).map_err(|e|e.to_string())??;
                weak.update(cx,|v,_|save_toolbar_preview(v, input.scale, "target/qa/two-bars/toolbar-before-theme.png")).map_err(|e|e.to_string())??;
                action!("theme");wait!();
                weak.update(cx,|v,_|save_toolbar_preview(v, input.scale, "target/qa/two-bars/toolbar-after-theme.png")).map_err(|e|e.to_string())??;
                if let Ok(visible)=DesktopCapture::capture(){if let Ok(image)=visible.read_region(visible.full()){export::save_png(image,&PathBuf::from("target/qa/v04-overlay.png"))?;}}
                action!("theme"); // 还原测试前的主题偏好。
                action!("settings");
                let settings = owned_smoke_window(unsafe { FindWindowW(None, &windows::core::HSTRING::from(crate::brand_name!())) }?)?;
                if unsafe { GetWindowLongPtrW(settings, GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0 { return Err("设置窗口不应默认置顶".into()); }
                unsafe { PostMessageW(Some(settings), WM_CLOSE, WPARAM(0), LPARAM(0))?; }
                wait!();
                action!("history");
                // 历史页在独立的主界面进程里打开，不占用截图会话。
                let history = (0..50).find_map(|_| {
                    let found = unsafe { FindWindowW(None, &windows::core::HSTRING::from(crate::brand_name!())) }.ok().filter(|h| !h.0.is_null());
                    if found.is_none() { std::thread::sleep(std::time::Duration::from_millis(100)); }
                    found
                }).ok_or("主界面历史页未打开")?;
                unsafe { PostMessageW(Some(history), WM_CLOSE, WPARAM(0), LPARAM(0))?; }
                wait!();
                action!("cursor");
                if !weak.update(cx,|v,_|v.include_cursor).map_err(|e|e.to_string())? { return Err("鼠标指针开关失效".into()); }
                action!("cursor");
                action!("delete");button!("undo");action!("clear");button!("undo");
                if weak.update(cx,|v,_|v.editor.document.items().len()).map_err(|e|e.to_string())?!=8{return Err("删除/清空不可撤销".into());}
                action!("clear");button!("crop");
                let monitors=weak.update(cx,|v,_|(0..v.source.screen_count()).map(|i|v.source.screen_rect(i)).collect::<Vec<_>>()).map_err(|e|e.to_string())?;
                if monitors.len()>1{let(a,b)=(monitors[0],monitors[1]);input.drag((a.x+a.width/2,a.y+a.height/2),(b.x+b.width/2,b.y+b.height/2));wait!();
                    weak.update(cx,|v,_|->Result<()>{let crop=v.selected()?;let image=v.selected_image()?;let mut covered=0;
                        for i in 0..v.source.screen_count(){let area=v.source.screen_rect(i);if let Some(part)=crop.intersection(area){covered+=1;let(x,y)=(part.x+part.width/2,part.y+part.height/2);let dst=(((y-crop.y)*crop.width+x-crop.x) as usize)*4;let src=(((y-area.y)*area.width+x-area.x) as usize)*4;if image.bgra[dst..dst+4]!=v.previews[i].as_bytes(0).unwrap()[src..src+4]{return Err("跨屏像素不一致".into());}}}
                        if covered<2{return Err("没有跨屏框选".into());}export::save_png(image,&PathBuf::from("target/qa/v04-crossscreen.png"))?;Ok(())
                    }).map_err(|e|e.to_string())??;
                }
                action!("all");if weak.update(cx,|v,_|v.editor.crop!=Some(v.source.full())).map_err(|e|e.to_string())?{return Err("全部屏按钮失败".into());}
                action!("monitor");let expected=weak.update(cx,|v,_|v.selected()).map_err(|e|e.to_string())??;
                button!("pin");wait!();
                let pin=owned_smoke_window(unsafe{FindWindowW(None,&windows::core::HSTRING::from(concat!(crate::brand_name!(), " · 贴图")))}?)?;
                let mut bounds=RECT::default();unsafe{GetClientRect(pin,&mut bounds)?;}
                if bounds.right as u32!=expected.width||bounds.bottom as u32!=expected.height{return Err("贴图尺寸错误".into());}
                if weak.update(cx,|_,_|()).is_ok(){return Err("贴图后截图资源未释放".into());}
                input_pin_zoom(pin)?;
                wait!();
                unsafe { GetClientRect(pin, &mut bounds)?; }
                if bounds.right as u32 <= expected.width { return Err("贴图滚轮缩放失败".into()); }
                let pin_input = Input { handle: pin.0 as usize, scale: input.scale };
                pin_input.key(0x30); wait!();
                unsafe { GetClientRect(pin, &mut bounds)?; }
                if bounds.right as u32 != expected.width { return Err("贴图恢复原始大小失败".into()); }
                pin_input.key(0x54); wait!();
                if unsafe { GetWindowLongPtrW(pin, GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0 { return Err("贴图置顶切换失败".into()); }
                unsafe{PostMessageW(Some(pin),WM_CLOSE,WPARAM(0),LPARAM(0))?;}
                println!("{{\"test\":\"two_toolbars_popups_dismiss_text_effects_history_crossscreen_pin\",\"ok\":true}}");
                Ok(())
            }.await;
            if let Err(e)=result{*errors.borrow_mut()=Some(e.to_string());let _=cx.update(|cx|cx.quit());}
        }).detach();
    });
}

// 探针避开编辑控制点，实际屏幕与共享导出允许抗锯齿的像素差。
fn verify_scene_pixels(view: &mut CaptureView, points: &[crate::annotation::Point]) -> Result<()> {
    let crop = view.selected()?;
    let expected = view.selected_image()?;
    let desktop = DesktopCapture::capture()?;
    for p in points {
        let (x, y) = (p.x.round() as u32, p.y.round() as u32);
        if x < crop.x || y < crop.y || x >= crop.x + crop.width || y >= crop.y + crop.height {
            return Err("渲染探针超出截图范围".into());
        }
        let actual = desktop.read_region(Rect {
            x,
            y,
            width: 1,
            height: 1,
        })?;
        let offset = (((y - crop.y) * crop.width + x - crop.x) * 4) as usize;
        for c in 0..3 {
            if (actual.bgra[c] as i32 - expected.bgra[offset + c] as i32).abs() > 32 {
                return Err(format!(
                    "旋转预览与导出像素不一致 ({x}, {y})：{:?} / {:?}",
                    actual.bgra,
                    &expected.bgra[offset..offset + 4]
                )
                .into());
            }
        }
    }
    Ok(())
}

fn input_pin_zoom(pin: HWND) -> Result<()> {
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(pin, &mut r)?;
        let x = r.left + 30;
        let y = r.top + 60;
        PostMessageW(
            Some(pin),
            WM_MOUSEWHEEL,
            WPARAM(120 << 16),
            LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
        )?;
    }
    Ok(())
}

// 从实际 GPUI 界面裁出两个栏，并铺到纯色预览板；不展示用户桌面。
fn save_toolbar_preview(view: &CaptureView, scale: f32, path: &str) -> Result<()> {
    let layout = view.toolbar_layout(view.selected()?, scale);
    let visible = DesktopCapture::capture()?;
    let primary = layout.primary;
    let property = layout.properties;
    let width = (primary.width.max(property.width) * scale).ceil() as u32;
    let gap = (8. * scale).ceil() as u32;
    let height =
        (primary.height * scale).ceil() as u32 + (property.height * scale).ceil() as u32 + gap;
    let p = crate::capture_toolbar::Palette::new(view.light_theme);
    let color = [
        (p.background & 255) as u8,
        ((p.background >> 8) & 255) as u8,
        ((p.background >> 16) & 255) as u8,
        255,
    ];
    let mut output = crate::capture::Image {
        width,
        height,
        bgra: color.repeat((width * height) as usize),
        readback_ms: 0.,
    };
    for (bounds, y) in [
        (primary, 0),
        (property, (primary.height * scale).ceil() as u32 + gap),
    ] {
        let rect = Rect {
            x: (bounds.x * scale).round() as u32,
            y: (bounds.y * scale).round() as u32,
            width: (bounds.width * scale).ceil() as u32,
            height: (bounds.height * scale).ceil() as u32,
        };
        let image = visible.read_region(rect)?;
        let x = width - image.width;
        let radius = 8. * scale;
        for row in 0..image.height {
            for col in 0..image.width {
                let rx = (col as f32 + 0.5).min(image.width as f32 - col as f32 - 0.5);
                let ry = (row as f32 + 0.5).min(image.height as f32 - row as f32 - 0.5);
                if rx < radius && ry < radius && (radius - rx).hypot(radius - ry) > radius {
                    continue;
                }
                let src = ((row * image.width + col) * 4) as usize;
                let dst = (((row + y) * output.width + col + x) * 4) as usize;
                output.bgra[dst..dst + 4].copy_from_slice(&image.bgra[src..src + 4]);
            }
        }
    }
    export::save_png(output, &PathBuf::from(path))
}

// 真实控件系统光标专项；启动前输入基线由独立 QA 父进程固定传入。
mod cursor_controls {
    use super::*;
    use gpui::{AsyncApp, WeakEntity};
    use serde_json::{json, Value};
    use std::mem::size_of;
    use windows::Win32::{
        Graphics::Gdi::{ClientToScreen, MonitorFromPoint, MONITOR_DEFAULTTONULL},
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{
                GetCurrentProcessId, GetCurrentThreadId, GetProcessId, GetProcessTimes,
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        },
        UI::Input::KeyboardAndMouse::{
            GetActiveWindow, GetCapture, GetLastInputInfo, LASTINPUTINFO,
        },
    };

    const TOKEN: windows::core::PCWSTR = w!("RustShot.QaCursorControlsToken");
    fn need(ok: bool, message: &str) -> Result<()> {
        if ok {
            Ok(())
        } else {
            Err(message.to_owned().into())
        }
    }
    fn qpc() -> Result<i64> {
        let mut value = 0;
        unsafe {
            QueryPerformanceCounter(&mut value)?;
        }
        Ok(value)
    }
    fn input_tick() -> Result<u32> {
        let mut value = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        need(
            unsafe { GetLastInputInfo(&mut value) }.as_bool(),
            "LASTINPUTINFO 查询失败",
        )?;
        Ok(value.dwTime)
    }
    fn startup_input_tick() -> Result<u32> {
        let value = std::env::var("RUSTSHOT_CURSOR_QA_INPUT_TICK")
            .map_err(|_| "缺失或无效的父进程启动前 LASTINPUT 基线；保留现场".to_owned())?;
        need(
            !value.is_empty()
                && value.len() <= 10
                && value.bytes().all(|byte| byte.is_ascii_digit()),
            "父进程启动前 LASTINPUT 基线必须为 DWORD 十进制整数；保留现场",
        )?;
        value.parse::<u32>().map_err(|_| {
            "父进程启动前 LASTINPUT 基线超出 DWORD 范围；保留现场"
                .to_owned()
                .into()
        })
    }
    fn creation(handle: HANDLE) -> Result<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe {
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)?;
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    fn class_name(handle: HWND) -> Result<String> {
        let mut name = [0u16; 256];
        let length = unsafe { GetClassNameW(handle, &mut name) };
        need(
            length > 0 && length < name.len() as i32,
            "窗口 class 无法完整读取",
        )?;
        Ok(String::from_utf16(&name[..length as usize])?)
    }
    fn serial(handle: HWND) -> usize {
        unsafe { GetPropW(handle, w!("RustShot.GpuiPresentSerial")) }.0 as usize
    }
    fn point_equal(a: POINT, b: POINT) -> bool {
        a.x == b.x && a.y == b.y
    }
    struct ProcessHandle(HANDLE);
    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    struct Guard {
        hwnd: HWND,
        process: ProcessHandle,
        pid: u32,
        tid: u32,
        creation: u64,
        class: String,
        token: usize,
        bounds: RECT,
        input_tick: u32,
        original: POINT,
        expected: POINT,
        present_serial: usize,
        present_size: usize,
        frequency: i64,
        hand: HCURSOR,
        ibeam: HCURSOR,
        wait: HCURSOR,
        appstarting: HCURSOR,
    }
    impl Guard {
        fn bind(
            hwnd: HWND,
            bounds: crate::geometry::ScreenRect,
            baseline: u32,
            original: POINT,
        ) -> Result<Self> {
            // 所有 Win32 调用均在异步任务内，不持有 App、Window 或 Entity 借用。
            need(
                std::env::var_os("RUSTSHOT_DATA_DIR").is_some(),
                "调用方须设置独立 QA 数据目录",
            )?;
            let mut pid = 0;
            let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            need(
                pid == unsafe { GetCurrentProcessId() } && tid == unsafe { GetCurrentThreadId() },
                "仅允许本原进程和当前 GPUI 线程窗口",
            )?;
            need(
                bounds.width < 65536 && bounds.height < 65536,
                "PresentSize 超出编码范围",
            )?;
            let process = ProcessHandle(unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    pid,
                )?
            });
            {
                let token = (qpc()? as usize ^ pid as usize).max(1);
                need(
                    unsafe { GetPropW(hwnd, TOKEN) }.0.is_null(),
                    "不覆盖既有 QA token",
                )?;
                let mut frequency = 0;
                unsafe {
                    QueryPerformanceFrequency(&mut frequency)?;
                }
                need(frequency > 0, "QPC frequency 无效")?;
                let mut guard = Self {
                    hwnd,
                    pid,
                    tid,
                    creation: creation(process.0)?,
                    class: class_name(hwnd)?,
                    token,
                    bounds: RECT {
                        left: bounds.left,
                        top: bounds.top,
                        right: bounds.left + bounds.width as i32,
                        bottom: bounds.top + bounds.height as i32,
                    },
                    input_tick: baseline,
                    original,
                    expected: original,
                    present_serial: serial(hwnd),
                    present_size: ((bounds.width << 16) | bounds.height) as usize,
                    frequency,
                    hand: unsafe { LoadCursorW(None, IDC_HAND)? },
                    ibeam: unsafe { LoadCursorW(None, IDC_IBEAM)? },
                    wait: unsafe { LoadCursorW(None, IDC_WAIT)? },
                    appstarting: unsafe { LoadCursorW(None, IDC_APPSTARTING)? },
                    process,
                };
                unsafe {
                    SetPropW(hwnd, TOKEN, Some(HANDLE(token as *mut _)))?;
                }
                guard.check()?;
                Ok(guard)
            }
        }
        fn check(&mut self) -> Result<()> {
            unsafe {
                need(
                    input_tick()? == self.input_tick,
                    "真实输入变化；保留现场，不恢复或关闭",
                )?;
                let mut pid = 0;
                need(
                    IsWindow(Some(self.hwnd)).as_bool()
                        && GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) == self.tid
                        && pid == self.pid,
                    "原 HWND/PID/TID 变化",
                )?;
                need(
                    GetCurrentThreadId() == self.tid
                        && GetProcessId(self.process.0) == self.pid
                        && creation(self.process.0)? == self.creation,
                    "原进程 HANDLE/creation 或操作线程变化",
                )?;
                need(
                    class_name(self.hwnd)? == self.class
                        && GetPropW(self.hwnd, TOKEN).0 as usize == self.token,
                    "原 class/token 变化",
                )?;
                need(
                    IsWindowVisible(self.hwnd).as_bool()
                        && GetForegroundWindow() == self.hwnd
                        && GetActiveWindow() == self.hwnd,
                    "原窗口不可见或失去前台/active",
                )?;
                let mut gui = GUITHREADINFO {
                    cbSize: size_of::<GUITHREADINFO>() as u32,
                    ..Default::default()
                };
                GetGUIThreadInfo(self.tid, &mut gui)?;
                need(
                    gui.hwndFocus == self.hwnd
                        && gui.hwndCapture.is_invalid()
                        && GetCapture().is_invalid(),
                    "原 focus/capture 门失败",
                )?;
                let mut bounds = RECT::default();
                GetWindowRect(self.hwnd, &mut bounds)?;
                need(bounds == self.bounds, "全虚拟桌面物理窗口 bounds 变化")?;
                let mut client = RECT::default();
                GetClientRect(self.hwnd, &mut client)?;
                need(
                    client.left == 0
                        && client.top == 0
                        && client.right == self.bounds.right - self.bounds.left
                        && client.bottom == self.bounds.bottom - self.bounds.top,
                    "物理 client 不覆盖全虚拟桌面",
                )?;
                let mut pointer = POINT::default();
                GetCursorPos(&mut pointer)?;
                need(
                    point_equal(pointer, self.expected)
                        && WindowFromPoint(pointer) == self.hwnd
                        && !MonitorFromPoint(pointer, MONITOR_DEFAULTTONULL).is_invalid(),
                    "鼠标离开预期自有窗口点；保留现场",
                )?;
                let current = serial(self.hwnd);
                need(
                    current > 0
                        && current >= self.present_serial
                        && GetPropW(self.hwnd, w!("RustShot.GpuiPresentSize")).0 as usize
                            == self.present_size,
                    "成功全窗 Present 缺失或回退",
                )?;
                self.present_serial = current;
            }
            Ok(())
        }
        fn screen_point(&mut self, logical: (f32, f32), scale: f32) -> Result<POINT> {
            self.check()?;
            need(
                logical.0.is_finite() && logical.1.is_finite() && scale.is_finite() && scale > 0.,
                "paint probe/scale 无效",
            )?;
            let mut point = POINT {
                x: (logical.0 * scale).round() as i32,
                y: (logical.1 * scale).round() as i32,
            };
            unsafe {
                ClientToScreen(self.hwnd, &mut point).ok()?;
            }
            need(
                point.x >= self.bounds.left
                    && point.x < self.bounds.right
                    && point.y >= self.bounds.top
                    && point.y < self.bounds.bottom
                    && unsafe { WindowFromPoint(point) } == self.hwnd
                    && !unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) }.is_invalid(),
                "实际 probe 中心不在原自有 fullscreen 窗口",
            )?;
            Ok(point)
        }
        fn move_to(&mut self, point: POINT) -> Result<()> {
            self.check()?;
            need(
                unsafe { WindowFromPoint(point) } == self.hwnd
                    && !unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) }.is_invalid(),
                "拒绝移动到非原窗口或物理显示器空隙",
            )?;
            unsafe {
                SetCursorPos(point.x, point.y)?;
            }
            self.expected = point;
            self.check()
        }
        fn cursor(&mut self) -> Result<(CURSORINFO, &'static str)> {
            self.check()?;
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                GetCursorInfo(&mut info)?;
            }
            need(
                point_equal(info.ptScreenPos, self.expected),
                "GetCursorInfo 与预期点不符",
            )?;
            let showing = info.flags == CURSOR_SHOWING;
            need(
                !(showing && (info.hCursor == self.wait || info.hCursor == self.appstarting)),
                "真实 cursor 为 WAIT/APPSTARTING，保留现场",
            )?;
            let kind = if !showing {
                "NONE"
            } else if info.hCursor == self.hand {
                "HAND"
            } else if info.hCursor == self.ibeam {
                "IBEAM"
            } else {
                "OTHER"
            };
            self.check()?;
            Ok((info, kind))
        }
        fn click(&mut self, logical: (f32, f32), scale: f32) -> Result<()> {
            self.check()?;
            let x = (logical.0 * scale).round() as i32;
            let y = (logical.1 * scale).round() as i32;
            need(
                (0..=i16::MAX as i32).contains(&x) && (0..=i16::MAX as i32).contains(&y),
                "WM_MOUSE client 坐标无法无损编码",
            )?;
            let point = self.screen_point(logical, scale)?;
            need(point_equal(point, self.expected), "仅点击刚实测的原控件点")?;
            let packed = LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize);
            unsafe {
                PostMessageW(Some(self.hwnd), WM_LBUTTONDOWN, WPARAM(1), packed)?;
                PostMessageW(Some(self.hwnd), WM_LBUTTONUP, WPARAM(0), packed)?;
            }
            Ok(())
        }
    }
    async fn tick(cx: &mut AsyncApp) {
        cx.background_executor()
            .timer(Duration::from_millis(12))
            .await;
    }
    async fn first_present(
        handle: HWND,
        bounds: crate::geometry::ScreenRect,
        baseline: u32,
        original: POINT,
        started: Instant,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let pid = unsafe { GetCurrentProcessId() };
        let tid = unsafe { GetCurrentThreadId() };
        let mut observed_pid = 0;
        need(
            unsafe { GetWindowThreadProcessId(handle, Some(&mut observed_pid)) } == tid
                && observed_pid == pid,
            "首帧等待仅允许原 GPUI 线程窗口",
        )?;
        let class = class_name(handle)?;
        let process = ProcessHandle(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )?
        });
        let created = creation(process.0)?;
        let expected = RECT {
            left: bounds.left,
            top: bounds.top,
            right: bounds.left + bounds.width as i32,
            bottom: bounds.top + bounds.height as i32,
        };
        let expected_size = ((bounds.width << 16) | bounds.height) as usize;
        need(
            !unsafe { MonitorFromPoint(original, MONITOR_DEFAULTTONULL) }.is_invalid(),
            "原鼠标点必须位于物理显示器",
        )?;
        loop {
            need(
                input_tick()? == baseline,
                "首帧等待中真实输入变化；保留现场",
            )?;
            need(
                started.elapsed() < Duration::from_secs(2),
                "首帧等待超过2秒",
            )?;
            unsafe {
                observed_pid = 0;
                need(
                    IsWindow(Some(handle)).as_bool()
                        && GetWindowThreadProcessId(handle, Some(&mut observed_pid)) == tid
                        && observed_pid == pid
                        && class_name(handle)? == class
                        && GetCurrentThreadId() == tid
                        && GetProcessId(process.0) == pid
                        && creation(process.0)? == created,
                    "首帧等待中原 HWND/PID/TID/class/进程 HANDLE 变化",
                )?;
                let mut pointer = POINT::default();
                GetCursorPos(&mut pointer)?;
                need(
                    point_equal(pointer, original),
                    "首帧等待中鼠标位置变化；保留现场",
                )?;
                let mut actual = RECT::default();
                GetWindowRect(handle, &mut actual)?;
                let mut client = RECT::default();
                GetClientRect(handle, &mut client)?;
                let mut gui = GUITHREADINFO {
                    cbSize: size_of::<GUITHREADINFO>() as u32,
                    ..Default::default()
                };
                GetGUIThreadInfo(tid, &mut gui)?;
                let ready = IsWindowVisible(handle).as_bool()
                    && actual == expected
                    && client.left == 0
                    && client.top == 0
                    && client.right == bounds.width as i32
                    && client.bottom == bounds.height as i32
                    && GetForegroundWindow() == handle
                    && GetActiveWindow() == handle
                    && gui.hwndFocus == handle
                    && gui.hwndCapture.is_invalid()
                    && GetCapture().is_invalid()
                    && WindowFromPoint(original) == handle
                    && serial(handle) > 0
                    && GetPropW(handle, w!("RustShot.GpuiPresentSize")).0 as usize == expected_size;
                need(
                    input_tick()? == baseline,
                    "首帧查询中真实输入变化；保留现场",
                )?;
                need(
                    started.elapsed() < Duration::from_secs(2),
                    "首帧查询结束已超过2秒",
                )?;
                if ready {
                    return Ok(());
                }
            }
            need(
                started.elapsed() < Duration::from_secs(2),
                "原窗口首帧/fullphysical/focus/Present 等待超过2秒",
            )?;
            tick(cx).await;
        }
    }
    async fn painted(
        view: &WeakEntity<CaptureView>,
        id: &'static str,
        floor: usize,
        guard: &mut Guard,
        cx: &mut AsyncApp,
    ) -> Result<(f32, f32)> {
        let started = Instant::now();
        loop {
            guard.check()?;
            let position = view
                .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
                .map_err(|e| e.to_string())?;
            // probe 是真实 paint callback 写入，且需要设置/点击后的成功 Present。
            if let Some(position) = position {
                if guard.present_serial > floor {
                    return Ok(position);
                }
            }
            need(
                started.elapsed() < Duration::from_secs(2),
                "控件没有实际 paint probe/新成功 Present",
            )?;
            tick(cx).await;
        }
    }
    async fn samples(
        id: &'static str,
        expected: Option<&'static str>,
        point: POINT,
        guard: &mut Guard,
        cx: &mut AsyncApp,
        records: &mut Vec<Value>,
    ) -> Result<()> {
        guard.move_to(point)?;
        let settling = Instant::now();
        loop {
            tick(cx).await;
            let (_, kind) = guard.cursor()?;
            if expected.is_none_or(|expected| kind == expected) {
                break;
            }
            need(
                settling.elapsed() < Duration::from_secs(2),
                "生产 hitbox 未设置期望的 HAND/IBEAM",
            )?;
        }
        let mut first = 0;
        let mut last = 0;
        for index in 0..3 {
            if index != 0 {
                tick(cx).await;
            }
            let (info, kind) = guard.cursor()?;
            need(
                expected.is_none_or(|expected| kind == expected),
                "控件三次实际 cursor 样本不一致",
            )?;
            let sampled = qpc()?;
            if index == 0 {
                first = sampled;
            }
            last = sampled;
            records.push(json!({"control":id,"index":index,"qpc":sampled.to_string(),"kind":kind,"showing":info.flags==CURSOR_SHOWING,"cursor_handle":(info.hCursor.0 as usize).to_string(),"x":point.x,"y":point.y,"hwnd":(guard.hwnd.0 as usize).to_string(),"pid":guard.pid,"tid":guard.tid,"class":guard.class,"token":guard.token.to_string(),"creation_filetime":guard.creation.to_string(),"present_serial":guard.present_serial,"present_size":guard.present_size,"last_input_tick":guard.input_tick,"busy":false,"original_identity_focus_capture_and_point_checked":true}));
            // 每条已完成样本即写元数据，以便后续失败仍保留此前事实；不记录按键/像素。
            println!(
                "{}",
                json!({"phase":"cursor-controls-sample","sample":records.last()})
            );
        }
        need(
            (last - first) as f64 * 1000. / guard.frequency as f64 >= 20.,
            "三个样本跨度不足20ms",
        )?;
        Ok(())
    }
    async fn verify(
        view: WeakEntity<CaptureView>,
        handle: usize,
        scale: f32,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        // 输入基线仅由父进程在创建本子进程前传入；此处查询只校验，不重新基线化。
        let started = Instant::now();
        let baseline = startup_input_tick()?;
        need(
            input_tick()? == baseline,
            "从父进程启动前到本 case 首次检查已出现真实输入；保留现场",
        )?;
        let mut original = POINT::default();
        unsafe {
            GetCursorPos(&mut original)?;
        }
        let bounds = view
            .update(cx, |v, _| v.source.bounds)
            .map_err(|e| e.to_string())?;
        need(
            bounds.width < 65536 && bounds.height < 65536,
            "首帧 PresentSize 超出编码范围",
        )?;
        first_present(
            HWND(handle as *mut _),
            bounds,
            baseline,
            original,
            started,
            cx,
        )
        .await?;
        let mut guard = Guard::bind(HWND(handle as *mut _), bounds, baseline, original)?;
        let mut records = Vec::new();
        let floor = guard.present_serial;
        view.update(cx, |v, cx| {
            // 只准备自己的选区状态；目标控件坐标全部来自生产 paint probes。
            v.editor.crop = Some(v.source.screen_rect(v.source.current));
            v.editor.select_tool(Tool::Crop);
            v.toolbar_monitor = v.source.current;
            v.ui_positions.borrow_mut().clear();
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        let rectangle = painted(&view, "rectangle", floor, &mut guard, cx).await?;
        let point = guard.screen_point(rectangle, scale)?;
        samples(
            "rectangle",
            Some("HAND"),
            point,
            &mut guard,
            cx,
            &mut records,
        )
        .await?;
        let floor = guard.present_serial;
        guard.click(rectangle, scale)?;
        let width = painted(&view, "width-menu", floor, &mut guard, cx).await?;
        need(
            view.update(cx, |v, _| v.editor.tool == Tool::Rectangle)
                .map_err(|e| e.to_string())?,
            "真实 rectangle 按钮未选择 Rectangle",
        )?;
        let point = guard.screen_point(width, scale)?;
        guard.move_to(point)?;
        tick(cx).await;
        let floor = guard.present_serial;
        guard.click(width, scale)?;
        let slider = painted(&view, "parameter-slider", floor, &mut guard, cx).await?;
        need(
            view.update(cx, |v, _| {
                v.toolbar_panel == crate::toolbar_layout::ToolbarPanel::Width
            })
            .map_err(|e| e.to_string())?,
            "真实 width-menu 未打开 Width 参数面板",
        )?;
        let point = guard.screen_point(slider, scale)?;
        samples(
            "parameter-slider",
            Some("HAND"),
            point,
            &mut guard,
            cx,
            &mut records,
        )
        .await?;
        let value = painted(&view, "parameter-value", floor, &mut guard, cx).await?;
        let point = guard.screen_point(value, scale)?;
        samples(
            "parameter-value",
            Some("IBEAM"),
            point,
            &mut guard,
            cx,
            &mut records,
        )
        .await?;
        let original = guard.original;
        samples(
            "restore-original-own-point",
            None,
            original,
            &mut guard,
            cx,
            &mut records,
        )
        .await?;
        guard.check()?;
        unsafe {
            RemovePropW(guard.hwnd, TOKEN)?;
        }
        Ok(
            json!({"case":"cursor-controls","ok":true,"sample_count":records.len(),"control_count":3,"samples_per_control":3,"restore_samples":3,"sample_span_min_ms":20,"qpc_frequency":guard.frequency,"records":records,"input_baseline_origin":"parent captured LASTINPUTINFO.dwTime before child creation; RUSTSHOT_CURSOR_QA_INPUT_TICK","parent_prelaunch_input_tick":baseline,"input_baseline_reinitialized_by_child":false,"software_set_cursor_pos":true,"real_own_mousemove_allowed":true,"isolate_input_used":false,"send_input_used":false,"pre_set_cursor_used":false,"keyboard_messages_sent":false,"only_own_button_down_up_posted":true,"hardware_input_verified":false,"overlay_pixels_verified":false,"successful_present_scope":"actual full-canvas Present exists; setup/click probes require a newer Present; fresh Present per hover sample not required","png_written":false,"clipboard_accessed":false,"input_change_preserves_session":true,"original_pointer_restored":true} ),
        )
    }
    pub(super) fn schedule(
        view: &Entity<CaptureView>,
        window: &mut Window,
        _cx: &mut App,
        errors: Rc<RefCell<Option<String>>>,
    ) {
        let weak = view.downgrade();
        window.on_next_frame(move |window, cx| {
            let handle = hwnd(window).0 as usize;
            let scale = window.scale_factor();
            cx.spawn(async move |cx| {
                match verify(weak, handle, scale, cx).await {
                    Ok(report) => { println!("{report}"); cx.update(|cx| cx.quit()).ok(); }
                    Err(error) => {
                        let message = error.to_string();
                        *errors.borrow_mut() = Some(message.clone());
                        println!("{}", json!({"case":"cursor-controls","ok":false,"error":message,"session_preserved":true,"pointer_restore_attempted_after_failure":false,"close_or_quit_attempted":false}));
                        // 不恢复位置、不关窗；调用方保留原 HANDLE、日志与现场。
                    }
                }
            }).detach();
        });
    }
}
