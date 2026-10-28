//! 铅笔连续性：只用合成轨迹验证原生高速输入、矢量覆盖和丢失抬键后的收笔。
use crate::{
    annotation::{Point, Style, Tool},
    capture::Image,
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{Input, SMOKE_MOVE},
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(240))
        .await;
}
fn post(input: Input, message: u32, p: (u32, u32)) -> Result<()> {
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(if message == WM_MOUSEMOVE {
                SMOKE_MOVE | 1
            } else {
                1
            }),
            LPARAM(((p.1 << 16) | p.0) as isize),
        )?;
    }
    Ok(())
}
async fn verify(weak: WeakEntity<CaptureView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    let area = weak
        .update(cx, |v, cx| -> Result<_> {
            let monitor = v.source.screen_rect(v.source.current);
            if monitor.width < 1100 || monitor.height < 750 {
                return Err("铅笔连续性专项需要至少 1100×750 显示器".into());
            }
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
            v.editor = crate::editor::Editor::new(v.source.full(), Some(area));
            v.editor.select_tool(Tool::Pen);
            v.editor.set_style(Style {
                color: 0xff4b5c,
                width: 4.,
                ..Default::default()
            });
            v.include_cursor = false;
            v.guides = false;
            v.show_guides = false;
            cx.notify();
            Ok(area)
        })
        .map_err(|e| e.to_string())??;
    frame(cx).await;
    let oval: Vec<_> = (0..=300)
        .map(|i| {
            let t = i as f32 / 300. * std::f32::consts::TAU;
            (
                (area.x as f32 + 360. + 15. * t.cos()).round() as u32,
                (area.y as f32 + 320. + 30. * t.sin()).round() as u32,
            )
        })
        .collect();
    let fast: Vec<_> = (0..=700)
        .map(|i| {
            let t = i as f32 / 700. * std::f32::consts::TAU * 2.;
            (
                (area.x as f32 + 640. + 110. * t.sin()).round() as u32,
                (area.y as f32 + 350. + 100. * (2. * t).sin()).round() as u32,
            )
        })
        .collect();
    for (index, points) in [&oval, &fast].into_iter().enumerate() {
        input.stroke(points)?;
        frame(cx).await;
        weak.update(cx, |v, _| -> Result<()> {
            if v.editor.is_dragging() || v.editor.document.items().len() != index + 1 {
                return Err("快速笔画未完整提交".into());
            }
            let mut expected = points.clone();
            expected.dedup();
            let actual = &v.editor.document.items()[index].annotation.points;
            if actual.len() != expected.len()
                || actual.iter().zip(expected).any(|(a, b)| {
                    a.distance(Point {
                        x: b.0 as f32,
                        y: b.1 as f32,
                    }) > 0.01
                })
            {
                return Err("原生快速输入丢点、乱序或混入其它笔画".into());
            }
            let scene = v.prepared_scene();
            if !scene.errors.is_empty()
                || scene
                    .layers
                    .iter()
                    .any(|l| !matches!(l, crate::scene::Layer::Vector { .. }))
            {
                return Err("连续铅笔没有完整矢量网格".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    }
    input.hover((area.x + 900, area.y + 580))?;
    frame(cx).await;
    let rect = Rect {
        x: area.x + 300,
        y: area.y + 230,
        width: 500,
        height: 250,
    };
    let native = DesktopCapture::capture()?.read_region(rect)?;
    let mut rendered = Image {
        width: rect.width,
        height: rect.height,
        bgra: vec![255; (rect.width * rect.height * 4) as usize],
        readback_ms: 0.,
    };
    weak.update(cx, |v, _| v.prepared_scene().apply(&mut rendered, rect))
        .map_err(|e| e.to_string())?;
    for pixels in [&native, &rendered] {
        for &(x, y) in oval.iter().chain(fast.iter()) {
            let mut darkest = 255;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let px = (x as i32 - rect.x as i32 + dx) as u32;
                    let py = (y as i32 - rect.y as i32 + dy) as u32;
                    if px < rect.width && py < rect.height {
                        darkest =
                            darkest.min(pixels.bgra[((py * rect.width + px) * 4 + 1) as usize]);
                    }
                }
            }
            if darkest > 130 {
                return Err(format!("连续笔迹出现覆盖缺口：({x},{y})，G={darkest}").into());
            }
        }
    }
    let folder = Path::new("target/qa/pencil-continuity");
    std::fs::create_dir_all(folder)?;
    export::save_png(native, &folder.join("native-fast-loops.png"))?;
    export::save_png(rendered, &folder.join("export-fast-loops.png"))?;
    println!(
        "{{\"test\":\"native_pencil_fast_loops_order_vector_preview_export_no_gaps\",\"ok\":true}}"
    );
    // 故意不发送 UP，随后发没有按键的 MOVE：应就地收笔，不能连到悬停点。
    let a = (area.x + 200, area.y + 500);
    let b = (area.x + 260, area.y + 520);
    let far = (area.x + 850, area.y + 580);
    post(input, WM_LBUTTONDOWN, a)?;
    post(input, WM_MOUSEMOVE, b)?;
    frame(cx).await;
    input.hover(far)?;
    frame(cx).await;
    weak.update(cx, |v, _| -> Result<()> {
        if v.editor.is_dragging() || v.editor.document.items().len() != 3 {
            return Err("丢失抬键后没有结束笔画".into());
        }
        if v.editor.document.items()[2]
            .annotation
            .points
            .last()
            .unwrap()
            .distance(Point {
                x: b.0 as f32,
                y: b.1 as f32,
            })
            > 0.01
        {
            return Err("无按键悬停被连接到笔画尾部".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    input.stroke(&[(area.x + 300, area.y + 510), (area.x + 400, area.y + 520)])?;
    frame(cx).await;
    weak.update(cx, |v, _| -> Result<()> {
        if v.editor.document.items().len() != 4 {
            return Err("异常收笔影响下一笔".into());
        }
        v.editor.undo();
        v.editor.undo();
        if v.editor.document.items().len() != 2 {
            return Err("笔画撤销边界异常".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    println!("{{\"test\":\"native_pencil_missing_up_does_not_bridge_hover_and_next_stroke\",\"ok\":true}}");
    Ok(())
}
pub fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("铅笔连续性专项未完成".into());
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
