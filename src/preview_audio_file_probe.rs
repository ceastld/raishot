//! 显式运行的文件音轨探针；始终静音，不证明可闻声音或界面呈现。
use super::{AudioStreamSelection, PreviewAudio};
use crate::{
    media_foundation::{MfRuntime, VideoReader},
    preview_audio_clock::ClockPhase,
    preview_audio_device_clock::PublishedClock,
    Result,
};
use serde_json::{json, Value};
use std::{
    fs,
    os::windows::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::MediaFoundation::{MFAudioFormat_AAC, MFMediaType_Audio},
    Storage::FileSystem::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    },
};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn fixture() -> Result<(PathBuf, u32)> {
    let path = PathBuf::from(
        std::env::var_os("RUSTSHOT_AUDIO_SWITCH_FIXTURE")
            .ok_or("请将 RUSTSHOT_AUDIO_SWITCH_FIXTURE 指向已生成的本地 H.264/AAC MP4 小样")?,
    );
    let metadata = fs::metadata(&path)?;
    ensure(metadata.is_file(), "音轨探针小样不是普通文件")?;
    let cloud_flags = FILE_ATTRIBUTE_OFFLINE.0
        | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0;
    ensure(
        metadata.file_attributes() & cloud_flags == 0,
        "音轨探针不读取云端占位文件",
    )?;
    let _runtime = MfRuntime::new()?;
    let reader = VideoReader::open(&path)?;
    if let Some(error) = &reader.info.stream_probe_error {
        return Err(format!("音轨小样的真实流探测未完整成功：{error}").into());
    }
    ensure(
        reader.duration >= Duration::from_secs(2),
        "音轨探针需要至少两秒的小样",
    )?;
    let stream = reader
        .info
        .streams
        .iter()
        .find(|stream| {
            stream.major_type == MFMediaType_Audio && stream.codec == Some(MFAudioFormat_AAC)
        })
        .map(|stream| stream.stream_index)
        .ok_or("音轨小样中没有实际探测到的 AAC 源流")?;
    Ok((path, stream))
}

