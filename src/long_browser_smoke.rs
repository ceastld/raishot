//! 隔离 Chrome 的 owned-client 校准与真实 DOM/DXGI；不使用合成 Win32 内容源。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    live_capture::LiveCapture,
    long_browser_oracle as oracle,
    long_browser_owner::{
        BrowserCalibrationPermit, BrowserCaptureOwner, BrowserLease, BrowserOwnerConfig,
    },
    long_capture_pipeline,
    long_capture_runtime::{self, FrameGate, LongOptions, LongPhase, LongSession},
    stitch::{ScrollAxis, StitchOptions, StitchSnapshot},
    Result,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
            WindowsAndMessaging::*,
        },
    },
};
const WAIT: Duration = Duration::from_secs(20);
const TOLERANCE: u8 = 1;

#[path = "long_browser_auto_smoke.rs"]
mod native_auto;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
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
struct Report {
    root: PathBuf,
    stages: Vec<Value>,
    cases: Vec<Value>,
    owner: Arc<BrowserCaptureOwner>,
    protocol_joined: bool,
    protocol: Value,
    native_auto: bool,
}
impl Report {
    fn new(config: &Value, owner: Arc<BrowserCaptureOwner>) -> Result<Self> {
        let mode = config["mode"].as_str().unwrap_or("manual");
        let requested =
            std::env::var("RUSTSHOT_BROWSER_QA_MODE").unwrap_or_else(|_| "manual".into());
        ensure(
            matches!(mode, "manual" | "native-auto") && mode == requested,
            "浏览器配置和独立 QA 模式不一致",
        )?;
        let root = PathBuf::from(
            config["artifact_root"]
                .as_str()
                .ok_or("artifact_root缺失")?,
        )
        .canonicalize()?;
        let base = std::env::current_dir()?.join("target/qa").canonicalize()?;
        ensure(root.starts_with(base), "browser记录目录不在本树QA范围")?;
        Ok(Self {
            root,
            stages: vec![],
            cases: vec![],
            owner,
            protocol_joined: false,
            protocol: Value::Null,
            native_auto: mode == "native-auto",
        })
    }
    fn save(&self, complete: bool, error: Option<String>) -> Result<()> {
        let group = if self.native_auto {
            "browser-native-auto"
        } else {
            "browser-roi-renderer-calibration"
        };
        let mode_limitations = if self.native_auto {
            vec!["真实后端 LongSession 原生定向 Auto；未操作生产 GPUI 工具栏或32000px长图", "Node 只读观察 DOM，输入守卫和实际调用计数由独立 Node 报告证明；不代表硬件滚轮或所有网站"]
        } else {
            vec![
                "真实后端LongSession，未操作生产工具栏、native Auto或32000px长图",
                "DOM wheel由own Page CDP路由；不证明硬件输入或真实网站",
            ]
        };
        let groups = if complete && error.is_none() {
            vec![json!({"name":group,"ok":true,"cases":self.cases})]
        } else {
            vec![]
        };
        let value = json!({"suite":"long-browser","pid":std::process::id(),"ok":complete&&error.is_none(),"error":error,
            "mode":if self.native_auto {"native-auto"} else {"manual"},
            "requested_groups":[group],"groups":groups,"stages":self.stages,
            "native_owner":self.owner.native_snapshot(),"ownership":self.owner.probe(),
            "real_browser_compositor":true,"real_dxgi_capture":complete,"full_desktop_freeze_used":false,
            "clipboard_untouched":true,"global_input_sent":false,"fixture_substituted":false,
            "capture_threads_after_close":long_capture_pipeline::active_capture_threads(),
            "consumer_sessions_after_close":long_capture_runtime::active_workers(),"protocol_thread_joined":self.protocol_joined,
            "protocol":self.protocol,"renderer_sha256_computed_by_node":true,"renderer_sha256_recomputed_by_rust":false,
            "limitations":["真实隔离Chrome app、自有320×240CSS scrollport、当前单显示器DPR","校准仅CPU读回受归属门保护的完整owned client，可能含自有Chrome UI；内容只采精确DOM ROI",
                "Win32归属检查到GPU Copy仍有普通TOCTOU","文字位于ROI外，本轮不测字形；纹理预设每通道误差≤1",
                mode_limitations[0],mode_limitations[1],"有限单组不证明混合DPI/HDR/复杂动画/长期内存"]});
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&value)?,
        )?;
        Ok(())
    }
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        self.stages.push(json!({"stage":name,"unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"detail":detail,"ownership":self.owner.probe()}));
        self.save(false, None)
    }
    fn pass(&mut self, mode: &str, detail: Value) -> Result<()> {
        self.cases
            .push(json!({"mode":mode,"ok":true,"detail":detail}));
        self.stage("subcase-passed", self.cases.last().cloned().unwrap())
    }
}
fn read_renderer(response: &Value, root: &Path) -> Result<Image> {
    let descriptor = &response["renderer"];
    let path =
        PathBuf::from(descriptor["path"].as_str().ok_or("renderer PNG路径缺失")?).canonicalize()?;
    ensure(
        path.starts_with(root)
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("png")),
        "renderer文件不属于本次PNG记录",
    )?;
    ensure(
        descriptor["sha256"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
        "renderer SHA256描述无效",
    )?;
    ensure(
        std::fs::metadata(&path)?.len() <= 32 * 1024 * 1024,
        "renderer PNG过大",
    )?;
    let image = crate::export::read_image(&path)?;
    ensure(
        descriptor["pixel_width"].as_u64() == Some(u64::from(image.width))
            && descriptor["pixel_height"].as_u64() == Some(u64::from(image.height)),
        "renderer实际PNG尺寸与描述不符",
    )?;
    Ok(image)
}
fn physical_scroll(state: &Value, dpr: f64) -> Result<u32> {
    ensure(
        state["pattern_id"] == "bgra-world-v1" && state["dpr"].as_f64() == Some(dpr),
        "DOM pattern/DPR改变",
    )?;
    let value = state["scroll_top"].as_f64().ok_or("实测scrollTop缺失")? * dpr;
    ensure(
        value >= 0. && value.is_finite() && (value - value.round()).abs() < 0.000_001,
        "首组scrollTop×DPR不是整数物理像素",
    )?;
    u32::try_from(value.round() as u64).map_err(Into::into)
}
fn read_owned(region: ScreenRect, gate: &FrameGate) -> Result<Image> {
    gate()?;
    let cancelled = || gate().is_err();
    let capture = LiveCapture::new_for_region_cancellable(region, &cancelled)?;
    let result = capture.read_cancellable(region, &cancelled)?;
    gate()?;
    Ok(result)
}
fn wait_session(
    session: &LongSession,
    label: &str,
    mut ready: impl FnMut(&long_capture_runtime::LongSnapshot) -> bool,
) -> Result<()> {
    let until = Instant::now() + WAIT;
    loop {
        let meta = session.snapshot();
        if ready(&meta) {
            return Ok(());
        }
        ensure(
            !meta.worker_exited && Instant::now() < until,
            &format!(
                "{label}未到达：{meta:?}；pipeline={:?}",
                session.pipeline_probe()
            ),
        )?;
        thread::sleep(Duration::from_millis(5));
    }
}
struct Session {
    inner: Option<LongSession>,
}
impl Session {
    fn get(&self) -> &LongSession {
        self.inner.as_ref().expect("session仍存活")
    }
    fn get_mut(&mut self) -> &mut LongSession {
        self.inner.as_mut().expect("session仍存活")
    }
    fn quiet(&self) -> bool {
        let m = self.get().snapshot();
        let p = self.get().pipeline_probe();
        m.generation == m.requested_generation
            && !m.busy
            && p.producer_quiesced
            && !p.capture_in_flight
            && !p.consumer_in_flight
            && p.queue_depth == 0
            && p.reserved_raw_bytes == 0
            && p.drain_fence.is_none()
    }
    fn pause(&mut self) -> Result<()> {
        self.get_mut().set_paused(true)?;
        wait_session(self.get(), "真实Pause ACK", |m| {
            m.phase == LongPhase::Paused && m.generation == m.requested_generation
        })?;
        let until = Instant::now() + WAIT;
        while !self.quiet() {
            ensure(
                Instant::now() < until,
                "Pause ACK尚未释放producer/raw/consumer",
            )?;
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
    fn close(&mut self) -> Result<Value> {
        let Some(session) = self.inner.as_mut() else {
            return Ok(json!({"already_closed":true}));
        };
        session.stop();
        let until = Instant::now() + WAIT;
        while !session.try_join_stopped() {
            ensure(Instant::now() < until, "真实浏览器采集两端未正常退出")?;
            thread::sleep(Duration::from_millis(5));
        }
        let meta = session.snapshot();
        let p = session.pipeline_probe();
        ensure(
            meta.worker_exited
                && !meta.worker_alive
                && !p.producer_alive
                && !p.consumer_alive
                && p.reserved_raw_bytes == 0
                && p.queue_depth == 0,
            "session退出/许可归还不完整",
        )?;
        let record = json!({"worker_exited":meta.worker_exited,"producer_alive":p.producer_alive,"consumer_alive":p.consumer_alive,"raw_bytes":p.reserved_raw_bytes,"queue_depth":p.queue_depth,"joined":true});
        self.inner.take();
        Ok(record)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn start(
    owner: &Arc<BrowserCaptureOwner>,
    region: ScreenRect,
    permit: &BrowserCalibrationPermit,
) -> Result<Session> {
    let factory = owner.bind_content(region, permit)?;
    let options = LongOptions {
        stitch: StitchOptions {
            axis: ScrollAxis::Vertical,
            detect_fixed_borders: false,
            fixed_start: Some(0),
            fixed_end: Some(0),
        },
        ..Default::default()
    };
    Ok(Session {
        inner: Some(LongSession::start_region_guarded(
            region, options, factory, None,
        )?),
    })
}
fn pixels(
    snapshot: &StitchSnapshot,
    spec: oracle::Spec,
    first_row: u32,
) -> Result<(Image, oracle::PixelCheck)> {
    let image = crate::long_document::render_region(
        snapshot,
        Rect::full(snapshot.width, snapshot.height),
        snapshot.width,
        snapshot.height,
    )?;
    let check = oracle::world(&image, spec, first_row, TOLERANCE)?;
    Ok((image, check))
}
fn same_sources(a: &StitchSnapshot, b: &StitchSnapshot) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.viewport == b.viewport
        && a.axis == b.axis
        && a.segments.len() == b.segments.len()
        && a.segments.iter().zip(&b.segments).all(|(a, b)| {
            a.origin == b.origin && a.source == b.source && Arc::ptr_eq(&a.image, &b.image)
        })
}
fn image_weaks(snapshot: &StitchSnapshot) -> Vec<Weak<Image>> {
    snapshot
        .segments
        .iter()
        .map(|s| Arc::downgrade(&s.image))
        .collect()
}

unsafe extern "system" fn occluder_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        let _ = DestroyWindow(h);
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(h, msg, w, l)
}
struct Occluder {
    hwnd: usize,
    thread: Option<JoinHandle<std::result::Result<(), String>>>,
}
impl Occluder {
    fn open(region: ScreenRect) -> Result<Self> {
        let (send, receive) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("rustshot-browser-owned-occluder".into())
            .spawn(move || -> std::result::Result<(), String> {
                (|| -> Result<()> {
                    unsafe {
                        let instance = GetModuleHandleW(None)?;
                        let class: Vec<u16> =
                            format!("RustShot.QA.BrowserOccluder.{}", std::process::id())
                                .encode_utf16()
                                .chain(Some(0))
                                .collect();
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(occluder_proc),
                            hInstance: instance.into(),
                            lpszClassName: PCWSTR(class.as_ptr()),
                            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                            ..Default::default()
                        };
                        ensure(RegisterClassW(&wc) != 0, "自有遮挡窗注册失败")?;
                        let h = CreateWindowExW(
                            WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                            PCWSTR(class.as_ptr()),
                            w!("RustShot · browser owned occluder"),
                            WS_POPUP | WS_VISIBLE,
                            region.left + (region.width / 2) as i32,
                            region.top + (region.height / 2) as i32,
                            32,
                            32,
                            None,
                            None,
                            Some(instance.into()),
                            None,
                        )?;
                        send.send(h.0 as usize)?;
                        let mut message = MSG::default();
                        loop {
                            let status = GetMessageW(&mut message, None, 0, 0);
                            ensure(status.0 >= 0, "自有遮挡窗口消息泵失败")?;
                            if status.0 == 0 {
                                break;
                            }
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                        ensure(!IsWindow(Some(h)).as_bool(), "自有遮挡窗口仍存活")?;
                        Ok(())
                    }
                })()
                .map_err(|error| error.to_string())
            })?;
        let hwnd = receive.recv_timeout(WAIT)?;
        Ok(Self {
            hwnd,
            thread: Some(thread),
        })
    }
    fn close(&mut self) -> Result<()> {
        if self.thread.is_none() {
            return Ok(());
        }
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )?;
        }
        let until = Instant::now() + WAIT;
        while self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            ensure(Instant::now() < until, "自有遮挡窗口线程未在期限内退出")?;
            thread::sleep(Duration::from_millis(5));
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "自有遮挡线程panic")??;
        }
        ensure(
            !unsafe { IsWindow(Some(HWND(self.hwnd as *mut _))) }.as_bool(),
            "自有遮挡窗口没有销毁",
        )
    }
}
impl Drop for Occluder {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// 显式new_lease之后重新读取真实renderer与DXGI，不给旧校准结果换一个新stamp。
fn recalibrate(
    bridge: &mut Bridge,
    report: &mut Report,
    owner: &Arc<BrowserCaptureOwner>,
    spec: oracle::Spec,
    offset: u32,
    expected: ScreenRect,
) -> Result<BrowserCalibrationPermit> {
    let permit = owner.calibration_permit()?;
    let response = bridge.request("calibrate", json!({}))?;
    ensure(
        physical_scroll(&response["state"], spec.dpr)? == offset,
        "新lease真实校准时DOM位置变化",
    )?;
    let renderer = read_renderer(&response, &report.root)?;
    let world = oracle::world(&renderer, spec, offset, TOLERANCE)?;
    let client_region = owner.client_region();
    let client = read_owned(client_region, permit.gate())?;
    let calibrated = oracle::calibrate(&client, &renderer, spec, TOLERANCE)?;
    let actual = ScreenRect {
        left: i32::try_from(i64::from(client_region.left) + i64::from(calibrated.offset_left))?,
        top: i32::try_from(i64::from(client_region.top) + i64::from(calibrated.offset_top))?,
        width: spec.width,
        height: spec.viewport_height,
    };
    ensure(
        actual == expected,
        "新lease完整真实像素校准的ROI与原ROI不同",
    )?;
    permit.gate()()?;
    report.stage(
        "fresh-lease-owned-client-calibration",
        json!({"world":world,"calibration":calibrated,"content_roi":rect_json(actual),"same_read_and_bind_permit":true}),
    )?;
    Ok(permit)
}

fn verify(
    bridge: &mut Bridge,
    report: &mut Report,
    owner: &Arc<BrowserCaptureOwner>,
) -> Result<()> {
    let permit = owner.calibration_permit()?;
    let response = bridge.request("calibrate", json!({}))?;
    let dpr = response["state"]["dpr"].as_f64().ok_or("DOM DPR缺失")?;
    let spec = oracle::Spec::new(dpr)?;
    ensure(
        physical_scroll(&response["state"], dpr)? == 0,
        "首帧DOM不在world起点",
    )?;
    let renderer = read_renderer(&response, &report.root)?;
    let renderer_check = oracle::world(&renderer, spec, 0, TOLERANCE)?;
    let client_region = owner.client_region();
    report.stage("before-owned-client-dxgi",json!({"client":rect_json(client_region),"renderer":response["renderer"],"state":response["state"]}))?;
    let client = read_owned(client_region, permit.gate())?;
    let calibrated = oracle::calibrate(&client, &renderer, spec, TOLERANCE)?;
    let css_left = response["state"]["scrollport"]["left"]
        .as_f64()
        .ok_or("DOM scrollport left缺失")?
        * dpr;
    let css_top = response["state"]["scrollport"]["top"]
        .as_f64()
        .ok_or("DOM scrollport top缺失")?
        * dpr;
    ensure(
        css_left.is_finite()
            && css_top.is_finite()
            && css_left >= 0.
            && css_top >= 0.
            && (css_left - css_left.round()).abs() < 0.000_001
            && (css_top - css_top.round()).abs() < 0.000_001
            && f64::from(calibrated.offset_left) == css_left.round()
            && f64::from(calibrated.offset_top) == css_top.round(),
        "renderer唯一物理偏移与kiosk实测DOM scrollport相位不同",
    )?;
    let region = ScreenRect {
        left: i32::try_from(i64::from(client_region.left) + i64::from(calibrated.offset_left))?,
        top: i32::try_from(i64::from(client_region.top) + i64::from(calibrated.offset_top))?,
        width: spec.width,
        height: spec.viewport_height,
    };
    report.pass("owned-client-unique-offset",json!({"renderer_world":renderer_check,"calibration":calibrated,"owned_client":rect_json(client_region),"content_roi":rect_json(region),"dpr":dpr,"titlebar_height_guessed":false}))?;
    drop(client);
    let mut session = start(owner, region, &permit)?;
    wait_session(session.get(), "浏览器真实LongSession首帧", |m| {
        m.result_available
            && m.width == region.width
            && m.height == region.height
            && m.generation == m.requested_generation
            && m.phase == LongPhase::Running
    })?;
    session.pause()?;
    let first = session
        .get()
        .stitch_snapshot()
        .ok_or("首帧Pause没有共享结果")?;
    let mut weaks = image_weaks(&first);
    let (first_image, first_check) = pixels(&first, spec, 0)?;
    let renderer_compare = oracle::region(&first_image, &renderer, (0, 0), TOLERANCE)?;
    report.pass("production-first-frame",json!({"world":first_check,"renderer_compare":renderer_compare,"pipeline":session.get().pipeline_probe()}))?;
    drop(first_image);
    drop(renderer);
    drop(first);
    session.get_mut().set_paused(false)?;
    let wheel = bridge.request("wheel", json!({"delta_y":32}))?;
    let offset = physical_scroll(&wheel["state"], dpr)?;
    ensure(
        offset > 0 && offset < spec.viewport_height,
        "真实DOM wheel位移无效",
    )?;
    let after_renderer = read_renderer(&wheel, &report.root)?;
    let after_check = oracle::world(&after_renderer, spec, offset, TOLERANCE)?;
    wait_session(session.get(), "实际DOM滚动后的DXGI增长", |m| {
        m.phase == LongPhase::Running
            && m.generation == m.requested_generation
            && m.height == spec.viewport_height + offset
            && m.viewport.y == offset
    })?;
    session.get_mut().drain_for_output()?;
    wait_session(session.get(), "浏览器尾帧drain ACK", |m| {
        m.phase == LongPhase::Paused && m.generation == m.requested_generation
    })?;
    let until = Instant::now() + WAIT;
    while !session.quiet() {
        ensure(Instant::now() < until, "浏览器尾帧许可未排空")?;
        thread::sleep(Duration::from_millis(5));
    }
    let grown = session.get().stitch_snapshot().ok_or("drain没有共享长图")?;
    weaks.extend(image_weaks(&grown));
    let (grown_image, grown_check) = pixels(&grown, spec, 0)?;
    let tail = oracle::region(&grown_image, &after_renderer, (0, offset), TOLERANCE)?;
    report.pass("actual-dom-wheel-and-production-growth",json!({"wheel":wheel["wheel"],"state":wheel["state"],"world":grown_check,"renderer_world":after_check,"renderer_final_viewport":tail,"output_size":[grown.width,grown.height],"viewport":format!("{:?}",grown.viewport),"pipeline":session.get().pipeline_probe()}))?;
    drop(grown_image);
    drop(after_renderer);
    let content_gate = (owner.bind_content(region, &permit)?)(region)?;
    report.stage(
        "before-owned-occlusion",
        json!({"region":rect_json(region),"session":format!("{:?}",session.get().snapshot())}),
    )?;
    let mut occluder = Occluder::open(region)?;
    let denied = content_gate()
        .err()
        .ok_or("自有不透明遮挡窗没有被归属门拒绝")?
        .to_string();
    let retained = session
        .get()
        .stitch_snapshot()
        .ok_or("遮挡时最后可信结果丢失")?;
    ensure(
        same_sources(&grown, &retained),
        "遮挡门拒绝改变了旧source Arc",
    )?;
    occluder.close()?;
    ensure(content_gate().is_err(), "遮挡关闭自动复活了旧lease gate")?;
    let closed = session.close()?;
    report.pass("owned-opaque-occlusion-rejected",json!({"gate_error":denied,"occluder_hwnd":occluder.hwnd,"owned_window_destroyed":true,"old_gate_still_rejected_after_uncover":true,"old_source_arcs_preserved":true,"close":closed}))?;
    drop(retained);

    // 遮挡已永久撤销原 token；独立新 session 才能证明主动 revoke 的拒绝原因。
    let renewed = bridge.request("new_lease", json!({}))?;
    ensure(
        physical_scroll(&renewed["state"], dpr)? == offset,
        "遮挡后显式newlease的DOM位置改变",
    )?;
    ensure(content_gate().is_err(), "显式newlease复活了遮挡旧gate")?;
    let revoke_permit = recalibrate(bridge, report, owner, spec, offset, region)?;
    let revoke_gate = (owner.bind_content(region, &revoke_permit)?)(region)?;
    revoke_gate()?;
    let mut revoke_session = start(owner, region, &revoke_permit)?;
    wait_session(
        revoke_session.get(),
        "revoke独立session真实首帧",
        |m| {
            m.result_available
                && m.phase == LongPhase::Running
                && m.height == region.height
                && m.generation == m.requested_generation
        },
    )?;
    revoke_session.pause()?;
    let revoke_snapshot = revoke_session
        .get()
        .stitch_snapshot()
        .ok_or("revoke独立session首帧丢失")?;
    weaks.extend(image_weaks(&revoke_snapshot));
    let (revoke_image, revoke_check) = pixels(&revoke_snapshot, spec, offset)?;
    drop(revoke_image);
    report.stage(
        "before-explicit-lease-revocation",
        json!({"session":format!("{:?}",revoke_session.get().snapshot()),"world":revoke_check}),
    )?;
    bridge.request("revoke_lease", json!({}))?;
    let denied = revoke_gate()
        .err()
        .ok_or("撤销DOM lease后新门仍有效")?
        .to_string();
    revoke_session.get_mut().capture_once()?;
    wait_session(
        revoke_session.get(),
        "真实producer拒绝撤销lease",
        |m| m.phase == LongPhase::Failed && m.generation == m.requested_generation,
    )?;
    let retained = revoke_session
        .get()
        .smoke_committed_snapshot()
        .ok_or("lease失败丢失可信图像")?;
    ensure(
        same_sources(&revoke_snapshot, &retained),
        "撤销lease后原source Arc被替换",
    )?;
    let closed = revoke_session.close()?;
    report.pass("revoked-lease-production-rejection",json!({"gate_error":denied,"world":revoke_check,"old_source_arcs_preserved":true,"close":closed}))?;
    drop(retained);
    drop(revoke_snapshot);
    drop(grown);

    // 重新安装一个新的 lease/session，避免用前项已撤销的门冒充 browser-close 校验。
    let renewed = bridge.request("new_lease", json!({}))?;
    ensure(
        physical_scroll(&renewed["state"], dpr)? == offset,
        "重新lease时DOM位置改变",
    )?;
    ensure(content_gate().is_err(), "新lease复活了旧gate")?;
    ensure(revoke_gate().is_err(), "新lease复活了主动revoke旧gate")?;
    let new_permit = recalibrate(bridge, report, owner, spec, offset, region)?;
    let new_gate = (owner.bind_content(region, &new_permit)?)(region)?;
    new_gate()?;
    let mut second = start(owner, region, &new_permit)?;
    wait_session(second.get(), "新lease真实初帧", |m| {
        m.result_available
            && m.phase == LongPhase::Running
            && m.height == region.height
            && m.generation == m.requested_generation
    })?;
    second.pause()?;
    let final_snapshot = second.get().stitch_snapshot().ok_or("newlease首帧丢失")?;
    weaks.extend(image_weaks(&final_snapshot));
    let (image, check) = pixels(&final_snapshot, spec, offset)?;
    drop(image);
    let browser_closed = bridge.request("close_browser", json!({}))?;
    let browser_hwnd = owner.config().hwnd;
    let until = Instant::now() + WAIT;
    while unsafe { IsWindow(Some(HWND(browser_hwnd as *mut _))) }.as_bool() {
        ensure(Instant::now() < until, "真实browser HWND未关闭")?;
        thread::sleep(Duration::from_millis(5));
    }
    let native_closed_error = BrowserCaptureOwner::from_config(owner.config().clone())
        .err()
        .ok_or("browser已关闭却仍能建立新的原生owner")?
        .to_string();
    let denied = new_gate()
        .err()
        .ok_or("真实browser关闭后新gate仍允许采集")?
        .to_string();
    second.get_mut().capture_once()?;
    wait_session(second.get(), "浏览器关闭producer拒绝", |m| {
        m.phase == LongPhase::Failed && m.generation == m.requested_generation
    })?;
    let retained = second
        .get()
        .smoke_committed_snapshot()
        .ok_or("browser关闭后可信结果丢失")?;
    ensure(
        same_sources(&final_snapshot, &retained),
        "browser关闭改变旧结果source Arc",
    )?;
    let closed = second.close()?;
    report.pass("actual-browser-close-retains-last-output",json!({"response":browser_closed,"native_hwnd_destroyed":true,"fresh_native_owner_rejected":true,"native_closed_error":native_closed_error,"close_operation_also_revokes_control_lease":true,"gate_error":denied,"world":check,"old_source_arcs_preserved":true,"close":closed}))?;
    drop(retained);
    drop(final_snapshot);
    drop(session);
    drop(revoke_session);
    drop(second);
    ensure(
        weaks.iter().all(|image| image.upgrade().is_none()),
        "正常关闭后源图Arc仍存活",
    )?;
    ensure(
        long_capture_pipeline::active_capture_threads() == 0
            && long_capture_runtime::active_workers() == 0
            && long_capture_runtime::panicked_workers() == 0,
        "浏览器采集两端线程没有真实正常退出",
    )?;
    report.pass("capture-resources-released",json!({"source_weak_count":weaks.len(),"all_source_weaks_expired":true,"capture_threads":0,"consumer_sessions":0,"panicked_threads":0}))?;
    bridge.finish()?;
    report.protocol_joined = true;
    report.protocol = bridge.probe();
    Ok(())
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut bridge = Bridge::new()?;
    let (owner, config) = bridge.initial()?;
    let mut report = Report::new(&config, owner.clone())?;
    report.stage(
        "owned-browser-configured",
        json!({"config":config,"native":owner.native_snapshot()}),
    )?;
    let result = if report.native_auto {
        native_auto::verify(&mut bridge, &mut report, &owner)
    } else {
        verify(&mut bridge, &mut report, &owner)
    };
    report.protocol = bridge.probe();
    report.save(
        result.is_ok(),
        result.as_ref().err().map(ToString::to_string),
    )?;
    result
}
