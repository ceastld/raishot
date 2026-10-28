//! 实际两级工具栏和保留边框的原生 Auto 验收；DOM 控制器仅观察。
use super::*;
use crate::{long_auto::AutoPhase, long_browser_oracle as short_oracle};
use gpui::RenderImage;
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE},
};

const GROUP: &str = "browser-ui-native-auto";
const CSS_HEIGHT: u32 = 1440;

fn read_renderer(response: &Value, root: &Path) -> Result<Image> {
    let descriptor = &response["renderer"];
    let path = PathBuf::from(
        descriptor["path"]
            .as_str()
            .ok_or("UI renderer PNG路径缺失")?,
    )
    .canonicalize()?;
    ensure(
        path.starts_with(root)
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            && std::fs::metadata(&path)?.len() <= 8 * 1024 * 1024,
        "UI renderer PNG越界或过大",
    )?;
    ensure(
        descriptor["sha256"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "UI renderer hash缺失",
    )?;
    let image = crate::export::read_image(&path)?;
    ensure(
        descriptor["pixel_width"].as_u64() == Some(u64::from(image.width))
            && descriptor["pixel_height"].as_u64() == Some(u64::from(image.height)),
        "UI renderer实际尺寸与描述不符",
    )?;
    Ok(image)
}

struct UiReport {
    root: PathBuf,
    owner: Arc<BrowserCaptureOwner>,
    cases: Vec<Value>,
    stages: Vec<Value>,
    events: Vec<Value>,
    observations: Vec<Value>,
    sequence: u64,
    posts: usize,
    fences: HashMap<u64, u64>,
    sources: SourceTrace,
    render_weaks: Vec<Weak<RenderImage>>,
    preview_sources: Vec<Weak<Image>>,
    protocol: Value,
}
impl UiReport {
    fn new(owner: Arc<BrowserCaptureOwner>, config: &Value) -> Result<Self> {
        ensure(
            config["mode"] == "native-auto" && config["scenario"] == "ui",
            "UI mode/scenario 不符",
        )?;
        ensure(
            std::env::var("RUSTSHOT_BROWSER_QA_MODE").as_deref() == Ok("native-auto"),
            "UI mode 环境不符",
        )?;
        let root = PathBuf::from(
            config["artifact_root"]
                .as_str()
                .ok_or("缺少 UI artifact_root")?,
        )
        .canonicalize()?;
        let base = std::env::current_dir()?.join("target/qa").canonicalize()?;
        ensure(root.starts_with(&base) && root != base, "UI记录目录越界")?;
        Ok(Self {
            root,
            owner,
            cases: vec![],
            stages: vec![],
            events: vec![],
            observations: vec![],
            sequence: 0,
            posts: 0,
            fences: HashMap::new(),
            sources: SourceTrace::default(),
            render_weaks: vec![],
            preview_sources: vec![],
            protocol: Value::Null,
        })
    }
    fn save(&self, complete: bool, error: Option<&str>) -> Result<()> {
        let ok = complete && error.is_none();
        let groups = if ok {
            vec![json!({"name":GROUP,"ok":true,"cases":self.cases})]
        } else {
            vec![]
        };
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&json!({
                "suite":"long-browser","mode":"native-auto","scenario":"ui","pid":std::process::id(),"ok":ok,"error":error,
                "requested_groups":[GROUP],"groups":groups,"stages":self.stages,"native_owner":self.owner.native_snapshot(),"ownership":self.owner.probe(),
                "real_browser_compositor":true,"real_dxgi_capture":ok,"full_desktop_freeze_used":false,"fixture_substituted":false,
                "clipboard_untouched":true,"global_input_sent":false,"protocol":self.protocol,"protocol_thread_joined":self.protocol["reader_joined"]==true,
                "capture_threads_after_close":long_capture_pipeline::active_capture_threads(),"consumer_sessions_after_close":long_capture_runtime::active_workers(),
                "production_border_removed_for_strict_gate":false,"gpui_toolbar_auto_verified":ok,"native_posts":self.posts,
                "limitations":["隔离自有 Chrome 的有限六屏页面/当前实际 DPI，未证明32k原生Auto或任意第三方网页",
                    "工具栏输入为原自有HWND定向消息；DOM仅只读观察，不冒称硬件输入",
                    "探针时间是观察时间；暂停ACK不等于浏览器已消费所有此前消息",
                    "drop_image/Weak/rendererDrop仅证明各自生命周期，不直接查询全部COM/atlas驻留"]
            }))?,
        )?;
        std::fs::write(
            self.root.join("native-ui-auto-trace.json"),
            serde_json::to_vec_pretty(&json!({
                "ok":ok,"error":error,"last_sequence":self.sequence,"posts":self.posts,"events":self.events,"observations":self.observations,
                "event_timestamp_is_observation_time":true,"event_limit":4096,"observation_limit":256,"dom_position_inferred_from_wheel":false
            }))?,
        )?;
        Ok(())
    }
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        self.stages.push(
            json!({"stage":name,"unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            "cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail,"ownership":self.owner.probe()}),
        );
        self.save(false, None)
    }
    fn pass(&mut self, name: &str, detail: Value) -> Result<()> {
        self.cases
            .push(json!({"mode":name,"ok":true,"detail":detail}));
        self.stage("subcase-passed", self.cases.last().cloned().unwrap())
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
            let (weak, lifecycle) =
                preview.update(cx, |v, _, _| (v.image_weaks(), v.lifecycle_probe()))?;
            ensure(
                lifecycle.current_bgra_used_bytes <= 4_000_000
                    && lifecycle.current_bgra_capacity_bytes >= lifecycle.current_bgra_used_bytes,
                "UI实时预览缓冲超过预算或容量记录不符",
            )?;
            if let Some(weak) = weak.current {
                self.render_weaks.push(weak);
            }
            self.render_weaks.extend(weak.retired);
            if let Some(weak) = weak.source {
                self.preview_sources.push(weak);
            }
            ensure(
                self.render_weaks.len() < 20000 && self.preview_sources.len() < 20000,
                "UI Weak 观察超过预算",
            )?;
        }
        if let Some(probe) = probe {
            for event in probe.auto_events {
                if event.sequence <= self.sequence {
                    continue;
                }
                ensure(
                    event.sequence == self.sequence + 1 && self.events.len() < 4096,
                    "UI Auto事件缺口或超过预算",
                )?;
                match event.event {
                    "post" => {
                        let fence = event.fence_sequence.ok_or("UI post缺少fence")?;
                        ensure(fence >= event.frame_sequence, "UI post fence早于提交帧")?;
                        self.fences.insert(event.epoch, fence);
                        self.posts += 1;
                    }
                    "observe" => {
                        if let Some(fence) = self.fences.get(&event.epoch) {
                            ensure(
                                event.fence_sequence == Some(*fence)
                                    && event.frame_sequence > *fence,
                                "UI Auto由旧帧确认",
                            )?;
                        }
                    }
                    "ignore-pre-post" => {
                        let fence = self.fences.get(&event.epoch).ok_or("UI ignore早于post")?;
                        ensure(
                            event.fence_sequence == Some(*fence) && event.frame_sequence <= *fence,
                            "UI忽略旧帧fence不符",
                        )?;
                    }
                    "capture-gate-rejected" => return Err("UI真实采集归属被拒绝".into()),
                    _ => return Err("未知UI Auto事件".into()),
                }
                self.sequence = event.sequence;
                self.events.push(json!({"observed_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"event":event}));
            }
        }
        Ok(meta)
    }
    fn observe(&mut self, state: &Value, meta: &LongSnapshot) -> Result<f64> {
        let top = state["scroll_top"].as_f64().ok_or("UI DOM scrollTop缺失")?;
        ensure(
            top.is_finite()
                && (0. ..=f64::from(CSS_HEIGHT - 240)).contains(&top)
                && state["pattern_id"] == "bgra-world-v1"
                && state["observer_mode"] == "native-auto",
            "UI DOM世界/位置不符",
        )?;
        ensure(self.observations.len() < 256, "UI DOM观察超预算")?;
        self.observations.push(json!({"state":state,"generation":meta.generation,"requested_generation":meta.requested_generation,
            "phase":format!("{:?}",meta.phase),"auto_active":meta.auto.active,"auto_phase":format!("{:?}",meta.auto.phase),
            "speed":meta.auto.speed,"steps":meta.auto.steps,"height":meta.height,"viewport_y":meta.viewport.y,"posts":self.posts}));
        Ok(top)
    }
}

async fn wait_ui(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    predicate: impl Fn(&LongSnapshot) -> bool,
    report: &mut UiReport,
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
async fn paused(
    panel: WindowHandle<LongCaptureView>,
    report: &mut UiReport,
    cx: &mut AsyncApp,
) -> Result<LongSnapshot> {
    wait_ui(
        panel,
        "真实按钮暂停",
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
            return Ok(meta);
        }
        ensure(Instant::now() < end, "UI暂停尚未真正排空")?;
        tick(cx).await;
    }
}
async fn notify_paused(
    bridge: &SharedBridge,
    meta: &LongSnapshot,
    cx: &mut AsyncApp,
) -> Result<Value> {
    request(
        bridge,
        "native_auto_phase",
        json!({"phase":"paused","generation":meta.generation,
        "producer_quiet":true,"consumer_quiet":true,"raw_slots":0,"queue_slots":0}),
        cx,
    )
    .await?;
    request(
        bridge,
        "observe_paused",
        json!({"generation":meta.generation}),
        cx,
    )
    .await
}
async fn prepare_active(
    bridge: &SharedBridge,
    panel: WindowHandle<LongCaptureView>,
    cx: &mut AsyncApp,
) -> Result<u64> {
    let generation = panel
        .update(cx, |v, _, _| {
            v.snapshot().requested_generation.checked_add(1)
        })?
        .ok_or("UI代数溢出")?;
    request(
        bridge,
        "native_auto_phase",
        json!({"phase":"active","generation":generation}),
        cx,
    )
    .await?;
    Ok(generation)
}
async fn wait_motion(
    bridge: &SharedBridge,
    panel: WindowHandle<LongCaptureView>,
    report: &mut UiReport,
    previous: f64,
    cx: &mut AsyncApp,
) -> Result<f64> {
    let end = Instant::now() + WAIT;
    loop {
        let response = request(bridge, "observe", json!({}), cx).await?;
        let meta = report.collect(panel, cx)?;
        let top = report.observe(&response["state"], &meta)?;
        if top > previous {
            return Ok(top);
        }
        ensure(
            meta.auto.active && Instant::now() < end,
            "UI Auto未引起真实DOM移动",
        )?;
        tick(cx).await;
    }
}
async fn stable_pause(
    panel: WindowHandle<LongCaptureView>,
    report: &mut UiReport,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let before = report.posts;
    let generation = panel.update(cx, |v, _, _| v.snapshot().generation)?;
    let begin = Instant::now();
    while begin.elapsed() < Duration::from_millis(350) {
        let meta = report.collect(panel, cx)?;
        ensure(
            report.posts == before
                && meta.phase == LongPhase::Paused
                && meta.generation == generation
                && !meta.auto.active,
            "暂停ACK后仍投递或恢复",
        )?;
        tick(cx).await;
    }
    Ok(
        json!({"posts_at_ack":before,"posts_after":report.posts,"generation":generation,
        "monitor_elapsed_ms":begin.elapsed().as_secs_f64()*1000.,"browser_queue_drained_claimed":false}),
    )
}

/// 与 HTML 公开六屏 marker 合同独立计算；没有引用产品预览、拼接或 GPUI 像素作期望。
fn world(image: &Image, spec: short_oracle::Spec, first: u32) -> Result<Value> {
    ensure(
        image.width == spec.width
            && first + image.height <= spec.document_height
            && image.bgra.len() == image.width as usize * image.height as usize * 4,
        "UI world范围非法",
    )?;
    let edge = |css: u32| (f64::from(css) * spec.dpr).round() as u32;
    let mut maximum = 0;
    for y in 0..image.height {
        for x in 0..image.width {
            let wy = first + y;
            let band = (0..6)
                .find(|&b| wy >= edge(b * 240) && wy < edge((b + 1) * 240))
                .ok_or("world band越界")?;
            let phase = band % 3;
            let rgba = if x < edge(12) && wy < edge(band * 240 + 12) {
                Some([251, 19, 73, 255])
            } else if x >= edge(308) && wy < edge(band * 240 + 12) {
                Some([17, 239, 101, 255])
            } else if x < edge(12) && wy >= edge(band * 240 + 228) {
                Some([37, 89, 247, 255])
            } else if x >= edge(308) && wy >= edge(band * 240 + 228) {
                Some([233, 181, 29, 255])
            } else if x >= edge(12) && x < edge(308) && wy < edge(band * 240 + 2) {
                Some([
                    (90 + phase * 31) as u8,
                    (40 + phase * 47) as u8,
                    (170 - phase * 29) as u8,
                    255,
                ])
            } else {
                None
            };
            let expected = if let Some([r, g, b, a]) = rgba {
                [b, g, r, a]
            } else {
                let n = wy.wrapping_mul(157).wrapping_add(x.wrapping_mul(71))
                    ^ wy.wrapping_mul(17).wrapping_add(x.wrapping_mul(113));
                [
                    (n % 251) as u8,
                    (wy.wrapping_mul(13).wrapping_add(x.wrapping_mul(19)) % 251) as u8,
                    (wy.wrapping_mul(wy).wrapping_add(x.wrapping_mul(41)) % 251) as u8,
                    255,
                ]
            };
            let i = (y as usize * image.width as usize + x as usize) * 4;
            for (c, channel) in expected.iter().enumerate() {
                let diff = image.bgra[i + c].abs_diff(*channel);
                maximum = maximum.max(diff);
                if diff > 1 || image.bgra[i + 3] != 255 {
                    return Err(format!("UI world像素不符({x},{wy}),channel{c},diff{diff}").into());
                }
            }
        }
    }
    Ok(
        json!({"pixels":u64::from(image.width)*u64::from(image.height),"max_channel_difference":maximum,"tolerance":1,"all_alpha_verified":true}),
    )
}

async fn verify(bridge: &SharedBridge, report: &mut UiReport, cx: &mut AsyncApp) -> Result<()> {
    let owner = report.owner.clone();
    let permit = owner.calibration_permit()?;
    let initial = request(bridge, "calibrate", json!({}), cx).await?;
    let mut spec = short_oracle::Spec::new(initial["state"]["dpr"].as_f64().ok_or("DPR缺失")?)?;
    spec.document_height = (f64::from(CSS_HEIGHT) * spec.dpr).round() as u32;
    ensure(
        initial["state"]["scroll_top"] == 0 && spec.document_height <= 2048,
        "UI首帧/DPR尺寸不符",
    )?;
    let renderer = read_renderer(&initial, &report.root)?;
    let client_region = owner.client_region();
    let gate = permit.gate().clone();
    let (region, calibration) = cx
        .background_executor()
        .spawn(async move {
            let client = read_owned(client_region, gate).map_err(|e| e.to_string())?;
            let calibration =
                short_oracle::calibrate(&client, &renderer, spec, 1).map_err(|e| e.to_string())?;
            Ok::<_, String>((
                ScreenRect {
                    left: client_region.left
                        + i32::try_from(calibration.offset_left).map_err(|e| e.to_string())?,
                    top: client_region.top
                        + i32::try_from(calibration.offset_top).map_err(|e| e.to_string())?,
                    width: spec.width,
                    height: spec.viewport_height,
                },
                calibration,
            ))
        })
        .await
        .map_err(|e| e.to_string())?;
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
        let descriptor = panel.update(cx, |v, _, _| {
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
        if let Some((descriptor, handles)) = descriptor {
            ensure(
                panel.update(cx, |v, _, _| v.session.is_none())?,
                "原Border登记时采集已经开始",
            )?;
            owner.register_border_hud(descriptor)?;
            break handles;
        }
        ensure(
            panel.update(cx, |v, _, _| v.session.is_none())? && Instant::now() < end,
            "UI未在首次采集前登记原Border",
        )?;
        tick(cx).await;
    };
    wait_ui(
        panel,
        "保留边框的真实首帧",
        |m| committed(m) && m.phase == LongPhase::Running && m.height == spec.viewport_height,
        report,
        cx,
    )
    .await?;
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
        "UI Auto未绑定原浏览器内容窗口",
    )?;
    let initial_preview = preview_presented(panel, region, cx).await?;
    let preview = panel.update(cx, |v, _, _| v.preview_window.ok_or("缺少右预览"))??;
    let (ledger, preview_entity) = preview.update(cx, |v, _, cx| {
        (v.lifecycle_ledger(), cx.entity().downgrade())
    })?;
    let panel_entity = panel.update(cx, |_, _, cx| cx.entity().downgrade())?;
    report.pass("owned-border-and-initial-preview",json!({"region":rect_json(region),"calibration":calibration,"target":target,
        "border_hwnds":border_handles,"preview":initial_preview,"lifecycle":ledger.snapshot(),"same_read_and_bind_permit":true}))?;

    panel_button(panel, "long-settings", cx).await?;
    let paused_initial = paused(panel, report, cx).await?;
    panel_button(panel, "long-settings", cx).await?;
    wait_ui(
        panel,
        "设置面板关闭后恢复采集",
        |m| committed(m) && m.phase == LongPhase::Running,
        report,
        cx,
    )
    .await?;
    let slow_generation = prepare_active(bridge, panel, cx).await?;
    panel_button(panel, "long-auto", cx).await?;
    let slow = wait_ui(
        panel,
        "一级Auto启动自适应滚动",
        |m| {
            committed(m)
                && m.generation == slow_generation
                && m.auto.active
                && (1.0..=10.0).contains(&m.auto.speed)
        },
        report,
        cx,
    )
    .await?;
    let slow_top = wait_motion(bridge, panel, report, 0., cx).await?;
    report.pass("primary-adaptive-auto-after-settings",json!({"initial_pause_generation":paused_initial.generation,"generation":slow.generation,"speed":slow.auto.speed,"actual_dom_top":slow_top}))?;

    panel_button(panel, "long-settings", cx).await?;
    let popup_pause = paused(panel, report, cx).await?;
    let response = notify_paused(bridge, &popup_pause, cx).await?;
    let popup_top = report.observe(&response["state"], &popup_pause)?;
    let popup_stable = stable_pause(panel, report, cx).await?;
    let fast_generation = prepare_active(bridge, panel, cx).await?;
    panel_button(panel, "long-settings", cx).await?;
    let fast = wait_ui(
        panel,
        "关闭二级面板恢复自适应滚动",
        |m| {
            committed(m)
                && m.generation == fast_generation
                && m.auto.active
                && (1.0..=10.0).contains(&m.auto.speed)
        },
        report,
        cx,
    )
    .await?;
    let fast_top = wait_motion(bridge, panel, report, popup_top, cx).await?;
    let same_target = panel.update(cx, |v, _, _| {
        v.session.as_ref().ok_or("session缺失")?.auto_target_probe()
    })??;
    ensure(same_target == target, "设置恢复重新绑定了滚动目标")?;
    report.pass("settings-popup-pauses-and-resumes-original-target",json!({"pause_generation":popup_pause.generation,"pause":popup_stable,
        "generation":fast.generation,"speed":fast.auto.speed,"actual_dom_before":popup_top,"actual_dom_after":fast_top,"same_target":same_target}))?;

    panel_button(panel, "long-pause", cx).await?;
    let button_pause = paused(panel, report, cx).await?;
    let response = notify_paused(bridge, &button_pause, cx).await?;
    report.observe(&response["state"], &button_pause)?;
    let stable = stable_pause(panel, report, cx).await?;
    let final_generation = prepare_active(bridge, panel, cx).await?;
    panel_button(panel, "long-auto", cx).await?;
    wait_ui(
        panel,
        "一级Auto恢复自适应滚动",
        |m| {
            committed(m)
                && m.generation == final_generation
                && m.auto.active
                && (1.0..=10.0).contains(&m.auto.speed)
        },
        report,
        cx,
    )
    .await?;
    report.pass(
        "primary-pause-and-auto-resume",
        json!({"pause":stable,"resume_generation":final_generation}),
    )?;

    let end = Instant::now() + WAIT;
    let mut next_observe = Instant::now();
    loop {
        let meta = report.collect(panel, cx)?;
        if Instant::now() >= next_observe {
            let response = request(bridge, "observe", json!({}), cx).await?;
            report.observe(&response["state"], &meta)?;
            next_observe = Instant::now() + Duration::from_millis(150);
        }
        if !meta.auto.active {
            ensure(
                meta.phase == LongPhase::Running && meta.auto.phase == AutoPhase::NoMotion,
                "UI Auto未在实际页尾正常停下",
            )?;
            break;
        }
        ensure(Instant::now() < end, "UI Auto未走完整页")?;
        tick(cx).await;
    }
    panel_button(panel, "long-pause", cx).await?;
    let finished = paused(panel, report, cx).await?;
    let response = notify_paused(bridge, &finished, cx).await?;
    let final_top = report.observe(&response["state"], &finished)?;
    ensure(
        final_top == f64::from(CSS_HEIGHT - 240)
            && finished.height == spec.document_height
            && finished.viewport.y == spec.document_height - spec.viewport_height,
        "UI真实DOM与成品范围不一致",
    )?;
    let full = panel.update(cx, |v, _, _| v.stitch_snapshot().ok_or("页尾没有成品"))??;
    let tail = read_renderer(&response, &report.root)?;
    ensure(
        tail.width == spec.width && tail.height == spec.viewport_height,
        "UI尾部renderer未覆盖完整视口",
    )?;
    let output = report.root.join("native-ui-auto-full.png");
    let full_check = cx
        .background_executor()
        .spawn(async move {
            let image = crate::long_document::render_region(
                &full,
                crate::geometry::Rect::full(full.width, full.height),
                full.width,
                full.height,
            )
            .map_err(|e| e.to_string())?;
            let world = world(&image, spec, 0).map_err(|e| e.to_string())?;
            let tail_check = short_oracle::region(
                &image,
                &tail,
                (0, spec.document_height - spec.viewport_height),
                1,
            )
            .map_err(|e| e.to_string())?;
            crate::export::save_png(image, &output).map_err(|e| e.to_string())?;
            Ok::<_, String>(json!({"world":world,"tail_renderer":tail_check,"output":output}))
        })
        .await
        .map_err(|e| e.to_string())?;
    let final_preview = preview_presented(panel, region, cx).await?;
    report.collect(panel, cx)?;
    let lifecycle = ledger.snapshot();
    ensure(
        lifecycle.render_images_created > 1
            && lifecycle.drop_image_successes > 0
            && lifecycle.drop_image_failures == 0
            && lifecycle.retired_images == 0
            && lifecycle.render_images_created - lifecycle.drop_image_successes == 1
            && lifecycle.current_bgra_used_bytes <= 4_000_000,
        "UI右预览退休或预算不符",
    )?;
    report.pass("full-output-and-preview-retirement",json!({"actual_spec":{"width":spec.width,"viewport_height":spec.viewport_height,"document_height":spec.document_height,"dpr":spec.dpr},
        "full_check":full_check,"final_dom_top":final_top,"preview":final_preview,"lifecycle":lifecycle,"posts":report.posts}))?;
    let panel_hwnd = panel.update(cx, |_, w, _| hwnd(w).0 as usize)?;
    let preview_hwnd = preview.update(cx, |_, w, _| hwnd(w).0 as usize)?;
    let close_begin = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    unsafe {
        PostMessageW(
            Some(HWND(panel_hwnd as *mut _)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        )?;
    }
    let mut handles = border_handles;
    handles.extend([panel_hwnd, preview_hwnd]);
    native_gone(&handles, cx).await?;
    let end = Instant::now() + WAIT;
    loop {
        let gone = panel_entity.upgrade().is_none()
            && preview_entity.upgrade().is_none()
            && report.render_weaks.iter().all(|w| w.upgrade().is_none())
            && report.preview_sources.iter().all(|w| w.upgrade().is_none())
            && long_capture_pipeline::active_capture_threads() == 0
            && long_capture_runtime::active_workers() == 0;
        if gone {
            break;
        }
        ensure(Instant::now() < end, "UI实体/预览弱引用/采集线程未释放")?;
        tick(cx).await;
    }
    source_gone(&report.sources.weaks, cx).await?;
    owner.retire_closed_border_hud()?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let closed = ledger.snapshot();
    ensure(
        closed.view_drop_observed
            && closed.current_image_id.is_none()
            && closed.current_bgra_capacity_bytes == 0,
        "右预览View未析构",
    )?;
    report.pass("ui-windows-sources-and-workers-released",json!({"closed_hwnds":handles,"close_begin_unix_ms":close_begin,
        "close_end_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"observed_source_weaks":report.sources.weaks.len(),
        "observed_render_weaks":report.render_weaks.len(),"observed_preview_source_weaks":report.preview_sources.len(),
        "all_observed_weaks_expired":true,"entities_gone":true,"lifecycle":closed,"cpu":cpu_memory()?,"gpu":gpu_memory(),"force_cleanup":false}))?;
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let bridge = Bridge::new()?;
    let (owner, config) = bridge.initial()?;
    let record = UiReport::new(owner, &config)?;
    let data = PathBuf::from(std::env::var_os("RUSTSHOT_DATA_DIR").ok_or("UI隔离data目录缺失")?)
        .canonicalize()?;
    ensure(
        data.starts_with(&record.root) && data != record.root,
        "UI偏好路径越界",
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
    let failure = Rc::new(RefCell::new(Some("完整UI Auto专项尚未完成".to_owned())));
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
                let mut record = report.borrow_mut().take().expect("UI独占report");
                let mut error = verify(&bridge, &mut record, cx)
                    .await
                    .err()
                    .map(|e| e.to_string());
                if error.is_some() {
                    let _ = record.save(false, error.as_deref());
                    record.owner.revoke("UI failure cleanup");
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
                        "UI协议没有正常EOF/join",
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
    if let Some(record) = final_report.borrow_mut().take() {
        record.save(true, error.as_deref())?;
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}
