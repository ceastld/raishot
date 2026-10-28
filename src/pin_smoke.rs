//! 贴图专项：只使用合成图；不读取、修改用户剪贴板，不遍历其它进程窗口。
use crate::{
    capture::Image,
    desktop::DesktopCapture,
    export,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
fn fixture() -> Result<Image> {
    let full = crate::geometry::Rect::full(600, 360);
    let mut image = crate::raster::empty(full);
    image.bgra.fill(255);
    for y in 220..330 {
        for x in 360..580 {
            let i = ((y * 600 + x) * 4) as usize;
            image.bgra[i..i + 4].copy_from_slice(&[60, 150, 240, 255]);
        }
    }
    let mut text = crate::annotation::Annotation::new(
        crate::annotation::Tool::Text,
        crate::annotation::Point { x: 28., y: 28. },
        crate::annotation::Style {
            color: 0x20252b,
            font_size: 48.,
            ..Default::default()
        },
    );
    text.update(crate::annotation::Point { x: 570., y: 130. });
    text.text = "RustShot 2026".into();
    if let Some((r, p)) = crate::raster::text(&text, full)? {
        crate::raster::composite(&mut image, full, &p, r);
    }
    Ok(image)
}
fn screenshot(p: Placement) -> Result<Image> {
    let desktop = DesktopCapture::capture()?;
    desktop.read_region(crate::geometry::Rect {
        x: (p.x - desktop.bounds.left) as u32,
        y: (p.y - desktop.bounds.top) as u32,
        width: p.width,
        height: p.height,
    })
}
async fn idle(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    for _ in 0..80 {
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        if pin
            .update(cx, |v, _, _| !v.processing)
            .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
    }
    Err("贴图后台任务超时".into())
}
pub fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    let initial = (|| -> Result<_> {
        crate::pin_store::clear()?;
        let mut cursor = POINT::default();
        unsafe {
            GetCursorPos(&mut cursor)?;
        }
        let work = native::work_area((cursor.x, cursor.y));
        let origin = (work.x + 80, work.y + 80);
        let source = fixture()?;
        let pin = PinView::open_state(source, origin, PinState::default(), cx)?;
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
    cx.spawn(async move|cx|{
  let result:Result<()>=async{
   let pause=Duration::from_millis(250);macro_rules! wait{()=>{cx.background_executor().timer(pause).await;}}macro_rules! action{($a:expr)=>{pin.update(cx,|v,w,cx|v.perform($a,w,cx)).map_err(|e|e.to_string())?;}}
   let folder=std::path::Path::new("target/qa/pin");std::fs::create_dir_all(folder)?;wait!();
   if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").ok().as_deref()!=Some("state") {
   let initial=pin.update(cx,|v,w,_|{v.hovered=false;v.status.clear();(native::rect(hwnd(w)),v.original.clone(),v.image.clone())}).map_err(|e|e.to_string())?;let rect=initial.0?;
   if (rect.width,rect.height)!=(600,360){return Err(format!("贴图 1:1 物理尺寸错误：{rect:?}").into());}
   let pixels=screenshot(rect)?;let i=((280*600+500)*4) as usize;if pixels.bgra[i..i+3]!=[60,150,240]{return Err(format!("真实贴图像素错误：{:?}",&pixels.bgra[i..i+4]).into());}export::save_png(pixels,&folder.join("pin-original.png"))?;
   input.wheel((100./input.scale,180./input.scale),120)?;wait!();
   let zoomed=pin.update(cx,|v,w,_|->Result<_>{if (v.state.zoom-1.1).abs()>0.001||!Arc::ptr_eq(&v.original,&initial.1)||!Arc::ptr_eq(&v.image,&initial.2){return Err("滚轮缩放重新生成图片或刻度错误".into());}native::rect(hwnd(w))}).map_err(|e|e.to_string())??;
   if (zoomed.x-(rect.x-10)).abs()>1||(zoomed.y-(rect.y-18)).abs()>1{return Err("缩放未保持鼠标锚点".into());}
   input.key(0x4c);wait!();input.wheel((100./input.scale,180./input.scale),120)?;wait!();
   pin.update(cx,|v,w,_|->Result<()>{if !v.state.locked||native::rect(hwnd(w))?!=zoomed{return Err("锁定后仍能缩放".into());}Ok(())}).map_err(|e|e.to_string())??;
   input.key(0x4c);wait!();action!(PinAction::Opacity(128));wait!();
   unsafe{let mut alpha=0;GetLayeredWindowAttributes(HWND(input.handle as *mut _),None,Some(&mut alpha),None)?;if alpha!=128{return Err("原生窗口透明度未生效".into());}}
   action!(PinAction::Opacity(255));action!(PinAction::Original);wait!();
   println!("{{\"test\":\"native_pin_pixels_physical_zoom_lock_opacity_shared_source\",\"ok\":true}}");
   let menu=pin.update(cx,|v,w,cx|->Result<_>{v.open_context_menu(w,cx)?;v.context_menu.ok_or_else(||"贴图菜单没有登记到宿主".into())}).map_err(|e|e.to_string())??;wait!();
   let menu_input=menu.update(cx,|_,w,_|Input{handle:hwnd(w).0 as usize,scale:w.scale_factor()}).map_err(|e|e.to_string())?;crate::gpui_smoke::isolate_input(HWND(menu_input.handle as *mut _))?;
   let tab=menu.update(cx,|v,_,_|v.positions.borrow().get("tab-1").copied()).map_err(|e|e.to_string())?.ok_or("菜单页签未绘制")?;menu_input.click(tab);wait!();
   let first=menu.update(cx,|v,w,_|->Result<_>{if v.tab!=1{return Err("实际菜单页签点击无效".into());}let p=v.positions.borrow().get("item-0").copied().ok_or("菜单项未绘制")?;export::save_png(screenshot(native::rect(hwnd(w))?)?,&folder.join("pin-menu.png"))?;Ok(p)}).map_err(|e|e.to_string())??;
   menu_input.click(first);wait!();idle(pin,cx).await?;
   pin.update(cx,|v,_,cx|->Result<()>{if v.state.adjustments.turns!=1||v.image_size!=(360,600){return Err("真实菜单旋转未完成".into());}let expected=v.state.adjustments.apply(&fixture()?)?;if v.pixels().bgra!=expected.bgra{return Err("贴图处理预览与导出内容不一致".into());}v.hovered=false;v.status.clear();cx.notify();Ok(())}).map_err(|e|e.to_string())??;wait!();
   let rotated=pin.update(cx,|_,w,_|native::rect(hwnd(w))).map_err(|e|e.to_string())??;export::save_png(screenshot(rotated)?,&folder.join("pin-rotated.png"))?;
   action!(PinAction::Undo);idle(pin,cx).await?;if pin.update(cx,|v,_,_|v.image_size).map_err(|e|e.to_string())?!=(600,360){return Err("处理撤销失败".into());}
   action!(PinAction::Thumbnail);wait!();let thumb=pin.update(cx,|v,w,_|->Result<_>{if !v.state.thumbnail{return Err("未进入缩略模式".into());}native::rect(hwnd(w))}).map_err(|e|e.to_string())??;if thumb.width.max(thumb.height)>161{return Err("缩略图尺寸不受限".into());}
   action!(PinAction::Thumbnail);wait!();
   println!("{{\"test\":\"native_pin_context_menu_rotation_export_undo_thumbnail\",\"ok\":true}}");
   action!(PinAction::Duplicate);wait!();
   let pins=cx.update(|cx|cx.windows().into_iter().filter_map(|h|h.downcast::<PinView>()).collect::<Vec<_>>()).map_err(|e|e.to_string())?;if pins.len()!=2{return Err("复制贴图未创建独立窗口".into());}
   for handle in &pins {let source=handle.update(cx,|v,_,_|v.original.clone()).map_err(|e|e.to_string())?;if !Arc::ptr_eq(&source,&initial.1){return Err("重复贴图没有共享原图".into());}}
   action!(PinAction::ClickThrough);wait!();if !native::transparent(HWND(input.handle as *mut _)){return Err("鼠标穿透未生效".into());}
   let owners=[std::process::id()];if native::manage(&owners,native::Manage::Hide)!=2{return Err("隐藏贴图数量错误".into());}wait!();unsafe{if IsWindowVisible(HWND(input.handle as *mut _)).as_bool(){return Err("隐藏操作未生效".into());}}
   native::manage(&owners,native::Manage::Show);wait!();if !native::transparent(HWND(input.handle as *mut _)){return Err("单独显示贴图不应改穿透状态".into());}
   native::manage(&owners,native::Manage::Interact);wait!();if native::transparent(HWND(input.handle as *mut _)){return Err("托盘恢复交互无效".into());}
   // 收起复制窗口，后续屏幕证据只取当前自有贴图。
   for other in pins {if other!=pin{other.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx)).map_err(|e|e.to_string())?;}}
   println!("{{\"test\":\"native_pin_duplicate_shared_pixels_hide_show_clickthrough_recover\",\"ok\":true}}");
   action!(PinAction::Ocr);idle(pin,cx).await?;wait!();
   // 字符级拖选从首词左端到末词右端；中心到中心只代表部分文字。
   let selection=pin.update(cx,|v,_,_|->Result<_>{let text=v.text.as_ref().ok_or("OCR 未返回文字")?;if !text.text.contains("2026")||text.words.len()<2||!v.text_mode{return Err("贴图本机 OCR 未识别合成文字".into());}let a=text.words.first().unwrap().rect;let b=text.words.last().unwrap().rect;Ok(((a.x+1,a.y+a.height/2),(b.x+b.width.saturating_sub(1),b.y+b.height/2)))}).map_err(|e|e.to_string())??;
   input.stroke(&[selection.0,selection.1])?;wait!();if !pin.update(cx,|v,_,_|v.selected_text().is_some_and(|s|s.contains("2026"))).map_err(|e|e.to_string())?{return Err("贴图文字拖选失败".into());}
   action!(PinAction::Text);action!(PinAction::Edit);wait!();
   let editors=cx.update(|cx|cx.windows().into_iter().filter_map(|h|h.downcast::<crate::capture_view::CaptureView>()).collect::<Vec<_>>()).map_err(|e|e.to_string())?;
   if editors.len()!=1{return Err("贴图编辑未打开复用编辑器".into());}
   editors[0].update(cx,|v,w,_|->Result<()>{if v.image_canvas.is_none()||v.source.full()!=crate::geometry::Rect::full(600,360){return Err("编辑器丢失贴图原图尺寸".into());}v.editor.select_tool(crate::annotation::Tool::Rectangle);v.editor.pointer_down((60,180),6.);v.editor.pointer_up((220,270));if v.selected_image()?.width!=600{return Err("贴图标注导出尺寸错误".into());}w.remove_window();Ok(())}).map_err(|e|e.to_string())??;wait!();
   println!("{{\"test\":\"native_pin_offline_ocr_word_selection_annotation_editor\",\"ok\":true}}");
   }
   action!(PinAction::Gray);idle(pin,cx).await?;
   action!(PinAction::ResetImage);idle(pin,cx).await?;
   if !pin.update(cx,|v,_,_|Arc::ptr_eq(&v.original,&v.image)).map_err(|e|e.to_string())?{return Err("重置图像重复分配了原图".into());}
   action!(PinAction::Ocr);idle(pin,cx).await?;
   let selection=pin.update(cx,|v,_,_|->Result<_>{let t=v.text.as_ref().ok_or("选字模式没有 OCR 结果")?;let a=t.words.first().ok_or("没有可选文字")?.rect;let b=t.words.last().unwrap().rect;Ok(((a.x+1,a.y+a.height/2),(b.x+b.width.saturating_sub(1),b.y+b.height/2)))}).map_err(|e|e.to_string())??;
   input.stroke(&[selection.0,selection.1])?;wait!();
   if !pin.update(cx,|v,_,_|v.selected_text().is_some()).map_err(|e|e.to_string())?{return Err("模式切换验证未建立文字选择".into());}
   action!(PinAction::Color);
   if !pin.update(cx,|v,_,_|v.color_pick&&!v.text_mode&&v.selected_text().is_none()).map_err(|e|e.to_string())?{return Err("取色模式保留了文字选择".into());}
   action!(PinAction::Ocr);idle(pin,cx).await?;
   if !pin.update(cx,|v,_,_|v.text_mode&&!v.color_pick).map_err(|e|e.to_string())?{return Err("选字和取色模式同时激活".into());}
   println!("{{\"test\":\"native_pin_reset_reuses_source_and_exclusive_selection_modes\",\"ok\":true}}");
   action!(PinAction::Gray);idle(pin,cx).await?;action!(PinAction::Zoom(1.5));wait!();
   let expected=pin.update(cx,|v,w,_|->Result<_>{Ok((v.pixels(),v.state.clone(),native::rect(hwnd(w))?))}).map_err(|e|e.to_string())??;
   // GPUI 原生层会在最后一个 HWND 销毁时退出；留一个隐藏的自有贴图完成恢复验证。
   action!(PinAction::Duplicate);wait!();
   let keepalive=cx.update(|cx|cx.windows().into_iter().filter_map(|h|h.downcast::<PinView>()).find(|h|*h!=pin)).map_err(|e|e.to_string())?.ok_or("恢复测试保活窗口未创建")?;
   keepalive.update(cx,|v,w,cx|v.perform(PinAction::Hide,w,cx)).map_err(|e|e.to_string())?;
   unsafe{PostMessageW(Some(HWND(input.handle as *mut _)),WM_CLOSE,WPARAM(0),LPARAM(0))?;}wait!();
   let restored=cx.update(|cx|->Result<_>{let mut saved=crate::pin_store::restore()?;let next=PinView::open_state(saved.image.take().unwrap(),saved.origin,saved.state.clone(),cx)?;saved.commit()?;Ok(next)}).map_err(|e|e.to_string())??;wait!();
   restored.update(cx,|v,w,_|->Result<()>{if v.pixels().bgra!=expected.0.bgra||v.state.adjustments!=expected.1.adjustments||(v.state.zoom-1.5).abs()>0.001||native::rect(hwnd(w))?!=expected.2||native::transparent(hwnd(w)){return Err("恢复未保留图片处理/比例/位置或恢复了穿透".into());}Ok(())}).map_err(|e|e.to_string())??;
   restored.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx)).map_err(|e|e.to_string())?;
   keepalive.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx)).map_err(|e|e.to_string())?;
   crate::pin_store::clear()?;
   let(w,p)=crate::capture::memory_mib();println!("{{\"test\":\"native_pin_close_restore_geometry_processing_no_clickthrough\",\"worker_working_set_mib\":{w:.2},\"worker_private_mib\":{p:.2},\"ok\":true}}");Ok(())
  }.await;*errors.borrow_mut()=result.err().map(|e|e.to_string());let _=cx.update(|cx|cx.quit());
 }).detach();
}
