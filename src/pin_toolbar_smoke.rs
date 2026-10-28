//! 贴图两级工具栏专项：只向自有合成窗口发送原生输入，配置与图片隔离在 QA 目录。
use crate::{
    annotation::Tool,
    capture::Image,
    capture_toolbar::{tool_description, Palette},
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input, SMOKE_MOVE},
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_toolbar::PinToolbar,
    pin_toolbar_layout::PinToolbarLayout,
    pin_view::{PinAction, PinView},
    toolbar_config::{AVAILABLE_TOOLS, DEFAULT_TOOLS},
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    path::Path,
    rc::Rc,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use windows::Win32::{
    Foundation::*,
    Graphics::{
        Dwm::DwmFlush,
        Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH, WHITE_BRUSH},
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const FOLDER: &str = "target/qa/pin-toolbar";
const CHROME_FOLDER: &str = "target/qa/pin-toolbar-chrome";
const PROBE_ID: usize = 0x52535442;
unsafe extern "system" fn backdrop_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(h, message, wp, lp)
}
/// 自有类使用黑白 stock brush，不依赖 STATIC 控件的系统颜色设置。
struct NativeBackdrop(Vec<usize>);
impl NativeBackdrop {
    fn new() -> Result<Self> {
        Self::new_color(false)
    }
    fn new_color(black: bool) -> Result<Self> {
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
        let class = if black {
            windows::core::w!("RustShot.QA.PinToolbar.Backdrop.Black")
        } else {
            windows::core::w!("RustShot.QA.PinToolbar.Backdrop.White")
        };
        unsafe {
            let atom = RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(backdrop_proc),
                hInstance: instance,
                hbrBackground: HBRUSH(
                    GetStockObject(if black { BLACK_BRUSH } else { WHITE_BRUSH }).0,
                ),
                lpszClassName: class,
                ..Default::default()
            });
            if atom == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(windows::core::Error::from_win32().into());
            }
        }
        let mut backdrop = Self(Vec::new());
        for _ in 0..2 {
            let handle = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                    class,
                    windows::core::w!("RustShot · 工具栏验收黑白底图"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    Some(instance),
                    None,
                )?
            };
            backdrop.0.push(handle.0 as usize);
        }
        Ok(backdrop)
    }
    fn position(&self, bars: Bars) -> Result<()> {
        for (handle, input) in self.0.iter().zip([bars.primary, bars.properties]) {
            let rect = native::rect(HWND(input.handle as *mut _))?;
            unsafe {
                // 只覆盖两栏自身 footprint，并紧贴对应栏的后面；不激活、不铺整个工作区。
                SetWindowPos(
                    HWND(*handle as *mut _),
                    Some(HWND(input.handle as *mut _)),
                    rect.x,
                    rect.y,
                    rect.width as i32,
                    rect.height as i32,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
            }
        }
        Ok(())
    }
    fn covers(&self, rects: [Placement; 2]) -> Result<()> {
        for (handle, expected) in self.0.iter().zip(rects) {
            let h = HWND(*handle as *mut _);
            if !unsafe { IsWindowVisible(h) }.as_bool() || native::rect(h)? != expected {
                return Err("工具栏未完整覆盖验收自有白底，禁止保存裸桌面截图".into());
            }
        }
        Ok(())
    }
    fn hide(&self) {
        for handle in &self.0 {
            unsafe {
                let _ = ShowWindow(HWND(*handle as *mut _), SW_HIDE);
            }
        }
    }
}
impl Drop for NativeBackdrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            unsafe {
                let _ = DestroyWindow(HWND(*handle as *mut _));
            }
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
struct NativeChanges {
    hidden: u32,
    destroyed: u32,
}
fn changes() -> &'static Mutex<HashMap<usize, NativeChanges>> {
    static CHANGES: OnceLock<Mutex<HashMap<usize, NativeChanges>>> = OnceLock::new();
    CHANGES.get_or_init(Default::default)
}
unsafe extern "system" fn bar_events(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    // 只记录窗口可见性与销毁；回调不访问 GPUI/Editor，避免同步重入应用状态。
    if let Ok(mut changes) = changes().lock() {
        if let Some(change) = changes.get_mut(&(h.0 as usize)) {
            if message == WM_SHOWWINDOW && wp.0 == 0 {
                change.hidden += 1;
            }
            if message == WM_WINDOWPOSCHANGED && lp.0 != 0 {
                let position = &*(lp.0 as *const WINDOWPOS);
                if position.flags.contains(SWP_HIDEWINDOW) {
                    change.hidden += 1;
                }
            }
            if message == WM_NCDESTROY {
                change.destroyed += 1;
            }
        }
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(h, Some(bar_events), id);
    }
    DefSubclassProc(h, message, wp, lp)
}
struct BarProbe([usize; 2]);
impl BarProbe {
    fn new(bars: Bars) -> Result<Self> {
        let probe = Self([bars.primary.handle, bars.properties.handle]);
        for handle in probe.0 {
            changes()
                .lock()
                .map_err(|_| "工具栏原生消息记录锁损坏")?
                .insert(handle, NativeChanges::default());
            unsafe {
                SetWindowSubclass(HWND(handle as *mut _), Some(bar_events), PROBE_ID, 0).ok()?;
            }
        }
        Ok(probe)
    }
    fn stable(&self) -> Result<()> {
        let changes = changes().lock().map_err(|_| "工具栏原生消息记录锁损坏")?;
        for handle in self.0 {
            let change = changes.get(&handle).ok_or("工具栏原生记录丢失")?;
            if change.hidden != 0 || change.destroyed != 0 {
                return Err(format!("绘制期间工具栏隐藏或重建：{handle:X} {change:?}").into());
            }
        }
        Ok(())
    }
}
impl Drop for BarProbe {
    fn drop(&mut self) {
        for handle in self.0 {
            unsafe {
                let _ = RemoveWindowSubclass(HWND(handle as *mut _), Some(bar_events), PROBE_ID);
            }
            if let Ok(mut changes) = changes().lock() {
                changes.remove(&handle);
            }
        }
    }
}
#[derive(Clone, Copy)]
struct Bars {
    primary: Input,
    properties: Input,
}
impl Bars {
    fn rects(self) -> Result<[Placement; 2]> {
        Ok([
            native::rect(HWND(self.primary.handle as *mut _))?,
            native::rect(HWND(self.properties.handle as *mut _))?,
        ])
    }
    fn visible(self) -> Result<()> {
        for input in [self.primary, self.properties] {
            let h = HWND(input.handle as *mut _);
            unsafe {
                if !IsWindow(Some(h)).as_bool() || !IsWindowVisible(h).as_bool() {
                    return Err("两级工具栏有窗口不存在或不可见".into());
                }
                if GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize == 0 {
                    return Err("一级或二级工具栏丢失 NoActivate".into());
                }
            }
        }
        if self.primary.handle == self.properties.handle {
            return Err("一级与二级工具栏没有独立窗口".into());
        }
        Ok(())
    }
}
async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(240))
        .await;
}
fn primary(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<WindowHandle<PinToolbar>> {
    pin.update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "空格未打开贴图一级工具栏".into())
}
fn bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let bar = primary(pin, cx)?;
    let (primary, properties) = bar
        .update(cx, |v, w, _| {
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                v.properties,
            )
        })
        .map_err(|e| e.to_string())?;
    let properties = properties
        .ok_or("点击绘制工具后未自动打开二级属性栏")?
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    let bars = Bars {
        primary,
        properties,
    };
    bars.visible()?;
    for input in [primary, properties] {
        gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    }
    Ok(bars)
}
/// 首次 HWND 仍可能带默认 caption frame；等待原生位置、视口和实际模型布局共同收敛。
/// 只用于手势开始前，不放宽绘制中的稳定性与像素要求。
async fn settled_bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let mut previous = None;
    let mut consecutive = 0;
    let mut reason = String::new();
    for _ in 0..100 {
        let ready = (|| -> Result<_> {
            let actual = bars(pin, cx)?;
            let rects = actual.rects()?;
            let bar = primary(pin, cx)?;
            let (main_viewport, property) = bar
                .update(cx, |v, w, _| {
                    (
                        (
                            f32::from(w.viewport_size().width),
                            f32::from(w.viewport_size().height),
                        ),
                        v.properties,
                    )
                })
                .map_err(|e| e.to_string())?;
            let property_viewport = property
                .ok_or("二级栏尚未创建")?
                .update(cx, |_, w, _| {
                    (
                        f32::from(w.viewport_size().width),
                        f32::from(w.viewport_size().height),
                    )
                })
                .map_err(|e| e.to_string())?;
            let (pin_rect, pin_scale) = pin
                .update(cx, |_, w, _| (native::rect(hwnd(w)), w.scale_factor()))
                .map_err(|e| e.to_string())?;
            let pin_rect = pin_rect?;
            let work = native::work_area((
                pin_rect.x + pin_rect.width as i32 / 2,
                pin_rect.y + pin_rect.height as i32 / 2,
            ));
            let layout = PinToolbarLayout::new(pin_rect, work, pin_scale);
            let property_work = native::work_area((
                layout.primary.x + layout.primary.width as i32 / 2,
                layout.primary.y + layout.primary.height as i32 / 2,
            ));
            let (tool, count, draft, probe) = inspect(pin, cx, |v| {
                (
                    v.editor
                        .selected_annotation()
                        .map_or(v.editor.tool, |a| a.tool),
                    v.editor.selected_ids().len(),
                    v.editor.text_draft.is_some(),
                    v.ui_positions.borrow().get("property-toolbar").copied(),
                )
            })?;
            let properties = PinToolbarLayout::properties(
                layout.primary,
                property_work,
                actual.primary.scale,
                PinToolbarLayout::property_size(
                    property_work,
                    actual.primary.scale,
                    tool,
                    count,
                    draft,
                ),
                layout.properties_above,
            );
            if rects != [layout.primary, properties] {
                return Err(format!(
                    "native {rects:?} 尚未等于期望布局 {:?}",
                    [layout.primary, properties]
                )
                .into());
            }
            for (rect, viewport, scale) in [
                (rects[0], main_viewport, actual.primary.scale),
                (rects[1], property_viewport, actual.properties.scale),
            ] {
                if rect.width.abs_diff((viewport.0 * scale).round() as u32) > 1
                    || rect.height.abs_diff((viewport.1 * scale).round() as u32) > 1
                {
                    return Err(format!(
                        "native {rect:?} 尚未同步 viewport {viewport:?} scale {scale}"
                    )
                    .into());
                }
            }
            let probe = probe.ok_or("属性栏当前帧尚未绘制 probe")?;
            if (probe.0 - property_viewport.0 / 2.).abs() > 1. {
                return Err(format!(
                    "属性 probe {probe:?} 尚未同步 viewport {property_viewport:?}"
                )
                .into());
            }
            Ok((
                actual,
                rects,
                [
                    main_viewport.0,
                    main_viewport.1,
                    property_viewport.0,
                    property_viewport.1,
                ],
            ))
        })();
        match ready {
            Ok((actual, rects, viewports)) => {
                let key = (
                    [actual.primary.handle, actual.properties.handle],
                    rects,
                    viewports,
                );
                consecutive = if previous == Some(key) {
                    consecutive + 1
                } else {
                    1
                };
                previous = Some(key);
                if consecutive >= 3 {
                    return Ok(actual);
                }
            }
            Err(error) => {
                reason = error.to_string();
                previous = None;
                consecutive = 0;
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    let _ = diagnostics(pin, "layout-wait-timeout", cx);
    Err(format!("两级栏布局未在 5 秒内收敛：{reason}").into())
}
fn inspect<T>(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&CaptureView) -> T,
) -> Result<T> {
    pin.update(cx, |v, _, cx| -> Result<T> {
        Ok(f(v.canvas.as_ref().ok_or("贴图标注画布不存在")?.read(cx)))
    })
    .map_err(|e| e.to_string())?
}
fn clear_positions(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    inspect(pin, cx, |v| v.ui_positions.borrow_mut().clear())
}
async fn button(pin: WindowHandle<PinView>, id: &'static str, cx: &mut AsyncApp) -> Result<()> {
    let bar = primary(pin, cx)?;
    let (input, point) = bar
        .update(cx, |v, w, _| {
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                v.positions.borrow().get(id).copied(),
            )
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.click(point.ok_or_else(|| format!("一级按钮未绘制：{id}"))?);
    frame(cx).await;
    Ok(())
}
fn require_properties(
    pin: WindowHandle<PinView>,
    tool: Tool,
    required: &[&'static str],
    absent: &[&'static str],
    cx: &mut AsyncApp,
) -> Result<()> {
    bars(pin, cx)?;
    inspect(pin, cx, |v| -> Result<()> {
        if v.editor.tool != tool || !v.pin_editing {
            return Err(format!("一级选择 {tool:?} 未进入同一画布的绘制模式").into());
        }
        let positions = v.ui_positions.borrow();
        for id in required {
            if !positions.contains_key(id) {
                return Err(format!("{tool:?} 二级属性没有刷新：{id}").into());
            }
        }
        for id in absent {
            if positions.contains_key(id) {
                return Err(format!("{tool:?} 二级属性仍残留前一个工具：{id}").into());
            }
        }
        Ok(())
    })?
}
fn pointer(input: Input, message: u32, point: (u32, u32)) -> Result<()> {
    let flags = match message {
        WM_MOUSEMOVE => SMOKE_MOVE | 1,
        WM_LBUTTONDOWN => 1,
        _ => 0,
    };
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(flags),
            LPARAM(((point.1 << 16) | point.0) as isize),
        )?;
    }
    Ok(())
}
fn background(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<u32> {
    inspect(pin, cx, |v| Palette::new(v.light_theme).background)
}
fn diagnostics(pin: WindowHandle<PinView>, label: &str, cx: &mut AsyncApp) -> Result<()> {
    let bar = primary(pin, cx)?;
    let (main, main_viewport, properties) = bar
        .update(cx, |v, w, _| {
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                (
                    f32::from(w.viewport_size().width),
                    f32::from(w.viewport_size().height),
                ),
                v.properties,
            )
        })
        .map_err(|e| e.to_string())?;
    let properties = properties.ok_or("尺寸诊断时二级栏不存在")?;
    let (property, property_viewport) = properties
        .update(cx, |_, w, _| {
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                (
                    f32::from(w.viewport_size().width),
                    f32::from(w.viewport_size().height),
                ),
            )
        })
        .map_err(|e| e.to_string())?;
    let (tool, probe) = inspect(pin, cx, |v| {
        (
            v.editor.tool,
            v.ui_positions.borrow().get("property-toolbar").copied(),
        )
    })?;
    let probe = probe
        .map(|p| format!("[{},{}]", p.0, p.1))
        .unwrap_or_else(|| "null".into());
    for (name, input, viewport) in [
        ("primary", main, main_viewport),
        ("secondary", property, property_viewport),
    ] {
        let rect = native::rect(HWND(input.handle as *mut _))?;
        println!("{{\"diag\":\"pin_toolbar_dimensions\",\"stage\":\"{label}\",\"tool\":\"{tool:?}\",\"bar\":\"{name}\",\"hwnd\":{},\"native_rect\":[{},{},{},{}],\"scale_factor\":{},\"viewport_size\":[{},{}],\"property_toolbar_probe_center\":{probe}}}", input.handle, rect.x, rect.y, rect.width, rect.height, input.scale, viewport.0, viewport.1);
        if rect
            .width
            .abs_diff((viewport.0 * input.scale).round() as u32)
            > 1
            || rect
                .height
                .abs_diff((viewport.1 * input.scale).round() as u32)
                > 1
        {
            return Err(format!("{label} {name} 原生尺寸与 GPUI 视口未同步").into());
        }
    }
    Ok(())
}
fn pixels(rects: [Placement; 2], backdrop: &NativeBackdrop, background: u32) -> Result<[Image; 2]> {
    backdrop.covers(rects)?;
    let desktop = DesktopCapture::capture()?;
    let read = |p: Placement| {
        // 去掉外框；栏后始终有自有纯白底，尺寸/透明度回归不会混入用户桌面。
        desktop.read_region(Rect {
            x: (p.x + 3 - desktop.bounds.left) as u32,
            y: (p.y + 3 - desktop.bounds.top) as u32,
            width: p.width.saturating_sub(6),
            height: p.height.saturating_sub(6),
        })
    };
    let images = [read(rects[0])?, read(rects[1])?];
    let expected = [
        background as u8,
        (background >> 8) as u8,
        (background >> 16) as u8,
    ];
    for (bar, image) in images.iter().enumerate() {
        if image.width < 8 || image.height < 3 {
            return Err("工具栏像素区域不足以验收不透明背景".into());
        }
        let mut mismatched = 0;
        let mut white = 0;
        let mut samples = 0;
        let mut colours = BTreeMap::<[u8; 3], u32>::new();
        // 栏内右侧留白应为不透明背景；避开文字、按钮和四角的圆角/边框。
        for y in image.height / 3..(image.height * 2 / 3).max(image.height / 3 + 1) {
            for x in image.width.saturating_sub(5)..image.width.saturating_sub(1) {
                let i = ((y * image.width + x) * 4) as usize;
                let pixel = &image.bgra[i..i + 3];
                *colours.entry([pixel[2], pixel[1], pixel[0]]).or_default() += 1;
                samples += 1;
                white += u32::from(pixel == [255; 3]);
                mismatched += u32::from(pixel.iter().zip(expected).any(|(a, b)| a.abs_diff(b) > 4));
            }
        }
        if mismatched != 0 || samples == 0 {
            // 失败时只输出数字，不保存泄露/未绘制图像。
            let colours = colours
                .into_iter()
                .map(|(rgb, count)| format!("[{},{},{},{count}]", rgb[0], rgb[1], rgb[2]))
                .collect::<Vec<_>>()
                .join(",");
            println!("{{\"diag\":\"pin_bar_right_strip\",\"bar\":{bar},\"native_size\":[{},{}],\"background\":{background},\"expected_rgb\":[{},{},{}],\"actual_rgb_counts\":[{colours}],\"samples\":{samples},\"mismatched\":{mismatched},\"white_backdrop_pixels\":{white}}}", rects[bar].width, rects[bar].height, expected[2], expected[1], expected[0]);
            return Err(format!(
                "工具栏 {bar} 右侧未铺满不透明背景：{mismatched}/{samples}，白底漏出 {white} 像素"
            )
            .into());
        }
    }
    Ok(images)
}
async fn drawing_stable(
    pin: WindowHandle<PinView>,
    input: Input,
    tool: Tool,
    backdrop: &NativeBackdrop,
    cx: &mut AsyncApp,
) -> Result<()> {
    let initial = settled_bars(pin, cx).await?;
    diagnostics(pin, &format!("drawing-{tool:?}"), cx)?;
    backdrop.position(initial)?;
    frame(cx).await;
    let rects = initial.rects()?;
    let color = background(pin, cx)?;
    let before = pixels(rects, backdrop, color)?;
    let probe = BarProbe::new(initial)?;
    let count = inspect(pin, cx, |v| v.editor.document.items().len())?;
    for (message, point) in [
        (WM_LBUTTONDOWN, (40, 60)),
        (WM_MOUSEMOVE, (150, 100)),
        (WM_MOUSEMOVE, (230, 170)),
        (WM_LBUTTONUP, (230, 170)),
    ] {
        pointer(input, message, point)?;
        frame(cx).await;
        let active = bars(pin, cx)?;
        if active.primary.handle != initial.primary.handle
            || active.properties.handle != initial.properties.handle
            || active.rects()?[0] != rects[0]
        {
            return Err(format!("{tool:?} 绘制时工具栏窗口重建或一级位置跳动").into());
        }
        probe.stable()?;
        if message != WM_LBUTTONUP {
            if active.rects()? != rects || !inspect(pin, cx, |v| v.editor.is_dragging())? {
                return Err(format!("{tool:?} 按住绘制时二级栏跳动或手势丢失").into());
            }
            let during = pixels(rects, backdrop, color)?;
            for (a, b) in before.iter().zip(&during) {
                if a.width != b.width || a.height != b.height || a.bgra != b.bgra {
                    return Err(format!("{tool:?} 按住绘制时工具栏像素变化或消失").into());
                }
            }
            if tool == Tool::Pen && message == WM_LBUTTONDOWN {
                for (i, image) in during.into_iter().enumerate() {
                    export::save_png(image, &Path::new(FOLDER).join(format!("pen-held-{i}.png")))?;
                }
            }
        }
    }
    if inspect(pin, cx, |v| {
        v.editor.is_dragging() || v.editor.document.items().len() != count + 1
    })? {
        return Err(format!("{tool:?} 两栏保持期间没有正常提交一条标注").into());
    }
    backdrop.hide();
    Ok(())
}
async fn picker_tool(
    pin: WindowHandle<PinView>,
    id: &'static str,
    tool: Tool,
    cx: &mut AsyncApp,
) -> Result<()> {
    button(pin, "move", cx).await?;
    let bar = primary(pin, cx)?;
    if bar
        .update(cx, |v, _, _| v.properties.is_some())
        .map_err(|e| e.to_string())?
    {
        return Err("Move 模式未收起二级绘制属性栏".into());
    }
    button(pin, "more-tools", cx).await?;
    frame(cx).await;
    let picker = bar
        .update(cx, |v, _, _| v.tool_picker)
        .map_err(|e| e.to_string())?
        .ok_or("Move 状态点击更多工具后 Picker 自行关闭")?;
    if inspect(pin, cx, |v| v.toolbar_panel)? != ToolbarPanel::Tools {
        return Err("Move 状态下 Picker 没有保持打开面板状态".into());
    }
    clear_positions(pin, cx)?;
    let (input, point) = picker
        .update(cx, |v, w, _| {
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                v.positions.borrow().get(id).copied(),
            )
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.click(point.ok_or_else(|| format!("Picker 工具没有绘制：{id}"))?);
    frame(cx).await;
    if bar
        .update(cx, |v, _, _| v.tool_picker.is_some())
        .map_err(|e| e.to_string())?
        || inspect(pin, cx, |v| v.toolbar_panel)? != ToolbarPanel::None
    {
        return Err("选择工具后 Picker 没有关闭或遗留第三条工具栏".into());
    }
    let required: &[&str] = if tool == Tool::Polyline {
        &["property-toolbar", "width-menu", "filled"]
    } else {
        &["property-toolbar", "width-menu", "opacity-menu"]
    };
    require_properties(pin, tool, required, &["font-menu"], cx)
}
fn layout_valid(bars: Bars, pin: Placement) -> Result<()> {
    let [main, properties] = bars.rects()?;
    let work = native::work_area((
        main.x + main.width as i32 / 2,
        main.y + main.height as i32 / 2,
    ));
    for p in [main, properties] {
        if p.x < work.x
            || p.y < work.y
            || p.x + p.width as i32 > work.x + work.width as i32
            || p.y + p.height as i32 > work.y + work.height as i32
        {
            return Err(format!("屏幕边缘工具栏越出工作区：{p:?}；{work:?}").into());
        }
    }
    // 下方二级在下，上方二级在上；一级始终更接近贴图。
    let gap = if main.y >= pin.y + pin.height as i32 {
        properties.y - (main.y + main.height as i32)
    } else if main.y + main.height as i32 <= pin.y {
        main.y - (properties.y + properties.height as i32)
    } else {
        return Err(format!("一级工具栏与贴图交叠：{main:?}；{pin:?}").into());
    };
    if gap < 0 || gap > (12. * bars.primary.scale).ceil() as i32 {
        return Err(format!("两栏没有保持同侧连续排列或发生交叠：{main:?} {properties:?}").into());
    }
    Ok(())
}
async fn edge_layout(
    pin: WindowHandle<PinView>,
    input: Input,
    backdrop: &NativeBackdrop,
    cx: &mut AsyncApp,
) -> Result<()> {
    let pin_rect = native::rect(HWND(input.handle as *mut _))?;
    let work = native::work_area((pin_rect.x, pin_rect.y));
    let right = work.x + (work.width as i32 - pin_rect.width as i32).max(0);
    let bottom = work.y + (work.height as i32 - pin_rect.height as i32).max(0);
    for (i, (x, y)) in [
        (work.x, work.y),
        (right, work.y),
        (right, bottom),
        (work.x, bottom),
    ]
    .into_iter()
    .enumerate()
    {
        native::place(HWND(input.handle as *mut _), Placement { x, y, ..pin_rect })?;
        frame(cx).await;
        let main = settled_bars(pin, cx).await?.rects()?[0];
        for (id, tool) in [
            ("text", Tool::Text),
            ("pen", Tool::Pen),
            ("rectangle", Tool::Rectangle),
        ] {
            clear_positions(pin, cx)?;
            button(pin, id, cx).await?;
            let active = settled_bars(pin, cx).await?;
            diagnostics(pin, &format!("edge-{i}-{tool:?}"), cx)?;
            layout_valid(active, native::rect(HWND(input.handle as *mut _))?)?;
            if active.rects()?[0] != main || inspect(pin, cx, |v| v.editor.tool)? != tool {
                return Err("边缘切换工具挪动一级工具栏或选错工具".into());
            }
            backdrop.position(active)?;
            frame(cx).await;
            let images = pixels(active.rects()?, backdrop, background(pin, cx)?)?;
            if tool == Tool::Rectangle {
                for (j, image) in images.into_iter().enumerate() {
                    export::save_png(
                        image,
                        &Path::new(FOLDER).join(format!("edge-{i}-bar-{j}.png")),
                    )?;
                }
            }
            backdrop.hide();
        }
    }
    Ok(())
}
fn collapsed(pin: WindowHandle<PinView>, old: Bars, cx: &mut AsyncApp) -> Result<()> {
    if pin
        .update(cx, |v, _, _| v.chrome || v.toolbar.is_some())
        .map_err(|e| e.to_string())?
    {
        return Err("收起后贴图仍持有一级工具栏".into());
    }
    for input in [old.primary, old.properties] {
        unsafe {
            if IsWindow(Some(HWND(input.handle as *mut _))).as_bool() {
                return Err("收起后一级或二级原生窗口仍存在".into());
            }
        }
    }
    Ok(())
}
/// 首屏实际按钮中心与原生视口一起验证，不以旧默认六工具的固定高度判断换行。
async fn chrome_layout(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let mut last = None;
    let mut reason = String::new();
    for _ in 0..60 {
        let ready = (|| -> Result<_> {
            let active = bars(pin, cx)?;
            let rects = active.rects()?;
            let (viewport, positions) = primary(pin, cx)?
                .update(cx, |v, w, _| {
                    (
                        (
                            f32::from(w.viewport_size().width),
                            f32::from(w.viewport_size().height),
                        ),
                        v.positions.borrow().clone(),
                    )
                })
                .map_err(|e| e.to_string())?;
            let tools = inspect(pin, cx, |v| v.toolbar_config.tools.clone())?;
            let (p, dpi) = pin
                .update(cx, |_, w, _| (native::rect(hwnd(w)), w.scale_factor()))
                .map_err(|e| e.to_string())?;
            let p = p?;
            let work = native::work_area((p.x + p.width as i32 / 2, p.y + p.height as i32 / 2));
            let expected = PinToolbarLayout::new_with_tool_count(p, work, dpi, tools.len()).primary;
            if rects[0] != expected
                || rects[0]
                    .width
                    .abs_diff((viewport.0 * active.primary.scale).round() as u32)
                    > 1
                || rects[0]
                    .height
                    .abs_diff((viewport.1 * active.primary.scale).round() as u32)
                    > 1
            {
                return Err("一级栏换行位置或视口尚未收敛".into());
            }
            let ids = tools
                .iter()
                .map(|&t| tool_description(t).0)
                .chain([
                    "move",
                    "select",
                    "more-tools",
                    "undo",
                    "redo",
                    "copy",
                    "save",
                    "done",
                ])
                .collect::<Vec<_>>();
            let mut centers = Vec::new();
            for id in ids {
                let &(x, y) = positions
                    .get(id)
                    .ok_or_else(|| format!("换行后按钮未绘制：{id}"))?;
                if !x.is_finite()
                    || !y.is_finite()
                    || x < 16.
                    || y < 16.
                    || x + 16. > viewport.0 + 0.5
                    || y + 16. > viewport.1 + 0.5
                {
                    return Err(
                        format!("按钮 {id} 超出实际一级视口：({x},{y}) / {viewport:?}").into(),
                    );
                }
                if centers
                    .iter()
                    .any(|&(a, b): &(f32, f32)| (x - a).abs() < 31.5 && (y - b).abs() < 31.5)
                {
                    return Err(format!("换行按钮重叠：{id}").into());
                }
                centers.push((x, y));
            }
            if tools.len() == AVAILABLE_TOOLS.len()
                && !centers.iter().any(|p| (p.1 - centers[0].1).abs() > 30.)
            {
                return Err("全工具配置未在实际窗口内换行".into());
            }
            Ok((active, (rects, viewport, centers)))
        })();
        match ready {
            Ok((active, key)) => {
                if last.as_ref() == Some(&key) {
                    return Ok(active);
                }
                last = Some(key);
            }
            Err(e) => {
                reason = e.to_string();
                last = None;
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    Err(format!("圆角工具栏布局未收敛：{reason}").into())
}
fn chrome_pixels(bars: Bars, backdrop: &NativeBackdrop) -> Result<Image> {
    bars.visible()?;
    let rects = bars.rects()?;
    backdrop.covers(rects)?;
    unsafe {
        DwmFlush()?;
    }
    let desktop = DesktopCapture::capture()?;
    let p = rects[0];
    // 只读回底窗完整覆盖的一级 footprint；临时桌面纹理不保存、不进入输出。
    let x = u32::try_from(p.x - desktop.bounds.left)?;
    let y = u32::try_from(p.y - desktop.bounds.top)?;
    desktop.read_region(Rect {
        x,
        y,
        width: p.width,
        height: p.height,
    })
}
fn chrome_composition(
    white: &Image,
    black: &Image,
    scale: f32,
    color: u32,
    label: &str,
) -> Result<()> {
    if white.width != black.width || white.height != black.height {
        return Err("黑白背板对照期间一级工具栏尺寸变化".into());
    }
    let (w, h) = (white.width, white.height);
    if w == 0 || h == 0 {
        return Err("圆角工具栏输出尺寸无效".into());
    }
    let pixel = |image: &Image, x: u32, y: u32| -> [u8; 3] {
        let at = ((y * image.width + x) * 4) as usize;
        [image.bgra[at], image.bgra[at + 1], image.bgra[at + 2]]
    };
    let side = (12. * scale).ceil() as u32;
    let mut aa = [0_u32; 4];
    for (corner, (right, bottom)) in [(false, false), (true, false), (true, true), (false, true)]
        .into_iter()
        .enumerate()
    {
        let (x, y) = (
            if right { w - 1 } else { 0 },
            if bottom { h - 1 } else { 0 },
        );
        if pixel(white, x, y).iter().any(|&v| v < 251) || pixel(black, x, y).iter().any(|&v| v > 4)
        {
            return Err(format!(
                "{label} 四角未露出自有黑白背板：corner {corner}, white {:?}, black {:?}",
                pixel(white, x, y),
                pixel(black, x, y)
            )
            .into());
        }
        for iy in 0..side.min(h) {
            for ix in 0..side.min(w) {
                let (x, y) = (
                    if right { w - 1 - ix } else { ix },
                    if bottom { h - 1 - iy } else { iy },
                );
                let a = pixel(white, x, y);
                let b = pixel(black, x, y);
                let d = [
                    a[0] as i16 - b[0] as i16,
                    a[1] as i16 - b[1] as i16,
                    a[2] as i16 - b[2] as i16,
                ];
                if d.iter().all(|&v| (8..=247).contains(&v))
                    && d.iter().max().unwrap() - d.iter().min().unwrap() <= 6
                {
                    aa[corner] += 1;
                }
            }
        }
        if aa[corner] == 0 {
            return Err(format!("{label} corner {corner} 无随背板变化的部分透明抗锯齿像素").into());
        }
    }
    let x = w.saturating_sub((4. * scale).round() as u32).min(w - 1);
    let expected = [
        (color & 255) as u8,
        ((color >> 8) & 255) as u8,
        ((color >> 16) & 255) as u8,
    ];
    for y in h / 3..h * 2 / 3 {
        for image in [white, black] {
            if pixel(image, x, y)
                .iter()
                .zip(expected)
                .any(|(&a, b)| a.abs_diff(b) > 4)
            {
                return Err(format!(
                    "{label} 栏内右侧背景随背板变化或未铺满：{:?} / {expected:?}",
                    pixel(image, x, y)
                )
                .into());
            }
        }
    }
    println!("{{\"diag\":\"pin_toolbar_composed_chrome\",\"stage\":\"{label}\",\"size\":[{w},{h}],\"aa_per_corner\":{aa:?},\"opaque_background\":true}}");
    Ok(())
}
async fn chrome(
    pin: WindowHandle<PinView>,
    input: Input,
    white: &NativeBackdrop,
    cx: &mut AsyncApp,
) -> Result<()> {
    std::fs::create_dir_all(CHROME_FOLDER)?;
    let black = NativeBackdrop::new_color(true)?;
    frame(cx).await;
    input.key(0x20);
    frame(cx).await;
    button(pin, "rectangle", cx).await?;
    let initial = chrome_layout(pin, cx).await?;
    let handle = primary(pin, cx)?;
    let property_handle = handle
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?;
    let probe = BarProbe::new(initial)?;
    let mut short_height = 0;
    for (light, all, label) in [
        (false, false, "dark-short"),
        (true, false, "light-short"),
        (true, true, "light-wrapped"),
        (false, true, "dark-wrapped"),
    ] {
        pin.update(cx, |v, _, cx| -> Result<()> {
            // pin_app 的 toolbar 专项已隔离到 target/qa；显式主题须保持到后续帧。
            let mut preferences = crate::preferences::Preferences::load();
            preferences.theme = if light {
                crate::theme::ThemeMode::Light
            } else {
                crate::theme::ThemeMode::Dark
            };
            preferences.save()?;
            v.canvas
                .as_ref()
                .ok_or("贴图画布不存在")?
                .update(cx, |v, cx| {
                    v.light_theme = light;
                    crate::theme::set_mode(
                        if light {
                            crate::theme::ThemeMode::Light
                        } else {
                            crate::theme::ThemeMode::Dark
                        },
                        cx,
                    );
                    v.toolbar_config.tools = if all {
                        AVAILABLE_TOOLS.to_vec()
                    } else {
                        DEFAULT_TOOLS.to_vec()
                    };
                    cx.notify();
                });
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        let active = chrome_layout(pin, cx).await?;
        if primary(pin, cx)? != handle
            || handle
                .update(cx, |v, _, _| v.properties)
                .map_err(|e| e.to_string())?
                != property_handle
            || active.primary.handle != initial.primary.handle
            || active.properties.handle != initial.properties.handle
        {
            return Err("换主题或工具数量重建了两级工具栏".into());
        }
        probe.stable()?;
        let height = active.rects()?[0].height;
        if !all {
            short_height = height;
        } else if height <= short_height {
            return Err("工具数增加后一级窗口未增高".into());
        }
        black.hide();
        white.position(active)?;
        frame(cx).await;
        let on_white = chrome_pixels(active, white)?;
        white.hide();
        black.position(active)?;
        frame(cx).await;
        let on_black = chrome_pixels(active, &black)?;
        let composition = chrome_composition(
            &on_white,
            &on_black,
            active.primary.scale,
            background(pin, cx)?,
            label,
        );
        // 断言前保存仅自有 footprint，失败时也可直接检查四角真实合成结果。
        export::save_png(
            on_white,
            &Path::new(CHROME_FOLDER).join(format!("{label}-white.png")),
        )?;
        export::save_png(
            on_black,
            &Path::new(CHROME_FOLDER).join(format!("{label}-black.png")),
        )?;
        composition?;
    }
    black.hide();
    button(pin, "pen", cx).await?;
    let active = chrome_layout(pin, cx).await?;
    let rects = active.rects()?;
    let count = inspect(pin, cx, |v| v.editor.document.items().len())?;
    for (message, point) in [
        (WM_LBUTTONDOWN, (40, 60)),
        (WM_MOUSEMOVE, (150, 100)),
        (WM_LBUTTONUP, (230, 170)),
    ] {
        pointer(input, message, point)?;
        frame(cx).await;
        let now = bars(pin, cx)?;
        if now.primary.handle != initial.primary.handle
            || now.properties.handle != initial.properties.handle
            || now.rects()? != rects
        {
            return Err("按下绘制时两级工具栏 HWND 或位置变化".into());
        }
        probe.stable()?;
        if message != WM_LBUTTONUP && !inspect(pin, cx, |v| v.editor.is_dragging())? {
            return Err("原生绘制按下未进入活动手势".into());
        }
    }
    if inspect(pin, cx, |v| {
        v.editor.is_dragging() || v.editor.document.items().len() != count + 1
    })? {
        return Err("圆角工具栏存续时未提交一条铅笔标注".into());
    }
    if primary(pin, cx)? != handle
        || handle
            .update(cx, |v, _, _| v.properties)
            .map_err(|e| e.to_string())?
            != property_handle
    {
        return Err("绘制切工具重建了两级 WindowHandle".into());
    }
    println!("{{\"test\":\"native_pin_toolbar_rounded_composed_corners_themes_resize_stable\",\"ok\":true}}");
    Ok(())
}
pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("贴图两级工具栏专项未完成".into());
    let previous_foreground = unsafe { GetForegroundWindow() }.0 as usize;
    let initial = (|| -> Result<_> {
        let expected = std::env::current_dir()?.join(FOLDER).join("settings");
        if crate::preferences::data_dir() != expected {
            return Err("两级工具栏专项未隔离用户配置".into());
        }
        std::fs::create_dir_all(&expected)?;
        std::fs::write(expected.join("tool-presets.json"), "{}\n")?;
        let work = native::work_area((100, 100));
        let backdrop = NativeBackdrop::new()?;
        let pin = PinView::open_state(
            Image {
                width: 600,
                height: 360,
                bgra: vec![255; 600 * 360 * 4],
                readback_ms: 0.,
            },
            (work.x + 100, work.y + 100),
            PinState::default(),
            cx,
        )?;
        let input = pin.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })?;
        gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
        Ok((pin, input, backdrop))
    })();
    let (pin, input, backdrop) = match initial {
        Ok(initial) => initial,
        Err(e) => {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let result: Result<()> = if std::env::var("RUSTSHOT_PIN_TOOLBAR_CASE").as_deref() == Ok("chrome") {
            chrome(pin, input, &backdrop, cx).await
        } else { async {
            frame(cx).await;
            input.key(0x20);
            frame(cx).await;
            primary(pin, cx)?;
            clear_positions(pin, cx)?;
            button(pin, "rectangle", cx).await?;
            require_properties(pin, Tool::Rectangle, &["property-toolbar", "width-menu", "filled", "radius-menu"], &["font-menu"], cx)?;
            let main = bars(pin, cx)?.primary.handle;
            drawing_stable(pin, input, Tool::Rectangle, &backdrop, cx).await?;
            clear_positions(pin, cx)?;
            button(pin, "pen", cx).await?;
            require_properties(pin, Tool::Pen, &["property-toolbar", "width-menu"], &["radius-menu", "font-menu", "filled"], cx)?;
            if bars(pin, cx)?.primary.handle != main { return Err("切换铅笔重建了一级工具栏".into()); }
            drawing_stable(pin, input, Tool::Pen, &backdrop, cx).await?;
            clear_positions(pin, cx)?;
            button(pin, "text", cx).await?;
            require_properties(pin, Tool::Text, &["property-toolbar", "font-menu", "font-family-menu", "bold"], &["width-menu", "radius-menu"], cx)?;
            if bars(pin, cx)?.primary.handle != main { return Err("切换文字重建了一级工具栏".into()); }
            let font_size = inspect(pin, cx, |v| v.editor.style.font_size)?;
            let point = inspect(pin, cx, |v| v.ui_positions.borrow().get("font-menu").copied())?.ok_or("文字字号控件未绘制")?;
            bars(pin, cx)?.properties.wheel(point, 120)?;
            frame(cx).await;
            if inspect(pin, cx, |v| v.editor.style.font_size)? != font_size + 1. { return Err("文字二级属性滚轮没有操作当前工具样式".into()); }
            println!("{{\"test\":\"native_pin_two_bars_mouse_tools_properties_and_drawing_no_flash\",\"ok\":true}}");
            picker_tool(pin, "polyline", Tool::Polyline, cx).await?;
            picker_tool(pin, "highlighter", Tool::Highlighter, cx).await?;
            println!("{{\"test\":\"native_pin_more_tools_from_move_restores_second_bar\",\"ok\":true}}");
            edge_layout(pin, input, &backdrop, cx).await?;
            println!("{{\"test\":\"native_pin_two_bar_edge_layout_order_work_area_stable_primary\",\"ok\":true}}");
            let old = bars(pin, cx)?;
            input.key(0x20);
            frame(cx).await;
            collapsed(pin, old, cx)?;
            input.key(0x20);
            frame(cx).await;
            button(pin, "pen", cx).await?;
            let reopened = bars(pin, cx)?;
            button(pin, "done", cx).await?;
            collapsed(pin, reopened, cx)?;
            if cx.update(|cx| cx.windows().len()).map_err(|e| e.to_string())? != 1 { return Err("收起两栏后遗留属性弹窗或 Picker 窗口".into()); }
            println!("{{\"test\":\"native_pin_space_done_collapse_both_toolbar_windows\",\"ok\":true}}");
            Ok(())
        }.await };
        // 正常、断言失败两条路径都先销毁两个自有底窗，再恢复用户前台。
        drop(backdrop);
        unsafe {
            let mut owner = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut owner));
            if owner == std::process::id() && previous_foreground != 0 {
                let _ = SetForegroundWindow(HWND(previous_foreground as *mut _));
            }
        }
        *errors.borrow_mut() = result.err().map(|e| format!("贴图两级工具栏专项：{e}"));
        let _ = pin.update(cx, |v, w, cx| v.perform(PinAction::Close, w, cx));
        let _ = cx.update(|cx| cx.quit());
    }).detach();
}
