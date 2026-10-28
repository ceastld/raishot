//! 选区外观原生专项：只读取自有合成窗口的小样，不保存桌面、图片或剪贴板。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture::Image,
    capture_view::{Action, CaptureView},
    crop_mask::Polygon,
    document::Object,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::PinState,
    pin_view::PinView,
    selection_finish::{FinishLooks, FinishPreset, SelectionFinishStyle},
    selection_finish_store::SelectionFinishStore,
    selection_finish_view::{look_id, FinishLayer, FinishSection},
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AppContext, AsyncApp, Entity, RenderImage, WeakEntity, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    fs::File,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::{ClientToScreen, GetDC, GetPixel, ReleaseDC, CLR_INVALID},
    UI::WindowsAndMessaging::*,
};

const FOLDER: &str = "target/qa/selection-finish";
const BACKGROUND: [u8; 4] = [255; 4];
const MARKER: [u8; 4] = [0x96, 0xc9, 0x39, 255];
const TIMEOUT: Duration = Duration::from_secs(40);
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

fn report_path() -> PathBuf {
    PathBuf::from(FOLDER).join(format!("ui-{}.jsonl", std::process::id()))
}
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
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
    fn pass(&mut self, name: &str, details: &str) -> Result<()> {
        if self.started.elapsed() >= TIMEOUT {
            return Err(format!("选区外观专项在 {name} 超时").into());
        }
        let details = if details.is_empty() {
            String::new()
        } else {
            format!(",{details}")
        };
        let line = format!(
            "{{\"test\":\"{name}\",\"elapsed_ms\":{:.1}{details},\"ok\":true}}",
            self.started.elapsed().as_secs_f64() * 1000.
        );
        println!("{line}");
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        Ok(())
    }
}
async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[i..i + 4].try_into().expect("fixture pixel")
}
fn fixture(width: u32, height: u32, color: [u8; 4]) -> Image {
    Image {
        width,
        height,
        bgra: color.repeat(width as usize * height as usize),
        readback_ms: 0.,
    }
}
fn copy_image(image: &Image) -> Image {
    Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: image.readback_ms,
    }
}
struct Fixture {
    area: Rect,
    global: ScreenRect,
    objects: Vec<Object>,
    preview: Arc<RenderImage>,
}
fn install_fixture(view: &mut CaptureView) -> Result<Fixture> {
    let global = view.source.bounds;
    let monitor = view.source.screen_rect(view.source.current);
    if monitor.width < 1000 || monitor.height < 740 {
        return Err("选区外观专项需要至少 1000×740 物理像素显示器".into());
    }
    let area = Rect {
        x: monitor.x + 100,
        y: monitor.y + 180,
        width: 480,
        height: 300,
    };
    if global.width as u64 * global.height as u64 > 32_000_000 {
        return Err("选区外观专项合成画布超过 3200 万像素".into());
    }
    // 保持桌面后端语义：圆角预览使用截图灰色遮罩，不能走文件的透明棋盘格。
    let buffer = image::RgbaImage::from_raw(
        global.width,
        global.height,
        fixture(global.width, global.height, BACKGROUND).bgra,
    )
    .ok_or("选区外观合成底图尺寸无效")?;
    let preview = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens: vec![FrozenScreen {
            rect: global,
            preview,
        }],
        bounds: global,
        current: 0,
        capture_ms: 0.,
    });
    view.source = source;
    view.previews = previews;
    view.image_canvas = None;
    view.editor = crate::editor::Editor::new(view.source.full(), Some(area));
    let mut marker = Annotation::new(
        Tool::Rectangle,
        Point {
            x: area.x as f32 + 100.,
            y: area.y as f32 + 100.,
        },
        Style {
            color: 0x39c996,
            filled: true,
            ..Default::default()
        },
    );
    marker.update(Point {
        x: area.x as f32 + 180.,
        y: area.y as f32 + 155.,
    });
    view.editor.document.insert(marker);
    view.editor.select_tool(Tool::Crop);
    view.include_cursor = false;
    view.cursor = None;
    view.cursor_preview = None;
    view.magnifier = false;
    view.guides = false;
    view.show_guides = false;
    view.window_targets.clear();
    view.hover_window = None;
    view.toolbar_monitor = 0;
    view.toolbar_position = None;
    view.toolbar_panel = ToolbarPanel::None;
    view.finish_style = SelectionFinishStyle::default();
    // 从原样与各样式出厂外观开始，不受此前会话记住的样式影响。
    view.finish_preset = FinishPreset::Raw;
    view.finish_looks = FinishLooks::default();
    view.finish_hover = None;
    view.finish_store.reset_session_style(view.finish_style);
    view.finish_store.reset_session_preset(view.finish_preset);
    view.finish_store.reset_session_looks(view.finish_looks);
    view.finish_preview = None;
    view.raster_previews.clear();
    view.status.clear();
    view.pointer = (area.x as i32 + 220, area.y as i32 + 220);
    view.pointer_sample = Point::from_tuple(view.pointer);
    Ok(Fixture {
        area,
        global,
        objects: view.editor.document.items().to_vec(),
        preview: view.previews[0].clone(),
    })
}
async fn button(
    view: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let p = control_point(view, input, id, cx).await?;
    input.click(p);
    step(cx).await;
    Ok(())
}
/// 单击样式即换上它记住的外观；选区条收起成单按钮时从画廊大缩略图里选。
async fn pick_look(
    view: &WeakEntity<CaptureView>,
    input: Input,
    look: FinishPreset,
    cx: &mut AsyncApp,
) -> Result<()> {
    let bar = look_id(look, false);
    let (inline, gallery) = view
        .update(cx, |v, _| {
            (
                v.ui_positions.borrow().contains_key(bar),
                v.toolbar_panel == ToolbarPanel::Finish
                    && v.finish_section == FinishSection::Gallery,
            )
        })
        .map_err(|e| e.to_string())?;
    if inline {
        button(view, input, bar, cx).await?;
    } else {
        // 画廊已打开时再点单按钮会把它收起。
        if !gallery {
            button(view, input, "selection-style-menu", cx).await?;
        }
        button(view, input, look_id(look, true), cx).await?;
    }
    view.update(cx, |v, _| -> Result<()> {
        if v.finish_preset != look {
            return Err(format!("单击样式未换成{look:?}，当前为{:?}", v.finish_preset).into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}
/// 打开当前样式的调整项：选区条“调整”；收起时画廊面板里同样带当前样式的调整项。
async fn open_tune(view: &WeakEntity<CaptureView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    let (open, inline) = view
        .update(cx, |v, _| {
            (
                v.toolbar_panel == ToolbarPanel::Finish,
                v.ui_positions.borrow().contains_key("selection-style-tune"),
            )
        })
        .map_err(|e| e.to_string())?;
    if !open {
        let id = if inline {
            "selection-style-tune"
        } else {
            "selection-style-menu"
        };
        button(view, input, id, cx).await?;
    }
    view.update(cx, |v, _| -> Result<()> {
        if v.toolbar_panel != ToolbarPanel::Finish {
            return Err("选区条未打开当前样式的外观面板".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}
/// 新截图会话按磁盘恢复：当前样式、它的槽位与面板里调出的外观一致。
fn stored(look: FinishPreset, expected: SelectionFinishStyle) -> Result<()> {
    let store = SelectionFinishStore::load();
    let slot = SelectionFinishStyle {
        remember_enabled: false,
        ..expected
    };
    if store.preset() != look
        || store.looks().get(look) != slot
        || store.preset().resolve(store.style()) != expected
    {
        return Err(format!("新的截图会话未按{look:?}恢复记住的外观").into());
    }
    Ok(())
}
async fn wheel(
    view: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let p = control_point(view, input, id, cx).await?;
    input.wheel(p, 120)?;
    step(cx).await;
    Ok(())
}
async fn control_point(
    view: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    let content = id.starts_with("finish-")
        && !matches!(
            id,
            "finish-menu"
                | "finish-reset"
                | "finish-close"
                | "finish-popup"
                | "finish-popup-scroll"
                | "finish-popup-scroll-top"
                | "finish-popup-scroll-bottom"
        );
    for attempt in 0..30 {
        let (target, top, bottom, panel) = view
            .update(cx, |v, _| {
                let positions = v.ui_positions.borrow();
                (
                    positions.get(id).copied(),
                    positions.get("finish-popup-scroll-top").copied(),
                    positions.get("finish-popup-scroll-bottom").copied(),
                    v.toolbar_panel,
                )
            })
            .map_err(|e| e.to_string())?;
        if !content || panel != ToolbarPanel::Finish {
            return target.ok_or_else(|| format!("选区外观真实控件未绘制：{id}").into());
        }
        let (top, bottom) = top.zip(bottom).ok_or("选区外观真实滚动视口未绘制")?;
        if ![top.0, top.1, bottom.0, bottom.1]
            .into_iter()
            .all(f32::is_finite)
            || bottom.1 <= top.1
            || (top.0 - bottom.0).abs() > 0.5
        {
            return Err("选区外观滚动视口探针无效".into());
        }
        if let Some(point) = target {
            if !point.0.is_finite() || !point.1.is_finite() {
                return Err("选区外观控件探针坐标无效".into());
            }
            if (top.1..=bottom.1).contains(&point.1) {
                return Ok(point);
            }
        }
        // top/bottom 的 x 来自视口左侧 2 DIP 留白，滚这里不会改到某个
        // 数值控件。没有绘制目标时先有限回顶，再向下查找；永不猜其坐标。
        let delta = target.map_or_else(
            || if attempt < 6 { 480 } else { -120 },
            |point| if point.1 < top.1 { 120 } else { -120 },
        );
        input.wheel((top.0, (top.1 + bottom.1) * 0.5), delta)?;
        step(cx).await;
    }
    Err(format!("选区外观控件在有限滚动后仍不可见：{id}").into())
}
fn control_mouse(input: Input, message: u32, p: (f32, f32), down: bool) -> Result<()> {
    let x = (p.0 * input.scale).round();
    let y = (p.1 * input.scale).round();
    if !x.is_finite()
        || !y.is_finite()
        || !(i16::MIN as f32..=i16::MAX as f32).contains(&x)
        || !(i16::MIN as f32..=i16::MAX as f32).contains(&y)
    {
        return Err("选区外观滑条消息坐标无法无损编码".into());
    }
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(
                usize::from(down)
                    | if message == WM_MOUSEMOVE {
                        gpui_smoke::SMOKE_MOVE
                    } else {
                        0
                    },
            ),
            LPARAM((((y as i16 as u16 as u32) << 16) | x as i16 as u16 as u32) as isize),
        )?;
    }
    Ok(())
}
async fn slider_to_maximum(
    view: &WeakEntity<CaptureView>,
    input: Input,
    start_id: &'static str,
    end_id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let start = control_point(view, input, start_id, cx).await?;
    let end = control_point(view, input, end_id, cx).await?;
    if ![start.0, start.1, end.0, end.1]
        .into_iter()
        .all(f32::is_finite)
        || end.0 <= start.0 + 2.
        || (start.1 - end.1).abs() > 0.5
    {
        return Err("选区外观滑条真实轨道端点无效".into());
    }
    // 轨道探针在滑条两端各内缩 8 DIP；越过端点 1 DIP 仍在控件内，
    // 使物理像素量化不把最大值误取为相邻的 0.1 档。Down 从中点开始，
    // 不先把圆角调到 0 而关掉圆角。
    let middle = ((start.0 + end.0) * 0.5, start.1);
    let maximum = (end.0 + 1., end.1);
    control_mouse(input, WM_LBUTTONDOWN, middle, true)?;
    control_mouse(input, WM_MOUSEMOVE, maximum, true)?;
    control_mouse(input, WM_LBUTTONUP, maximum, false)?;
    step(cx).await;
    Ok(())
}
async fn numeric(
    view: &WeakEntity<CaptureView>,
    input: Input,
    window: WindowHandle<CaptureView>,
    value_id: &'static str,
    text: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    button(view, input, value_id, cx).await?;
    // 只在此窗口全选后一次清空，不让逐字符删除的中间值实时改动样式。
    let select_all = gpui::Keystroke::parse("ctrl-a").map_err(|e| e.to_string())?;
    cx.update_window(window.into(), |_, w, cx| {
        w.dispatch_keystroke(select_all, cx)
    })
    .map_err(|e| e.to_string())?;
    input.key(0x2e);
    unsafe {
        for unit in text.encode_utf16() {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_CHAR,
                WPARAM(unit as usize),
                LPARAM(0),
            )?;
        }
    }
    step(cx).await;
    let before = view
        .update(cx, |v, _| v.finish_style)
        .map_err(|e| e.to_string())?;
    input.key(0x0d);
    step(cx).await;
    view.update(cx, |v, _| -> Result<()> {
        if v.toolbar_panel != ToolbarPanel::Finish || v.finish_style != before {
            return Err("内联数值 Enter 未保留外观面板，或重复修改了已实时生效的样式".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}
fn invariant(view: &CaptureView, installed: &Fixture) -> Result<()> {
    if view.editor.crop != Some(installed.area)
        || view.editor.document.items() != installed.objects.as_slice()
        || !Arc::ptr_eq(&view.previews[0], &installed.preview)
    {
        return Err("选区外观改变了原图、选区或标注的全屏坐标".into());
    }
    let raw = view.source.read_region(Rect {
        x: installed.area.x + 200,
        y: installed.area.y + 200,
        width: 2,
        height: 2,
    })?;
    if raw.bgra != BACKGROUND.repeat(4) {
        return Err("选区外观写入了原始冻结图".into());
    }
    Ok(())
}
fn cache(view: &CaptureView) -> Result<Vec<Arc<RenderImage>>> {
    let preview = view.finish_preview.as_ref().ok_or("边带预览缓存未生成")?;
    if preview.parts.is_empty() {
        return Err("边带预览缓存为空".into());
    }
    Ok(preview
        .parts
        .iter()
        .filter(|part| part.layer != FinishLayer::Corner)
        .map(|p| p.preview.clone())
        .collect())
}
fn same_cache(view: &CaptureView, previous: &[Arc<RenderImage>]) -> Result<()> {
    let current = cache(view)?;
    if current.len() != previous.len()
        || !current.iter().zip(previous).all(|(a, b)| Arc::ptr_eq(a, b))
    {
        return Err("悬停或绘制重新生成了纯装饰边带纹理".into());
    }
    Ok(())
}
/// 只读覆盖窗的自有合成像素；遮挡、越界或其它进程命中时直接失败。
fn native_pixel(input: Input, x: u32, y: u32) -> Result<[u8; 4]> {
    unsafe {
        let handle = HWND(input.handle as *mut _);
        let mut process = 0;
        GetWindowThreadProcessId(handle, Some(&mut process));
        if process != std::process::id() || !IsWindowVisible(handle).as_bool() {
            return Err("像素探针窗口不属于本次自有合成会话".into());
        }
        let mut client = RECT::default();
        GetClientRect(handle, &mut client)?;
        if x >= client.right as u32 || y >= client.bottom as u32 {
            return Err("自有合成像素探针超出客户区".into());
        }
        let mut point = POINT {
            x: x as i32,
            y: y as i32,
        };
        ClientToScreen(handle, &mut point).ok()?;
        if WindowFromPoint(point) != handle {
            return Err("自有合成小样被其它窗口遮挡，停止原生像素读取".into());
        }
        let dc = GetDC(None);
        if dc.is_invalid() {
            return Err("原生合成像素读取设备不可用".into());
        }
        let color = GetPixel(dc, point.x, point.y);
        ReleaseDC(None, dc);
        if color.0 == CLR_INVALID {
            return Err("原生合成像素读取失败".into());
        }
        Ok([
            (color.0 >> 16) as u8,
            (color.0 >> 8) as u8,
            color.0 as u8,
            255,
        ])
    }
}
fn rgb_error(actual: [u8; 4], expected: [u8; 4]) -> u16 {
    (0..3)
        .map(|i| (actual[i] as i16 - expected[i] as i16).unsigned_abs())
        .max()
        .unwrap_or(0)
}
fn on_matte(pixel: [u8; 4], matte: u8) -> [u8; 4] {
    let a = pixel[3] as u32;
    let channel = |c: u8| ((c as u32 * a + matte as u32 * (255 - a) + 127) / 255) as u8;
    [channel(pixel[0]), channel(pixel[1]), channel(pixel[2]), 255]
}
fn verify_native_edges(view: &mut CaptureView, input: Input, area: Rect) -> Result<(u16, usize)> {
    let output = view.selected_image()?;
    let pad = view.effective_finish_style().padding().left;
    let mut maximum = 0;
    // 远离工具栏、绿色选区框与尺寸标签：直边、圆角镂空、圆角边框各取一个自有小样。
    let radius = view.finish_style.corner_radius as u32;
    let arc = (4..radius)
        .flat_map(|x| (4..radius).map(move |y| (x, y)))
        .find(|&(x, y)| {
            let dx = x as f64 + 0.5 - radius as f64;
            let dy = y as f64 + 0.5 - radius as f64;
            (dx.hypot(dy) - radius as f64).abs() < 0.35
        })
        .ok_or("圆角边缘未找到独立几何抗锯齿小样")?;
    for (x, y) in [
        (pad - 10, pad + 73),
        (pad - 3, pad + 73),
        (pad + 4, pad + 4),
        (pad + arc.0, pad + arc.1),
    ] {
        let screen_x = area.x + x - pad;
        let screen_y = area.y + y - pad;
        let expected = on_matte(pixel(&output, x, y), 136);
        let actual = native_pixel(input, screen_x, screen_y)?;
        let error = rgb_error(actual, expected);
        maximum = maximum.max(error);
        if error > 12 {
            return Err(format!("GPU外观边缘与透明导出不一致：({x},{y}) RGB误差={error}").into());
        }
    }
    let actual = native_pixel(input, area.x + 135, area.y + 125)?;
    if rgb_error(actual, MARKER) > 4 {
        return Err("原生预览里的标注因外观发生平移或变色".into());
    }
    Ok((maximum, 5))
}
fn verify_aa_sample() -> Result<usize> {
    let output = SelectionFinishStyle {
        corner_enabled: true,
        corner_radius: 12.,
        ..Default::default()
    }
    .compose(fixture(31, 25, [35, 91, 197, 128]))?;
    if pixel(&output, 0, 0) != [0; 4] || pixel(&output, 15, 12) != [35, 91, 197, 128] {
        return Err("圆角小样未保持透明角和 straight alpha 中心".into());
    }
    let mut levels = BTreeSet::new();
    for p in output.bgra.chunks_exact(4) {
        if p[3] > 0 && p[3] < 128 {
            if p[..3] != [35, 91, 197] {
                return Err("圆角抗锯齿预乘了RGB，产生黑边".into());
            }
            levels.insert(p[3]);
        }
    }
    if levels.len() < 6 {
        return Err(format!("圆角边缘只有 {} 档alpha", levels.len()).into());
    }
    Ok(levels.len())
}

async fn verify(
    view: WeakEntity<CaptureView>,
    window: WindowHandle<CaptureView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut qa = Qa::new()?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    let installed = view
        .update(cx, |v, cx| {
            let result = install_fixture(v);
            cx.notify();
            result
        })
        .map_err(|e| e.to_string())??;
    step(cx).await;
    // 选区条单击“描边”即换上它的出厂外观：圆角 + 外侧边框，无阴影。
    pick_look(&view, input, FinishPreset::Stroke, cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        invariant(v, &installed)?;
        let s = v.finish_style;
        if s != FinishPreset::Stroke.defaults()
            || !s.corner_enabled
            || s.shadow_enabled
            || !s.border_enabled
        {
            return Err(format!("选区条“描边”未换上带圆角与边框、无阴影的外观：{s:?}").into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    // “调整”只改当前样式；面板里没有效果开关，阴影扩散从 0 设值即开启阴影。
    open_tune(&view, input, cx).await?;
    numeric(&view, input, window, "finish-blur-value", "28", cx).await?;
    numeric(&view, input, window, "finish-opacity-value", "55", cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        invariant(v, &installed)?;
        let s = v.finish_style;
        if v.finish_preset != FinishPreset::Stroke
            || !s.shadow_enabled
            || !s.border_enabled
            || s.shadow_blur != 28.
            || (s.shadow_opacity - 0.55).abs() > 0.0001
        {
            return Err(format!("扩散从 0 设值未开启阴影，或改换了当前样式：{s:?}").into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    slider_to_maximum(&view, input, "finish-radius-start", "finish-radius-end", cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        invariant(v, &installed)?;
        if v.finish_style.corner_radius != 60. || !v.finish_style.corner_enabled {
            return Err("真实圆角滑条拖动未到达最大值或改错目标".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    numeric(&view, input, window, "finish-radius-value", "24", cx).await?;
    numeric(&view, input, window, "finish-thickness-value", "4", cx).await?;
    for id in [
        "finish-border-inside",
        "finish-border-outside",
        "finish-border-red",
    ] {
        button(&view, input, id, cx).await?;
    }
    for id in [
        "finish-radius",
        "finish-blur",
        "finish-opacity",
        "finish-thickness",
    ] {
        wheel(&view, input, id, cx).await?;
    }
    let remembered = view
        .update(cx, |v, _| -> Result<_> {
            invariant(v, &installed)?;
            let s = v.finish_style;
            if s.corner_radius != 25.
                || s.shadow_blur != 29.
                || (s.shadow_opacity - 0.56).abs() > 0.0001
                || s.border_thickness != 5.
                || s.border_inside
                || s.border_color != 0xffff_5368
            {
                return Err(format!("原生滑条/内联数值/滚轮操作未生效：{s:?}").into());
            }
            // 调整只记进当前样式的槽位，其它样式保持出厂外观。
            let slot = SelectionFinishStyle {
                remember_enabled: false,
                ..s
            };
            if v.finish_preset != FinishPreset::Stroke
                || v.finish_looks.get(FinishPreset::Stroke) != slot
                || v.finish_looks.get(FinishPreset::Card) != FinishPreset::Card.defaults()
            {
                return Err("调整未记进“描边”槽位，或改到了其它样式".into());
            }
            Ok(s)
        })
        .map_err(|e| e.to_string())??;
    qa.pass(
        "native_finish_crop_properties_popup_slider_numeric_wheel",
        "",
    )?;
    // 从数字编辑回到外观控制后 Esc 才按主面板合同收起；数字框内的 Esc
    // 只撤回该输入草稿，不能把它误当关闭整页。
    window
        .update(cx, |v, w, _| w.focus(&v.focus))
        .map_err(|e| e.to_string())?;
    input.key(0x1b);
    step(cx).await;

    let baked = view
        .update(cx, |v, _| -> Result<Image> {
            invariant(v, &installed)?;
            if v.toolbar_panel != ToolbarPanel::None {
                return Err("Esc未关闭外观参数面板".into());
            }
            let plain = v.selected_content_image()?;
            if (plain.width, plain.height) != (installed.area.width, installed.area.height)
                || pixel(&plain, 135, 125) != MARKER
                || pixel(&plain, 0, 0) != BACKGROUND
            {
                return Err("OCR用未装饰内容被圆角/扩展边框污染".into());
            }
            let output = v.selected_image()?;
            // 阴影从外侧边框外沿投出：边框 5 + 扩散 29。
            let pad = 34;
            if (output.width, output.height) != (plain.width + 2 * pad, plain.height + 2 * pad)
                || pixel(&output, pad + 135, pad + 125) != MARKER
            {
                return Err("外观输出尺寸或标注内容坐标错误".into());
            }
            let edge = Rect {
                x: 0,
                y: 0,
                width: 72,
                height: 51,
            };
            v.source.bounds.left = -2048;
            v.source.bounds.top = -700;
            v.editor.crop = Some(edge);
            let origin = v.finished_origin(edge);
            let expanded = v.selected_image()?;
            v.source.bounds = installed.global;
            v.editor.crop = Some(installed.area);
            if origin != (-2082, -734) || (expanded.width, expanded.height) != (140, 119) {
                return Err("负原点/屏幕边缘错误裁掉padding或平移了输出origin".into());
            }
            Ok(output)
        })
        .map_err(|e| e.to_string())??;
    // 导出时已保存会话偏好；新截图按“描边”及其记住的参数恢复。
    stored(FinishPreset::Stroke, remembered)?;
    qa.pass(
        "native_finish_export_padding_negative_origin_plain_ocr",
        "\"padding\":34",
    )?;

    // 高 DPI 的 Crop 角点可能盖住 (4,4) 小样；切到绘制工具后测实际外观层。
    button(&view, input, "pen", cx).await?;
    unsafe {
        if DwmFlush() < 0 {
            return Err("外观原生呈现同步失败".into());
        }
    }
    let (maximum, probes) = view
        .update(cx, |v, _| verify_native_edges(v, input, installed.area))
        .map_err(|e| e.to_string())??;
    let levels = verify_aa_sample()?;
    qa.pass(
        "native_finish_gpu_edges_and_straight_alpha",
        &format!("\"probes\":{probes},\"max_rgb_error\":{maximum},\"aa_levels\":{levels}"),
    )?;

    let previous = view
        .update(cx, |v, _| cache(v))
        .map_err(|e| e.to_string())??;
    input.hover((installed.area.x + 260, installed.area.y + 210))?;
    step(cx).await;
    view.update(cx, |v, cx| {
        cx.notify();
        same_cache(v, &previous)
    })
    .map_err(|e| e.to_string())??;
    step(cx).await;
    button(&view, input, "pen", cx).await?;
    input.stroke(&[
        (installed.area.x + 260, installed.area.y + 200),
        (installed.area.x + 280, installed.area.y + 210),
        (installed.area.x + 310, installed.area.y + 205),
    ])?;
    step(cx).await;
    view.update(cx, |v, _| -> Result<()> {
        same_cache(v, &previous)?;
        if v.editor.document.items().len() != installed.objects.len() + 1
            || v.editor.crop != Some(installed.area)
        {
            return Err("缓存复用时实际铅笔绘制未正常提交".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    qa.pass(
        "native_finish_prerender_cache_survives_hover_and_drawing",
        "",
    )?;

    view.update(cx, |v, cx| -> Result<()> {
        let a = installed.area;
        let mask = Polygon::new(&[
            Point {
                x: a.x as f32 + 13.25,
                y: a.y as f32 + 17.25,
            },
            Point {
                x: a.x as f32 + 380.75,
                y: a.y as f32 + 39.25,
            },
            Point {
                x: a.x as f32 + 173.25,
                y: a.y as f32 + 260.75,
            },
        ])
        .ok_or("合成套索无效")?;
        for draft in [true, false] {
            if draft {
                v.editor.lasso_draft = Some(mask.clone());
            } else {
                v.editor.lasso_draft = None;
                v.editor.document.set_crop_mask(Some(mask.clone()));
            }
            let rect = v.output_rect()?;
            let output = v.selected_image()?;
            if (output.width, output.height) != (rect.width, rect.height)
                || v.finished_origin(rect)
                    != (
                        installed.global.left + rect.x as i32,
                        installed.global.top + rect.y as i32,
                    )
                || v.effective_finish_style().has_effect()
                || !output.bgra.chunks_exact(4).any(|p| p[3] == 0)
                || !output.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255)
            {
                return Err("套索草稿或已应用套索未绕过全部外观/扩展边距".into());
            }
        }
        v.editor.document.set_crop_mask(None);
        v.editor.lasso_draft = None;
        cx.notify();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    step(cx).await;
    qa.pass(
        "native_finish_lasso_draft_and_applied_bypass_all_effects",
        "",
    )?;

    let file = cx
        .update(|cx| crate::image_editor::open(copy_image(&baked), None, cx))
        .map_err(|e| e.to_string())??;
    file.update(cx, |v, _, _| -> Result<()> {
        if v.finish_style.corner_enabled
            || v.finish_style.shadow_enabled
            || v.finish_style.border_enabled
        {
            return Err("图片编辑默认再次套用记住的外观".into());
        }
        if v.selected_image()?.bgra != baked.bgra {
            return Err("图片编辑重复修饰已输出的图片".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    file.update(cx, |v, w, _| {
        v.flush_tool_presets();
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    step(cx).await;
    let pin = cx
        .update(|cx| {
            PinView::open_state(
                copy_image(&baked),
                (
                    installed.global.left + installed.area.x as i32,
                    installed.global.top + installed.area.y as i32,
                ),
                PinState::default(),
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    pin.update(cx, |v, w, cx| -> Result<()> {
        v.ensure_canvas(w, cx)?;
        let canvas = v.canvas.as_ref().ok_or("合成贴图未建立画布")?;
        canvas.update(cx, |c, _| -> Result<()> {
            if c.finish_style.corner_enabled
                || c.finish_style.shadow_enabled
                || c.finish_style.border_enabled
            {
                return Err("贴图默认再次套用记住的外观".into());
            }
            c.finish_style = remembered;
            if c.effective_finish_style().has_effect() {
                return Err("贴图未强制绕过截图外观".into());
            }
            Ok(())
        })?;
        if v.output(cx)?.bgra != baked.bgra {
            return Err("贴图重复修饰已输出图片".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    step(cx).await;
    stored(FinishPreset::Stroke, remembered)
        .map_err(|_| "图片/贴图默认关闭外观回灌了截图记忆配置")?;
    qa.pass(
        "native_finish_file_pin_no_double_effect_and_preferences_preserved",
        "",
    )?;

    window
        .update(cx, |v, w, _| {
            w.activate_window();
            w.focus(&v.focus);
        })
        .map_err(|e| e.to_string())?;
    step(cx).await;
    // 每个样式记住自己的参数：绘制模式下从选区条换回原样即关闭全部效果，
    // 新截图也以原样开始；“描边”调过的参数仍留在它的槽位里，换回即恢复。
    let slot = SelectionFinishStyle {
        remember_enabled: false,
        ..remembered
    };
    pick_look(&view, input, FinishPreset::Raw, cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        if v.finish_style.has_effect() || v.finish_looks.get(FinishPreset::Stroke) != slot {
            return Err("换回原样未关闭效果，或“描边”丢失了调过的参数".into());
        }
        v.flush_tool_presets();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    let next = SelectionFinishStore::load();
    if next.preset() != FinishPreset::Raw
        || next.preset().resolve(next.style()).has_effect()
        || next.looks().get(FinishPreset::Stroke) != slot
    {
        return Err("换回原样后新截图仍带外观，或丢失“描边”记住的参数".into());
    }
    pick_look(&view, input, FinishPreset::Stroke, cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        if v.finish_style != remembered {
            return Err("换回“描边”未恢复它记住的参数".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    if cx
        .update(|cx| cx.windows().len())
        .map_err(|e| e.to_string())?
        != 1
    {
        return Err("选区外观专项遗留了图片/贴图/属性窗口".into());
    }
    qa.pass("native_finish_new_session_remember_off_retains_params", "")?;
    window
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
    *errors.borrow_mut() = Some("选区外观专项尚未完成".into());
    let weak = view.downgrade();
    let owner = window
        .window_handle()
        .downcast::<CaptureView>()
        .expect("capture window");
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let done = Rc::new(Cell::new(false));
    window.on_next_frame(move |_, cx| {
        let watchdog_done = done.clone();
        let watchdog_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(TIMEOUT).await;
            if !watchdog_done.get() {
                *watchdog_errors.borrow_mut() = Some("选区外观原生专项超时（40秒）".into());
                let _ = cx.update(|cx| cx.quit());
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(input.handle as *mut _)),
                        WM_NULL,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let result = verify(weak, owner, input, cx).await;
            done.set(true);
            if let Err(error) = &result {
                let line = format!(
                    "{{\"test\":\"native_finish_failed\",\"error\":\"{}\",\"ok\":false}}",
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
            let _ = cx.update(|cx| cx.quit());
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(input.handle as *mut _)),
                    WM_NULL,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        })
        .detach();
    });
}
