//! 自有内嵌音频窗口只观察生产提交回执，不补造音频时钟或呈现证据。
use super::*;
use std::collections::BTreeSet;
use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP};

pub(super) fn synthetic_audio(video_frame: u8) -> Vec<i16> {
    let first = u64::from(video_frame) * 4_800;
    let mut pcm = Vec::with_capacity(4_800 * 2);
    for offset in 0..4_800_u64 {
        let time = (first + offset) as f64 / 48_000.;
        for frequency in [440., 660.] {
            let value = (std::f64::consts::TAU * frequency * time).sin() * 0.1;
            pcm.push((value * f64::from(i16::MAX)) as i16);
        }
    }
    pcm
}

pub(super) struct Observation {
    frame: Painted,
    accepted: u64,
    rate: f32,
    audio_info: Option<Vec<(u32, usize)>>,
    report: serde_json::Value,
}

pub(super) struct PauseClockObservation {
    pub(super) ready: bool,
    pub(super) diagnostic: serde_json::Value,
}

/// 当前代设备已暂停后，还要等视频工作者消费同一个结算位置，才可建立冻结基准。
pub(super) fn pause_clock_observation(
    window: WindowHandle<VideoView>,
    cx: &mut AsyncApp,
) -> Result<PauseClockObservation> {
    window.update(cx, |view, _, _| -> Result<_> {
        let player = view.player.as_ref().ok_or("暂停时钟检查缺少播放器")?;
        if let Some(error) = player.audio_error() {
            return Err(format!("暂停时钟检查的音频不可用：{error}").into());
        }
        let audio_info = player.audio_info();
        let qpc = crate::media_playback_sync::qpc_now_100ns();
        // audio_clock_at 已在 Player 内核对当前 transport generation，旧代不能确认暂停。
        let clock = qpc.and_then(|qpc| player.audio_clock_at(qpc));
        let (position, duration) = player.position();
        let playing = player.playing();
        let seeking = player.seeking();
        let ready = !playing
            && !seeking
            && match audio_info.as_deref() {
                Some([]) => true,
                Some(_) => clock.is_some_and(|published| {
                    published.clock.phase == crate::preview_audio_clock::ClockPhase::Paused
                        && !published.clock.advancing
                        && published.clock.media_position == position
                }),
                None => false,
            };
        Ok(PauseClockObservation {
            ready,
            diagnostic: serde_json::json!({
                "ready":ready,"source_generation":view.source_generation,
                "position_ns":position.as_nanos().to_string(),
                "duration_ns":duration.as_nanos().to_string(),
                "playing":playing,"seeking":seeking,"audio_info":audio_info,
                "audio_switching":player.audio_switching(),"qpc_100ns":qpc,
                "clock":clock.map(|published| serde_json::json!({
                    "transport_generation":published.transport_generation,
                    "epoch":published.clock.generation,
                    "phase":format!("{:?}",published.clock.phase),
                    "advancing":published.clock.advancing,
                    "position_ns":published.clock.media_position.as_nanos().to_string(),
                    "queued_ns":published.clock.queued_media.as_nanos().to_string(),
                    "qpc_100ns":published.clock.qpc_100ns,
                })),
            }),
        })
    })?
}

fn accepted(report: &serde_json::Value, stage: &str) -> Result<u64> {
    let mut total = 0_u64;
    for row in report["groups"].as_array().ok_or("同步报告缺少分组")? {
        if row["stage"].as_str() == Some(stage) {
            total = total
                .checked_add(
                    row["accepted_total"]
                        .as_u64()
                        .ok_or("同步报告缺少累计样本数")?,
                )
                .ok_or("同步累计样本数溢出")?;
        }
    }
    Ok(total)
}

pub(super) fn capture(window: WindowHandle<VideoView>, cx: &mut AsyncApp) -> Result<Observation> {
    let frame = painted(window, cx)?.ok_or("内嵌音频检查缺少真实提交帧")?;
    window.update(cx, |view, _, _| -> Result<_> {
        ensure(
            view.muted && view.volume == 0.,
            "内嵌音频检查失去静音或零音量",
        )?;
        let player = view.player.as_ref().ok_or("内嵌音频检查缺少播放器")?;
        if let Some(error) = player.audio_error() {
            return Err(format!("内嵌音频不可用：{error}").into());
        }
        let audio_info = player.audio_info();
        if let Some(tracks) = &audio_info {
            ensure(
                tracks.as_slice() == [(48_000, 2)],
                "内嵌AAC没有解码为预期48kHz双声道音轨",
            )?;
        }
        let report = player.sync_report();
        ensure(
            accepted(&report, "presented")? == 0,
            "提交检查不能生成Presented证据",
        )?;
        report["observations_without_aligned_audio_clock"]
            .as_u64()
            .ok_or("同步报告缺少无时钟观测数")?;
        Ok(Observation {
            frame,
            accepted: accepted(&report, "submitted")?,
            rate: player.rate,
            audio_info,
            report,
        })
    })?
}

