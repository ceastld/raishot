//! 四角圆角把手原生验收：合成白底，真实鼠标消息、旋转/视口与对象历史。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    document::ObjectId,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_view::PinView,
    spotlight_smoke::frame,
    Result,
};
use gpui::{App, AsyncApp, Entity, WeakEntity, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

const PROBES: [&str; 4] = [
    "radius-top-left",
    "radius-top-right",
    "radius-bottom-right",
    "radius-bottom-left",
];
fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn seed(v: &mut CaptureView, area: Rect, tool: Tool) -> ObjectId {
    v.editor.cancel_gesture();
    v.editor.document.clear();
    let mut a = Annotation::new(
        tool,
        point(area.x as f32 + 150., area.y as f32 + 130.),
        Style {
            radius: 0.,
            width: 3.,
            color: 0xf04b66,
            ..Style::default()
        },
    );
    a.update(point(area.x as f32 + 450., area.y as f32 + 330.));
    a.rotation = 27.;
    let id = v.editor.document.insert(a.clone());
    if tool == Tool::Spotlight {
        let mut other = a.clone();
        other.translate(350., 0.);
        other.style.radius = 9.;
        v.editor.document.insert(other);
    }
    let mut next = a.clone();
    next.style.radius = 7.;
    v.editor.document.replace(id, next);
    v.editor.document.undo();
    v.editor.select_tool(Tool::Edit);
    v.editor.selected = Some(id);
    v.editor.style = a.style;
    v.ui_positions.borrow_mut().clear();
    id
}
fn screen(v: &CaptureView, p: Point) -> Point {
    v.image_canvas
        .as_ref()
        .map_or(p, |s| s.viewport.window_point(p))
}
fn control(v: &CaptureView, input: Input, corner: usize) -> Result<(Point, Point)> {
    let zoom = v.image_canvas.as_ref().map_or(1., |s| s.viewport.zoom);
    let handles = v.editor.radius_controls(6. * input.scale / zoom);
    let knob = handles
        .iter()
        .find(|h| h.corner == corner)
        .ok_or("圆角把手未可达")?;
    let &(x, y) = v
        .ui_positions
        .borrow()
        .get(PROBES[corner])
        .ok_or("圆角把手没有真实绘制")?;
    let actual = point(x * input.scale, y * input.scale);
    if actual.distance(screen(v, knob.position)) > 1.6 {
        return Err("圆角把手没有跟随旋转/缩放视口".into());
    }
    let a = v.editor.selected_annotation().ok_or("没有选中对象")?;
    let signs = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)][corner];
    let local = a.to_local(knob.position);
    let end = a.to_world(point(
        local.x + signs.0 * 20. / 2_f32.sqrt(),
        local.y + signs.1 * 20. / 2_f32.sqrt(),
    ));
    Ok((actual, screen(v, end)))
}
fn rounded(p: Point) -> (u32, u32) {
    (p.x.round().max(0.) as u32, p.y.round().max(0.) as u32)
}
fn send(input: Input, message: u32, p: Point) -> Result<()> {
    let (x, y) = rounded(p);
    let buttons = if message == WM_MOUSEMOVE {
        gpui_smoke::SMOKE_MOVE | 1
    } else {
        1
    };
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(buttons),
            LPARAM(((y << 16) | x) as isize),
        )?;
    }
    Ok(())
}
fn pass(test: &str) {
    println!("{{\"test\":\"{test}\",\"ok\":true}}");
}
async fn verify(
    view: WeakEntity<CaptureView>,
    input: Input,
    cx: &mut AsyncApp,
    cancel: bool,
) -> Result<()> {
    frame(cx).await;
    frame(cx).await;
    if cancel {
        let (start, end) = view
            .update(cx, |v, _| control(v, input, 0))
            .map_err(|e| e.to_string())??;
        let revision = view
            .update(cx, |v, _| v.editor.document.revision())
            .map_err(|e| e.to_string())?;
        input.click((start.x / input.scale, start.y / input.scale));
        frame(cx).await;
        if !view
            .update(cx, |v, _| {
                v.editor.document.revision() == revision && v.editor.document.can_redo()
            })
            .map_err(|e| e.to_string())?
        {
            return Err("圆角无变化点击破坏历史/重做".into());
        }
        send(input, WM_LBUTTONDOWN, start)?;
        send(input, WM_MOUSEMOVE, end)?;
        frame(cx).await;
        if !view
            .update(cx, |v, _| {
                v.editor.is_dragging_radius()
                    && v.ui_positions.borrow().contains_key("radius-value-hint")
                    && v.editor.document.revision() == revision
            })
            .map_err(|e| e.to_string())?
        {
            return Err("圆角拖动草稿提前提交或数值提示缺失".into());
        }
        input.key(0x1b);
        frame(cx).await;
        unsafe {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_LBUTTONUP,
                WPARAM(0),
                LPARAM(0),
            )?;
        }
        frame(cx).await;
        if !view
            .update(cx, |v, _| {
                !v.editor.is_dragging()
                    && v.editor.document.revision() == revision
                    && v.editor.document.can_redo()
                    && v.editor
                        .selected_annotation()
                        .is_some_and(|a| a.style.radius == 0.)
            })
            .map_err(|e| e.to_string())?
        {
            return Err("Esc取消圆角拖动破坏原对象/历史".into());
        }
        pass("native_radius_handle_noop_preview_escape_cancel_preserves_redo");
    }
    for corner in 0..4 {
        let (start, end) = view
            .update(cx, |v, _| control(v, input, corner))
            .map_err(|e| e.to_string())??;
        let before = view
            .update(cx, |v, _| v.editor.selected_annotation().unwrap().clone())
            .map_err(|e| e.to_string())?;
        input.drag(rounded(start), rounded(end));
        frame(cx).await;
        view.update(cx, |v, cx| -> Result<()> {
            let a = v.editor.selected_annotation().ok_or("圆角拖动丢失选择")?;
            if (a.style.radius - 20.).abs() > 1.5
                || a.points != before.points
                || a.rotation != before.rotation
            {
                return Err(
                    format!("四角{corner}拖动改变了位置/角度或半径：{}", a.style.radius).into(),
                );
            }
            if a.tool == Tool::Spotlight
                && v.editor
                    .document
                    .items()
                    .iter()
                    .any(|o| Some(o.id) != v.editor.selected && o.annotation.style.radius != 9.)
            {
                return Err("圆角串改了另一聚光灯孔".into());
            }
            let id = v.editor.selected.ok_or("圆角对象选择已丢失")?;
            v.editor.undo();
            if v.editor
                .document
                .get(id)
                .ok_or("撤销丢失对象")?
                .style
                .radius
                != 0.
            {
                return Err("圆角一次撤销未恢复按下值".into());
            }
            v.editor.redo();
            if (v
                .editor
                .document
                .get(id)
                .ok_or("重做丢失对象")?
                .style
                .radius
                - 20.)
                .abs()
                > 1.5
            {
                return Err("圆角重做未恢复".into());
            }
            v.editor.undo();
            v.editor.selected = Some(id);
            v.editor.style = v.editor.document.get(id).unwrap().style;
            cx.notify();
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        frame(cx).await;
    }
    Ok(())
}
async fn verify_capture(
    view: WeakEntity<CaptureView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    let highlight = std::env::var("RUSTSHOT_RADIUS_TOOL").as_deref() == Ok("highlight");
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    unsafe {
        let h = HWND(input.handle as *mut _);
        if !IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
    let area = view
        .update(cx, |v, cx| {
            let area = crate::spotlight_smoke::install(v)?;
            seed(
                v,
                area,
                if highlight {
                    Tool::HighlighterRect
                } else {
                    Tool::Rectangle
                },
            );
            cx.notify();
            Ok::<_, Box<dyn std::error::Error>>(area)
        })
        .map_err(|e| e.to_string())??;
    if highlight {
        verify(view.clone(), input, cx, false).await?;
        pass("native_highlight_radius_four_rotated_handles_one_undo");
        let samples = view
            .update(cx, |v, cx| -> Result<_> {
                let id = v.editor.selected.ok_or("高亮框选择丢失")?;
                let mut a = v.editor.document.get(id).ok_or("高亮框对象丢失")?.clone();
                a.style.radius = 40.;
                a.style.width = 80.; // 隐藏的旧荧光笔笔径不应增加高亮框描边。
                a.style.opacity = 0.35;
                let b = a.local_bounds();
                let samples = [(1., 1.), (-5., 100.), (150., 100.), (40., 10.)]
                    .map(|(x, y)| a.to_world(point(b.x + x, b.y + y)));
                v.editor.document.replace(id, a);
                v.editor.selected = None;
                v.editor.select_tool(Tool::HighlighterRect);
                cx.notify();
                Ok(samples)
            })
            .map_err(|e| e.to_string())??;
        frame(cx).await;
        frame(cx).await;
        let native = crate::spotlight_smoke::native_owned_region(area)?;
        let exported = view
            .update(cx, |v, _| -> Result<_> {
                let mut image = v.source.read_region(area)?;
                v.prepared_scene().apply(&mut image, area);
                Ok(image)
            })
            .map_err(|e| e.to_string())??;
        for (index, p) in samples.into_iter().enumerate() {
            let x = p.x.round() as u32;
            let y = p.y.round() as u32;
            if !area.contains((x as i32, y as i32)) {
                return Err("高亮框像素样本越出自有白底".into());
            }
            let offset = (((y - area.y) * area.width + x - area.x) * 4) as usize;
            let actual = &native.bgra[offset..offset + 3];
            let output = &exported.bgra[offset..offset + 3];
            println!("{{\"stage\":\"highlight_radius_pixel\",\"index\":{index},\"native\":{actual:?},\"exported\":{output:?}}}");
            if actual.iter().zip(output).any(|(a, b)| a.abs_diff(*b) > 4)
                || (index < 2 && output != [255, 255, 255])
                || (index >= 2 && output == [255, 255, 255])
            {
                return Err("高亮圆角透明角/外沿/内部的原生预览与导出不一致".into());
            }
        }
        pass("native_highlight_radius_fill_only_corner_preview_export");
        return Ok(());
    }
    if std::env::var("RUSTSHOT_RADIUS_SKIP_VERIFIED_RECT").as_deref() != Ok("1") {
        verify(view.clone(), input, cx, true).await?;
        pass("native_radius_handle_rectangle_four_corners_rotated_undo_redo");
    } else {
        frame(cx).await;
        frame(cx).await;
    }
    let (knob, _) = view
        .update(cx, |v, _| control(v, input, 0))
        .map_err(|e| e.to_string())??;
    let sample = Rect {
        x: (knob.x.round() as u32).saturating_sub(7),
        y: (knob.y.round() as u32).saturating_sub(7),
        width: 15,
        height: 15,
    };
    let native = crate::spotlight_smoke::native_owned_region(sample)?;
    // 圆角柄是白底标注蓝描边的菱形：按标注蓝（BGRA）计数，导出里不应出现。
    let gold = |pixel: &[u8]| {
        pixel[0] > 200
            && pixel[1] > 120
            && pixel[1] < 200
            && pixel[2] < 160
            && pixel[0] as i16 - pixel[2] as i16 > 60
    };
    let actual = native.bgra.chunks_exact(4).filter(|p| gold(p)).count();
    let exported = view
        .update(cx, |v, _| -> Result<_> {
            let mut image = v.source.read_region(sample)?;
            v.prepared_scene().apply(&mut image, sample);
            Ok(image)
        })
        .map_err(|e| e.to_string())??;
    let exported_gold = exported.bgra.chunks_exact(4).filter(|p| gold(p)).count();
    let mut native_window = RECT::default();
    let visible = unsafe {
        GetWindowRect(HWND(input.handle as *mut _), &mut native_window)?;
        IsWindowVisible(HWND(input.handle as *mut _)).as_bool()
    };
    let original = view
        .update(cx, |v, _| (v.source.bounds.left, v.source.bounds.top))
        .map_err(|e| e.to_string())?;
    println!("{{\"stage\":\"radius_window_geometry\",\"visible\":{visible},\"window\":[{},{},{},{}],\"canvas_origin\":[{},{}],\"knob\":[{},{}]}}", native_window.left,native_window.top,native_window.right,native_window.bottom,original.0,original.1,knob.x,knob.y);
    println!("{{\"stage\":\"radius_chrome_pixels\",\"native_gold\":{actual},\"exported_gold\":{exported_gold},\"native_center\":{:?}}}", &native.bgra[(7*15+7)*4..(7*15+7)*4+4]);
    if actual < 8 || exported_gold != 0 {
        return Err(format!("原生圆角矢量把手未绘制或进入了导出：native_gold={actual}, exported_gold={exported_gold}").into());
    }
    pass("native_radius_handle_vector_chrome_visible_excluded_from_export");
    view.update(cx, |v, cx| {
        let center =
            crate::transform::center(v.editor.selected_annotation().unwrap().local_bounds());
        v.image_canvas = Some(crate::image_editor::ImageCanvas {
            path: None,
            viewport: crate::viewport::Viewport {
                zoom: 1.75,
                offset: point(-0.75 * center.x + 55., -0.75 * center.y + 45.),
            },
            auto_fit: false,
            host: (0, 0),
            pan: None,
        });
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    verify(view.clone(), input, cx, false).await?;
    pass("native_radius_handle_four_corners_zoom_pan_stable_geometry");
    view.update(cx, |v, cx| {
        v.image_canvas = None;
        seed(v, area, Tool::Spotlight);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    verify(view, input, cx, false).await?;
    pass("native_radius_handle_spotlight_local_radius_shared_layer_preserved");
    Ok(())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    w: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("圆角把手专项未完成".into());
    let view = view.downgrade();
    let input = Input {
        handle: hwnd(w).0 as usize,
        scale: w.scale_factor(),
    };
    w.on_next_frame(move |_, cx| {
        let done = Rc::new(Cell::new(false));
        let watchdog = done.clone();
        let timeout_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_secs(40))
                .await;
            if !watchdog.get() {
                *timeout_errors.borrow_mut() = Some("圆角把手专项超时".into());
                let _ = cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let result = verify_capture(view, input, cx).await;
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
pub(crate) async fn verify_pin(
    pin: WindowHandle<PinView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    unsafe {
        let h = HWND(input.handle as *mut _);
        if !IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
    frame(cx).await;
    input.key(0x20);
    frame(cx).await;
    frame(cx).await;
    let canvas = pin
        .update(cx, |v, _, _| v.canvas.clone())
        .map_err(|e| e.to_string())?
        .ok_or("贴图画布未创建")?;
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图一级栏未创建")?;
    canvas
        .update(cx, |v, cx| {
            v.pin_editing = true;
            seed(v, Rect::full(800, 500), Tool::Rectangle);
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    verify(canvas.downgrade(), input, cx, false).await?;
    if pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        != Some(bar)
    {
        return Err("圆角编辑重建贴图一级栏".into());
    }
    pass("native_radius_handle_pin_four_corners_shared_editor_toolbar_stable");
    Ok(())
}
