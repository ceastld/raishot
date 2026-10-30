//! 设置真实交互专项；只操作本进程窗口，配置与合成图均隔离于 QA 会话。
use crate::{
    annotation::Tool,
    canvas_source::CanvasSource,
    capture::Image,
    capture_selection_bar::SelectionBarDisplay,
    capture_view::{Action, CaptureView},
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::PinState,
    pin_toolbar::PinToolbar,
    pin_view::PinView,
    preferences::Preferences,
    selection_finish::FinishPreset,
    selection_finish_view::look_id,
    settings_view::{SettingsTab, SettingsView},
    toolbar_config::{ToolbarConfig, DEFAULT_TOOLS},
    toolbar_layout::{PanelRect, ToolbarPanel},
    Result,
};
use gpui::{App, AppContext, AsyncApp, Entity, RenderImage, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs::File,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const FOLDER: &str = "target/qa/customization";
const TIMEOUT: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(2);
const FIXED_CAPTURE_BUTTONS: [&str; 9] = [
    "edit",
    "undo",
    "redo",
    "more",
    "copy",
    "save",
    "cancel",
    "selection-size-menu",
    "selection-bar-more",
];
const SELECTION_BAR_BUTTONS: [&str; 2] = ["selection-size-menu", "selection-bar-more"];
const AREA: Rect = Rect {
    x: 80,
    y: 100,
    width: 600,
    height: 300,
};

fn report_path() -> PathBuf {
    PathBuf::from(FOLDER).join(format!("ui-{}.jsonl", std::process::id()))
}
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
struct Qa {
    file: File,
    started: Instant,
}
impl Qa {
    fn new() -> Result<Self> {
        std::fs::create_dir_all(FOLDER)?;
        Ok(Self {
            file: File::create(report_path())?,
            started: Instant::now(),
        })
    }
    fn pass(&mut self, name: &str) -> Result<()> {
        ensure(self.started.elapsed() < TIMEOUT, "自定义设置专项超时")?;
        let line = format!(
            "{{\"test\":\"{name}\",\"elapsed_ms\":{:.1},\"ok\":true}}",
            self.started.elapsed().as_secs_f64() * 1000.
        );
        println!("{line}");
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        Ok(())
    }
}
fn ensure(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
}
fn input(window: &Window) -> Input {
    Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    }
}
fn viewport(window: &Window) -> (f32, f32) {
    (
        f32::from(window.viewport_size().width),
        f32::from(window.viewport_size().height),
    )
}
fn visible(p: (f32, f32), bounds: (f32, f32)) -> bool {
    p.0 >= 8. && p.0 < bounds.0 - 8. && p.1 >= 8. && p.1 < bounds.1 - 8.
}
fn panel_contains(rect: PanelRect, point: (f32, f32)) -> bool {
    point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
}
/// 选区条：尺寸、样式、更多入口固定在条内，不随工具栏隐藏/重排移动。
fn capture_selection_bar(
    view: &CaptureView,
    window: &Window,
) -> Result<HashMap<&'static str, (f32, f32)>> {
    let scale = window.scale_factor();
    let selected = view.selected()?;
    let main = view.toolbar_layout(selected, scale).primary;
    let bar = view
        .capture_selection_bar_layout(selected, window)
        .ok_or("普通截图缺少选区条布局")?;
    let positions = view.ui_positions.borrow();
    ensure(
        !positions.contains_key("crop"),
        "普通截图主栏仍保留独立Crop按钮",
    )?;
    ensure(
        !positions.contains_key("property-toolbar"),
        "默认范围操控仍绘制旧Crop外观属性行",
    )?;
    ensure(
        !(bar.panel.x < main.right()
            && bar.panel.right() > main.x
            && bar.panel.y < main.bottom()
            && bar.panel.bottom() > main.y),
        "选区条与主工具栏发生重叠",
    )?;
    // 样式组：空间够时五个缩略图 + “调整”，收起时只剩当前样式一个按钮。
    let mut style = Vec::new();
    if bar.display == SelectionBarDisplay::Compact {
        style.push("selection-style-menu");
    } else {
        style.extend(FinishPreset::ALL.map(|look| look_id(look, false)));
        style.push("selection-style-tune");
    }
    let mut entries = HashMap::new();
    entries.insert(
        "selection-bar",
        (
            bar.panel.x + bar.panel.width / 2.,
            bar.panel.y + bar.panel.height / 2.,
        ),
    );
    for id in FIXED_CAPTURE_BUTTONS.into_iter().chain(style) {
        let point = positions
            .get(id)
            .copied()
            .ok_or_else(|| format!("截图固定入口未绘制：{id}"))?;
        if FIXED_CAPTURE_BUTTONS.contains(&id) && !SELECTION_BAR_BUTTONS.contains(&id) {
            ensure(
                panel_contains(main, point) && !panel_contains(bar.panel, point),
                "截图固定主栏入口移入选区条或超出主栏",
            )?;
        } else {
            ensure(
                panel_contains(bar.panel, point) && !panel_contains(main, point),
                "选区条入口移入主工具栏或超出选区条",
            )?;
            entries.insert(id, point);
        }
    }
    Ok(entries)
}
fn owned(input: Input) -> Result<()> {
    let mut process = 0;
    let h = HWND(input.handle as *mut _);
    unsafe { GetWindowThreadProcessId(h, Some(&mut process)) };
    ensure(
        process == std::process::id() && unsafe { IsWindowVisible(h) }.as_bool(),
        "专项输入目标不是本进程可见窗口",
    )
}

