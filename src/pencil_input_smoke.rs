//! 铅笔输入专项：绘制无选框，旧笔迹上继续落笔，显式编辑后可移动。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    interaction::Hover,
    Result,
};
use gpui::{App, Entity, Window};
use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
fn drawing(v: &CaptureView, point: (u32, u32), dpi: f32) -> Result<()> {
    if v.editor.tool != Tool::Pen
        || !v.editor.selected_ids().is_empty()
        || v.editor.rotation_handle(6. * dpi).is_some()
    {
        return Err("铅笔完成后仍处于对象选择状态".into());
    }
    if v.ui_positions.borrow().contains_key("rotation-handle") {
        return Err("铅笔绘制后仍显示旋转把手".into());
    }
    if v.editor.hover((point.0 as i32, point.1 as i32), 6. * dpi) != Hover::Crosshair {
        return Err("铅笔悬停在旧笔迹上变成了移动光标".into());
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
                let area=weak.update(cx,|v,cx|->Result<Rect>{
                    let monitor=v.source.screen_rect(v.source.current);
                    if monitor.width<1300||monitor.height<850 {return Err("铅笔专项需要至少 1300×850 的显示器".into());}
                    let area=Rect{x:monitor.x+30,y:monitor.y+30,width:1200,height:750};
                    v.editor.clear();v.editor.crop=Some(area);v.include_cursor=false;
                    let mut bg=Annotation::new(Tool::Rectangle,Point{x:area.x as f32,y:area.y as f32},Style{color:0xffffff,filled:true,..Default::default()});bg.update(Point{x:(area.x+area.width) as f32,y:(area.y+area.height) as f32});v.editor.document.insert(bg);
                    v.editor.select_tool(Tool::Pen);v.editor.set_style(Style{color:0x25374b,width:3.,..Default::default()});
                    v.pointer=((area.x+1100) as i32,(area.y+650) as i32);v.pointer_sample=Point::from_tuple(v.pointer);
                    cx.notify();Ok(area)
                }).map_err(|e|e.to_string())??;
                let pause=Duration::from_millis(250);
                macro_rules! wait {()=>{cx.background_executor().timer(pause).await;}}
                macro_rules! button {($id:expr)=>{{let p=weak.update(cx,|v,_|v.ui_positions.borrow().get($id).copied()).map_err(|e|e.to_string())?.ok_or_else(||format!("找不到按钮：{}",$id))?;input.click(p);wait!();}}}
                let p=|x,y|(area.x+x,area.y+y);
                wait!();
                input.stroke(&[p(320,220),p(400,220),p(480,220),p(480,320),p(620,320)])?;wait!();
                let original=weak.update(cx,|v,_|->Result<_>{drawing(v,p(350,220),input.scale)?;if v.editor.document.items().len()!=2{return Err("首笔数量不正确".into());}Ok(v.editor.document.items()[1].annotation.clone())}).map_err(|e|e.to_string())??;
                input.hover(p(400,220))?;input.wheel(((area.x+400) as f32/input.scale,(area.y+220) as f32/input.scale),120)?;wait!();
                weak.update(cx,|v,_|->Result<()>{if v.editor.document.items()[1].annotation!=original||v.editor.style.width<=original.style.width{return Err("落笔后滚轮误改已有笔迹".into());}Ok(())}).map_err(|e|e.to_string())??;
                input.stroke(&[p(400,220),p(400,300),p(400,420)])?;wait!();
                input.drag(p(320,220),p(320,220));wait!();
                weak.update(cx,|v,_|->Result<()>{drawing(v,p(400,220),input.scale)?;let items=v.editor.document.items();if items.len()!=4||items[1].annotation!=original||items[2].annotation.style.width<=original.style.width||items[3].annotation.points.len()!=1{return Err("重叠落笔或点画误选/移动了旧笔迹".into());}Ok(())}).map_err(|e|e.to_string())??;
                button!("undo");button!("undo");
                weak.update(cx,|v,_|->Result<()>{drawing(v,p(350,220),input.scale)?;if v.editor.document.items().len()!=2||v.editor.document.items()[1].annotation!=original{return Err("连续笔迹未逐笔撤销".into());}Ok(())}).map_err(|e|e.to_string())??;
                println!("{{\"test\":\"native_pencil_unselected_overlap_dot_next_style_undo\",\"ok\":true}}");
                button!("edit");input.drag(p(350,220),p(370,250));wait!();
                let moved=weak.update(cx,|v,_|->Result<_>{let items=v.editor.document.items();if items.len()!=2||v.editor.selected_ids().len()!=1||!v.ui_positions.borrow().contains_key("rotation-handle"){return Err("对象编辑模式无法选中笔迹".into());}let a=&items[1].annotation;if a.points[0].distance(Point{x:original.points[0].x+20.,y:original.points[0].y+30.})>0.01{return Err("编辑模式移动笔迹失败".into());}Ok(a.clone())}).map_err(|e|e.to_string())??;
                button!("pen");input.stroke(&[p(370,250),p(390,350),p(420,390)])?;wait!();
                input.hover(p(1100,650))?;wait!();
                weak.update(cx,|v,_|->Result<()>{drawing(v,p(370,250),input.scale)?;if v.editor.document.items().len()!=3||v.editor.document.items()[1].annotation!=moved{return Err("返回铅笔后未恢复连续绘制".into());}Ok(())}).map_err(|e|e.to_string())??;
                let rect=Rect{x:area.x+260,y:area.y+150,width:460,height:350};
                let image=DesktopCapture::capture()?.read_region(rect)?;
                let blue=image.bgra.chunks_exact(4).filter(|c|c[0]>225&&c[1]>140&&c[1]<220&&c[2]<185).count();
                if blue!=0{return Err(format!("铅笔画布残留 {blue} 个选框蓝色像素").into());}
                let folder=Path::new("target/qa/pencil-input");std::fs::create_dir_all(folder)?;export::save_png(image,&folder.join("continuous-drawing.png"))?;
                println!("{{\"test\":\"native_pencil_explicit_edit_then_draw_no_selection_pixels\",\"ok\":true}}");
                Ok(())
            }.await;
            if let Err(e)=result{*errors.borrow_mut()=Some(e.to_string());}
            let _=cx.update(|cx|cx.quit());
            unsafe {let _=PostMessageW(Some(HWND(input.handle as *mut _)),WM_NULL,WPARAM(0),LPARAM(0));}
        }).detach();
    });
}

