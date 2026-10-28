//! 半格自动滚动专项：仅向自有 HWND 投递消息，采集受归属门保护的真实 DXGI ROI。
use crate::{
    capture::Image,
    geometry::ScreenRect,
    long_auto::{AutoDirection, AutoPhase, AutoScrollOptions},
    long_capture_pipeline,
    long_capture_runtime::{self, LongPhase, LongSession, LongSnapshot},
    long_capture_smoke::{self as fixture, Fixture, FixturePattern, WheelEvent},
    stitch::{ScrollAxis, StitchSnapshot},
    Result,
};
use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        System::Threading::CreateMutexW,
        UI::{HiDpi::*, WindowsAndMessaging::IsWindow},
    },
};

const WAIT: Duration = Duration::from_secs(20);
const QUIET_CHECK: Duration = Duration::from_millis(700);
const MIN: i32 = 128;
const MAX: i32 = 320;
const BANDS: (u32, u32) = (24, 20);
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const DEFAULT_GROUPS: [&str; 4] = ["bounds", "speed", "no-motion", "target"];
const GROUPS: [&str; 5] = ["bounds", "speed", "no-motion", "target", "capture-gate"];

#[path = "long_auto_gate_smoke.rs"]
mod gate_smoke;

fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
fn group_selection() -> Result<Vec<String>> {
    let Some(raw) = std::env::var_os("RUSTSHOT_LONG_AUTO_GROUPS") else {
        return Ok(DEFAULT_GROUPS.iter().map(|s| (*s).to_owned()).collect());
    };
    let raw = raw.to_string_lossy();
    let mut selected = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        ensure(
            GROUPS.contains(&name),
            &format!("未知自动滚动专项组：{name}"),
        )?;
        ensure(!selected.iter().any(|s| s == name), "自动滚动专项组重复")?;
        selected.push(name.to_owned());
    }
    ensure(!selected.is_empty(), "自动滚动专项组不能为空")?;
    Ok(selected)
}
struct Qa {
    root: PathBuf,
    file: File,
    started: Instant,
    groups: Vec<serde_json::Value>,
    owned: Vec<usize>,
    capture_evidence: Vec<serde_json::Value>,
}
impl Qa {
    fn new() -> Result<Self> {
        let root = std::env::var_os("RUSTSHOT_LONG_AUTO_QA_DIR")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?.join(format!(
                "target/qa/long-auto-pacer/session-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
            )));
        std::fs::create_dir_all(root.join("settings"))?;
        std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
        std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
        let file = File::create(root.join("ui.jsonl"))?;
        Ok(Self {
            root,
            file,
            started: Instant::now(),
            groups: Vec::new(),
            owned: Vec::new(),
            capture_evidence: Vec::new(),
        })
    }
    fn fixture(&mut self, axis: ScrollAxis, bands: (u32, u32)) -> Result<Fixture> {
        let region = ScreenRect {
            width: WIDTH,
            height: HEIGHT,
            ..fixture::source_region(fixture::work_area()?)
        };
        let target = Fixture::new_owned(region, axis, bands, FixturePattern::Noise)?;
        self.owned.push(target.native());
        Ok(target)
    }
    fn observe_capture(&mut self, target: &Fixture, session: &LongSession) -> Result<()> {
        let ownership = target.capture_probe_snapshot();
        let pipeline = session.pipeline_probe();
        ensure(!pipeline.reads.is_empty(), "专项未发生真实抓屏读回")?;
        ensure(
            ownership["checks"].as_u64().unwrap_or(0) > 0
                && ownership["desktop_getpixel_probe"].as_bool() == Some(false),
            "专项未使用自有 ROI 归属门，或触发桌面 GetPixel",
        )?;
        self.capture_evidence.push(serde_json::json!({
            "hwnd":target.native(),"region":{
                "left":target.region.left,"top":target.region.top,
                "width":target.region.width,"height":target.region.height},
            "ownership":ownership,"pipeline":pipeline,
        }));
        Ok(())
    }
    fn record(&mut self, name: &str, detail: serde_json::Value) -> Result<()> {
        let row = serde_json::json!({"name":name,"ok":true,"detail":detail});
        let event = serde_json::json!({"suite":"long-auto-pacer","pid":std::process::id(),
            "elapsed_ms":self.started.elapsed().as_secs_f64()*1000.,"group":row});
        println!("{event}");
        writeln!(self.file, "{event}")?;
        self.file.flush()?;
        self.groups.push(row);
        Ok(())
    }
    fn finish(&mut self, requested: &[String], error: Option<&str>) -> Result<()> {
        let captures = long_capture_pipeline::active_capture_threads();
        let consumers = long_capture_runtime::active_workers();
        let windows_destroyed = self
            .owned
            .iter()
            .all(|h| !unsafe { IsWindow(Some(HWND(*h as *mut _))) }.as_bool());
        let real_dxgi = !self.capture_evidence.is_empty()
            && self.capture_evidence.iter().all(|e| {
                e["pipeline"]["reads"]
                    .as_array()
                    .is_some_and(|r| !r.is_empty())
                    && e["ownership"]["desktop_getpixel_probe"].as_bool() == Some(false)
            });
        let mut errors: Vec<String> = error.into_iter().map(str::to_owned).collect();
        if captures != 0 || consumers != 0 || !windows_destroyed {
            errors.push(format!(
                "资源未退出：producer={captures},consumer={consumers},owned_windows_destroyed={windows_destroyed}"
            ));
        }
        let report = serde_json::json!({
            "suite":"long-auto-pacer","pid":std::process::id(),
            "ok":errors.is_empty() && self.groups.len()==requested.len() && real_dxgi,
            "requested_groups":requested,"groups":self.groups,"errors":errors,
            "elapsed_ms":self.started.elapsed().as_secs_f64()*1000.,
            "real_dxgi_capture":real_dxgi,"full_desktop_freeze_used":false,
            "clipboard_untouched":true,"global_input_sent":false,
            "capture_threads_after_close":captures,"consumer_sessions_after_close":consumers,
            "owned_windows_destroyed":windows_destroyed,
            "panicked_capture_or_consumer_threads":long_capture_runtime::panicked_workers(),
            "ownership":self.capture_evidence,
        });
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        writeln!(self.file, "{report}")?;
        self.file.flush()?;
        println!("{report}");
        Ok(())
    }
}

