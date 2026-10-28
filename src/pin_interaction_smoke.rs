//! 本机交互专项只截取自有合成图片窗口，不改写剪贴板或移动全局鼠标。
use crate::{
    capture::Image,
    export,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{cell::RefCell, rc::Rc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
fn fixture() -> Image {
    let mut bytes = vec![255; 600 * 360 * 4];
    for y in 0..360 {
        for x in 300..600 {
            let i = (y * 600 + x) * 4;
            bytes[i..i + 3].copy_from_slice(if (x / 12 + y / 12) % 2 == 0 {
                &[60, 150, 240]
            } else {
                &[160, 90, 30]
            });
        }
    }
    Image {
        width: 600,
        height: 360,
        bgra: bytes,
        readback_ms: 0.,
    }
}
fn snapshot(p: Placement, name: &str) -> Result<()> {
    let desktop = crate::desktop::DesktopCapture::capture()?;
    let image = desktop.read_region(crate::geometry::Rect {
        x: (p.x - desktop.bounds.left) as u32,
        y: (p.y - desktop.bounds.top) as u32,
        width: p.width,
        height: p.height,
    })?;
    export::save_png(
        image,
        &std::path::Path::new("target/qa/pin-interaction").join(name),
    )
}
async fn pause(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(260))
        .await;
}
async fn button(pin: WindowHandle<PinView>, id: &'static str, cx: &mut AsyncApp) -> Result<()> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("浮动工具栏未打开")?;
    let (input, p) = bar
        .update(cx, |v, w, _| -> Result<_> {
            let p = v
                .positions
                .borrow()
                .get(id)
                .copied()
                .ok_or_else(|| format!("按钮未绘制：{id}"))?;
            Ok((
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                p,
            ))
        })
        .map_err(|e| e.to_string())??;
    crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.click(p);
    pause(cx).await;
    Ok(())
}
pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    let initial = (|| -> Result<_> {
        std::fs::create_dir_all("target/qa/pin-interaction")?;
        let work = native::work_area((100, 100));
        let pin = PinView::open_state(
            if std::env::var("RUSTSHOT_PIN_ONLY_DRAG").as_deref() == Ok("1") {
                export::read_image(std::path::Path::new("target/qa/pin-interaction/export.png"))?
            } else {
                fixture()
            },
            (work.x + 100, work.y + 100),
            PinState::default(),
            cx,
        )?;
        let input = pin.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })?;
        crate::gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
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
    cx.spawn(async move|cx|{let result:Result<()>=async{
  pause(cx).await;
  if std::env::var("RUSTSHOT_PIN_ONLY_DRAG").as_deref()!=Ok("1") && std::env::var("RUSTSHOT_PIN_ONLY_CLOSE").as_deref()!=Ok("1") {
  let bounds=||native::rect(HWND(input.handle as *mut _));
  if std::env::var("RUSTSHOT_PIN_SKIP_MOVE").as_deref()!=Ok("1") {let initial=bounds()?;
  input.drag((100,120),(180,160));pause(cx).await;
  let moved=bounds()?;if (moved.x,moved.y)!=(initial.x+80,initial.y+40){return Err(format!("实际鼠标拖动无效：{initial:?} -> {moved:?}").into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Lock,w,cx)).map_err(|e|e.to_string())?;input.drag((220,200),(300,240));pause(cx).await;if bounds()?!=moved{return Err("锁定后仍移动".into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Lock,w,cx)).map_err(|e|e.to_string())?;
  input.drag((599,359),(699,419));pause(cx).await;if bounds()?.width!=700{return Err(format!("四角等比缩放无效：{:?}",bounds()?).into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Original,w,cx)).map_err(|e|e.to_string())?;pause(cx).await;
  println!("{{\"test\":\"native_pin_drag_lock_corner_resize\",\"ok\":true}}");
  }
  input.key(0x20);pause(cx).await;
  let bar=pin.update(cx,|v,_,_|v.toolbar).map_err(|e|e.to_string())?.ok_or("空格未显示浮动工具栏")?;
  bar.update(cx,|v,w,_|->Result<()>{let p=native::rect(hwnd(w))?;if v.positions.borrow().len()<14{return Err("工具栏按钮未完整绘制".into());}for(id,point)in v.positions.borrow().iter(){if point.0<0.||point.1<0.||point.0*w.scale_factor()>p.width as f32||point.1*w.scale_factor()>p.height as f32{return Err(format!("工具栏裁切：{id}").into());}}snapshot(p,"toolbar.png")}).map_err(|e|e.to_string())??;
  let before=bounds()?;button(pin,"rectangle",cx).await?;input.drag((50,70),(230,190));pause(cx).await;
  pin.update(cx,|v,_,cx|->Result<()>{let c=v.canvas.as_ref().ok_or("没有贴图内画布")?;if c.read(cx).editor.document.items().len()!=1{return Err("工具栏矩形没有画在贴图内".into());}
  if native::rect(HWND(input.handle as *mut _))?!=before{return Err("画矩形误拖动了贴图".into());}let out=v.output(cx)?;let i=((70*out.width+100)*4)as usize;if out.bgra[i+2]<200||out.bgra[i+1]>130{return Err("导出漏掉矩形标注".into());}Ok(())}).map_err(|e|e.to_string())??;
  button(pin,"pen",cx).await?;input.stroke(&[(55,240),(80,210),(110,260),(150,220),(210,260)])?;pause(cx).await;
  button(pin,"mosaic",cx).await?;input.drag((340,90),(540,230));pause(cx).await;
  let output=pin.update(cx,|v,_,cx|v.output(cx)).map_err(|e|e.to_string())??;export::save_png(output,std::path::Path::new("target/qa/pin-interaction/export.png"))?;
  button(pin,"undo",cx).await?;let count=pin.update(cx,|v,_,cx|v.canvas.as_ref().unwrap().read(cx).editor.document.items().len()).map_err(|e|e.to_string())?;if count!=2{return Err("浮栏撤销没有作用于贴图文档".into());}
  button(pin,"redo",cx).await?;
  input.key(0x20);pause(cx).await;
  pin.update(cx,|v,_,cx|->Result<()>{if v.chrome||v.toolbar.is_some()||v.canvas.as_ref().unwrap().read(cx).editor.document.items().len()!=3{return Err("收起工具栏丢失标注".into());}Ok(())}).map_err(|e|e.to_string())??;
  snapshot(bounds()?,"annotated.png")?;
  input.drag((100,120),(125,140));pause(cx).await;if bounds()?.x!=before.x+25{return Err("标注后无法再次拖动贴图".into());}
  input.key(0x20);pause(cx).await;
  // 长按空格产生的重复 keydown 不得反复开关。
  unsafe{PostMessageW(Some(HWND(input.handle as *mut _)),WM_KEYDOWN,WPARAM(0x20),LPARAM(1<<30))?;}pause(cx).await;
  if !pin.update(cx,|v,_,_|v.chrome).map_err(|e|e.to_string())?{return Err("长按空格导致工具栏闪烁".into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Zoom(1.1),w,cx)).map_err(|e|e.to_string())?;pause(cx).await;if bounds()?.width!=660{return Err(format!("贴图缩放未生效：{:?}, {:?}",bounds()?,pin.update(cx,|v,_,_|(v.state.zoom,v.status.clone())).map_err(|e|e.to_string())?).into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Zoom(0.1),w,cx)).map_err(|e|e.to_string())?;pause(cx).await;
  let bar=pin.update(cx,|v,_,_|v.toolbar.unwrap()).map_err(|e|e.to_string())?;
  if bar.update(cx,|_,w,_|native::rect(hwnd(w))).map_err(|e|e.to_string())??.width<=bounds()?.width{return Err("小贴图裁切浮动工具栏".into());}
  println!("{{\"test\":\"native_pin_space_floating_toolbar_inline_vectors_export_undo_small_pin\",\"ok\":true}}");
  pin.update(cx,|v,w,cx|v.perform(PinAction::Rotate(1),w,cx)).map_err(|e|e.to_string())?;
  for _ in 0..40{pause(cx).await;if !pin.update(cx,|v,_,_|v.processing).map_err(|e|e.to_string())?{break;}}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Undo,w,cx)).map_err(|e|e.to_string())?;
  for _ in 0..40{pause(cx).await;if !pin.update(cx,|v,_,_|v.processing).map_err(|e|e.to_string())?{break;}}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Undo,w,cx)).map_err(|e|e.to_string())?;pause(cx).await;
  if pin.update(cx,|v,_,cx|v.canvas.as_ref().map(|c|c.read(cx).editor.document.items().len())).map_err(|e|e.to_string())?!=Some(2){return Err("旋转撤销后未恢复可编辑标注文档".into());}
  pin.update(cx,|v,w,cx|v.perform(PinAction::Redo,w,cx)).map_err(|e|e.to_string())?;
  pin.update(cx,|v,w,cx|v.perform(PinAction::Redo,w,cx)).map_err(|e|e.to_string())?;
  for _ in 0..40{pause(cx).await;if !pin.update(cx,|v,_,_|v.processing).map_err(|e|e.to_string())?{break;}}
  if pin.update(cx,|v,_,_|v.image_size).map_err(|e|e.to_string())?!=(360,600){return Err("标注和整体处理的重做顺序错误".into());}
  println!("{{\"test\":\"native_pin_annotation_processing_boundary_undo_redo\",\"ok\":true}}");
  }
  if std::env::var("RUSTSHOT_PIN_ONLY_CLOSE").as_deref()!=Ok("1") {
  let image=pin.update(cx,|v,_,cx|v.output(cx)).map_err(|e|e.to_string())??;
  let path=crate::pin_drag::prepare(image)?;let data=crate::pin_drag::data_object(&path)?;
  unsafe{use windows::Win32::{System::{Com::*,Ole::*},UI::Shell::*};let format=FORMATETC{cfFormat:CF_HDROP.0,ptd:std::ptr::null_mut(),dwAspect:DVASPECT_CONTENT.0,lindex:-1,tymed:TYMED_HGLOBAL.0 as u32};let mut medium=data.GetData(&format)?;let mut name=[0_u16;1024];let len=DragQueryFileW(HDROP(medium.u.hGlobal.0),0,Some(&mut name));ReleaseStgMedium(&mut medium);if len==0||std::fs::canonicalize(std::path::PathBuf::from(String::from_utf16_lossy(&name[..len as usize])))?!=path{return Err("原生拖出数据没有正确 PNG 路径".into());}}
  println!("{{\"test\":\"pin_native_drag_payload_contains_annotated_png\",\"ok\":true}}");
  let target=pin.update(cx,|v,_,_|v.drop_target.clone().unwrap()).map_err(|e|e.to_string())?;
  unsafe{use windows::Win32::System::{Ole::*,SystemServices::*};let mut effect=DROPEFFECT_COPY;let p=POINTL{x:0,y:0};target.DragEnter(&data,MK_LBUTTON,p,&mut effect)?;if effect!=DROPEFFECT_COPY{return Err("文件拖入未被接受".into());}target.Drop(&data,MODIFIERKEYS_FLAGS(0),p,&mut effect)?;}
  for _ in 0..40{pause(cx).await;if !pin.update(cx,|v,_,_|v.processing).map_err(|e|e.to_string())?{break;}}
  let others=cx.update(|cx|cx.windows().into_iter().filter_map(|w|w.downcast::<PinView>()).filter(|w|*w!=pin).collect::<Vec<_>>()).map_err(|e|e.to_string())?;
  if others.len()!=1{return Err("原生文件拖入未新建贴图".into());}
  for other in others{other.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx)).map_err(|e|e.to_string())?;}
  println!("{{\"test\":\"native_pin_drop_target_opens_image_without_replacing_source\",\"ok\":true}}");

  }
  // GPUI 会在最后一个 HWND 销毁时退出；保留隐藏的自有辅助窗口，完成资源清理断言。
  let keepalive=cx.update(|cx|PinView::open_state(fixture(),(100,100),PinState::default(),cx)).map_err(|e|e.to_string())??;
  keepalive.update(cx,|v,w,cx|v.perform(PinAction::Hide,w,cx)).map_err(|e|e.to_string())?;
  pin.update(cx,|v,w,cx|{if !v.chrome{v.perform(PinAction::Chrome,w,cx);}}).map_err(|e|e.to_string())?;pause(cx).await;
  let bar=pin.update(cx,|v,_,_|v.toolbar).map_err(|e|e.to_string())?.ok_or("关闭测试未建立悬浮工具栏")?;
  let bar_hwnd=bar.update(cx,|_,w,_|hwnd(w)).map_err(|e|e.to_string())?;
  unsafe{PostMessageW(Some(HWND(input.handle as *mut _)),WM_CLOSE,WPARAM(0),LPARAM(0))?;}pause(cx).await;
  if cx.update(|cx|cx.windows().len()!=1).map_err(|e|e.to_string())? || unsafe{IsWindow(Some(bar_hwnd)).as_bool()}{return Err("关闭贴图后悬浮工具栏残留".into());}
  println!("{{\"test\":\"native_pin_close_releases_owned_toolbar\",\"ok\":true}}");
  keepalive.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx)).map_err(|e|e.to_string())?;Ok(())
 }.await;if let Err(e)=result{*errors.borrow_mut()=Some(format!("贴图交互专项：{e}"));}else{*errors.borrow_mut()=None;}let _=cx.update(|cx|cx.quit());}).detach();
}
