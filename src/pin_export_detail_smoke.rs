//! 放大导出细节专项：仅合成 Pin、自有 HWND/文件；Shell 源永不返回 DROP。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
    gpui_capture::hwnd,
    pin_drag_request::{DragTicket, NativeDragRequest},
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{
    cell::RefCell,
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{implement, BOOL, HRESULT},
    Win32::{
        Foundation::*,
        System::{Ole::*, SystemServices::MODIFIERKEYS_FLAGS, Threading::*},
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
};
const FOLDER: &str = "target/qa/pin-export-details";
const FILTER_ID: usize = 0x5253_6401;
const OBSERVER_ID: usize = 0x5253_6402;
fn ensure(ok: bool, text: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(text.into())
    }
}
fn fixture(width: u32, height: u32) -> Image {
    Image {
        width,
        height,
        bgra: [50, 120, 180, 255].repeat(width as usize * height as usize),
        readback_ms: 0.,
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
// 本专项的手势是 posted input。过滤自有窗口上的物理悬停，避免用户鼠标位置污染夹具。
unsafe extern "system" fn filter(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_MOUSEMOVE && wp.0 & crate::gpui_smoke::SMOKE_MOVE == 0 {
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(h, Some(filter), id) };
    }
    unsafe { DefSubclassProc(h, message, wp, lp) }
}
fn begin(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<(usize, DragTicket)> {
    pin.update(cx, |v, w, cx| -> Result<_> {
        let h = hwnd(w);
        v.gesture = None;
        v.drag_request = None;
        unsafe {
            SetCapture(h);
        }
        let request = NativeDragRequest::begin(h)?;
        let ticket = request.ticket();
        v.drag_request = Some(request);
        v.save_busy(true, cx);
        // 必须安装在请求 subclass 之后，未标记的物理悬停不进入其取消状态。
        unsafe {
            let _ = RemoveWindowSubclass(h, Some(filter), FILTER_ID);
            SetWindowSubclass(h, Some(filter), FILTER_ID, 0).ok()?;
        }
        ensure(
            unsafe { GetCapture() } == h && !ticket.cancelled(),
            "请求没有保留自有 capture",
        )?;
        Ok((h.0 as usize, ticket))
    })
    .map_err(|e| e.to_string())?
}
fn end(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| {
        v.drag_request = None;
        v.gesture = None;
        v.save_busy(false, cx);
        unsafe {
            let _ = RemoveWindowSubclass(hwnd(w), Some(filter), FILTER_ID);
        }
    })
    .map_err(|e| e.to_string().into())
}
fn post(h: usize, message: u32, wp: usize, lp: isize) -> Result<()> {
    unsafe {
        PostMessageW(Some(HWND(h as *mut _)), message, WPARAM(wp), LPARAM(lp))?;
    }
    Ok(())
}
async fn request_events(
    pin: WindowHandle<PinView>,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (h, old) = begin(pin, cx)?;
    // client 外负坐标仍路由到有 capture 的自有窗口，不移动系统鼠标。
    post(
        h,
        WM_LBUTTONUP,
        0,
        (((-20_i16 as u16 as u32) << 16) | (-30_i16 as u16 as u32)) as isize,
    )?;
    tick(cx).await;
    ensure(old.cancelled(), "窗口外松开没有取消旧拖出请求")?;
    post(h, WM_LBUTTONDOWN, 1, (40 << 16) | 40)?;
    tick(cx).await;
    ensure(old.cancelled(), "再次按下复活了旧请求")?;
    end(pin, cx)?;
    let (_, fresh) = begin(pin, cx)?;
    ensure(
        !old.same_request(&fresh) && !fresh.cancelled(),
        "新请求与已取消请求不独立",
    )?;
    end(pin, cx)?;
    logs.push(serde_json::json!({"group":"native-drag-release-repress-independent-request","ok":true,"old_cancelled":old.cancelled(),"new_identity":true,"input":"owned posted outside-client UP then DOWN"}));
    for (name, message, value) in [
        ("escape", WM_KEYDOWN, usize::from(VK_ESCAPE.0)),
        ("cancelmode", WM_CANCELMODE, 0),
    ] {
        let (h, ticket) = begin(pin, cx)?;
        post(h, message, value, 0)?;
        tick(cx).await;
        ensure(ticket.cancelled(), &format!("{name}没有取消原生请求"))?;
        end(pin, cx)?;
        logs.push(serde_json::json!({"group":format!("native-drag-{name}"),"ok":true,"cancelled":ticket.cancelled()}));
    }
    let other =
        cx.update(|cx| PinView::open_state(fixture(64, 64), (180, 180), PinState::default(), cx))??;
    let (_, ticket) = begin(pin, cx)?;
    other
        .update(cx, |_, w, _| unsafe {
            SetCapture(hwnd(w));
        })
        .map_err(|e| e.to_string())?;
    ensure(ticket.cancelled(), "真实捕获转移到另一个自有窗口没有取消")?;
    end(pin, cx)?;
    other
        .update(cx, |_, w, _| w.remove_window())
        .map_err(|e| e.to_string())?;
    logs.push(serde_json::json!({"group":"native-drag-capture-loss","ok":true,"transfer":"another-owned-Pin-HWND","cancelled":ticket.cancelled()}));
    let doomed =
        cx.update(|cx| PinView::open_state(fixture(64, 64), (180, 180), PinState::default(), cx))??;
    let (dead_hwnd, ticket) = begin(doomed, cx)?;
    doomed
        .update(cx, |_, w, _| w.remove_window())
        .map_err(|e| e.to_string())?;
    for _ in 0..12 {
        tick(cx).await;
        if !unsafe { IsWindow(Some(HWND(dead_hwnd as *mut _))) }.as_bool() {
            break;
        }
    }
    ensure(
        ticket.cancelled() && !unsafe { IsWindow(Some(HWND(dead_hwnd as *mut _))) }.as_bool(),
        "窗口销毁后请求/capture仍有效",
    )?;
    logs.push(serde_json::json!({"group":"native-drag-window-destroy","ok":true,"cancelled":ticket.cancelled(),"hwnd_destroyed":true}));
    // 普通输出共用快照仍结束原生捕获；保留只属于 Drag 路由。
    pin.update(cx, |v, w, cx| -> Result<()> {
        unsafe {
            SetCapture(hwnd(w));
        }
        let _ = v.user_export_snapshot(cx)?;
        ensure(
            unsafe { GetCapture() }.is_invalid(),
            "非Drag导出没有释放capture",
        )
    })
    .map_err(|e| e.to_string())??;
    logs.push(serde_json::json!({"group":"native-nondrag-snapshot-releases-capture","ok":true}));
    Ok(())
}
#[derive(Default)]
struct ProbeStats {
    thread: AtomicU32,
    queries: AtomicUsize,
    saw_active: AtomicBool,
    saw_cancelled: AtomicBool,
    capture_changes: AtomicUsize,
    capture_hwnd: AtomicUsize,
    capture_thread: AtomicU32,
    null_capture: AtomicBool,
    sta_capture_hwnd: AtomicUsize,
    sta_capture_thread: AtomicU32,
    source_capture_owned: AtomicBool,
    owned_escape_posts: AtomicUsize,
    pre_capture_gate_ready: AtomicBool,
}
#[derive(Clone, Copy)]
enum ProbeCase {
    Observe,
    CancelBeforeQuery,
    CancelBeforeCapture,
}
impl ProbeCase {
    fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe-handoff",
            Self::CancelBeforeQuery => "cancel-before-first-query",
            Self::CancelBeforeCapture => "cancel-before-capture",
        }
    }
}
unsafe extern "system" fn capture_observer(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let stats = unsafe { &*(data as *const ProbeStats) };
    if message == WM_CAPTURECHANGED {
        stats.capture_changes.fetch_add(1, Ordering::Relaxed);
        stats.capture_hwnd.store(lp.0 as usize, Ordering::Release);
        let next = HWND(lp.0 as *mut _);
        if next.is_invalid() {
            stats.null_capture.store(true, Ordering::Release);
        } else {
            stats.capture_thread.store(
                unsafe { GetWindowThreadProcessId(next, None) },
                Ordering::Release,
            );
        }
    }
    if message == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(h, Some(capture_observer), id) };
    }
    unsafe { DefSubclassProc(h, message, wp, lp) }
}
#[implement(IDropSource)]
struct ProbeSource {
    ticket: DragTicket,
    stats: Arc<ProbeStats>,
}
#[allow(non_snake_case)]
impl IDropSource_Impl for ProbeSource_Impl {
    fn QueryContinueDrag(&self, _: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        self.ticket.observe_ole_capture();
        self.stats.queries.fetch_add(1, Ordering::Relaxed);
        let capture = unsafe { GetCapture() };
        let mut process = 0;
        let thread = if capture.is_invalid() {
            0
        } else {
            unsafe { GetWindowThreadProcessId(capture, Some(&mut process)) }
        };
        if thread != 0
            && thread == unsafe { GetCurrentThreadId() }
            && process == unsafe { GetCurrentProcessId() }
        {
            self.stats
                .sta_capture_hwnd
                .store(capture.0 as usize, Ordering::Release);
            self.stats
                .sta_capture_thread
                .store(thread, Ordering::Release);
            self.stats
                .source_capture_owned
                .store(true, Ordering::Release);
        }
        let active = self.ticket.ole_active();
        let cancelled = self.ticket.cancelled();
        self.stats.saw_active.fetch_or(active, Ordering::AcqRel);
        self.stats
            .saw_cancelled
            .fetch_or(cancelled, Ordering::AcqRel);
        // 首次回调记录真实接管顺序后立即取消；不等待或依赖用户的物理输入。
        // 此受控源没有返回 DROP 的代码路径。
        DRAGDROP_S_CANCEL
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
struct CaptureObserver {
    handle: usize,
    stats: Arc<ProbeStats>,
}
impl Drop for CaptureObserver {
    fn drop(&mut self) {
        let h = HWND(self.handle as *mut _);
        unsafe {
            if IsWindow(Some(h)).as_bool()
                && !RemoveWindowSubclass(h, Some(capture_observer), OBSERVER_ID).as_bool()
            {
                // 非预期移除失败时保留指针的强引用，不能使仍安装的回调悬空。
                let _ = Arc::into_raw(self.stats.clone());
            }
        }
    }
}
async fn ole_probe_case(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
    case: ProbeCase,
) -> Result<serde_json::Value> {
    let cancel_before_first_query = !matches!(case, ProbeCase::Observe);
    let (h, ticket) = begin(pin, cx)?;
    let stats = Arc::new(ProbeStats::default());
    let pointer = Arc::as_ptr(&stats) as usize;
    pin.update(cx, |_, w, _| unsafe {
        SetWindowSubclass(hwnd(w), Some(capture_observer), OBSERVER_ID, pointer).ok()
    })
    .map_err(|e| e.to_string())??;
    let observer = CaptureObserver {
        handle: h,
        stats: stats.clone(),
    };
    let path = Path::new(FOLDER).join("owned-ole-probe.png");
    crate::export::save_png(fixture(32, 24), &path)?;
    // Shell PIDL 接口要求绝对路径，不能把 QA 相对路径当作生产拖出路径。
    let path = std::fs::canonicalize(path)?;
    let worker_ticket = ticket.clone();
    let worker_stats = stats.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = (|| -> std::result::Result<u32, String> {
            unsafe {
                OleInitialize(None).map_err(|e| e.to_string())?;
            }
            struct Ole(DragTicket);
            impl Drop for Ole {
                fn drop(&mut self) {
                    self.0.clear_ole_capture();
                    unsafe {
                        OleUninitialize();
                    }
                }
            }
            let _ole = Ole(worker_ticket.clone());
            let thread = unsafe { GetCurrentThreadId() };
            worker_stats.thread.store(thread, Ordering::Release);
            let data = crate::pin_drag::data_object(&path).map_err(|e| e.to_string())?;
            let source: IDropSource = ProbeSource {
                ticket: worker_ticket.clone(),
                stats: worker_stats.clone(),
            }
            .into();
            if !worker_ticket.arm_ole(thread) {
                return Err("Shell探针进入前已取消".into());
            }
            let _watch = worker_ticket.watch_ole().map_err(|e| e.to_string())?;
            if worker_ticket.cancelled() {
                return Err("Shell探针建窗前已取消".into());
            }
            if matches!(case, ProbeCase::CancelBeforeCapture) {
                // 为 GetGUIThreadInfo 建立本 STA 的消息队列，不取走任何消息。
                let mut message = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut message, None, WM_APP, WM_APP, PM_NOREMOVE);
                }
                worker_stats
                    .pre_capture_gate_ready
                    .store(true, Ordering::Release);
                if !gate_rx
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|e| e.to_string())?
                {
                    return Err("STA建捕获前取消夹具未建立".into());
                }
                // 模拟最后一次取消检查与 SHDoDragDrop 建捕获之间的竞态。
                // 这里不再次提前退出，才能验证 watch 后续发现捕获并唤醒。
            }
            unsafe { SHDoDragDrop(None, &data, &source, DROPEFFECT_COPY) }
                .map(|effect| effect.0)
                .map_err(|e| e.to_string())
        })();
        let _ = tx.send(result);
    });
    let started = Instant::now();
    let mut timeout_cancel = false;
    let mut pumps = 0;
    let mut cancel_sent_before_query = false;
    let mut queries_at_cancel = None;
    let mut capture_empty_at_cancel = false;
    let mut pre_capture_gate_released = false;
    let result = loop {
        tick(cx).await;
        pumps += 1;
        // 更新真实自有 GPUI 窗口，同时让 UI 的 capture-change 消息正常分派。
        pin.update(cx, |_, _, cx| cx.notify())
            .map_err(|e| e.to_string())?;
        match rx.try_recv() {
            Ok(result) => break Some(result),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                break Some(Err("Shell探针线程未报告".into()))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if started.elapsed() > Duration::from_secs(3) {
            timeout_cancel = true;
            ticket.cancel();
        }
        // OLE 模态循环不保证派发 STA timer；由仍在 pump 的 UI 只唤醒
        // 本实例该 STA 已实际捕获的 HWND，绝不向全局或用户窗口投递输入。
        let sta = stats.thread.load(Ordering::Acquire);
        if sta != 0 {
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let info_ok = unsafe { GetGUIThreadInfo(sta, &mut info) }.is_ok();
            if matches!(case, ProbeCase::CancelBeforeCapture)
                && !pre_capture_gate_released
                && stats.pre_capture_gate_ready.load(Ordering::Acquire)
            {
                let queries = stats.queries.load(Ordering::Acquire);
                capture_empty_at_cancel = info_ok && info.hwndCapture.is_invalid();
                let confirmed = capture_empty_at_cancel && queries == 0;
                if confirmed {
                    queries_at_cancel = Some(queries);
                    ticket.cancel();
                    cancel_sent_before_query = true;
                }
                let _ = gate_tx.send(confirmed);
                pre_capture_gate_released = true;
            }
            if info_ok && !info.hwndCapture.is_invalid() {
                let mut process = 0;
                let actual =
                    unsafe { GetWindowThreadProcessId(info.hwndCapture, Some(&mut process)) };
                if actual == sta && process == unsafe { GetCurrentProcessId() } {
                    stats
                        .sta_capture_hwnd
                        .store(info.hwndCapture.0 as usize, Ordering::Release);
                    if matches!(case, ProbeCase::CancelBeforeQuery) && !cancel_sent_before_query {
                        let queries = stats.queries.load(Ordering::Acquire);
                        if queries == 0 {
                            // 该子例仅靠生产取消/监测唤醒；QA不得投递Esc补绿。
                            queries_at_cancel = Some(queries);
                            ticket.cancel();
                            cancel_sent_before_query = true;
                        }
                    }
                    if !cancel_before_first_query
                        && unsafe {
                            PostMessageW(
                                Some(info.hwndCapture),
                                WM_KEYDOWN,
                                WPARAM(usize::from(VK_ESCAPE.0)),
                                LPARAM(1),
                            )
                        }
                        .is_ok()
                    {
                        stats.owned_escape_posts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if started.elapsed() > Duration::from_secs(6) {
            break None;
        }
    };
    let active = stats.saw_active.load(Ordering::Acquire);
    let capture_thread = stats.capture_thread.load(Ordering::Acquire);
    let sta = stats.thread.load(Ordering::Acquire);
    let no_drop = matches!(&result, Some(Ok(0)));
    let owned_capture = stats.source_capture_owned.load(Ordering::Acquire);
    let observed_sta = stats.sta_capture_thread.load(Ordering::Acquire);
    let source_cancelled = stats.saw_cancelled.load(Ordering::Acquire);
    let qa_esc = stats.owned_escape_posts.load(Ordering::Acquire);
    let worker_exited = result.is_some() && worker.join().is_ok();
    let phase_ok = if cancel_before_first_query {
        cancel_sent_before_query
            && queries_at_cancel == Some(0)
            && source_cancelled
            && qa_esc == 0
            && (!matches!(case, ProbeCase::CancelBeforeCapture)
                || capture_empty_at_cancel && pre_capture_gate_released)
    } else {
        active
    };
    let ok = no_drop
        && phase_ok
        && owned_capture
        && observed_sta == sta
        && sta != 0
        && !timeout_cancel
        && worker_exited;
    let report = serde_json::json!({"mode":case.name(),"ok":ok,"sta_thread":sta,"ui_capture_change_thread":capture_thread,"ui_capture_change_hwnd":stats.capture_hwnd.load(Ordering::Acquire),"capture_change_count":stats.capture_changes.load(Ordering::Acquire),"null_capture_seen":stats.null_capture.load(Ordering::Acquire),"sta_capture_hwnd":stats.sta_capture_hwnd.load(Ordering::Acquire),"sta_capture_thread":observed_sta,"source_capture_owned":owned_capture,"qa_escape_posts":qa_esc,"production_cancel_only":cancel_before_first_query,"ole_active_observed_at_first_query":active,"ticket_cancelled_observed_at_first_query":source_cancelled,"cancel_sent_before_query":cancel_sent_before_query,"queries_at_cancel":queries_at_cancel,"capture_empty_at_cancel":capture_empty_at_cancel,"pre_capture_gate_released":pre_capture_gate_released,"query_calls":stats.queries.load(Ordering::Acquire),"result":result,"ui_pumps":pumps,"timeout_cancel":timeout_cancel,"worker_joined":worker_exited,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"source_never_returns_drop":true,"source_cancels_at_first_query":true,"physical_drag_success_tested":false,"global_input_used":false,"wake":if cancel_before_first_query {"production ticket.cancel()/watch_ole only; no QA Esc"} else {"UI GetGUIThreadInfo(STA), verified own capture HWND, directed Esc"}});
    // observer 指向 stats Arc；UI移除完成后才允许这个本地强引用释放。
    drop(observer);
    end(pin, cx)?;
    Ok(report)
}
async fn ole_probe(
    pin: WindowHandle<PinView>,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let observe = ole_probe_case(pin, cx, ProbeCase::Observe).await?;
    let cancel = ole_probe_case(pin, cx, ProbeCase::CancelBeforeQuery).await?;
    let early = ole_probe_case(pin, cx, ProbeCase::CancelBeforeCapture).await?;
    let ok = [&observe, &cancel, &early]
        .iter()
        .all(|case| case["ok"].as_bool() == Some(true));
    logs.push(serde_json::json!({"group":"native-shell-controlled-cancel-handoff","ok":ok,"subcases":[observe,cancel,early],"source_never_returns_drop":true,"physical_drag_success_tested":false}));
    ensure(ok, "真实Shell捕获/取消子例失败；见保留的Shell组数据")
}
async fn watermark_save(
    pin: WindowHandle<PinView>,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (snapshot, source, scene) = pin
        .update(cx, |v, _, cx| -> Result<_> {
            let snapshot = v.user_export_snapshot(cx)?;
            let scene = v
                .canvas
                .as_ref()
                .unwrap()
                .update(cx, |c, _| c.prepared_scene());
            Ok((snapshot, v.image.clone(), scene))
        })
        .map_err(|e| e.to_string())??;
    let (expected, cropped) = cx
        .background_executor()
        .spawn(async move {
            let expected = snapshot.render().map_err(|e| e.to_string())?;
            let full = Rect::full(700, 400);
            let crop = Rect {
                x: 100,
                y: 40,
                width: 350,
                height: 260,
            };
            let projection = crate::output_projection::OutputProjection::new(crop, 1.5)
                .map_err(|e| e.to_string())?;
            let pixels = crate::scene::SourcePixels {
                fill: None,
                canvas: full,
                planes: vec![crate::scene::PixelPlane {
                    rect: full,
                    bgra: source.as_bytes(0).ok_or("夹具没有像素")?,
                }],
                overlay: None,
            };
            let mut image = projection.background(&pixels).map_err(|e| e.to_string())?;
            scene
                .apply_projected(&mut image, &pixels, &projection)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((expected, image))
        })
        .await
        .map_err(|e| e.to_string())?;
    ensure(
        (expected.width, expected.height) == (1050, 600),
        "目标水印输出尺寸错误",
    )?;
    for y in 0..cropped.height as usize {
        let offset = ((y + 60) * expected.width as usize + 150) * 4;
        let width = cropped.width as usize * 4;
        ensure(
            expected.bgra[offset..offset + width] == cropped.bgra[y * width..(y + 1) * width],
            "边缘小数旋转水印在非零裁剪中跳相位",
        )?;
    }
    let marked = expected
        .bgra
        .chunks_exact(4)
        .filter(|p| *p != [50, 120, 180, 255])
        .count();
    ensure(marked > 100, "水印夹具没有产生有效目标字形")?;
    let folder = crate::preferences::data_dir().join("output");
    let before: std::collections::HashSet<_> = std::fs::read_dir(&folder)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    pin.update(cx, |v, w, cx| v.perform(PinAction::QuickSave, w, cx))
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        tick(cx).await;
        let (busy, status) = pin
            .update(cx, |v, _, _| (v.saving, v.status.clone()))
            .map_err(|e| e.to_string())?;
        if !busy {
            ensure(
                status.starts_with("已保存："),
                &format!("水印快速保存失败：{status}"),
            )?;
            break;
        }
        ensure(
            started.elapsed() < Duration::from_secs(15),
            "水印快速保存超时",
        )?;
    }
    let png = std::fs::read_dir(&folder)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| !before.contains(p) && p.extension().is_some_and(|x| x == "png"))
        .ok_or("没有新的水印PNG")?;
    let saved = crate::export::read_image(&png)?;
    ensure(
        saved.width == expected.width
            && saved.height == expected.height
            && saved.bgra == expected.bgra,
        "实际QuickSave水印PNG与后台目标输出不一致",
    )?;
    logs.push(serde_json::json!({"group":"native-edge-fractional-watermark-quick-save","ok":true,"png":png,"target_dimensions":[1050,600],"rotations":[13.,-23.],"source_origins":[[2.25,3.75],[-2.25,-3.75]],"target_marked_pixels":marked,"png_pixels_exact":true,"nonzero_crop_phase_exact":true}));
    pin.update(cx, |v, w, cx| {
        v.hovered = false;
        v.set_canvas_active(false, w, cx).expect("水印夹具收栏成功");
        w.request_frame_capture([0, 0, 160, 100])
    })
    .map_err(|e| e.to_string())??;
    let mut frame = None;
    for _ in 0..40 {
        tick(cx).await;
        frame = pin
            .update(cx, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?;
        if frame.is_some() {
            break;
        }
    }
    let frame = frame
        .ok_or("自有水印GPU帧没有读回")?
        .map_err(|e| e.to_string())?;
    ensure(frame.len() == 160 * 100 * 4, "水印GPU帧尺寸错误")?;
    let colored = frame
        .chunks_exact(4)
        .filter(|p| u16::from(p[0].abs_diff(50)) + u16::from(p[1].abs_diff(120)) > 8)
        .count();
    ensure(colored > 20, "自有GPUI窗口没有绘制水印内容")?;
    logs.push(serde_json::json!({"group":"native-watermark-owned-gpui-frame","ok":true,"bytes":frame.len(),"colored_pixels":colored,"backend":"owned GPU framebuffer","target_export_and_preview_glyph_masks_equal_tested":false}));
    Ok(())
}
pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("贴图导出细节专项未完成".into());
    let scope = std::env::var("RUSTSHOT_PIN_EXPORT_DETAILS_SCOPE").unwrap_or_else(|_| "all".into());
    let setup = (|| -> Result<_> {
        ensure(
            scope == "all" || scope == "shell" || scope == "requests_shell",
            "导出细节专项 scope 只能为 all、shell 或 requests_shell",
        )?;
        std::fs::create_dir_all(FOLDER)?;
        ensure(
            crate::preferences::data_dir()
                == std::env::current_dir()?.join(FOLDER).join("settings"),
            "专项设置目录没有隔离",
        )?;
        crate::preferences::Preferences {
            history: false,
            pin_restore: false,
            ..Default::default()
        }
        .save()?;
        let mut store = crate::save_store::SaveStore::load();
        store.config = crate::save_options::SaveConfig::default();
        store.config.directory = crate::preferences::data_dir().join("output");
        store.config.filename_template = "QA_details_{width}x{height}_{time}".into();
        store.save()?;
        let pin = PinView::open_state(
            fixture(700, 400),
            (100, 100),
            PinState {
                zoom: 1.5,
                ..Default::default()
            },
            cx,
        )?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            crate::gpui_smoke::isolate_input(hwnd(w))?;
            v.ensure_canvas(w, cx)?;
            v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                for (x, y, rotation, outline, text) in [
                    (2.25, 3.75, 13., false, "Phase QA"),
                    (-2.25, -3.75, -23., true, "边缘 QA"),
                ] {
                    let mut a = Annotation::new(
                        Tool::Watermark,
                        Point { x, y },
                        Style {
                            width: 4.,
                            font_size: 24.,
                            opacity: 0.6,
                            color: 0xe82070,
                            ..Default::default()
                        },
                    );
                    a.update(Point {
                        x: x + 620.,
                        y: y + 360.,
                    });
                    a.rotation = rotation;
                    a.text = text.into();
                    a.style.text.outline = outline;
                    a.style.text.outline_width = 6.75;
                    c.editor.document.insert(a);
                }
                c.editor.select_tool(Tool::Edit);
                c.pin_editing = false;
                cx.notify();
            });
            Ok(())
        })??;
        Ok(pin)
    })();
    let pin = match setup {
        Ok(pin) => pin,
        Err(e) => {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let mut groups = Vec::new();
        for _ in 0..4 { tick(cx).await; }
        let result = async {
            if scope != "shell" {
                request_events(pin, &mut groups, cx).await?;
            }
            if scope == "all" {
                watermark_save(pin, &mut groups, cx).await?;
            }
            ole_probe(pin, &mut groups, cx).await
        }.await;
        let error = result.err().map(|e| e.to_string());
        let report = serde_json::json!({"test":"pin-export-details","scope":scope,"ok":error.is_none(),"error":error,"groups":groups,"clipboard_read":false,"clipboard_written":false,"desktop_read":false,"global_pointer_moved":false,"global_input_injected":false,"source":"owned synthetic Pin/window/file/GPU frame; Shell source always cancels"});
        let write = std::fs::write(Path::new(FOLDER).join("report.json"), serde_json::to_vec_pretty(&report).unwrap_or_default());
        *errors.borrow_mut() = error.or_else(|| write.err().map(|e| e.to_string()));
        println!("{report}");
        let _ = pin.update(cx, |v, w, cx| { v.drag_request = None; v.close_toolbar(cx); w.remove_window(); });
        let _ = cx.update(|cx| cx.quit());
    }).detach();
}