struct Fixture {
    global: ScreenRect,
    preview: Arc<RenderImage>,
}
impl Fixture {
    fn new(view: &CaptureView) -> Result<Self> {
        let global = view.source.bounds;
        ensure(
            AREA.valid(global.width, global.height)
                && u64::from(global.width) * u64::from(global.height) <= 32_000_000,
            "专项合成画布尺寸不足或超过 3200 万像素",
        )?;
        let pixels = [0xee, 0xf3, 0xf5, 255].repeat(global.width as usize * global.height as usize);
        let buffer = image::RgbaImage::from_raw(global.width, global.height, pixels)
            .ok_or("专项合成底图尺寸无效")?;
        Ok(Self {
            global,
            preview: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        })
    }
    fn install(&self, view: &mut CaptureView) {
        let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
            screens: vec![FrozenScreen {
                rect: self.global,
                preview: self.preview.clone(),
            }],
            bounds: self.global,
            current: 0,
            capture_ms: 0.,
        });
        let mut fresh = CaptureView::new(
            source,
            previews,
            view.focus.clone(),
            Some(AREA),
            view.capture_lock.take(),
            None,
            Vec::new(),
        );
        fresh.magnifier = false;
        fresh.guides = false;
        fresh.show_guides = false;
        fresh.include_cursor = false;
        fresh.finish_style = Default::default();
        fresh.finish_store.reset_session_style(fresh.finish_style);
        fresh.editor.select_tool(Tool::Crop);
        *view = fresh;
    }
}

