//! 聚光灯专项：自有白底、亮区并集、旋转边缘和截图/贴图参数入口。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture::Image,
    capture_view::CaptureView,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::PinState,
    pin_view::PinView,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

pub(crate) async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
}
fn white(width: u32, height: u32) -> Image {
    Image {
        width,
        height,
        bgra: vec![255; width as usize * height as usize * 4],
        readback_ms: 0.,
    }
}
fn hole(area: Rect, a: (f32, f32), b: (f32, f32), radius: f32) -> Annotation {
    let mut item = Annotation::new(
        Tool::Spotlight,
        Point {
            x: area.x as f32 + a.0,
            y: area.y as f32 + a.1,
        },
        Style {
            color: 0x31a3e8,
            width: 4.,
            radius,
            opacity: 0.5,
            ..Default::default()
        },
    );
    item.update(Point {
        x: area.x as f32 + b.0,
        y: area.y as f32 + b.1,
    });
    item
}
pub(crate) fn install(v: &mut CaptureView) -> Result<Rect> {
    let global = v.source.bounds;
    let monitor = v.source.screen_rect(v.source.current);
    if monitor.width < 1250 || monitor.height < 850 {
        return Err("聚光灯专项需要至少1250×850显示器".into());
    }
    let area = Rect {
        x: monitor.x + 60,
        y: monitor.y + 50,
        width: 1080,
        height: 680,
    };
    let buffer = image::RgbaImage::from_raw(
        global.width,
        global.height,
        white(global.width, global.height).bgra,
    )
    .ok_or("合成白底尺寸无效")?;
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
    v.source = source;
    v.previews = previews;
    v.image_canvas = None;
    v.editor = crate::editor::Editor::new(v.source.full(), Some(area));
    v.editor
        .document
        .insert(hole(area, (160., 140.), (440., 340.), 32.));
    v.editor
        .document
        .insert(hole(area, (360., 240.), (640., 440.), 32.));
    v.editor.select_tool(Tool::Spotlight);
    v.include_cursor = false;
    v.cursor = None;
    v.cursor_preview = None;
    v.magnifier = false;
    v.guides = false;
    v.show_guides = false;
    v.window_targets.clear();
    v.hover_window = None;
    v.toolbar_panel = ToolbarPanel::None;
    v.toolbar_position = None;
    v.toolbar_monitor = 0;
    v.finish_style = Default::default();
    v.finish_preview = None;
    v.raster_previews.clear();
    v.pointer = (area.x as i32 + 1010, area.y as i32 + 610);
    v.pointer_sample = Point::from_tuple(v.pointer);
    Ok(area)
}
fn pixel(image: &Image, p: (u32, u32)) -> [u8; 4] {
    let i = ((p.1 * image.width + p.0) * 4) as usize;
    image.bgra[i..i + 4].try_into().expect("fixture pixel")
}
pub(crate) fn native_owned_region(area: Rect) -> Result<Image> {
    // 当前静态截图也使用GDI合成色彩；避免HDR桌面复制参考的色彩转换差异混入几何验收。
    let (source, _) = CanvasSource::frozen_desktop(FrozenDesktop::capture()?);
    source.read_region(area)
}
fn pass(name: &str) {
    println!("{{\"test\":\"{name}\",\"ok\":true}}");
}
fn send_text(input: Input, value: &str) -> Result<()> {
    unsafe {
        for unit in value.encode_utf16() {
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
async fn click(
    view: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..25 {
        if let Some(p) = view
            .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
            .map_err(|e| e.to_string())?
        {
            input.click(p);
            frame(cx).await;
            return Ok(());
        }
        frame(cx).await;
    }
    Err(format!("参数入口未绘制：{id}").into())
}
async fn verify_capture(
    view: WeakEntity<CaptureView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    let area = view
        .update(cx, |v, cx| {
            let a = install(v);
            cx.notify();
            a
        })
        .map_err(|e| e.to_string())??;
    frame(cx).await;
    frame(cx).await;
    let mut exported = view
        .update(cx, |v, _| -> Result<Image> {
            let scene = v.prepared_scene();
            if !scene.errors.is_empty() {
                return Err(scene.errors.join(";").into());
            }
            let mut image = v.source.read_region(area)?;
            scene.apply(&mut image, area);
            Ok(image)
        })
        .map_err(|e| e.to_string())??;
    let native = native_owned_region(area)?;
    for (p, expected) in [
        ((80, 80), [128, 128, 128, 255]),
        ((220, 200), [255; 4]),
        ((560, 380), [255; 4]),
        ((400, 280), [255; 4]),
        ((438, 280), [255; 4]),
        ((162, 142), [128, 128, 128, 255]),
    ] {
        for (label, image) in [("预览", &native), ("导出", &exported)] {
            let actual = pixel(image, p);
            if actual
                .iter()
                .zip(expected)
                .any(|(a, b)| (*a as i16 - b as i16).abs() > 3)
            {
                return Err(format!("{label}共享亮区/圆角颜色异常：{p:?}, {actual:?}").into());
            }
        }
    }
    pass("native_spotlight_union_once_alpha_rounded_holes_no_internal_border");
    // 旋转孔洞和0边框仍使用同一矢量边界，导出只计算请求区域。
    view.update(cx, |v, cx| {
        let objects = v
            .editor
            .document
            .items()
            .iter()
            .map(|o| {
                let mut a = o.annotation.clone();
                a.style.width = 0.;
                a.rotation = 27.;
                (o.id, a)
            })
            .collect::<Vec<_>>();
        v.editor.document.replace_many(&objects);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    view.update(cx, |v, _| {
        exported = v.source.read_region(area)?;
        v.prepared_scene().apply(&mut exported, area);
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .map_err(|e| e.to_string())??;
    let rotated = native_owned_region(area)?;
    let (mut count, mut large_error, mut sum) = (0_usize, 0_usize, 0_u64);
    for y in 90..490 {
        for x in 90..720 {
            let expected = pixel(&exported, (x, y))[1];
            if expected > 132 && expected < 251 {
                count += 1;
                let error =
                    (pixel(&rotated, (x, y))[1] as i16 - expected as i16).unsigned_abs() as u64;
                sum += error;
                if error > 24 {
                    large_error += 1;
                }
            }
        }
    }
    if count < 120 || large_error * 20 > count || sum as f64 / count.max(1) as f64 > 10. {
        return Err(format!(
            "旋转抗锯齿预览/输出差异：边缘{count},大误差{large_error},总误差{sum}"
        )
        .into());
    }
    println!("{{\"test\":\"native_spotlight_rotated_preview_export_analytic_edges_zero_border\",\"edge_pixels\":{count},\"mean_error\":{:.3},\"ok\":true}}",sum as f64/count as f64);
    // 空白画布的参数菜单通过真实鼠标和文本消息操作，不用模型值伪装UI验证。
    click(&view, input, "radius-menu", cx).await?;
    frame(cx).await;
    send_text(input, "17")?;
    frame(cx).await;
    click(&view, input, "parameter-apply", cx).await?;
    if view
        .update(cx, |v, _| v.editor.style.radius)
        .map_err(|e| e.to_string())?
        != 17.
    {
        return Err("截图精确圆角输入未提交17".into());
    }
    click(&view, input, "radius-menu", cx).await?;
    frame(cx).await;
    send_text(input, "29")?;
    frame(cx).await;
    // 连续投递关闭/重开，窗口尚未绘制“无弹窗”帧时也必须丢弃旧数值草稿。
    let (cancel, reopen) = view
        .update(cx, |v, _| {
            let positions = v.ui_positions.borrow();
            (
                positions.get("parameter-cancel").copied(),
                positions.get("radius-menu").copied(),
            )
        })
        .map_err(|e| e.to_string())?;
    input.click(cancel.ok_or("取消控件没有绘制")?);
    input.click(reopen.ok_or("圆角入口没有绘制")?);
    frame(cx).await;
    if view
        .update(cx, |v, _| v.editor.style.radius)
        .map_err(|e| e.to_string())?
        != 17.
    {
        return Err("取消参数草稿修改了截图样式".into());
    }
    send_text(input, "11.5")?;
    frame(cx).await;
    click(&view, input, "parameter-apply", cx).await?;
    if view
        .update(cx, |v, _| v.editor.style.radius)
        .map_err(|e| e.to_string())?
        != 11.5
    {
        return Err("连续关闭重开的参数框复用了旧数值或丢失全文选择".into());
    }
    for id in ["width-menu", "opacity-menu"] {
        let found = view
            .update(cx, |v, _| v.ui_positions.borrow().contains_key(id))
            .map_err(|e| e.to_string())?;
        if !found {
            return Err(format!("聚光灯属性缺少{id}").into());
        }
    }
    pass("native_spotlight_capture_exact_radius_cancel_width_opacity_controls");
    Ok(())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("聚光灯专项未完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    window.on_next_frame(move |_, cx| {
        let done = Rc::new(Cell::new(false));
        let timeout_done = done.clone();
        let timeout_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_secs(40))
                .await;
            if !timeout_done.get() {
                *timeout_errors.borrow_mut() = Some("截图聚光灯专项超时".into());
                let _ = cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let result = verify_capture(weak, input, cx).await;
            done.set(true);
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
pub(crate) fn schedule_pin(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    let initial = (|| -> Result<_> {
        let work = crate::pin_native::work_area((100, 100));
        let pin = PinView::open_state(
            white(800, 500),
            (work.x + 100, work.y + 100),
            PinState::default(),
            cx,
        )?;
        let input = pin.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })?;
        gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
        Ok((pin, input))
    })();
    let (pin, input) = match initial {
        Ok(v) => v,
        Err(e) => {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
            return;
        }
    };
    let done = Rc::new(Cell::new(false));
    let watchdog = done.clone();
    let timeout_errors = errors.clone();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_secs(40))
            .await;
        if !watchdog.get() {
            *timeout_errors.borrow_mut() = Some("贴图聚光灯专项超时".into());
            let _ = cx.update(|cx| cx.quit());
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let result = if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("radius") {
            crate::radius_handle_smoke::verify_pin(pin, input, cx).await
        } else if matches!(
            std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref(),
            Ok("parameter"
                | "highlight_parameter"
                | "finish_text_parameter"
                | "radius_bulk_parameter")
        ) {
            crate::parameter_smoke::verify_pin(pin, input, cx).await
        } else {
            verify_pin(pin, input, cx).await
        };
        done.set(true);
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
}
async fn verify_pin(pin: WindowHandle<PinView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    frame(cx).await;
    input.key(0x20);
    frame(cx).await;
    frame(cx).await;
    let canvas = pin
        .update(cx, |v, _, _| v.canvas.clone())
        .map_err(|e| e.to_string())?
        .ok_or("贴图画布未创建")?;
    canvas
        .update(cx, |v, cx| {
            v.pin_editing = true;
            v.editor.select_tool(Tool::Spotlight);
            v.ui_positions.borrow_mut().clear();
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("一级栏未创建")?;
    let properties = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?
        .ok_or("二级栏未创建")?;
    let secondary = properties
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(secondary.handle as *mut _))?;
    for id in ["width-menu", "radius-menu", "opacity-menu"] {
        if !canvas
            .update(cx, |v, _| v.ui_positions.borrow().contains_key(id))
            .map_err(|e| e.to_string())?
        {
            return Err(format!("贴图二级栏缺少{id}").into());
        }
    }
    let weak = canvas.downgrade();
    click(&weak, secondary, "radius-menu", cx).await?;
    let popup = properties
        .update(cx, |v, _, _| v.popup)
        .map_err(|e| e.to_string())?
        .ok_or("贴图圆角独立参数窗未创建")?;
    let popup_input = popup
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(popup_input.handle as *mut _))?;
    frame(cx).await;
    let popup_was_foreground =
        unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) == HWND(input.handle as *mut _) };
    send_text(popup_input, "23")?;
    frame(cx).await;
    click(&weak, popup_input, "parameter-apply", cx).await?;
    if canvas
        .update(cx, |v, _| v.editor.style.radius)
        .map_err(|e| e.to_string())?
        != 23.
    {
        return Err("贴图精确圆角23未同步画布".into());
    }
    // 后台验收的合成鼠标消息不会赋予Windows前台资格；仅在本贴图曾取得前台时验恢复。
    if popup_was_foreground && unsafe { GetForegroundWindow() } != HWND(input.handle as *mut _) {
        return Err("贴图参数窗关闭后未恢复本贴图输入焦点".into());
    }
    println!("{{\"stage\":\"pin_parameter_foreground\",\"foreground_restore_verified\":{popup_was_foreground}}}");
    let current_primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("参数操作关闭了一级栏")?;
    if current_primary != primary {
        return Err("贴图参数操作重建了一级栏".into());
    }
    pass("native_spotlight_pin_two_bars_exact_radius_shared_parameter_popup");
    // 绘制两个重叠亮区并确认同一文档、共享暗层与原图保护。
    input.drag((100, 100), (400, 300));
    frame(cx).await;
    input.drag((620, 400), (300, 200));
    frame(cx).await;
    let output = canvas
        .update(cx, |v, _| v.selected_content_image())
        .map_err(|e| e.to_string())??;
    if pixel(&output, (350, 250)) != [255; 4] || pixel(&output, (40, 40))[1] != 128 {
        return Err("贴图重叠亮区或暗度不同于截图".into());
    }
    pass("native_spotlight_pin_overlapping_holes_preserve_original_shared_dim");
    Ok(())
}