fn same_source(observed: &Painted, expected: &Painted) -> Result<()> {
    ensure(
        observed.generation == expected.generation && observed.path == expected.path,
        "音频提交阶段的视频来源发生变化",
    )
}

fn next_fixture_pts(pts: Duration) -> Result<Option<Duration>> {
    ensure(
        pts < Duration::from_secs(3) && pts.as_nanos() % 100_000_000 == 0,
        "提交PTS不在自有10fps时间线上",
    )?;
    Ok((pts < Duration::from_millis(2_900)).then(|| pts + Duration::from_millis(100)))
}

async fn collect(
    window: WindowHandle<VideoView>,
    before: Observation,
    label: &str,
    control: &str,
    expected_rate: f32,
    cx: &mut AsyncApp,
) -> Result<(Observation, serde_json::Value)> {
    let deadline = Instant::now() + WAIT;
    let mut last_serial = before.frame.serial;
    let mut last_pts = before.frame.pts;
    let mut last_accepted = before.accepted;
    let mut serials = BTreeSet::new();
    let mut submitted_pts = BTreeSet::new();
    let mut points = Vec::new();
    loop {
        let observed = capture(window, cx)?;
        same_source(&observed.frame, &before.frame)?;
        ensure(
            observed.accepted >= last_accepted,
            "同一来源累计提交样本数倒退",
        )?;
        if observed.frame.serial != last_serial {
            ensure(
                observed.frame.serial > last_serial,
                "同一音频提交阶段serial倒退",
            )?;
            ensure(observed.frame.pts >= last_pts, "同一音频提交阶段PTS倒退")?;
            next_fixture_pts(observed.frame.pts)?;
            last_serial = observed.frame.serial;
            last_pts = observed.frame.pts;
        }
        if observed.accepted > last_accepted
            && observed.frame.serial != before.frame.serial
            && observed.frame.playing
            && !observed.frame.seeking
            && observed.rate == expected_rate
            && observed.audio_info.is_some()
            && serials.insert(observed.frame.serial)
        {
            submitted_pts.insert(observed.frame.pts);
            points.push(serde_json::json!({
                "serial":observed.frame.serial,"pts_ms":observed.frame.pts.as_millis(),
                "fixture_next_pts_ms":next_fixture_pts(observed.frame.pts)?.map(|pts|pts.as_millis()),
            }));
        }
        last_accepted = observed.accepted;
        let added = observed.accepted - before.accepted;
        if added >= 3 && serials.len() >= 3 && submitted_pts.len() >= 3 {
            let evidence = serde_json::json!({
                "phase":label,"control":control,"fixture_fps":10,
                "source_generation":observed.frame.generation,"playing":observed.frame.playing,
                "rate":observed.rate,"audio_info":observed.audio_info,
                "submitted_accepted_before":before.accepted,"submitted_accepted_after":observed.accepted,
                "new_submitted_accepted":added,"observed_submitted_points":points,
                "distinct_submitted_pts":submitted_pts.len(),
                "sync_before":before.report,"sync_after":observed.report,
                "presented_accepted":accepted(&observed.report,"presented")?,
                "presented_verified":false,"audible_playback_verified":false,
                "sync_quality_threshold_applied":false,
            });
            return Ok((observed, evidence));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{label}未取得3个新音频时钟对齐的真实提交帧：{}",
                observed.report
            )
            .into());
        }
        tick(cx).await;
    }
}

pub(super) async fn collect_playing(
    window: WindowHandle<VideoView>,
    before: Observation,
    label: &str,
    control: &str,
    expected_rate: f32,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    collect(window, before, label, control, expected_rate, cx)
        .await
        .map(|(_, evidence)| evidence)
}

struct SliderHold {
    input: Input,
    point: LPARAM,
    released: bool,
}

impl SliderHold {
    fn begin(input: Input, point: (f32, f32)) -> Result<Self> {
        let (x, y) = (
            (point.0 * input.scale).round(),
            (point.1 * input.scale).round(),
        );
        ensure(
            x.is_finite()
                && y.is_finite()
                && x >= 0.
                && y >= 0.
                && x <= f32::from(i16::MAX)
                && y <= f32::from(i16::MAX),
            "进度条原生坐标超出消息范围",
        )?;
        let point = LPARAM((((y as u32) << 16) | x as u32) as isize);
        unsafe {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_LBUTTONDOWN,
                WPARAM(1),
                point,
            )?;
        }
        Ok(Self {
            input,
            point,
            released: false,
        })
    }

    fn release(&mut self) -> Result<()> {
        unsafe {
            PostMessageW(
                Some(HWND(self.input.handle as *mut _)),
                WM_LBUTTONUP,
                WPARAM(0),
                self.point,
            )?;
        }
        self.released = true;
        Ok(())
    }
}