async fn open_settings(cx: &mut AsyncApp) -> Result<WindowHandle<SettingsView>> {
    let settings = cx
        .update(SettingsView::open_window)
        .map_err(|e| e.to_string())??;
    let target = settings
        .update(cx, |_, w, _| input(w))
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(target.handle as *mut _))?;
    activate_settings(settings, cx).await?;
    Ok(settings)
}
async fn activate_settings(settings: WindowHandle<SettingsView>, cx: &mut AsyncApp) -> Result<()> {
    let target = settings
        .update(cx, |_, w, _| {
            if !w.is_window_active() || unsafe { GetForegroundWindow() } != hwnd(w) {
                w.activate_window();
            }
            input(w)
        })
        .map_err(|e| e.to_string())?;
    owned(target)?;
    let started = Instant::now();
    loop {
        let active = settings
            .update(cx, |_, w, _| w.is_window_active())
            .map_err(|e| e.to_string())?;
        let foreground = unsafe { GetForegroundWindow() };
        if active && foreground == HWND(target.handle as *mut _) {
            return Ok(());
        }
        if started.elapsed() >= READY_TIMEOUT {
            return Err(format!(
                "QA 设置窗口激活超时；window_active={active} foreground=0x{:x} own=0x{:x}",
                foreground.0 as usize, target.handle
            )
            .into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// 先真实点击所属设置页签，再把控件滚动到右侧内容视口内，最多 28 次。
async fn settings_button(
    settings: WindowHandle<SettingsView>,
    id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    activate_settings(settings, cx).await?;
    let requested_tab = SettingsTab::for_control(id);
    for _ in 0..28 {
        let (target, bounds, position, active, tab_matches, tab_position, content) = settings
            .update(cx, |v, w, _| {
                let positions = v.ui_positions.borrow();
                (
                    input(w),
                    viewport(w),
                    positions.get(id).copied(),
                    w.is_window_active(),
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
        let foreground = unsafe { GetForegroundWindow() };
        if !active || foreground != HWND(target.handle as *mut _) {
            return Err(format!("QA 设置输入被外部窗口打断；id={id} window_active={active} foreground=0x{:x} own=0x{:x}", foreground.0 as usize, target.handle).into());
        }
        if !tab_matches {
            if let Some(at) = tab_position.filter(|at| visible(*at, bounds)) {
                target.click(at);
            }
            step(cx).await;
            continue;
        }
        let Some(((top_left, bottom_right), wheel_at)) = content else {
            step(cx).await;
            continue;
        };
        let in_content = |p: (f32, f32)| {
            visible(p, bounds)
                && p.0 > top_left.0 + 8.
                && p.0 < bottom_right.0 - 8.
                && p.1 > top_left.1 + 8.
                && p.1 < bottom_right.1 - 8.
        };
        ensure(in_content(wheel_at), "设置内容视口探针越界")?;
        if let Some(p) = position.filter(|p| in_content(*p)) {
            target.click(p);
            step(cx).await;
            return Ok(());
        }
        let delta = if position.is_some_and(|p| p.1 <= top_left.1 + 8.) {
            360
        } else {
            -360
        };
        target.wheel(wheel_at, delta)?;
        step(cx).await;
    }
    Err(format!("设置按钮未在限次滚动后进入视口：{id}").into())
}
async fn recorded_key(
    settings: WindowHandle<SettingsView>,
    key: VIRTUAL_KEY,
    cx: &mut AsyncApp,
) -> Result<()> {
    settings_button(settings, "hotkey-record", cx).await?;
    let target = settings
        .update(cx, |v, w, _| -> Result<Input> {
            let focused = v.hotkey_focus.is_focused(w);
            let native_active = v.hotkey_native.as_ref().is_some_and(|native| native.is_active());
            let probe = v.ui_positions.borrow().get("hotkey-record").copied();
            let details = format!(
                "recording_hotkey={} hotkey_focused={} window_active={} native_active={} foreground=0x{:x} own=0x{:x} probe={probe:?} viewport={:?} status={:?}",
                v.recording_hotkey, focused, w.is_window_active(), native_active,
                unsafe { GetForegroundWindow() }.0 as usize, input(w).handle, viewport(w), v.status,
            );
            if !w.is_window_active() || unsafe { GetForegroundWindow() } != hwnd(w) {
                return Err(format!("QA 录制被外部窗口打断；{details}").into());
            }
            if !(v.recording_hotkey && focused) {
                return Err(format!("真实录制按钮未启用专用录制区；{details}").into());
            }
            if !native_active {
                return Err(format!("本专项必须使用原生 VK 录制适配；{details}").into());
            }
            Ok(input(w))
        })
        .map_err(|e| e.to_string())??;
    ensure(
        unsafe { GetForegroundWindow() } == HWND(target.handle as *mut _),
        "录制窗口已失去前台焦点",
    )?;
    // 仅投递自有 HWND；不会触发系统 PrintScreen、发送输入或改用户键盘状态。
    target.key(usize::from(key.0));
    step(cx).await;
    Ok(())
}

fn ordered_tools(
    positions: &HashMap<&'static str, (f32, f32)>,
    config: &ToolbarConfig,
) -> Result<Vec<Tool>> {
    let mut tools = Vec::with_capacity(config.tools.len());
    for &tool in &config.tools {
        let id = crate::capture_toolbar::tool_description(tool).0;
        let position = positions
            .get(id)
            .copied()
            .ok_or_else(|| format!("配置工具未实际绘制：{id}"))?;
        tools.push((tool, position));
    }
    tools.sort_by(|(_, a), (_, b)| a.1.total_cmp(&b.1).then_with(|| a.0.total_cmp(&b.0)));
    Ok(tools.into_iter().map(|(tool, _)| tool).collect())
}
fn capture_diagnostics(v: &CaptureView, w: &Window) -> String {
    let positions = v.ui_positions.borrow();
    let missing: Vec<_> = FIXED_CAPTURE_BUTTONS
        .into_iter()
        .filter(|id| !positions.contains_key(*id))
        .collect();
    let mut keys: Vec<_> = positions.keys().copied().collect();
    keys.sort_unstable();
    let native = hwnd(w);
    let mut rect = RECT::default();
    let native_rect = unsafe { GetWindowRect(native, &mut rect) }
        .is_ok()
        .then_some((rect.left, rect.top, rect.right, rect.bottom));
    format!("missing={missing:?} keys={keys:?} viewport={:?} crop={:?} toolbar_panel={:?} toolbar_monitor={} saving={} processing={} pin_mode={} tool={:?} changing_crop={} window_active={} hwnd=0x{:x} visible={} iconic={} native_active=0x{:x} foreground=0x{:x} native_rect={native_rect:?}",
        viewport(w), v.editor.crop, v.toolbar_panel, v.toolbar_monitor, v.saving, v.processing,
        v.pin_mode, v.editor.tool, v.editor.is_changing_crop(), w.is_window_active(), native.0 as usize,
        unsafe { IsWindowVisible(native) }.as_bool(), unsafe { IsIconic(native) }.as_bool(),
        unsafe { GetActiveWindow() }.0 as usize, unsafe { GetForegroundWindow() }.0 as usize)
}
async fn capture_render_ready(owner: WindowHandle<CaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let started = Instant::now();
    let mut activation_seen = false;
    loop {
        let (active, ready, details) = owner
            .update(cx, |v, w, _| {
                let active = w.is_window_active() && unsafe { GetForegroundWindow() } == hwnd(w);
                let ready = FIXED_CAPTURE_BUTTONS
                    .iter()
                    .all(|id| v.ui_positions.borrow().contains_key(*id));
                (active, ready, capture_diagnostics(v, w))
            })
            .map_err(|e| e.to_string())?;
        if activation_seen && !active {
            return Err(format!("QA 截图工具栏等待被外部窗口打断；{details}").into());
        }
        activation_seen |= active;
        if active && ready {
            return Ok(());
        }
        if started.elapsed() >= READY_TIMEOUT {
            let reason = if activation_seen {
                "QA 截图工具栏绘制就绪超时"
            } else {
                "QA 截图窗口激活超时"
            };
            return Err(format!("{reason}；{details}").into());
        }
        // 只等待正常绘制；不重建视图、通知或再次激活窗口掩盖产品状态。
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}
async fn capture_button(
    owner: WindowHandle<CaptureView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (target, p, bounds) = owner
        .update(cx, |v, w, _| {
            (
                input(w),
                v.ui_positions.borrow().get(id).copied(),
                viewport(w),
            )
        })
        .map_err(|e| e.to_string())?;
    owned(target)?;
    let p = p.ok_or_else(|| format!("截图工具按钮未绘制：{id}"))?;
    ensure(visible(p, bounds), "截图按钮处于视口外")?;
    target.click(p);
    step(cx).await;
    Ok(())
}
async fn pin_button(
    bar: WindowHandle<PinToolbar>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (target, p, bounds) = bar
        .update(cx, |v, w, _| {
            (input(w), v.positions.borrow().get(id).copied(), viewport(w))
        })
        .map_err(|e| e.to_string())?;
    owned(target)?;
    gpui_smoke::isolate_input(HWND(target.handle as *mut _))?;
    let p = p.ok_or_else(|| format!("贴图工具按钮未绘制：{id}"))?;
    ensure(visible(p, bounds), "贴图按钮处于视口外")?;
    target.click(p);
    step(cx).await;
    Ok(())
}

async fn verify(owner: WindowHandle<CaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let mut qa = Qa::new()?;
    ensure(
        std::env::var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").as_deref() == Ok("1")
            && crate::preferences::data_dir().as_path()
                == std::path::Path::new(&format!("{FOLDER}/settings-{}", std::process::id())),
        "专项配置目录或托盘通知隔离未启用",
    )?;
    let installed = owner
        .update(cx, |v, w, cx| -> Result<Fixture> {
            let fixture = Fixture::new(v)?;
            fixture.install(v);
            gpui_smoke::isolate_input(hwnd(w))?;
            cx.notify();
            Ok(fixture)
        })
        .map_err(|e| e.to_string())??;
    step(cx).await;
    capture_render_ready(owner, cx).await?;
    let default_bar = owner
        .update(cx, |view, window, _| capture_selection_bar(view, window))
        .map_err(|e| e.to_string())??;
    let settings = open_settings(cx).await?;

    recorded_key(settings, VK_SNAPSHOT, cx).await?;
    let snapshot = settings
        .update(cx, |v, _, _| -> Result<Preferences> {
            ensure(!v.recording_hotkey, "PrintScreen 未结束录制")?;
            ensure(
                v.preferences.hotkey == 3
                    && v.preferences.custom_hotkey.key == u32::from(VK_SNAPSHOT.0)
                    && v.preferences.custom_hotkey.modifiers == 0,
                "原生 PrintScreen 未保存为正确自定义组合",
            )?;
            Ok(v.preferences.clone())
        })
        .map_err(|e| e.to_string())??;
    ensure(
        Preferences::load().hotkey == 3
            && Preferences::load().custom_hotkey == snapshot.custom_hotkey,
        "自定义热键未写入隔离配置",
    )?;
    qa.pass("native_settings_record_printscreen_and_persist")?;

    recorded_key(settings, VK_ESCAPE, cx).await?;
    settings
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                !v.recording_hotkey
                    && v.preferences.hotkey == snapshot.hotkey
                    && v.preferences.custom_hotkey == snapshot.custom_hotkey,
                "Esc 取消录制改变了已有组合",
            )
        })
        .map_err(|e| e.to_string())??;
    recorded_key(settings, VK_BACK, cx).await?;
    settings
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                !v.recording_hotkey
                    && v.preferences.hotkey == 4
                    && v.preferences.hotkey().is_none(),
                "录制区 Backspace 未禁用热键",
            )
        })
        .map_err(|e| e.to_string())??;
    ensure(Preferences::load().hotkey().is_none(), "禁用热键未保存")?;
    settings_button(settings, "hotkey1", cx).await?;
    settings
        .update(cx, |_, w, _| w.remove_window())
        .map_err(|e| e.to_string())?;
    step(cx).await;
    let settings = open_settings(cx).await?;
    settings
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                v.preferences.hotkey == 1
                    && v.preferences.hotkey_label() == "Ctrl+Alt+A"
                    && v.preferences.custom_hotkey == snapshot.custom_hotkey,
                "关闭重开设置未保留预设或最后一次自定义组合",
            )
        })
        .map_err(|e| e.to_string())??;
    qa.pass("native_settings_cancel_disable_preset_and_reopen")?;

    settings_button(settings, "toolbar-reset", cx).await?;
    ensure(
        ToolbarConfig::load().tools == DEFAULT_TOOLS,
        "恢复默认工具栏未保存",
    )?;
    settings_button(settings, "toolbar-remove-group-shape", cx).await?;
    settings_button(settings, "toolbar-add-group-shape", cx).await?;
    let config = ToolbarConfig::load();
    ensure(
        config.tools
            == [
                Tool::Arrow,
                Tool::Pen,
                Tool::Text,
                Tool::Mosaic,
                Tool::PixelEraser,
                Tool::Rectangle,
            ],
        "真实工具栏设置未移除形状组并重新添加到末尾",
    )?;
    settings
        .update(cx, |v, _, _| {
            ensure(v.toolbar_config == config, "设置模型与磁盘工具栏顺序不一致")
        })
        .map_err(|e| e.to_string())??;
    settings
        .update(cx, |_, w, _| w.remove_window())
        .map_err(|e| e.to_string())?;
    step(cx).await;
    qa.pass("native_settings_toolbar_reset_hide_reorder_and_persist")?;

    // 使用真正的新 CaptureView 构造读取配置，复用合成原图 Arc，没有新的整屏像素副本。
    owner
        .update(cx, |v, w, cx| -> Result<()> {
            installed.install(v);
            ensure(v.toolbar_config == config, "新截图会话未加载共享工具栏配置")?;
            w.activate_window();
            w.focus(&v.focus);
            cx.notify();
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    capture_render_ready(owner, cx).await?;
    owner
        .update(cx, |v, w, _| -> Result<()> {
            let positions = v.ui_positions.borrow();
            ensure(
                !positions.contains_key("rectangle"),
                "隐藏矩形仍显示在截图一级栏",
            )?;
            if FIXED_CAPTURE_BUTTONS
                .iter()
                .any(|id| !positions.contains_key(*id))
            {
                return Err(format!(
                    "截图自定义移除了固定操作入口；{}",
                    capture_diagnostics(v, w)
                )
                .into());
            }
            let bar = capture_selection_bar(v, w)?;
            ensure(
                bar.iter().all(|(id, point)| {
                    default_bar.get(id).is_some_and(|before| {
                        (point.0 - before.0).abs() * w.scale_factor() <= 1.
                            && (point.1 - before.1).abs() * w.scale_factor() <= 1.
                    })
                }),
                "工具栏隐藏/重排配置改变了选区条入口或位置",
            )?;
            ensure(
                ordered_tools(&positions, &config)? == config.tools,
                "截图绘制顺序与设置不一致",
            )
        })
        .map_err(|e| e.to_string())??;
    capture_button(owner, "more", cx).await?;
    capture_button(owner, "rectangle", cx).await?;
    owner
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                v.editor.tool == Tool::Rectangle
                    && v.toolbar_panel == ToolbarPanel::None
                    && v.editor.crop == Some(AREA)
                    && Arc::ptr_eq(&v.previews[0], &installed.preview),
                "隐藏矩形不能从更多选择，或选择工具改动了底图/选区",
            )
        })
        .map_err(|e| e.to_string())??;
    qa.pass("native_capture_shared_order_fixed_actions_and_hidden_tool_access")?;

    let pin = cx
        .update(|cx| {
            PinView::open_state(
                Image {
                    width: 360,
                    height: 200,
                    bgra: [0x96, 0xc9, 0x39, 255].repeat(360 * 200),
                    readback_ms: 0.,
                },
                (installed.global.left + 120, installed.global.top + 120),
                PinState::default(),
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    let pin_input = pin
        .update(cx, |v, w, _| {
            w.activate_window();
            w.focus(&v.focus);
            input(w)
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(pin_input.handle as *mut _))?;
    step(cx).await;
    pin_input.key(usize::from(VK_SPACE.0));
    step(cx).await;
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("空格未打开贴图主工具栏")?;
    // owned HWND 初建位置/尺寸回调可能在下一帧才完成。
    for _ in 0..6 {
        let ready = bar
            .update(cx, |v, _, _| {
                v.positions.borrow().contains_key("more-tools")
            })
            .map_err(|e| e.to_string())?;
        if ready {
            break;
        }
        step(cx).await;
    }
    pin.update(cx, |v, _, cx| -> Result<()> {
        let canvas = v.canvas.as_ref().ok_or("贴图标注画布未创建")?;
        ensure(
            canvas.read(cx).toolbar_config == config,
            "贴图未加载同一工具栏配置",
        )
    })
    .map_err(|e| e.to_string())??;
    bar.update(cx, |v, _, _| -> Result<()> {
        let positions = v.positions.borrow();
        ensure(
            !positions.contains_key("rectangle"),
            "隐藏矩形仍显示在贴图主栏",
        )?;
        for id in [
            "move",
            "select",
            "undo",
            "redo",
            "more-tools",
            "copy",
            "save",
            "done",
        ] {
            ensure(positions.contains_key(id), "贴图自定义移除了固定操作入口")?;
        }
        ensure(
            ordered_tools(&positions, &config)? == config.tools,
            "贴图绘制顺序与截图/设置不一致",
        )
    })
    .map_err(|e| e.to_string())??;
    pin_button(bar, "more-tools", cx).await?;
    let picker = bar
        .update(cx, |v, _, _| v.tool_picker)
        .map_err(|e| e.to_string())?
        .ok_or("贴图更多按钮未创建工具选择窗")?;
    let (target, p, bounds) = picker
        .update(cx, |v, w, _| {
            (
                input(w),
                v.positions.borrow().get("rectangle").copied(),
                viewport(w),
            )
        })
        .map_err(|e| e.to_string())?;
    owned(target)?;
    gpui_smoke::isolate_input(HWND(target.handle as *mut _))?;
    let p = p.ok_or("贴图更多未绘制隐藏矩形入口")?;
    ensure(visible(p, bounds), "贴图更多矩形按钮位于视口外")?;
    target.click(p);
    step(cx).await;
    pin.update(cx, |v, _, cx| -> Result<()> {
        let canvas = v.canvas.as_ref().ok_or("贴图画布丢失")?;
        ensure(
            v.chrome && canvas.read(cx).editor.tool == Tool::Rectangle,
            "贴图更多不能选择隐藏矩形",
        )
    })
    .map_err(|e| e.to_string())??;
    qa.pass("native_pin_shared_order_fixed_actions_and_hidden_tool_access")?;
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    for _ in 0..10 {
        step(cx).await;
        if cx
            .update(|cx| cx.windows().len())
            .map_err(|e| e.to_string())?
            == 1
        {
            break;
        }
    }
    ensure(
        cx.update(|cx| cx.windows().len())
            .map_err(|e| e.to_string())?
            == 1,
        "专项遗留了设置、贴图、主栏、属性或更多窗口",
    )?;
    qa.pass("native_customization_owned_windows_cleaned")?;
    owner
        .update(cx, |v, w, cx| v.perform(Action::Cancel, w, cx))
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("自定义设置专项尚未完成".into());
    let owner = window
        .window_handle()
        .downcast::<CaptureView>()
        .expect("capture window");
    let wake = hwnd(window).0 as usize;
    let _ = view;
    let done = Rc::new(Cell::new(false));
    window.on_next_frame(move |_, cx| {
        let watchdog_done = done.clone();
        let watchdog_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(TIMEOUT).await;
            if !watchdog_done.get() {
                *watchdog_errors.borrow_mut() = Some("自定义设置原生专项超时（60秒）".into());
                let _ = cx.update(|cx| cx.quit());
                unsafe {
                    let _ = PostMessageW(Some(HWND(wake as *mut _)), WM_NULL, WPARAM(0), LPARAM(0));
                }
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let result = verify(owner, cx).await;
            done.set(true);
            if let Err(error) = &result {
                let line = format!(
                    "{{\"test\":\"native_customization_failed\",\"error\":\"{}\",\"ok\":false}}",
                    escape(&error.to_string())
                );
                println!("{line}");
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(report_path())
                {
                    let _ = writeln!(file, "{line}");
                }
            }
            *errors.borrow_mut() = result.err().map(|e| e.to_string());
            // 应用属于本专项进程；失败时也关闭其全部自有 GPUI 窗口。
            let _ = cx.update(|cx| {
                for handle in cx.windows() {
                    let _ = cx.update_window(handle, |_, w, _| w.remove_window());
                }
                cx.quit();
            });
            unsafe {
                let _ = PostMessageW(Some(HWND(wake as *mut _)), WM_NULL, WPARAM(0), LPARAM(0));
            }
        })
        .detach();
    });
}
