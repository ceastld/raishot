//! 超长图转贴图/历史/连续处理的实际 GPUI 验收，只使用自有合成像素。
use crate::{
    capture::Image,
    gpui_capture::hwnd,
    process_memory::{cpu_memory, gpu_memory},
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{
    div, prelude::*, App, AppContext, Application, AsyncApp, Render, Window, WindowHandle,
    WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
const WIDTH: u32 = 960;
const HEIGHT: u32 = 32_000;
const COLOR: [u8; 4] = [60, 100, 140, 255];
const WAIT: Duration = Duration::from_secs(20);
#[path = "pin_geometry_smoke.rs"]
mod geometry;
#[path = "long_projection_quality_smoke.rs"]
mod quality;
#[path = "pin_refill_smoke.rs"]
mod refill;
#[path = "pin_workspace_idle_smoke.rs"]
mod workspace_idle;
#[path = "pin_workspace_overlap_smoke.rs"]
mod workspace_overlap;
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
fn fixture() -> Image {
    Image {
        width: WIDTH,
        height: HEIGHT,
        bgra: COLOR.repeat((WIDTH * HEIGHT) as usize),
        readback_ms: 0.,
    }
}
struct Report {
    root: PathBuf,
    requested: Vec<String>,
    stages: Vec<serde_json::Value>,
    groups: Vec<serde_json::Value>,
}
impl Report {
    fn record(&mut self, stage: &str, detail: serde_json::Value) -> Result<()> {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        self.stages.push(serde_json::json!({"stage":stage,"pid":std::process::id(),"unix_ms":unix_ms,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail}));
        self.save(None, false)
    }
    fn save(&self, error: Option<&str>, complete: bool) -> Result<()> {
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "suite":"long-runtime-memory","ok":complete&&error.is_none(),"error":error,"pid":std::process::id(),
            "width":WIDTH,"height":HEIGHT,"raw_bytes":u64::from(WIDTH)*u64::from(HEIGHT)*4,
            "rounds":if self.requested.iter().any(|g|matches!(g.as_str(),"pin"|"geometry-reuse"|"geometry-borrowed"|"refill-reuse"|"refill-borrowed"|"workspace-overlap")){3}else{1},
                "real_dxgi_capture":false,"source":"owned-synthetic-32000px-Pin","clipboard_untouched":true,
                "requested_groups":self.requested,"groups":self.groups,"stages":self.stages,
                "atlas_residency_directly_queried":false,"worker_exit_observed_by_runner":false,
            "limitations":["合成长图实际GPUI/PNG/处理，不含真实滚动DXGI采集", "记录正常drop_image成功调用与进程级GPU用量，未直接查询ImageId驻留", "3轮有限验收不能证明长期无泄漏", "pin组使用均匀图；geometry/refill组使用非均色独立全像素期望，细分范围见各组记录", "geometry-borrowed/refill-borrowed是本版强制原借用路径的QA控制组，不是旧二进制的实测"]
            }))?,
        )?;
        Ok(())
    }
}
async fn presented(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    let done = Rc::new(Cell::new(false));
    let signal = done.clone();
    pin.update(cx, |_, w, _| {
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, _| signal.set(true));
            w.refresh();
        });
        w.refresh();
    })?;
    let end = Instant::now() + WAIT;
    while !done.get() {
        ensure(Instant::now() < end, "自有贴图呈现超时")?;
        tick(cx).await;
    }
    Ok(())
}
async fn ready(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if pin.update(cx, |v, w, _| -> Result<bool> {
            Ok(!v.processing
                && v.canvas.is_some()
                && v.status.is_empty()
                && v.placement(w)? == crate::pin_native::rect(hwnd(w))?)
        })?? {
            presented(pin, cx).await?;
            if pin.update(cx, |v, w, _| -> Result<bool> {
                Ok(!v.processing
                    && v.placement(w)? == crate::pin_native::rect(hwnd(w))?
                    && v.preview_texture_lifecycle().1 == 0)
            })?? {
                return Ok(());
            }
        }
        ensure(Instant::now() < end, "超长贴图处理/分块画布没有收敛")?;
    }
}
async fn pixel(
    pin: WindowHandle<PinView>,
    expected: [u8; 4],
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    pin.update(cx, |_, w, _| -> Result<()> {
        let bounds = crate::pin_native::rect(hwnd(w))?;
        ensure(bounds.width >= 8 && bounds.height >= 8, "贴图窗口太小")?;
        w.request_frame_capture([bounds.width / 2 - 1, bounds.height / 2 - 1, 3, 3])?;
        Ok(())
    })??;
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        if let Some(value) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
            let bytes = value.map_err(|e| e.to_string())?;
            ensure(bytes.len() == 36, "自有GPU抽样长度错误")?;
            let actual = &bytes[16..20];
            ensure(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 3),
                &format!("超长贴图实际GPU像素错误：{actual:?}/{expected:?}"),
            )?;
            return Ok(
                serde_json::json!({"actual":actual,"expected":expected,"source":"owned-gpui-framebuffer"}),
            );
        }
        ensure(Instant::now() < end, "自有GPU读回超时")?;
    }
}
async fn close(
    pin: WindowHandle<PinView>,
    weak: Weak<gpui::RenderImage>,
    cx: &mut AsyncApp,
) -> Result<()> {
    pin.update(cx, |v, w, cx| {
        v.perform(PinAction::Close, w, cx);
    })?;
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        let absent = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<PinView>().is_none())
        })?;
        if absent && weak.upgrade().is_none() {
            break;
        }
        ensure(Instant::now() < end, "关闭后原始长图/贴图窗口没有释放")?;
    }
    Ok(())
}
async fn verify(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let history = report.requested.iter().any(|g| g == "history");
    let process = report.requested.iter().any(|g| g == "pin");
    for round in 1..=if history || process { 3 } else { 0 } {
        let image = fixture();
        report.record(
            "before-pin",
            serde_json::json!({"round":round,"raw_bytes":image.bgra.len()}),
        )?;
        let pin = if history && round == 1 {
            cx.update(|cx| PinView::open_with_history(image, (100, 100), cx))??;
            cx.update(|cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|w| w.downcast::<PinView>())
                    .ok_or("没有历史贴图窗口")
            })??
        } else {
            cx.update(|cx| PinView::open_state(image, (100, 100), PinState::default(), cx))??
        };
        let weak = pin.update(cx, |v, w, _| {
            crate::gpui_smoke::isolate_input(hwnd(w))?;
            Ok::<_, Box<dyn std::error::Error>>(Arc::downgrade(&v.original))
        })??;
        ready(pin, cx).await?;
        let initial = pixel(pin, COLOR, cx).await?;
        report.record("pin-ready", serde_json::json!({"round":round,"pixel":initial,"lifecycle":pin.update(cx,|v,_,_|v.preview_texture_lifecycle())?}))?;
        if history && round == 1 {
            let end = Instant::now() + WAIT;
            let path = loop {
                let entries = cx
                    .background_executor()
                    .spawn(async { crate::history::entries().map_err(|e| e.to_string()) })
                    .await
                    .map_err(|e| e.to_string())?;
                let owned_pid = format!("-{}-", std::process::id());
                if let Some(entry) = entries.iter().find(|entry| {
                    entry
                        .path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(&owned_pid))
                }) {
                    break entry.path.clone();
                }
                ensure(Instant::now() < end, "借用共享长图的历史编码未完成")?;
                tick(cx).await;
            };
            let decoded = cx
                .background_executor()
                .spawn(async move { crate::export::read_image(&path).map_err(|e| e.to_string()) })
                .await
                .map_err(|e| e.to_string())?;
            ensure(
                (decoded.width, decoded.height) == (WIDTH, HEIGHT),
                "长图历史PNG尺寸错误",
            )?;
            for y in [0, 4095, 4096, 16383, 16384, 31999] {
                let start = ((y * WIDTH + WIDTH / 2) * 4) as usize;
                ensure(
                    decoded.bgra[start..start + 4] == COLOR,
                    "长图历史PNG跨块像素错误",
                )?;
            }
            drop(decoded);
            report.groups.push(serde_json::json!({"name":"shared-32000px-pin-history-png","ok":true,"sampled_rows":6,"history_input_arc_shared":true}));
            report.record(
                "history-encoded",
                serde_json::json!({"full_qa_decode_released":true}),
            )?;
        }
        if !process {
            close(pin, weak, cx).await?;
            break;
        }
        let mut count = 0;
        for (name, action, expected) in [
            ("brightness", PinAction::Brightness(10), [85, 125, 165, 255]),
            ("flip", PinAction::FlipX, [85, 125, 165, 255]),
            ("undo", PinAction::Undo, [85, 125, 165, 255]),
            ("redo", PinAction::Redo, [85, 125, 165, 255]),
            ("rotate", PinAction::Rotate(1), [85, 125, 165, 255]),
            ("reset", PinAction::ResetImage, COLOR),
        ] {
            pin.update(cx, |v, w, cx| v.perform(action, w, cx))?;
            ready(pin, cx).await?;
            let pixels = pixel(pin, expected, cx).await?;
            let life = pin.update(cx, |v, _, _| v.preview_texture_lifecycle())?;
            ensure(life.1 == 0, "已显示后仍保留退休队列")?;
            ensure(life.3 > count, "替换长图未执行正常纹理释放")?;
            count = life.3;
            report.record(
                "processed",
                serde_json::json!({"round":round,"action":name,"pixels":pixels,"lifecycle":life}),
            )?;
        }
        close(pin, weak, cx).await?;
        ensure(
            crate::long_capture_runtime::active_workers() == 0,
            "关闭后还有采集线程",
        )?;
        report.record("pin-closed-source-released",serde_json::json!({"round":round,"source_weak_expired":true,"pin_windows":0,"capture_workers":0}))?;
    }
    if process {
        report.groups.push(serde_json::json!({"name":"three-rounds-32000px-pin-process-retire-close","ok":true,"rounds":3,"processed_frames":18,"original_arc_released_each_round":true}));
    }
    if report.requested.iter().any(|g| g == "quality") {
        quality::verify(report, cx).await?;
    }
    if report.requested.iter().any(|g| g == "geometry-reuse") {
        geometry::verify(report, cx, false).await?;
    }
    if report.requested.iter().any(|g| g == "geometry-borrowed") {
        geometry::verify(report, cx, true).await?;
    }
    if report.requested.iter().any(|g| g == "geometry-shared") {
        geometry::verify_shared(report, cx).await?;
    }
    if report.requested.iter().any(|g| g == "refill-reuse") {
        refill::verify(report, cx, false).await?;
    }
    if report.requested.iter().any(|g| g == "refill-borrowed") {
        refill::verify(report, cx, true).await?;
    }
    if report.requested.iter().any(|g| g == "refill-shared") {
        refill::verify_shared(report, cx).await?;
    }
    if report.requested.iter().any(|g| g == "workspace-overlap") {
        workspace_overlap::verify(report, cx).await?;
    }
    if report.requested.iter().any(|g| g == "workspace-idle") {
        workspace_idle::verify(report, cx).await?;
    }
    Ok(())
}
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_LONG_RUNTIME_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/long-runtime-memory"));
    std::fs::create_dir_all(root.join("settings"))?;
    std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    crate::preferences::Preferences {
        history: true,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    let request =
        std::env::var("RUSTSHOT_LONG_RUNTIME_GROUPS").unwrap_or_else(|_| "pin,quality".into());
    let requested: Vec<_> = request.split(',').map(|g| g.trim().to_string()).collect();
    ensure(
        !requested.is_empty()
            && requested.iter().all(|g| {
                matches!(
                    g.as_str(),
                    "history"
                        | "pin"
                        | "quality"
                        | "geometry-reuse"
                        | "geometry-borrowed"
                        | "geometry-shared"
                        | "refill-reuse"
                        | "refill-borrowed"
                        | "refill-shared"
                        | "workspace-overlap"
                        | "workspace-idle"
                )
            })
            && requested.iter().filter(|g| g.as_str() == "history").count() <= 1
            && requested.iter().filter(|g| g.as_str() == "pin").count() <= 1
            && requested.iter().filter(|g| g.as_str() == "quality").count() <= 1,
        "未知/重复超长图专项组",
    )?;
    ensure(
        !requested.iter().any(|g| {
            g.starts_with("geometry-")
                || g.starts_with("refill-")
                || g == "workspace-overlap"
                || g == "workspace-idle"
        }) || requested.len() == 1,
        "几何内存控制组必须在各自独立工作进程中执行",
    )?;
    let mut report = Report {
        root,
        requested,
        stages: Vec::new(),
        groups: Vec::new(),
    };
    report.record("before-application", serde_json::json!({}))?;
    let holder = Rc::new(RefCell::new(Some(report)));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("超长图内存专项未完成".to_string())));
    let final_error = error.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            if let Err(e) = cx.open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Host),
            ) {
                *error.borrow_mut() = Some(e.to_string());
                cx.quit();
                return;
            }
            cx.spawn(async move |cx| {
                let mut report = holder.borrow_mut().take().expect("独占QA报告");
                *error.borrow_mut() = verify(&mut report, cx).await.err().map(|e| e.to_string());
                *holder.borrow_mut() = Some(report);
                let _ = cx.update(|cx| {
                    for window in cx.windows() {
                        let _ = cx.update_window(window, |_, w, _| w.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    let error = final_error.borrow_mut().take();
    if let Some(mut report) = final_holder.borrow_mut().take() {
        report.record(
            "after-application",
            serde_json::json!({"capture_workers":crate::long_capture_runtime::active_workers()}),
        )?;
        report.save(error.as_deref(), true)?;
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}

pub(crate) fn run_workspace_overlap() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("RUSTSHOT_WORKSPACE_OVERLAP_QA_DIR")
            .ok_or("工作区重叠专项缺少隔离目录")?,
    );
    std::fs::create_dir_all(&root)?;
    let allowed = std::env::current_dir()?.join("target/qa/workspace-overlap-native");
    std::fs::create_dir_all(&allowed)?;
    let root = std::fs::canonicalize(root)?;
    ensure(
        root.starts_with(std::fs::canonicalize(allowed)?),
        "工作区重叠专项须在隔离 QA 目录内",
    )?;
    std::env::set_var("RUSTSHOT_LONG_RUNTIME_QA_DIR", &root);
    std::env::set_var("RUSTSHOT_LONG_RUNTIME_GROUPS", "workspace-overlap");
    run()?;
    println!(
        "{}",
        serde_json::json!({"phase":"workspace_overlap_smoke_complete","pid":std::process::id(),"report":root.join("report.json"),"ok":true,"synthetic_only":true})
    );
    Ok(())
}

pub(crate) fn run_workspace_idle() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("RUSTSHOT_PIN_WORKSPACE_IDLE_QA_DIR")
            .ok_or("贴图静置保存专项缺少隔离目录")?,
    );
    std::fs::create_dir_all(&root)?;
    let allowed = std::env::current_dir()?.join("target/qa/pin-autosave-idle-native");
    std::fs::create_dir_all(&allowed)?;
    let root = std::fs::canonicalize(root)?;
    ensure(
        root.starts_with(std::fs::canonicalize(allowed)?),
        "贴图静置保存专项须在隔离QA目录内",
    )?;
    std::env::set_var("RUSTSHOT_LONG_RUNTIME_QA_DIR", &root);
    std::env::set_var("RUSTSHOT_LONG_RUNTIME_GROUPS", "workspace-idle");
    run()?;
    println!(
        "{}",
        serde_json::json!({"phase":"workspace_idle_smoke_complete","pid":std::process::id(),"report":root.join("report.json"),"ok":true,"synthetic_only":true})
    );
    Ok(())
}