fn wait_for<T>(
    audio: &PreviewAudio,
    stage: &str,
    expect_error: bool,
    mut ready: impl FnMut(&PreviewAudio) -> Option<T>,
) -> Result<T> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        ensure(
            super::lock_state(&audio.state).muted,
            "探针在播放前后都必须保持静音",
        )?;
        if !expect_error {
            if let Some(error) = audio.error() {
                return Err(format!("{stage}失败：{error}").into());
            }
        }
        if let Some(result) = ready(audio) {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(format!("{stage}未在三秒内完成；当前声音错误：{:?}", audio.error()).into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn wait_paused(
    audio: &PreviewAudio,
    generation: u64,
    origin: Option<Duration>,
) -> Result<PublishedClock> {
    wait_for(audio, "等待声音暂停", false, |audio| {
        let clock = audio.clock()?;
        (!audio.stream_switching()
            && audio.audio_info().is_some_and(|tracks| !tracks.is_empty())
            && clock.transport_generation == generation
            && clock.clock.phase == ClockPhase::Paused
            && !clock.clock.advancing
            && origin.is_none_or(|origin| clock.clock.media_position == origin))
        .then_some(clock)
    })
}

fn wait_playing(audio: &PreviewAudio, generation: u64, origin: Duration) -> Result<PublishedClock> {
    wait_for(audio, "等待真实设备时钟推进", false, |audio| {
        let clock = audio.clock()?;
        (clock.transport_generation == generation
            && clock.clock.advancing
            && matches!(
                clock.clock.phase,
                ClockPhase::Playing | ClockPhase::Draining
            )
            && clock.clock.media_position >= origin + Duration::from_millis(20))
        .then_some(clock)
    })
}

fn exercise(audio: &PreviewAudio, stream: u32) -> Result<Value> {
    // open_file 初始暂停；静音确认之前不发送任何播放命令。
    audio.set_muted(true);
    ensure(
        !super::lock_state(&audio.state).playing,
        "文件音频初始状态必须暂停",
    )?;
    let origin = Duration::from_millis(500);
    audio.select_file_stream(1, AudioStreamSelection::Stream(stream), origin, false, 1.);
    wait_paused(audio, 1, Some(origin))?;
    let formats = audio.audio_info().ok_or("准备音轨后缺少解码格式")?;
    audio.set_transport(1, Some(origin), true, 1.);
    let played = wait_playing(audio, 1, origin)?;

    audio.set_transport(2, None, false, 1.);
    let paused = wait_paused(audio, 2, None)?;
    ensure(
        paused.clock.media_position >= played.clock.media_position,
        "暂停结算导致播放位置回退",
    )?;
    std::thread::sleep(Duration::from_millis(30));
    let frozen = audio.clock().ok_or("暂停期间丢失设备时钟")?;
    ensure(
        frozen.transport_generation == 2
            && frozen.clock.phase == ClockPhase::Paused
            && frozen.clock.media_position == paused.clock.media_position
            && !frozen.clock.advancing,
        "暂停后的媒体时钟未冻结",
    )?;

    audio.select_file_stream(
        3,
        AudioStreamSelection::Disabled,
        paused.clock.media_position,
        true,
        1.,
    );
    wait_for(audio, "禁用文件声音", false, |audio| {
        (!audio.stream_switching()
            && audio.file_selection() == Some(AudioStreamSelection::Disabled)
            && audio.clock().is_none()
            && audio.audio_info().is_some_and(|tracks| tracks.is_empty()))
        .then_some(())
    })?;

    let restore_origin = Duration::from_millis(250);
    audio.select_file_stream(
        4,
        AudioStreamSelection::Stream(u32::MAX),
        restore_origin,
        true,
        1.,
    );
    wait_for(audio, "拒绝不存在的音轨", true, |audio| {
        (!audio.stream_switching()
            && audio.file_selection() == Some(AudioStreamSelection::Stream(u32::MAX))
            && audio.clock().is_none())
        .then(|| audio.error())
        .flatten()
    })?;
    audio.select_file_stream(
        5,
        AudioStreamSelection::Stream(stream),
        restore_origin,
        false,
        1.,
    );
    wait_paused(audio, 5, Some(restore_origin))?;
    ensure(audio.error().is_none(), "重新选择有效音轨后旧错误未清除")?;
    audio.set_transport(5, Some(restore_origin), true, 1.);
    let restored = wait_playing(audio, 5, restore_origin)?;
    ensure(
        audio.file_selection() == Some(AudioStreamSelection::Stream(stream)),
        "恢复后选中的真实流发生变化",
    )?;
    Ok(json!({
        "ok": true, "scope": "muted_aac_track_switch_device_clock_and_close",
        "audible_output": false, "gui_observed": false, "stream_index": stream,
        "decoded_formats": formats, "first_played_ms": played.clock.media_position.as_millis(),
        "paused_ms": paused.clock.media_position.as_millis(),
        "restored_played_ms": restored.clock.media_position.as_millis(),
    }))
}

fn close_diagnostic(
    audio: &PreviewAudio,
    stage: &str,
    started: Instant,
    stop_elapsed: Option<Duration>,
    wait_started: Option<Instant>,
) {
    let state = match audio.state.try_lock() {
        Ok(state) => json!({
            "available": true,
            "playing": state.playing,
            "muted": state.muted,
            "generation": state.generation,
            "transport_generation": state.transport_generation,
            "stream_switching": state.file.as_ref().map(|file| file.switching),
            "clock_phase": state.published.map(|clock| format!("{:?}", clock.clock.phase)),
        }),
        Err(std::sync::TryLockError::WouldBlock) => json!({"available":false,"reason":"locked"}),
        Err(std::sync::TryLockError::Poisoned(_)) => json!({"available":false,"reason":"poisoned"}),
    };
    let error = match audio.error.try_lock() {
        Ok(error) => json!({"available":true,"value":&*error}),
        Err(std::sync::TryLockError::WouldBlock) => json!({"available":false,"reason":"locked"}),
        Err(std::sync::TryLockError::Poisoned(_)) => json!({"available":false,"reason":"poisoned"}),
    };
    eprintln!(
        "{}",
        json!({
            "scope":"muted_aac_close_diagnostic",
            "stage":stage,
            "stop_ms":stop_elapsed.map(|elapsed| elapsed.as_millis()),
            "wait_ms":wait_started.map(|started| started.elapsed().as_millis()),
            "total_ms":started.elapsed().as_millis(),
            "stop_requested":audio.stop.load(std::sync::atomic::Ordering::Acquire),
            "thread_finished":audio.thread.as_ref().map(|thread| thread.is_finished()),
            "thread_joined":stage == "after_join",
            "worker_exit":audio.worker_exit.is_exited(),
            "state":state,
            "error":error,
        })
    );
}

fn stop_and_join(audio: &mut PreviewAudio) -> Result<()> {
    let started = Instant::now();
    let deadline = started + STEP_TIMEOUT;
    close_diagnostic(audio, "before_stop", started, None, None);
    let stop_started = Instant::now();
    audio.stop();
    let stop_elapsed = stop_started.elapsed();
    let wait_started = Instant::now();
    close_diagnostic(
        audio,
        "after_stop",
        started,
        Some(stop_elapsed),
        Some(wait_started),
    );
    while !audio
        .thread
        .as_ref()
        .ok_or("音轨探针缺少后台线程")?
        .is_finished()
    {
        if Instant::now() >= deadline {
            close_diagnostic(
                audio,
                "wait_timeout",
                started,
                Some(stop_elapsed),
                Some(wait_started),
            );
            return Err(format!(
                "停止文件声音后三秒内后台线程仍未退出；stop={}ms，wait={}ms，total={}ms",
                stop_elapsed.as_millis(),
                wait_started.elapsed().as_millis(),
                started.elapsed().as_millis(),
            )
            .into());
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
    // 完成门之前保留原句柄；失败不会把仍在运行的线程提前从所有者中摘走。
    close_diagnostic(
        audio,
        "before_join",
        started,
        Some(stop_elapsed),
        Some(wait_started),
    );
    let thread = audio.thread.take().ok_or("音轨探针缺少后台线程")?;
    if thread.join().is_err() {
        close_diagnostic(
            audio,
            "join_failed",
            started,
            Some(stop_elapsed),
            Some(wait_started),
        );
        return Err("文件声音后台线程意外终止".into());
    }
    let stopped = audio.clock().is_none() && !audio.stream_switching();
    close_diagnostic(
        audio,
        "after_join",
        started,
        Some(stop_elapsed),
        Some(wait_started),
    );
    ensure(stopped, "停止后仍发布声音时钟或切轨状态")?;
    ensure(
        started.elapsed() <= STEP_TIMEOUT,
        format!(
            "停止文件声音的总阶段超过三秒；stop={}ms，wait={}ms，total={}ms",
            stop_elapsed.as_millis(),
            wait_started.elapsed().as_millis(),
            started.elapsed().as_millis(),
        ),
    )
}

#[test]
#[ignore = "显式指定 RUSTSHOT_AUDIO_SWITCH_FIXTURE 并连接默认声音设备；全程静音"]
fn preview_audio_file_probe_muted_aac_switch_failure_recovery_and_close() -> Result<()> {
    let (path, stream) = fixture()?;
    let mut audio = PreviewAudio::open_file(path)?;
    let result = exercise(&audio, stream);
    // 中间步骤失败也先停止并回收，避免检查留下后台设备或解码线程。
    let closed = stop_and_join(&mut audio);
    match (result, closed) {
        (Ok(report), Ok(())) => {
            println!("{report}");
            Ok(())
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close_error)) => {
            Err(format!("{error}；关闭检查也失败：{close_error}").into())
        }
    }
}
