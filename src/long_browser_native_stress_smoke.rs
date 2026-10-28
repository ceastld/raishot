//! 保留生产边框和两级工具栏：同一工作进程三轮原生 Auto 32k 生命周期。
use super::*;
use crate::long_auto::AutoPhase;
use gpui::RenderImage;

const GROUP: &str = "browser-native-auto-three-cycles";
const AUTO_WAIT: Duration = Duration::from_secs(160);

fn unix_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

struct NativeReport {
    root: PathBuf,
    owner: Arc<BrowserCaptureOwner>,
    cases: Vec<Value>,
    stages: Vec<Value>,
    trace: BufWriter<std::fs::File>,
    global_sequence: u64,
    sequence: u64,
    posts: usize,
    cycle: u32,
    fences: HashMap<u64, u64>,
    sources: SourceTrace,
    render_weaks: Vec<Weak<RenderImage>>,
    render_seen: HashSet<usize>,
    preview_sources: Vec<Weak<Image>>,
    preview_seen: HashSet<usize>,
    close_private: Vec<u64>,
    protocol: Value,
}
impl NativeReport {
    fn new(owner: Arc<BrowserCaptureOwner>, config: &Value) -> Result<Self> {
        ensure(
            config["mode"] == "native-auto"
                && config["scenario"] == "native-stress"
                && std::env::var("RUSTSHOT_BROWSER_QA_MODE").as_deref() == Ok("native-auto"),
            "原生三轮 mode/scenario 不符",
        )?;
        let root = PathBuf::from(config["artifact_root"].as_str().ok_or("记录目录缺失")?)
            .canonicalize()?;
        let base = std::env::current_dir()?.join("target/qa").canonicalize()?;
        ensure(root.starts_with(&base) && root != base, "记录目录越界")?;
        let trace = BufWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("native-stress-trace.jsonl"))?,
        );
        Ok(Self {
            root,
            owner,
            cases: vec![],
            stages: vec![],
            trace,
            global_sequence: 0,
            sequence: 0,
            posts: 0,
            cycle: 0,
            fences: HashMap::new(),
            sources: SourceTrace::default(),
            render_weaks: vec![],
            render_seen: HashSet::new(),
            preview_sources: vec![],
            preview_seen: HashSet::new(),
            close_private: vec![],
            protocol: Value::Null,
        })
    }
    fn save(&mut self, complete: bool, error: Option<&str>) -> Result<()> {
        self.trace.flush()?;
        let ok = complete && error.is_none();
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&json!({
                "suite":"long-browser-native-stress","mode":"native-auto","scenario":"native-stress",
                "pid":std::process::id(),"ok":ok,"error":error,"requested_groups":[GROUP],
                "groups":if ok {vec![json!({"name":GROUP,"ok":true,"cases":self.cases})]}else{vec![]},
                "stages":self.stages,"native_owner":self.owner.native_snapshot(),"ownership":self.owner.probe(),
                "real_browser_compositor":true,"real_dxgi_capture":ok,"full_desktop_freeze_used":false,
                "fixture_substituted":false,"clipboard_untouched":true,"global_input_sent":false,
                "production_border_removed_for_strict_gate":false,"gpui_toolbar_auto_verified":ok,
                "protocol":self.protocol,"protocol_thread_joined":self.protocol["reader_joined"]==true,
                "capture_threads_after_close":long_capture_pipeline::active_capture_threads(),
                "consumer_sessions_after_close":long_capture_runtime::active_workers(),
                "native_posts":self.posts,"trace_file":"native-stress-trace.jsonl",
                "close_private_bytes":self.close_private,"close_growth_limit_bytes":CLOSE_STABILITY_BYTES,
                "whole_reference_bgra_bytes":0,"runtime_memory_excludes_compiler_browser_and_node":true,
                "limitations":["隔离自有Chrome的固定32k页面/实际125%DPI，不证明任意网页或混合DPI",
                    "Auto/Edit/Return/Pin由真实自有HWND工具栏触发；五项Pin处理调用生产action",
                    "100ms离散样本不是连续内存峰值；QA隐藏Host到应用退出才销毁",
                    "Weak/drop_image/rendererDrop分别证明生命周期，未直接查询全部COM/atlas驻留"]
            }))?,
        )?;
        Ok(())
    }
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        ensure(self.stages.len() < 128, "阶段记录超过预算")?;
        self.stages
            .push(json!({"stage":name,"unix_ms":unix_ms()?,"cycle":self.cycle,
            "cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail}));
        self.save(false, None)
    }
    fn line(&mut self, value: Value) -> Result<()> {
        serde_json::to_writer(&mut self.trace, &value)?;
        self.trace.write_all(b"\n")?;
        Ok(())
    }
    fn begin(&mut self, cycle: u32) -> Result<()> {
        self.cycle = cycle;
        self.sequence = 0;
        self.fences.clear();
        // 已确认上轮 Weak 全失效后，只保留计数和磁盘记录。
        self.sources = SourceTrace::default();
        self.render_weaks.clear();
        self.render_seen.clear();
        self.preview_sources.clear();
        self.preview_seen.clear();
        self.line(
            json!({"kind":"cycle-begin","cycle":cycle,"unix_ms":unix_ms()?,
            "global_sequence":self.global_sequence}),
        )
    }
    fn collect(
        &mut self,
        panel: WindowHandle<LongCaptureView>,
        cx: &mut AsyncApp,
    ) -> Result<LongSnapshot> {
        let (meta, probe, snapshot, preview) = panel.update(cx, |v, _, _| {
            (
                v.snapshot(),
                v.session.as_ref().map(|s| s.pipeline_probe()),
                v.stitch_snapshot(),
                v.preview_window,
            )
        })?;
        if let Some(snapshot) = snapshot {
            self.sources.observe(&snapshot);
        }
        if let Some(preview) = preview {
            let (weaks, lifecycle) =
                preview.update(cx, |v, _, _| (v.image_weaks(), v.lifecycle_probe()))?;
            ensure(
                lifecycle.current_bgra_used_bytes <= 4_000_000
                    && lifecycle.current_bgra_capacity_bytes >= lifecycle.current_bgra_used_bytes,
                "32k右预览缓冲超预算",
            )?;
            for weak in weaks.current.into_iter().chain(weaks.retired) {
                if self.render_seen.insert(weak.as_ptr() as usize) {
                    self.render_weaks.push(weak);
                }
            }
            if let Some(weak) = weaks.source {
                if self.preview_seen.insert(weak.as_ptr() as usize) {
                    self.preview_sources.push(weak);
                }
            }
            ensure(
                self.render_weaks.len() < 4096 && self.preview_sources.len() < 4096,
                "32k Weak观察超预算",
            )?;
        }
        if let Some(probe) = probe {
            for event in probe.auto_events {
                if event.sequence <= self.sequence {
                    continue;
                }
                ensure(
                    event.sequence == self.sequence + 1 && self.global_sequence < 32768,
                    "32k Auto事件缺口或超预算",
                )?;
                match event.event {
                    "post" => {
                        let fence = event.fence_sequence.ok_or("post缺少fence")?;
                        ensure(fence >= event.frame_sequence, "post fence早于提交帧")?;
                        self.fences.insert(event.epoch, fence);
                        self.posts += 1;
                    }
                    "observe" => {
                        if let Some(fence) = self.fences.get(&event.epoch) {
                            ensure(
                                event.fence_sequence == Some(*fence)
                                    && event.frame_sequence > *fence,
                                "32k Auto由旧帧确认",
                            )?;
                        }
                    }
                    "ignore-pre-post" => {
                        let fence = self.fences.get(&event.epoch).ok_or("ignore早于post")?;
                        ensure(
                            event.fence_sequence == Some(*fence) && event.frame_sequence <= *fence,
                            "32k忽略旧帧fence不符",
                        )?;
                    }
                    "capture-gate-rejected" => return Err("32k原生采集归属被拒绝".into()),
                    _ => return Err("未知32k Auto事件".into()),
                }
                self.sequence = event.sequence;
                self.global_sequence += 1;
                self.line(json!({"kind":"auto-event","cycle":self.cycle,"global_sequence":self.global_sequence,
                    "observed_unix_ms":unix_ms()?,"event":event}))?;
            }
        }
        self.trace.flush()?;
        Ok(meta)
    }
    fn observe(&mut self, state: &Value, meta: &LongSnapshot) -> Result<f64> {
        let top = state["scroll_top"]
            .as_f64()
            .ok_or("32k DOM scrollTop缺失")?;
        ensure(
            top.is_finite()
                && (0. ..=25088.).contains(&top)
                && state["pattern_id"] == "bgra-world-tiled-v2"
                && state["observer_mode"] == "native-auto"
                && state["cycle"] == self.cycle,
            "32k DOM世界/位置不符",
        )?;
        let physical = top * 1.25;
        ensure(
            (physical - physical.round()).abs() <= 0.003,
            "32k实际物理phase超界",
        )?;
        self.line(
            json!({"kind":"dom-observation","cycle":self.cycle,"state":state,
            "generation":meta.generation,"requested_generation":meta.requested_generation,
            "phase":format!("{:?}",meta.phase),"auto_phase":format!("{:?}",meta.auto.phase),
            "auto_active":meta.auto.active,"height":meta.height,"viewport_y":meta.viewport.y,
            "posts":self.posts,"raw_physical_top":physical,"device_phase_tolerance":0.003}),
        )?;
        Ok(top)
    }
}

async fn wait_state(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    predicate: impl Fn(&LongSnapshot) -> bool,
    report: &mut NativeReport,
    cx: &mut AsyncApp,
) -> Result<LongSnapshot> {
    let end = Instant::now() + WAIT;
    loop {
        let meta = report.collect(panel, cx)?;
        if predicate(&meta) {
            return Ok(meta);
        }
        ensure(
            meta.phase != LongPhase::Failed && Instant::now() < end,
            &format!("{label}失败/超时：{meta:?}"),
        )?;
        tick(cx).await;
    }
}
async fn pause_ack(
    panel: WindowHandle<LongCaptureView>,
    report: &mut NativeReport,
    cx: &mut AsyncApp,
) -> Result<(LongSnapshot, Value)> {
    wait_state(
        panel,
        "一级暂停",
        |m| committed(m) && m.phase == LongPhase::Paused && !m.busy,
        report,
        cx,
    )
    .await?;
    let end = Instant::now() + WAIT;
    loop {
        let meta = report.collect(panel, cx)?;
        let quiet = panel.update(cx, |v, _, _| {
            v.session.as_ref().is_some_and(|s| {
                let p = s.pipeline_probe();
                p.producer_quiesced
                    && !p.capture_in_flight
                    && !p.consumer_in_flight
                    && p.queue_depth == 0
                    && p.reserved_raw_bytes == 0
                    && p.drain_fence.is_none()
            })
        })?;
        if quiet {
            return Ok((
                meta,
                json!({"producer_quiet":true,"consumer_quiet":true,"raw_slots":0,
                "queue_slots":0,"drain_fence":null,"unix_ms":unix_ms()?}),
            ));
        }
        ensure(Instant::now() < end, "32k暂停未真正排空")?;
        tick(cx).await;
    }
}

async fn cycle(
    bridge: &SharedBridge,
    cycle: u32,
    report: &mut NativeReport,
    cx: &mut AsyncApp,
) -> Result<()> {
    report.begin(cycle)?;
    let owner = report.owner.clone();
    let permit = owner.calibration_permit()?;
    let response = request(bridge, "calibrate", json!({}), cx).await?;
    ensure(
        response["state"]["scroll_top"] == 0 && response["state"]["cycle"] == cycle,
        "32k新轮未回到world0",
    )?;
    let spec = oracle::Spec::new(response["state"]["dpr"].as_f64().ok_or("DPR缺失")?)?;
    let renderer = read_renderer(&response, &report.root)?;
    let client = owner.client_region();
    let gate = permit.gate().clone();
    let (region,calibration)=cx.background_executor().spawn(async move {
        (|| -> Result<_> {
            let renderer_check=oracle::verify_image(&renderer,0)?;
            let pixels=read_owned(client,gate)?;
            let offset=calibrate(&pixels,&renderer)?;
            Ok((ScreenRect { left:client.left+i32::try_from(offset.0)?,top:client.top+i32::try_from(offset.1)?,
                width:spec.width,height:spec.viewport_height },
                json!({"unique_matches":1,"offset":offset,"renderer":renderer_check,"same_read_and_bind_permit":true})))
        })().map_err(|e|e.to_string())
    }).await.map_err(|e|e.to_string())?;
    let factory = owner.bind_content(region, &permit)?;
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
    let border_handles = loop {
        let border = panel.update(cx, |v, _, _| {
            v.border.as_ref().map(|b| {
                (
                    b.hud_descriptor(),
                    b.handles()
                        .iter()
                        .map(|h| h.0 as usize)
                        .chain(b.resize_handles().iter().map(|(_, h, _)| h.0 as usize))
                        .collect::<Vec<_>>(),
                )
            })
        })?;
        if let Some((descriptor, handles)) = border {
            ensure(
                panel.update(cx, |v, _, _| v.session.is_none())?,
                "原Border登记前采集已开始",
            )?;
            owner.register_border_hud(descriptor)?;
            break handles;
        }
        ensure(
            panel.update(cx, |v, _, _| v.session.is_none())? && Instant::now() < end,
            "32k原Border未及时登记",
        )?;
        tick(cx).await;
    };
    wait_state(
        panel,
        "保留边框的32k首帧",
        |m| committed(m) && m.phase == LongPhase::Running && m.height == 640,
        report,
        cx,
    )
    .await?;
    let panel_hwnd = panel.update(cx, |_, w, _| hwnd(w).0 as usize)?;
    let panel_entity = panel.update(cx, |_, _, cx| cx.entity().downgrade())?;
    let first_preview = preview_presented(panel, region, cx).await?;
    let preview = panel.update(cx, |v, _, _| v.preview_window.ok_or("32k右预览缺失"))??;
    let preview_hwnd = preview.update(cx, |_, w, _| hwnd(w).0 as usize)?;
    let (ledger, preview_entity) = preview.update(cx, |v, _, cx| {
        (v.lifecycle_ledger(), cx.entity().downgrade())
    })?;
    let target = panel.update(cx, |v, _, _| {
        v.session.as_ref().ok_or("session缺失")?.auto_target_probe()
    })??;
    let native = owner.native_snapshot();
    ensure(
        target.root == native.hwnd
            && target.root_process == native.window_pid
            && target.region == region
            && native.content_window.as_ref().is_some_and(|c| {
                c.hwnd == target.window
                    && c.window_pid == target.process
                    && c.window_thread == target.thread
            }),
        "32k Auto未绑定原内容HWND",
    )?;
    report.stage(
        "cycle-initial-ui",
        json!({"spec":spec,"region":rect_json(region),"calibration":calibration,
        "target":target,"border_hwnds":border_handles,"preview":first_preview}),
    )?;
    // 通过原两级 UI 验证设置暂停与恢复，再启动自适应滚动。
    panel_button(panel, "long-settings", cx).await?;
    pause_ack(panel, report, cx).await?;
    panel_button(panel, "long-settings", cx).await?;
    wait_state(
        panel,
        "设置关闭恢复采集",
        |m| committed(m) && m.phase == LongPhase::Running,
        report,
        cx,
    )
    .await?;
    let active_generation = panel
        .update(cx, |v, _, _| {
            v.snapshot().requested_generation.checked_add(1)
        })?
        .ok_or("generation溢出")?;
    request(
        bridge,
        "native_auto_phase",
        json!({"phase":"active","generation":active_generation}),
        cx,
    )
    .await?;
    let posts_before = report.posts;
    panel_button(panel, "long-auto", cx).await?;
    wait_state(
        panel,
        "一级Auto真实启动",
        |m| {
            committed(m)
                && m.generation == active_generation
                && m.auto.active
                && (1.0..=10.0).contains(&m.auto.speed)
        },
        report,
        cx,
    )
    .await?;
    let end = Instant::now() + AUTO_WAIT;
    let mut next_observe = Instant::now();
    loop {
        let meta = report.collect(panel, cx)?;
        if Instant::now() >= next_observe {
            let response = request(bridge, "observe", json!({}), cx).await?;
            report.observe(&response["state"], &meta)?;
            next_observe = Instant::now() + Duration::from_millis(500);
        }
        if !meta.auto.active {
            ensure(
                meta.phase == LongPhase::Running && meta.auto.phase == AutoPhase::NoMotion,
                &format!("32k Auto未正常到页尾：{meta:?}"),
            )?;
            break;
        }
        ensure(
            meta.phase != LongPhase::Failed && Instant::now() < end,
            "32k Auto失败/未走完整页",
        )?;
        tick(cx).await;
    }
    panel_button(panel, "long-pause", cx).await?;
    let (finished, quiet_ack) = pause_ack(panel, report, cx).await?;
    request(
        bridge,
        "native_auto_phase",
        json!({"phase":"paused","generation":finished.generation,
        "producer_quiet":true,"consumer_quiet":true,"raw_slots":0,"queue_slots":0}),
        cx,
    )
    .await?;
    let response = request(
        bridge,
        "observe_paused",
        json!({"generation":finished.generation}),
        cx,
    )
    .await?;
    let final_top = report.observe(&response["state"], &finished)?;
    ensure(
        final_top == 25088. && finished.height == 32000 && finished.viewport.y == 31360,
        &format!("32k实际DOM和成品范围不符 top={final_top} meta={finished:?}"),
    )?;
    let pause_posts = report.posts;
    let stable = Instant::now();
    while stable.elapsed() < Duration::from_millis(350) {
        let meta = report.collect(panel, cx)?;
        ensure(
            report.posts == pause_posts
                && meta.phase == LongPhase::Paused
                && !meta.auto.active
                && meta.generation == finished.generation,
            "32k暂停ACK后仍投递",
        )?;
        tick(cx).await;
    }
    let pause_monitor_ms = stable.elapsed().as_secs_f64() * 1000.;
    let full = panel.update(cx, |v, _, _| v.stitch_snapshot().ok_or("32k页尾没有成品"))??;
    let tail = read_renderer(&response, &report.root)?;
    let full_check=cx.background_executor().spawn(async move {
        (|| -> Result<_> { Ok(json!({"world":oracle::verify_snapshot(&full,0)?,
            "tail_renderer":oracle::compare_viewport(&full,&tail)?,"source_capacity":oracle::source_allocations(&full)?})) })()
            .map_err(|e|e.to_string())
    }).await.map_err(|e|e.to_string())?;
    let final_preview = preview_presented(panel, region, cx).await?;
    report.collect(panel, cx)?;
    let lifecycle_before_close = ledger.snapshot();
    ensure(
        lifecycle_before_close.render_images_created > 1
            && lifecycle_before_close.drop_image_successes > 0
            && lifecycle_before_close.drop_image_failures == 0
            && lifecycle_before_close.retired_images == 0
            && lifecycle_before_close.render_images_created
                - lifecycle_before_close.drop_image_successes
                == 1,
        "32k右预览退休未收敛",
    )?;
    report.stage("capture-drained-32000",json!({"generation":finished.generation,"actual_dom_top":final_top,
        "posts":report.posts-posts_before,"check":full_check,"preview":final_preview,"lifecycle":lifecycle_before_close}))?;
    let panel_close_begin = unix_ms()?;
    panel_button(panel, "long-pin", cx).await?;
    let pin = find_window::<PinView>("Pin", cx).await?;
    pin_ready(pin, cx).await?;
    let mut capture_handles = border_handles.clone();
    capture_handles.extend([panel_hwnd, preview_hwnd]);
    native_gone(&capture_handles, cx).await?;
    source_gone(&report.sources.weaks, cx).await?;
    let panel_close_end = unix_ms()?;
    let (pin_weak, pin_entity, samples) = pin.update(cx, |v, _, cx| -> Result<_> {
        Ok((
            Arc::downgrade(&v.original),
            cx.entity().downgrade(),
            pin_samples(v, spec, 0, false)?,
        ))
    })??;
    report.stage("pin-open",json!({"actual_image":samples,"all_captured_sources_expired":true,
        "actual_gpui_sample":frame_sample(pin,None,cx).await?,"lifecycle":pin.update(cx,|v,_,_|v.preview_texture_lifecycle())?}))?;
    let mut retired = 0;
    let mut processing = vec![];
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
                pin_samples(v, spec, brightness, flip)?,
                v.preview_texture_lifecycle(),
            ))
        })??;
        ensure(
            lifecycle.1 == 0 && lifecycle.3 >= retired,
            "32k Pin退休队列不收敛",
        )?;
        retired = lifecycle.3;
        let detail = json!({"action":name,"samples":samples,"lifecycle":lifecycle});
        report.stage("pin-processing", detail.clone())?;
        processing.push(detail);
    }
    ensure(retired > 0, "32k Pin处理没有实际drop_image")?;
    // 借用生产Pin共享原图，stream PNG仅多一行转换缓冲；不增加117MiB物化副本。
    let source = pin.update(cx, |v, _, _| v.original.clone())?;
    let output = report
        .root
        .join(format!("native-stress-cycle-{cycle}-full.png"));
    let output_copy = output.clone();
    cx.background_executor()
        .spawn(async move {
            crate::export::save_png_pixels(
                960,
                32000,
                source.as_bytes(0).ok_or("Pin共享像素缺失")?,
                &output_copy,
            )
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?;
    let full_png = json!({"path":output,"bytes":std::fs::metadata(&output)?.len(),
        "production_stream_encoder":true,"full_image_clone":false,"width":960,"height":32000});
    let pin_handles = cx.update(|cx| -> Result<Vec<usize>> {
        cx.windows()
            .into_iter()
            .filter(|w| w.downcast::<Host>().is_none())
            .map(|w| {
                cx.update_window(w, |_, w, _| hwnd(w).0 as usize)
                    .map_err(Into::into)
            })
            .collect()
    })??;
    let pin_close_begin = unix_ms()?;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Close, w, cx))?;
    native_gone(&pin_handles, cx).await?;
    let end = Instant::now() + WAIT;
    loop {
        if panel_entity.upgrade().is_none()
            && preview_entity.upgrade().is_none()
            && pin_entity.upgrade().is_none()
            && pin_weak.upgrade().is_none()
            && report.render_weaks.iter().all(|w| w.upgrade().is_none())
            && report.preview_sources.iter().all(|w| w.upgrade().is_none())
            && long_capture_pipeline::active_capture_threads() == 0
            && long_capture_runtime::active_workers() == 0
        {
            break;
        }
        ensure(Instant::now() < end, "32k关闭后实体/源/预览/线程未释放")?;
        tick(cx).await;
    }
    source_gone(&report.sources.weaks, cx).await?;
    owner.retire_closed_border_hud()?;
    ensure(
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<Host>().is_some())
        })?,
        "32k关闭后GPUI窗口残留",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let pin_close_end = unix_ms()?;
    let lifecycle_closed = ledger.snapshot();
    ensure(
        lifecycle_closed.view_drop_observed
            && lifecycle_closed.current_image_id.is_none()
            && lifecycle_closed.current_bgra_capacity_bytes == 0,
        "32k右预览View未析构",
    )?;
    let private = cpu_memory()?["private_bytes"]
        .as_u64()
        .ok_or("runtime private缺失")?;
    report.close_private.push(private);
    let revoke = request(bridge, "revoke_lease", json!({}), cx).await?;
    ensure(revoke["lease_revoked"] == true, "正常关闭后未撤销旧lease")?;
    let mut handles = capture_handles;
    handles.extend(pin_handles.iter().copied());
    let mut gpui_handles = vec![panel_hwnd, preview_hwnd];
    gpui_handles.extend(pin_handles.iter().copied());
    let detail = json!({"spec":spec,"active_generation":active_generation,"paused_generation":finished.generation,
        "actual_dom_top":final_top,"posts":report.posts-posts_before,"quiet_ack":quiet_ack,
        "pause_monitor_ms":pause_monitor_ms,"posts_at_pause":pause_posts,"posts_after_pause":pause_posts,
        "full_png":full_png,"full_check":full_check,"preview":final_preview,"lifecycle_before_close":lifecycle_before_close,
        "lifecycle_closed":lifecycle_closed,"processing":processing,
        "close":{"begin_unix_ms":panel_close_begin,"end_unix_ms":pin_close_end,"private_bytes":private,
            "closed_hwnds":handles,"gpui_hwnds":gpui_handles,"entities_gone":true,"all_observed_weaks_expired":true,
            "observed_source_weaks":report.sources.weaks.len(),"observed_render_weaks":report.render_weaks.len(),
            "observed_preview_source_weaks":report.preview_sources.len(),"capture_threads":0,"consumer_sessions":0,
            "settle_after_resources_ms":500,"force_cleanup":false,"window_intervals":[
                {"hwnds":[panel_hwnd,preview_hwnd],"begin_unix_ms":panel_close_begin,"end_unix_ms":panel_close_end},
                {"hwnds":pin_handles,"begin_unix_ms":pin_close_begin,"end_unix_ms":pin_close_end}]}});
    report.cases.push(json!({"cycle":cycle,"mode":format!("cycle-{cycle}-native-auto-32000-pin-close"),"ok":true,"detail":detail}));
    report.stage("cycle-close", report.cases.last().cloned().unwrap())?;
    Ok(())
}

