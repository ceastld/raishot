//! 仅验长截图预览的替换帧：两轴增长夹具进入真实 GPUI，不采集桌面或发送用户输入。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    long_capture_preview::{LongCapturePreview, LongPreviewLifecycleProbe},
    long_capture_runtime::LongPreview,
    process_memory::{cpu_memory, gpu_memory},
    pin_model::Placement,
    stitch::ScrollAxis,
    Result,
};
use gpui::{
    div, App, AppContext, Application, AsyncApp, Context, Render, Window, WindowHandle,
    WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::{HANDLE, HWND, POINT},
        Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY},
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::*,
    },
};

const APPENDS: u32 = 12;
const WAIT: Duration = Duration::from_secs(10);
const PROPERTY: windows::core::PCWSTR = w!("RustShot.LongPreviewStability.Owned");
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(8))
        .await;
}
fn placement_json(p: Placement) -> Value {
    json!({"x":p.x,"y":p.y,"width":p.width,"height":p.height})
}

struct Report {
    root: PathBuf,
    file: std::fs::File,
    stages: Vec<Value>,
    groups: Vec<Value>,
    gpu_samples: u32,
}
impl Report {
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        let value = json!({"name":name,"unix_ms":unix_ms()?,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail});
        serde_json::to_writer(&mut self.file, &value)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.stages.push(value);
        Ok(())
    }
}
struct Owned {
    h: HWND,
    pid: u32,
    tid: u32,
    token: Rc<()>,
}
impl Owned {
    fn register(h: HWND) -> Result<Self> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        ensure(
            pid == std::process::id() && tid == unsafe { GetCurrentThreadId() },
            "只登记实际自有预览 UI HWND",
        )?;
        let token = Rc::new(());
        unsafe {
            SetPropW(h, PROPERTY, Some(HANDLE(Rc::as_ptr(&token) as *mut _)))?;
        }
        Ok(Self { h, pid, tid, token })
    }
    fn check(&self) -> Result<()> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(self.h, Some(&mut pid)) };
        ensure(
            pid == self.pid
                && tid == self.tid
                && pid == std::process::id()
                && unsafe {
                    IsWindow(Some(self.h)).as_bool()
                        && GetPropW(self.h, PROPERTY).0 == Rc::as_ptr(&self.token) as *mut _
                },
            "原预览 HWND/PID/TID/SetProp 身份失效",
        )
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            if GetPropW(self.h, PROPERTY).0 == Rc::as_ptr(&self.token) as *mut _ {
                let _ = RemovePropW(self.h, PROPERTY);
            }
        }
    }
}
struct Lifetimes {
    images: Vec<Weak<gpui::RenderImage>>,
    sources: Vec<Weak<Image>>,
}
impl Lifetimes {
    fn observe(
        &mut self,
        preview: WindowHandle<LongCapturePreview>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let weak = preview.update(cx, |v, _, _| v.image_weaks())?;
        self.images.extend(weak.current);
        self.images.extend(weak.retired);
        self.sources.extend(weak.source);
        Ok(())
    }
    fn expired(&self) -> bool {
        self.images.iter().all(|w| w.upgrade().is_none())
            && self.sources.iter().all(|w| w.upgrade().is_none())
    }
}
fn color(step: u32) -> [u8; 4] {
    [
        31 + step as u8 * 7,
        83 + step as u8 * 5,
        151 - step as u8 * 3,
        255,
    ]
}
fn fixture(step: u32, axis: ScrollAxis, dpi: f32) -> LongPreview {
    let long = 240 * (step + 1);
    let projected = long.min(1400);
    let cross_pixels = (200. * dpi.clamp(1., 2.5)).round() as u32;
    let long_pixels = (projected as f64 * f64::from(cross_pixels) / 400.).round() as u32;
    let (width, height, image_width, image_height, source_rect, viewport) =
        if axis == ScrollAxis::Vertical {
            (
                400,
                long,
                cross_pixels,
                long_pixels,
                Rect {
                    x: 0,
                    y: long - projected,
                    width: 400,
                    height: projected,
                },
                Rect {
                    x: 0,
                    y: long - 240,
                    width: 400,
                    height: 240,
                },
            )
        } else {
            (
                long,
                400,
                long_pixels,
                cross_pixels,
                Rect {
                    x: long - projected,
                    y: 0,
                    width: projected,
                    height: 400,
                },
                Rect {
                    x: long - 240,
                    y: 0,
                    width: 240,
                    height: 400,
                },
            )
        };
    let mut bgra = Vec::with_capacity(image_width as usize * image_height as usize * 4);
    for _ in 0..image_width * image_height {
        bgra.extend_from_slice(&color(step));
    }
    LongPreview {
        image: Arc::new(Image {
            width: image_width,
            height: image_height,
            bgra,
            readback_ms: 0.,
        }),
        revision: u64::from(step) + 1,
        generation: 1,
        width,
        height,
        viewport,
        axis,
        source_rect,
    }
}
fn work_area() -> Result<Placement> {
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    ensure(
        unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() },
        "无法读取实际预览测试工作区",
    )?;
    Ok(Placement {
        x: info.rcWork.left,
        y: info.rcWork.top,
        width: (info.rcWork.right - info.rcWork.left) as u32,
        height: (info.rcWork.bottom - info.rcWork.top) as u32,
    })
}
fn probe(
    preview: WindowHandle<LongCapturePreview>,
    owned: &Owned,
    cx: &mut AsyncApp,
) -> Result<(Placement, LongPreviewLifecycleProbe, Value)> {
    owned.check()?;
    preview.update(cx, |v,w,_| -> Result<_> {
        ensure(hwnd(w) == owned.h, "GPUI 预览与原 HWND 不匹配")?;
        if let Some(error) = &v.error { return Err(error.clone().into()); }
        let native = crate::pin_native::rect(owned.h)?;
        let dpi = w.scale_factor();
        let painted = (*v.preview_bounds.borrow()).map(|b| json!({"x":f32::from(b.origin.x),"y":f32::from(b.origin.y),"width":f32::from(b.size.width),"height":f32::from(b.size.height)}));
        Ok((native,v.lifecycle_probe(),json!({"ready":v.ready,"dpi":dpi,"native_visible":unsafe {IsWindowVisible(owned.h)}.as_bool(),"placement":placement_json(v.placement),"native_rect":placement_json(native),"painted_bounds":painted,
            "displayed_revision":v.displayed.as_ref().map(|f|f.revision),"source_rect":v.displayed.as_ref().map(|f|json!({"x":f.source_rect.x,"y":f.source_rect.y,"width":f.source_rect.width,"height":f.source_rect.height})),"width":v.displayed.as_ref().map(|f|f.width),"height":v.displayed.as_ref().map(|f|f.height)})))
    })?
}
fn pixel_matches(bytes: &[u8], expected: [u8; 4]) -> bool {
    bytes
        .chunks_exact(4)
        .all(|p| p.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1))
}
async fn capture(
    preview: WindowHandle<LongCapturePreview>,
    owned: &Owned,
    roi: [u32; 4],
    cx: &mut AsyncApp,
) -> Result<Image> {
    let end = Instant::now() + WAIT;
    loop {
        owned.check()?;
        tick(cx).await;
        if let Some(result) = preview.update(cx, |_, w, _| w.take_frame_capture())? {
            let bgra = result.map_err(|e| e.to_string())?;
            ensure(
                bgra.len() == roi[2] as usize * roi[3] as usize * 4,
                "真实 GPU 小 ROI 长度错误",
            )?;
            return Ok(Image {
                width: roi[2],
                height: roi[3],
                bgra,
                readback_ms: 0.,
            });
        }
        ensure(Instant::now() < end, "真实 GPU 小 ROI 读回超时")?;
    }
}
fn save_sample(report: &mut Report, name: &str, image: Image) -> Result<Value> {
    let path = report.root.join(name);
    ensure(!path.exists(), "GPU 样本路径已存在")?;
    let dimensions = [image.width, image.height];
    crate::export::save_png(image, &path)?;
    report.gpu_samples += 1;
    Ok(
        json!({"path":path,"bytes":std::fs::metadata(&path)?.len(),"width":dimensions[0],"height":dimensions[1]}),
    )
}
async fn settled(
    preview: WindowHandle<LongCapturePreview>,
    owned: &Owned,
    revision: u64,
    cx: &mut AsyncApp,
) -> Result<(Placement, LongPreviewLifecycleProbe, Value)> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        let state = probe(preview, owned, cx)?;
        if state.2["ready"] == true
            && state.1.painted_generation == 1
            && state.1.painted_revision == revision
            && state.1.retired_images == 0
        {
            return Ok(state);
        }
        ensure(Instant::now() < end, "新预览实际 paint/旧纹理退休未完成")?;
    }
}
async fn axis_case(axis: ScrollAxis, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let name = if axis == ScrollAxis::Vertical {
        "vertical"
    } else {
        "horizontal"
    };
    let work = work_area()?;
    ensure(
        work.width >= 400 && work.height >= 350,
        "当前工作区过小，无法提供预览 GPU 采样空间",
    )?;
    let region = ScreenRect {
        left: work.x + 40,
        top: work.y + 40,
        width: 160,
        height: 160,
    };
    let toolbar = Placement {
        x: work.x + 40,
        y: work.y + (work.height - 100) as i32,
        width: 300,
        height: 70,
    };
    let preview = cx.update(|cx| LongCapturePreview::open(region, axis, toolbar, cx))??;
    let (handle, dpi, entity_weak, ledger) = preview.update(cx, |v, w, cx| {
        (
            hwnd(w),
            w.scale_factor(),
            cx.entity().downgrade(),
            v.lifecycle_ledger(),
        )
    })?;
    let owned = Owned::register(handle)?;
    let mut life = Lifetimes {
        images: Vec::new(),
        sources: Vec::new(),
    };
    let result:Result<Value>=async {
        preview.update(cx,|v,w,cx|v.set_frame(fixture(0,axis,dpi),toolbar,w,cx))??;
        let (shell,base,initial)=settled(preview,&owned,1,cx).await?; life.observe(preview,cx)?;
        ensure(shell==preview.update(cx,|v,_,_|v.placement)? && initial["native_visible"]==true,"预览外壳未实际就位")?;
        let roi=preview.update(cx,|v,w,_| -> Result<[u32;4]> {
            let bounds=(*v.preview_bounds.borrow()).ok_or("首帧实际 paint bounds 缺失")?;
            let scale=w.scale_factor();
            let x=(f32::from(bounds.origin.x)*scale).round() as u32+16;
            let y=(f32::from(bounds.origin.y)*scale).round() as u32+16;
            ensure((f32::from(bounds.size.width)*scale) >=40. && (f32::from(bounds.size.height)*scale)>=40.,"首帧太小，不能避开边框抽样")?;
            Ok([x,y,12,12])
        })??;
        ensure(i64::from(shell.x)+i64::from(roi[0])>=i64::from(work.x)
            && i64::from(shell.y)+i64::from(roi[1])>=i64::from(work.y)
            && i64::from(shell.x)+i64::from(roi[0]+roi[2])<=i64::from(work.x)+i64::from(work.width)
            && i64::from(shell.y)+i64::from(roi[1]+roi[3])<=i64::from(work.y)+i64::from(work.height),"GPU ROI 必须在实际屏幕工作区内")?;
        preview.update(cx,|_,w,_|w.request_frame_capture(roi))??;
        let image=capture(preview,&owned,roi,cx).await?;
        let matches=pixel_matches(&image.bgra,color(0));
        let initial_png=save_sample(report,&format!("{name}-initial.png"),image)?;
        ensure(matches,"首帧 GPU 颜色与夹具不符")?;
        report.stage("preview-axis-opened",json!({"axis":name,"hwnd":handle.0 as usize,"pid":owned.pid,"tid":owned.tid,"work_area":placement_json(work),"shell":placement_json(shell),"roi":roi,"initial":initial,"baseline":base,"png":initial_png,"expected_bgra":color(0)}))?;
        let mut samples=Vec::new();
        for step in 1..=APPENDS {
            let old=preview.update(cx,|v,_,_|v.image_weaks().current)?;
            owned.check()?;
            preview.update(cx,|v,w,cx| -> Result<()> { w.request_frame_capture(roi)?; v.set_frame(fixture(step,axis,dpi),toolbar,w,cx) })??;
            let (pending_rect,pending_probe,pending)=probe(preview,&owned,cx)?;
            ensure(pending_rect==shell && pending_probe.native_placements==base.native_placements
                && pending_probe.native_resizes==base.native_resizes && pending_probe.native_moves==base.native_moves,"追加片段触发了原生窗口尺寸/位置变化")?;
            ensure(pending["dpi"]==initial["dpi"],"当前限定 DPI 在追加期间发生变化")?;
            ensure(pending_probe.painted_revision==u64::from(step) && pending["displayed_revision"]==u64::from(step)+1,"没有观察到真实替换 pending 状态")?;
            ensure(old.as_ref().is_some_and(|w|w.upgrade().is_some()),"新图还未实际 paint 就丢失旧图")?;
            life.observe(preview,cx)?;
            report.stage("preview-replacement-pending",json!({"axis":name,"step":step,"pending":pending,"probe":pending_probe,"prior_texture_weak_alive":true}))?;
            let image=capture(preview,&owned,roi,cx).await?;
            let new_matches=pixel_matches(&image.bgra,color(step));
            let previous_matches=pixel_matches(&image.bgra,color(step-1));
            let replacement_png=save_sample(report,&format!("{name}-{step:02}-replacement.png"),image)?;
            ensure(new_matches||previous_matches,"替换时 GPU ROI 出现背景/空白/错误版本")?;
            let (after_rect,after_probe,after)=settled(preview,&owned,u64::from(step)+1,cx).await?;
            ensure(after_rect==shell && after_probe.native_placements==base.native_placements
                && after_probe.native_resizes==base.native_resizes && after_probe.native_moves==base.native_moves
                && after_probe.paint_failures==0 && after_probe.drop_image_failures==0,"追加后的外壳或 paint 计数异常")?;
            ensure(after_probe.drop_image_successes==u64::from(step)
                && after_probe.retired_after_paint==after_probe.drop_image_successes,"旧纹理未在成功 paint 后退休")?;
            let end=Instant::now()+WAIT;
            while old.as_ref().is_some_and(|w|w.upgrade().is_some()) { tick(cx).await; ensure(Instant::now()<end,"已退休旧纹理 CPU Weak 未释放")?; }
            preview.update(cx,|_,w,_|w.request_frame_capture(roi))??;
            let image=capture(preview,&owned,roi,cx).await?;
            let exact=pixel_matches(&image.bgra,color(step));
            let settled_png=save_sample(report,&format!("{name}-{step:02}-settled.png"),image)?;
            ensure(exact,"成功 paint 后 GPU ROI 不是最新图片")?;
            let sample=json!({"step":step,"revision":u64::from(step)+1,"roi":roi,"pending":pending,"pending_probe":pending_probe,"replacement_png":replacement_png,"replacement_class":if new_matches{"new"}else{"previous"},"allowed_replacement_bgra":[color(step-1),color(step)],"settled_png":settled_png,"expected_settled_bgra":color(step),"after":after,"after_probe":after_probe,"prior_texture_weak_expired":true});
            report.stage("preview-replacement-sampled",json!({"axis":name,"sample":sample}))?; samples.push(sample); life.observe(preview,cx)?;
        }
        let final_probe=ledger.snapshot();
        ensure(final_probe.current_bgra_used_bytes<=4_000_000 && final_probe.current_bgra_capacity_bytes>=final_probe.current_bgra_used_bytes,"当前预览像素预算异常")?;
        Ok(json!({"name":name,"ok":true,"hwnd":handle.0 as usize,"pid":owned.pid,"tid":owned.tid,"shell":placement_json(shell),"roi":roi,"appends":APPENDS,"source_long_lengths":[240,480,720,960,1200,1440,1680,1920,2160,2400,2640,2880,3120],"windowed_source_reached":true,"initial_png":initial_png,"baseline":base,"final_probe":final_probe,"samples":samples}))
    }.await;
    let close_request = unix_ms()?;
    report.stage("preview-axis-close-request",json!({"axis":name,"hwnd":handle.0 as usize,"close_request_unix_ms":close_request,"owner_remove_window":true}))?;
    owned.check()?;
    preview.update(cx, |_, w, _| w.remove_window())?;
    let end = Instant::now() + WAIT;
    while unsafe { IsWindow(Some(handle)).as_bool() }
        || entity_weak.upgrade().is_some()
        || !life.expired()
    {
        tick(cx).await;
        ensure(
            Instant::now() < end,
            "关闭后预览 HWND/实体/像素 Weak 未释放",
        )?;
    }
    let closed = unix_ms()?;
    report.stage("preview-axis-closed",json!({"axis":name,"hwnd":handle.0 as usize,"close_request_unix_ms":close_request,"destroyed_observed_unix_ms":closed,"all_observed_weaks_expired":true,"lifecycle":ledger.snapshot(),"forced_cleanup":false}))?;
    let wait = Instant::now();
    while wait.elapsed() < Duration::from_millis(650) {
        tick(cx).await;
    }
    report.stage("preview-axis-settled",json!({"axis":name,"hwnd":handle.0 as usize,"closed_observed_unix_ms":closed,"post_close_wait_ms":wait.elapsed().as_millis(),"hidden_keepalive":true,"all_observed_weaks_expired":life.expired(),"lifecycle":ledger.snapshot()}))?;
    report.groups.push(result?);
    Ok(())
}
pub(crate) fn run() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("RUSTSHOT_LONG_PREVIEW_STABILITY_QA_DIR")
            .ok_or("缺少预览稳定性隔离 QA 目录")?,
    );
    ensure(root.is_absolute(), "QA 路径必须为绝对路径")?;
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    let settings = root.join("settings");
    std::fs::create_dir_all(&settings)?;
    if let Some(actual) = std::env::var_os("RUSTSHOT_DATA_DIR") {
        ensure(
            std::fs::canonicalize(actual)? == std::fs::canonicalize(&settings)?,
            "预览测试 data 目录必须是当前 artifact/settings",
        )?;
    }
    std::env::set_var("RUSTSHOT_DATA_DIR", &settings);
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    let report = Report {
        file: OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("stages.jsonl"))?,
        root,
        stages: Vec::new(),
        groups: Vec::new(),
        gpu_samples: 0,
    };
    let holder = Rc::new(RefCell::new(Some(report)));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("预览稳定性专项未完成".to_string())));
    let final_error = error.clone();
    Application::new().run(move |cx:&mut App| {
        let keepalive=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},|_,cx|cx.new(|_|KeepAlive)) {Ok(v)=>v,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        cx.spawn(async move |cx| {
            let mut report=holder.borrow_mut().take().expect("独占预览稳定性报告");
            let result:Result<()>=async {report.stage("cold-own-application",json!({"pid":std::process::id(),"hidden_keepalive":true,"real_production_preview":true,"synthetic_growth_fixture":true,"desktop_read":false,"global_input_sent":false}))?;
                for axis in [ScrollAxis::Vertical,ScrollAxis::Horizontal] {axis_case(axis,&mut report,cx).await?;} Ok(())}.await;
            let failure=result.err().map(|e|e.to_string());
            if failure.is_some() { if let Ok(windows)=cx.update(|cx|cx.windows()) {for window in windows {if let Some(preview)=window.downcast::<LongCapturePreview>() {let _=preview.update(cx,|_,w,_|w.remove_window());}}} for _ in 0..80 {tick(cx).await;} }
            *error.borrow_mut()=failure; *holder.borrow_mut()=Some(report);
            let _=keepalive.update(cx,|_,w,_|w.remove_window()); let _=cx.update(|cx|cx.quit());
        }).detach();
    });
    let failure = final_error.borrow_mut().take();
    if let Some(mut report) = final_holder.borrow_mut().take() {
        report.stage("after-application",json!({"application_returned_normally":true,"capture_threads":crate::long_capture_pipeline::active_capture_threads(),"consumer_sessions":crate::long_capture_runtime::active_workers(),"forced_process_termination":false}))?;
        let path = report.root.join("report.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        serde_json::to_writer_pretty(
            &mut file,
            &json!({"suite":"long-preview-stability-native","pid":std::process::id(),"ok":failure.is_none(),"error":failure,"groups":report.groups,"stages":report.stages,"data_dir":crate::preferences::data_dir(),"axes":2,"appends_per_axis":APPENDS,"planned_gpu_samples_per_axis":25,"actual_sampled_gpu_frames":report.gpu_samples,"sampling_scope":"request-next-frame GPU ROI before replacement plus settled next-frame ROI; not every physical presentation","physical_display_all_frames_verified":false,"synthetic_growth_fixture":true,"real_production_preview":true,"desktop_read":false,"clipboard_touched":false,"global_input_sent":false,"application_returned_normally":true,"forced_cleanup":false,"all_gpu_or_com_resources_verified":false,"broader_goal_complete":false}),
        )?;
        file.flush()?;
        println!(
            "{}",
            json!({"phase":"long_preview_stability_smoke_complete","pid":std::process::id(),"ok":failure.is_none(),"report":path})
        );
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}
