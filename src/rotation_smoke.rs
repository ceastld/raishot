//! 只验收旋转把手的真实位置、连续拖动和缩放平移后的连接线。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    transform, Result,
};
use gpui::{App, Entity, Window};
use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

fn screen(v: &CaptureView, p: Point) -> Point {
    v.image_canvas.as_ref().map_or(p, |s| Point {
        x: p.x * s.viewport.zoom + s.viewport.offset.x,
        y: p.y * s.viewport.zoom + s.viewport.offset.y,
    })
}
fn verify(v: &CaptureView, dpi: f32) -> Result<(Point, Point, Point)> {
    let a = v.editor.selected_annotation().ok_or("没有测试图形")?;
    let b = a.local_bounds();
    let zoom = v.image_canvas.as_ref().map_or(1., |s| s.viewport.zoom);
    let anchor = screen(
        v,
        a.to_world(Point {
            x: b.x + b.width / 2.,
            y: b.y,
        }),
    );
    let expected = screen(
        v,
        a.to_world(Point {
            x: b.x + b.width / 2.,
            y: b.y - 24. * dpi / zoom,
        }),
    );
    let &(x, y) = v
        .ui_positions
        .borrow()
        .get("rotation-handle")
        .ok_or("没有绘制旋转把手")?;
    let actual = Point {
        x: x * dpi,
        y: y * dpi,
    };
    if actual.distance(expected) > 1.5 {
        return Err(format!("把手未跟随旋转选框：actual={actual:?}, expected={expected:?}").into());
    }
    Ok((actual, anchor, screen(v, transform::center(b))))
}
fn verify_connector(image: &crate::capture::Image, rect: Rect, a: Point, b: Point) -> Result<()> {
    for t in [0.25, 0.5, 0.7] {
        let x = (a.x + (b.x - a.x) * t).round() as i32 - rect.x as i32;
        let y = (a.y + (b.y - a.y) * t).round() as i32 - rect.y as i32;
        let mut blue = false;
        for dy in -2..=2 {
            for dx in -2..=2 {
                let (x, y) = (x + dx, y + dy);
                if x < 0 || y < 0 || x >= image.width as i32 || y >= image.height as i32 {
                    continue;
                }
                let i = ((y as u32 * image.width + x as u32) * 4) as usize;
                let p = &image.bgra[i..i + 4];
                blue |= p[0] > 225
                    && p[1] > 140
                    && p[1] < 220
                    && p[2] < 185
                    && p[0] as i16 - p[2] as i16 > 50;
            }
        }
        if !blue {
            return Err(format!("把手连接线在 t={t} 处错位或缺失").into());
        }
    }
    Ok(())
}
pub fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    window.on_next_frame(move |_,cx| {
        cx.spawn(async move |cx| {
            let result: Result<()> = async {
                crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
                let area = weak.update(cx, |v,cx| -> Result<Rect> {
                    let monitor = v.source.screen_rect(v.source.current);
                    if monitor.width < 1400 || monitor.height < 900 { return Err("旋转专项需至少 1400×900 显示器".into()); }
                    let area = Rect { x:monitor.x+40,y:monitor.y+40,width:1280,height:800 };
                    let p = |x:f32,y:f32| Point {x:area.x as f32+x,y:area.y as f32+y};
                    v.editor.document.clear();v.editor.selection.clear();v.editor.crop=Some(area);v.editor.tool=Tool::Edit;v.include_cursor=false;
                    let mut background=Annotation::new(Tool::Rectangle,p(0.,0.),Style {color:0xffffff,filled:true,..Default::default()});background.update(p(1280.,800.));v.editor.document.insert(background);
                    let mut shape=Annotation::new(Tool::Rectangle,p(520.,300.),Style {color:0x25374b,width:3.,..Default::default()});shape.update(p(700.,390.));v.editor.selected=Some(v.editor.document.insert(shape));
                    v.pointer=(area.x as i32+1100,area.y as i32+700);v.pointer_sample=Point::from_tuple(v.pointer);
                    cx.notify();Ok(area)
                }).map_err(|e|e.to_string())??;
                let pause=Duration::from_millis(250);
                cx.background_executor().timer(pause).await;
                let folder=Path::new("target/qa/rotation");std::fs::create_dir_all(folder)?;
                for (step, delta, angle) in [(0,45.,45.),(1,45.,90.),(2,90.,-180.),(3,45.,-135.)] {
                    if step == 3 {
                        weak.update(cx, |v,cx| {
                            let center=transform::center(v.editor.selected_annotation().unwrap().local_bounds());
                            v.image_canvas=Some(crate::image_editor::ImageCanvas { path:None,viewport:crate::viewport::Viewport {zoom:1.75,offset:Point{x:-0.75*center.x+95.,y:-0.75*center.y+60.}},auto_fit:false,host:(0,0),pan:None });cx.notify();
                        }).map_err(|e|e.to_string())?;
                        cx.background_executor().timer(pause).await;
                    }
                    let (knob,_,pivot)=weak.update(cx,|v,_| verify(v,input.scale)).map_err(|e|e.to_string())??;
                    let end=transform::Rotation::new(pivot,delta).map(knob);
                    input.drag((knob.x.round() as u32,knob.y.round() as u32),(end.x.round() as u32,end.y.round() as u32));
                    cx.background_executor().timer(pause).await;
                    input.hover((area.x+1100,area.y+700))?;
                    cx.background_executor().timer(pause).await;
                    let (knob,anchor,_)=weak.update(cx,|v,_| -> Result<_> {
                        let actual=v.editor.selected_annotation().ok_or("拖动把手丢失选择")?.rotation;
                        let error=(actual-angle+180.).rem_euclid(360.)-180.;
                        if error.abs()>1. { return Err(format!("实际把手未命中或连续拖动跳变：角度={actual}，预期={angle}").into()); }
                        verify(v,input.scale)
                    }).map_err(|e|e.to_string())??;
                    let rect=Rect{x:area.x+330,y:area.y+140,width:600,height:470};
                    let image=DesktopCapture::capture()?.read_region(rect)?;
                    verify_connector(&image,rect,anchor,knob)?;
                    export::save_png(image,&folder.join(format!("rotation-handle-{step}.png")))?;
                    println!("{{\"test\":\"native_rotation_handle\",\"step\":{step},\"angle\":{angle},\"zoom_and_pan\":{},\"ok\":true}}",step==3);
                }
                Ok(())
            }.await;
            if let Err(e)=result { *errors.borrow_mut()=Some(e.to_string()); }
            let _=cx.update(|cx|cx.quit());
            unsafe {let _=PostMessageW(Some(HWND(input.handle as *mut _)),WM_NULL,WPARAM(0),LPARAM(0));}
        }).detach();
    });
}
