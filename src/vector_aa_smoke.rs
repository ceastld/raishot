//! 专门验收当前矢量渲染；不重复工具栏、录屏、OCR 等已通过流程。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    desktop::DesktopCapture,
    export,
    geometry::Rect,
    gpui_capture::hwnd,
    Result,
};
use gpui::{App, Entity, Window};
use std::{cell::RefCell, collections::BTreeSet, path::Path, rc::Rc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
pub fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    let weak = view.downgrade();
    let own = hwnd(window).0 as usize;
    window.on_next_frame(move|_,cx|{
        cx.spawn(async move|cx|{
            let result:Result<()>=async {
                crate::gpui_smoke::isolate_input(HWND(own as *mut _))?;
                let area=weak.update(cx,|v,cx|->Result<Rect>{
                    let monitor=v.source.screen_rect(v.source.current);
                    if monitor.width<1400||monitor.height<900{return Err("矢量专项需至少 1400×900 显示器".into());}
                    let area=Rect{x:monitor.x+40,y:monitor.y+40,width:1280,height:800};
                    v.editor.crop=Some(area);v.editor.document.clear();v.editor.selected=None;v.editor.selection.clear();
                    v.include_cursor=false;v.editor.tool=Tool::Edit;
                    v.pointer=(area.x as i32+1200,area.y as i32+760);v.pointer_sample=Point::from_tuple(v.pointer);
                    let p=|x:f32,y:f32|Point{x:area.x as f32+x,y:area.y as f32+y};
                    let mut background=Annotation::new(Tool::Rectangle,p(0.,0.),Style{color:0xffffff,filled:true,width:1.,..Default::default()});background.update(p(1280.,800.));v.editor.document.insert(background);
                    let mut pen=Annotation::new(Tool::Pen,p(40.,70.),Style{color:0x25374b,width:1.,..Default::default()});
                    for i in 1..320 {pen.update(p(40.+i as f32,70.+(i as f32*0.137+0.3).round()));}v.editor.document.insert(pen);
                    let mut ellipse=Annotation::new(Tool::Ellipse,p(60.,175.),Style{color:0x25374b,width:3.,..Default::default()});ellipse.update(p(510.,430.));v.editor.document.insert(ellipse);
                    let mut crossing=Annotation::new(Tool::Pen,p(650.,170.),Style{color:0x25374b,width:12.,opacity:0.35,pencil_smooth:false,..Default::default()});for q in[p(870.,390.),p(650.,390.),p(870.,170.)]{crossing.update(q);}v.editor.document.insert(crossing);
                    let mut filled=Annotation::new(Tool::Rectangle,p(610.,470.),Style{color:0x25374b,width:2.,radius:26.,filled:true,opacity:0.5,..Default::default()});filled.update(p(950.,615.));v.editor.document.insert(filled);
                    let mut arrow=Annotation::new(Tool::Arrow,p(50.,550.),Style{color:0x25374b,width:4.,dashed:true,opacity:0.6,..Default::default()});arrow.update(p(450.,580.));v.editor.document.insert(arrow);
                    let mut step=Annotation::new(Tool::Pen,p(50.,680.),Style{color:0x25374b,width:1.,..Default::default()});for q in[p(250.,680.),p(250.,682.),p(450.,682.)]{step.update(q);}v.editor.document.insert(step);
                    let scene=v.prepared_scene();if !scene.errors.is_empty(){return Err(scene.errors.join(";").into());}
                    if scene.layers.iter().any(|l|!matches!(l,crate::scene::Layer::Vector{..})){return Err("基础图形仍落入位图或逐段渲染".into());}
                    cx.notify();Ok(area)
                }).map_err(|e|e.to_string())??;
                cx.background_executor().timer(Duration::from_millis(500)).await;
                // 只读取本窗口的合成白底标注，辅助工具栏和桌面内容不进入验收图。
                let rect=Rect{x:area.x+15,y:area.y+15,width:1080,height:720};
                let native=DesktopCapture::capture()?.read_region(rect)?;
                let mut exported=weak.update(cx,|v,_|v.source.read_region(rect)).map_err(|e|e.to_string())??;
                weak.update(cx,|v,_|v.prepared_scene().apply(&mut exported,rect)).map_err(|e|e.to_string())?;
                let mut levels=BTreeSet::new();let mut error=0.;
                for i in 10..310 {
                    let x=area.x+40+i;let ideal=area.y as f32+70.+i as f32*0.137+0.3;
                    let mut weight=0.;let mut moment=0.;
                    for y in ideal as u32-4..ideal as u32+5 {
                        let j=(((y-rect.y)*rect.width+x-rect.x)*4) as usize;
                        let color=native.bgra[j+1];if color>56&&color<253{levels.insert(color);}
                        let ink=255.-color as f32;weight+=ink;moment+=ink*(y as f32+0.5);
                    }
                    if weight<60.{return Err(format!("解析矢量细线缺口：x={x}").into());}
                    error+=(moment/weight-ideal-0.137/2.).powi(2);
                }
                let rms=(error/300.).sqrt();
                let pixel=|image:&crate::capture::Image,x:u32,y:u32|{let i=(((area.y+y-rect.y)*rect.width+area.x+x-rect.x)*4) as usize;image.bgra[i+1]};
                let (cross,arm)=(pixel(&native,760,280),pixel(&native,690,210));
                let folder=Path::new("target/qa/vector-aa");std::fs::create_dir_all(folder)?;
                export::save_png(native, &folder.join("native-vector-aa.png"))?;
                export::save_png(exported,&folder.join("export-vector-aa.png"))?;
                if levels.len()<32{return Err(format!("矢量边缘仍只有 {} 档覆盖率",levels.len()).into());}
                if rms>0.25{return Err(format!("解析细线偏移：RMS={rms}").into());}
                if (cross as i16-arm as i16).abs()>3 || !(180..=190).contains(&cross){return Err(format!("透明交叉叠加或覆盖异常：交点={cross}，线段={arm}").into());}
                println!("{{\"test\":\"native_vector_analytic_aa_continuous_levels_once_alpha\",\"edge_levels\":{},\"rms_px\":{rms:.4},\"cross_green\":{cross},\"arm_green\":{arm},\"all_shapes_vector\":true,\"ok\":true}}",levels.len());
                // 真实 4× 视口缩放：同一矢量网格继续渲染，不能把上一帧标注纹理拉大。
                weak.update(cx,|v,cx|->Result<()>{
                    let scene=v.prepared_scene();let crate::scene::Layer::Vector{mesh:before,..}=&scene.layers[1] else{return Err("细线没有矢量网格".into());};
                    let origin=Point{x:area.x as f32+40.,y:area.y as f32+70.};
                    v.image_canvas=Some(crate::image_editor::ImageCanvas{path:None,viewport:crate::viewport::Viewport{zoom:4.,offset:Point{x:-3.*origin.x,y:-3.*origin.y}},auto_fit:false,host:(0,0),pan:None});
                    let after=v.prepared_scene();let crate::scene::Layer::Vector{mesh:after,..}=&after.layers[1] else{return Err("缩放后细线位图化".into());};
                    if !std::sync::Arc::ptr_eq(before,after){return Err("纯视口缩放修改了矢量文档缓存".into());}
                    cx.notify();Ok(())
                }).map_err(|e|e.to_string())??;
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let zoom_rect=Rect{x:area.x+45,y:area.y+55,width:1000,height:200};
                let zoomed=DesktopCapture::capture()?.read_region(zoom_rect)?;let mut zoom_levels=BTreeSet::new();
                for x in 20..980 {
                    let ideal=15.+(x as f32+5.)*0.137+1.2;
                    let mut weight=0.;
                    for y in ideal as u32-8..ideal as u32+9 {
                        let i=((y*zoom_rect.width+x)*4) as usize;let color=zoomed.bgra[i+1];if color>56&&color<253{zoom_levels.insert(color);}weight+=255.-color as f32;
                    }
                    if weight<450.{return Err(format!("4× 矢量缩放后出现笔迹缺口：x={x}").into());}
                }
                export::save_png(zoomed,&folder.join("native-vector-aa-4x.png"))?;
                if zoom_levels.len()<64{return Err(format!("4× 缩放仍为粗糙覆盖：{} 档",zoom_levels.len()).into());}
                println!("{{\"test\":\"native_vector_viewport_4x_reuses_geometry_continuous_edges\",\"edge_levels\":{},\"ok\":true}}",zoom_levels.len());
                Ok(())
            }.await;
            if let Err(e)=result{*errors.borrow_mut()=Some(e.to_string());}
            let _=cx.update(|cx|cx.quit());
            unsafe {let _=PostMessageW(Some(HWND(own as *mut _)),WM_NULL,WPARAM(0),LPARAM(0));}
        }).detach();
    });
}
