//! 同一 Rust 进程三轮真实 owned Chrome 32k 采集、编辑、贴图生命周期。
#[path = "long_browser_native_stress_smoke.rs"]
pub(crate) mod native_stress;
#[path = "long_browser_nested_smoke.rs"]
pub(crate) mod nested;
#[path = "long_browser_ui_auto_smoke.rs"]
pub(crate) mod ui_auto;
use crate::{
    capture::Image,
    geometry::ScreenRect,
    gpui_capture::hwnd,
    gpui_smoke::Input,
    live_capture::LiveCapture,
    long_browser_owner::{BrowserCaptureOwner, BrowserLease, BrowserOwnerConfig},
    long_browser_stress_oracle as oracle, long_capture_pipeline,
    long_capture_runtime::{self, FrameGate, LongOptions, LongPhase, LongSnapshot},
    long_capture_view::LongCaptureView,
    process_memory::{cpu_memory, gpu_memory},
    pin_view::{PinAction, PinView},
    stitch::{ScrollAxis, StitchOptions, StitchSnapshot},
    Result,
};
use gpui::{
    div, App, AppContext, Application, AsyncApp, Context, Render, Window, WindowHandle,
    WindowOptions,
};
use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::{BufRead, BufWriter, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Condvar, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        HiDpi::{
            AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
            SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_UNAWARE,
        },
        WindowsAndMessaging::{
            GetClientRect, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
        },
    },
};
const WAIT: Duration = Duration::from_secs(30);
const CLOSE_STABILITY_BYTES: u64 = 16 * 1024 * 1024;
/// 专项只改变自有后台线程；同步调用完成或失败均恢复进入前的上下文。
struct RestoreThreadDpi(DPI_AWARENESS_CONTEXT);
impl Drop for RestoreThreadDpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn rect_json(r: ScreenRect) -> Value {
    json!({"left":r.left,"top":r.top,"width":r.width,"height":r.height})
}
#[derive(Default)]
struct Inbox {
    owner: Option<Arc<BrowserCaptureOwner>>,
    config: Option<Value>,
    lease: Option<Value>,
    sequence: u64,
    replies: HashMap<u64, Value>,
    disconnected: bool,
    error: Option<String>,
    rejected_heartbeats: u64,
    last_rejected_heartbeat: Option<String>,
}
struct Bridge {
    inbox: Arc<(Mutex<Inbox>, Condvar)>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}
