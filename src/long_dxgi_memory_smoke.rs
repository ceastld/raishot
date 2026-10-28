//! 自有不透明 Win32 窗口的 32000px 实际 DXGI 长采集与取消验收。
//! 仅选区读回，不调用全桌面冻结、剪贴板或系统输入。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    long_capture_runtime::{self, FrameGate, LongOptions, LongPhase, LongSession, LongSnapshot},
    long_capture_smoke::{Fixture, FixturePattern, ORIGIN},
    long_document::{self, LongDocument},
    process_memory::{cpu_memory, gpu_memory},
    stitch::{ScrollAxis, StitchOptions, StitchSnapshot},
    Result,
};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    thread,
    time::{Duration, Instant},
};
const WIDTH: u32 = 960;
const VIEW_HEIGHT: u32 = 640;
const HEIGHT: u32 = 32_000;
const WAIT: Duration = Duration::from_secs(15);

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_string().into())
    }
}
fn options() -> LongOptions {
    LongOptions {
        stitch: StitchOptions {
            axis: ScrollAxis::Vertical,
            detect_fixed_borders: false,
            fixed_start: Some(0),
            fixed_end: Some(0),
        },
        // 手动逐帧确认，不把 QA 节奏当作生产默认 16ms 的吞吐测量。
        poll_interval: Duration::from_millis(100),
    }
}
fn settled(meta: &LongSnapshot) -> bool {
    !meta.busy && !meta.capture_in_flight && meta.generation == meta.requested_generation
}
fn wait(
    session: &LongSession,
    label: &str,
    predicate: impl Fn(&LongSnapshot) -> bool,
) -> Result<LongSnapshot> {
    let end = Instant::now() + WAIT;
    loop {
        let meta = session.snapshot();
        if predicate(&meta) {
            return Ok(meta);
        }
        ensure(
            meta.phase != LongPhase::Failed,
            &format!("{label}：{}", meta.status),
        )?;
        ensure(
            Instant::now() < end,
            &format!(
                "{label}超时：{}×{} phase={:?} poll={} gen={}/{} {}",
                meta.width,
                meta.height,
                meta.phase,
                meta.poll_count,
                meta.generation,
                meta.requested_generation,
                meta.status
            ),
        )?;
        thread::sleep(Duration::from_millis(3));
    }
}
fn pause(session: &mut LongSession) -> Result<(LongSnapshot, StitchSnapshot)> {
    session.set_paused(true)?;
    let meta = wait(session, "实际暂停与 GPU 资源释放", |m| {
        settled(m) && m.phase == LongPhase::Paused
    })?;
    ensure(
        meta.capture_storage_pixels == 0 && meta.capture_screen_count == 0,
        "暂停 ACK 后仍持有采集纹理",
    )?;
    let snapshot = session.stitch_snapshot().ok_or("暂停没有最后成功条带")?;
    Ok((meta, snapshot))
}
fn finish(session: &mut LongSession) -> Result<()> {
    session.stop();
    let end = Instant::now() + WAIT;
    while !session.try_join_stopped() {
        ensure(Instant::now() < end, "真实采集 worker 未完成正常 join")?;
        thread::sleep(Duration::from_millis(3));
    }
    let meta = session.snapshot();
    ensure(
        meta.phase == LongPhase::Stopped
            && meta.worker_exited
            && !meta.worker_alive
            && !meta.capture_in_flight
            && meta.capture_storage_pixels == 0,
        "停止 ACK 不是实际 GPU/线程正常退出",
    )?;
    ensure(
        long_capture_runtime::panicked_workers() == 0,
        "实际采集线程发生过panic",
    )?;
    ensure(
        long_capture_runtime::active_workers() == 0,
        "正常 join 后还有采集 worker",
    )
}
fn source_refs(snapshot: &StitchSnapshot) -> (Vec<Weak<Image>>, u64) {
    let mut seen = HashSet::new();
    let mut bytes = 0_u64;
    let refs = snapshot
        .segments
        .iter()
        .filter_map(|segment| {
            if seen.insert(Arc::as_ptr(&segment.image)) {
                bytes += segment.image.bgra.len() as u64;
                Some(Arc::downgrade(&segment.image))
            } else {
                None
            }
        })
        .collect();
    (refs, bytes)
}
fn oracle(snapshot: &StitchSnapshot) -> Result<usize> {
    let mut rows: Vec<_> = [
        0, 1, 639, 640, 4095, 4096, 8191, 8192, 16383, 16384, 23999, 24000, 31998, 31999,
    ]
    .into_iter()
    .filter(|&y| y < snapshot.height)
    .collect();
    for segment in &snapshot.segments {
        for y in [
            segment.origin.1,
            segment
                .origin
                .1
                .saturating_add(segment.source.height)
                .saturating_sub(1),
        ] {
            if y < snapshot.height {
                rows.push(y);
            }
        }
    }
    rows.sort_unstable();
    rows.dedup();
    let mut checked = 0;
    for y in rows {
        for x in [0, 31, WIDTH / 2, WIDTH - 1] {
            let rect = Rect {
                x,
                y,
                width: 1,
                height: 1,
            };
            let actual = long_document::render_region(snapshot, rect, 1, 1)?;
            let expected = crate::long_capture_smoke::scene_for_pattern(
                FixturePattern::Noise,
                1,
                1,
                ScrollAxis::Vertical,
                (0, 0),
                (ORIGIN + x as i32, ORIGIN + y as i32),
                0,
            );
            ensure(
                actual.bgra == expected.bgra,
                &format!(
                    "真实 DXGI 拼接像素不一致：{x},{y} actual={:?} expected={:?}",
                    actual.bgra, expected.bgra
                ),
            )?;
            checked += 1;
        }
    }
    Ok(checked)
}
struct Report {
    root: PathBuf,
    requested: Vec<String>,
    stages: Vec<serde_json::Value>,
    groups: Vec<serde_json::Value>,
}
impl Report {
    fn save(&self, complete: bool, error: Option<&str>) -> Result<()> {
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "suite":"long-dxgi-memory","pid":std::process::id(),"ok":complete&&error.is_none(),"error":error,
                "real_dxgi_capture":true,"source":"owned-opaque-topmost-window-ROI","requested_groups":self.requested,
                "width":WIDTH,"viewport_height":VIEW_HEIGHT,"target_height":HEIGHT,
                "groups":self.groups,"stages":self.stages,"clipboard_untouched":true,
                "global_input_sent":false,"full_desktop_freeze_used":false,"qa_poll_ms":100,
                "limitations":["DXGI取得输出帧资源，CPU只读自有ROI；归属检查到GPU拷贝之间仍有竞态",
                    "owned texture像素不包含DXGI duplication/system surface/驱动缓存，另测进程GPU用量",
                    "合成滚动窗口实际DXGI与生产LongSession，不证明第三方网页、混合DPI或16ms吞吐",
                    "后端暂停/几何编辑移交验证不替代GPUI按钮/窗口全工作流",
                    "三轮有限内存测量不能证明长期无泄漏"],
            }))?,
        )?;
        Ok(())
    }
    fn record(&mut self, name: &str, detail: serde_json::Value) -> Result<()> {
        self.stages.push(serde_json::json!({"stage":name,"pid":std::process::id(),"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail}));
        self.save(false, None)
    }
}
fn capture_metrics(meta: &LongSnapshot) -> serde_json::Value {
    serde_json::json!({
        "dimensions":[meta.width,meta.height],"generation":meta.generation,
        "requested_generation":meta.requested_generation,"preview_revision":meta.preview_revision,
        "accepted_frames":meta.accepted_frames,"poll_count":meta.poll_count,
        "owned_texture_pixels":meta.capture_storage_pixels,"full_output_pixels":meta.capture_output_pixels,
        "capture_screen_count":meta.capture_screen_count,"worker_alive":meta.worker_alive,
        "capture_in_flight":meta.capture_in_flight,
    })
}
fn growth(report: &mut Report, region: ScreenRect) -> Result<()> {
    for round in 1..=3 {
        let mut fixture =
            Fixture::new_owned(region, ScrollAxis::Vertical, (0, 0), FixturePattern::Noise)?;
        fixture.set_limits(ORIGIN, ORIGIN + (HEIGHT - VIEW_HEIGHT) as i32)?;
        report.record("round-fixture-ready", serde_json::json!({"round":round}))?;
        let mut session = LongSession::start_guarded(region, options(), fixture.capture_gate()?)?;
        let mut meta = wait(&session, "实际初始 ROI", |m| {
            m.height == VIEW_HEIGHT && m.capture_storage_pixels > 0
        })?;
        ensure(
            meta.capture_screen_count == 1
                && meta.capture_storage_pixels == u64::from(WIDTH) * u64::from(VIEW_HEIGHT),
            "持久 Snapshot 纹理没有裁到实际选区",
        )?;
        ensure(
            meta.capture_output_pixels >= meta.capture_storage_pixels,
            "全输出物理元数据丢失",
        )?;
        report.record(
            "initial-real-roi",
            serde_json::json!({"round":round,"capture":capture_metrics(&meta)}),
        )?;
        let mut previous_revision = meta.preview_revision;
        let mut next_milestone = 8_000;
        for step in 1..=123 {
            let delta = if step == 123 { 128 } else { 256 };
            fixture.wheel(ScrollAxis::Vertical, if delta == 128 { 4 } else { 8 })?;
            let expected = VIEW_HEIGHT
                + if step == 123 {
                    HEIGHT - VIEW_HEIGHT
                } else {
                    step * 256
                };
            meta = wait(&session, "真实滚动逐帧确认", |m| {
                m.height == expected
            })?;
            ensure(
                meta.width == WIDTH
                    && fixture.origin() == (ORIGIN, ORIGIN + (expected - VIEW_HEIGHT) as i32),
                "滚动原点/成品高度不符",
            )?;
            if meta.height >= next_milestone {
                let snapshot = session
                    .smoke_committed_snapshot()
                    .ok_or("当前代没有已提交条带")?;
                let checked = oracle(&snapshot)?;
                let (_, bytes) = source_refs(&snapshot);
                drop(snapshot);
                let preview = session.preview_frame().ok_or("真实增长缺少预览")?;
                ensure(
                    preview.height <= meta.height && preview.revision > previous_revision,
                    "右侧预览 revision 没有随真实滚动增长",
                )?;
                previous_revision = preview.revision;
                report.record("real-growth-milestone",serde_json::json!({"round":round,"milestone":next_milestone,"capture":capture_metrics(&meta),"pixel_oracle_checks":checked,"unique_committed_bgra_bytes":bytes,"preview_dimensions":[preview.image.width,preview.image.height],"preview_source_height":preview.height}))?;
                drop(preview);
                next_milestone += 8_000;
            }
        }
        let (paused, snapshot) = pause(&mut session)?;
        ensure(
            snapshot.width == WIDTH && snapshot.height == HEIGHT,
            "最终真实成品不是960×32000",
        )?;
        let checked = oracle(&snapshot)?;
        let (refs, bytes) = source_refs(&snapshot);
        report.record("paused-real-32000",serde_json::json!({"round":round,"capture":capture_metrics(&paused),"unique_committed_bgra_bytes":bytes,"oracle_checks":checked,"gate":fixture.capture_probe_snapshot()}))?;
        let mut document = LongDocument::new(snapshot)?;
        ensure(
            document.crop(Rect {
                x: 8,
                y: 16,
                width: WIDTH - 16,
                height: HEIGHT - 32,
            })?,
            "几何编辑裁剪未提交",
        )?;
        ensure(document.cut(4096, 4352)?, "几何编辑删段未提交")?;
        ensure(
            document.undo() && document.redo() && document.undo() && document.undo(),
            "几何编辑撤销/重做合同错误",
        )?;
        let restored = document.snapshot();
        ensure(
            restored.width == WIDTH && restored.height == HEIGHT,
            "编辑 Undo 未恢复长图",
        )?;
        ensure(oracle(&restored)? == checked, "编辑恢复后抽样数变化")?;
        drop(restored);
        drop(document);
        report.record("shared-geometry-editor-return",serde_json::json!({"round":round,"raw_snapshot_shared":true,"capture_resources_paused_released":true}))?;
        // 恢复后以同一末端视口懒重建，不能重复追加或清空已有条带。
        let polls = paused.poll_count;
        session.set_paused(false)?;
        let resumed = wait(&session, "暂停后的实际 DXGI 重建", |m| {
            m.poll_count > polls && m.capture_storage_pixels > 0 && m.height == HEIGHT
        })?;
        let (_, returned) = pause(&mut session)?;
        ensure(
            returned.height == HEIGHT && oracle(&returned)? == checked,
            "暂停恢复重复追加或丢失像素",
        )?;
        drop(returned);
        report.record(
            "resume-rebuilt-real-roi",
            serde_json::json!({"round":round,"capture":capture_metrics(&resumed)}),
        )?;
        finish(&mut session)?;
        drop(session);
        ensure(
            refs.iter().all(|reference| reference.upgrade().is_none()),
            "实际长采集关闭后条带原始像素未释放",
        )?;
        fixture.close()?;
        report.record("round-closed-real-resources",serde_json::json!({"round":round,"capture_workers":long_capture_runtime::active_workers(),"all_source_weaks_expired":true,"fixture_thread_and_hwnd_destroyed":true,"forced_cleanup":false}))?;
    }
    report.groups.push(serde_json::json!({"name":"real-roi-32000-three-rounds-growth-pause-resume-close","ok":true,"rounds":3,"scroll_steps_per_round":123,"content_pixels":u64::from(WIDTH)*u64::from(HEIGHT)}));
    report.save(false, None)
}
struct Latch {
    armed: AtomicBool,
    entered: AtomicBool,
    release: AtomicBool,
}
impl Latch {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        }
    }
}
fn cancel(report: &mut Report, region: ScreenRect) -> Result<()> {
    for mode in [
        "in-flight-stop",
        "paused-close",
        "drop-reaper-close",
        "owned-fixture-destroy",
    ] {
        let mut fixture =
            Fixture::new_owned(region, ScrollAxis::Vertical, (0, 0), FixturePattern::Noise)?;
        let owned = fixture.capture_gate()?;
        let latch = Arc::new(Latch::new());
        let worker_latch = latch.clone();
        let gate: FrameGate = Arc::new(move || {
            owned()?;
            if worker_latch.armed.load(Ordering::Acquire) {
                worker_latch.entered.store(true, Ordering::Release);
                let end = Instant::now() + WAIT;
                while !worker_latch.release.load(Ordering::Acquire) {
                    ensure(Instant::now() < end, "自有采集取消栅栏超时")?;
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(())
        });
        let mut session = LongSession::start_guarded(region, options(), gate)?;
        wait(&session, "取消前实际初始帧", |m| {
            m.height == VIEW_HEIGHT
        })?;
        let snapshot = session
            .smoke_committed_snapshot()
            .ok_or("取消前初帧未提交")?;
        let (refs, _) = source_refs(&snapshot);
        let checked = oracle(&snapshot)?;
        drop(snapshot);
        let probe = session.worker_probe();
        let mut in_flight = false;
        let result = (|| -> Result<()> {
            if mode == "in-flight-stop" {
                latch.armed.store(true, Ordering::Release);
                let end = Instant::now() + WAIT;
                while !latch.entered.load(Ordering::Acquire) {
                    ensure(Instant::now() < end, "没有到达实际read_frame归属检查点")?;
                    thread::sleep(Duration::from_millis(1));
                }
                in_flight = session.snapshot().capture_in_flight;
                ensure(in_flight, "栅栏不是实际采集在途")?;
                session.stop();
                latch.release.store(true, Ordering::Release);
                finish(&mut session)?;
            } else if mode == "paused-close" {
                let (_, snapshot) = pause(&mut session)?;
                ensure(
                    oracle(&snapshot)? == checked,
                    "暂停关闭前最后成功结果不一致",
                )?;
                drop(snapshot);
                finish(&mut session)?;
            } else if mode == "owned-fixture-destroy" {
                let (_, snapshot) = pause(&mut session)?;
                drop(snapshot);
                fixture.close()?;
                session.set_paused(false)?;
                let end = Instant::now() + WAIT;
                loop {
                    let meta = session.snapshot();
                    if meta.phase == LongPhase::Failed {
                        ensure(
                            settled(&meta)
                                && meta.height == VIEW_HEIGHT
                                && meta.accepted_frames == 1
                                && meta.capture_storage_pixels == 0,
                            "窗口销毁后仍提交帧或保留GPU采集资源",
                        )?;
                        ensure(
                            oracle(
                                &session
                                    .stitch_snapshot()
                                    .ok_or("归属失败丢失最后成功结果")?,
                            )? == checked,
                            "归属失败污染已有像素",
                        )?;
                        break;
                    }
                    ensure(Instant::now() < end, "自有窗口销毁后归属门未拒绝")?;
                    thread::sleep(Duration::from_millis(3));
                }
                finish(&mut session)?;
            }
            Ok(())
        })();
        // 任何失败都先解开仅本次 callback 的栅栏，再由普通 Drop 收尾。
        latch.release.store(true, Ordering::Release);
        drop(session);
        result?;
        let end = Instant::now() + WAIT;
        while !probe.load(Ordering::Acquire)
            || long_capture_runtime::active_workers() != 0
            || refs.iter().any(|reference| reference.upgrade().is_some())
        {
            ensure(
                Instant::now() < end,
                "Drop收尾线程没有实际结束capture worker",
            )?;
            thread::sleep(Duration::from_millis(3));
        }
        ensure(
            long_capture_runtime::panicked_workers() == 0,
            "Drop收尾出现采集线程panic",
        )?;
        ensure(
            refs.iter().all(|reference| reference.upgrade().is_none()),
            "取消后原始帧未释放",
        )?;
        fixture.close()?;
        report.record("real-cancel-closed",serde_json::json!({"mode":mode,"capture_in_flight_observed":in_flight,"actual_worker_probe_exited":true,"capture_workers":0,"panicked_workers":long_capture_runtime::panicked_workers(),"capture_join_directly_observed":mode!="drop-reaper-close","source_weaks_expired":true,"fixture_closed":true,"gate":fixture.capture_probe_snapshot(),"forced_cleanup":false,"in_flight_scope":"owned validation checkpoint within production read_frame; not simulated driver hang"}))?;
    }
    report.groups.push(serde_json::json!({"name":"real-roi-cancel-stop-pause-drop-worker-release","ok":true,"cases":4,"fixture_destroy_failure_retains_last_result":true,"foreign_occlusion_failure_tested":false}));
    report.save(false, None)
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_LONG_DXGI_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/long-dxgi-memory"));
    std::fs::create_dir_all(&root)?;
    let requested: Vec<String> = std::env::var("RUSTSHOT_LONG_DXGI_GROUPS")
        .unwrap_or_else(|_| "growth,cancel".into())
        .split(',')
        .map(|value| value.trim().to_string())
        .collect();
    ensure(
        !requested.is_empty()
            && requested
                .iter()
                .all(|group| matches!(group.as_str(), "growth" | "cancel"))
            && requested.iter().collect::<HashSet<_>>().len() == requested.len(),
        "真实DXGI请求组无效",
    )?;
    let mut report = Report {
        root,
        requested,
        stages: vec![],
        groups: vec![],
    };
    let result = (|| -> Result<()> {
        let work = crate::long_capture_smoke::work_area()?;
        let region = ScreenRect {
            left: work.left + 32,
            top: work.top + 32,
            width: WIDTH,
            height: VIEW_HEIGHT,
        };
        report.record(
            "before-real-capture",
            serde_json::json!({"capture_workers":long_capture_runtime::active_workers()}),
        )?;
        if report.requested.iter().any(|group| group == "growth") {
            growth(&mut report, region)?;
        }
        if report.requested.iter().any(|group| group == "cancel") {
            cancel(&mut report, region)?;
        }
        ensure(
            long_capture_runtime::active_workers() == 0,
            "验收完成后采集worker仍存活",
        )?;
        Ok(())
    })();
    let error = result.as_ref().err().map(ToString::to_string);
    report.save(true, error.as_deref())?;
    result
}
