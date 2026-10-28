//! 工具栏稳定性专项：原生按下/移动/抬起分帧检查，只保存合成底图上的工具栏。
use crate::{
    annotation::{Point, Tool},
    capture::Image,
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{Input, SMOKE_MOVE},
    toolbar_layout::{PanelRect, ToolbarLayout},
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

fn pointer(input: Input, message: u32, p: (u32, u32)) -> Result<()> {
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
            LPARAM(((p.1 << 16) | p.0) as isize),
        )?;
    }
    Ok(())
}
async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(200))
        .await;
}
fn bars(v: &CaptureView, scale: f32) -> Result<ToolbarLayout> {
    for id in ["primary-toolbar", "property-toolbar"] {
        if !v.ui_positions.borrow().contains_key(id) {
            return Err(format!("当前帧缺少工具栏：{id}").into());
        }
    }
    Ok(v.toolbar_layout(v.selected()?, scale))
}
fn pixels(layout: ToolbarLayout, scale: f32) -> Result<[Image; 2]> {
    let screen = DesktopCapture::capture()?;
    let read = |r: PanelRect| {
        screen.read_region(Rect {
            x: ((r.x + 8.) * scale).round() as u32,
            y: ((r.y + 8.) * scale).round() as u32,
            width: ((r.width - 16.) * scale).floor() as u32,
            height: ((r.height - 16.) * scale).floor() as u32,
        })
    };
    Ok([read(layout.primary)?, read(layout.properties)?])
}
fn unchanged(before: &[Image; 2], after: &[Image; 2]) -> Result<()> {
    for (a, b) in before.iter().zip(after) {
        if a.width != b.width || a.height != b.height || a.bgra != b.bgra {
            return Err("按住绘制时工具栏像素发生变化或消失".into());
        }
    }
    Ok(())
}
async fn verify(view: WeakEntity<CaptureView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    let (area, anchor, screens) = view
        .update(cx, |v, cx| -> Result<_> {
            let monitor = v.source.screen_rect(v.source.current);
            if monitor.width < 1100 || monitor.height < 750 {
                return Err("工具栏专项需要至少 1100×750 的显示器".into());
            }
            // 用纯白预览替换所有屏幕，QA 图片不包含冻结的用户桌面。
            for (i, preview) in v.previews.iter_mut().enumerate() {
                let r = v.source.screen_rect(i);
                *preview = Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::from_pixel(r.width, r.height, image::Rgba([255; 4])),
                )]));
            }
            let area = Rect {
                x: monitor.x + 30,
                y: monitor.y + 30,
                width: 1000,
                height: 650,
            };
            let screens = v.source.screen_count();
            // 有多屏时故意在另一屏绘制，确认抬笔不会搬走工具栏。
            let anchor = (v.source.current + 1) % screens;
            v.editor.crop = Some(area);
            v.toolbar_monitor = anchor;
            v.include_cursor = false;
            v.guides = false;
            v.show_guides = false;
            v.pointer = ((area.x + 800) as i32, (area.y + 500) as i32);
            v.pointer_sample = Point::from_tuple(v.pointer);
            cx.notify();
            Ok((area, anchor, screens))
        })
        .map_err(|e| e.to_string())??;
    let a = (area.x + 250, area.y + 220);
    let b = (area.x + 450, area.y + 380);
    let folder = Path::new("target/qa/toolbar-stability");
    std::fs::create_dir_all(folder)?;
    for (index, tool) in [Tool::Pen, Tool::Rectangle, Tool::Arrow]
        .into_iter()
        .enumerate()
    {
        view.update(cx, |v, cx| {
            v.editor = crate::editor::Editor::new(v.source.full(), Some(area));
            v.editor.select_tool(tool);
            v.ui_positions.borrow_mut().clear();
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        input.hover(a)?;
        frame(cx).await;
        let initial = view
            .update(cx, |v, _| bars(v, input.scale))
            .map_err(|e| e.to_string())??;
        let before = pixels(initial, input.scale)?;
        for (message, position) in [(WM_LBUTTONDOWN, a), (WM_MOUSEMOVE, b)] {
            view.update(cx, |v, _| v.ui_positions.borrow_mut().clear())
                .map_err(|e| e.to_string())?;
            pointer(input, message, position)?;
            frame(cx).await;
            let active = view
                .update(cx, |v, _| -> Result<_> {
                    if !v.editor.is_dragging() || v.editor.is_changing_crop() {
                        return Err("未进入标注绘制手势".into());
                    }
                    bars(v, input.scale)
                })
                .map_err(|e| e.to_string())??;
            if initial.primary != active.primary || initial.properties != active.properties {
                return Err("绘制中工具栏位置发生变化".into());
            }
            let during = pixels(active, input.scale)?;
            unchanged(&before, &during)?;
            if index == 0 && message == WM_LBUTTONDOWN {
                for (i, image) in during.into_iter().enumerate() {
                    export::save_png(image, &folder.join(format!("pen-held-toolbar-{i}.png")))?;
                }
            }
        }
        view.update(cx, |v, _| v.ui_positions.borrow_mut().clear())
            .map_err(|e| e.to_string())?;
        pointer(input, WM_LBUTTONUP, b)?;
        frame(cx).await;
        view.update(cx, |v, _| -> Result<()> {
            if v.editor.is_dragging() || v.editor.document.items().len() != 1 {
                return Err("保留工具栏后笔画未正常提交".into());
            }
            if bars(v, input.scale)?.primary != initial.primary || v.toolbar_monitor != anchor {
                return Err("抬笔后工具栏跳动或更换显示器".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    }
    println!("{{\"test\":\"native_toolbar_visible_pixels_down_move_up_pen_rectangle_arrow\",\"screens\":{screens},\"ok\":true}}");
    view.update(cx, |v, cx| {
        v.editor = crate::editor::Editor::new(v.source.full(), Some(area));
        v.editor.select_tool(Tool::Pen);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    let close = view
        .update(cx, |v, _| v.ui_positions.borrow().get("cancel").copied())
        .map_err(|e| e.to_string())?
        .ok_or("关闭按钮未绘制")?;
    let end = (
        (close.0 * input.scale).round() as u32,
        (close.1 * input.scale).round() as u32,
    );
    pointer(input, WM_LBUTTONDOWN, a)?;
    frame(cx).await;
    pointer(input, WM_MOUSEMOVE, end)?;
    frame(cx).await;
    pointer(input, WM_LBUTTONUP, end)?;
    frame(cx).await;
    view.update(cx, |v, _| -> Result<()> {
        if v.editor.is_dragging()
            || v.editor.document.items().len() != 1
            || v.editor.tool != Tool::Pen
        {
            return Err("在工具栏上松手触发按钮或未结束笔画".into());
        }
        let tip = v.editor.document.items()[0]
            .annotation
            .points
            .last()
            .unwrap();
        if tip.distance(Point {
            x: end.0 as f32,
            y: end.1 as f32,
        }) > 1.
        {
            return Err("笔画越过工具栏后丢失终点".into());
        }
        v.editor.undo();
        if !v.editor.document.items().is_empty() {
            return Err("跨工具栏绘制没有作为一次笔画撤销".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    println!("{{\"test\":\"native_toolbar_drag_release_over_close_keeps_stroke\",\"ok\":true}}");
    Ok(())
}
pub fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("工具栏专项未执行完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    window.on_next_frame(move |_, cx| {
        cx.spawn(async move |cx| {
            let result = verify(weak, input, cx).await;
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