struct Session {
    session: LongSession,
    probe: Arc<std::sync::atomic::AtomicBool>,
    closed: bool,
}
impl Session {
    fn start(target: &Fixture, axis: ScrollAxis, bands: (u32, u32)) -> Result<Self> {
        let mut options = fixture::options(axis, bands);
        options.poll_interval = Duration::from_millis(16);
        let session = LongSession::start_guarded(target.region, options, target.capture_gate()?)?;
        let probe = session.worker_probe();
        let result = Self {
            session,
            probe,
            closed: false,
        };
        wait(&result.session, "自有 DXGI 初帧 ACK", |m| {
            m.phase == LongPhase::Running && acknowledged(m) && m.result_available
        })?;
        Ok(result)
    }
    fn close(&mut self) -> Result<serde_json::Value> {
        self.session.stop();
        let deadline = Instant::now() + WAIT;
        loop {
            let p = self.session.pipeline_probe();
            if self.probe.load(Ordering::Acquire)
                && !p.producer_alive
                && !p.consumer_alive
                && p.producer_quiesced
                && p.reserved_raw_bytes == 0
                && p.queue_depth == 0
                && self.session.try_join_stopped()
            {
                ensure(
                    long_capture_runtime::panicked_workers() == 0,
                    "采集线程 panic",
                )?;
                self.closed = true;
                return Ok(
                    serde_json::json!({"worker_exited":self.session.snapshot().worker_exited,
                    "producer_alive":p.producer_alive,"consumer_alive":p.consumer_alive,
                    "producer_quiesced":p.producer_quiesced,"queue_depth":p.queue_depth,
                    "reserved_raw_bytes":p.reserved_raw_bytes,"joined":true}),
                );
            }
            if Instant::now() >= deadline {
                return Err(
                    format!("采集线程没有正常退出：{:?}", self.session.pipeline_probe()).into(),
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}
fn acknowledged(m: &LongSnapshot) -> bool {
    m.generation == m.requested_generation
}
fn wait(
    session: &LongSession,
    label: &str,
    predicate: impl Fn(&LongSnapshot) -> bool,
) -> Result<LongSnapshot> {
    let deadline = Instant::now() + WAIT;
    loop {
        let m = session.snapshot();
        if predicate(&m) {
            return Ok(m);
        }
        if m.phase == LongPhase::Failed || m.worker_exited || Instant::now() >= deadline {
            return Err(format!(
                "{label} 未完成：{m:?}；pipeline={:?}",
                session.pipeline_probe()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn freeze(session: &mut LongSession) -> Result<StitchSnapshot> {
    session.drain_for_output()?;
    wait(session, "输出尾帧屏障", |m| {
        let p = session.pipeline_probe();
        acknowledged(m)
            && m.phase == LongPhase::Paused
            && !m.busy
            && p.producer_quiesced
            && p.queue_depth == 0
            && p.reserved_raw_bytes == 0
            && !p.consumer_in_flight
    })?;
    session
        .stitch_snapshot()
        .ok_or_else(|| "真实暂停 ACK 后没有生产导出快照".into())
}
fn committed(session: &LongSession) -> Result<StitchSnapshot> {
    session
        .smoke_committed_snapshot()
        .ok_or_else(|| "当前代没有已提交像素".into())
}
fn same_segments(a: &StitchSnapshot, b: &StitchSnapshot) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.axis == b.axis
        && a.viewport == b.viewport
        && a.segments.len() == b.segments.len()
        && a.segments.iter().zip(&b.segments).all(|(x, y)| {
            Arc::ptr_eq(&x.image, &y.image) && x.source == y.source && x.origin == y.origin
        })
}
fn expect_image(
    snapshot: &StitchSnapshot,
    axis: ScrollAxis,
    bands: (u32, u32),
    world_origin: (i32, i32),
    extension: u32,
    label: &str,
) -> Result<Image> {
    let (width, height) = match axis {
        ScrollAxis::Vertical => (WIDTH, HEIGHT + extension),
        ScrollAxis::Horizontal => (WIDTH + extension, HEIGHT),
    };
    let expected = fixture::scene_for_pattern(
        FixturePattern::Noise,
        width,
        height,
        axis,
        bands,
        world_origin,
        0,
    );
    let actual = fixture::stitched_image(snapshot)?;
    fixture::same_image(&actual, &expected, label)?;
    Ok(actual)
}
fn start_auto(session: &mut LongSession, direction: AutoDirection) -> Result<()> {
    session.set_auto_scroll(Some(AutoScrollOptions { direction }))
}
fn observe_no_more(target: &Fixture) -> Result<u64> {
    let before = target.wheel_count();
    let deadline = Instant::now() + QUIET_CHECK;
    while Instant::now() < deadline {
        ensure(
            target.wheel_count() == before,
            "AutoStop 后继续向自有目标投递滚轮",
        )?;
        thread::sleep(Duration::from_millis(5));
    }
    Ok(before)
}
fn signed_delta(axis: ScrollAxis, direction: AutoDirection) -> i16 {
    match (axis, direction) {
        (ScrollAxis::Vertical, AutoDirection::Forward)
        | (ScrollAxis::Horizontal, AutoDirection::Reverse) => -60,
        _ => 60,
    }
}
fn verify_halves(
    events: &[WheelEvent],
    axis: ScrollAxis,
    direction: AutoDirection,
    initial: (i32, i32),
    limits: (i32, i32),
) -> Result<()> {
    let delta = signed_delta(axis, direction);
    let mut position = initial;
    for (index, event) in events.iter().enumerate() {
        ensure(
            event.delta == delta,
            &format!("第{}次不是 signed60：{event:?}", index + 1),
        )?;
        let notch = if index % 2 == 0 {
            0
        } else {
            i32::from(delta / 60)
        };
        ensure(
            event.completed_notches == notch,
            &format!("第{}次半格累计错误：{event:?}", index + 1),
        )?;
        match axis {
            ScrollAxis::Vertical => {
                position.1 = (position.1 - fixture::STEP * notch).clamp(limits.0, limits.1)
            }
            ScrollAxis::Horizontal => {
                position.0 = (position.0 + fixture::STEP * notch).clamp(limits.0, limits.1)
            }
        }
        ensure(
            (event.x, event.y) == position,
            &format!(
                "第{}次实际坐标未遵守半格累计：{event:?}，预期{position:?}",
                index + 1
            ),
        )?;
    }
    Ok(())
}
fn timing(events: &[WheelEvent], interval_ms: f64) -> Result<serde_json::Value> {
    ensure(events.len() >= 2, "实际滚轮收件不足两个，无法量测间隔")?;
    let mut values: Vec<f64> = events
        .windows(2)
        .map(|pair| pair[1].elapsed_ms - pair[0].elapsed_ms)
        .collect();
    ensure(
        values
            .iter()
            .all(|n| n.is_finite() && *n >= interval_ms - 3.),
        &format!(
            "实际收件间隔短于速度下限{}ms（3ms采样容差）：{values:?}",
            interval_ms
        ),
    )?;
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.sort_by(f64::total_cmp);
    let median = values[values.len() / 2];
    Ok(
        serde_json::json!({"count":events.len(),"target_minimum_ms":interval_ms,
        "measurement_tolerance_ms":3.,"minimum_ms":values[0],"median_ms":median,
        "mean_ms":mean,"maximum_ms":values[values.len()-1],
        "exact_cadence_claimed":false}),
    )
}

fn bounds(qa: &mut Qa) -> Result<serde_json::Value> {
    let mut cases = Vec::new();
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for direction in [AutoDirection::Forward, AutoDirection::Reverse] {
            let initial = if direction == AutoDirection::Forward {
                MIN
            } else {
                MAX
            };
            let mut target = qa.fixture(axis, BANDS)?;
            target.set_limits(initial, initial)?;
            target.set_limits(MIN, MAX)?;
            let mut guard = Session::start(&target, axis, BANDS)?;
            expect_image(
                &committed(&guard.session)?,
                axis,
                BANDS,
                (initial, initial),
                0,
                "边界初帧 CPU oracle",
            )?;
            start_auto(&mut guard.session, direction)?;
            let stopped = wait(&guard.session, "边界 AutoStop", |m| {
                acknowledged(m)
                    && !m.auto.active
                    && m.auto.phase == AutoPhase::NoMotion
                    && m.phase == LongPhase::Running
            })?;
            let count = observe_no_more(&target)?;
            let events = target.wheel_events();
            ensure(
                count == 14 && events.len() == 14 && stopped.auto.steps == 14,
                &format!(
                    "六整格+边界两半格应为14次收件：count={count},steps={},events={events:?}",
                    stopped.auto.steps
                ),
            )?;
            verify_halves(&events, axis, direction, (initial, initial), (MIN, MAX))?;
            timing(&events, 10.)?;
            let endpoint = if direction == AutoDirection::Forward {
                MAX
            } else {
                MIN
            };
            let final_position = match axis {
                ScrollAxis::Vertical => (initial, endpoint),
                ScrollAxis::Horizontal => (endpoint, initial),
            };
            ensure(
                target.origin() == final_position,
                "实际目标没有到达滚动边界",
            )?;
            ensure(
                (events[11].x, events[11].y) == final_position
                    && (events[12].x, events[12].y) == final_position
                    && (events[13].x, events[13].y) == final_position,
                "末端两半格不是在已到达的边界上停止",
            )?;
            let snapshot = freeze(&mut guard.session)?;
            let world = match axis {
                ScrollAxis::Vertical => (initial, MIN),
                ScrollAxis::Horizontal => (MIN, initial),
            };
            expect_image(
                &snapshot,
                axis,
                BANDS,
                world,
                (MAX - MIN) as u32,
                "双向边界完整像素 oracle",
            )?;
            qa.observe_capture(&target, &guard.session)?;
            let closed = guard.close()?;
            target.close()?;
            cases.push(serde_json::json!({"axis":format!("{axis:?}"),"direction":format!("{direction:?}"),
                "wheel_count":count,"events":events,"auto_steps":stopped.auto.steps,
                "auto_stop_phase":format!("{:?}",stopped.auto.phase),"capture_phase_after_auto_stop":format!("{:?}",stopped.phase),
                "axis_endpoint":endpoint,"final_origin":final_position,
                "output_size":[snapshot.width,snapshot.height],"all_output_pixels_match":true,"close":closed}));
        }
    }
    Ok(
        serde_json::json!({"cases":cases,"fixed_bands":BANDS,"quiet_observed_ms":QUIET_CHECK.as_millis()}),
    )
}

fn speed(qa: &mut Qa) -> Result<serde_json::Value> {
    const SAMPLES: usize = 16;
    let mut target = qa.fixture(ScrollAxis::Vertical, (0, 0))?;
    let limits = (fixture::ORIGIN, fixture::ORIGIN + fixture::STEP * 32);
    target.set_limits(limits.0, limits.1)?;
    let mut guard = Session::start(&target, ScrollAxis::Vertical, (0, 0))?;
    expect_image(
        &committed(&guard.session)?,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        0,
        "自适应速度初帧 CPU oracle",
    )?;
    start_auto(&mut guard.session, AutoDirection::Forward)?;
    let active = wait(&guard.session, "自适应半格收件样本", |m| {
        m.phase == LongPhase::Running && m.auto.active && target.wheel_events().len() >= SAMPLES
    })?;
    ensure(
        active.auto.speed > 1. && active.auto.speed <= 10.,
        "连续可靠匹配后自适应滚动没有提速",
    )?;
    guard.session.set_auto_scroll(None)?;
    let stopped = wait(&guard.session, "显式停止自动仍可采集", |m| {
        acknowledged(m) && !m.auto.active && m.phase == LongPhase::Running
    })?;
    let count = observe_no_more(&target)?;
    let events = target.wheel_events();
    ensure(
        count == events.len() as u64 && events.len() >= SAMPLES && events.len() < 32,
        "停止自动后真实消息计数/样本上限不符",
    )?;
    verify_halves(
        &events,
        ScrollAxis::Vertical,
        AutoDirection::Forward,
        (fixture::ORIGIN, fixture::ORIGIN),
        limits,
    )?;
    let metric = timing(&events[..SAMPLES], 10.)?;
    let origin = target.origin();
    let extension = (origin.1 - fixture::ORIGIN) as u32;
    wait(
        &guard.session,
        "停止自动后最后实际位置仍可匹配",
        |m| {
            m.phase == LongPhase::Running
                && !m.auto.active
                && m.height == HEIGHT + extension
                && acknowledged(m)
        },
    )?;
    let snapshot = freeze(&mut guard.session)?;
    expect_image(
        &snapshot,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        extension,
        "自适应累计最终像素 oracle",
    )?;
    qa.observe_capture(&target, &guard.session)?;
    let closed = guard.close()?;
    target.close()?;
    Ok(
        serde_json::json!({"mode":"adaptive","observed_speed":active.auto.speed,"sample_count":SAMPLES,"receipt_timing":metric,
        "wheel_count":count,"events":events,"stop_generation":stopped.generation,
        "phase_after_stop":format!("{:?}",stopped.phase),"final_origin":origin,
        "output_size":[snapshot.width,snapshot.height],"all_output_pixels_match":true,"close":closed,
        "poll_interval_ms":16,"all_messages_are_half_notches":true}),
    )
}

fn no_motion(qa: &mut Qa) -> Result<serde_json::Value> {
    let mut target = qa.fixture(ScrollAxis::Vertical, (0, 0))?;
    target.set_limits(fixture::ORIGIN, fixture::ORIGIN)?;
    let mut guard = Session::start(&target, ScrollAxis::Vertical, (0, 0))?;
    let baseline = committed(&guard.session)?;
    expect_image(
        &baseline,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        0,
        "无运动初帧 CPU oracle",
    )?;
    start_auto(&mut guard.session, AutoDirection::Forward)?;
    let stopped = wait(&guard.session, "两半格无响应停止自动", |m| {
        acknowledged(m)
            && !m.auto.active
            && m.auto.phase == AutoPhase::NoMotion
            && m.phase == LongPhase::Running
    })?;
    let count = observe_no_more(&target)?;
    let stopped_events = target.wheel_events();
    ensure(
        count == 2 && stopped_events.len() == 2 && stopped.auto.steps == 2,
        &format!(
            "无响应只应预发120：实际{count}，steps={}",
            stopped.auto.steps
        ),
    )?;
    verify_halves(
        &stopped_events,
        ScrollAxis::Vertical,
        AutoDirection::Forward,
        (fixture::ORIGIN, fixture::ORIGIN),
        (fixture::ORIGIN, fixture::ORIGIN),
    )?;
    timing(&stopped_events, 100.)?;
    let unchanged = committed(&guard.session)?;
    ensure(
        same_segments(&baseline, &unchanged),
        "无运动自动停止改变了可信条带 Arc",
    )?;
    expect_image(
        &unchanged,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        0,
        "无运动停止后像素 oracle",
    )?;
    // 实际投递整格手动消息，证明 AutoStop 后生产采集继续运行，而非只检查 phase 字段。
    target.set_limits(fixture::ORIGIN, fixture::ORIGIN + fixture::STEP)?;
    target.wheel(ScrollAxis::Vertical, 1)?;
    let manual = wait(
        &guard.session,
        "AutoStop 后手动真实滚动匹配",
        |m| {
            acknowledged(m)
                && m.phase == LongPhase::Running
                && !m.auto.active
                && m.height == HEIGHT + fixture::STEP as u32
        },
    )?;
    ensure(
        target.origin() == (fixture::ORIGIN, fixture::ORIGIN + fixture::STEP),
        "手动整格没有实际移动目标",
    )?;
    ensure(target.wheel_count() == 3, "手动继续时自动策略被复活")?;
    let snapshot = freeze(&mut guard.session)?;
    expect_image(
        &snapshot,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        fixture::STEP as u32,
        "无运动停止后手动追加完整像素 oracle",
    )?;
    qa.observe_capture(&target, &guard.session)?;
    let events = target.wheel_events();
    ensure(
        events.len() == 3 && events[2].delta == -120 && events[2].completed_notches == -1,
        "手动继续没有收到真实整格消息",
    )?;
    let closed = guard.close()?;
    target.close()?;
    Ok(
        serde_json::json!({"auto_steps":stopped.auto.steps,"auto_stop_phase":format!("{:?}",stopped.auto.phase),
        "capture_phase_after_auto_stop":format!("{:?}",stopped.phase),"auto_events":stopped_events,
        "manual_events":events,"manual_phase":format!("{:?}",manual.phase),
        "manual_append_pixels":fixture::STEP,"output_size":[snapshot.width,snapshot.height],
        "all_output_pixels_match":true,"baseline_same_arc_after_auto_stop":true,
        "quiet_observed_ms":QUIET_CHECK.as_millis(),"close":closed}),
    )
}

fn target_closed(qa: &mut Qa) -> Result<serde_json::Value> {
    let mut target = qa.fixture(ScrollAxis::Vertical, (0, 0))?;
    let mut guard = Session::start(&target, ScrollAxis::Vertical, (0, 0))?;
    let baseline = committed(&guard.session)?;
    let initial = fixture::stitched_image(&baseline)?;
    expect_image(
        &baseline,
        ScrollAxis::Vertical,
        (0, 0),
        (fixture::ORIGIN, fixture::ORIGIN),
        0,
        "目标关闭初帧 CPU oracle",
    )?;
    // 第一半格不移动；自适应从慢开始，关闭时旧基线可精确比较。
    start_auto(&mut guard.session, AutoDirection::Forward)?;
    wait(&guard.session, "关闭前真实第一半格收件", |m| {
        m.phase == LongPhase::Running && m.auto.active && target.wheel_count() >= 1
    })?;
    let events = target.wheel_events();
    ensure(
        events.len() == 1 && events[0].delta == -60 && events[0].completed_notches == 0,
        "目标关闭夹具没有停留在第一个不移动的真实半格",
    )?;
    qa.observe_capture(&target, &guard.session)?;
    target.close()?;
    ensure(
        !unsafe { IsWindow(Some(HWND(target.native() as *mut _))) }.as_bool(),
        "实际目标 HWND 尚未销毁",
    )?;
    let deadline = Instant::now() + WAIT;
    let terminal = loop {
        let m = guard.session.snapshot();
        let p = guard.session.pipeline_probe();
        if acknowledged(&m)
            && !m.auto.active
            && m.auto.phase == AutoPhase::Blocked
            && matches!(m.phase, LongPhase::Paused | LongPhase::Failed)
            && !m.busy
            && p.producer_quiesced
            && p.queue_depth == 0
            && p.reserved_raw_bytes == 0
            && !p.capture_in_flight
            && !p.consumer_in_flight
        {
            break m;
        }
        if Instant::now() >= deadline || m.worker_exited {
            return Err(format!("关闭目标未收束到真实安全终态：{m:?}；{p:?}").into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let count = observe_no_more(&target)?;
    ensure(count == 1, "目标销毁后发生额外滚轮消息")?;
    let saved = guard
        .session
        .stitch_snapshot()
        .ok_or("目标失效后没有保留生产输出快照")?;
    ensure(
        same_segments(&baseline, &saved),
        "目标关闭后可信条带 Arc 被替换",
    )?;
    fixture::same_image(
        &fixture::stitched_image(&saved)?,
        &initial,
        "目标失效后旧结果像素",
    )?;
    let guard_probe = target.capture_probe_snapshot();
    let trigger = if terminal.phase == LongPhase::Failed {
        ensure(
            guard_probe["failures"].as_u64().unwrap_or(0) > 0
                && terminal.status.contains("采集失败"),
            "Failed 未提供真实采集守护错误证据",
        )?;
        "producer-frame-gate"
    } else {
        ensure(
            !terminal.auto.status.is_empty(),
            "Paused 缺少目标验证拒绝原因",
        )?;
        "native-target-validation-or-post"
    };
    let pipeline = guard.session.pipeline_probe();
    let closed = guard.close()?;
    Ok(
        serde_json::json!({"closed_hwnd":target.native(),"wheel_count":count,"events":events,
        "capture_phase":format!("{:?}",terminal.phase),"auto_phase":format!("{:?}",terminal.auto.phase),
        "trigger":trigger,"status":terminal.status,"auto_status":terminal.auto.status,
        "capture_guard":guard_probe,"pipeline":pipeline,"baseline_same_arc":true,
        "all_saved_pixels_unchanged":true,"quiet_observed_ms":QUIET_CHECK.as_millis(),"close":closed}),
    )
}

pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut qa = Qa::new()?;
    let selection = group_selection();
    let requested = selection.as_ref().cloned().unwrap_or_default();
    let result = (|| -> Result<()> {
        let groups = selection?;
        let capture = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let _capture = crate::gpui_capture::CaptureLock::new(capture);
        ensure(!already_exists, "已有截图会话，未启动自动专项")?;
        ensure(
            long_capture_runtime::active_workers() == 0
                && long_capture_pipeline::active_capture_threads() == 0,
            "专项启动前存在采集线程",
        )?;
        for name in &groups {
            let detail = match name.as_str() {
                "bounds" => bounds(&mut qa),
                "speed" => speed(&mut qa),
                "no-motion" => no_motion(&mut qa),
                "target" => target_closed(&mut qa),
                "capture-gate" => gate_smoke::verify(&mut qa),
                _ => unreachable!(),
            }?;
            qa.record(name, detail)?;
        }
        ensure(
            long_capture_runtime::active_workers() == 0
                && long_capture_pipeline::active_capture_threads() == 0,
            "自动专项残留采集线程",
        )?;
        ensure(
            long_capture_runtime::panicked_workers() == 0,
            "自动专项采集线程 panic",
        )
    })();
    let error = result.as_ref().err().map(ToString::to_string);
    qa.finish(&requested, error.as_deref())?;
    result
}
