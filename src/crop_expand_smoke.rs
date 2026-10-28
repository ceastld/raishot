//! 真实 GPUI 外八格选区验收：自有白底、多屏原点、原生消息和内存导出。
//! 仅 JSONL 写入 target/qa；不改系统鼠标/键状态，不读剪贴板，不保存图像。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture_view::CaptureView,
    crop_mask::Polygon,
    editor::Editor,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    scene::Scene,
    toolbar_layout::{ToolbarLayout, ToolbarPanel},
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use std::{
    cell::{Cell, RefCell},
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::{Dwm::DwmFlush, Gdi::*},
    UI::{Input::KeyboardAndMouse::GetDoubleClickTime, WindowsAndMessaging::*},
};

type Log = Rc<RefCell<File>>;
fn record(log: &Log, value: serde_json::Value) -> Result<()> {
    let mut file = log.borrow_mut();
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
fn coordinates(rect: Rect) -> [u32; 4] {
    [rect.x, rect.y, rect.width, rect.height]
}
#[derive(Clone, Copy)]
struct OwnedInput {
    input: Input,
    tid: u32,
}
impl OwnedInput {
    fn check(self) -> Result<HWND> {
        let handle = HWND(self.input.handle as *mut _);
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(handle, Some(&mut pid)) };
        if pid != std::process::id() || tid == 0 || tid != self.tid {
            return Err("专项原 HWND/PID/TID 身份发生变化".into());
        }
        Ok(handle)
    }
    fn send(self, message: u32, point: (i32, i32), held: bool) -> Result<()> {
        if !(0..=i16::MAX as i32).contains(&point.0) || !(0..=i16::MAX as i32).contains(&point.1) {
            return Err("专项坐标不在原生有符号 client LPARAM 范围".into());
        }
        let handle = self.check()?;
        let buttons = usize::from(held)
            | if message == WM_MOUSEMOVE {
                gpui_smoke::SMOKE_MOVE
            } else {
                0
            };
        unsafe {
            PostMessageW(
                Some(handle),
                message,
                WPARAM(buttons),
                LPARAM((((point.1 as u16 as u32) << 16) | point.0 as u16 as u32) as isize),
            )?;
        }
        Ok(())
    }
    fn escape(self) -> Result<()> {
        let handle = self.check()?;
        unsafe {
            PostMessageW(Some(handle), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
            PostMessageW(Some(handle), WM_KEYUP, WPARAM(0x1b), LPARAM(0))?;
        }
        Ok(())
    }
}
async fn pause(cx: &mut AsyncApp, duration: Duration) {
    cx.background_executor().timer(duration).await;
}
async fn drag(
    view: &WeakEntity<CaptureView>,
    input: OwnedInput,
    from: (i32, i32),
    to: (i32, i32),
    cx: &mut AsyncApp,
) -> Result<()> {
    input.send(WM_LBUTTONDOWN, from, true)?;
    for _ in 0..80 {
        if view
            .update(cx, |v, _| {
                v.editor.is_dragging() && v.pointer_down_start == Some(from)
            })
            .map_err(|e| e.to_string())?
        {
            input.send(WM_MOUSEMOVE, to, true)?;
            return input.send(WM_LBUTTONUP, to, false);
        }
        pause(cx, Duration::from_millis(25)).await;
    }
    Err("原生 DOWN 未进入预期自有裁剪手势".into())
}
async fn settled(
    view: &WeakEntity<CaptureView>,
    expected: Rect,
    dragging: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..80 {
        if view
            .update(cx, |v, _| {
                v.editor.crop == Some(expected) && v.editor.is_dragging() == dragging
            })
            .map_err(|e| e.to_string())?
        {
            pause(cx, Duration::from_millis(120)).await;
            return Ok(());
        }
        pause(cx, Duration::from_millis(25)).await;
    }
    Err(format!(
        "原生选区未收敛：{:?}，dragging={dragging}",
        coordinates(expected)
    )
    .into())
}
fn install(view: &mut CaptureView, scale: f32) -> Result<Rect> {
    let bounds = view.source.bounds;
    if bounds.width > i16::MAX as u32 || bounds.height > i16::MAX as u32 {
        return Err("专项虚拟桌面超出 client 消息坐标范围".into());
    }
    let current = (0..view.source.screen_count())
        .find(|&index| {
            let monitor = view.source.screen_rect(index);
            monitor.width as f32 / scale >= 850. && monitor.height as f32 / scale >= 700.
        })
        .ok_or("外八格专项需要至少850×700逻辑像素显示器")?;
    let monitor = view.source.screen_rect(current);
    let screens = (0..view.source.screen_count())
        .map(|index| {
            let rect = view.source.screen_rect(index);
            FrozenScreen {
                rect: ScreenRect {
                    left: bounds.left + rect.x as i32,
                    top: bounds.top + rect.y as i32,
                    width: rect.width,
                    height: rect.height,
                },
                preview: Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::from_pixel(rect.width, rect.height, image::Rgba([255; 4])),
                )])),
            }
        })
        .collect();
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens,
        bounds,
        current,
        capture_ms: 0.,
    });
    view.source = source;
    view.previews = previews;
    view.image_canvas = None;
    view.editor = Editor::new(view.source.full(), None);
    view.toolbar_config = Default::default();
    view.toolbar_monitor = current;
    view.toolbar_panel = ToolbarPanel::None;
    // 固定纯测试栏位置，避免底侧格点击经过栏本身；后续另测真正的显示器锚点。
    view.toolbar_position = Some((
        monitor.x as f32 / scale + 80.,
        (monitor.y + monitor.height) as f32 / scale - 150.,
    ));
    view.include_cursor = false;
    view.cursor = None;
    view.cursor_preview = None;
    view.magnifier = false;
    view.guides = false;
    view.show_guides = false;
    view.auto_adsorb = false;
    view.control_selection_ready = false;
    view.control_request_pending = false;
    view.window_targets.clear();
    view.hover_window = None;
    view.finish_style = Default::default();
    view.finish_preview = None;
    view.finish_advanced = false;
    view.raster_previews.clear();
    view.cancel_pointer();
    let physical = |value: f32| (value * scale).round() as u32;
    let base = Rect {
        x: monitor.x + physical(250.),
        y: monitor.y + physical(200.),
        width: physical(220.),
        height: physical(150.),
    };
    let mut mark = Annotation::new(
        Tool::Rectangle,
        Point {
            x: base.x as f32 + 35. * scale,
            y: base.y as f32 + 35. * scale,
        },
        Style {
            filled: true,
            color: 0xe04070,
            radius: 0.,
            width: 1.,
            ..Default::default()
        },
    );
    mark.update(Point {
        x: base.x as f32 + 95. * scale,
        y: base.y as f32 + 85. * scale,
    });
    view.editor.document.insert(mark);
    view.editor.set_crop(Some(base));
    view.ui_positions.borrow_mut().clear();
    Ok(base)
}
async fn reset(view: &WeakEntity<CaptureView>, base: Rect, cx: &mut AsyncApp) -> Result<()> {
    view.update(cx, |v, cx| {
        v.cancel_pointer();
        v.editor.set_crop(Some(base));
        v.toolbar_panel = ToolbarPanel::None;
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    settled(view, base, false, cx).await
}
fn painted_toolbar(view: &CaptureView, scale: f32) -> Result<ToolbarLayout> {
    let layout = view.toolbar_layout(view.selected()?, scale);
    for (key, rect) in [
        ("primary-toolbar", layout.primary),
        ("property-toolbar", layout.properties),
    ] {
        let actual = view
            .ui_positions
            .borrow()
            .get(key)
            .copied()
            .ok_or("专项两级栏未真实绘制")?;
        let expected = (rect.x + rect.width / 2., rect.y + rect.height / 2.);
        if (actual.0 - expected.0).abs() * scale > 1. || (actual.1 - expected.1).abs() * scale > 1.
        {
            return Err("专项两级栏真实绘制位置与几何不一致".into());
        }
    }
    Ok(layout)
}
fn native_color(input: OwnedInput, point: (i32, i32)) -> Result<[u8; 3]> {
    let handle = input.check()?;
    unsafe {
        if !IsWindowVisible(handle).as_bool() || GetForegroundWindow() != handle {
            return Err("专项自有窗口不再可见/前台；停止像素检查".into());
        }
        let mut global = POINT {
            x: point.0,
            y: point.1,
        };
        ClientToScreen(handle, &mut global).ok()?;
        if WindowFromPoint(global) != handle {
            return Err("专项自有像素被其他窗口覆盖；停止读取".into());
        }
        DwmFlush()?;
        if WindowFromPoint(global) != handle {
            return Err("专项 DWM 同步后自有像素覆盖改变；停止读取".into());
        }
        let dc = GetDC(None);
        if dc.is_invalid() {
            return Err("专项自有合成点的屏幕 DC 获取失败".into());
        }
        let color = GetPixel(dc, global.x, global.y);
        let released = ReleaseDC(None, dc);
        if color.0 == u32::MAX || released != 1 {
            return Err("专项自有 GetPixel/ReleaseDC 失败".into());
        }
        if GetForegroundWindow() != handle || WindowFromPoint(global) != handle {
            return Err("专项像素读取期间自有覆盖改变".into());
        }
        Ok([(color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8])
    }
}
async fn verify(
    view: WeakEntity<CaptureView>,
    input: OwnedInput,
    log: &Log,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(input.check()?)?;
    let base = view
        .update(cx, |v, cx| {
            let result = install(v, input.input.scale);
            cx.notify();
            result
        })
        .map_err(|e| e.to_string())??;
    let annotations = view
        .update(cx, |v, _| v.editor.document_annotations())
        .map_err(|e| e.to_string())?;
    // GPUI 按 DOWN 自行累计 click_count；每次等待真实系统双击间隔，不伪造 DBLCLK。
    let pace = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64 + 50);
    let left = base.x as i32;
    let top = base.y as i32;
    let right = (base.x + base.width) as i32;
    let bottom = (base.y + base.height) as i32;
    let middle = (left + base.width as i32 / 2, top + base.height as i32 / 2);
    let gap = (80. * input.input.scale).round() as i32;
    let points = [
        (left - gap, top - gap),
        (middle.0, top - gap),
        (right + gap, top - gap),
        (left - gap, middle.1),
        (right + gap, middle.1),
        (left - gap, bottom + gap),
        (middle.0, bottom + gap),
        (right + gap, bottom + gap),
    ];
    for (index, point) in points.into_iter().enumerate() {
        reset(&view, base, cx).await?;
        pause(cx, pace).await;
        drag(&view, input, point, point, cx).await?;
        let expected = Rect::drag(
            (left.min(point.0), top.min(point.1)),
            (right.max(point.0), bottom.max(point.1)),
            view.update(cx, |v, _| v.source.bounds.width)
                .map_err(|e| e.to_string())?,
            view.update(cx, |v, _| v.source.bounds.height)
                .map_err(|e| e.to_string())?,
        );
        settled(&view, expected, false, cx).await?;
        view.update(cx, |v, _| -> Result<()> {
            if v.toolbar_monitor != v.source.monitor_at(point)
                || v.editor.document_annotations() != annotations
            {
                return Err("外格点击改变标注或未锚定结束显示器".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        record(
            log,
            serde_json::json!({"test":"outside_cell_click", "index":index,
            "ok":true, "crop":coordinates(expected)}),
        )?;
    }
    for (name, from, to, expected) in [
        (
            "edge_drag",
            (left, middle.1),
            (left + gap / 2, middle.1),
            Rect {
                x: (left + gap / 2) as u32,
                width: base.width - gap as u32 / 2,
                ..base
            },
        ),
        (
            "outside_shrink",
            (left - gap, middle.1),
            (middle.0, middle.1),
            Rect {
                x: middle.0 as u32,
                width: (right - middle.0) as u32,
                ..base
            },
        ),
        (
            "outside_cross_opposite",
            (left - gap, top - gap),
            (right + gap, bottom + gap),
            Rect {
                x: right as u32,
                y: bottom as u32,
                width: gap as u32,
                height: gap as u32,
            },
        ),
    ] {
        reset(&view, base, cx).await?;
        pause(cx, pace).await;
        drag(&view, input, from, to, cx).await?;
        settled(&view, expected, false, cx).await?;
        record(
            log,
            serde_json::json!({"test":name,"ok":true,"crop":coordinates(expected)}),
        )?;
    }
    reset(&view, base, cx).await?;
    let revision = view
        .update(cx, |v, cx| {
            let mask = Polygon::new(&[
                Point {
                    x: left as f32,
                    y: top as f32,
                },
                Point {
                    x: right as f32,
                    y: top as f32,
                },
                Point {
                    x: right as f32,
                    y: bottom as f32,
                },
                Point {
                    x: left as f32,
                    y: bottom as f32,
                },
            ])
            .expect("valid fixture polygon");
            v.editor.document.set_crop_mask(Some(mask));
            cx.notify();
            v.editor.document.revision()
        })
        .map_err(|e| e.to_string())?;
    pause(cx, pace).await;
    drag(&view, input, middle, middle, cx).await?;
    settled(&view, base, false, cx).await?;
    view.update(cx, |v, _| -> Result<()> {
        if v.editor.document.revision() != revision || v.editor.document.crop_mask.is_none() {
            return Err("无变化点击丢弃原 mask/history".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pause(cx, pace).await;
    input.send(WM_LBUTTONDOWN, points[0], true)?;
    input.send(WM_MOUSEMOVE, points[0], true)?;
    let expanded = Rect {
        x: points[0].0 as u32,
        y: points[0].1 as u32,
        width: (right - points[0].0) as u32,
        height: (bottom - points[0].1) as u32,
    };
    settled(&view, expanded, true, cx).await?;
    input.escape()?;
    settled(&view, base, false, cx).await?;
    input.send(WM_LBUTTONUP, points[0], false)?;
    pause(cx, Duration::from_millis(120)).await;
    view.update(cx, |v, _| -> Result<()> {
        if v.editor.crop != Some(base)
            || v.editor.document.revision() != revision
            || v.editor.document.crop_mask.is_none()
            || v.editor.document_annotations() != annotations
        {
            return Err("取消外格拖动未恢复原 crop/mask/标注".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    record(
        log,
        serde_json::json!({"test":"cancel_nochange_mask_history","ok":true}),
    )?;

    reset(&view, base, cx).await?;
    let (other, endpoint) = view
        .update(cx, |v, _| -> Result<_> {
            let original = v.source.monitor_at(middle);
            let other = (0..v.source.screen_count())
                .find(|&index| index != original)
                .ok_or("专项需要两个显示器，以真实验证结束锚点与闲置跨屏悬停")?;
            let rect = v.source.screen_rect(other);
            Ok((
                other,
                (
                    rect.x as i32 + rect.width as i32 / 2,
                    rect.y as i32 + rect.height as i32 / 2,
                ),
            ))
        })
        .map_err(|e| e.to_string())??;
    pause(cx, pace).await;
    drag(&view, input, points[4], endpoint, cx).await?;
    let expected = view
        .update(cx, |v, _| {
            Rect::drag(
                (left, top),
                (endpoint.0, bottom),
                v.source.bounds.width,
                v.source.bounds.height,
            )
        })
        .map_err(|e| e.to_string())?;
    settled(&view, expected, false, cx).await?;
    view.update(cx, |v, cx| -> Result<()> {
        if v.toolbar_monitor != other
            || v.editor.document_annotations() != annotations
            || v.editor.document.crop_mask.is_some()
            || v.editor.lasso_draft.is_some()
        {
            return Err("跨屏拖动未使用结束显示器或移动标注".into());
        }
        v.toolbar_position = None;
        v.ui_positions.borrow_mut().clear();
        cx.notify();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pause(cx, Duration::from_millis(240)).await;
    let toolbar = view
        .update(cx, |v, _| -> Result<_> {
            painted_toolbar(v, input.input.scale)
        })
        .map_err(|e| e.to_string())??;
    input.send(WM_MOUSEMOVE, middle, false)?;
    pause(cx, Duration::from_millis(240)).await;
    view.update(cx, |v, _| -> Result<()> {
        let actual = painted_toolbar(v, input.input.scale)?;
        let monitor = v.source.screen_rect(other);
        for rect in [actual.primary, actual.properties] {
            if rect.x * input.input.scale < monitor.x as f32 - 1.
                || rect.y * input.input.scale < monitor.y as f32 - 1.
                || rect.right() * input.input.scale > (monitor.x + monitor.width) as f32 + 1.
                || rect.bottom() * input.input.scale > (monitor.y + monitor.height) as f32 + 1.
            {
                return Err("工具栏几何超出结束显示器".into());
            }
        }
        if v.toolbar_monitor != other
            || actual.primary != toolbar.primary
            || actual.properties != toolbar.properties
        {
            return Err("闲置跨屏悬停改变工具栏锚点/位置".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    record(
        log,
        serde_json::json!({"test":"endpoint_monitor_anchor_idle_hover_geometry","ok":true,"monitor":other}),
    )?;

    // 将合成栏置于导出 crop 内：GPUI 应画出栏，导出仍只含白底与全屏标注。
    let (annotation_probe, chrome_probe, expected_pixels) = view
        .update(cx, |v, cx| -> Result<_> {
            let monitor = v.source.screen_rect(v.source.monitor_at(middle));
            v.editor.set_crop(Some(monitor));
            v.toolbar_monitor = v.source.monitor_at(middle);
            v.toolbar_position = Some((
                monitor.x as f32 / input.input.scale + 80.,
                (monitor.y + monitor.height) as f32 / input.input.scale - 150.,
            ));
            let scene = v.prepared_scene();
            let mut exported = v.source.read_region(monitor)?;
            scene.try_apply(&mut exported, monitor)?;
            let oracle = Scene::default().updated(&annotations, &v.source_pixels());
            let mut expected = v.source.read_region(monitor)?;
            oracle.try_apply(&mut expected, monitor)?;
            if exported.bgra != expected.bgra || v.editor.document_annotations() != annotations {
                return Err("扩展后 Scene/导出改变全屏标注内容".into());
            }
            let mut encoded = Vec::new();
            crate::export::write_png_pixels(
                exported.width,
                exported.height,
                &exported.bgra,
                &mut encoded,
                png::Compression::Fast,
            )?;
            let decoded = image::load_from_memory(&encoded)?.into_rgba8();
            if decoded.width() != exported.width
                || decoded.height() != exported.height
                || decoded
                    .as_raw()
                    .chunks_exact(4)
                    .zip(exported.bgra.chunks_exact(4))
                    .any(|(a, b)| a != [b[2], b[1], b[0], b[3]])
            {
                return Err("内存 PNG 导出与 Scene 不一致".into());
            }
            let layout = v.toolbar_layout(monitor, input.input.scale);
            let chrome = (
                ((layout.primary.right() - 4.) * input.input.scale).round() as i32,
                ((layout.primary.y + layout.primary.height / 2.) * input.input.scale).round()
                    as i32,
            );
            let mark = (
                left + (60. * input.input.scale).round() as i32,
                top + (60. * input.input.scale).round() as i32,
            );
            let pixel = |p: (i32, i32)| -> Result<[u8; 3]> {
                if !monitor.contains(p) {
                    return Err("专项像素不在合成导出范围".into());
                }
                let index = (((p.1 as u32 - monitor.y) * monitor.width + p.0 as u32 - monitor.x)
                    * 4) as usize;
                Ok(exported.bgra[index..index + 3].try_into()?)
            };
            let pixels = [pixel(mark)?, pixel(chrome)?];
            if pixels[0] != [0x70, 0x40, 0xe0] || pixels[1] != [255; 3] {
                return Err("合成标注 oracle/导出排除栏失败".into());
            }
            cx.notify();
            Ok((mark, chrome, pixels))
        })
        .map_err(|e| e.to_string())??;
    pause(cx, Duration::from_millis(360)).await;
    view.update(cx, |v, _| painted_toolbar(v, input.input.scale))
        .map_err(|e| e.to_string())??;
    let actual_mark = native_color(input, annotation_probe)?;
    let actual_chrome = native_color(input, chrome_probe)?;
    if actual_mark
        .iter()
        .zip(expected_pixels[0])
        .any(|(a, b)| a.abs_diff(b) > 5)
        || actual_chrome.iter().all(|&value| value > 240)
    {
        return Err("真实 GPUI 标注/栏像素不符或栏未显示".into());
    }
    record(
        log,
        serde_json::json!({"test":"global_annotation_native_scene_png_export_chrome_excluded","ok":true,
        "png_memory_only":true,"native_pixel_samples":2,"exported_files":0}),
    )?;
    Ok(())
}
// 仅 RUSTSHOT_QA_CROP_CHROME=1 调用；原外八格专项默认路径保持原文。
mod crop_chrome_qa {
    use super::*;
    use gpui::AnyWindowHandle;
    use windows::{
        core::w,
        Win32::{
            Foundation::{HANDLE, RECT},
            System::Threading::GetCurrentThreadId,
            UI::{
                HiDpi::*,
                Input::KeyboardAndMouse::{
                    GetActiveWindow, GetCapture, GetFocus, GetLastInputInfo, LASTINPUTINFO,
                },
            },
        },
    };
    const PROPERTY: windows::core::PCWSTR = w!("RustShot.QaCropChrome.Owner");
    const BLUE: [u8; 3] = [0xd4, 0x78, 0];
    fn need(ok: bool, error: &str) -> Result<()> {
        if ok {
            Ok(())
        } else {
            Err(error.into())
        }
    }
    fn tick() -> Result<u32> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        need(
            unsafe { GetLastInputInfo(&mut info) }.as_bool(),
            "chrome LASTINPUTINFO查询失败",
        )?;
        Ok(info.dwTime)
    }
    struct Owner {
        input: OwnedInput,
        token: Rc<u8>,
        baseline: u32,
        class: Vec<u16>,
        removed: bool,
    }
    impl Owner {
        fn new(input: OwnedInput) -> Result<Self> {
            let handle = input.check()?;
            let baseline = std::env::var("RUSTSHOT_SMOKE_QA_INPUT_TICK")?.parse::<u32>()?;
            need(tick()? == baseline, "chrome原创建前输入基线已变")?;
            need(
                unsafe { GetPropW(handle, PROPERTY) }.0.is_null(),
                "chrome不会覆盖既有owner property",
            )?;
            let mut class = vec![0_u16; 256];
            let count = unsafe { GetClassNameW(handle, &mut class) };
            need(count > 0, "chrome自身class查询失败")?;
            class.truncate(count as usize);
            let token = Rc::new(0_u8);
            unsafe {
                SetPropW(handle, PROPERTY, Some(HANDLE(Rc::as_ptr(&token) as *mut _)))?;
            }
            Ok(Self {
                input,
                token,
                baseline,
                class,
                removed: false,
            })
        }
        fn check(&self) -> Result<HWND> {
            let handle = self.input.check()?;
            need(
                unsafe { IsWindow(Some(handle)) }.as_bool()
                    && unsafe { GetCurrentThreadId() } == self.input.tid,
                "chrome原自身 HWND/GPUI线程失效",
            )?;
            need(tick()? == self.baseline, "chrome真实输入已变；不重设原基线")?;
            need(
                unsafe { GetPropW(handle, PROPERTY) }.0 == Rc::as_ptr(&self.token) as *mut _,
                "chrome nonce已变",
            )?;
            let mut class = [0_u16; 256];
            let count = unsafe { GetClassNameW(handle, &mut class) };
            need(
                count > 0 && class[..count as usize] == self.class,
                "chrome原自身class已变",
            )?;
            need(
                unsafe { IsWindowVisible(handle) }.as_bool()
                    && unsafe { GetForegroundWindow() } == handle
                    && unsafe { GetActiveWindow() } == handle
                    && unsafe { GetFocus() } == handle,
                "chrome原自身实际visible/foreground/active/focus门失败",
            )?;
            let mut gui = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                GetGUIThreadInfo(self.input.tid, &mut gui)?;
            }
            need(
                gui.hwndActive == handle
                    && gui.hwndFocus == handle
                    && gui.hwndCapture.is_invalid()
                    && unsafe { GetCapture() }.is_invalid(),
                "chrome自身GUI状态/capture未收敛",
            )?;
            Ok(handle)
        }
        fn finish(&mut self) -> Result<()> {
            let handle = self.check()?;
            let removed = unsafe { RemovePropW(handle, PROPERTY) }?;
            need(
                removed.0 == Rc::as_ptr(&self.token) as *mut _,
                "chrome nonce checked移除失败",
            )?;
            self.removed = true;
            Ok(())
        }
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            if !self.removed {
                if let Ok(handle) = self.input.check() {
                    if unsafe { GetPropW(handle, PROPERTY) }.0 == Rc::as_ptr(&self.token) as *mut _
                    {
                        unsafe {
                            let _ = RemovePropW(handle, PROPERTY);
                        }
                    }
                }
            }
        }
    }
    #[derive(Clone, Debug, PartialEq, serde::Serialize)]
    struct WindowState {
        pid: u32,
        tid: u32,
        nonce: usize,
        class: Vec<u16>,
        scale_bits: u32,
        outer: [i32; 4],
        client: [i32; 4],
        origin: [i32; 2],
        physical: [u32; 2],
        pmv2: bool,
    }
    fn mapping(owner: &Owner, window: &Window, bounds: ScreenRect) -> Result<WindowState> {
        let handle = owner.check()?;
        need(hwnd(window) == handle, "chrome WindowHandle不是原自身HWND")?;
        let pmv2 = unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        }
        .as_bool();
        need(pmv2, "chrome物理几何要求原GPUI线程PMv2；不改DPI context")?;
        let mut outer = RECT::default();
        unsafe {
            GetWindowRect(handle, &mut outer)?;
        }
        owner.check()?;
        let mut client = RECT::default();
        unsafe {
            GetClientRect(handle, &mut client)?;
        }
        owner.check()?;
        let mut origin = POINT::default();
        need(
            unsafe { ClientToScreen(handle, &mut origin) }.as_bool(),
            "chrome自身client原点查询失败",
        )?;
        owner.check()?;
        let scale = window.scale_factor();
        let viewport = window.viewport_size();
        let physical = [
            f64::from(f32::from(viewport.width)) * f64::from(scale),
            f64::from(f32::from(viewport.height)) * f64::from(scale),
        ];
        need(
            scale.is_finite()
                && scale > 0.
                && scale.to_bits() == owner.input.input.scale.to_bits()
                && client.left == 0
                && client.top == 0
                && i64::from(client.right) == i64::from(bounds.width)
                && i64::from(client.bottom) == i64::from(bounds.height)
                && origin.x == bounds.left
                && origin.y == bounds.top
                && physical[0].round() == f64::from(bounds.width)
                && physical[1].round() == f64::from(bounds.height),
            "chrome原自身物理client/viewport/origin/scale不符",
        )?;
        Ok(WindowState {
            pid: std::process::id(),
            tid: owner.input.tid,
            nonce: Rc::as_ptr(&owner.token) as usize,
            class: owner.class.clone(),
            scale_bits: scale.to_bits(),
            outer: [outer.left, outer.top, outer.right, outer.bottom],
            client: [client.left, client.top, client.right, client.bottom],
            origin: [origin.x, origin.y],
            physical: [bounds.width, bounds.height],
            pmv2,
        })
    }
    #[derive(Clone, Debug, PartialEq)]
    struct Model {
        bounds: ScreenRect,
        current: usize,
        previews: Vec<usize>,
        stamp: (u64, u64),
        crop: Option<Rect>,
        pointer: (i32, i32),
        hover: Option<Rect>,
        zoom: f32,
        offset: Point,
    }
    fn model(view: &CaptureView) -> Model {
        let viewport = view
            .image_canvas
            .as_ref()
            .map(|v| v.viewport)
            .unwrap_or_default();
        Model {
            bounds: view.source.bounds,
            current: view.source.current,
            previews: view
                .previews
                .iter()
                .map(|p| Arc::as_ptr(p) as usize)
                .collect(),
            stamp: view.editor.scene_stamp(),
            crop: view.editor.crop,
            pointer: view.pointer,
            hover: view.hover_selection().map(|r| r.rect),
            zoom: viewport.zoom,
            offset: viewport.offset,
        }
    }
    struct Pixels(Vec<u8>);
    impl Drop for Pixels {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }
    async fn outside_pointer(
        view: &WeakEntity<CaptureView>,
        input: OwnedInput,
        physical: (i32, i32),
        expected: (i32, i32),
        cx: &mut AsyncApp,
    ) -> Result<()> {
        input.send(WM_MOUSEMOVE, physical, false)?;
        for _ in 0..80 {
            if view
                .update(cx, |v, _| {
                    v.pointer == expected
                        && v.pointer_down_start.is_none()
                        && !v.editor.is_dragging()
                })
                .map_err(|e| e.to_string())?
            {
                pause(cx, Duration::from_millis(120)).await;
                return Ok(());
            }
            pause(cx, Duration::from_millis(25)).await;
        }
        Err("chrome自身WM pointer未收敛到ROI外指定坐标".into())
    }
    // 独立固定规格；不调用 selection_chrome 或任何生产expected geometry函数。
    fn handles(rect: Rect) -> [(u32, u32); 8] {
        let (l, t, r, b) = (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height);
        let (mx, my) = (l + rect.width / 2, t + rect.height / 2);
        [
            (l - 6, t - 6),
            (mx - 3, t - 6),
            (r, t - 6),
            (l - 6, my - 3),
            (r, my - 3),
            (l - 6, b),
            (mx - 3, b),
            (r, b),
        ]
    }
    fn oracle(
        bytes: &[u8],
        roi: [u32; 4],
        crop: Rect,
        confirmed: bool,
    ) -> Result<(usize, usize, usize)> {
        let pixel = |x: u32, y: u32, expected: [u8; 3]| -> Result<()> {
            let index = (((y - roi[1]) * roi[2] + x - roi[0]) * 4) as usize;
            let actual = bytes.get(index..index + 4).ok_or("chrome ROI索引越界")?;
            need(actual[3] == 255 && actual[..3].iter().zip(expected).all(|(a,b)| a.abs_diff(b) <= 1),
                &format!("chrome自有framebuffer不符：client({x},{y}) expected={expected:?} actual={actual:?}"))
        };
        let (l, t, r, b) = (crop.x, crop.y, crop.x + crop.width, crop.y + crop.height);
        for y in t..b {
            for x in l..r {
                pixel(x, y, [255; 3])?;
            }
        }
        let boxes = handles(crop);
        let in_handle = |x, y| {
            confirmed
                && boxes
                    .iter()
                    .any(|&(hx, hy)| x >= hx && x < hx + 6 && y >= hy && y < hy + 6)
        };
        let mut frame_pixels = 0;
        for x in l - 1..=r {
            for y in [t - 1, b] {
                if !in_handle(x, y) {
                    pixel(x, y, BLUE)?;
                    frame_pixels += 1;
                }
            }
        }
        for y in t..b {
            for x in [l - 1, r] {
                if !in_handle(x, y) {
                    pixel(x, y, BLUE)?;
                    frame_pixels += 1;
                }
            }
        }
        let mut handle_pixels = 0;
        if confirmed {
            for (hx, hy) in boxes {
                for dy in 0..6 {
                    for dx in 0..6 {
                        pixel(
                            hx + dx,
                            hy + dy,
                            if dx == 0 || dx == 5 || dy == 0 || dy == 5 {
                                BLUE
                            } else {
                                [255; 3]
                            },
                        )?;
                        handle_pixels += 1;
                    }
                }
            }
        } else {
            // hover仅细外框，不绘制八个确认控制点。
            for (hx, hy) in boxes {
                pixel(hx + 2, hy + 2, [136; 3])?;
            }
        }
        // 上方尺寸标签是独立 chrome，不将它的像素误当作遮罩；仍完整核所有上框/上handle。
        for (x, y) in [
            (l - 8, t + crop.height / 3),
            (r + 7, t + crop.height / 3),
            (l - 8, t + crop.height * 2 / 3),
            (l + crop.width * 3 / 4, b + 7),
        ] {
            pixel(x, y, [136; 3])?;
        }
        Ok((
            (crop.width * crop.height) as usize,
            frame_pixels,
            handle_pixels,
        ))
    }
    async fn capture(
        view: &WeakEntity<CaptureView>,
        own: AnyWindowHandle,
        owner: &Owner,
        log: &Log,
        name: &str,
        sample: (Rect, bool),
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (crop, confirmed) = sample;
        let expected = view
            .update(cx, |v, _| -> Result<Model> {
                need(
                    v.editor.tool == Tool::Crop
                        && !v.pin_mode
                        && !v.editor.is_dragging()
                        && v.editor.document_annotations().is_empty(),
                    "chrome fixture并非空白/已收敛Crop模型",
                )?;
                need(
                    if confirmed {
                        v.editor.crop.is_some()
                    } else {
                        v.editor.crop.is_none() && v.hover_selection().map(|h| h.rect) == Some(crop)
                    },
                    "chrome原crop/hover状态不符",
                )?;
                Ok(model(v))
            })
            .map_err(|e| e.to_string())??;
        let baseline = own
            .update(cx, |_, w, _| mapping(owner, w, expected.bounds))
            .map_err(|e| e.to_string())??;
        let roi = [
            crop.x.checked_sub(8).ok_or("chrome左边距不足")?,
            crop.y.checked_sub(8).ok_or("chrome上边距不足")?,
            crop.width.checked_add(16).ok_or("chrome ROI宽溢出")?,
            crop.height.checked_add(16).ok_or("chrome ROI高溢出")?,
        ];
        need(
            (Rect {
                x: roi[0],
                y: roi[1],
                width: roi[2],
                height: roi[3],
            })
            .valid(baseline.physical[0], baseline.physical[1]),
            "chrome ROI未全落自身client",
        )?;
        let byte_count = roi[2] as usize * roi[3] as usize * 4;
        let physical_pointer = [
            expected.offset.x + expected.pointer.0 as f32 * expected.zoom,
            expected.offset.y + expected.pointer.1 as f32 * expected.zoom,
        ];
        let pointer_outside_roi = physical_pointer[0] < roi[0] as f32
            || physical_pointer[0] >= (roi[0] + roi[2]) as f32
            || physical_pointer[1] < roi[1] as f32
            || physical_pointer[1] >= (roi[1] + roi[3]) as f32;
        need(
            !confirmed || pointer_outside_roi,
            "chrome确认状态pointer必须在ROI外，不能混入Move光标overlay",
        )?;
        owner.check()?;
        unsafe {
            DwmFlush()?;
        }
        owner.check()?;
        own.update(cx, |_, w, _| -> Result<()> {
            need(
                mapping(owner, w, expected.bounds)? == baseline,
                "chrome request前geometry改变",
            )?;
            w.request_frame_capture(roi)?;
            need(
                mapping(owner, w, expected.bounds)? == baseline,
                "chrome request后geometry改变",
            )
        })
        .map_err(|e| e.to_string())??;
        for poll in 1..=80_u32 {
            pause(cx, Duration::from_millis(25)).await;
            need(
                view.update(cx, |v, _| model(v))
                    .map_err(|e| e.to_string())?
                    == expected,
                "chrome等待期间原source/scene/crop/pointer/viewport改变",
            )?;
            let captured = own
                .update(cx, |_, w, _| -> Result<_> {
                    need(
                        mapping(owner, w, expected.bounds)? == baseline,
                        "chrome take前geometry改变",
                    )?;
                    let result = w.take_frame_capture();
                    need(
                        mapping(owner, w, expected.bounds)? == baseline,
                        "chrome take后geometry改变",
                    )?;
                    Ok(result)
                })
                .map_err(|e| e.to_string())??;
            if let Some(captured) = captured {
                let bytes = Pixels(captured?);
                need(bytes.0.len() == byte_count, "chrome selfBB紧密BGRA长度不符")?;
                let (content_pixels, frame_pixels, handle_pixels) =
                    oracle(&bytes.0, roi, crop, confirmed)?;
                record(
                    log,
                    serde_json::json!({"test":name,"ok":true,"owner_hwnd":owner.input.input.handle,
                    "owner":baseline,"roi":roi,"projected_crop":coordinates(crop),"source_crop":expected.crop.map(coordinates),
                    "source_bounds":[expected.bounds.left,expected.bounds.top,expected.bounds.width,expected.bounds.height],
                    "source_current":expected.current,"scene_stamp":expected.stamp,
                    "physical_pointer":physical_pointer,"pointer_outside_roi":pointer_outside_roi,
                    "zoom":expected.zoom,"offset":[expected.offset.x,expected.offset.y],"frame_width_physical":1,"handle_size_physical":6,
                    "chrome_rgb":0x0078d4_u32,"handle_fill_rgb":0xffffff_u32,"content_pixels":content_pixels,"frame_pixels":frame_pixels,
                    "handle_pixels":handle_pixels,"outside_mask_probes":4,"request_count":1,"completion_poll":poll,
                    "confirmed":confirmed,"source_scene_crop_viewport_stable":true,"source_preview_arc_ids":expected.previews,
                    "color_tolerance":1,"self_framebuffer_actual":true,"desktop_rgb_read":false,"saved_images":0}),
                )?;
                return Ok(());
            }
        }
        Err("chrome单次selfBB请求2秒内未完成；不按颜色重试".into())
    }
    async fn verify(
        view: WeakEntity<CaptureView>,
        own: AnyWindowHandle,
        input: OwnedInput,
        log: &Log,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        gpui_smoke::isolate_input(input.check()?)?;
        let mut owner = Owner::new(input)?;
        let base = view
            .update(cx, |v, cx| -> Result<Rect> {
                let mut base = install(v, input.input.scale)?;
                base.width = 256;
                base.height = 192;
                v.editor = Editor::new(v.source.full(), None);
                v.control_hover = None;
                let bounds = v.source.bounds;
                v.window_targets
                    .push(crate::selection_assist::WindowTarget {
                        rect: base,
                        global_bounds: RECT {
                            left: bounds.left + base.x as i32,
                            top: bounds.top + base.y as i32,
                            right: bounds.left + (base.x + base.width) as i32,
                            bottom: bounds.top + (base.y + base.height) as i32,
                        },
                        title: "QA自有缓存目标；非UIA provider验证".into(),
                        hwnd: input.input.handle,
                        process_id: std::process::id(),
                        thread_id: input.tid,
                    });
                v.auto_adsorb = true;
                cx.notify();
                Ok(base)
            })
            .map_err(|e| e.to_string())??;
        let center = (
            base.x as i32 + base.width as i32 / 2,
            base.y as i32 + base.height as i32 / 2,
        );
        input.send(WM_MOUSEMOVE, center, false)?;
        for _ in 0..80 {
            if view
                .update(cx, |v, _| {
                    v.pointer == center && v.hover_selection().map(|h| h.rect) == Some(base)
                })
                .map_err(|e| e.to_string())?
            {
                break;
            }
            pause(cx, Duration::from_millis(25)).await;
        }
        capture(
            &view,
            own,
            &owner,
            log,
            "hover_outer_frame_interior_clear_outside_dim",
            (base, false),
            cx,
        )
        .await?;
        view.update(cx, |v, cx| {
            v.editor.set_crop(Some(base));
            v.window_targets.clear();
            v.control_hover = None;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        settled(&view, base, false, cx).await?;
        let outside = (base.x as i32 - 32, base.y as i32 + 220);
        outside_pointer(&view, input, outside, outside, cx).await?;
        capture(
            &view,
            own,
            &owner,
            log,
            "confirmed_outer_frame_eight_handles_content_clear",
            (base, true),
            cx,
        )
        .await?;
        view.update(cx, |v, cx| {
            v.cancel_pointer();
            v.editor.set_crop(None);
            v.auto_adsorb = false;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        drag(
            &view,
            input,
            ((base.x + base.width) as i32, (base.y + base.height) as i32),
            (base.x as i32, base.y as i32),
            cx,
        )
        .await?;
        settled(&view, base, false, cx).await?;
        // 自有消息把画布pointer移到ROI外；系统鼠标保持原位，Move自绘箭头不混入oracle。
        outside_pointer(&view, input, outside, outside, cx).await?;
        capture(
            &view,
            own,
            &owner,
            log,
            "reverse_drag_same_physical_crop_chrome",
            (base, true),
            cx,
        )
        .await?;
        view.update(cx, |v, cx| {
            v.image_canvas = Some(crate::image_editor::ImageCanvas {
                path: None,
                viewport: crate::viewport::Viewport {
                    zoom: 1.5,
                    offset: Point {
                        x: -0.5 * base.x as f32,
                        y: -0.5 * base.y as f32,
                    },
                },
                auto_fit: false,
                host: (v.source.bounds.width, v.source.bounds.height),
                pan: None,
            });
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        outside_pointer(
            &view,
            input,
            (base.x as i32 - 48, base.y as i32 + 330),
            outside,
            cx,
        )
        .await?;
        let projected = Rect {
            x: base.x,
            y: base.y,
            width: 384,
            height: 288,
        };
        capture(
            &view,
            own,
            &owner,
            log,
            "zoom_1_5_fixed_physical_chrome",
            (projected, true),
            cx,
        )
        .await?;
        owner.finish()?;
        Ok(())
    }
    pub(super) fn schedule(
        view: &Entity<CaptureView>,
        window: &mut Window,
        errors: Rc<RefCell<Option<String>>>,
    ) {
        *errors.borrow_mut() = Some("crop chrome专项未完成".into());
        let weak = view.downgrade();
        let own = window.window_handle();
        let input = Input {
            handle: hwnd(window).0 as usize,
            scale: window.scale_factor(),
        };
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(HWND(input.handle as *mut _), Some(&mut pid)) };
        let prepared = (|| -> Result<_> {
            need(
                pid == std::process::id() && tid > 0,
                "chrome初始窗口非本进程",
            )?;
            let folder = PathBuf::from("target/qa/crop-chrome");
            std::fs::create_dir_all(&folder)?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = folder.join(format!("{}-{nonce}.jsonl", std::process::id()));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            Ok((Rc::new(RefCell::new(file)), path))
        })();
        window.on_next_frame(move|_,cx| {
            let (log,path)=match prepared {Ok(v)=>v,Err(e)=>{*errors.borrow_mut()=Some(e.to_string());cx.quit();return;}};
            let done=Rc::new(Cell::new(false));let watchdog=done.clone();let timeout_errors=errors.clone();let timeout_log=log.clone();
            cx.spawn(async move|cx| {pause(cx,Duration::from_secs(90)).await;if !watchdog.replace(true) {
                *timeout_errors.borrow_mut()=Some("crop chrome专项90秒超时".into());
                let _=record(&timeout_log,serde_json::json!({"phase":"summary","ok":false,"error":"timeout"}));let _=cx.update(|cx|cx.quit());
            }}).detach();
            cx.spawn(async move|cx| {
                let result=verify(weak,own,OwnedInput{input,tid},&log,cx).await;if done.replace(true) {return;}
                let mut error=result.err().map(|e|e.to_string());
                if let Err(e)=record(&log,serde_json::json!({"phase":"summary","ok":error.is_none(),"error":error,"pid":std::process::id(),"tid":tid,
                    "checks":4,"qa_flag":"RUSTSHOT_QA_CROP_CHROME=1","input":"owned PostMessage SMOKE_MOVE; no global input",
                    "source":"synthetic white; actual monitor topology","self_framebuffer_actual":error.is_none(),"owned_screen_pixel_reads":0,
                    "saved_images":0,"clipboard_accessed":false,"export_performed":false,"performance_acceptance":false,
                    "fixed_original_input_tick_preserved":true,"system_resize_cursor_actual_verified":false,"early_native_chrome_actual_verified":false,
                    "hover_scope":"cached own synthetic target; no UIA provider query","zoom_scope":"QA imageCanvas viewport; real image open flow not exercised"})) {
                    error=Some(format!("chrome JSONL写入失败：{e}；原错误：{error:?}"));
                }
                println!("{}",serde_json::json!({"phase":"crop_chrome_artifact","path":path,"ok":error.is_none()}));
                *errors.borrow_mut()=error;let _=cx.update(|cx|cx.quit());
            }).detach();
        });
    }
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    if std::env::var("RUSTSHOT_QA_CROP_CHROME").as_deref() == Ok("1") {
        crop_chrome_qa::schedule(view, window, errors);
        return;
    }
    *errors.borrow_mut() = Some("外八格专项未完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let mut pid = 0;
    let tid = unsafe { GetWindowThreadProcessId(HWND(input.handle as *mut _), Some(&mut pid)) };
    let prepared = (|| -> Result<_> {
        if pid != std::process::id() || tid == 0 {
            return Err("外八格专项初始窗口不是本进程".into());
        }
        let folder = PathBuf::from("target/qa/crop-expand");
        std::fs::create_dir_all(&folder)?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = folder.join(format!("{}-{nonce}.jsonl", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((Rc::new(RefCell::new(file)), path))
    })();
    window.on_next_frame(move|_,cx| {
        let (log,path)=match prepared {Ok(value)=>value,Err(error)=>{*errors.borrow_mut()=Some(error.to_string());cx.quit();return;}};
        let done=Rc::new(Cell::new(false));let watchdog=done.clone();
        let timeout_errors=errors.clone();let timeout_log=log.clone();
        cx.spawn(async move|cx| {
            pause(cx,Duration::from_secs(180)).await;
            if !watchdog.replace(true) {
                *timeout_errors.borrow_mut()=Some("外八格专项180秒超时".into());
                let _=record(&timeout_log,serde_json::json!({"phase":"summary","ok":false,"error":"timeout"}));
                let _=cx.update(|cx|cx.quit());
            }
        }).detach();
        cx.spawn(async move|cx| {
            let input=OwnedInput {input,tid};
            let result=verify(weak,input,&log,cx).await;
            if done.replace(true) {return;}
            let mut error=result.err().map(|e|e.to_string());
            if let Err(write_error)=record(&log,serde_json::json!({"phase":"summary","ok":error.is_none(),
                "error":error,"pid":std::process::id(),"tid":tid,
                "input":"owned PostMessage SMOKE_MOVE; no global input",
                "source":"synthetic white; actual monitor topology",
                "export":"same production Scene + PNG codec in memory; preferences/save/clipboard not invoked",
                "live_desktop_image_used":false,"owned_screen_pixel_reads":2,"saved_images":0,"performance_acceptance":false})) {
                error=Some(format!("专项JSONL写入失败：{write_error}；原错误：{error:?}"));
            }
            println!("{}",serde_json::json!({"phase":"crop_expand_artifact","path":path,"ok":error.is_none()}));
            *errors.borrow_mut()=error;let _=cx.update(|cx|cx.quit());
            if let Ok(handle)=input.check() {unsafe {let _=PostMessageW(Some(handle),WM_NULL,WPARAM(0),LPARAM(0));}}
        }).detach();
    });
}