async fn verify(bridge: &SharedBridge, report: &mut NativeReport, cx: &mut AsyncApp) -> Result<()> {
    report.stage(
        "gpui-hidden-host-baseline",
        json!({"hidden_host":true,"whole_reference_bgra_bytes":0}),
    )?;
    for round in 1..=3 {
        if round > 1 {
            let reset = request(
                bridge,
                "reset",
                json!({"cycle":round,"previous_cycle_closed":true,
                "capture_threads_after_close":0,"consumer_sessions_after_close":0}),
                cx,
            )
            .await?;
            ensure(
                reset["reset_is_fixture_preparation"] == true && reset["state"]["scroll_top"] == 0,
                "轮间reset未settle",
            )?;
            let lease = request(bridge, "new_lease", json!({}), cx).await?;
            ensure(lease["state"]["scroll_top"] == 0, "轮间显式新lease非法")?;
            report.owner.client_gate()?()?;
        }
        cycle(bridge, round, report, cx).await?;
    }
    ensure(report.close_private.len() == 3, "原生32k未实际完成三轮")?;
    let growth = report.close_private[2].saturating_sub(report.close_private[0]);
    report.stage(
        "three-cycle-close-stability",
        json!({"close_private_bytes":report.close_private,
        "growth_bytes":growth,"limit_bytes":CLOSE_STABILITY_BYTES,"pre_registered":true}),
    )?;
    ensure(
        growth <= CLOSE_STABILITY_BYTES,
        "三轮原生32k关闭后runtime private增长超过原预设16MiB",
    )
}

pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let bridge = Bridge::new()?;
    let (owner, config) = bridge.initial()?;
    let record = NativeReport::new(owner, &config)?;
    let data = PathBuf::from(std::env::var_os("RUSTSHOT_DATA_DIR").ok_or("隔离data目录缺失")?)
        .canonicalize()?;
    ensure(
        data.starts_with(&record.root) && data != record.root,
        "32k偏好目录越界",
    )?;
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    let bridge = Arc::new(Mutex::new(bridge));
    let report = Rc::new(RefCell::new(Some(record)));
    let final_report = report.clone();
    let failure = Rc::new(RefCell::new(Some("原生32k三轮尚未完成".to_owned())));
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
                let mut record = report.borrow_mut().take().expect("原生32k独占report");
                let mut error = verify(&bridge, &mut record, cx)
                    .await
                    .err()
                    .map(|e| e.to_string());
                if error.is_some() {
                    let _ = record.save(false, error.as_deref());
                    record.owner.revoke("native stress failure cleanup");
                    let _ = cx.update(|cx| {
                        for w in cx.windows() {
                            if w.downcast::<Host>().is_none() {
                                let _ = cx.update_window(w, |_, w, _| w.remove_window());
                            }
                        }
                    });
                }
                let cleanup = async {
                    request(&bridge, "close_browser", json!({}), cx).await?;
                    native_gone_browser(record.owner.config().hwnd, cx).await?;
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
                    record.protocol = bridge.lock().unwrap_or_else(|e| e.into_inner()).probe();
                    ensure(
                        record.protocol["reader_joined"] == true
                            && record.protocol["disconnected"] == true,
                        "原生32k协议没有正常EOF/join",
                    )
                }
                .await;
                if let Err(e) = cleanup {
                    error = Some(error.map_or_else(|| e.to_string(), |old| format!("{old}; {e}")));
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
    if let Some(mut record) = final_report.borrow_mut().take() {
        record.save(true, error.as_deref())?;
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}
