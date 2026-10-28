//! 自有滚动窗口的真实 DXGI 流水线验收；不冻结桌面、不访问剪贴板或发全局输入。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    long_auto::{AutoPhase, AutoScrollOptions},
    long_capture_pipeline,
    long_capture_runtime::{
        self, FrameGate, LongOptions, LongPhase, LongSession, LongSnapshot, MatchHook,
    },
    long_capture_smoke::{Fixture, FixturePattern, ORIGIN, STEP},
    long_document,
    process_memory::{cpu_memory, gpu_memory},
    stitch::{ScrollAxis, StitchOptions, StitchSnapshot},
    Result,
};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};
pub(crate) const WIDTH: u32 = 640;
pub(crate) const HEIGHT: u32 = 480;
pub(crate) const WAIT: Duration = Duration::from_secs(15);
pub(crate) fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_string().into())
    }
}
pub(crate) fn options(interval_ms: u64) -> LongOptions {
    LongOptions {
        stitch: StitchOptions {
            axis: ScrollAxis::Vertical,
            detect_fixed_borders: false,
            fixed_start: Some(0),
            fixed_end: Some(0),
        },
        poll_interval: Duration::from_millis(interval_ms),
    }
}
pub(crate) fn settled(meta: &LongSnapshot) -> bool {
    !meta.busy && !meta.capture_in_flight && meta.generation == meta.requested_generation
}
pub(crate) fn wait(
    session: &LongSession,
    label: &str,
    predicate: impl Fn(&LongSnapshot, &serde_json::Value) -> bool,
) -> Result<LongSnapshot> {
    let end = Instant::now() + WAIT;
    loop {
        let meta = session.snapshot();
        let probe = serde_json::to_value(session.pipeline_probe())?;
        if predicate(&meta, &probe) {
            return Ok(meta);
        }
        ensure(
            meta.phase != LongPhase::Failed,
            &format!("{label}：{}", meta.status),
        )?;
        ensure(
            Instant::now() < end,
            &format!("{label}超时：{} probe={probe}", meta.status),
        )?;
        thread::sleep(Duration::from_millis(3));
    }
}
pub(crate) fn drain(session: &mut LongSession) -> Result<StitchSnapshot> {
    session.drain_for_output()?;
    wait(session, "真实排空与资源释放", |m, p| {
        settled(m)
            && m.phase == LongPhase::Paused
            && p["producer_quiesced"] == true
            && p["queue_depth"] == 0
            && p["consumer_in_flight"] == false
    })?;
    ensure(
        session.snapshot().capture_storage_pixels == 0,
        "排空ACK仍持有采集纹理",
    )?;
    session
        .stitch_snapshot()
        .ok_or_else(|| "排空没有共享输出".into())
}
pub(crate) fn finish(session: &mut LongSession) -> Result<()> {
    session.stop();
    let end = Instant::now() + WAIT;
    while !session.try_join_stopped() {
        ensure(Instant::now() < end, "两端线程未正常join")?;
        thread::sleep(Duration::from_millis(3));
    }
    let meta = session.snapshot();
    let p = serde_json::to_value(session.pipeline_probe())?;
    ensure(
        meta.phase == LongPhase::Stopped
            && meta.worker_exited
            && !meta.worker_alive
            && !meta.capture_in_flight
            && p["producer_alive"] == false
            && p["consumer_alive"] == false
            && p["reserved_raw_bytes"] == 0,
        "关闭不是两端真实退出/许可归还",
    )?;
    ensure(
        long_capture_runtime::active_workers() == 0
            && long_capture_pipeline::active_capture_threads() == 0
            && long_capture_runtime::panicked_workers() == 0
            && long_capture_pipeline::panicked_capture_threads() == 0,
        "关闭后线程残留或panic",
    )
}
pub(crate) fn oracle(snapshot: &StitchSnapshot, expected_height: u32) -> Result<usize> {
    ensure(
        snapshot.width == WIDTH && snapshot.height == expected_height,
        "流水线输出尺寸不符",
    )?;
    let actual = long_document::render_region(
        snapshot,
        Rect::full(WIDTH, expected_height),
        WIDTH,
        expected_height,
    )?;
    oracle_image(&actual, expected_height)?;
    Ok(actual.bgra.len() / 4)
}
pub(crate) fn oracle_image(actual: &Image, expected_height: u32) -> Result<()> {
    let expected = crate::long_capture_smoke::scene_for_pattern(
        FixturePattern::Noise,
        WIDTH,
        expected_height,
        ScrollAxis::Vertical,
        (0, 0),
        (ORIGIN, ORIGIN),
        0,
    );
    ensure(
        actual.width == WIDTH && actual.height == expected_height && actual.bgra == expected.bgra,
        "真实DXGI结果与独立世界坐标全像素oracle不同",
    )
}
fn source_refs(snapshot: &StitchSnapshot) -> Vec<Weak<Image>> {
    let mut seen = HashSet::new();
    snapshot
        .segments
        .iter()
        .filter(|s| seen.insert(Arc::as_ptr(&s.image)))
        .map(|s| Arc::downgrade(&s.image))
        .collect()
}
#[derive(Default)]
struct LatchState {
    armed: bool,
    entered: bool,
    released: bool,
}
#[derive(Default)]
pub(crate) struct MatchLatch {
    state: Mutex<LatchState>,
    wake: Condvar,
}
impl MatchLatch {
    pub(crate) fn arm(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = LatchState {
            armed: true,
            entered: false,
            released: false,
        };
    }
    pub(crate) fn entered(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).entered
    }
    pub(crate) fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.released = true;
        state.armed = false;
        self.wake.notify_all();
    }
    pub(crate) fn hook(self: &Arc<Self>) -> MatchHook {
        let latch = self.clone();
        Arc::new(move |_| {
            latch.checkpoint();
            Ok(())
        })
    }
    fn checkpoint(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.armed {
            state.entered = true;
            self.wake.notify_all();
            while !state.released {
                state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}
pub(crate) struct ReleaseLatch(pub(crate) Arc<MatchLatch>);
impl Drop for ReleaseLatch {
    fn drop(&mut self) {
        self.0.release();
    }
}
pub(crate) struct Report {
    pub(crate) root: PathBuf,
    pub(crate) requested: Vec<String>,
    pub(crate) groups: Vec<serde_json::Value>,
    pub(crate) stages: Vec<serde_json::Value>,
}
impl Report {
    pub(crate) fn save(&self, complete: bool, error: Option<&str>) -> Result<()> {
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "suite":"long-pipeline","pid":std::process::id(),"ok":complete&&error.is_none(),"error":error,
                "requested_groups":self.requested,"groups":self.groups,"stages":self.stages,
                "real_dxgi_capture":true,"full_desktop_freeze_used":false,"clipboard_untouched":true,"global_input_sent":false,
                "limitations":["DXGI取得输出帧但CPU只读取自有ROI；Win32归属检查到Copy仍有竞态",
                    "时间样本是实际read起止而非新增桌面帧率；背压/驱动/调度会超过16ms目标",
                    "暂停时采集器释放不代表GPUI设备或系统驱动缓存全部归零",
                    "自有噪声滚动页和有限多轮不证明第三方网页、混合DPI或长期无泄漏",
                    "瞬态许可以64MiB为目标并最多4帧；单帧超目标时仅容许1帧，可信基线/条带/灰度另计",
                    "Release使用panic=abort；core unwind验证不代表Release捕获panic并保留会话"],
            }))?,
        )?;
        Ok(())
    }
    pub(crate) fn record(&mut self, stage: &str, detail: serde_json::Value) -> Result<()> {
        self.stages.push(serde_json::json!({"stage":stage,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail}));
        self.save(false, None)
    }
    fn pass(&mut self, group: &str, detail: serde_json::Value) -> Result<()> {
        self.groups
            .push(serde_json::json!({"name":group,"ok":true,"detail":detail}));
        self.save(false, None)
    }
}
fn fixture(region: ScreenRect) -> Result<Fixture> {
    Fixture::new_owned(region, ScrollAxis::Vertical, (0, 0), FixturePattern::Noise)
}
fn start(fixture: &Fixture, hook: Option<MatchHook>, interval: u64) -> Result<LongSession> {
    LongSession::start_guarded_with_match_hook(
        fixture.region,
        options(interval),
        fixture.capture_gate()?,
        hook,
    )
}
fn read_statistics(probe: &serde_json::Value) -> serde_json::Value {
    let reads = probe["reads"].as_array().cloned().unwrap_or_default();
    let mut intervals: Vec<f64> = reads
        .windows(2)
        .filter_map(|pair| {
            Some(pair[1]["read_start_ms"].as_f64()? - pair[0]["read_start_ms"].as_f64()?)
        })
        .collect();
    intervals.sort_by(f64::total_cmp);
    let percentile = |p: f64| {
        intervals
            .get(((intervals.len().saturating_sub(1)) as f64 * p).round() as usize)
            .copied()
    };
    serde_json::json!({"sample_count":reads.len(),"read_start_interval_p50_ms":percentile(0.5),
        "read_start_interval_p95_ms":percentile(0.95),"read_start_interval_max_ms":intervals.last(),"target_ms":16,"new_desktop_fps_claimed":false})
}
fn cadence(region: ScreenRect, report: &mut Report) -> Result<()> {
    let mut fixture = fixture(region)?;
    let latch = Arc::new(MatchLatch::default());
    let release = ReleaseLatch(latch.clone());
    let mut session = start(&fixture, Some(latch.hook()), 16)?;
    session.set_smoke_coalescing(false);
    wait(&session, "首帧", |m, _| {
        m.height == HEIGHT && m.accepted_frames == 1
    })?;
    latch.arm();
    wait(&session, "真实匹配停点和FIFO背压", |_, p| {
        latch.entered()
            && p["queue_depth"] == 2
            && p["last_issued_sequence"].as_u64().unwrap_or(0) >= 4
            && p["wait_reason"] == "queue-full"
    })?;
    let held = serde_json::to_value(session.pipeline_probe())?;
    ensure(
        held["peak_queue_depth"] == 2
            && held["peak_reserved_raw_bytes"].as_u64().unwrap_or(u64::MAX)
                <= held["raw_budget_bytes"].as_u64().unwrap_or(0),
        "真实FIFO容量或raw预算超限",
    )?;
    report.record("consumer-held-producer-still-read", held)?;
    latch.release();
    wait(&session, "释放匹配后恢复实际读取", |_, p| {
        p["reads"].as_array().is_some_and(|r| r.len() >= 18)
    })?;
    let probe = serde_json::to_value(session.pipeline_probe())?;
    let stats = read_statistics(&probe);
    let snapshot = drain(&mut session)?;
    let checks = oracle(&snapshot, HEIGHT)?;
    drop(snapshot);
    finish(&mut session)?;
    drop(session);
    fixture.close()?;
    drop(release);
    report.pass("cadence", serde_json::json!({"producer_reads_during_match_hold":true,"fifo_backpressure_observed":true,
        "coalescing_enabled":false,"statistics":stats,"probe":probe,"pixel_checks":checks,"gate":fixture.capture_probe_snapshot()}))
}
fn fast_scroll(region: ScreenRect, report: &mut Report) -> Result<()> {
    let mut fixture = fixture(region)?;
    fixture.set_limits(ORIGIN, ORIGIN + 3072)?;
    let mut session = start(&fixture, None, 16)?;
    wait(&session, "快滚首帧及首个预览就绪", |m, _| {
        m.height == HEIGHT && m.phase == LongPhase::Running && !m.busy && m.preview_revision > 0
    })?;
    let before = session.snapshot().preview_revision;
    let started = Instant::now();
    let mut presentation_samples = Vec::with_capacity(32);
    for step in 0..32 {
        fixture.wheel(ScrollAxis::Vertical, 3)?;
        let meta = session.snapshot();
        presentation_samples.push(serde_json::json!({"step":step+1,"elapsed_ms":started.elapsed().as_millis(),
            "committed_height":meta.height,"viewport_y":meta.viewport.y,"poll_sequence":meta.poll_count}));
        // 固定呈现节奏，不等待拼接；无间隔压力失败保留为当前吞吐边界。
        let next = started + Duration::from_millis((step + 1) * 40);
        if let Some(delay) = next.checked_duration_since(Instant::now()) {
            thread::sleep(delay);
        }
    }
    report.record(
        "fast-owned-presentations",
        serde_json::json!({"minimum_step_interval_ms":40,"samples":presentation_samples}),
    )?;
    wait(&session, "快速连续呈现最终视口", |m, _| {
        m.height == HEIGHT + 3072 && m.viewport.y == 3072
    })?;
    let snapshot = drain(&mut session)?;
    let checks = oracle(&snapshot, HEIGHT + 3072)?;
    let p = serde_json::to_value(session.pipeline_probe())?;
    ensure(
        session.snapshot().preview_revision > before,
        "快滚后的预览没有增长",
    )?;
    report.record("fast-owned-scroll-final", serde_json::json!({"probe":p,"elapsed_ms":started.elapsed().as_millis(),"pixel_checks":checks}))?;
    let refs = source_refs(&snapshot);
    drop(snapshot);
    finish(&mut session)?;
    drop(session);
    ensure(
        refs.iter().all(|w| w.upgrade().is_none()),
        "快滚结束原图残留",
    )?;
    fixture.close()?;
    report.pass(
        "fast-scroll",
        serde_json::json!({"presented_steps":32,"notches_per_step":3,"pixels_per_step":96,
        "waited_for_stitch_each_step":false,"minimum_step_interval_ms":40,"dimensions":[WIDTH,HEIGHT+3072],"pixel_checks":checks,
        "probe":p,"source_weaks_expired":true,"gate":fixture.capture_probe_snapshot()}),
    )
}
fn auto_fence(region: ScreenRect, report: &mut Report) -> Result<()> {
    let mut fixture = fixture(region)?;
    fixture.set_limits(ORIGIN, ORIGIN + 3 * STEP)?;
    // 在真实匹配边界制造有限背压，确保实际 post 时存在旧 permit/排队帧。
    let hook: MatchHook = Arc::new(|_| {
        thread::sleep(Duration::from_millis(40));
        Ok(())
    });
    let mut session = start(&fixture, Some(hook), 16)?;
    wait(&session, "自动首帧", |m, _| {
        m.height == HEIGHT && m.phase == LongPhase::Running
    })?;
    session.set_auto_scroll(Some(AutoScrollOptions::default()))?;
    // 连续收集有界生产 probe；2500ms 末尾等待可能正常淘汰最初的 post 事件。
    let sampled_events = RefCell::new(Vec::new());
    let meta = wait(&session, "实际自动滚动到达边界", |m, probe| {
        if let Some(events) = probe["auto_events"].as_array() {
            let mut sampled = sampled_events.borrow_mut();
            for event in events {
                if !sampled.contains(event) {
                    sampled.push(event.clone());
                }
            }
        }
        m.height == HEIGHT + 3 * STEP as u32
            && !m.auto.active
            && m.auto.phase == AutoPhase::NoMotion
            && m.phase == LongPhase::Running
    })?;
    let probe = serde_json::to_value(session.pipeline_probe())?;
    ensure(
        meta.auto.steps == fixture.wheel_count() && meta.auto.steps == 8,
        "自动post计数与自有接收不一致或重复盲发",
    )?;
    ensure(
        probe["auto_ignored_pre_post_frames"].as_u64().unwrap_or(0) > 0,
        "实际自动post没有验证到旧permit帧过滤",
    )?;
    let events = sampled_events.into_inner();
    ensure(
        events.iter().any(|e| e["event"] == "post")
            && events.iter().any(|e| e["event"] == "ignore-pre-post"),
        "有界事件中没有实际post和旧帧过滤",
    )?;
    for event in &events {
        if let Some(fence) = event["fence_sequence"].as_u64() {
            let sequence = event["frame_sequence"]
                .as_u64()
                .ok_or("实际自动事件无帧序号")?;
            ensure(
                match event["event"].as_str() {
                    Some("observe") => sequence > fence,
                    Some("ignore-pre-post" | "post") => sequence <= fence,
                    _ => false,
                },
                "自动响应观察越过了实际post栅栏",
            )?;
        }
    }
    let snap = drain(&mut session)?;
    let checks = oracle(&snap, HEIGHT + 3 * STEP as u32)?;
    drop(snap);
    finish(&mut session)?;
    drop(session);
    fixture.close()?;
    report.pass(
        "auto-fence",
        serde_json::json!({"post_steps":meta.auto.steps,"received_wheels":fixture.wheel_count(),
        "boundary_phase":"NoMotion","match_delay_ms":40,"pre_post_queued_frames_ignored":true,
        "manual_capture_kept_running":meta.phase == LongPhase::Running,"wheel_events":fixture.wheel_events(),
        "sampled_auto_events":events,
        "probe":probe,"pixel_checks":checks,"gate":fixture.capture_probe_snapshot()}),
    )
}
fn close_memory(region: ScreenRect, report: &mut Report) -> Result<()> {
    let mut cases = vec![];
    for mode in [
        "full-queue",
        "pending-drain",
        "paused-drop",
        "in-flight-stop",
        "owned-fixture-destroy",
    ] {
        let mut fixture = fixture(region)?;
        let latch = Arc::new(MatchLatch::default());
        let release = ReleaseLatch(latch.clone());
        let owned = fixture.capture_gate()?;
        let gate_latch = latch.clone();
        let gate: FrameGate = Arc::new(move || {
            owned()?;
            if mode == "in-flight-stop"
                && thread::current().name() == Some("rustshot-long-producer")
            {
                gate_latch.checkpoint();
            }
            Ok(())
        });
        let hook = (mode != "in-flight-stop").then(|| latch.hook());
        let mut session =
            LongSession::start_guarded_with_match_hook(region, options(16), gate, hook)?;
        session.set_smoke_coalescing(false);
        wait(&session, "关闭首帧", |m, _| m.height == HEIGHT)?;
        if mode == "paused-drop" {
            let snapshot = drain(&mut session)?;
            drop(snapshot);
        } else if mode == "in-flight-stop" {
            latch.arm();
            wait(&session, "producer实际read归属检查停点", |_, p| {
                latch.entered() && p["capture_in_flight"] == true
            })?;
        } else if mode == "owned-fixture-destroy" {
            let snapshot = drain(&mut session)?;
            oracle(&snapshot, HEIGHT)?;
            drop(snapshot);
            fixture.close()?;
            session.set_paused(false)?;
            let end = Instant::now() + WAIT;
            loop {
                let meta = session.snapshot();
                if meta.phase == LongPhase::Failed && settled(&meta) {
                    break;
                }
                ensure(Instant::now() < end, "自有源销毁未真实报告失败")?;
                thread::sleep(Duration::from_millis(3));
            }
            let snapshot = session.stitch_snapshot().ok_or("源销毁后可信结果丢失")?;
            oracle(&snapshot, HEIGHT)?;
            drop(snapshot);
        } else {
            latch.arm();
            wait(&session, "关闭前真实满队列", |_, p| {
                latch.entered() && p["queue_depth"] == 2
            })?;
            if mode == "pending-drain" {
                session.drain_for_output()?;
            }
        }
        let p = serde_json::to_value(session.pipeline_probe())?;
        let snapshot = session
            .smoke_committed_snapshot()
            .or_else(|| session.stitch_snapshot());
        let refs = snapshot.as_ref().map(source_refs).unwrap_or_default();
        drop(snapshot);
        let worker = session.worker_probe();
        session.stop();
        latch.release();
        let directly_joined = mode != "paused-drop";
        if directly_joined {
            finish(&mut session)?;
        }
        drop(session);
        let end = Instant::now() + WAIT;
        while !worker.load(std::sync::atomic::Ordering::Acquire)
            || long_capture_runtime::active_workers() != 0
            || long_capture_pipeline::active_capture_threads() != 0
            || refs.iter().any(|w| w.upgrade().is_some())
        {
            ensure(Instant::now() < end, "Drop/关闭后真实资源未释放")?;
            thread::sleep(Duration::from_millis(3));
        }
        fixture.close()?;
        drop(release);
        let value = serde_json::json!({"mode":mode,"before_close_probe":p,"direct_join_observed":directly_joined,
            "source_weaks_expired":true,"capture_threads":0,"consumer_sessions":0,"forced_cleanup":false,
            "gate":fixture.capture_probe_snapshot()});
        report.record("pipeline-close-released", value.clone())?;
        cases.push(value);
    }
    report.pass("close-memory",serde_json::json!({"cases":cases,"finite_rounds":5,
        "in_flight_scope":"owned validation checkpoint in actual producer read; not a driver hang"}))
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_LONG_PIPELINE_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/long-pipeline"));
    std::fs::create_dir_all(&root)?;
    let requested: Vec<String> = std::env::var("RUSTSHOT_LONG_PIPELINE_GROUPS")
        .unwrap_or_else(|_| "cadence,fast-scroll,drain,auto-fence,close-memory".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    ensure(
        !requested.is_empty()
            && requested.iter().all(|s| {
                matches!(
                    s.as_str(),
                    "cadence" | "fast-scroll" | "drain" | "auto-fence" | "close-memory"
                )
            })
            && requested.iter().collect::<HashSet<_>>().len() == requested.len(),
        "流水线请求组无效",
    )?;
    let work = crate::long_capture_smoke::work_area()?;
    ensure(
        work.right - work.left >= 1280 && work.bottom - work.top >= 800,
        "自有采集与操作条空间不足",
    )?;
    let region = ScreenRect {
        left: work.left + 64,
        top: work.top + 180,
        width: WIDTH,
        height: HEIGHT,
    };
    let mut report = Report {
        root,
        requested,
        groups: vec![],
        stages: vec![],
    };
    let result = (|| -> Result<()> {
        report.record(
            "before-pipeline",
            serde_json::json!({"capture_threads":long_capture_pipeline::active_capture_threads()}),
        )?;
        for group in report.requested.clone() {
            match group.as_str() {
                "cadence" => cadence(region, &mut report)?,
                "fast-scroll" => fast_scroll(region, &mut report)?,
                "auto-fence" => auto_fence(region, &mut report)?,
                "close-memory" => close_memory(region, &mut report)?,
                "drain" => crate::long_pipeline_ui_smoke::run(region, &mut report)?,
                _ => unreachable!(),
            }
        }
        Ok(())
    })();
    report.save(
        true,
        result.as_ref().err().map(|e| e.to_string()).as_deref(),
    )?;
    result
}
