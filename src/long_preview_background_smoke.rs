//! 自有预览窗口的透明背景短专项；只读回小 GPU ROI，不采集桌面。
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

const WAIT: Duration = Duration::from_secs(20);
const PROPERTY: windows::core::PCWSTR = w!("RustShot.LongPreviewBackground.Owned");
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
fn ensure(ok: bool, error: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(error.into())
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
fn bounds_json(b: gpui::Bounds<gpui::Pixels>) -> Value {
    json!({"x":f32::from(b.origin.x),"y":f32::from(b.origin.y),"width":f32::from(b.size.width),"height":f32::from(b.size.height)})
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
        let row = json!({"name":name,"unix_ms":unix_ms()?,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail});
        serde_json::to_writer(&mut self.file, &row)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.stages.push(row);
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
            "只登记实际自有预览 HWND",
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
                && pid == std::process::id()
                && tid == self.tid
                && unsafe {
                    IsWindow(Some(self.h)).as_bool()
                        && GetPropW(self.h, PROPERTY).0 == Rc::as_ptr(&self.token) as *mut _
                },
            "原预览 HWND/PID/TID/token 已失效",
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
        46 + 7 * step as u8,
        118 + 7 * step as u8,
        192 - 7 * step as u8,
        255,
    ]
}
fn fixture(step: u32, axis: ScrollAxis, dpi: f32) -> LongPreview {
    let long = 240 * (step + 1);
    let cross = (200. * dpi.clamp(1., 2.5)).round() as u32;
    let projected_long = (long as f64 * f64::from(cross) / 400.).round() as u32;
    let (width, height, iw, ih) = if axis == ScrollAxis::Vertical {
        (400, long, cross, projected_long)
    } else {
        (long, 400, projected_long, cross)
    };
    let mut bgra = Vec::with_capacity(iw as usize * ih as usize * 4);
    for _ in 0..iw * ih {
        bgra.extend_from_slice(&color(step));
    }
    LongPreview {
        image: Arc::new(Image {
            width: iw,
            height: ih,
            bgra,
            readback_ms: 0.,
        }),
        revision: u64::from(step) + 1,
        generation: 1,
        width,
        height,
        viewport: Rect::full(width, height),
        axis,
        source_rect: Rect::full(width, height),
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
        "无法读取实际预览工作区",
    )?;
    Ok(Placement {
        x: info.rcWork.left,
        y: info.rcWork.top,
        width: (info.rcWork.right - info.rcWork.left) as u32,
        height: (info.rcWork.bottom - info.rcWork.top) as u32,
    })
}
fn state(
    preview: WindowHandle<LongCapturePreview>,
    owned: &Owned,
    cx: &mut AsyncApp,
) -> Result<(Placement, LongPreviewLifecycleProbe, Value)> {
    owned.check()?;
    preview.update(cx,|v,w,_|->Result<_> {
        ensure(hwnd(w)==owned.h,"GPUI 与原预览 HWND 不一致")?;
        if let Some(error)=&v.error {return Err(error.clone().into());}
        let native=crate::pin_native::rect(owned.h)?;
        Ok((native,v.lifecycle_probe(),json!({"ready":v.ready,"dpi":w.scale_factor(),"native_visible":unsafe {IsWindowVisible(owned.h)}.as_bool(),"placement":placement_json(v.placement),"native_rect":placement_json(native),"image_bounds":(*v.preview_bounds.borrow()).map(bounds_json),"label_bounds":v.label_bounds_for_qa().map(bounds_json),"displayed_revision":v.displayed.as_ref().map(|f|f.revision)})))
    })?
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
        let snapshot = state(preview, owned, cx)?;
        let painted = if revision == 0 {
            snapshot.1.render_images_created == 0
                && snapshot.2["image_bounds"].is_null()
                && snapshot.2["label_bounds"].is_null()
        } else {
            snapshot.1.painted_generation == 1
                && snapshot.1.painted_revision == revision
                && snapshot.1.retired_images == 0
                && !snapshot.2["label_bounds"].is_null()
        };
        if snapshot.2["ready"] == true && snapshot.2["native_visible"] == true && painted {
            return Ok(snapshot);
        }
        ensure(
            Instant::now() < end,
            "透明预览/实际 paint/标签/退休 ACK 超时",
        )?;
    }
}
async fn sample(
    report: &mut Report,
    preview: WindowHandle<LongCapturePreview>,
    owned: &Owned,
    roi: [u32; 4],
    name: &str,
    expected: [u8; 4],
    cx: &mut AsyncApp,
) -> Result<Value> {
    owned.check()?;
    preview.update(cx, |_, w, _| w.request_frame_capture(roi))??;
    let end = Instant::now() + WAIT;
    let bgra = loop {
        tick(cx).await;
        owned.check()?;
        if let Some(result) = preview.update(cx, |_, w, _| w.take_frame_capture())? {
            break result.map_err(|e| e.to_string())?;
        }
        ensure(Instant::now() < end, "自有 GPU 小 ROI 读回超时")?;
    };
    ensure(
        bgra.len() == roi[2] as usize * roi[3] as usize * 4,
        "GPU 样本长度错误",
    )?;
    let actual = bgra.get(..4).unwrap_or_default().to_vec();
    // 透明区要求真实 BGRA 全零，不能只检 alpha 后忽略隐藏 RGB。
    let matches = bgra.chunks_exact(4).all(|p| {
        p.iter().zip(expected).all(|(&a, b)| {
            if expected[3] == 0 {
                a == b
            } else {
                a.abs_diff(b) <= 1
            }
        })
    });
    let path = report.root.join(format!("{name}.png"));
    ensure(!path.exists(), "GPU PNG 路径已存在")?;
    crate::export::save_png(
        Image {
            width: roi[2],
            height: roi[3],
            bgra,
            readback_ms: 0.,
        },
        &path,
    )?;
    report.gpu_samples += 1;
    let proof = json!({"path":path,"bytes":std::fs::metadata(&path)?.len(),"width":roi[2],"height":roi[3],"roi":roi,"expected_bgra":expected,"first_actual_bgra":actual,"all_sample_pixels_verified":matches,"tolerance":if expected[3]==0 {0}else{1}});
    report.stage(
        "preview-background-gpu-sample",
        json!({"name":name,"sample":proof}),
    )?;
    ensure(matches, "GPU ROI 不是预期源色/透明全零")?;
    Ok(proof)
}
fn physical_bounds(
    preview: WindowHandle<LongCapturePreview>,
    cx: &mut AsyncApp,
) -> Result<([f32; 4], [f32; 4], f32)> {
    preview.update(cx, |v, w, _| -> Result<_> {
        let b = (*v.preview_bounds.borrow()).ok_or("实际图像 paint bounds 缺失")?;
        let l = v.label_bounds_for_qa().ok_or("实际尺寸胶囊 bounds 缺失")?;
        let s = w.scale_factor();
        Ok((
            [
                f32::from(b.origin.x) * s,
                f32::from(b.origin.y) * s,
                f32::from(b.size.width) * s,
                f32::from(b.size.height) * s,
            ],
            [
                f32::from(l.origin.x) * s,
                f32::from(l.origin.y) * s,
                f32::from(l.size.width) * s,
                f32::from(l.size.height) * s,
            ],
            s,
        ))
    })?
}
fn rois(shell: Placement, image: [f32; 4], axis: ScrollAxis) -> Result<([u32; 4], [[u32; 4]; 3])> {
    let center = [
        (image[0] + image[2] / 2. - 4.).round() as u32,
        (image[1] + image[3] / 2. - 4.).round() as u32,
        8,
        8,
    ];
    let blanks = if axis == ScrollAxis::Vertical {
        let x = center[0];
        let edge = image[1] + image[3];
        ensure(
            edge + 100. < shell.height as f32,
            "短图未留下足够未使用长轴空间",
        )?;
        [
            [x, (edge + 64.).ceil() as u32, 8, 8],
            [x, ((edge + shell.height as f32) / 2.).round() as u32, 8, 8],
            [x, shell.height - 12, 8, 8],
        ]
    } else {
        let edge = image[0] + image[2];
        ensure(
            edge + 100. < shell.width as f32,
            "短图未留下足够未使用长轴空间",
        )?;
        [
            [(edge + 64.).ceil() as u32, center[1], 8, 8],
            [shell.width - 12, center[1], 8, 8],
            [shell.width - 12, shell.height - 12, 8, 8],
        ]
    };
    for r in std::iter::once(&center).chain(blanks.iter()) {
        ensure(
            r[0] + r[2] <= shell.width && r[1] + r[3] <= shell.height,
            "自有 ROI 越出当前客户区",
        )?;
    }
    Ok((center, blanks))
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
        "当前工作区不足以提供有意义透明区域",
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
    let (h, dpi, entity_weak, ledger) = preview.update(cx, |v, w, cx| {
        (
            hwnd(w),
            w.scale_factor(),
            cx.entity().downgrade(),
            v.lifecycle_ledger(),
        )
    })?;
    let owned = Owned::register(h)?;
    let mut life = Lifetimes {
        images: Vec::new(),
        sources: Vec::new(),
    };
    let result:Result<Value>=async {
        let (shell,baseline,empty)=settled(preview,&owned,0,cx).await?;
        let empty_png=sample(report,preview,&owned,[shell.width/2-4,shell.height/2-4,8,8],&format!("{name}-empty"),[0;4],cx).await?;
        report.stage("preview-background-axis-opened",json!({"axis":name,"hwnd":h.0 as usize,"pid":owned.pid,"tid":owned.tid,"shell":placement_json(shell),"empty":empty,"baseline":baseline,"empty_png":empty_png}))?;
        let mut frames=Vec::new();
        for step in 0..=1 {
            preview.update(cx,|v,w,cx|v.set_frame(fixture(step,axis,dpi),toolbar,w,cx))??;
            let (native,probe,painted)=settled(preview,&owned,u64::from(step)+1,cx).await?; life.observe(preview,cx)?;
            ensure(native==shell && probe.native_placements==baseline.native_placements && probe.native_moves==baseline.native_moves && probe.native_resizes==baseline.native_resizes,"首帧/追加帧改变了预览原生外壳")?;
            ensure(probe.paint_failures==0 && probe.drop_image_failures==0 && probe.drop_image_successes==u64::from(step),"真实 paint/纹理退休计数异常")?;
            let (image,label,scale)=physical_bounds(preview,cx)?;
            ensure((scale-dpi).abs()<0.001,"专项限定 DPI 发生变化")?;
            // 胶囊贴当前图像下沿；不用原生窗口 footer 位置代替已绘制图像底部。
            let bottom=image[1]+image[3];
            let gap=if axis==ScrollAxis::Vertical {4.}else{16.};
            ensure((label[1]-bottom-gap*scale).abs()<=2.*scale && label[1]+label[3]<=shell.height as f32 && (label[0]-image[0]).abs()<=2.*scale,"尺寸胶囊未跟随实际图像底部或仍停在原生 footer")?;
            let (center,blank)=rois(shell,image,axis)?;
            let source_png=sample(report,preview,&owned,center,&format!("{name}-{step}-source"),color(step),cx).await?;
            let mut blank_pngs=Vec::new();
            for (index,roi) in blank.into_iter().enumerate() {blank_pngs.push(sample(report,preview,&owned,roi,&format!("{name}-{step}-blank-{index}"),[0;4],cx).await?);}
            let frame=json!({"step":step,"painted":painted,"probe":probe,"image_physical_bounds":image,"label_physical_bounds":label,"source_png":source_png,"blank_pngs":blank_pngs,"label_follows_image_bottom":true,"native_shell_unchanged":true});
            report.stage("preview-background-frame-verified",json!({"axis":name,"frame":frame}))?; frames.push(frame);
        }
        Ok(json!({"name":name,"ok":true,"hwnd":h.0 as usize,"pid":owned.pid,"tid":owned.tid,"shell":placement_json(shell),"empty_png":empty_png,"appends":1,"frames":frames,"final_probe":ledger.snapshot()}))
    }.await;
    life.observe(preview, cx)?;
    let request = unix_ms()?;
    report.stage(
        "preview-background-close-request",
        json!({"axis":name,"hwnd":h.0 as usize,"close_request_unix_ms":request}),
    )?;
    owned.check()?;
    preview.update(cx, |_, w, _| w.remove_window())?;
    let end = Instant::now() + WAIT;
    while unsafe { IsWindow(Some(h)).as_bool() }
        || entity_weak.upgrade().is_some()
        || !life.expired()
    {
        tick(cx).await;
        ensure(
            Instant::now() < end,
            "预览 HWND/Entity/source/image Weak 关闭未完成",
        )?;
    }
    let closed = unix_ms()?;
    report.stage("preview-background-closed",json!({"axis":name,"hwnd":h.0 as usize,"destroyed_observed_unix_ms":closed,"entity_removed":true,"owned_hwnd_destroyed":true,"all_observed_weaks_expired":true,"lifecycle":ledger.snapshot()}))?;
    let wait = Instant::now();
    while wait.elapsed() < Duration::from_millis(650) {
        tick(cx).await;
    }
    report.stage("preview-background-settled",json!({"axis":name,"hwnd":h.0 as usize,"post_close_wait_ms":wait.elapsed().as_millis(),"all_observed_weaks_expired":life.expired(),"lifecycle":ledger.snapshot()}))?;
    report.groups.push(result?);
    Ok(())
}
pub(crate) fn run() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("RUSTSHOT_LONG_PREVIEW_BACKGROUND_QA_DIR")
            .ok_or("缺少背景专项隔离目录")?,
    );
    ensure(root.is_absolute(), "QA 路径必须为绝对路径")?;
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    let settings = root.join("settings");
    std::fs::create_dir_all(&settings)?;
    if let Some(actual) = std::env::var_os("RUSTSHOT_DATA_DIR") {
        ensure(
            std::fs::canonicalize(actual)? == std::fs::canonicalize(&settings)?,
            "QA data 必须为本次 artifact/settings",
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
    let error = Rc::new(RefCell::new(Some("透明背景专项未完成".to_owned())));
    let final_error = error.clone();
    Application::new().run(move |cx:&mut App| {
        let keepalive=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},|_,cx|cx.new(|_|KeepAlive)){Ok(w)=>w,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        cx.spawn(async move |cx| {
            let mut report=holder.borrow_mut().take().expect("独占背景专项报告");
            let result:Result<()>=async {report.stage("cold-own-application",json!({"pid":std::process::id(),"hidden_keepalive":true,"real_production_preview":true,"synthetic_short_fixture":true,"desktop_read":false,"global_input_sent":false}))?; for axis in [ScrollAxis::Vertical,ScrollAxis::Horizontal] {axis_case(axis,&mut report,cx).await?;} Ok(())}.await;
            let failure=result.err().map(|e|e.to_string());
            if failure.is_some() {if let Ok(windows)=cx.update(|cx|cx.windows()){for window in windows {if let Some(preview)=window.downcast::<LongCapturePreview>(){let _=preview.update(cx,|_,w,_|w.remove_window());}}} for _ in 0..80 {tick(cx).await;}}
            *error.borrow_mut()=failure; *holder.borrow_mut()=Some(report); let _=keepalive.update(cx,|_,w,_|w.remove_window()); let _=cx.update(|cx|cx.quit());
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
            &json!({"suite":"long-preview-background-native","pid":std::process::id(),"ok":failure.is_none(),"error":failure,"groups":report.groups,"stages":report.stages,"data_dir":crate::preferences::data_dir(),"axes":2,"appends_per_axis":1,"planned_gpu_samples_per_axis":9,"actual_sampled_gpu_frames":report.gpu_samples,"sampling_scope":"own GPUI next-frame 8x8 BGRA ROIs; empty/source/unused-long-axis incl old native footer","physical_display_all_frames_verified":false,"synthetic_short_fixture":true,"real_production_preview":true,"desktop_read":false,"clipboard_touched":false,"global_input_sent":false,"application_returned_normally":true,"forced_cleanup":false,"all_gpu_or_com_resources_verified":false,"broader_goal_complete":false}),
        )?;
        file.flush()?;
        println!(
            "{}",
            json!({"phase":"long_preview_background_smoke_complete","pid":std::process::id(),"ok":failure.is_none(),"report":path})
        );
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}