impl Bridge {
    fn new() -> Result<Self> {
        let inbox = Arc::new((Mutex::new(Inbox::default()), Condvar::new()));
        let reader_inbox = inbox.clone();
        let reader = thread::Builder::new()
            .name("rustshot-browser-protocol".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let input = std::io::stdin();
                    for line in input.lock().lines() {
                        let line = line?;
                        ensure(line.len() <= 256 * 1024, "浏览器协议单行超过限制")?;
                        if line.trim().is_empty() {
                            continue;
                        }
                        let packet: Value = serde_json::from_str(&line)?;
                        let (lock, wake) = &*reader_inbox;
                        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        match packet["type"].as_str() {
                            Some("config") => {
                                ensure(
                                    packet["protocol"] == 1 && state.config.is_none(),
                                    "重复或未知浏览器配置协议",
                                )?;
                                let config = packet["config"].clone();
                                let native: BrowserOwnerConfig =
                                    serde_json::from_value(config["native_owner"].clone())?;
                                state.owner = Some(BrowserCaptureOwner::from_config(native)?);
                                state.config = Some(config);
                            }
                            Some("new_lease" | "lease") => {
                                ensure(packet["protocol"] == 1, "lease协议不符")?;
                                let sequence = packet["seq"].as_u64().ok_or("lease序号缺失")?;
                                ensure(sequence > state.sequence, "lease序号没有递增")?;
                                state.sequence = sequence;
                                let owner = state
                                    .owner
                                    .as_ref()
                                    .ok_or("lease早于原生owner配置")?
                                    .clone();
                                if packet["valid"].as_bool() != Some(true) {
                                    owner.revoke(
                                        packet["reason"].as_str().unwrap_or("Node明确撤销lease"),
                                    );
                                    state.lease = None;
                                } else {
                                    let mut fields =
                                        packet.as_object().ok_or("lease不是对象")?.clone();
                                    for field in ["type", "protocol", "seq", "valid", "reason"] {
                                        fields.remove(field);
                                    }
                                    let lease: BrowserLease =
                                        serde_json::from_value(Value::Object(fields))?;
                                    if packet["type"] == "new_lease" {
                                        owner.install_lease(lease)?;
                                        state.lease = Some(packet.clone());
                                    } else {
                                        // 原生遮挡会永久撤销 token；旧 heartbeat 不得续回，也不能杀掉
                                        // 用于显式 new_lease/Browser.close/正常 EOF 的协议连接。
                                        match owner.update_lease(lease) {
                                            Ok(()) => state.lease = Some(packet.clone()),
                                            Err(error) => {
                                                state.lease = None;
                                                state.rejected_heartbeats =
                                                    state.rejected_heartbeats.saturating_add(1);
                                                state.last_rejected_heartbeat =
                                                    Some(error.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                            Some("response") => {
                                ensure(packet["protocol"] == 1, "response协议不符")?;
                                let id = packet["id"].as_u64().ok_or("response没有id")?;
                                ensure(!state.replies.contains_key(&id), "response id重复")?;
                                state.replies.insert(id, packet);
                            }
                            Some("revoke_lease" | "revoke" | "revoked" | "abort") => {
                                let reason =
                                    packet["reason"].as_str().unwrap_or("浏览器控制器撤销会话");
                                if let Some(owner) = &state.owner {
                                    owner.revoke(reason);
                                }
                                state.lease = None;
                                if packet["type"] == "abort" {
                                    state.error = Some(reason.to_owned());
                                }
                            }
                            _ => return Err("未知浏览器协议消息".into()),
                        }
                        wake.notify_all();
                    }
                    Ok(())
                })();
                let (lock, wake) = &*reader_inbox;
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(owner) = &state.owner {
                    owner.disconnect();
                }
                if let Err(error) = result {
                    state.error = Some(error.to_string());
                }
                state.disconnected = true;
                wake.notify_all();
            })?;
        Ok(Self {
            inbox,
            reader: Some(reader),
            next_id: 0,
        })
    }
    fn wait<T>(&self, mut ready: impl FnMut(&mut Inbox) -> Option<T>) -> Result<T> {
        let until = Instant::now() + WAIT;
        let (lock, wake) = &*self.inbox;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = ready(&mut state) {
                return Ok(value);
            }
            if let Some(error) = &state.error {
                return Err(error.clone().into());
            }
            ensure(!state.disconnected, "浏览器协议连接提前关闭")?;
            ensure(Instant::now() < until, "浏览器控制协议等待超时")?;
            state = wake
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn initial(&self) -> Result<(Arc<BrowserCaptureOwner>, Value)> {
        self.wait(|state| {
            Some((state.owner.clone()?, state.config.clone()?)).filter(|_| state.lease.is_some())
        })
    }
    fn request(&mut self, op: &str, fields: Value) -> Result<Value> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("browser request id溢出")?;
        let id = self.next_id;
        let mut request = json!({"type":"request","protocol":1,"id":id,"op":op});
        if let Some(fields) = fields.as_object() {
            for (key, value) in fields {
                ensure(
                    !["type", "protocol", "id", "op"].contains(&key.as_str()),
                    "request字段覆盖协议身份",
                )?;
                request[key] = value.clone();
            }
        }
        let mut out = std::io::stdout().lock();
        writeln!(out, "{request}")?;
        out.flush()?;
        drop(out);
        let response = self.wait(|state| state.replies.remove(&id))?;
        ensure(
            response["ok"].as_bool() == Some(true),
            &format!("浏览器操作{op}失败：{response}"),
        )?;
        Ok(response)
    }
    fn finish(&mut self) -> Result<()> {
        self.request("finish", json!({}))?;
        let until = Instant::now() + WAIT;
        while self
            .reader
            .as_ref()
            .is_some_and(|reader| !reader.is_finished())
        {
            ensure(Instant::now() < until, "browser stdin线程未收到正常EOF")?;
            thread::sleep(Duration::from_millis(5));
        }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| "browser协议线程panic")?;
        }
        let state = self.inbox.0.lock().unwrap_or_else(|e| e.into_inner());
        ensure(
            state.disconnected && state.error.is_none(),
            &format!("正常EOF前协议失败：{:?}", state.error),
        )?;
        Ok(())
    }
    fn probe(&self) -> Value {
        let state = self.inbox.0.lock().unwrap_or_else(|e| e.into_inner());
        json!({"last_lease_sequence":state.sequence,"rejected_old_heartbeats":state.rejected_heartbeats,
            "last_rejected_heartbeat":state.last_rejected_heartbeat,"disconnected":state.disconnected,
            "reader_joined":self.reader.is_none(),"error":state.error})
    }
}

type SharedBridge = Arc<Mutex<Bridge>>;
async fn request(
    bridge: &SharedBridge,
    op: &str,
    fields: Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let bridge = bridge.clone();
    let op = op.to_owned();
    cx.background_executor()
        .spawn(async move {
            bridge
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request(&op, fields)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(Into::into)
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(10))
        .await;
}
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
struct Report {
    root: PathBuf,
    owner: Arc<BrowserCaptureOwner>,
    stages: Vec<Value>,
    cases: Vec<Value>,
    protocol: Value,
    close_private: Vec<u64>,
    startup_config: Value,
}
#[derive(Serialize)]
struct CompletedGroup<'a> {
    name: &'static str,
    ok: bool,
    cases: &'a [Value],
}
#[derive(Serialize)]
struct WheelProbe<'a> {
    cycle: u32,
    step: u32,
    ack: &'a Value,
}
impl Report {
    fn new(config: Value, owner: Arc<BrowserCaptureOwner>) -> Result<Self> {
        let root = PathBuf::from(
            config["artifact_root"]
                .as_str()
                .ok_or("artifact_root缺失")?,
        )
        .canonicalize()?;
        let base = std::env::current_dir()?.join("target/qa").canonicalize()?;
        ensure(
            root.starts_with(&base) && root != base,
            "stress记录目录不在独立QA子目录",
        )?;
        Ok(Self {
            root,
            owner,
            stages: vec![],
            cases: vec![],
            protocol: Value::Null,
            close_private: vec![],
            startup_config: config,
        })
    }
    fn save(&self, complete: bool, error: Option<&str>) -> Result<()> {
        let success = complete && error.is_none();
        // 借用记录直接写文件；测量进程不能为报告克隆整棵 Value 再分配整份 JSON。
        let mut writer = BufWriter::new(std::fs::File::create(self.root.join("report.json"))?);
        let mut serializer = serde_json::Serializer::pretty(&mut writer);
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("suite", "long-browser-stress")?;
        map.serialize_entry("pid", &std::process::id())?;
        map.serialize_entry("ok", &success)?;
        map.serialize_entry("error", &error)?;
        map.serialize_entry("requested_groups", &["browser-stress-three-cycles"])?;
        let groups = if success {
            vec![CompletedGroup {
                name: "browser-stress-three-cycles",
                ok: true,
                cases: &self.cases,
            }]
        } else {
            vec![]
        };
        map.serialize_entry("groups", &groups)?;
        map.serialize_entry("cases", &self.cases)?;
        map.serialize_entry("stages", &self.stages)?;
        map.serialize_entry("cycles", &3)?;
        map.serialize_entry("width", &960)?;
        map.serialize_entry("height", &32000)?;
        map.serialize_entry("real_browser_compositor", &true)?;
        map.serialize_entry("real_dxgi_capture", &!self.cases.is_empty())?;
        map.serialize_entry("full_desktop_freeze_used", &false)?;
        map.serialize_entry("fixture_substituted", &false)?;
        map.serialize_entry("global_input_sent", &false)?;
        map.serialize_entry("clipboard_untouched", &true)?;
        map.serialize_entry("history_disabled", &true)?;
        map.serialize_entry("native_owner", &self.owner.native_snapshot())?;
        map.serialize_entry("ownership", &self.owner.probe())?;
        map.serialize_entry(
            "capture_threads_after_close",
            &long_capture_pipeline::active_capture_threads(),
        )?;
        map.serialize_entry(
            "consumer_sessions_after_close",
            &long_capture_runtime::active_workers(),
        )?;
        map.serialize_entry(
            "panicked_threads",
            &long_capture_runtime::panicked_workers(),
        )?;
        map.serialize_entry("protocol_thread_joined", &self.protocol["reader_joined"])?;
        map.serialize_entry("protocol", &self.protocol)?;
        map.serialize_entry("close_private_bytes", &self.close_private)?;
        map.serialize_entry(
            "pre_registered_close_cycle1_to_cycle3_max_growth_bytes",
            &CLOSE_STABILITY_BYTES,
        )?;
        map.serialize_entry(
            "runtime_memory_sample_source",
            "current RustShot GPUI worker PID; browser process excluded",
        )?;
        map.serialize_entry(
            "source_capacity_scope",
            "snapshot可达unique Arc<Image> Vec.capacity，非进程总内存",
        )?;
        map.serialize_entry("renderer_sha256_computed_by_node", &true)?;
        map.serialize_entry("renderer_sha256_recomputed_by_rust", &false)?;
        map.serialize_entry("atlas_residency_directly_queried", &false)?;
        map.serialize_entry("normal_worker_exit_observed_by_runner", &false)?;
        map.serialize_entry("wheel_probe_jsonl", "wheel-probes.jsonl")?;
        map.serialize_entry("wheel_probe_history_retained_in_process", &false)?;
        map.serialize_entry("report_serialization_borrows_values", &true)?;
        map.serialize_entry("limitations", &[
            "隔离Chrome自有确定性32k页面，固定DPR1.25；不代表第三方网站/混合DPI/HDR/动画",
            "严格browser gate不豁免HUD；本QA在初始化等待期销毁ROI内部生产边框，只实测选区外工具栏和预览",
            "DOM wheel由own Page CDP路由，工具栏Edit/Return/Pin用自有HWND posted mouse，不证明硬件输入",
            "长图逐行独立oracle；预览GPU只抽样实际呈现像素，不宣称全图GPU与原世界逐像素一致",
            "drop_image成功调用不等于直接查询atlas驻留；GPUI驱动缓存可保留",
            "3轮有限关闭稳定性，不证明长期无泄漏；stage最大值非连续采样峰值，runner另测PID峰值",
            "不导出第二份整图作期望；生产Pin移交物化原长图属于实际功能成本",
        ])?;
        map.end()?;
        writer.flush()?;
        Ok(())
    }
    fn wheel_probe(&self, cycle: u32, step: u32, ack: &Value) -> Result<()> {
        // 完整逐步探针落盘，仅在内存保留成功元数据与有界 scalar 摘要。
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("wheel-probes.jsonl"))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &WheelProbe { cycle, step, ack })?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        let mut stage = json!({"stage":name,"pid":std::process::id(),
            "unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            "cpu":cpu_memory()?,"gpu":gpu_memory(),"ownership":self.owner.probe()});
        stage
            .as_object_mut()
            .unwrap()
            .insert("detail".into(), detail);
        self.stages.push(stage);
        self.save(false, None)
    }
}
fn read_renderer(response: &Value, root: &Path) -> Result<Image> {
    let d = &response["renderer"];
    let path = PathBuf::from(d["path"].as_str().ok_or("renderer PNG路径缺失")?).canonicalize()?;
    ensure(
        path.starts_with(root)
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("png")),
        "renderer PNG越过本次记录目录",
    )?;
    ensure(
        std::fs::metadata(&path)?.len() <= 8 * 1024 * 1024,
        "renderer PNG超过有界上限",
    )?;
    ensure(
        d["sha256"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "renderer hash缺失",
    )?;
    let image = crate::export::read_image(&path)?;
    ensure(
        image.width == 960
            && image.height == 640
            && d["pixel_width"] == 960
            && d["pixel_height"] == 640,
        "renderer尺寸并非960×640",
    )?;
    Ok(image)
}
fn physical_scroll(state: &Value, cycle: u32) -> Result<u32> {
    ensure(
        state["pattern_id"] == "bgra-world-tiled-v2"
            && state["dpr"].as_f64() == Some(1.25)
            && state["cycle"] == cycle,
        "DOM pattern/DPR/cycle改变",
    )?;
    let p = &state["physical_spec"];
    ensure(
        p["width"] == 960
            && p["viewport_height"] == 640
            && p["document_height"] == 32000
            && p["tile_height"] == 1000,
        "DOM physical spec改变",
    )?;
    let pool = &state["pool"];
    ensure(
        pool["slot_count"] == 3
            && pool["backing_bytes"] == 11520000
            && pool["scratch_bytes"] == 3840000
            && pool["ready"] == true,
        "DOM tile pool未settle或预算改变",
    )?;
    let y = state["scroll_top"].as_f64().ok_or("DOM scrollTop缺失")? * 1.25;
    ensure(
        y.is_finite() && (0. ..=31360.).contains(&y) && (y - y.round()).abs() < 0.000001,
        "DOM实际物理scrollTop非法",
    )?;
    ensure(
        pool["visible_world_range"]["first_row"].as_f64() == Some(y)
            && pool["visible_world_range"]["row_count"] == 640,
        "DOM visible world range不符",
    )?;
    Ok(y.round() as u32)
}
fn close_rect(a: ScreenRect, b: ScreenRect) -> bool {
    i64::from(a.left) < i64::from(b.left) + i64::from(b.width)
        && i64::from(b.left) < i64::from(a.left) + i64::from(a.width)
        && i64::from(a.top) < i64::from(b.top) + i64::from(b.height)
        && i64::from(b.top) < i64::from(a.top) + i64::from(a.height)
}
fn native_rect(handle: usize) -> Result<ScreenRect> {
    let h = HWND(handle as *mut _);
    let mut pid = 0;
    let mut r = RECT::default();
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
        GetWindowRect(h, &mut r)?;
    }
    ensure(
        pid == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
        "UI HWND不属于本进程",
    )?;
    Ok(ScreenRect {
        left: r.left,
        top: r.top,
        width: u32::try_from(i64::from(r.right) - i64::from(r.left))?,
        height: u32::try_from(i64::from(r.bottom) - i64::from(r.top))?,
    })
}
fn read_owned(region: ScreenRect, gate: FrameGate) -> Result<Image> {
    gate()?;
    let cancelled = || gate().is_err();
    let live = LiveCapture::new_for_region_cancellable(region, &cancelled)?;
    let image = live.read_cancellable(region, &cancelled)?;
    gate()?;
    Ok(image)
}
// 与任意renderer子矩形比较；9点仅粗筛，每个候选必须完整逐字节确认。
fn calibrate(client: &Image, renderer: &Image) -> Result<(u32, u32)> {
    ensure(
        renderer.width <= client.width && renderer.height <= client.height,
        "renderer无法放入owned client",
    )?;
    let anchors = [
        (3, 3),
        (480, 3),
        (956, 3),
        (3, 320),
        (480, 320),
        (956, 320),
        (3, 636),
        (480, 636),
        (956, 636),
    ];
    let same = |x: u32, y: u32, px: u32, py: u32| {
        let a = ((y + py) as usize * client.width as usize + (x + px) as usize) * 4;
        let b = (py as usize * renderer.width as usize + px as usize) * 4;
        client.bgra[a..a + 4]
            .iter()
            .zip(&renderer.bgra[b..b + 4])
            .all(|(&a, &b)| a.abs_diff(b) <= 1)
    };
    let mut found = None;
    for y in 0..=client.height - renderer.height {
        for x in 0..=client.width - renderer.width {
            if !anchors.iter().all(|&(px, py)| same(x, y, px, py)) {
                continue;
            }
            if (0..renderer.height).all(|py| (0..renderer.width).all(|px| same(x, y, px, py))) {
                ensure(
                    found.is_none(),
                    "ownedclient renderer存在多个完整匹配，拒绝猜ROI",
                )?;
                found = Some((x, y));
            }
        }
    }
    found.ok_or_else(|| "ownedclient内没有唯一完整renderer匹配".into())
}
#[derive(Default)]
struct SourceTrace {
    weaks: Vec<Weak<Image>>,
    seen: HashSet<usize>,
}
impl SourceTrace {
    fn observe(&mut self, snapshot: &StitchSnapshot) {
        for s in &snapshot.segments {
            if self.seen.insert(Arc::as_ptr(&s.image) as usize) {
                self.weaks.push(Arc::downgrade(&s.image));
            }
        }
    }
}
async fn presented<T: 'static + Render>(handle: WindowHandle<T>, cx: &mut AsyncApp) -> Result<()> {
    let ready = Rc::new(Cell::new(false));
    let out = ready.clone();
    handle.update(cx, |_, w, _| {
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, _| out.set(true));
            w.refresh();
        });
        w.refresh();
    })?;
    let end = Instant::now() + WAIT;
    while !ready.get() {
        ensure(Instant::now() < end, "GPUI双帧呈现超时")?;
        tick(cx).await;
    }
    Ok(())
}
async fn frame_sample<T: 'static + Render>(
    handle: WindowHandle<T>,
    requested: Option<[u32; 4]>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let area = handle.update(cx, |_, w, _| -> Result<_> {
        let r = native_rect(hwnd(w).0 as usize)?;
        ensure(r.width >= 8 && r.height >= 8, "GPUI窗口过小")?;
        let area = requested.unwrap_or([r.width / 2 - 1, r.height / 2 - 1, 3, 3]);
        ensure(
            area[0] + area[2] <= r.width && area[1] + area[3] <= r.height,
            "GPUI抽样超出自有窗口",
        )?;
        w.request_frame_capture(area)?;
        Ok(area)
    })??;
    let end = Instant::now() + WAIT;
    loop {
        if let Some(p) = handle.update(cx, |_, w, _| w.take_frame_capture())? {
            let p = p.map_err(|e| e.to_string())?;
            ensure(
                p.len() == 36 && p.chunks_exact(4).all(|p| p[3] == 255),
                "实际GPUI framebuffer抽样非法",
            )?;
            return Ok(
                json!({"source":"owned-gpui-framebuffer","physical_area":area,"bgra":p,"full_pixel_oracle":false}),
            );
        }
        ensure(Instant::now() < end, "实际GPUI framebuffer读回超时")?;
        tick(cx).await;
    }
}
async fn panel_wait(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    predicate: impl Fn(&LongSnapshot, Option<&long_capture_pipeline::PipelineProbe>) -> bool,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + WAIT;
    loop {
        // 轮询只读类型化状态；完整历史仅在成功ACK或失败时序列化。
        // 成功路径不能逐次分配整份JSON及两份错误字符串，污染关闭内存测量。
        let (m, p) = panel.update(cx, |v, _, _| {
            (v.snapshot(), v.session.as_ref().map(|s| s.pipeline_probe()))
        })?;
        if predicate(&m, p.as_ref()) {
            return Ok(json!({"meta":format!("{m:?}"),"pipeline":p}));
        }
        if m.phase == LongPhase::Failed {
            return Err(format!("{label}失败：{m:?}/{p:?}").into());
        }
        if Instant::now() >= end {
            return Err(format!("{label}超时：{m:?}/{p:?}").into());
        }
        tick(cx).await;
    }
}
fn committed(m: &LongSnapshot) -> bool {
    m.result_available && m.generation == m.requested_generation && m.preview_revision > 0
}
async fn quiet(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<Value> {
    panel.update(cx, |v, _, _| {
        v.session
            .as_mut()
            .ok_or("无实际LongSession")?
            .set_paused(true)
    })??;
    panel_wait(
        panel,
        "Pause排空",
        |m, p| {
            committed(m)
                && !m.busy
                && m.phase == LongPhase::Paused
                && p.is_some_and(|p| {
                    p.producer_quiesced
                        && !p.capture_in_flight
                        && !p.consumer_in_flight
                        && p.queue_depth == 0
                        && p.reserved_raw_bytes == 0
                        && p.drain_fence.is_none()
                })
        },
        cx,
    )
    .await
}
async fn resume(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<()> {
    panel.update(cx, |v, _, _| {
        v.session
            .as_mut()
            .ok_or("无实际LongSession")?
            .set_paused(false)
    })??;
    panel_wait(
        panel,
        "检查点恢复真实采集",
        |m, _| committed(m) && m.phase == LongPhase::Running,
        cx,
    )
    .await?;
    Ok(())
}
fn post_click(input: Input, p: (f32, f32)) -> Result<()> {
    native_rect(input.handle)?;
    ensure(
        unsafe { IsWindowVisible(HWND(input.handle as *mut _)) }.as_bool(),
        "自有点击目标不可见",
    )?;
    input.click(p);
    Ok(())
}
async fn panel_button(
    panel: WindowHandle<LongCaptureView>,
    id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    panel.update(cx, |_, w, _| crate::gpui_smoke::isolate_input(hwnd(w)))??;
    presented(panel, cx).await?;
    let end = Instant::now() + WAIT;
    loop {
        if let Some((i, p)) = panel.update(cx, |v, w, _| {
            v.ui_positions.borrow().get(id).copied().map(|p| {
                (
                    Input {
                        handle: hwnd(w).0 as usize,
                        scale: w.scale_factor(),
                    },
                    p,
                )
            })
        })? {
            i.hover((
                (p.0 * i.scale).round() as u32,
                (p.1 * i.scale).round() as u32,
            ))?;
            presented(panel, cx).await?;
            return post_click(i, p);
        }
        ensure(Instant::now() < end, &format!("实际工具栏按钮{id}不存在"))?;
        tick(cx).await;
    }
}
async fn find_window<T: 'static + Render>(
    label: &str,
    cx: &mut AsyncApp,
) -> Result<WindowHandle<T>> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(w) = cx.update(|cx| cx.windows().into_iter().find_map(|w| w.downcast::<T>()))? {
            return Ok(w);
        }
        ensure(Instant::now() < end, &format!("没有实际{label}窗口"))?;
        tick(cx).await;
    }
}
async fn native_gone(handles: &[usize], cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    while handles
        .iter()
        .any(|&h| unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool())
    {
        ensure(Instant::now() < end, "关闭后自有原生HWND未销毁")?;
        tick(cx).await;
    }
    Ok(())
}
async fn source_gone(weaks: &[Weak<Image>], cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    while weaks.iter().any(|w| w.upgrade().is_some())
        || long_capture_runtime::active_workers() != 0
        || long_capture_pipeline::active_capture_threads() != 0
    {
        ensure(
            Instant::now() < end,
            "关闭后captured source Arc/真实采集线程仍存活",
        )?;
        tick(cx).await;
    }
    ensure(
        long_capture_runtime::panicked_workers() == 0,
        "真实采集线程panic",
    )
}
async fn pin_ready(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
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
        ensure(
            Instant::now() < end,
            "实际32k Pin处理/原生窗口/预览退休没有收敛",
        )?;
        tick(cx).await;
    }
}
fn pin_samples(pin: &PinView, spec: oracle::Spec, brightness: i16, flip: bool) -> Result<Value> {
    ensure(
        pin.original_size == (960, 32000) && pin.image_size == (960, 32000),
        "生产Pin移交尺寸错误",
    )?;
    let bytes = pin.image.as_bytes(0).ok_or("Pin当前BGRA缺失")?;
    let original = pin.original.as_bytes(0).ok_or("Pin原图BGRA缺失")?;
    ensure(
        bytes.len() == 960 * 32000 * 4 && original.len() == bytes.len(),
        "Pin BGRA实际长度不符",
    )?;
    let mut count = 0;
    for y in [0, 999, 1000, 4095, 4096, 7999, 15999, 23999, 31999] {
        for x in [3, 480, 956] {
            let i = (y as usize * 960 + x as usize) * 4;
            let expected = spec.pixel(x, y);
            ensure(
                original[i..i + 4]
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 1),
                "Pin原图独立world oracle抽样错误",
            )?;
            let mut expected = spec.pixel(if flip { 959 - x } else { x }, y);
            // Pin亮度百分比合同使用255比例，和独立期望相同舍入到25。
            let adjustment = i32::from(brightness) * 255 / 100;
            for channel in expected.iter_mut().take(3) {
                *channel = (i32::from(*channel) + adjustment).clamp(0, 255) as u8;
            }
            ensure(
                bytes[i..i + 4]
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 1),
                "Pin连续处理独立world抽样错误",
            )?;
            count += 1;
        }
    }
    Ok(
        json!({"sampled_pixels":count,"world_oracle":true,"borrowed_bgra":true,"full_image_clone":false,"brightness":brightness,"flip_x":flip}),
    )
}
async fn preview_presented(
    panel: WindowHandle<LongCaptureView>,
    region: ScreenRect,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let end = Instant::now() + WAIT;
    let preview = loop {
        if let Some(preview) = panel.update(cx, |v, _, _| v.preview_window)? {
            break preview;
        }
        ensure(Instant::now() < end, "实际旁侧预览未初始化")?;
        tick(cx).await;
    };
    loop {
        let m = panel.update(cx, |v, _, _| v.snapshot())?;
        let ready = preview.update(cx, |v, _, _| {
            v.ready
                && v.error.is_none()
                && v.displayed.as_ref().is_some_and(|p| {
                    p.generation == m.generation
                        && p.revision == m.preview_revision
                        && p.width == m.width
                        && p.height == m.height
                })
                && v.preview_bounds.borrow().is_some()
        })?;
        if ready {
            break;
        }
        ensure(
            Instant::now() < end,
            "真实preview revision/paint未与session收敛",
        )?;
        tick(cx).await;
    }
    presented(preview, cx).await?;
    let (handle,rect,detail,area)=preview.update(cx,|v,w,_|->Result<_>{
        let handle=hwnd(w).0 as usize;let rect=native_rect(handle)?;
        ensure(unsafe{IsWindowVisible(hwnd(w))}.as_bool()&&!close_rect(rect,region),"真实旁侧预览遮挡ROI或不可见")?;
        let p=v.displayed.as_ref().ok_or("preview无实际frame")?;
        let b=(*v.preview_bounds.borrow()).ok_or("preview未真正paint image")?;
        let dpi=w.scale_factor();
        let x=(f32::from(b.origin.x+b.size.width/2.)*dpi).round()as u32;
        let y=(f32::from(b.origin.y+b.size.height/2.)*dpi).round()as u32;
        Ok((handle,rect,json!({"generation":p.generation,"revision":p.revision,"result_width":p.width,"result_height":p.height,
            "preview_bgra_bytes":p.image.bgra.len(),"source_rect":format!("{:?}",p.source_rect),
            "painted_image_bounds":format!("{b:?}"),"displayed":true}),[x.saturating_sub(1),y.saturating_sub(1),3,3]))
    })??;
    let pixels = frame_sample(preview, Some(area), cx).await?;
    let panel_rect = panel.update(cx, |_, w, _| native_rect(hwnd(w).0 as usize))??;
    ensure(!close_rect(panel_rect, region), "实际采集toolbar遮挡ROI")?;
    Ok(
        json!({"hwnd":handle,"native_rect":rect_json(rect),"toolbar_native_rect":rect_json(panel_rect),"outside_roi":true,"frame":detail,"gpu_sample":pixels}),
    )
}
async fn checkpoint(
    bridge: &SharedBridge,
    panel: WindowHandle<LongCaptureView>,
    cycle: u32,
    step: u32,
    sources: &mut SourceTrace,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    // 可信条带仅在真实Pause ACK后公开，遵守生产快照门，不在Running偷读。
    quiet(panel, cx).await?;
    let response = request(bridge, "checkpoint", json!({}), cx).await?;
    let y = physical_scroll(&response["state"], cycle)?;
    let root = report.root.clone();
    let snapshot = panel.update(cx, |v, _, _| {
        v.stitch_snapshot().ok_or("checkpoint缺可信snapshot")
    })??;
    sources.observe(&snapshot);
    let evidence = cx
        .background_executor()
        .spawn(async move {
            (||->Result<Value>{
        let renderer=read_renderer(&response,&root)?;
        let renderer_check=oracle::verify_image(&renderer,y)?;
        let view_check=oracle::compare_viewport(&snapshot,&renderer)?;
        let world_check=oracle::verify_snapshot(&snapshot,0)?;
        let allocations=oracle::source_allocations(&snapshot)?;
        Ok(json!({"cycle":cycle,"wheel_step":step,"physical_scroll_top":y,"renderer":renderer_check,
            "viewport":view_check,"world":world_check,"source_allocations":allocations,
            "whole_reference_bgra_bytes":0}))
    })().map_err(|e|e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?;
    report.stage("checkpoint", evidence)?;
    if step != 126 {
        resume(panel, cx).await?;
    }
    Ok(())
}
async fn cycle(
    bridge: &SharedBridge,
    cycle: u32,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let owner = report.owner.clone();
    let root = report.root.clone();
    // 唯一 full-client 读取只在原生归属+新lease确认且本轮GPUI界面尚未打开时发生。
    let calibration_permit = owner.calibration_permit()?;
    let response = request(bridge, "calibrate", json!({}), cx).await?;
    ensure(
        physical_scroll(&response["state"], cycle)? == 0,
        "新cycle未回到world0",
    )?;
    let gate = calibration_permit.gate().clone();
    let client = owner.client_region();
    let visible_content = owner.content_region();
    let calibration = cx
        .background_executor()
        .spawn(async move {
            (|| -> Result<_> {
                let renderer = read_renderer(&response, &root)?;
                let spec = oracle::Spec::new(1.25)?;
                let renderer_check = oracle::verify_image(&renderer, 0)?;
                let pixels = read_owned(client, gate)?;
                let offset = calibrate(&pixels, &renderer)?;
                let region = ScreenRect {
                    left: i32::try_from(i64::from(client.left) + i64::from(offset.0))?,
                    top: i32::try_from(i64::from(client.top) + i64::from(offset.1))?,
                    width: spec.width,
                    height: spec.viewport_height,
                };
                Ok((
                    region,
                    json!({"unique_matches":1,"offset":offset,"renderer":renderer_check,
            "owned_client":rect_json(client),"owned_visible_content":rect_json(visible_content),
            "calibration_temporary_bgra_bytes":pixels.bgra.len()+renderer.bgra.len(),
            "calibration_temporaries_released":true}),
                ))
            })()
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?;
    let (region, calibration) = calibration;
    let factory = owner.bind_content(region, &calibration_permit)?;
    report.stage(
        "cycle-baseline",
        json!({"cycle":cycle,"region":rect_json(region),"calibration":calibration,
            "dom_scroller_mapping_matches_calibrated_roi":true,
            "content_mapping":owner.content_mapping_probe()}),
    )?;
    let options = LongOptions {
        stitch: StitchOptions {
            axis: ScrollAxis::Vertical,
            detect_fixed_borders: false,
            fixed_start: Some(0),
            fixed_end: Some(0),
        },
        ..Default::default()
    };
    let panel =
        cx.update(|cx| LongCaptureView::open_region_guarded(region, options, factory, None, cx))??;
    let end = Instant::now() + WAIT;
    let retired_border = loop {
        if let Some(handles) = panel.update(cx, |v, _, _| {
            v.border.take().map(|border| {
                let mut handles: Vec<_> = border.handles().iter().map(|h| h.0 as usize).collect();
                handles.extend(border.resize_handles().iter().map(|(_, h, _)| h.0 as usize));
                drop(border);
                handles
            })
        })? {
            break handles;
        }
        ensure(
            panel.update(cx, |v, _, _| v.session.is_none())?,
            "stress未在采集前移除内部边框",
        )?;
        ensure(Instant::now() < end, "production border setup未出现")?;
        tick(cx).await;
    };
    native_gone(&retired_border, cx).await?;
    panel_wait(
        panel,
        "真实浏览器首帧",
        |m, _| committed(m) && m.phase == LongPhase::Running && m.height == 640,
        cx,
    )
    .await?;
    let panel_hwnd = panel.update(cx, |_, w, _| hwnd(w).0 as usize)?;
    let preview = preview_presented(panel, region, cx).await?;
    let preview_hwnd = preview["hwnd"]
        .as_u64()
        .ok_or("preview native handle缺失")? as usize;
    let mut sources = SourceTrace::default();
    quiet(panel, cx).await?;
    let initial = panel.update(cx, |v, _, _| {
        v.stitch_snapshot().ok_or("初帧Pause后缺可信snapshot")
    })??;
    sources.observe(&initial);
    let initial_world = cx
        .background_executor()
        .spawn(async move { oracle::verify_snapshot(&initial, 0).map_err(|e| e.to_string()) })
        .await
        .map_err(|e| e.to_string())?;
    resume(panel, cx).await?;
    report.stage(
        "capture-initial",
        json!({"cycle":cycle,"preview":preview,"production_border_removed_for_strict_gate":true,
        "production_border_hwnds_destroyed":retired_border,"actual_toolbar":true,"initial_world":initial_world}),
    )?;
    let mut wheels = vec![];
    for step in 1..=126 {
        let before = Instant::now();
        let delta = if step == 126 { 88 } else { 200 };
        let response = request(bridge, "wheel", json!({"delta_y":delta}), cx).await?;
        let y = physical_scroll(&response["state"], cycle)?;
        let expected_y = if step == 126 { 31360 } else { step * 250 };
        ensure(
            y == expected_y
                && response["wheel"]["settled_by_actual_scroll_and_paint_ready_and_two_raf"]
                    == true
                && response["wheel"]["actual_delta_css"].as_f64() == Some(f64::from(delta)),
            "实际浏览器wheel/paint/scrollTop未按合同完成",
        )?;
        let wait = panel_wait(
            panel,
            "真实增长ACK",
            |m, _| {
                committed(m)
                    && m.phase == LongPhase::Running
                    && m.height == 640 + y
                    && m.viewport.y == y
            },
            cx,
        )
        .await;
        let wait = match wait {
            Ok(wait) => wait,
            Err(error) => {
                let state = panel.update(cx, |v, _, _| {
                    json!({"snapshot":format!("{:?}",v.snapshot()),"displayed":format!("{:?}",v.displayed_snapshot()),
                        "ui_status":v.status,"pipeline":v.session.as_ref().map(|s|s.pipeline_probe())})
                }).unwrap_or_else(|error| json!({"window_unavailable":error.to_string()}));
                report.stage(
                    "capture-error",
                    json!({"cycle":cycle,"step":step,"error":error.to_string(),"state":state}),
                )?;
                report.save(false, Some(&error.to_string()))?;
                return Err(error);
            }
        };
        report.wheel_probe(cycle, step, &wait)?;
        let mut ack = wait;
        let pipeline = ack["pipeline"]
            .as_object_mut()
            .ok_or("成功增长ACK缺少pipeline探针")?;
        pipeline.remove("reads");
        pipeline.remove("auto_events");
        let mut wheel = json!({"step":step,"dom_physical_y":y,"delta_css":delta,"elapsed_ms":before.elapsed().as_millis()});
        wheel.as_object_mut().unwrap().insert("ack".into(), ack);
        wheels.push(wheel);
        if [32, 64, 96, 126].contains(&step) {
            checkpoint(bridge, panel, cycle, step, &mut sources, report, cx).await?;
        }
    }
    let quiet = quiet(panel, cx).await?;
    report.stage(
        "capture-drained-32000",
        json!({"cycle":cycle,"drain":quiet,"preview":preview_presented(panel,region,cx).await?}),
    )?;
    // 生产贴图按钮直接物化输出，原采集线程和strip必须正常退休。
    panel_button(panel, "long-pin", cx).await?;
    let pin = find_window::<PinView>("Pin", cx).await?;
    pin_ready(pin, cx).await?;
    native_gone(&[panel_hwnd, preview_hwnd], cx).await?;
    source_gone(&sources.weaks, cx).await?;
    let (weak, samples) = pin.update(cx, |v, _, _| -> Result<_> {
        Ok((
            Arc::downgrade(&v.original),
            pin_samples(v, oracle::Spec::new(1.25)?, 0, false)?,
        ))
    })??;
    report.stage("pin-open",json!({"cycle":cycle,"actual_image":samples,"source_weak_count":sources.weaks.len(),"all_captured_source_weaks_expired":true,
        "capture_threads":0,"consumer_sessions":0,"panel_preview_hwnds_destroyed":true,
        "pin_lifecycle":pin.update(cx,|v,_,_|v.preview_texture_lifecycle())?,"gpu_sample":frame_sample(pin,None,cx).await?}))?;
    let mut retired = 0;
    for (name, action, brightness, flip) in [
        ("brightness", PinAction::Brightness(10), 10, false),
        ("flip", PinAction::FlipX, 10, true),
        ("undo", PinAction::Undo, 10, false),
        ("redo", PinAction::Redo, 10, true),
        ("reset", PinAction::ResetImage, 0, false),
    ] {
        pin.update(cx, |v, w, cx| v.perform(action, w, cx))?;
        pin_ready(pin, cx).await?;
        let (samples, lifecycle) = pin.update(cx, |v, _, _| -> Result<_> {
            Ok((
                pin_samples(v, oracle::Spec::new(1.25)?, brightness, flip)?,
                v.preview_texture_lifecycle(),
            ))
        })??;
        ensure(
            lifecycle.1 == 0 && lifecycle.3 >= retired,
            "Pin退休队列未收敛或成功drop计数回退",
        )?;
        retired = lifecycle.3;
        report.stage(
            "pin-processing",
            json!({"cycle":cycle,"action":name,"samples":samples,"lifecycle":lifecycle}),
        )?;
    }
    ensure(retired > 0, "真实32k Pin连续处理未调用旧preview drop_image")?;
    let pin_handles = cx.update(|cx| -> Result<Vec<usize>> {
        let mut handles = vec![];
        for w in cx
            .windows()
            .into_iter()
            .filter(|w| w.downcast::<Host>().is_none())
        {
            handles.push(cx.update_window(w, |_, w, _| hwnd(w).0 as usize)?);
        }
        Ok(handles)
    })??;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Close, w, cx))?;
    native_gone(&pin_handles, cx).await?;
    let end = Instant::now() + WAIT;
    while weak.upgrade().is_some() {
        ensure(
            Instant::now() < end,
            "Pin关闭后原RenderImage/CPU处理checkpoint未释放",
        )?;
        tick(cx).await;
    }
    source_gone(&sources.weaks, cx).await?;
    ensure(
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<Host>().is_some())
        })?,
        "cycle关闭后实际GPUI窗口残留",
    )?;
    // 每轮相同静置时间，先确认源/线程/HWND退休，再等待GPU异步删除与分配器收敛。
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let private = cpu_memory()?["private_bytes"]
        .as_u64()
        .ok_or("runtime private bytes缺失")?;
    report.close_private.push(private);
    report.stage("cycle-close",json!({"cycle":cycle,"private_bytes":private,"source_weak_count":sources.weaks.len(),"all_source_weaks_expired":true,
        "pin_original_weak_expired":true,"native_hwnds_destroyed":true,"capture_threads":0,"consumer_sessions":0,"normal_join_observed":true}))?;
    let revoke = request(bridge, "revoke_lease", json!({}), cx).await?;
    ensure(
        revoke["lease_revoked"] == true,
        "cycle正常关闭后Node未撤销lease",
    )?;
    let mut case = json!({"mode":format!("cycle-{cycle}-real-browser-32000-edit-return-pin-close"),"ok":true,
        "wheel_count":126,"physical_height":32000,"row_oracle":true,"whole_reference_bgra_bytes":0,
        "actual_editor_return":true,"actual_production_pin_handoff":true,"private_bytes_after_close":private,
        "source_arcs_released":true,"pin_original_released":true,"native_hwnds_destroyed":true,"capture_threads":0,"consumer_sessions":0});
    case.as_object_mut()
        .unwrap()
        .insert("wheels".into(), Value::Array(wheels));
    report.cases.push(case);
    report.save(false, None)?;
    Ok(())
}
async fn verify(bridge: &SharedBridge, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let owner = report.owner.clone();
    let dpi_check = cx
        .background_executor()
        .spawn(async move {
            (|| -> Result<Value> {
                let initial = unsafe { GetThreadDpiAwarenessContext() };
                ensure(!initial.0.is_null(), "后台线程初始DPI上下文无效")?;
                let previous = unsafe {
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE)
                };
                if previous.0.is_null() {
                    return Err(format!(
                        "专项进入UNAWARE上下文失败：{}",
                        windows::core::Error::from_win32()
                    )
                    .into());
                }
                let restore = RestoreThreadDpi(previous);
                // 这个作用域没有 await；gate 仍按真实Job/窗口/全ROI遮挡及租约验证。
                let checked = (|| -> Result<Value> {
                    let entered = unsafe { GetThreadDpiAwarenessContext() };
                    ensure(
                        unsafe {
                            AreDpiAwarenessContextsEqual(
                                entered,
                                DPI_AWARENESS_CONTEXT_UNAWARE,
                            )
                        }
                        .as_bool(),
                        "后台线程未实际进入UNAWARE上下文",
                    )?;
                    let native = owner.native_snapshot();
                    let window = HWND(native.hwnd as *mut _);
                    let mut pid = 0;
                    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
                    ensure(
                        pid == native.window_pid && thread == native.window_thread,
                        "DPI专项原自有浏览器窗口身份改变",
                    )?;
                    let mut client = RECT::default();
                    let mut origin = POINT::default();
                    let mut bounds = RECT::default();
                    unsafe {
                        GetClientRect(window, &mut client)?;
                        ClientToScreen(window, &mut origin).ok()?;
                        GetWindowRect(window, &mut bounds)?;
                    }
                    let observed_client = ScreenRect {
                        left: origin.x,
                        top: origin.y,
                        width: u32::try_from(i64::from(client.right) - i64::from(client.left))?,
                        height: u32::try_from(i64::from(client.bottom) - i64::from(client.top))?,
                    };
                    let observed_window = ScreenRect {
                        left: bounds.left,
                        top: bounds.top,
                        width: u32::try_from(i64::from(bounds.right) - i64::from(bounds.left))?,
                        height: u32::try_from(i64::from(bounds.bottom) - i64::from(bounds.top))?,
                    };
                    let gate_result = owner.client_gate().and_then(|gate| gate());
                    let after_gate = unsafe { GetThreadDpiAwarenessContext() };
                    ensure(
                        unsafe {
                            AreDpiAwarenessContextsEqual(
                                after_gate,
                                DPI_AWARENESS_CONTEXT_UNAWARE,
                            )
                        }
                        .as_bool(),
                        "严格browser client gate未恢复调用者UNAWARE上下文",
                    )?;
                    gate_result?;
                    Ok(json!({
                        "caller_context":"DPI_AWARENESS_CONTEXT_UNAWARE",
                        "context_comparison_api":"AreDpiAwarenessContextsEqual",
                        "initial_context":initial.0 as isize,
                        "requested_unaware_context":DPI_AWARENESS_CONTEXT_UNAWARE.0 as isize,
                        "entered_context":entered.0 as isize,
                        "after_gate_context":after_gate.0 as isize,
                        "strict_owned_browser_client_gate_passed":true,
                        "caller_unaware_context_restored":true,
                        "owned_browser_hwnd":owner.config().hwnd,
                        "unaware_observed_client_rect":rect_json(observed_client),
                        "unaware_observed_window_rect":rect_json(observed_window),
                        "unaware_geometry_differs_from_physical":observed_client != native.client_rect.screen()
                            || observed_window != native.window_rect.screen(),
                        "expected_native_snapshot":native,
                        "pixels_read":false,
                        "foreign_window_exception":false,
                    }))
                })();
                drop(restore);
                let after_restore = unsafe { GetThreadDpiAwarenessContext() };
                ensure(
                    unsafe {
                        AreDpiAwarenessContextsEqual(after_restore, initial)
                    }
                    .as_bool(),
                    "DPI专项结束后未恢复后台线程初始上下文",
                )?;
                let mut detail = checked?;
                detail["after_restore_context"] = json!(after_restore.0 as isize);
                detail["background_initial_context_restored"] = json!(true);
                Ok(detail)
            })()
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;
    report.stage("dpi-context-restored", dpi_check)?;
    report.stage(
        "gpui-hidden-host-baseline",
        json!({"hidden_host":true,"startup_config":report.startup_config}),
    )?;
    for round in 1..=3 {
        if round > 1 {
            // 上轮严格源/线程/真实HWND消失之后才执行浏览器fixture准备。
            let reset = request(bridge, "reset", json!({"cycle":round}), cx).await?;
            ensure(
                reset["reset_is_fixture_preparation"] == true
                    && physical_scroll(&reset["state"], round)? == 0,
                "轮间reset未settle",
            )?;
            let new_lease = request(bridge, "new_lease", json!({}), cx).await?;
            ensure(
                physical_scroll(&new_lease["state"], round)? == 0,
                "显式新lease状态非法",
            )?;
            report.owner.client_gate()?()?;
        }
        cycle(bridge, round, report, cx).await?;
    }
    ensure(
        report.close_private.len() == 3,
        "未实际完成三轮runtime关闭采样",
    )?;
    let growth = report.close_private[2].saturating_sub(report.close_private[0]);
    report.stage("three-cycle-close-stability",json!({"close_private_bytes":report.close_private,"growth_bytes":growth,"limit_bytes":CLOSE_STABILITY_BYTES,"pre_registered":true}))?;
    ensure(
        growth <= CLOSE_STABILITY_BYTES,
        "三轮真实浏览器32k关闭后runtime private增长超过预设16MiB",
    )?;
    Ok(())
}
async fn finish_bridge(
    bridge: &SharedBridge,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    request(bridge, "close_browser", json!({}), cx).await?;
    native_gone_browser(report.owner.config().hwnd, cx).await?;
    let b = bridge.clone();
    cx.background_executor()
        .spawn(async move {
            b.lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish()
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?;
    report.protocol = bridge.lock().unwrap_or_else(|e| e.into_inner()).probe();
    ensure(
        report.protocol["reader_joined"] == true && report.protocol["disconnected"] == true,
        "browser protocol没有正常EOF/join",
    )
}
async fn native_gone_browser(handle: usize, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    while unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool() {
        ensure(Instant::now() < end, "ownedbrowser HWND未正常关闭")?;
        tick(cx).await;
    }
    Ok(())
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let bridge = Bridge::new()?;
    let (owner, config) = bridge.initial()?;
    let report = Report::new(config, owner)?;
    let app_data =
        PathBuf::from(std::env::var_os("RUSTSHOT_DATA_DIR").ok_or("缺少隔离RUSTSHOT_DATA_DIR")?);
    std::fs::create_dir_all(&app_data)?;
    let app_data = app_data.canonicalize()?;
    ensure(
        app_data.starts_with(&report.root) && app_data != report.root,
        "继承偏好目录不在本次隔离QA子目录",
    )?;
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    ensure(
        crate::preferences::data_dir().canonicalize()? == app_data,
        "生产偏好未使用本次隔离目录",
    )?;
    let saved = crate::preferences::Preferences::load();
    ensure(
        !saved.history && !saved.pin_restore,
        "实际隔离偏好未禁用历史或Pin恢复",
    )?;
    let bridge = Arc::new(Mutex::new(bridge));
    let report = Rc::new(RefCell::new(Some(report)));
    let final_report = report.clone();
    let failure = Rc::new(RefCell::new(Some("真实browser32k三轮未完成".to_owned())));
    let final_failure = failure.clone();
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
                *failure.borrow_mut() = Some(e.to_string());
                cx.quit();
                return;
            }
            cx.spawn(async move |cx| {
                let mut record = report.borrow_mut().take().expect("stress独占report");
                let mut error = verify(&bridge, &mut record, cx)
                    .await
                    .err()
                    .map(|e| e.to_string());
                if error.is_some() {
                    *failure.borrow_mut() = error.clone();
                    let _ = record.save(false, error.as_deref());
                    // 只移除当前QA创建的本进程GPUI窗口；不向任何用户窗口发输入/关闭。
                    // 隐藏Host保留至协议EOF/join和报告归还，防止最后窗口关闭提前结束消息循环。
                    let _ = cx.update(|cx| {
                        for w in cx.windows() {
                            if w.downcast::<Host>().is_none() {
                                let _ = cx.update_window(w, |_, w, _| w.remove_window());
                            }
                        }
                    });
                    record.owner.revoke("stress failure cleanup");
                }
                let cleanup = finish_bridge(&bridge, &mut record, cx).await;
                if let Err(e) = cleanup {
                    let message = format!("browser正常清理失败：{e}");
                    error = Some(error.map_or(message.clone(), |e| format!("{e}; {message}")));
                }
                *failure.borrow_mut() = error;
                *report.borrow_mut() = Some(record);
                let _ = cx.update(|cx| {
                    for w in cx.windows() {
                        let _ = cx.update_window(w, |_, w, _| w.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    let error = final_failure.borrow_mut().take();
    if let Some(mut r) = final_report.borrow_mut().take() {
        r.stage(
            "after-application",
            json!({"normal_application_return":true}),
        )?;
        r.save(true, error.as_deref())?;
    }
    if let Some(e) = error {
        return Err(e.into());
    }
    Ok(())
}