impl Drop for SliderHold {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release();
        }
    }
}

pub(super) async fn exercise_controls(
    window: WindowHandle<VideoView>,
    input: Input,
    stepped: &Painted,
    cx: &mut AsyncApp,
) -> Result<(Vec<serde_json::Value>, u32)> {
    let after_step = capture(window, cx)?;
    same_source(&after_step.frame, stepped)?;
    ensure(
        !after_step.frame.playing && !after_step.frame.seeking,
        "逐帧后没有保持暂停",
    )?;
    // 保留逐帧原断言，再从片头附近显式回卷，为三段播放观测留下媒体时长。
    let (rewind_target, point, seek_input) = slider_point(window, input, 0.05, cx)?;
    let rewind_floor = Duration::from_millis((rewind_target.as_millis() / 100 * 100) as u64);
    seek_input.click(point);
    wait_frame(
        window,
        cx,
        "音频阶段回卷未提交暂停目标帧",
        |frame| {
            frame.generation == stepped.generation
                && frame.path == stepped.path
                && frame.serial > stepped.serial
                && frame.pts == rewind_floor
                && !frame.playing
                && !frame.seeking
        },
    )
    .await?;
    let rewound = paused_frame(window, cx).await?;
    same_source(&rewound, stepped)?;
    let rewind_next = next_fixture_pts(rewound.pts)?.ok_or("音频回卷目标没有下一帧")?;
    ensure(
        rewound.pts == rewind_floor && rewound.pts <= rewind_target && rewind_next > rewind_target,
        "音频回卷没有提交覆盖目标的实际帧",
    )?;
    let rewind = serde_json::json!({
        "control":"native_slider_click","target_ms":rewind_target.as_secs_f64()*1000.,
        "submitted_pts_ms":rewound.pts.as_millis(),"next_fixture_pts_ms":rewind_next.as_millis(),
        "serial":rewound.serial,"paused_stable_ms":200,
    });
    let before = capture(window, cx)?;
    let mut keys = 0;
    text_contract_smoke::key(input, VK_SPACE, false, false, false)?;
    keys += 1;
    let (_, mut resume) = collect(window, before, "resume", "native_space", 1., cx).await?;
    resume["rewind_anchor"] = rewind;

    let before = capture(window, cx)?;
    window.update(cx, |view, _, cx| -> Result<()> {
        view.player
            .as_mut()
            .ok_or("倍速检查缺少播放器")?
            .set_rate(1.5);
        cx.notify();
        Ok(())
    })??;
    let (_, rate) = collect(
        window,
        before,
        "rate_change",
        "Player::set_rate(1.5)",
        1.5,
        cx,
    )
    .await?;

    let before = capture(window, cx)?;
    ensure(
        before.frame.playing && !before.frame.seeking,
        "播放中定位开始时没有播放",
    )?;
    let (target, point, seek_input) = slider_point(window, input, 0.2, cx)?;
    let floor = Duration::from_millis((target.as_millis() / 100 * 100) as u64);
    let next = next_fixture_pts(floor)?.ok_or("播放中定位目标没有下一帧")?;
    let mut hold = SliderHold::begin(seek_input, point)?;
    let deadline = Instant::now() + WAIT;
    let held = loop {
        let observed = capture(window, cx)?;
        same_source(&observed.frame, &before.frame)?;
        let held = window.update(cx, |view, _, _| {
            matches!(view.scrub, Some(Scrub::Seek)) && view.seek_resume
        })?;
        if held
            && !observed.frame.playing
            && !observed.frame.seeking
            && observed.frame.serial != before.frame.serial
            && observed.frame.pts == floor
        {
            ensure(
                observed.frame.pts <= target && next > target,
                "按住进度条时没有提交覆盖目标的实际帧",
            )?;
            break observed;
        }
        ensure(
            Instant::now() < deadline,
            "播放中按住进度条后未暂停并提交目标帧",
        )?;
        tick(cx).await;
    };
    let held_evidence = serde_json::json!({
        "target_ms":target.as_secs_f64()*1000.,"submitted_pts_ms":held.frame.pts.as_millis(),
        "next_fixture_pts_ms":next.as_millis(),"serial":held.frame.serial,
        "paused_while_mouse_down":!held.frame.playing,"seek_resume_intent":true,
        "sync":held.report,
    });
    hold.release()?;
    let (_, mut seek) = collect(
        window,
        held,
        "playing_seek",
        "native_slider_down_hold_up",
        1.5,
        cx,
    )
    .await?;
    seek["held_target"] = held_evidence;
    let before_pause = capture(window, cx)?;
    text_contract_smoke::key(input, VK_SPACE, false, false, false)?;
    keys += 1;
    let paused = paused_frame(window, cx).await?;
    same_source(&paused, &before_pause.frame)?;
    capture(window, cx)?;
    Ok((vec![resume, rate, seek], keys))
}
