//! 32k 水印真实贴图专项；原图、工作区、两级输入、导出和退休共用生产路径。
use crate::{
    annotation::Tool,
    capture::Image,
    capture_view::{Action, CaptureView},
    gpui_capture::hwnd,
    gpui_smoke::Input,
    process_memory::{cpu_memory, gpu_memory},
    pin_model::{PinState, Placement},
    pin_properties::PinPropertyPopup,
    pin_property_bar::PinPropertyBar,
    pin_tool_picker::PinToolPicker,
    pin_toolbar::PinToolbar,
    pin_view::{PinAction, PinView},
    text_contract_smoke::{self, AdapterStats},
    text_field::TextField,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{
    div, prelude::*, App, AppContext, Application, AsyncApp, Context, Render, RenderImage,
    WeakEntity, Window, WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        System::Threading::GetCurrentThreadId,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};
const WIDTH: u32 = 960;
const HEIGHT: u32 = 32_000;
const PIXEL_BUDGET: u64 = 8_000_000;
const GROUP: &str = "three-cycle-32000px-watermark-pin";
const WAIT: Duration = Duration::from_secs(35);
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
fn rect_json(r: crate::geometry::Rect) -> Value {
    json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height})
}
fn byte_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
async fn settle(cx: &mut AsyncApp) {
    for _ in 0..4 {
        tick(cx).await;
    }
}
struct OwnedWindow {
    handle: usize,
    process: u32,
    thread: u32,
}
struct GuardState {
    windows: RefCell<Vec<OwnedWindow>>,
    attempts: RefCell<Vec<Value>>,
    token: Rc<()>,
}
thread_local! {
    static OUTPUT_GUARD: RefCell<Option<Rc<GuardState>>> = const { RefCell::new(None) };
}
struct OutputGuard(Rc<GuardState>);
impl OutputGuard {
    fn new() -> Result<Self> {
        let state = Rc::new(GuardState {
            windows: RefCell::new(Vec::new()),
            attempts: RefCell::new(Vec::new()),
            token: Rc::new(()),
        });
        OUTPUT_GUARD.with(|slot| -> Result<()> {
            ensure(slot.borrow().is_none(), "长图水印守卫不能嵌套")?;
            *slot.borrow_mut() = Some(state.clone());
            Ok(())
        })?;
        Ok(Self(state))
    }
    fn register(&self, input: Input) -> Result<()> {
        let h = HWND(input.handle as *mut _);
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut process)) };
        ensure(
            thread == unsafe { GetCurrentThreadId() } && process == std::process::id(),
            "只登记原自有 UI HWND",
        )?;
        unsafe {
            SetPropW(
                h,
                w!("RustShot.WatermarkLongMemory.OutputGuard"),
                Some(HANDLE(Rc::as_ptr(&self.0.token) as *mut _)),
            )?;
        }
        let mut windows = self.0.windows.borrow_mut();
        if !windows.iter().any(|w| w.handle == input.handle) {
            windows.push(OwnedWindow {
                handle: input.handle,
                process,
                thread,
            });
        }
        Ok(())
    }
    fn matches(&self, handle: HWND) -> bool {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(handle, Some(&mut process)) };
        self.0
            .windows
            .borrow()
            .iter()
            .any(|v| v.handle == handle.0 as usize && v.thread == thread && v.process == process)
            && process == std::process::id()
            && unsafe {
                IsWindow(Some(handle)).as_bool()
                    && GetPropW(handle, w!("RustShot.WatermarkLongMemory.OutputGuard")).0
                        == Rc::as_ptr(&self.0.token) as *mut _
            }
    }
    fn attempts(&self) -> usize {
        self.0.attempts.borrow().len()
    }
}
impl Drop for OutputGuard {
    fn drop(&mut self) {
        OUTPUT_GUARD.with(|slot| {
            if slot
                .borrow()
                .as_ref()
                .is_some_and(|s| Rc::ptr_eq(s, &self.0))
            {
                *slot.borrow_mut() = None;
            }
        });
        for owned in self.0.windows.borrow().iter() {
            let h = HWND(owned.handle as *mut _);
            unsafe {
                if GetPropW(h, w!("RustShot.WatermarkLongMemory.OutputGuard")).0
                    == Rc::as_ptr(&self.0.token) as *mut _
                {
                    let _ = RemovePropW(h, w!("RustShot.WatermarkLongMemory.OutputGuard"));
                }
            }
        }
    }
}
fn guard_output(window: &Window, domain: &str, action: &str) -> bool {
    OUTPUT_GUARD.with(|slot| {
        let state = slot.borrow();
        let Some(state) = state.as_ref() else { return false };
        let mut process = 0;
        let h = hwnd(window);
        let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut process)) };
        if !state.windows.borrow().iter().any(|o| o.handle == h.0 as usize && o.process == process && o.thread == thread)
            || unsafe { GetPropW(h, w!("RustShot.WatermarkLongMemory.OutputGuard")).0 != Rc::as_ptr(&state.token) as *mut _ } { return false; }
        state.attempts.borrow_mut().push(json!({"unix_ms":unix_ms().ok(),"domain":domain,"action":action,"hwnd":h.0 as usize,"blocked_before_side_effect":true}));
        true
    })
}
pub(crate) fn guard_capture_output(action: &Action, window: &Window) -> bool {
    let action = match action {
        Action::Copy => "Copy",
        Action::Save => "Save",
        Action::QuickSave => "QuickSave",
        Action::Cancel => "Cancel",
        Action::Pin => "Pin",
        Action::Record => "Record",
        Action::LongCapture => "LongCapture",
        Action::Reselect => "Reselect",
        _ => return false,
    };
    guard_output(window, "CaptureView", action)
}
pub(crate) fn guard_pin_output(action: &PinAction, window: &Window) -> bool {
    let action = match action {
        PinAction::Copy => "Copy",
        PinAction::CopyText => "CopyText",
        PinAction::CopyParagraphText => "CopyParagraphText",
        PinAction::Close => "Close",
        PinAction::Save => "Save",
        PinAction::QuickSave => "QuickSave",
        PinAction::Hide => "Hide",
        PinAction::Crop => "Crop",
        PinAction::Duplicate => "Duplicate",
        PinAction::Edit => "Edit",
        PinAction::Chrome => "Chrome",
        _ => return false,
    };
    guard_output(window, "PinView", action)
}
struct Report {
    root: PathBuf,
    file: File,
    stages: Vec<Value>,
}
impl Report {
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        let stage = json!({"name":name,"unix_ms":unix_ms()?,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail});
        serde_json::to_writer(&mut self.file, &stage)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.stages.push(stage);
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Surface {
    Main(WindowHandle<PinView>),
    Primary(WindowHandle<PinToolbar>),
    Properties(WindowHandle<PinPropertyBar>),
    Popup(WindowHandle<PinPropertyPopup>),
    Picker(WindowHandle<PinToolPicker>),
}
impl Surface {
    fn input(self, cx: &mut AsyncApp) -> Result<Input> {
        let input = |w: &Window| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        };
        Ok(match self {
            Self::Main(h) => h.update(cx, |_, w, _| input(w))?,
            Self::Primary(h) => h.update(cx, |_, w, _| input(w))?,
            Self::Properties(h) => h.update(cx, |_, w, _| input(w))?,
            Self::Popup(h) => h.update(cx, |_, w, _| input(w))?,
            Self::Picker(h) => h.update(cx, |_, w, _| input(w))?,
        })
    }
}
#[derive(Clone, Copy)]
struct Host(WindowHandle<PinView>);
impl Host {
    fn canvas<T>(
        self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut CaptureView, &mut Window, &mut Context<CaptureView>) -> T,
    ) -> Result<T> {
        Ok(self.0.update(cx, |v, w, cx| {
            v.canvas
                .as_ref()
                .expect("自有长图 canvas")
                .update(cx, |c, cx| f(c, w, cx))
        })?)
    }
    fn primary(self, cx: &mut AsyncApp) -> Result<Surface> {
        self.0
            .update(cx, |v, _, _| {
                v.toolbar.map(Surface::Primary).ok_or("长图一级栏缺失")
            })?
            .map_err(Into::into)
    }
    fn properties(self, popup: bool, cx: &mut AsyncApp) -> Result<Surface> {
        self.0.update(cx, |v, _, cx| -> Result<Surface> {
            v.toolbar
                .ok_or("长图一级栏缺失")?
                .update(cx, |b, _, cx| {
                    if popup {
                        b.popup(cx).map(Surface::Popup).ok_or("长图参数弹窗缺失")
                    } else {
                        b.properties
                            .map(Surface::Properties)
                            .ok_or("长图二级栏缺失")
                    }
                })?
                .map_err(Into::into)
        })?
    }
    fn point(self, surface: Surface, id: &str, cx: &mut AsyncApp) -> Result<Option<(f32, f32)>> {
        Ok(match surface {
            Surface::Primary(h) => h.update(cx, |b, _, _| b.positions.borrow().get(id).copied())?,
            Surface::Picker(h) => h.update(cx, |b, _, _| b.positions.borrow().get(id).copied())?,
            _ => self.canvas(cx, |c, _, _| c.ui_positions.borrow().get(id).copied())?,
        })
    }
    fn field<T>(
        self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut TextField, &mut Window, &mut Context<TextField>) -> T,
    ) -> Result<T> {
        let field = self.canvas(cx, |c, _, _| {
            c.watermark_field.clone().ok_or("长图水印输入缺失")
        })??;
        let Surface::Properties(window) = self.properties(false, cx)? else {
            return Err("长图输入窗口类型错误".into());
        };
        Ok(window.update(cx, |_, w, cx| field.update(cx, |v, cx| f(v, w, cx)))?)
    }
}
fn install(input: Input, guard: &OutputGuard, stats: &Rc<AdapterStats>) -> Result<()> {
    guard.register(input)?;
    text_contract_smoke::install(input, stats)
}
async fn button(
    host: Host,
    surface: Surface,
    id: &str,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + WAIT;
    let point = loop {
        if let Some(point) = host.point(surface, id, cx)? {
            break point;
        }
        ensure(Instant::now() < end, &format!("长图实际按钮未绘制：{id}"))?;
        tick(cx).await;
    };
    let input = surface.input(cx)?;
    install(input, guard, stats)?;
    ensure(
        guard.matches(HWND(input.handle as *mut _)),
        "按钮原 HWND 身份失效",
    )?;
    input.click(point);
    settle(cx).await;
    ensure(guard.attempts() == 0, "实际点击触发被阻断的输出/关闭")?;
    Ok(
        json!({"id":id,"hwnd":input.handle,"logical_point":[point.0,point.1],"dpi":input.scale,"posted_own_wndproc":true}),
    )
}
fn mouse_activation(input: Input, guard: &OutputGuard) -> Result<Value> {
    let h = HWND(input.handle as *mut _);
    let root = unsafe { GetAncestor(h, GA_ROOTOWNER) };
    let original = || {
        guard.matches(h)
            && guard.matches(root)
            && unsafe {
                GetAncestor(h, GA_ROOTOWNER) == root
                    && GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) == root
            }
    };
    ensure(original(), "只激活当前自有前台窗口组")?;
    ensure(
        unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize == 0,
        "长图水印仍有 NOACTIVATE",
    )?;
    let result = unsafe {
        SendMessageW(
            h,
            WM_MOUSEACTIVATE,
            Some(WPARAM(root.0 as usize)),
            Some(LPARAM(
                HTCLIENT as isize | ((WM_LBUTTONDOWN as isize) << 16),
            )),
        )
    };
    ensure(
        result.0 == MA_ACTIVATE as isize && original(),
        "实际输入窗未接受激活",
    )?;
    unsafe {
        let _ = SetActiveWindow(h);
    }
    ensure(original(), "激活期间自有前台身份漂移")?;
    unsafe {
        let _ = SetFocus(Some(h));
    }
    ensure(
        original() && unsafe { GetFocus() } == h,
        "实际输入窗未获取 native focus",
    )?;
    Ok(
        json!({"hwnd":input.handle,"root_hwnd":root.0 as usize,"mouseactivate_result":result.0,"native_mouse_activation_simulated":true,"hardware_activation_tested":false,"gpui_focus_overridden":false}),
    )
}
async fn field_click(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let surface = host.properties(false, cx)?;
    let input = surface.input(cx)?;
    install(input, guard, stats)?;
    let activation = mouse_activation(input, guard)?;
    settle(cx).await;
    let click = button(host, surface, "watermark-field", guard, stats, cx).await?;
    let gpui = host.field(cx, |v, w, _| v.focus_handle.is_focused(w))?;
    let routed = host.canvas(cx, |c, _, _| c.watermark_input_focused)?;
    ensure(
        gpui && routed && unsafe { GetFocus().0 as usize } == input.handle,
        "长图实际 native/GPUI 输入焦点不一致",
    )?;
    Ok(
        json!({"click":click,"mouse_activation":activation,"native_focus_matches":true,"gpui_focus":gpui,"production_input_focused":routed}),
    )
}
async fn type_text(
    host: Host,
    value: &str,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let focus = field_click(host, guard, stats, cx).await?;
    let input = host.properties(false, cx)?.input(cx)?;
    text_contract_smoke::key(input, VK_A, true, false, false)?;
    settle(cx).await;
    for unit in value.encode_utf16() {
        ensure(
            guard.matches(HWND(input.handle as *mut _)),
            "字符原 HWND 已失效",
        )?;
        unsafe {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_CHAR,
                WPARAM(unit as usize),
                LPARAM(0),
            )?;
        }
    }
    settle(cx).await;
    ensure(
        host.field(cx, |v, _, _| {
            v.committed_text() == value && !v.is_composing()
        })?,
        "长图 Unicode 没有实际提交",
    )?;
    Ok(
        json!({"actual_text":value,"utf16_units":value.encode_utf16().count(),"focus":focus,"committed_by_production_input":true}),
    )
}
async fn parameter(
    host: Host,
    menu: &str,
    preset: &str,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let open = button(host, host.properties(false, cx)?, menu, guard, stats, cx).await?;
    let choice = button(host, host.properties(true, cx)?, preset, guard, stats, cx).await?;
    ensure(
        host.canvas(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::None)?,
        "快选没有提交并关闭",
    )?;
    Ok(
        json!({"menu":menu,"open":open,"choice":choice,"commit_kind":"quick-preset-commits-and-closes","explicit_apply_clicked":false}),
    )
}
async fn presented(host: Host, cx: &mut AsyncApp) -> Result<()> {
    let signal = Rc::new(Cell::new(false));
    let ready = signal.clone();
    host.0.update(cx, |_, w, _| {
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, _| ready.set(true));
            w.refresh();
        });
        w.refresh();
    })?;
    let end = Instant::now() + WAIT;
    while !signal.get() {
        ensure(Instant::now() < end, "长图真实呈现帧超时")?;
        tick(cx).await;
    }
    settle(cx).await;
    Ok(())
}
async fn ready(host: Host, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if host.0.update(cx, |v, w, _| -> Result<bool> {
            Ok(!v.processing
                && !v.saving
                && v.canvas.is_some()
                && v.status.is_empty()
                && v.placement(w)? == crate::pin_native::rect(hwnd(w))?)
        })?? {
            presented(host, cx).await?;
            return Ok(());
        }
        ensure(Instant::now() < end, "长图生产处理/定位未收敛")?;
    }
}
fn source(host: Host, cx: &mut AsyncApp) -> Result<Value> {
    host.0.update(cx, |v, _, cx| -> Result<Value> {
        let bytes = v.original.as_bytes(0).ok_or("长图原像素缺失")?;
        let canvas = v.canvas.as_ref().ok_or("长图共享画布缺失")?.read(cx);
        let preview = canvas.previews.first().ok_or("長图底图预览缺失")?;
        let original = canvas.source.original(preview);
        Ok(json!({"render_image_id":v.original.id.0,"bgra_pointer":bytes.as_ptr() as usize,"bgra_length":bytes.len(),"dimensions":v.original_size,"shared_with_image":Arc::ptr_eq(&v.original,&v.image),"shared_with_canvas_source":std::ptr::eq(v.original.as_ref(),original)}))
    })?
}
#[derive(Default)]
struct Lifetimes {
    render: Vec<Weak<RenderImage>>,
    fields: Vec<WeakEntity<TextField>>,
    canvases: Vec<WeakEntity<CaptureView>>,
    pins: Vec<WeakEntity<PinView>>,
}
impl Lifetimes {
    fn observe(&mut self, host: Host, cx: &mut AsyncApp) -> Result<Value> {
        host.0.update(cx, |v, w, cx| -> Result<Value> {
            self.pins.push(cx.entity().downgrade());
            self.render.extend([Arc::downgrade(&v.original),Arc::downgrade(&v.image)]);
            let ids = v.preview_texture_ids_for_qa();
            let canvas = v.canvas.as_ref().ok_or("长图 cache probe 没有 canvas")?;
            self.canvases.push(canvas.downgrade());
            let c = canvas.read(cx);
            let visible = crate::pin_annotation::shared_preview_region(c, w);
            let base = c.source.screen_rect(0);
            ensure(base == visible, "共享底图预览未使用真实屏内范围")?;
            if let Some(field) = &c.watermark_field { self.fields.push(field.downgrade()); }
            let previews: Vec<_> = c.watermark_previews.iter().map(|p| {
                self.render.push(Arc::downgrade(&p.image));
                json!({"image_id":p.image.id.0,"rect":rect_json(p.rect),"dimensions":p.dimensions,"pixels":p.dimensions.0 as u64*p.dimensions.1 as u64,"cpu_bytes":p.image.as_bytes(0).map_or(0,|b|b.len())})
            }).collect();
            self.render.extend(c.previews.iter().map(Arc::downgrade));
            self.render.extend(c.raster_previews.iter().map(|(_,p)|Arc::downgrade(p)));
            let pixels: u64 = c.watermark_previews.iter().map(|p|p.dimensions.0 as u64*p.dimensions.1 as u64).sum();
            ensure(pixels <= PIXEL_BUDGET, "长图水印显示缓存超过 8M 像素")?;
            ensure(c.watermark_previews.iter().all(|p|p.rect==visible), "水印与共享底图没有使用同一个屏内 ROI")?;
            ensure(c.watermark_previews.iter().all(|p|ids.contains(&p.image.id.0)), "水印图没有进入实际 Pin 生命周期 current")?;
            Ok(json!({"watermark_previews":previews,"watermark_preview_count":c.watermark_previews.len(),"watermark_preview_pixels":pixels,"watermark_preview_cpu_bytes":pixels*4,"watermark_preview_pixel_limit":PIXEL_BUDGET,"base_shared_preview_rect":rect_json(base),"actual_shared_visible_region":rect_json(visible),"watermark_matches_shared_visible_region":true,"pin_lifecycle_current_ids":ids,"pin_lifecycle_counts":v.preview_texture_lifecycle(),"derived_cache_not_source_pixels":true,"all_gpu_or_com_verified":false}))
        })?
    }
    fn expired(&self) -> bool {
        self.render.iter().all(|v| v.upgrade().is_none())
            && self.fields.iter().all(|v| v.upgrade().is_none())
            && self.canvases.iter().all(|v| v.upgrade().is_none())
            && self.pins.iter().all(|v| v.upgrade().is_none())
    }
}
async fn pose(host: Host, band: &str, zoom: f32, cx: &mut AsyncApp) -> Result<Value> {
    host.0
        .update(cx, |v, w, cx| v.zoom(zoom, Some((0., 0.)), w, cx))??;
    ready(host, cx).await?;
    let placement = host.0.update(cx, |v, w, cx| -> Result<Placement> {
        let mut p = v.placement(w)?;
        let work = crate::pin_native::work_area((0, 0));
        let world_y = match band {
            "top" => 0.,
            "middle" => HEIGHT as f32 / 2. - 500.,
            "bottom" => HEIGHT as f32 - 1000.,
            _ => return Err("未知长图观察位置".into()),
        };
        p.x = work.x + 160;
        p.y = work.y + 160 - (world_y * v.state.zoom).round() as i32;
        v.request_placement(p, w, cx);
        Ok(p)
    })??;
    ready(host, cx).await?;
    let value = host.canvas(cx, |c,w,_| {
        let vp=c.canvas_viewport(w);
        json!({"band":band,"requested_zoom":zoom,"actual_zoom":vp.zoom,"placement":{"x":placement.x,"y":placement.y,"width":placement.width,"height":placement.height},"projection_offset":[vp.offset.x,vp.offset.y],"native_pan_production_request_placement":true,"source_dimensions":[WIDTH,HEIGHT]})
    })?;
    ensure(
        (value["actual_zoom"].as_f64().unwrap_or(0.) - f64::from(zoom)).abs() < 0.001,
        "长图合法缩放没有实际生效",
    )?;
    Ok(value)
}
fn visible_roi(host: Host, cx: &mut AsyncApp) -> Result<[u32; 4]> {
    host.0.update(cx, |v, w, _| -> Result<_> {
        let p = v.placement(w)?;
        let work = crate::pin_native::work_area((0, 0));
        let left = p.x.max(work.x + 12) + 4;
        let top = p.y.max(work.y + 168) + 4;
        let right =
            (p.x as i64 + i64::from(p.width)).min(work.x as i64 + i64::from(work.width) - 12);
        let bottom =
            (p.y as i64 + i64::from(p.height)).min(work.y as i64 + i64::from(work.height) - 120);
        ensure(
            right - i64::from(left) >= 80 && bottom - i64::from(top) >= 100,
            "长图自有屏内 ROI 不足",
        )?;
        Ok([
            (left - p.x) as u32,
            (top - p.y) as u32,
            (right - i64::from(left)).min(480) as u32,
            (bottom - i64::from(top)).min(360) as u32,
        ])
    })?
}
// 光标已改由系统绘制、不进入 GPU 帧；保留指针周围排除区作保守 ROI，不影响像素 oracle。
const CURSOR_MARGIN_DIP: f32 = 14.;
#[derive(Clone, Copy, PartialEq)]
struct CursorGeometry {
    pointer: (i32, i32),
    viewport: crate::viewport::Viewport,
    dpi: f32,
    center: crate::annotation::Point,
    exclusion: [i32; 4],
}
impl CursorGeometry {
    fn intersects(self, roi: [u32; 4]) -> bool {
        (roi[0] as i64) < i64::from(self.exclusion[2])
            && (roi[1] as i64) < i64::from(self.exclusion[3])
            && i64::from(roi[0]) + i64::from(roi[2]) > i64::from(self.exclusion[0])
            && i64::from(roi[1]) + i64::from(roi[3]) > i64::from(self.exclusion[1])
    }
    fn value(self, phase: &str) -> Value {
        json!({"phase":phase,"pointer_source":[self.pointer.0,self.pointer.1],"viewport":{"zoom":self.viewport.zoom,"offset":[self.viewport.offset.x,self.viewport.offset.y]},"dpi":self.dpi,"center_physical":[self.center.x,self.center.y],"exclusion":self.exclusion})
    }
}
fn cursor_geometry(host: Host, cx: &mut AsyncApp) -> Result<CursorGeometry> {
    host.canvas(cx, |c, w, _| {
        let viewport = c.canvas_viewport(w);
        let center = viewport.window_point(crate::annotation::Point::from_tuple(c.pointer));
        let dpi = w.scale_factor();
        let margin = CURSOR_MARGIN_DIP * dpi;
        CursorGeometry {
            pointer: c.pointer,
            viewport,
            dpi,
            center,
            exclusion: [
                (center.x - margin).floor() as i32,
                (center.y - margin).floor() as i32,
                (center.x + margin).ceil() as i32,
                (center.y + margin).ceil() as i32,
            ],
        }
    })
}
fn roi(host: Host, cx: &mut AsyncApp) -> Result<([u32; 4], [u32; 4])> {
    let candidate = visible_roi(host, cx)?;
    let cursor = cursor_geometry(host, cx)?;
    if !cursor.intersects(candidate) {
        return Ok((candidate, candidate));
    }
    let [x, y, width, height] = candidate;
    let right = x + width;
    let bottom = y + height;
    let clamp_x = |value: i32| i64::from(value).clamp(i64::from(x), i64::from(right)) as u32;
    let clamp_y = |value: i32| i64::from(value).clamp(i64::from(y), i64::from(bottom)) as u32;
    let [left, top, end_x, end_y] = cursor.exclusion;
    let end_x = clamp_x(end_x);
    let end_y = clamp_y(end_y);
    let selected = [
        [x, y, clamp_x(left) - x, height],
        [end_x, y, right - end_x, height],
        [x, y, width, clamp_y(top) - y],
        [x, end_y, width, bottom - end_y],
    ]
    .into_iter()
    .filter(|r| r[2] >= 80 && r[3] >= 100 && !cursor.intersects(*r))
    .max_by_key(|r| u64::from(r[2]) * u64::from(r[3]))
    .ok_or("避开编辑光标后没有足够大的屏内采样区域")?;
    Ok((candidate, selected))
}
async fn frame(
    host: Host,
    roi: [u32; 4],
    phase: &str,
    samples: &mut Vec<Value>,
    cx: &mut AsyncApp,
) -> Result<Image> {
    presented(host, cx).await?;
    let before = cursor_geometry(host, cx)?;
    ensure(!before.intersects(roi), "GPU 采样前编辑光标进入冻结 ROI")?;
    samples.push(before.value(&format!("{phase}-before")));
    host.0
        .update(cx, |_, w, _| w.request_frame_capture(roi))??;
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if let Some(result) = host.0.update(cx, |_, w, _| w.take_frame_capture())? {
            let bytes = result.map_err(|e| e.to_string())?;
            let after = cursor_geometry(host, cx)?;
            ensure(
                before == after && !after.intersects(roi),
                "GPU 采样期间光标或视口变化，不能沿用旧 ROI",
            )?;
            samples.push(after.value(&format!("{phase}-after")));
            ensure(
                bytes.len() == roi[2] as usize * roi[3] as usize * 4,
                "GPU ROI 实际字节数错误",
            )?;
            return Ok(Image {
                width: roi[2],
                height: roi[3],
                bgra: bytes,
                readback_ms: 0.,
            });
        }
        ensure(Instant::now() < end, "长图 GPU ROI 返回超时")?;
    }
}
fn png(image: Image, path: &Path) -> Result<Value> {
    let width = image.width;
    let height = image.height;
    ensure(!path.exists(), "禁止覆盖长图 QA 图片")?;
    crate::export::save_png(image, path)?;
    Ok(json!({"path":path,"bytes":std::fs::metadata(path)?.len(),"width":width,"height":height}))
}
#[allow(clippy::too_many_arguments)]
async fn gpu_pair(
    host: Host,
    cycle: u32,
    band: &str,
    root: &Path,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    life: &mut Lifetimes,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (candidate, rect) = roi(host, cx)?;
    let mut cursor_samples = Vec::new();
    let before = frame(host, rect, "watermark", &mut cursor_samples, cx).await?;
    let before_hash = byte_hash(&before.bgra);
    // 清空是独立可撤销命令，不能并入刚才的连续文字/样式组。
    field_click(host, guard, stats, cx).await?;
    let input = host.properties(false, cx)?.input(cx)?;
    text_contract_smoke::key(input, VK_RETURN, false, false, false)?;
    settle(cx).await;
    let clear = button(
        host,
        host.properties(false, cx)?,
        "watermark-clear",
        guard,
        stats,
        cx,
    )
    .await?;
    let empty = frame(host, rect, "empty", &mut cursor_samples, cx).await?;
    let changed = before
        .bgra
        .chunks_exact(4)
        .zip(empty.bgra.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    let non_white = empty
        .bgra
        .chunks_exact(4)
        .filter(|p| *p != [255, 255, 255, 255])
        .count();
    let first_non_white = empty
        .bgra
        .chunks_exact(4)
        .enumerate()
        .find(|(_, p)| *p != [255, 255, 255, 255])
        .map(|(index, pixel)| {
            json!({"x":index % empty.width as usize,"y":index / empty.width as usize,"bgra":pixel})
        });
    let surface_geometry = host.0.update(cx, |v, w, cx| -> Result<Value> {
        let native = crate::pin_native::rect(hwnd(w))?;
        let mut client = RECT::default();
        let mut origin = POINT::default();
        unsafe {
            GetClientRect(hwnd(w), &mut client)?;
            ensure(
                windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(w), &mut origin).as_bool(),
                "自有窗口客户区原点读取失败",
            )?;
        }
        let canvas = v.canvas.as_ref().ok_or("GPU 诊断画布缺失")?.read(cx);
        let visible = crate::pin_annotation::shared_preview_region(canvas, w);
        let viewport = canvas.canvas_viewport(w);
        Ok(json!({"native_window":{"x":native.x,"y":native.y,"width":native.width,"height":native.height},"client":{"screen_x":origin.x,"screen_y":origin.y,"left":client.left,"top":client.top,"right":client.right,"bottom":client.bottom},"actual_dpi":w.scale_factor(),"viewport_zoom":viewport.zoom,"viewport_offset":[viewport.offset.x,viewport.offset.y],"base_shared_region":rect_json(canvas.source.screen_rect(0)),"visible_region":rect_json(visible),"selection":canvas.selected().ok().map(rect_json),"canvas_status":canvas.status,"pin_status":v.status,"tool":format!("{:?}",canvas.editor.tool),"watermark_cache_count":canvas.watermark_previews.len()}))
    })??;
    // 判据失败也保留实际帧，便于区分画布缺块、HUD 和取样坐标错误。
    let with = png(
        before,
        &root.join(format!("cycle-{cycle}-{band}-watermark-gpu.png")),
    )?;
    let without = png(
        empty,
        &root.join(format!("cycle-{cycle}-{band}-empty-gpu.png")),
    )?;
    let diagnosis = json!({"cycle":cycle,"band":band,"roi":rect,"sampling":{"candidate_roi":candidate,"roi":rect,"cursor_margin_dip":CURSOR_MARGIN_DIP,"cursor_samples":cursor_samples},"surface_geometry":surface_geometry,"actual_changed_pixels":changed,"empty_non_white_pixels":non_white,"first_non_white":first_non_white,"watermark":with,"empty":without});
    let mut diagnostic_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(format!("cycle-{cycle}-{band}-gpu-diagnostic.json")))?;
    serde_json::to_writer(&mut diagnostic_file, &diagnosis)?;
    ensure(changed > 0, "长图位置真实 GPU 没有水印像素差异")?;
    ensure(
        non_white == 0,
        &format!("长图 empty ROI 混入 HUD/边框/其它像素：{diagnosis}"),
    )?;
    let undo = button(host, host.primary(cx)?, "undo", guard, stats, cx).await?;
    let restored = frame(host, rect, "restored", &mut cursor_samples, cx).await?;
    ensure(
        byte_hash(&restored.bgra) == before_hash,
        "清空撤销后同一 GPU ROI 没有精确恢复",
    )?;
    let restored_with = png(
        restored,
        &root.join(format!("cycle-{cycle}-{band}-restored-gpu.png")),
    )?;
    let cache = life.observe(host, cx)?;
    Ok(
        json!({"cycle":cycle,"band":band,"roi":rect,"sampling":{"candidate_roi":candidate,"roi":rect,"cursor_margin_dip":CURSOR_MARGIN_DIP,"cursor_samples":cursor_samples},"watermark":with,"empty":without,"restored":restored_with,"actual_changed_pixels":changed,"restored_bgra_fnv64":before_hash.to_string(),"restored_same_roi_exact":true,"clear":clear,"undo":undo,"cache":cache,"scope":"same owned framebuffer ROI outside editor cursor, empty reference white; not exact glyph oracle"}),
    )
}
async fn workspace_idle(host: Host, cx: &mut AsyncApp) -> Result<Value> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if let Some(probe) = host.0.update(cx, |v, _, _| v.workspace_snapshot_probe())? {
            ensure(probe.snapshot_failures == 0, "长图真实工作区自动保存失败")?;
            if !probe.busy
                && probe.snapshots_committed > 0
                && probe.encode_starts == probe.encode_finishes
            {
                // 后续内容仍可能在静置期；保持真实 tick 直到静置上限后再次核对。
                return Ok(serde_json::to_value(probe)?);
            }
        }
        ensure(Instant::now() < end, "长图工作区保存未完成")?;
    }
}
async fn seal_undo(
    host: Host,
    cycle: u32,
    life: &mut Lifetimes,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let before = life.observe(host, cx)?;
    let old_canvas = host.0.update(cx, |v, _, _| {
        v.canvas.as_ref().expect("seal canvas").downgrade()
    })?;
    let old_canvas_id = old_canvas.entity_id();
    let old_ids = host.canvas(cx, |c, _, _| {
        c.watermark_previews
            .iter()
            .map(|p| p.image.id.0)
            .collect::<Vec<_>>()
    })?;
    let original_target = host
        .canvas(cx, |c, _, _| {
            c.editor
                .document
                .items()
                .iter()
                .find(|o| o.annotation.tool == Tool::Watermark)
                .map(|o| o.id)
        })?
        .ok_or("seal 原水印对象缺失")?;
    let old_wm = host.canvas(cx, |c, _, _| {
        c.watermark_previews
            .iter()
            .map(|p| Arc::downgrade(&p.image))
            .collect::<Vec<_>>()
    })?;
    let source_before = source(host, cx)?;
    host.0
        .update(cx, |v, w, cx| v.perform(PinAction::Brightness(10), w, cx))?;
    ready(host, cx).await?;
    let old_cache = old_canvas
        .upgrade()
        .ok_or("生产合并没有保存原 canvas checkpoint")?;
    let old_cache_counts = cx.update(|cx| {
        let c = old_cache.read(cx);
        [
            c.watermark_previews.len(),
            c.raster_previews.len(),
            c.retired_raster_previews.len(),
        ]
    })?;
    drop(old_cache);
    ensure(
        old_cache_counts[0] == 0,
        "合并 checkpoint 仍持有水印显示派生缓存",
    )?;
    ensure(
        old_wm.iter().all(|p| p.upgrade().is_none()),
        "旧水印缓存 CPU Weak 合并后没有过期",
    )?;
    let after = life.observe(host, cx)?;
    let before_released = before["pin_lifecycle_counts"][3]
        .as_u64()
        .ok_or("缺少释放计数")?;
    ensure(
        after["pin_lifecycle_counts"][1] == 0
            && after["pin_lifecycle_counts"][3].as_u64().unwrap_or(0) > before_released,
        "合并后的旧 GPU key 没有实际退休",
    )?;
    let after_ids = host
        .0
        .update(cx, |v, _, _| v.preview_texture_ids_for_qa())?;
    ensure(
        old_ids.iter().all(|id| !after_ids.contains(id)),
        "合并后 current 仍持有旧水印 key",
    )?;
    let source_sealed = source(host, cx)?;
    for _ in 0..2 {
        host.0
            .update(cx, |v, w, cx| v.perform(PinAction::Undo, w, cx))?;
        ready(host, cx).await?;
    }
    let restored = source(host, cx)?;
    ensure(
        restored == source_before,
        "生产 Undo 没有恢复原共享底图身份",
    )?;
    ensure(
        host.canvas(cx, |c, _, _| {
            c.editor
                .document
                .items()
                .iter()
                .any(|o| o.id == original_target && o.annotation.tool == Tool::Watermark)
        })?,
        "生产 Undo 没有恢复原水印对象",
    )?;
    ensure(
        host.0.update(cx, |v, _, _| {
            v.canvas
                .as_ref()
                .is_some_and(|c| c.entity_id() == old_canvas_id)
        })?,
        "生产 Undo 没有恢复原 canvas checkpoint",
    )?;
    let cache = life.observe(host, cx)?;
    ensure(
        cache["watermark_preview_count"].as_u64().unwrap_or(0) > 0,
        "Undo 没有重建实际水印显示缓存",
    )?;
    report.stage("actual-watermark-seal-and-undo",json!({"cycle":cycle,"action":"PinAction::Brightness(10), Undo, Undo","cache_before":before,"cache_after_seal":after,"cache_after_undo":cache,"source_before":source_before,"source_sealed_generation":source_sealed,"source_after_undo":restored,"old_watermark_ids":old_ids,"current_ids_after_seal":after_ids,"original_target_id":original_target,"old_checkpoint_cache_counts":old_cache_counts,"old_watermark_cpu_weaks_expired":true,"old_canvas_restored_by_undo":true,"atlas_residency_directly_queried":false}))
}
async fn close_cycle(
    host: Host,
    cycle: u32,
    guard: &OutputGuard,
    life: &Lifetimes,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let main = Surface::Main(host.0).input(cx)?;
    let original: Vec<_> = guard
        .0
        .windows
        .borrow()
        .iter()
        .map(|w| w.handle)
        .filter(|h| unsafe { IsWindow(Some(HWND(*h as *mut _))).as_bool() })
        .collect();
    ensure(
        guard.matches(HWND(main.handle as *mut _)),
        "关闭原 Pin 身份失效",
    )?;
    let request = unix_ms()?;
    report.stage("cycle-close-request",json!({"cycle":cycle,"main_hwnd":main.handle,"original_registered_hwnds":original,"native_close_request_unix_ms":request,"normal_wm_close":true}))?;
    unsafe {
        PostMessageW(
            Some(HWND(main.handle as *mut _)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        )?;
    }
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        let destroyed = original
            .iter()
            .all(|h| unsafe { !IsWindow(Some(HWND(*h as *mut _))).as_bool() });
        if destroyed && life.expired() {
            break;
        }
        ensure(
            Instant::now() < end,
            "长图原 HWND/观察到的资源 Weak 关闭未释放",
        )?;
    }
    let destroyed = unix_ms()?;
    report.stage("cycle-closed",json!({"cycle":cycle,"main_hwnd":main.handle,"original_registered_hwnds":original,"native_close_request_unix_ms":request,"all_original_hwnds_destroyed_observed_unix_ms":destroyed,"all_original_hwnds_destroyed":true,"all_observed_entity_and_render_weaks_expired":true,"render_weaks_observed":life.render.len(),"field_weaks_observed":life.fields.len(),"capture_entity_weaks_observed":life.canvases.len(),"pin_entity_weaks_observed":life.pins.len(),"normal_wm_close":true,"forced_cleanup":false}))?;
    let settled = Instant::now();
    while settled.elapsed() < Duration::from_millis(850) {
        tick(cx).await;
    }
    ensure(life.expired(), "静置后旧资源再次出现")?;
    report.stage("cycle-settled",json!({"cycle":cycle,"main_hwnd":main.handle,"closed_observed_unix_ms":destroyed,"post_close_wait_ms":settled.elapsed().as_millis(),"minimum_post_close_wait_ms":600,"hidden_keepalive":true,"capture_threads":crate::long_capture_pipeline::active_capture_threads(),"capture_workers":crate::long_capture_runtime::active_workers(),"all_observed_weaks_expired":true,"continuous_peak_proven":false}))
}
async fn cycle(
    cycle: u32,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let image = Image {
        width: WIDTH,
        height: HEIGHT,
        bgra: vec![255; WIDTH as usize * HEIGHT as usize * 4],
        readback_ms: 0.,
    };
    let host =
        Host(cx.update(|cx| PinView::open_state(image, (160, 160), PinState::default(), cx))??);
    let main = Surface::Main(host.0).input(cx)?;
    host.0.update(cx, |v, w, cx| -> Result<()> {
        v.ensure_canvas(w, cx)?;
        v.perform(PinAction::Chrome, w, cx);
        Ok(())
    })??;
    install(main, guard, stats)?;
    ready(host, cx).await?;
    pose(host, "top", 0.25, cx).await?;
    let mut life = Lifetimes::default();
    let initial = source(host, cx)?;
    ensure(
        initial["shared_with_image"] == true && initial["shared_with_canvas_source"] == true,
        "长图不是单一共享原图",
    )?;
    let workspace = workspace_idle(host, cx).await?;
    let more = button(host, host.primary(cx)?, "more-tools", guard, stats, cx).await?;
    let picker = host.0.update(cx, |v, _, cx| -> Result<Surface> {
        Ok(v.toolbar.ok_or("一级栏缺失")?.update(cx, |b, _, _| {
            b.tool_picker.map(Surface::Picker).ok_or("工具列表缺失")
        })??)
    })??;
    let watermark = button(host, picker, "watermark", guard, stats, cx).await?;
    let primary = host.primary(cx)?.input(cx)?;
    let secondary = host.properties(false, cx)?.input(cx)?;
    install(primary, guard, stats)?;
    install(secondary, guard, stats)?;
    report.stage("cycle-opened",json!({"cycle":cycle,"main_hwnd":main.handle,"primary_hwnd":primary.handle,"secondary_hwnd":secondary.handle,"original_source":initial,"workspace_probe":workspace,"more":more,"watermark":watermark,"dimensions":[WIDTH,HEIGHT],"base_pattern":"opaque-white-bgra255-v1","actual_pin_workspace":true,"actual_two_level_toolbar":true}))?;
    let first = type_text(host, &format!("水印中文🙂 第{cycle}轮"), guard, stats, cx).await?;
    // 分开首建对象与后续修改，合并处理的递归 Undo 可恢复真实原对象。
    text_contract_smoke::key(secondary, VK_RETURN, false, false, false)?;
    settle(cx).await;
    let started = unix_ms()?;
    let mut edits = Vec::new();
    for value in [
        format!("水印中文🙂 长图{cycle} A"),
        format!("水印中文🙂 长图{cycle} B"),
    ] {
        edits.push(type_text(host, &value, guard, stats, cx).await?);
        ensure(source(host, cx)? == initial, "文字修改复制或替换了原底图")?;
        life.observe(host, cx)?;
    }
    let font = parameter(host, "font-menu", "font40", guard, stats, cx).await?;
    let alpha = parameter(host, "opacity-menu", "alpha65", guard, stats, cx).await?;
    ensure(source(host, cx)? == initial, "水印字号/透明修改了原底图")?;
    let cache = life.observe(host, cx)?;
    report.stage("actual-continuous-watermark-edit",json!({"cycle":cycle,"started_unix_ms":started,"first_create":first,"edits":edits,"font":font,"opacity":alpha,"source_identity_unchanged":true,"actual_source":source(host,cx)?,"cache":cache,"no_full_cpu_copy_in_input_adapter":true}))?;
    if cycle == 2 {
        seal_undo(host, cycle, &mut life, report, cx).await?;
    }
    let final_text = format!("水印中文🙂 长图第{cycle}轮 最终");
    let final_edit = type_text(host, &final_text, guard, stats, cx).await?;
    parameter(host, "font-menu", "font40", guard, stats, cx).await?;
    parameter(host, "opacity-menu", "alpha65", guard, stats, cx).await?;
    ensure(source(host, cx)? == initial, "最终原底图身份未恢复")?;
    for (band, zoom) in [("top", 0.25), ("middle", 0.375), ("bottom", 0.5)] {
        let pose = pose(host, band, zoom, cx).await?;
        let pair = gpu_pair(host, cycle, band, &report.root, guard, stats, &mut life, cx).await?;
        ensure(source(host, cx)? == initial, "平移/缩放复制或替换底图")?;
        report.stage(
            "actual-three-position-watermark-gpu",
            json!({"cycle":cycle,"pose":pose,"pair":pair,"source_identity_unchanged":true}),
        )?;
    }
    let metadata=host.canvas(cx,|c,_,_|->Result<Value>{
        let object=c.editor.document.items().iter().find(|o|o.annotation.tool==Tool::Watermark).ok_or("最终水印对象缺失")?;
        ensure(object.annotation.text==final_text,"GPU清空/撤销后文案没恢复")?;
        let a=&object.annotation;
        let points:Vec<_>=a.points.iter().map(|p|json!({"x":p.x,"y":p.y})).collect();
        Ok(json!({"object_id":object.id,"text":a.text,"style":{"font_size":a.style.font_size,"opacity":a.style.opacity,"color":a.style.color,"bold":a.style.bold,"outline_width":a.style.text.outline_width},"rotation":a.rotation,"points":points,"allowed_change_rect":{"x":0,"y":0,"width":WIDTH,"height":HEIGHT},"full_canvas_bound_only":true,"precise_glyph_region_verified":false}))
    })??;
    // 原尺寸保存共用 Pin 工作区/合并的正式输出；用户缩放导出另遵循 state.zoom。
    let export_started = unix_ms()?;
    let output = host.0.update(cx, |v, _, cx| v.output(cx))??;
    ensure(
        (output.width, output.height) == (WIDTH, HEIGHT),
        "正式原尺寸输出没有保留 32k 尺寸",
    )?;
    let path = report.root.join(format!("cycle-{cycle}-export.png"));
    let export = cx
        .background_executor()
        .spawn(async move { png(output, &path).map_err(|e| e.to_string()) })
        .await
        .map_err(|e| e.to_string())?;
    let export_finished = unix_ms()?;
    let user_projection=host.0.update(cx,|v,_,cx|v.user_export_snapshot(cx).map(|s|json!({"width":s.projection.width,"height":s.projection.height,"state_zoom":v.state.zoom,"original_size_path_used":false})))??;
    host.0.update(cx, |v, _, _| -> Result<()> {
        ensure(
            v.original
                .as_bytes(0)
                .ok_or("原图缺失")?
                .iter()
                .all(|p| *p == 255),
            "原图完整白色像素被改写",
        )
    })??;
    report.stage("actual-original-size-watermark-export",json!({"cycle":cycle,"export":export,"export_started_unix_ms":export_started,"export_finished_unix_ms":export_finished,"export_route":"PinView.output -> CaptureView.selected_image -> export.save_png","user_export_snapshot_actual_projection":user_projection,"watermark":metadata,"final_input":final_edit,"original_source":source(host,cx)?,"original_all_bytes_unchanged":true,"original_pixels":WIDTH as u64*HEIGHT as u64,"full_png_decode_in_runtime":false,"final_png_glyph_oracle":false}))?;
    close_cycle(host, cycle, guard, &life, report, cx).await
}
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_WATERMARK_LONG_QA_DIR")
        .map(PathBuf::from)
        .ok_or("长图水印独立 artifact 目录缺失")?;
    std::fs::create_dir_all(root.join("settings"))?;
    let root = root.canonicalize()?;
    let allowed = std::env::current_dir()?.join("target/qa").canonicalize()?;
    ensure(
        root.starts_with(&allowed) && root != allowed,
        "长图水印 artifact 不在独立 target/qa 子目录",
    )?;
    let settings = root.join("settings").canonicalize()?;
    if let Some(existing) = std::env::var_os("RUSTSHOT_DATA_DIR") {
        ensure(
            PathBuf::from(existing).canonicalize()? == settings,
            "长图水印 data_dir 必须精确匹配 artifact/settings",
        )?;
    }
    std::env::set_var("RUSTSHOT_DATA_DIR", &settings);
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    std::fs::write(settings.join("tool-presets.json"), "{}\n")?;
    let report = Report {
        file: OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("stages.jsonl"))?,
        root,
        stages: Vec::new(),
    };
    let holder = Rc::new(RefCell::new(Some(report)));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("长图水印专项未完成".to_owned())));
    let final_error = error.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move|cx:&mut App|{
        let keepalive=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},|_,cx|cx.new(|_|KeepAlive)){Ok(v)=>v,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        cx.spawn(async move|cx|{
            let mut report=holder.borrow_mut().take().expect("独占长图水印报告");
            let stats=Rc::new(AdapterStats::default());let mut completed=0u32;
            let guard=OutputGuard::new();
            let result:Result<()>=async{
                let guard=guard.as_ref().map_err(|e|e.to_string())?;
                report.stage("cold-own-application",json!({"pid":std::process::id(),"data_dir":crate::preferences::data_dir(),"dimensions":[WIDTH,HEIGHT],"rounds":3,"hidden_keepalive":true,"fixture":"opaque-white-bgra255-v1","desktop_capture_called":false}))?;
                for n in 1..=3{cycle(n,guard,&stats,&mut report,cx).await?;completed+=1;}
                ensure(guard.attempts()==0&&stats.keys.get()==stats.restored.get()&&stats.errors.get()==0,"输出守卫/键盘恢复失败")?;
                Ok(())
            }.await;
            let mut failure=result.err().map(|e|e.to_string());
            if let Some(message)=&failure{let _=report.stage("actual-failure",json!({"error":message,"completed_cycles":completed}));}
            // 出错仍正常关闭剩余自有窗口，不以强杀制造释放成功。
            if failure.is_some(){
                if let Ok(windows)=cx.update(|cx|cx.windows()){
                    for window in windows{
                        if let Some(pin)=window.downcast::<PinView>(){if let Ok(input)=Surface::Main(pin).input(cx){unsafe{let _=PostMessageW(Some(HWND(input.handle as *mut _)),WM_CLOSE,WPARAM(0),LPARAM(0));}}}
                    }
                }
                for _ in 0..80{tick(cx).await;}
            }
            let attempts=guard.as_ref().ok().map_or_else(Vec::new,|g|g.0.attempts.borrow().clone());
            if let Err(e)=report.stage("summary",json!({"suite":"watermark-long-memory-native","requested_groups":[GROUP],"completed_cycles":completed,"rounds":3,"all_three_cycles_verified":completed==3,"width":WIDTH,"height":HEIGHT,"raw_bytes":WIDTH as u64*HEIGHT as u64*4,"output_guard_attempts":attempts,"keyboard_adapter":{"keys":stats.keys.get(),"restored":stats.restored.get(),"errors":stats.errors.get()},"desktop_read":false,"clipboard_read":false,"clipboard_written":false,"global_pointer_moved":false,"global_input_injected":false,"full_png_decode_in_runtime":false,"continuous_peak_proven":false,"atlas_residency_directly_queried":false,"all_gpu_or_com_resources_verified":false,"broader_goal_complete":false,"error":failure})){failure=Some(e.to_string());}
            *error.borrow_mut()=failure;*holder.borrow_mut()=Some(report);drop(guard);
            let _=keepalive.update(cx,|_,w,_|w.remove_window());let _=cx.update(|cx|cx.quit());
        }).detach();
    });
    let failure = final_error.borrow_mut().take();
    if let Some(mut report) = final_holder.borrow_mut().take() {
        report.stage("after-application",json!({"capture_workers":crate::long_capture_runtime::active_workers(),"capture_threads":crate::long_capture_pipeline::active_capture_threads(),"application_returned_normally":true,"forced_process_termination":false}))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(report.root.join("report.json"))?;
        serde_json::to_writer_pretty(
            &mut file,
            &json!({"suite":"watermark-long-memory-native","ok":failure.is_none(),"error":failure,"pid":std::process::id(),"requested_groups":[GROUP],"rounds":3,"width":WIDTH,"height":HEIGHT,"stages":report.stages,"application_returned_normally":true,"forced_cleanup":false,"continuous_peak_proven":false}),
        )?;
        file.flush()?;
        println!(
            "{}",
            json!({"phase":"watermark_long_memory_smoke_complete","pid":std::process::id(),"ok":failure.is_none(),"report":report.root.join("report.json")})
        );
    }
    if let Some(e) = failure {
        return Err(e.into());
    }
    Ok(())
}
