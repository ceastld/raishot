//! 自有图片编辑器与贴图两级水印副栏：真实原生输入，不采集桌面或改写剪贴板。
use crate::{
    annotation::Tool,
    capture::Image,
    capture_view::{Action, CaptureView},
    document::Object,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    pin_model::PinState,
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
    div, prelude::*, App, AppContext, Application, AsyncApp, Context, EntityInputHandler, Render,
    RenderImage, WeakEntity, Window, WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const WIDTH: u32 = 720;
const HEIGHT: u32 = 420;
const PREVIEW_PIXEL_LIMIT: u64 = 8_000_000;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(50))
        .await;
}
async fn settle(cx: &mut AsyncApp) {
    tick(cx).await;
    tick(cx).await;
}

struct OwnedWindow {
    handle: usize,
    process: u32,
    thread: u32,
}
struct GuardState {
    armed: Cell<bool>,
    windows: RefCell<Vec<OwnedWindow>>,
    attempts: RefCell<Vec<Value>>,
    token: Rc<()>,
    artifact: PathBuf,
}
thread_local! {
    static OUTPUT_GUARD: RefCell<Option<Rc<GuardState>>> = const { RefCell::new(None) };
}
/// 原窗口属性随 DestroyWindow 消失；句柄复用不能继承本次 QA 输出权限。
struct OutputGuard(Rc<GuardState>);
impl OutputGuard {
    fn new(artifact: PathBuf) -> Result<Self> {
        let state = Rc::new(GuardState {
            armed: Cell::new(true),
            windows: RefCell::new(Vec::new()),
            attempts: RefCell::new(Vec::new()),
            token: Rc::new(()),
            artifact,
        });
        OUTPUT_GUARD.with(|slot| -> Result<()> {
            ensure(slot.borrow().is_none(), "水印输出守卫不能嵌套")?;
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
            thread != 0 && process == std::process::id(),
            "水印守卫只登记原自有 HWND",
        )?;
        let token = Rc::as_ptr(&self.0.token) as usize;
        unsafe {
            SetPropW(
                h,
                w!("RustShot.WatermarkToolbarSmoke.OutputGuard"),
                Some(HANDLE(token as *mut _)),
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
    fn attempts(&self) -> usize {
        self.0.attempts.borrow().len()
    }
    fn disarm(&self) {
        self.0.armed.set(false);
    }
}
impl Drop for OutputGuard {
    fn drop(&mut self) {
        self.disarm();
        OUTPUT_GUARD.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|s| Rc::ptr_eq(s, &self.0)) {
                *slot = None;
            }
        });
        for owned in self.0.windows.borrow().iter() {
            unsafe {
                let h = HWND(owned.handle as *mut _);
                if std::ptr::eq(
                    GetPropW(h, w!("RustShot.WatermarkToolbarSmoke.OutputGuard"))
                        .0
                        .cast::<()>(),
                    Rc::as_ptr(&self.0.token),
                ) {
                    let _ = RemovePropW(h, w!("RustShot.WatermarkToolbarSmoke.OutputGuard"));
                }
            }
        }
    }
}
fn guard_output(window: &Window, domain: &'static str, action: &'static str) -> bool {
    OUTPUT_GUARD.with(|slot| {
        let state = slot.borrow();
        let Some(state) = state.as_ref().filter(|s| s.armed.get()) else {
            return false;
        };
        let h = hwnd(window);
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut process)) };
        let exact = state
            .windows
            .borrow()
            .iter()
            .any(|o| o.handle == h.0 as usize && o.process == process && o.thread == thread);
        let original = unsafe {
            std::ptr::eq(
                GetPropW(h, w!("RustShot.WatermarkToolbarSmoke.OutputGuard"))
                    .0
                    .cast::<()>(),
                Rc::as_ptr(&state.token),
            )
        };
        if !exact || !original {
            return false;
        }
        state
            .attempts
            .borrow_mut()
            .push(json!({"unix_ms":unix_ms().ok(),
            "domain":domain,"action":action,"hwnd":h.0 as usize,
            "process":process,"thread":thread,"artifact":state.artifact,
            "blocked_before_side_effect":true}));
        true
    })
}
/// 默认没有本次 QA 守卫时恒 false；不按 PID/类名放行或拦截其它窗口。
pub(crate) fn guard_capture_output(action: &Action, window: &Window) -> bool {
    let name = match action {
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
    guard_output(window, "CaptureView", name)
}
pub(crate) fn guard_pin_output(action: &PinAction, window: &Window) -> bool {
    let name = match action {
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
    guard_output(window, "PinView", name)
}

struct Report {
    root: PathBuf,
    file: File,
    stages: Vec<Value>,
}
impl Report {
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        let value = json!({"name":name,"unix_ms":unix_ms()?,"detail":detail});
        serde_json::to_writer(&mut self.file, &value)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.stages.push(value);
        Ok(())
    }
}
fn fixture() -> Image {
    Image {
        width: WIDTH,
        height: HEIGHT,
        bgra: vec![255; (WIDTH * HEIGHT * 4) as usize],
        readback_ms: 0.,
    }
}
#[derive(Clone, Copy)]
enum Host {
    Image(WindowHandle<CaptureView>),
    Pin(WindowHandle<PinView>),
}
#[derive(Clone, Copy)]
enum Surface {
    Image(WindowHandle<CaptureView>),
    Primary(WindowHandle<PinToolbar>),
    Properties(WindowHandle<PinPropertyBar>),
    Popup(WindowHandle<PinPropertyPopup>),
    Picker(WindowHandle<PinToolPicker>),
}
impl Surface {
    fn input(self, cx: &mut AsyncApp) -> Result<Input> {
        let get = |w: &Window| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        };
        Ok(match self {
            Self::Image(h) => h.update(cx, |_, w, _| get(w))?,
            Self::Primary(h) => h.update(cx, |_, w, _| get(w))?,
            Self::Properties(h) => h.update(cx, |_, w, _| get(w))?,
            Self::Popup(h) => h.update(cx, |_, w, _| get(w))?,
            Self::Picker(h) => h.update(cx, |_, w, _| get(w))?,
        })
    }
}
impl Host {
    fn name(self) -> &'static str {
        match self {
            Self::Image(_) => "image_editor",
            Self::Pin(_) => "pin",
        }
    }
    fn update<T>(
        self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut CaptureView, &mut Window, &mut Context<CaptureView>) -> T,
    ) -> Result<T> {
        Ok(match self {
            Self::Image(h) => h.update(cx, f)?,
            Self::Pin(h) => h.update(cx, |v, w, cx| {
                v.canvas
                    .as_ref()
                    .expect("自有水印 Pin canvas")
                    .update(cx, |c, cx| f(c, w, cx))
            })?,
        })
    }
    fn main(self, cx: &mut AsyncApp) -> Result<Input> {
        self.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
    }
    fn primary(self, cx: &mut AsyncApp) -> Result<Surface> {
        match self {
            Self::Image(h) => Ok(Surface::Image(h)),
            Self::Pin(h) => h
                .update(cx, |v, _, _| {
                    v.toolbar.map(Surface::Primary).ok_or("自有 Pin 一级栏缺失")
                })?
                .map_err(Into::into),
        }
    }
    fn properties(self, popup: bool, cx: &mut AsyncApp) -> Result<Surface> {
        match self {
            Self::Image(h) => Ok(Surface::Image(h)),
            Self::Pin(h) => h.update(cx, |v, _, cx| -> Result<Surface> {
                v.toolbar
                    .ok_or("Pin 一级栏缺失")?
                    .update(cx, |b, _, cx| -> Result<Surface> {
                        if popup {
                            Ok(Surface::Popup(b.popup(cx).ok_or("Pin 数值弹窗缺失")?))
                        } else {
                            Ok(Surface::Properties(b.properties.ok_or("Pin 二级栏缺失")?))
                        }
                    })?
            })?,
        }
    }
    fn field<T>(
        self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut TextField, &mut Window, &mut Context<TextField>) -> T,
    ) -> Result<T> {
        let field = self.update(cx, |c, _, _| {
            c.watermark_field.clone().ok_or("水印输入框缺失")
        })??;
        match self.properties(false, cx)? {
            Surface::Image(h) => {
                Ok(h.update(cx, |_, w, cx| field.update(cx, |v, cx| f(v, w, cx)))?)
            }
            Surface::Properties(h) => {
                Ok(h.update(cx, |_, w, cx| field.update(cx, |v, cx| f(v, w, cx)))?)
            }
            _ => Err("水印输入不在二级栏".into()),
        }
    }
    fn positions(
        self,
        surface: Surface,
        cx: &mut AsyncApp,
        id: &'static str,
    ) -> Result<Option<(f32, f32)>> {
        match surface {
            Surface::Primary(h) => {
                Ok(h.update(cx, |b, _, _| b.positions.borrow().get(id).copied())?)
            }
            Surface::Picker(h) => {
                Ok(h.update(cx, |b, _, _| b.positions.borrow().get(id).copied())?)
            }
            _ => self.update(cx, |c, _, _| c.ui_positions.borrow().get(id).copied()),
        }
    }
}
fn chars(input: Input, value: &str) -> Result<()> {
    for unit in value.encode_utf16() {
        unsafe {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_CHAR,
                WPARAM(unit as usize),
                LPARAM(0),
            )?;
        }
    }
    Ok(())
}
fn install(input: Input, guard: &OutputGuard, stats: &Rc<AdapterStats>) -> Result<()> {
    guard.register(input)?;
    text_contract_smoke::install(input, stats)
}
async fn button(
    host: Host,
    surface: Surface,
    id: &'static str,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let mut point = None;
    for _ in 0..40 {
        point = host.positions(surface, cx, id)?;
        if point.is_some() {
            break;
        }
        tick(cx).await;
    }
    let point = point.ok_or_else(|| format!("实际水印控件未绘制：{id}"))?;
    let input = surface.input(cx)?;
    install(input, guard, stats)?;
    input.click(point);
    settle(cx).await;
    Ok(
        json!({"id":id,"hwnd":input.handle,"logical_point":[point.0,point.1],"dpi":input.scale,
        "posted_own_wndproc":true}),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawIdentity {
    image_id: usize,
    pointer: usize,
    length: usize,
    width: u32,
    height: u32,
}
fn raw(host: Host, cx: &mut AsyncApp) -> Result<RawIdentity> {
    host.update(cx, |c, _, _| -> Result<RawIdentity> {
        let preview = c.previews.first().ok_or("底图预览缺失")?;
        let image = c.source.original(preview);
        let bytes = image.as_bytes(0).ok_or("原始底图字节缺失")?;
        let full = c.source.full();
        ensure(
            bytes.len() == (WIDTH * HEIGHT * 4) as usize && bytes.iter().all(|b| *b == 255),
            "合成底图真实像素被改写",
        )?;
        Ok(RawIdentity {
            image_id: image.id.0,
            pointer: bytes.as_ptr() as usize,
            length: bytes.len(),
            width: full.width,
            height: full.height,
        })
    })?
}
fn items(host: Host, cx: &mut AsyncApp) -> Result<Vec<Object>> {
    host.update(cx, |c, _, _| c.editor.document.items().to_vec())
}
fn watermark(host: Host, cx: &mut AsyncApp) -> Result<(u64, usize, String)> {
    host.update(cx, |c, _, _| -> Result<_> {
        let id = c.editor.watermark.target().ok_or("托管水印没有原对象 ID")?;
        let (index, object) = c
            .editor
            .document
            .items()
            .iter()
            .enumerate()
            .find(|(_, o)| o.id == id)
            .ok_or("原托管水印对象缺失")?;
        ensure(
            object.annotation.tool == Tool::Watermark && c.editor.text_draft.is_none(),
            "水印副栏误用普通文字草稿",
        )?;
        Ok((id, index, object.annotation.text.clone()))
    })?
}
fn focus(host: Host, cx: &mut AsyncApp) -> Result<Value> {
    let input = host.properties(false, cx)?.input(cx)?;
    let (logical, composing, value, committed, native) = host.field(cx, |v, w, _| {
        (
            v.focus_handle.is_focused(w),
            v.is_composing(),
            v.text().to_owned(),
            v.committed_text().to_owned(),
            unsafe { GetFocus().0 as usize },
        )
    })?;
    let routed = host.update(cx, |c, _, _| c.watermark_input_focused)?;
    Ok(json!({"field_hwnd":input.handle,"native_focus_hwnd":native,
        "native_focus_matches":native == input.handle,"gpui_focus":logical,
        "production_input_focused":routed,"composing":composing,
        "display_text":value,"committed_text":committed}))
}
fn require_focus(host: Host, cx: &mut AsyncApp) -> Result<Value> {
    let result = focus(host, cx)?;
    ensure(
        result["native_focus_matches"] == true
            && result["gpui_focus"] == true
            && result["production_input_focused"] == true,
        &format!("真实 native/GPUI 水印输入焦点不一致：{result}"),
    )?;
    Ok(result)
}

struct Lifetimes {
    render: Vec<Weak<RenderImage>>,
    fields: Vec<WeakEntity<TextField>>,
    captures: Vec<WeakEntity<CaptureView>>,
    pins: Vec<WeakEntity<PinView>>,
}
impl Lifetimes {
    fn observe(&mut self, host: Host, cx: &mut AsyncApp) -> Result<Value> {
        host.update(cx, |c, _, cx| -> Result<Value> {
            self.captures.push(cx.entity().downgrade());
            if let Some(field) = &c.watermark_field { self.fields.push(field.downgrade()); }
            for preview in &c.previews { self.render.push(Arc::downgrade(preview)); }
            let scene = c.prepared_scene();
            let metadata = scene.layers.iter().filter(|layer|
                matches!(layer, crate::scene::Layer::Watermark { .. })).count();
            let total: u64 = c.watermark_previews.iter().map(|p|
                u64::from(p.dimensions.0) * u64::from(p.dimensions.1)).sum();
            for preview in &c.watermark_previews { self.render.push(Arc::downgrade(&preview.image)); }
            ensure(total <= PREVIEW_PIXEL_LIMIT, "水印显示缓存超过总像素预算")?;
            ensure(scene.errors.is_empty(), "水印 Scene 存在实际绘制错误")?;
            Ok(json!({"watermark_metadata_layers":metadata,"watermark_preview_count":c.watermark_previews.len(),
                "watermark_preview_pixels":total,"watermark_preview_pixel_limit":PREVIEW_PIXEL_LIMIT,
                "scene_errors":scene.errors,"cache_scope":"observed small synthetic viewport; large source not tested"}))
        })?
    }
    fn expired(&self) -> bool {
        self.render.iter().all(|w| w.upgrade().is_none())
            && self.fields.iter().all(|w| w.upgrade().is_none())
            && self.captures.iter().all(|w| w.upgrade().is_none())
            && self.pins.iter().all(|w| w.upgrade().is_none())
    }
}

async fn select_watermark(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let primary = host.primary(cx)?;
    let more = button(
        host,
        primary,
        if matches!(host, Host::Pin(_)) {
            "more-tools"
        } else {
            "more"
        },
        guard,
        stats,
        cx,
    )
    .await?;
    let surface = match host {
        Host::Image(h) => Surface::Image(h),
        Host::Pin(h) => h.update(cx, |v, _, cx| -> Result<Surface> {
            Ok(v.toolbar
                .ok_or("Pin 原一级栏缺失")?
                .update(cx, |b, _, _| {
                    b.tool_picker
                        .map(Surface::Picker)
                        .ok_or("Pin 更多绘制工具窗缺失")
                })??)
        })??,
    };
    let choice = button(host, surface, "watermark", guard, stats, cx).await?;
    ensure(
        host.update(cx, |c, _, _| {
            c.editor.tool == Tool::Watermark && c.editor.text_draft.is_none()
        })?,
        "更多工具的实际点击没有进入水印副栏",
    )?;
    report.stage(
        "real-watermark-tool-selection",
        json!({"host":host.name(),"more":more,"choice":choice,
        "tool":"Watermark","ordinary_text_draft":false}),
    )
}
async fn above_layout(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let detail = match host {
        Host::Image(_) => {
            let primary = host.primary(cx)?;
            let grip = host
                .positions(primary, cx, "toolbar-grip")?
                .ok_or("图片编辑实际拖拽把手未绘制")?;
            let input = primary.input(cx)?;
            install(input, guard, stats)?;
            let a = (
                (grip.0 * input.scale).round() as u32,
                (grip.1 * input.scale).round() as u32,
            );
            let b = (a.0, (210. * input.scale).round() as u32);
            input.drag(a, b);
            settle(cx).await;
            host.update(cx, |c, w, _| -> Result<Value> {
                let layout = c.toolbar_layout(c.source.full(), w.scale_factor());
                ensure(layout.properties_above && layout.properties.bottom() <= layout.primary.y,
                    "真实拖拽后副栏没有在一级栏上方")?;
                let field = c.ui_positions.borrow().get("watermark-field").copied().ok_or("上方水印输入没有绘制")?;
                let clear = c.ui_positions.borrow().get("watermark-clear").copied().ok_or("上方清空按钮没有绘制")?;
                ensure((field.1 - clear.1).abs() <= 2., "水印输入与清空被换行拆开")?;
                Ok(json!({"actual_grip_drag":{"from_physical":a,"to_physical":b},
                    "properties_above":layout.properties_above,
                    "primary":[layout.primary.x,layout.primary.y,layout.primary.width,layout.primary.height],
                    "properties":[layout.properties.x,layout.properties.y,layout.properties.width,layout.properties.height],
                    "field_point":field,"clear_point":clear,"popup_direction":"production layout"}))
            })??
        }
        Host::Pin(_) => {
            let primary = host.primary(cx)?.input(cx)?;
            let secondary = host.properties(false, cx)?.input(cx)?;
            install(primary, guard, stats)?;
            install(secondary, guard, stats)?;
            let p = crate::pin_native::rect(HWND(primary.handle as *mut _))?;
            let s = crate::pin_native::rect(HWND(secondary.handle as *mut _))?;
            ensure(
                s.y as i64 + i64::from(s.height) <= i64::from(p.y),
                "底部自有贴图没有采用上方二级浮栏",
            )?;
            let properties = host.properties(false, cx)?;
            let field = host
                .positions(properties, cx, "watermark-field")?
                .ok_or("Pin 水印输入未绘制")?;
            let clear = host
                .update(cx, |c, _, _| {
                    c.ui_positions.borrow().get("watermark-clear").copied()
                })?
                .ok_or("Pin 清空未绘制")?;
            ensure((field.1 - clear.1).abs() <= 2., "Pin 水印输入与清空被拆开")?;
            json!({"primary_hwnd":primary.handle,"secondary_hwnd":secondary.handle,
                "primary":[p.x,p.y,p.width,p.height],"properties":[s.x,s.y,s.width,s.height],
                "properties_above":true,"field_point":field,"clear_point":clear})
        }
    };
    report.stage(
        "actual-above-two-level-layout",
        json!({"host":host.name(),"geometry":detail}),
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
    let action = button(host, surface, "watermark-field", guard, stats, cx).await?;
    let focus = require_focus(host, cx)?;
    Ok(json!({"click":action,"focus":focus,"mouse_activation":activation}))
}
/// PostMessage 点击没有 OS 的鼠标激活；仅补自有窗口激活，不指定控件 FocusHandle。
fn mouse_activation(input: Input, guard: &OutputGuard) -> Result<Value> {
    let h = HWND(input.handle as *mut _);
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut process)) };
    let root = unsafe { GetAncestor(h, GA_ROOTOWNER) };
    let original = || unsafe {
        let mut current = 0;
        let mut root_process = 0;
        IsWindow(Some(h)).as_bool()
            && IsWindow(Some(root)).as_bool()
            && GetWindowThreadProcessId(h, Some(&mut current)) == thread
            && current == process
            && GetWindowThreadProcessId(root, Some(&mut root_process)) == thread
            && root_process == process
            && GetAncestor(h, GA_ROOTOWNER) == root
            && GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) == root
            && std::ptr::eq(
                GetPropW(h, w!("RustShot.WatermarkToolbarSmoke.OutputGuard"))
                    .0
                    .cast::<()>(),
                Rc::as_ptr(&guard.0.token),
            )
            && std::ptr::eq(
                GetPropW(root, w!("RustShot.WatermarkToolbarSmoke.OutputGuard"))
                    .0
                    .cast::<()>(),
                Rc::as_ptr(&guard.0.token),
            )
    };
    ensure(
        process == std::process::id()
            && thread == unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
            && guard
                .0
                .windows
                .borrow()
                .iter()
                .any(|w| w.handle == input.handle && w.process == process && w.thread == thread)
            && original(),
        "鼠标激活只能作用于原自有前台窗口组",
    )?;
    ensure(
        unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize == 0,
        "水印输入窗口仍有 NOACTIVATE",
    )?;
    let before_focus = unsafe { GetFocus().0 as usize };
    let before_active = unsafe { GetActiveWindow().0 as usize };
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
        "自有窗口未接受鼠标激活",
    )?;
    unsafe {
        let _ = SetActiveWindow(h);
    }
    ensure(original(), "激活后原窗口身份变化")?;
    unsafe {
        let _ = SetFocus(Some(h));
    }
    ensure(
        original() && unsafe { GetFocus() } == h,
        "鼠标激活未聚焦原输入窗口",
    )?;
    Ok(
        json!({"native_mouse_activation_simulated":true,"hardware_activation_tested":false,
        "gpui_focus_overridden":false,"before_focus_hwnd":before_focus,
        "after_focus_hwnd":h.0 as usize,"activation_result":result.0,"original_owned_root":root.0 as usize,
        "before_active_hwnd":before_active,"after_active_hwnd":unsafe { GetActiveWindow().0 as usize }}),
    )
}
async fn press(
    host: Host,
    vk: VIRTUAL_KEY,
    control: bool,
    held: bool,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let input = host.properties(false, cx)?.input(cx)?;
    install(input, guard, stats)?;
    text_contract_smoke::key(input, vk, control, false, held)?;
    settle(cx).await;
    ensure(
        guard.attempts() == 0,
        "水印按键透传到宿主输出/关闭；已由精确自有窗口守卫阻止",
    )
}
fn host_live(host: Host, cx: &mut AsyncApp) -> Result<Value> {
    let main = host.main(cx)?;
    let primary = host.primary(cx)?.input(cx)?;
    let properties = host.properties(false, cx)?.input(cx)?;
    let alive = |h| unsafe { IsWindow(Some(HWND(h as *mut _))).as_bool() };
    let busy = host.update(cx, |c, _, _| c.saving || c.processing)?;
    let chrome = match host {
        Host::Image(_) => true,
        Host::Pin(h) => h.update(cx, |v, _, _| v.chrome && !v.saving && !v.processing)?,
    };
    ensure(
        alive(main.handle) && alive(primary.handle) && alive(properties.handle) && !busy && chrome,
        "水印输入触发了关闭、收栏或输出",
    )?;
    Ok(
        json!({"main_hwnd":main.handle,"primary_hwnd":primary.handle,"secondary_hwnd":properties.handle,
        "all_original_windows_live":true,"saving_or_processing":busy,"pin_chrome_visible":chrome}),
    )
}
async fn frame(host: Host, cx: &mut AsyncApp) -> Result<Image> {
    let roi = host.update(cx, |c, w, _| -> Result<[u32; 4]> {
        let viewport = c.canvas_viewport(w);
        let p = viewport.window_point(crate::annotation::Point { x: 80., y: 230. });
        let width = (500. * viewport.zoom).round() as u32;
        let height = (140. * viewport.zoom).round() as u32;
        let client = w.viewport_size();
        let actual = [
            (f32::from(client.width) * w.scale_factor()).round() as u32,
            (f32::from(client.height) * w.scale_factor()).round() as u32,
        ];
        ensure(
            p.x >= 0. && p.y >= 0. && width > 0 && height > 0,
            "水印 GPU 观测区域无效",
        )?;
        let roi = [p.x.round() as u32, p.y.round() as u32, width, height];
        ensure(
            roi[0] + width <= actual[0] && roi[1] + height <= actual[1],
            "水印 GPU 观测超出自有客户区",
        )?;
        w.request_frame_capture(roi)?;
        Ok(roi)
    })??;
    for _ in 0..80 {
        tick(cx).await;
        if let Some(result) = host.update(cx, |_, w, _| w.take_frame_capture())? {
            let bgra = result.map_err(|e| e.to_string())?;
            ensure(
                bgra.len() == roi[2] as usize * roi[3] as usize * 4,
                "水印自有 GPU 字节数错误",
            )?;
            return Ok(Image {
                width: roi[2],
                height: roi[3],
                bgra,
                readback_ms: 0.,
            });
        }
    }
    Err("水印自有 GPU frame capture 超时".into())
}
async fn parameters(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let target = watermark(host, cx)?;
    let mut details = Vec::new();
    for (menu, panel, preset, value) in [
        ("font-menu", ToolbarPanel::Font, "font40", 40.),
        ("opacity-menu", ToolbarPanel::Opacity, "alpha65", 0.65),
    ] {
        let open = button(host, host.properties(false, cx)?, menu, guard, stats, cx).await?;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == panel)?,
            "实际水印参数页没有打开",
        )?;
        let popup = host.properties(true, cx)?;
        let choice = button(host, popup, preset, guard, stats, cx).await?;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::None)?,
            "实际水印参数页没有正常返回",
        )?;
        let current = watermark(host, cx)?;
        ensure(current == target, "参数编辑修改了水印 ID/序位或文字")?;
        let actual = host.update(cx, |c, _, _| {
            let a = c.editor.document.get(target.0).expect("水印目标存在");
            if panel == ToolbarPanel::Font {
                a.style.font_size
            } else {
                a.style.opacity
            }
        })?;
        ensure(
            (actual - value).abs() < 0.0001,
            "水印实际参数值没有应用到原对象",
        )?;
        let focus = require_focus(host, cx)?;
        details.push(json!({"menu":menu,"open":open,"choice":choice,
            "commit_kind":"quick-preset-commits-and-closes","explicit_apply_clicked":false,
            "actual_value":actual,"original_target_id":target.0,"original_index":target.1,"returned_focus":focus}));
    }
    // 参数页返回后直接走 WM_CHAR，不由 QA 手工设置逻辑焦点。
    press(host, VK_END, false, false, guard, stats, cx).await?;
    chars(host.properties(false, cx)?.input(cx)?, "续")?;
    settle(cx).await;
    ensure(
        watermark(host, cx)?.2 == format!("{}续", target.2),
        "参数返回后原生输入没有回到水印字段",
    )?;
    report.stage(
        "actual-font-opacity-and-focus-return",
        json!({"host":host.name(),"parameters":details,
        "native_character_after_return":"续","focus_overridden_by_qa":false}),
    )
}
async fn ime(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    field_click(host, guard, stats, cx).await?;
    press(host, VIRTUAL_KEY(0x41), true, false, guard, stats, cx).await?;
    let before = items(host, cx)?;
    let candidate = "候选中文🙂";
    let utf = candidate.encode_utf16().count();
    host.field(cx, |v, w, cx| -> Result<()> {
        v.replace_and_mark_text_in_range(None, candidate, Some(utf..utf), w, cx);
        let marked = v
            .marked_text_range(w, cx)
            .ok_or("水印 controlled adapter 没有 marked range")?;
        ensure(
            marked.end - marked.start == utf && v.is_composing(),
            "水印 marked UTF16 长度错误",
        )
    })??;
    settle(cx).await;
    ensure(items(host, cx)? == before, "IME 预编辑写入了可撤销文档")?;
    let composing = require_focus(host, cx)?;
    for held in [false, true] {
        for vk in [VK_RETURN, VK_ESCAPE] {
            press(host, vk, false, held, guard, stats, cx).await?;
            ensure(
                items(host, cx)? == before && host.field(cx, |v, _, _| v.is_composing())?,
                "组合态终止键破坏水印 marked 或已提交内容",
            )?;
            host_live(host, cx)?;
        }
    }
    host.field(cx, |v, _, cx| v.cancel_watermark_composition(cx))?;
    settle(cx).await;
    ensure(
        items(host, cx)? == before && !host.field(cx, |v, _, _| v.is_composing())?,
        "水印 controlled adapter 取消没有恢复原提交内容",
    )?;
    press(host, VIRTUAL_KEY(0x41), true, false, guard, stats, cx).await?;
    let confirmed = "确认中文🙂 M";
    host.field(cx, |v, w, cx| {
        v.replace_and_mark_text_in_range(None, candidate, None, w, cx);
        v.replace_text_in_range(None, confirmed, w, cx);
    })?;
    settle(cx).await;
    ensure(
        watermark(host, cx)?.2 == confirmed && !host.field(cx, |v, _, _| v.is_composing())?,
        "水印生产 IME 确认没有提交和清除 marked",
    )?;
    report.stage("watermark-ime-controlled-adapter",json!({"host":host.name(),"before_composing_focus":composing,
        "candidate":candidate,"candidate_utf16_length":utf,"confirmed":confirmed,
        "document_unchanged_during_preedit_and_cancel":true,"native_termination_keys_during_marked":4,
        "production_entity_input_handler":true,"real_ime_candidate_ui_tested":false}))
}
async fn terminal(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut details = Vec::new();
    for (name, key) in [("Enter", VK_RETURN), ("Escape", VK_ESCAPE)] {
        let clicked = field_click(host, guard, stats, cx).await?;
        let before = items(host, cx)?;
        let input = host.properties(false, cx)?.input(cx)?;
        press(host, key, false, false, guard, stats, cx).await?;
        ensure(items(host, cx)? == before, "fresh 水印终止键改写内容或历史")?;
        let live = host_live(host, cx)?;
        text_contract_smoke::key(input, key, false, false, true)?;
        settle(cx).await;
        ensure(
            items(host, cx)? == before && guard.attempts() == 0,
            "水印结束后 held 终止键透传输出或关闭",
        )?;
        host_live(host, cx)?;
        details.push(json!({"key":name,"field_before":clicked,"fresh_posted":true,
            "held_after_finish_posted":true,"original_windows":live,"output_attempts":guard.attempts()}));
    }
    report.stage("fresh-and-held-watermark-terminal-boundaries",json!({"host":host.name(),"keys":details,
        "clipboard_read":false,"clipboard_written":false,"blocked_output_attempts":guard.attempts()}))
}
async fn clear_history(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    life: &mut Lifetimes,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let original = watermark(host, cx)?;
    let clear = button(
        host,
        host.properties(false, cx)?,
        "watermark-clear",
        guard,
        stats,
        cx,
    )
    .await?;
    let cleared = watermark(host, cx)?;
    ensure(
        cleared.0 == original.0 && cleared.1 == original.1 && cleared.2.is_empty(),
        "清空没有保留原水印 ID/序位",
    )?;
    let empty_scene = life.observe(host, cx)?;
    ensure(
        empty_scene["watermark_metadata_layers"] == 0,
        "空白水印仍创建绘制层",
    )?;
    let undo = button(host, host.primary(cx)?, "undo", guard, stats, cx).await?;
    ensure(
        watermark(host, cx)? == original,
        "实际 Undo 没有恢复清空前水印",
    )?;
    let redo = button(host, host.primary(cx)?, "redo", guard, stats, cx).await?;
    ensure(
        watermark(host, cx)? == cleared,
        "实际 Redo 没有恢复原对象的空白水印",
    )?;
    let cleared_field = host.field(cx, |v, _, _| v.committed_text().to_owned())?;
    ensure(cleared_field.is_empty(), "Undo/Redo 后副栏没有从文档回填")?;
    report.stage(
        "real-clear-undo-redo-same-layer",
        json!({"host":host.name(),"clear":clear,"undo":undo,"redo":redo,
        "original_id":original.0,"original_index":original.1,"restored_text":original.2,
        "after_redo_text":cleared.2,"empty_scene":empty_scene}),
    )
}
async fn host_case(
    host: Host,
    guard: &OutputGuard,
    stats: &Rc<AdapterStats>,
    life: &mut Lifetimes,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let main = host.main(cx)?;
    install(main, guard, stats)?;
    unsafe {
        let _ = SetForegroundWindow(HWND(main.handle as *mut _));
    }
    settle(cx).await;
    ensure(
        unsafe { GetForegroundWindow().0 as usize } == main.handle,
        "Windows 未把前台焦点交给本次原自有宿主；不能冒称原生输入通过",
    )?;
    let original = raw(host, cx)?;
    select_watermark(host, guard, stats, report, cx).await?;
    above_layout(host, guard, stats, report, cx).await?;
    let focused = field_click(host, guard, stats, cx).await?;
    let before_gpu = frame(host, cx).await?;
    let text = "水印中文🙂 M";
    chars(host.properties(false, cx)?.input(cx)?, text)?;
    settle(cx).await;
    let target = watermark(host, cx)?;
    ensure(
        target.2 == text,
        "实际 WM_CHAR 未完整提交中文、emoji 和空格",
    )?;
    ensure(
        host.update(cx, |c, _, _| {
            let a = c.editor.document.get(target.0).expect("原水印对象");
            a.points.len() == 2
                && a.points[0] == crate::annotation::Point { x: 0., y: 0. }
                && a.points[1]
                    == crate::annotation::Point {
                        x: WIDTH as f32,
                        y: HEIGHT as f32,
                    }
        })?,
        "新托管水印没有覆盖完整原画布",
    )?;
    let cache = life.observe(host, cx)?;
    ensure(
        cache["watermark_metadata_layers"] == 1
            && cache["watermark_preview_count"].as_u64().unwrap_or(0) > 0,
        "水印没有使用实际 metadata Scene 与显示缓存",
    )?;
    let after_gpu = frame(host, cx).await?;
    ensure(
        before_gpu.width == after_gpu.width && before_gpu.height == after_gpu.height,
        "水印 GPU 前后观测尺寸变化",
    )?;
    let changed = before_gpu
        .bgra
        .chunks_exact(4)
        .zip(after_gpu.bgra.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    crate::export::save_png(
        before_gpu,
        &report.root.join(format!("{}-empty-gpu.png", host.name())),
    )?;
    crate::export::save_png(
        after_gpu,
        &report
            .root
            .join(format!("{}-watermark-gpu.png", host.name())),
    )?;
    ensure(changed > 8, "自有 GPU 区域没有实际水印绘制变化")?;
    report.stage("actual-unicode-watermark-and-gpu-preview",json!({"host":host.name(),"field_focus":focused,
        "actual_text":target.2,"target_id":target.0,"target_index":target.1,"source_dimensions":[WIDTH,HEIGHT],
        "gpu_changed_pixels":changed,"gpu_comparison_scope":"own region visual change; not full glyph oracle",
        "cache":cache,"ordinary_text_draft":false}))?;
    parameters(host, guard, stats, report, cx).await?;
    ime(host, guard, stats, report, cx).await?;
    terminal(host, guard, stats, report, cx).await?;
    clear_history(host, guard, stats, life, report, cx).await?;
    ensure(raw(host, cx)? == original, "工具栏编辑改写或复制了原始底图")?;
    host_live(host, cx)?;
    life.observe(host, cx)?;
    report.stage("host-watermark-contract-complete",json!({"host":host.name(),"ok":true,
        "original_source":{"render_image_id":original.image_id,"bgra_pointer":original.pointer,
            "bgra_length":original.length,"dimensions":[original.width,original.height]},
        "original_source_identity_and_white_pixels_unchanged":true,"output_attempts":guard.attempts()}))
}

async fn close_owned(
    hosts: &[Host],
    guard: &OutputGuard,
    life: &Lifetimes,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let original: Vec<_> = guard.0.windows.borrow().iter().map(|w| w.handle).collect();
    guard.disarm();
    let begin = unix_ms()?;
    for &host in hosts {
        // 错误路径也先取消本测试的 controlled preedit，再走原自有 HWND 正常关闭。
        let _ = host.field(cx, |v, _, cx| v.cancel_watermark_composition(cx));
        if let Ok(input) = host.main(cx) {
            unsafe {
                PostMessageW(
                    Some(HWND(input.handle as *mut _)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                )?;
            }
        }
    }
    let started = Instant::now();
    let mut destroyed = false;
    let mut weak_expired = false;
    for _ in 0..160 {
        tick(cx).await;
        destroyed = original
            .iter()
            .all(|h| unsafe { !IsWindow(Some(HWND(*h as *mut _))).as_bool() });
        weak_expired = life.expired();
        if destroyed && weak_expired {
            break;
        }
    }
    let end = unix_ms()?;
    report.stage("normal-owned-window-and-resource-close",json!({
        "original_registered_hwnds":original,"native_close_request_unix_ms":begin,
        "all_original_hwnds_destroyed_observed_unix_ms":destroyed.then_some(end),
        "close_stage_unix_ms":end,"elapsed_ms":started.elapsed().as_millis(),
        "all_original_hwnds_destroyed":destroyed,"all_observed_entity_and_render_weaks_expired":weak_expired,
        "render_weaks_observed":life.render.len(),"field_weaks_observed":life.fields.len(),
        "capture_entity_weaks_observed":life.captures.len(),"pin_entity_weaks_observed":life.pins.len(),
        "normal_wm_close":true,"forced_cleanup":false,"renderer_drop_log_verified":false,
        "all_gpu_or_com_resources_verified":false}))?;
    ensure(
        destroyed && weak_expired,
        "水印自有窗口或已观测预览资源没有在正常关闭后释放",
    )
}
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 每次独立 Application 冷进程；不经过会捕获真实桌面的 run_mode。
pub(crate) fn run() -> Result<()> {
    let requested_host = std::env::var("RUSTSHOT_WATERMARK_QA_HOST").ok();
    ensure(
        requested_host
            .as_deref()
            .is_none_or(|host| matches!(host, "pin" | "image_editor")),
        "水印 QA 宿主筛选无效",
    )?;
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_WATERMARK_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(
            std::env::current_dir()?
                .join("target/qa/watermark-toolbar")
                .join(format!("{}-{}", unix_ms().unwrap_or(0), std::process::id())),
        );
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let allowed = std::env::current_dir()?.join("target/qa").canonicalize()?;
    ensure(
        root.starts_with(&allowed) && root != allowed,
        "水印 QA 产物必须在当前 target/qa 的独立子目录",
    )?;
    ensure(
        !root.join("report.json").exists(),
        "不能覆盖已存在的水印 QA 报告",
    )?;
    std::fs::create_dir_all(root.join("settings"))?;
    let settings = root.join("settings").canonicalize()?;
    if let Some(existing) = std::env::var_os("RUSTSHOT_DATA_DIR") {
        ensure(
            PathBuf::from(existing).canonicalize()? == settings,
            "水印 QA DataDir 必须精确匹配 artifact/settings",
        )?;
    }
    std::env::set_var("RUSTSHOT_DATA_DIR", &settings);
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    ensure(
        crate::preferences::data_dir() == settings,
        "水印 QA 设置没有隔离",
    )?;
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    std::fs::write(settings.join("tool-presets.json"), "{}\n")?;
    let report = Report {
        root: root.clone(),
        file: OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("stages.jsonl"))?,
        stages: Vec::new(),
    };
    let report_slot = Rc::new(RefCell::new(Some(report)));
    let final_report = report_slot.clone();
    let error = Rc::new(RefCell::new(Some("水印工具栏专项没有完成".to_owned())));
    let final_error = error.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move|cx:&mut App| {
        let keepalive=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},
            |_,cx|cx.new(|_|KeepAlive)) {
            Ok(h)=>h,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}
        };
        cx.spawn(async move|cx| {
            let mut report=report_slot.borrow_mut().take().expect("独占水印报告");
            let mut life=Lifetimes{render:Vec::new(),fields:Vec::new(),captures:Vec::new(),pins:Vec::new()};
            let stats=Rc::new(AdapterStats::default());
            let mut hosts=Vec::new();
            let mut completed=Vec::new();
            let guard=OutputGuard::new(report.root.clone());
            let mut close_error=None;
            let result:Result<()>=async {
                let guard=guard.as_ref().map_err(|e|e.to_string())?;
                report.stage("cold-own-application",json!({"pid":std::process::id(),"data_dir":crate::preferences::data_dir(),
                    "fixture":"opaque synthetic white image","dimensions":[WIDTH,HEIGHT],
                    "desktop_capture_called":false,"hidden_keepalive":true}))?;
                let image=cx.update(|cx|crate::image_editor::open(fixture(),None,cx))??;
                hosts.push(Host::Image(image));
                let pin=cx.update(|cx| {
                    let work=crate::pin_native::work_area((0,0));
                    let origin=(work.x+120,work.y+(work.height.saturating_sub(HEIGHT+24)) as i32);
                    PinView::open_state(fixture(),origin,PinState::default(),cx)
                })??;
                hosts.push(Host::Pin(pin));
                pin.update(cx,|v,w,cx|->Result<()> {
                    v.ensure_canvas(w,cx)?;
                    v.perform(PinAction::Chrome,w,cx);
                    life.pins.push(cx.entity().downgrade());
                    life.render.push(Arc::downgrade(&v.original));
                    Ok(())
                })??;
                settle(cx).await;
                for &host in &hosts {
                    install(host.main(cx)?,guard,&stats)?;
                    life.observe(host,cx)?;
                }
                for &host in hosts.iter().filter(|host| requested_host.as_deref()
                    .is_none_or(|name| name == host.name())) {
                    host_case(host,guard,&stats,&mut life,&mut report,cx).await?;
                    completed.push(host.name());
                }
                ensure(stats.keys.get()>0&&stats.keys.get()==stats.restored.get()&&stats.errors.get()==0,
                    "水印 tagged UI 线程键盘状态未完整保存恢复")?;
                ensure(guard.attempts()==0,"水印专项出现被阻止的宿主输出")?;
                Ok(())
            }.await;
            let mut failure=result.err().map(|e|e.to_string());
            if let Some(message)=failure.as_ref() {
                if let Err(e)=report.stage("actual-failure",json!({"error":message,"completed_hosts":completed})) {
                    failure=Some(format!("{message}; stage write: {e}"));
                }
            }
            let output_attempts=guard.as_ref().ok().map_or_else(Vec::new,|g|g.0.attempts.borrow().clone());
            if let Ok(guard)=guard.as_ref() {
                if let Err(e)=close_owned(&hosts,guard,&life,&mut report,cx).await {
                    close_error=Some(e.to_string());
                }
            }
            let summary=json!({"suite":"watermark-toolbar-native","requested_groups":["watermark-image-editor-and-pin-two-level"],
                "host_filter":requested_host,"native_mouse_activation_simulated":true,
                "completed_hosts":completed,"all_two_hosts_verified":completed.len()==2,
                "output_guard":{"exact_original_hwnd_and_property_token":true,"attempts":output_attempts},
                "keyboard_adapter":{"keys":stats.keys.get(),"restored":stats.restored.get(),"errors":stats.errors.get(),
                    "scope":"tagged own HWND WM_KEYDOWN, saved/restored UI thread keyboard state"},
                "ime_controlled_adapter":true,"real_ime_candidate_ui_tested":false,
                "desktop_read":false,"clipboard_read":false,"clipboard_written":false,
                "global_pointer_moved":false,"global_input_injected":false,
                "gpu_scope":"own framebuffer region visual change; not full glyph oracle",
                "large_source_cache_bound_tested":false,"all_gpu_or_com_resources_verified":false,
                "runtime_memory_peak_measured":false,"close_error":close_error,"error":failure});
            if let Err(e)=report.stage("summary",summary) {
                failure=Some(failure.map_or_else(||e.to_string(),|old|format!("{old}; summary write: {e}")));
            }
            *error.borrow_mut()=failure.or(close_error);
            drop(guard);
            *report_slot.borrow_mut()=Some(report);
            let _=keepalive.update(cx,|_,w,_|w.remove_window());
            let _=cx.update(|cx|cx.quit());
        }).detach();
    });
    let failure = final_error.borrow_mut().take();
    if let Some(mut report) = final_report.borrow_mut().take() {
        report.stage(
            "after-application",
            json!({"capture_workers":crate::long_capture_runtime::active_workers(),
            "capture_threads":crate::long_capture_pipeline::active_capture_threads(),
            "application_returned_normally":true,"forced_process_termination":false}),
        )?;
        let ok = failure.is_none();
        let summary = json!({"suite":"watermark-toolbar-native","ok":ok,"error":failure,
            "stages":report.stages,"application_returned_normally":true,"forced_cleanup":false,
            "scope":"one cold worker, synthetic image editor plus Pin real two-level toolbar, own WndProc and IME controlled adapter"});
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(report.root.join("report.json"))?;
        serde_json::to_writer_pretty(&mut file, &summary)?;
        file.flush()?;
        println!("{}", report.root.display());
    }
    if let Some(error) = failure {
        Err(error.into())
    } else {
        Ok(())
    }
}