/// 大量来回涂抹后的帧间隔：同一画面分别关闭/开启静态标注缓存，悬停与继续画一笔。
pub fn schedule_perf(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    use std::{cell::Cell, time::Instant};
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let frames: Rc<RefCell<Vec<Instant>>> = Default::default();
    let recording = Rc::new(Cell::new(false));
    fn record(window: &mut Window, frames: Rc<RefCell<Vec<Instant>>>, recording: Rc<Cell<bool>>) {
        window.on_next_frame(move |window, _| {
            if recording.get() {
                frames.borrow_mut().push(Instant::now());
            }
            record(window, frames, recording);
        });
    }
    record(window, frames.clone(), recording.clone());
    window.on_next_frame(move |_, cx| {
        cx.spawn(async move |cx| {
            let result: Result<()> = async {
                crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
                let strokes = std::env::var("RUSTSHOT_PERF_STROKES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(40_u32);
                let area = weak
                    .update(cx, |v, cx| -> Result<Rect> {
                        let monitor = v.source.screen_rect(v.source.current);
                        if monitor.width < 1300 || monitor.height < 850 {
                            return Err("性能专项需要至少 1300×850 的显示器".into());
                        }
                        let area = Rect { x: monitor.x + 30, y: monitor.y + 30, width: 1200, height: 750 };
                        v.editor.clear();
                        v.editor.crop = Some(area);
                        v.include_cursor = false;
                        v.editor.select_tool(Tool::Pen);
                        v.editor.set_style(Style { color: 0x25374b, width: 4., ..Default::default() });
                        // 默认按真实鼠标密度：1000Hz 补回样本、相邻 1–2 px、连续绕圈并右移（一笔 1500 样本）。
                        // RUSTSHOT_PERF_SHAPE=sparse 回到旧的稀疏往返（每笔 400 点、相邻约 20 px）。
                        let sparse = std::env::var("RUSTSHOT_PERF_SHAPE").is_ok_and(|v| v == "sparse");
                        for s in 0..strokes {
                            if sparse {
                                let y0 = area.y as f32 + 20. + (s * 13 % 500) as f32;
                                let at = |t: f32| {
                                    let x = area.x as f32 + 50. + 1000. * (0.5 - 0.5 * (t * 8. * std::f32::consts::PI).cos());
                                    (x as i32, (y0 + 200. * t) as i32)
                                };
                                v.editor.pointer_down(at(0.), 4.);
                                for i in 1..=400 {
                                    v.editor.pointer_move(at(i as f32 / 400.));
                                }
                                v.editor.pointer_up(at(1.));
                                continue;
                            }
                            let (x0, y0) = (area.x as f32 + 70. + (s % 3 * 300) as f32, area.y as f32 + 70. + (s * 13 % 560) as f32);
                            let at = |i: usize| {
                                let t = i as f32 * 0.037;
                                let r = 38. + 14. * (t * 0.31 + s as f32).sin();
                                let hash = (i as u32 ^ (s + 1).wrapping_mul(2654435761)).wrapping_mul(2246822519);
                                let jitter = (hash >> 16) as f32 / 65535. - 0.5;
                                ((x0 + i as f32 * 0.3 + r * t.cos() + jitter * 0.8).round() as i32,
                                 (y0 + r * 0.8 * t.sin() - jitter * 0.6).round() as i32)
                            };
                            v.editor.pointer_down(at(0), 4.);
                            for i in 1..1500 {
                                v.editor.pointer_move(at(i));
                            }
                            v.editor.pointer_up(at(1499));
                        }
                        v.pointer = ((area.x + 1100) as i32, (area.y + 650) as i32);
                        cx.notify();
                        Ok(area)
                    })
                    .map_err(|e| e.to_string())??;
                macro_rules! wait {
                    ($ms:expr) => {
                        cx.background_executor().timer(Duration::from_millis($ms)).await;
                    };
                }
                let pack = |x: f32, y: f32| LPARAM(((((area.y as f32 + y) as isize) << 16) | (area.x as f32 + x) as isize) as isize);
                let h = HWND(input.handle as *mut _);
                let mut report = Vec::new();
                for (phase, disabled) in [("direct", true), ("retained", false)] {
                    weak.update(cx, |v, cx| {
                        v.retained_annotations.borrow_mut().disabled = disabled;
                        cx.notify();
                    })
                    .map_err(|e| e.to_string())?;
                    wait!(600);
                    // hover/draw 用铅笔；drag 在编辑模式拖动第 11 笔起点，覆盖下层/实时/上层三段；
                    // mosaic 在全部笔迹之上放一块马赛克并拖动它，逐帧重新采样下层合成。
                    for (kind, button, tool) in [
                        ("hover", false, Tool::Pen),
                        ("draw", true, Tool::Pen),
                        ("drag", true, Tool::Edit),
                        ("mosaic", true, Tool::Edit),
                    ] {
                        let mosaic = kind == "mosaic";
                        weak.update(cx, |v, cx| {
                            if mosaic {
                                v.editor.select_tool(Tool::Mosaic);
                                let at = |x: u32, y: u32| ((area.x + x) as i32, (area.y + y) as i32);
                                v.editor.pointer_down(at(150, 120), 4.);
                                v.editor.pointer_move(at(400, 300));
                                v.editor.pointer_move(at(550, 420));
                                v.editor.pointer_up(at(550, 420));
                            }
                            v.editor.select_tool(tool);
                            cx.notify();
                        })
                        .map_err(|e| e.to_string())?;
                        wait!(300);
                        let drag = tool == Tool::Edit;
                        let (x0, y0) = if mosaic {
                            (350., 270.)
                        } else if drag {
                            (50., 150.)
                        } else {
                            (100., 300.)
                        };
                        let revision = weak.update(cx, |v, _| v.editor.document.revision()).map_err(|e| e.to_string())?;
                        frames.borrow_mut().clear();
                        recording.set(true);
                        let started = Instant::now();
                        let mut moves = 0;
                        unsafe {
                            if button {
                                PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(1), pack(x0, y0))?;
                            }
                            // 按时间而不是次数推进：直接绘制很慢时仍在固定时长内结束。
                            while started.elapsed() < Duration::from_millis(1500) {
                                let t = started.elapsed().as_secs_f32() / 1.5;
                                moves += 1;
                                let (x, y) = if drag {
                                    (x0 + 400. * t, y0 + 100. * t)
                                } else {
                                    (x0 + 900. * (0.5 - 0.5 * (t * 6. * std::f32::consts::PI).cos()), y0 + 150. * t)
                                };
                                PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(crate::gpui_smoke::SMOKE_MOVE | usize::from(button)), pack(x, y))?;
                                wait!(4);
                            }
                            if button {
                                PostMessageW(Some(h), WM_LBUTTONUP, WPARAM(0), pack(x0, y0 + 150.))?;
                            }
                        }
                        wait!(100);
                        recording.set(false);
                        let elapsed = started.elapsed().as_secs_f64() * 1000.;
                        let stamps = frames.borrow().clone();
                        let committed = weak.update(cx, |v, _| v.editor.document.revision() != revision).map_err(|e| e.to_string())?;
                        let mut gaps: Vec<f64> = stamps.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.).collect();
                        gaps.sort_by(|a, b| a.total_cmp(b));
                        let avg = gaps.iter().sum::<f64>() / gaps.len().max(1) as f64;
                        let p95 = gaps.get(gaps.len() * 95 / 100).copied().unwrap_or(0.);
                        let max = gaps.last().copied().unwrap_or(0.);
                        report.push(format!(
                            "{{\"phase\":\"{phase}\",\"input\":\"{kind}\",\"strokes\":{strokes},\"moves\":{moves},\"committed\":{committed},\"elapsed_ms\":{elapsed:.0},\"frames\":{},\"avg_gap_ms\":{avg:.1},\"p95_gap_ms\":{p95:.1},\"max_gap_ms\":{max:.1}}}",
                            stamps.len()
                        ));
                        if button {
                            // 撤销新笔／拖动（马赛克再撤销创建），两组对照使用相同画面。
                            weak.update(cx, |v, cx| {
                                v.editor.undo();
                                if mosaic {
                                    v.editor.undo();
                                }
                                cx.notify();
                            })
                            .map_err(|e| e.to_string())?;
                        }
                        wait!(300);
                    }
                }
                for line in report {
                    println!("{line}");
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                *errors.borrow_mut() = Some(e.to_string());
            }
            let _ = cx.update(|cx| cx.quit());
            unsafe {
                let _ = PostMessageW(Some(HWND(input.handle as *mut _)), WM_NULL, WPARAM(0), LPARAM(0));
            }
        })
        .detach();
    });
}
