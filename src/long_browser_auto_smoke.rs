//! 生产定向滚轮 → 隔离 Chrome DOM → DXGI 长图；控制器只能观察。
use super::*;
use crate::long_auto::{AutoDirection, AutoPhase, AutoScrollOptions};
use crate::long_auto_native::ScrollTargetProbe;

#[derive(Default)]
struct Trace {
    sequence: u64,
    posts: usize,
    generation: Option<u64>,
    post_fence: Option<u64>,
    events: Vec<Value>,
    observations: Vec<Value>,
}
impl Trace {
    fn collect(&mut self, session: &LongSession) -> Result<()> {
        for event in session.pipeline_probe().auto_events {
            if event.sequence <= self.sequence {
                continue;
            }
            ensure(
                event.sequence == self.sequence + 1,
                "Auto 事件 ring 出现未收集的缺口",
            )?;
            ensure(self.events.len() < 4096, "Auto 探针超过本组有界事件预算")?;
            ensure(
                Some(event.epoch) == self.generation,
                "Auto 探针事件属于旧请求代数",
            )?;
            match event.event {
                "post" => {
                    let fence = event.fence_sequence.ok_or("生产 post 缺少抓屏 fence")?;
                    ensure(fence >= event.frame_sequence, "post fence 早于已提交帧")?;
                    self.post_fence = Some(fence);
                }
                "observe" => {
                    if let Some(fence) = self.post_fence {
                        ensure(
                            event.fence_sequence == Some(fence) && event.frame_sequence > fence,
                            "原生 post 被 fence 之前的旧帧确认",
                        )?;
                    }
                }
                "ignore-pre-post" => {
                    let fence = self.post_fence.ok_or("忽略旧帧事件早于首个 post")?;
                    ensure(
                        event.fence_sequence == Some(fence) && event.frame_sequence <= fence,
                        "旧帧忽略事件与当前 post fence 不符",
                    )?;
                }
                "capture-gate-rejected" => {}
                _ => return Err("未知 Auto 探针事件".into()),
            }
            self.sequence = event.sequence;
            self.posts += usize::from(event.event == "post");
            self.events.push(json!({"observed_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"event":event}));
        }
        Ok(())
    }
    fn observe(&mut self, state: &Value, session: &LongSession) -> Result<()> {
        ensure(self.observations.len() < 96, "只读 DOM 观察超过本组预算")?;
        let meta = session.snapshot();
        self.observations
            .push(json!({"state":state,"generation":meta.generation,
            "requested_generation":meta.requested_generation,"phase":format!("{:?}",meta.phase),
            "auto_phase":format!("{:?}",meta.auto.phase),"auto_active":meta.auto.active,
            "auto_steps":meta.auto.steps,"width":meta.width,"height":meta.height,
            "viewport_y":meta.viewport.y,"accepted_frames":meta.accepted_frames,
            "discarded_frames":meta.discarded_frames,"post_events_collected":self.posts,
            "foreground_hwnd":unsafe {GetForegroundWindow()}.0 as usize}));
        Ok(())
    }
    fn save(&self, root: &Path, ok: bool, error: Option<String>) -> Result<()> {
        std::fs::write(
            root.join("native-auto-trace.json"),
            serde_json::to_vec_pretty(&json!({
                "ok":ok,"error":error,"last_sequence":self.sequence,"posts":self.posts,
                "events":self.events,"observations":self.observations,
                "event_limit":4096,"observation_limit":96,
                "event_timestamp_is_observation_time":true,"dom_position_inferred_from_wheel":false
            }))?,
        )?;
        Ok(())
    }
}
fn verify_target(
    owner: &BrowserCaptureOwner,
    target: &ScrollTargetProbe,
    region: ScreenRect,
) -> Result<&'static str> {
    let native = owner.native_snapshot();
    ensure(
        target.root == native.hwnd
            && target.root_process == native.window_pid
            && target.root_thread == native.window_thread
            && target.root_class == native.class_name
            && target.root_rect == native.window_rect.screen()
            && target.region == region,
        "生产 Auto 根窗口绑定不属于原浏览器冻结身份",
    )?;
    if target.window == native.hwnd {
        ensure(
            target.process == native.window_pid
                && target.thread == native.window_thread
                && target.class == native.class_name
                && target.window_rect == native.window_rect.screen(),
            "Auto 根窗口消息接收身份不符",
        )?;
        Ok("original-browser-root")
    } else {
        let content = native
            .content_window
            .as_ref()
            .ok_or("Auto 子窗口没有原句柄归属证据")?;
        ensure(
            target.window == content.hwnd
                && target.process == content.window_pid
                && target.thread == content.window_thread
                && target.class == content.class_name,
            "Auto 消息接收窗口不是已持句柄/creation/Job 的原内容窗口",
        )?;
        Ok("original-browser-content-window")
    }
}
fn begin(
    session: &mut Session,
    bridge: &mut Bridge,
    trace: &mut Trace,
    direction: AutoDirection,
) -> Result<()> {
    trace.collect(session.get())?;
    session.get_mut().set_paused(false)?;
    wait_session(session.get(), "Auto 前恢复采集 ACK", |m| {
        m.phase == LongPhase::Running && m.generation == m.requested_generation && !m.busy
    })?;
    session
        .get_mut()
        .set_auto_scroll(Some(AutoScrollOptions { direction }))?;
    let generation = session.get().snapshot().requested_generation;
    trace.generation = Some(generation);
    trace.post_fence = None;
    bridge.request(
        "native_auto_phase",
        json!({"phase":"active","generation":generation}),
    )?;
    wait_session(session.get(), "真实 Auto 请求 ACK", |m| {
        m.generation == generation
            && m.requested_generation == generation
            && m.phase == LongPhase::Running
            && m.auto.active
    })
}
fn drain(session: &mut Session, bridge: &mut Bridge, trace: &mut Trace) -> Result<Value> {
    session.get_mut().drain_for_output()?;
    let until = Instant::now() + WAIT;
    loop {
        trace.collect(session.get())?;
        let meta = session.get().snapshot();
        if meta.phase == LongPhase::Paused && session.quiet() {
            break;
        }
        ensure(
            Instant::now() < until && meta.phase != LongPhase::Failed,
            "Auto 最终帧 drain 没有达到 quiet ACK",
        )?;
        thread::sleep(Duration::from_millis(10));
    }
    let generation = session.get().snapshot().generation;
    bridge.request(
        "native_auto_phase",
        json!({"phase":"paused","generation":generation,
        "producer_quiet":true,"consumer_quiet":true,"raw_slots":0,"queue_slots":0}),
    )?;
    bridge.request("observe_paused", json!({"generation":generation}))
}
fn finite_top(state: &Value, spec: oracle::Spec) -> Result<f64> {
    ensure(
        state["pattern_id"] == "bgra-world-v1"
            && state["dpr"].as_f64() == Some(spec.dpr)
            && state["observer_mode"] == "native-auto",
        "只读 DOM 来源或 DPR 改变",
    )?;
    let top = state["scroll_top"].as_f64().ok_or("DOM scrollTop 缺失")?;
    ensure(
        top.is_finite() && (0. ..=480.).contains(&top),
        "实际 DOM 位置越界",
    )?;
    Ok(top)
}
pub(super) fn verify(
    bridge: &mut Bridge,
    report: &mut Report,
    owner: &Arc<BrowserCaptureOwner>,
) -> Result<()> {
    let mut trace = Trace::default();
    let result = verify_inner(bridge, report, owner, &mut trace);
    trace.save(
        &report.root,
        result.is_ok(),
        result.as_ref().err().map(ToString::to_string),
    )?;
    result
}
fn verify_inner(
    bridge: &mut Bridge,
    report: &mut Report,
    owner: &Arc<BrowserCaptureOwner>,
    trace: &mut Trace,
) -> Result<()> {
    let permit = owner.calibration_permit()?;
    let initial = bridge.request("calibrate", json!({}))?;
    let spec = oracle::Spec::new(initial["state"]["dpr"].as_f64().ok_or("DOM DPR 缺失")?)?;
    ensure(
        finite_top(&initial["state"], spec)? == 0.,
        "首帧 DOM 不在文档起点",
    )?;
    let renderer = read_renderer(&initial, &report.root)?;
    let renderer_world = oracle::world(&renderer, spec, 0, TOLERANCE)?;
    let client_region = owner.client_region();
    let client = read_owned(client_region, permit.gate())?;
    let calibration = oracle::calibrate(&client, &renderer, spec, TOLERANCE)?;
    let region = ScreenRect {
        left: i32::try_from(i64::from(client_region.left) + i64::from(calibration.offset_left))?,
        top: i32::try_from(i64::from(client_region.top) + i64::from(calibration.offset_top))?,
        width: spec.width,
        height: spec.viewport_height,
    };
    // 正常 app 的真实 origin 由唯一完整像素匹配冻结；只对旧 kiosk 保留精确 CSS 原点合同。
    if owner.config().content_window.is_none() {
        ensure(
            initial["state"]["scrollport"]["left"]
                .as_f64()
                .map(|v| v * spec.dpr)
                == Some(f64::from(calibration.offset_left))
                && initial["state"]["scrollport"]["top"]
                    .as_f64()
                    .map(|v| v * spec.dpr)
                    == Some(f64::from(calibration.offset_top)),
            "kiosk 唯一校准偏移与 DOM CSS 相位不符",
        )?;
    }
    drop(client);
    let mut session = start(owner, region, &permit)?;
    wait_session(session.get(), "真实浏览器初帧", |m| {
        m.phase == LongPhase::Running
            && m.generation == m.requested_generation
            && m.width == spec.width
            && m.height == spec.viewport_height
    })?;
    let target = session.get().auto_target_probe()?;
    let route = verify_target(owner, &target, region)?;
    session.pause()?;
    let first = session
        .get()
        .stitch_snapshot()
        .ok_or("首帧暂停没有共享结果")?;
    let mut weaks = image_weaks(&first);
    let (image, first_world) = pixels(&first, spec, 0)?;
    let first_renderer = oracle::region(&image, &renderer, (0, 0), TOLERANCE)?;
    report.pass(
        "owned-browser-route-and-first-frame",
        json!({"target":target,"route":route,
        "calibration":calibration,"renderer_world":renderer_world,"first_world":first_world,
        "renderer_compare":first_renderer,"mapping":owner.content_mapping_probe(),
        "same_read_and_bind_permit":true,"titlebar_height_guessed":false,
        "foreground_hwnd":unsafe {GetForegroundWindow()}.0 as usize}),
    )?;
    drop(image);
    drop(renderer);
    drop(first);
    begin(&mut session, bridge, trace, AutoDirection::Forward)?;
    let until = Instant::now() + WAIT;
    let mut next_observe = Instant::now();
    loop {
        trace.collect(session.get())?;
        let meta = session.get().snapshot();
        if Instant::now() >= next_observe {
            let response = bridge.request("observe", json!({}))?;
            finite_top(&response["state"], spec)?;
            trace.observe(&response["state"], session.get())?;
            trace.save(&report.root, false, None)?;
            report.stage("native-auto-progress", json!({"scroll_top":response["state"]["scroll_top"],
                "scroll_epoch":response["state"]["scroll_epoch"],"auto_phase":format!("{:?}",meta.auto.phase),
                "steps":meta.auto.steps,"height":meta.height,"viewport_y":meta.viewport.y,
                "wheel_events":response["state"]["input_observer"]["wheel_events"],
                "scroll_events":response["state"]["input_observer"]["scroll_events"]}))?;
            next_observe = Instant::now() + Duration::from_millis(250);
        }
        if !meta.auto.active {
            ensure(
                meta.phase == LongPhase::Running && meta.auto.phase == AutoPhase::NoMotion,
                &format!("Auto 未在真实页尾无运动时正常停下：{meta:?}"),
            )?;
            ensure(
                meta.auto.steps > 0 && trace.posts == meta.auto.steps as usize,
                "生产 steps 与逐项收集的 post 事件不符",
            )?;
            break;
        }
        ensure(
            meta.phase == LongPhase::Running && Instant::now() < until,
            &format!("原生 Auto 没有走完浏览器：{meta:?}"),
        )?;
        thread::sleep(Duration::from_millis(10));
    }
    let stopped = session.get().snapshot();
    let response = drain(&mut session, bridge, trace)?;
    let offset = physical_scroll(&response["state"], spec.dpr)?;
    ensure(
        offset == spec.document_height - spec.viewport_height
            && response["state"]["input_observer"]["wheel_events"]
                .as_u64()
                .unwrap_or(0)
                > 0
            && response["state"]["input_observer"]["scroll_events"]
                .as_u64()
                .unwrap_or(0)
                > 0,
        "原生消息成功但实际 DOM 未滚到文档末尾",
    )?;
    let full = session
        .get()
        .stitch_snapshot()
        .ok_or("Auto drain 没有长图")?;
    weaks.extend(image_weaks(&full));
    ensure(
        full.width == spec.width
            && full.height == spec.document_height
            && full.viewport.y == offset,
        "Auto 实际拼接范围与完整 DOM 文档不一致",
    )?;
    let (full_image, full_world) = pixels(&full, spec, 0)?;
    let final_renderer = read_renderer(&response, &report.root)?;
    let final_world = oracle::world(&final_renderer, spec, offset, TOLERANCE)?;
    let tail = oracle::region(&full_image, &final_renderer, (0, offset), TOLERANCE)?;
    let output = report.root.join("native-auto-full.png");
    crate::export::save_png(full_image, &output)?;
    ensure(
        session.get().auto_target_probe()? == target,
        "Auto 前后原消息绑定发生变化",
    )?;
    report.pass(
        "native-post-dom-motion-and-dxgi-full-output",
        json!({"state":response["state"],
        "full_world":full_world,"renderer_world":final_world,"renderer_final_viewport":tail,
        "output_png":output,"output_size":[full.width,full.height],"viewport_y":full.viewport.y,
        "post_count":trace.posts,"auto_steps":stopped.auto.steps,"no_motion_phase":"NoMotion",
        "still_manual_running_before_drain":stopped.phase==LongPhase::Running,
        "same_frozen_target":true,"target":target,"pipeline":session.get().pipeline_probe()}),
    )?;
    drop(final_renderer);

    // 在同一原绑定/原租约中反向恢复，直到实际 DOM 运动后撤销；不校准或 CDP 帮助滚动。
    let before_reverse_posts = trace.posts;
    begin(&mut session, bridge, trace, AutoDirection::Reverse)?;
    let until = Instant::now() + WAIT;
    let mut next_observe = Instant::now();
    loop {
        trace.collect(session.get())?;
        let meta = session.get().snapshot();
        if Instant::now() >= next_observe {
            let response = bridge.request("observe", json!({}))?;
            let top = finite_top(&response["state"], spec)?;
            trace.observe(&response["state"], session.get())?;
            let observed = session.get().snapshot();
            if top < 480.
                && trace.posts > before_reverse_posts
                && observed.phase == LongPhase::Running
                && observed.auto.active
                && observed.generation == observed.requested_generation
            {
                break;
            }
            next_observe = Instant::now() + Duration::from_millis(100);
        }
        ensure(
            Instant::now() < until && meta.phase == LongPhase::Running && meta.auto.active,
            "同窗口反向 Auto 没有造成实际 DOM 位移",
        )?;
        thread::sleep(Duration::from_millis(10));
    }
    let before_revoke = session.get().snapshot();
    ensure(
        before_revoke.phase == LongPhase::Running
            && before_revoke.auto.active
            && before_revoke.generation == before_revoke.requested_generation,
        "撤约前生产 Auto 已不活跃",
    )?;
    let revoked = bridge.request("revoke_lease", json!({}))?;
    wait_session(session.get(), "活跃 Auto 的原 lease 拒绝", |m| {
        m.phase == LongPhase::Failed && m.generation == m.requested_generation
    })?;
    trace.collect(session.get())?;
    let rejected_posts = trace.posts;
    let failed = session.get().snapshot();
    ensure(
        failed.phase == LongPhase::Failed
            && trace.posts - before_reverse_posts == failed.auto.steps as usize,
        "反向 Auto 实际 post 事件与 Failed ACK 的步骤计数不符",
    )?;
    let failed_ack_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let retained = session
        .get()
        .smoke_committed_snapshot()
        .ok_or("撤约后可信结果丢失")?;
    let (image, retained_world) = pixels(&retained, spec, 0)?;
    drop(image);
    ensure(
        retained.width == full.width && retained.height == full.height,
        "撤约后可信长图范围丢失",
    )?;
    let monitor_started = Instant::now();
    let until = monitor_started + Duration::from_millis(700);
    while Instant::now() < until {
        trace.collect(session.get())?;
        ensure(
            trace.posts == rejected_posts,
            "Failed ACK 后 Auto 继续投递滚轮",
        )?;
        thread::sleep(Duration::from_millis(10));
    }
    let monitor_elapsed_ms = monitor_started.elapsed().as_secs_f64() * 1000.;
    let monitor_end_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let after = session
        .get()
        .smoke_committed_snapshot()
        .ok_or("Failed 后可信图消失")?;
    ensure(
        same_sources(&retained, &after) && session.get().auto_target_probe()? == target,
        "Failed 后原可信 source 或原滚轮绑定改变",
    )?;
    weaks.extend(image_weaks(&retained));
    report.pass("active-native-auto-lease-revocation", json!({"response":revoked,
        "retained_world":retained_world,"posts_at_failed_ack":rejected_posts,"posts_after_700ms":trace.posts,
        "no_additional_posts_after_failed_ack":true,"source_arcs_stable_after_failed_ack":true,
        "failed_ack_unix_ms":failed_ack_unix_ms,"monitor_end_unix_ms":monitor_end_unix_ms,
        "monitor_elapsed_ms":monitor_elapsed_ms,"reverse_post_count":trace.posts-before_reverse_posts,
        "reverse_auto_steps_at_failed_ack":failed.auto.steps,
        "failed_ack_time_is_observation_after_flushing_old_events":true,
        "before_revoke_generation":before_revoke.generation,"active_before_revoke":true,
        "pre_revoke_in_flight_commits_not_forced_quiet":true,
        "same_frozen_target":true,"pipeline":session.get().pipeline_probe()}))?;
    let closed = session.close()?;
    drop(after);
    drop(retained);
    drop(full);
    drop(session);
    ensure(
        weaks.iter().all(|v| v.upgrade().is_none()),
        "关闭后浏览器采集源图仍存活",
    )?;
    ensure(
        long_capture_pipeline::active_capture_threads() == 0
            && long_capture_runtime::active_workers() == 0
            && long_capture_runtime::panicked_workers() == 0,
        "真实采集两端未正常退出",
    )?;
    let browser_closed = bridge.request("close_browser", json!({}))?;
    let hwnd = HWND(owner.config().hwnd as *mut _);
    let until = Instant::now() + WAIT;
    while unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        ensure(Instant::now() < until, "原浏览器 HWND 没有正常销毁")?;
        thread::sleep(Duration::from_millis(10));
    }
    report.pass(
        "native-auto-resources-released",
        json!({"close":closed,"source_weak_count":weaks.len(),
        "all_source_weaks_expired":true,"capture_threads":0,"consumer_sessions":0,
        "panicked_threads":0,"browser":browser_closed,"native_hwnd_destroyed":true}),
    )?;
    bridge.finish()?;
    report.protocol_joined = true;
    report.protocol = bridge.probe();
    Ok(())
}
