//! 显式启动系统输入检测的原生探针；不请求麦克风、播放或保存媒体。
use super::{AudioInputCheck, InputCheckSnapshot};
use crate::{
    audio_capture::{AudioSource, CaptureSourceSnapshot, CaptureSourceState},
    audio_devices::{resolve_endpoint, EndpointKind, EndpointSelection},
    media_foundation::MfRuntime,
    media_playback_sync::WorkerExit,
    Result,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use windows::Win32::{
    Media::Audio::{
        IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    },
    System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL},
};

const MISSING_ENDPOINT: &str = "rustshot-probe-missing-output-endpoint";
const DEADLINE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(10);
const SCOPE: &str = "explicit_system_input_meter_fixed_endpoint_fallback_and_close";

/// 保持引擎有一个明确的零值流；检测不依赖桌面恰好有其它程序发声。
struct SilentRender {
    client: IAudioClient,
    writer: IAudioRenderClient,
    frames: u32,
    endpoint: String,
}

impl SilentRender {
    fn new() -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let resolved = resolve_endpoint(
                &enumerator,
                EndpointKind::Render,
                &EndpointSelection::Default,
            )?;
            let client: IAudioClient = resolved.device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let initialized =
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None);
            CoTaskMemFree(Some(format.cast()));
            initialized?;
            let render = Self {
                frames: client.GetBufferSize()?,
                writer: client.GetService()?,
                client,
                endpoint: resolved.info.id,
            };
            render.pump()?;
            render.client.Start()?;
            Ok(render)
        }
    }

    fn pump(&self) -> Result<()> {
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let available = self
                .frames
                .checked_sub(padding)
                .ok_or("静音设备 padding 无效")?;
            if available != 0 {
                self.writer.GetBuffer(available)?;
                self.writer
                    .ReleaseBuffer(available, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}

impl Drop for SilentRender {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn details(snapshot: &InputCheckSnapshot) -> String {
    format!(
        "starting={} revision={} error={:?} warnings={:?} sources={:?}",
        snapshot.starting, snapshot.revision, snapshot.error, snapshot.warnings, snapshot.sources
    )
}

fn system_source(snapshot: &InputCheckSnapshot) -> Result<&CaptureSourceSnapshot> {
    ensure(
        snapshot.sources.len() == 1,
        format!("检测必须只有一个系统来源：{}", details(snapshot)),
    )?;
    let source = snapshot.sources.first().ok_or("系统检测来源缺失")?;
    ensure(
        source.source == AudioSource::System,
        format!("检测出现非系统来源：{source:?}"),
    )?;
    if let Some(error) = &source.error {
        return Err(format!("系统输入检测失败：{error}；{source:?}").into());
    }
    ensure(
        source.state != CaptureSourceState::Failed,
        format!("系统来源报告失败：{source:?}"),
    )?;
    Ok(source)
}

fn wait_snapshot(
    check: &AudioInputCheck,
    silent: &SilentRender,
    stage: &str,
    mut ready: impl FnMut(&InputCheckSnapshot) -> Result<bool>,
) -> Result<InputCheckSnapshot> {
    let deadline = Instant::now() + DEADLINE;
    loop {
        silent.pump()?;
        let snapshot = check.snapshot();
        if let Some(error) = &snapshot.error {
            return Err(format!("{stage}失败：{error}；{}", details(&snapshot)).into());
        }
        if !snapshot.starting && ready(&snapshot)? {
            return Ok(snapshot);
        }
        if check.is_closed() {
            return Err(format!("{stage}期间 owner 提前退出：{}", details(&snapshot)).into());
        }
        if Instant::now() >= deadline {
            return Err(format!("{stage}未在三秒内完成：{}", details(&snapshot)).into());
        }
        std::thread::sleep(POLL);
    }
}

fn has_live_level(source: &CaptureSourceSnapshot) -> Result<bool> {
    let Some(level) = source.level else {
        return Ok(false);
    };
    ensure(
        level
            .peak
            .iter()
            .chain(&level.rms)
            .all(|value| value.is_finite() && *value >= 0.),
        format!("系统输入电平包含非有限或负值：{source:?}"),
    )?;
    ensure(
        level
            .rms
            .iter()
            .zip(level.peak)
            .all(|(rms, peak)| *rms <= peak),
        format!("系统输入 RMS 超过对应峰值：{source:?}"),
    )?;
    Ok(source.state == CaptureSourceState::Running
        && source.packets > 0
        && source.total_input_frames > 0
        && level.input_frames > 0
        && level.is_fresh())
}

fn exercise(
    check: &AudioInputCheck,
    requested: &EndpointSelection,
    silent: &SilentRender,
) -> Result<Value> {
    let ready = wait_snapshot(
        check,
        silent,
        "等待系统来源启动与端点回退",
        |snapshot| Ok(system_source(snapshot)?.state == CaptureSourceState::Running),
    )?;
    let source = system_source(&ready)?;
    ensure(
        &source.requested == requested,
        format!("回退改写了请求端点：{source:?}"),
    )?;
    let endpoint = source
        .endpoint
        .as_ref()
        .ok_or_else(|| format!("回退后没有实际端点：{source:?}"))?;
    ensure(
        endpoint.kind == EndpointKind::Render
            && !endpoint.id.is_empty()
            && endpoint.id != MISSING_ENDPOINT
            && endpoint.id == silent.endpoint,
        format!("回退后的系统输出端点身份无效：{source:?}"),
    )?;
    ensure(
        source.fallback.is_some(),
        format!("不存在的固定端点没有报告回退：{source:?}"),
    )?;
    ensure(
        source
            .warning
            .as_ref()
            .is_some_and(|warning| !warning.trim().is_empty()),
        format!("端点回退没有来源警告：{}", details(&ready)),
    )?;

    let first = wait_snapshot(
        check,
        silent,
        "等待真实系统输入包与新鲜电平",
        |snapshot| has_live_level(system_source(snapshot)?),
    )?;
    let first_source = system_source(&first)?;
    let mut previous_counts = (first_source.packets, first_source.total_input_frames);
    let later = wait_snapshot(
        check,
        silent,
        "等待下一真实系统输入包",
        |snapshot| {
            let source = system_source(snapshot)?;
            ensure(
                source.packets >= previous_counts.0
                    && source.total_input_frames >= previous_counts.1,
                format!("系统输入包计数或帧计数倒退：previous={previous_counts:?}；{source:?}"),
            )?;
            previous_counts = (source.packets, source.total_input_frames);
            Ok(source.packets > first_source.packets
                && source.total_input_frames > first_source.total_input_frames
                && has_live_level(source)?)
        },
    )?;
    let later_source = system_source(&later)?;
    ensure(
        &later_source.requested == requested,
        "后续输入包改写了固定端点请求",
    )?;
    ensure(
        later_source.endpoint.as_ref() == Some(endpoint),
        "观测期间实际端点身份变化",
    )?;
    let level = later_source.level.ok_or("后续真实包没有电平")?;
    Ok(json!({
        "ok": true,
        "scope": SCOPE,
        "system_input_requested": true,
        "microphone_requested": false,
        "audible_playback_requested": false,
        "silent_render_stream": true,
        "gui_observed": false,
        "media_written": false,
        "requested_fixed_endpoint_preserved": true,
        "fallback_observed": true,
        "fallback_warning": source.warning,
        "first_packets": first_source.packets,
        "later_packets": later_source.packets,
        "first_input_frames": first_source.total_input_frames,
        "later_input_frames": later_source.total_input_frames,
        "level_age_ms": level.age.as_secs_f64() * 1000.,
        "peak": level.peak,
        "rms": level.rms,
        "silent_packet": level.silent_packet,
    }))
}

fn wait_closed(probe: &WorkerExit) -> Result<Duration> {
    let started = Instant::now();
    while !probe.is_exited() {
        ensure(
            started.elapsed() < DEADLINE,
            "close/drop 后三秒内系统输入 owner 未退出",
        )?;
        std::thread::sleep(POLL);
    }
    let elapsed = started.elapsed();
    ensure(elapsed <= DEADLINE, "系统输入 owner 退出观测超过三秒")?;
    Ok(elapsed)
}

#[test]
#[ignore = "显式检测系统输入端点回退、电平及关闭；绝不请求麦克风，无 GUI 或媒体播放"]
fn native_audio_input_check_system_meter_fallback_and_close() -> Result<()> {
    let _runtime = MfRuntime::new()?;
    let silent = SilentRender::new()?;
    let requested = EndpointSelection::Fixed {
        id: MISSING_ENDPOINT.into(),
    };
    let mut check = AudioInputCheck::start(
        vec![AudioSource::System],
        requested.clone(),
        EndpointSelection::Default,
    )?;
    let probe = check.closed_probe();
    let result = exercise(&check, &requested, &silent);
    let close_started = Instant::now();
    check.close();
    let close_call = close_started.elapsed();
    let frozen = check.snapshot();
    drop(check);
    // 验证失败也等待资源 owner；token 不保留采集对象或 PCM。
    let closed = wait_closed(&probe);
    let result = match (result, closed) {
        (Ok(mut report), Ok(wait)) => {
            ensure(!frozen.starting, "close 后快照仍处于启动中")?;
            report["close_call_ms"] = (close_call.as_secs_f64() * 1000.).into();
            report["close_to_owner_exit_ms"] = ((close_call + wait).as_secs_f64() * 1000.).into();
            report["owner_exited"] = true.into();
            Ok(report)
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(format!("{error}；关闭验证也失败：{close}").into()),
    };
    match result {
        Ok(report) => {
            println!("{report}");
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                json!({"ok":false,"scope":SCOPE,"error":error.to_string()})
            );
            Err(error)
        }
    }
}
