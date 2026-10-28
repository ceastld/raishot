//! 显式长源/4K 流式与释放检查；仅播放末尾几秒，不代表长时间播放或录制漂移验收。
use crate::{
    media_foundation::{
        AudioReader, MfRuntime, Mp4Writer, VideoReader, AUDIO_CHANNELS, AUDIO_RATE,
    },
    recording_preview::{PlaybackShutdownProbe, Player},
    Result,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::Media::MediaFoundation::{
    MFAudioFormat_AAC, MFMediaType_Audio, MFVideoFormat_H264,
};

const WIDTH: u32 = 3_840;
const HEIGHT: u32 = 2_160;
const FPS: u32 = 2;
const SECONDS: u32 = 120;
const PIXEL_BYTES: usize = WIDTH as usize * HEIGHT as usize * 4;
const ENCODE_BUDGET: Duration = Duration::from_secs(180);
const READ_BUDGET: Duration = Duration::from_secs(30);
const ROUND_BUDGET: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(10);

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}
fn budget(started: Instant, limit: Duration, stage: &str) -> Result<()> {
    ensure(
        started.elapsed() < limit,
        format!("4K {stage} 超过 {limit:?} 时间预算"),
    )
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            std::env::temp_dir().join(format!("rustshot-long-4k-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> PathBuf {
        self.0.join("long-4k.mp4")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(&self.0);
    }
}

fn encode(path: &Path, report: &mut Value) -> Result<()> {
    let started = Instant::now();
    let mut writer = Mp4Writer::new(path, WIDTH, HEIGHT, FPS, 4_000_000, true)
        .map_err(|error| format!("3840×2160/2fps H.264/AAC 写入器创建失败（不降尺寸）：{error}"))?;
    let mut pixels = vec![0_u8; PIXEL_BYTES];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[16, 32, 48, 255]);
    }
    let silence = vec![0_i16; (AUDIO_RATE / FPS * AUDIO_CHANNELS) as usize];
    let writing = (|| -> Result<()> {
        for index in 0..SECONDS * FPS {
            budget(started, ENCODE_BUDGET, "流式写入")?;
            // 改动小块以保留可辨帧内容；始终复用同一个完整 BGRA 面。
            for row in 0..64_usize {
                for column in 0..64_usize {
                    let at = (row * WIDTH as usize + column) * 4;
                    pixels[at..at + 4].copy_from_slice(&[index as u8, 32, 48, 255]);
                }
            }
            let time = Duration::from_millis(u64::from(index) * 500);
            writer.write_video(&pixels, WIDTH, time, Duration::from_millis(500))?;
            budget(started, ENCODE_BUDGET, "视频写入")?;
            writer.write_audio(&silence, time)?;
            budget(started, ENCODE_BUDGET, "音频写入")?;
        }
        Ok(())
    })();
    if let Err(error) = writing {
        report["encoder"] = writer.diagnostics().clone();
        return Err(error);
    }
    let (finished, diagnostics) = writer.finish_with_diagnostics();
    report["encoder"] = diagnostics;
    finished.map_err(|error| format!("4K fixture Finalize 失败：{error}"))?;
    budget(started, ENCODE_BUDGET, "编码及封装")?;
    report["fixture_encode_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
    report["fixture_file_bytes"] = fs::metadata(path)?.len().into();
    Ok(())
}

fn inspect(path: &Path) -> Result<Value> {
    let started = Instant::now();
    let mut reader = VideoReader::open(path)?;
    ensure(
        (reader.width, reader.height) == (WIDTH, HEIGHT),
        "真实读取器没有保留 4K 尺寸",
    )?;
    ensure(
        reader.info.codec == MFVideoFormat_H264 && reader.info.stream_probe_error.is_none(),
        "fixture 不是可完整探测的 H.264 源",
    )?;
    ensure(
        reader.duration.abs_diff(Duration::from_secs(120)) <= Duration::from_millis(250),
        "真实容器时长不是 120 秒",
    )?;
    ensure(
        reader.info.streams.iter().any(|stream| {
            stream.major_type == MFMediaType_Audio && stream.codec == Some(MFAudioFormat_AAC)
        }),
        "fixture 没有实际 AAC 源流",
    )?;
    let first = reader.next()?.ok_or("4K fixture 没有真实首帧")?;
    let first_pts = first.time;
    ensure(
        first.time == Duration::ZERO && first.bgra.len() == PIXEL_BYTES,
        "4K 真实首帧 PTS 或像素长度不符",
    )?;
    drop(first);
    reader.seek(Duration::from_millis(115_250))?;
    let mut floor = None;
    let mut future = None;
    for _ in 0..=SECONDS * FPS {
        budget(started, READ_BUDGET, "metadata/尾段独立回读")?;
        let frame = reader.next()?.ok_or("独立回读未找到目标后的真实帧")?;
        ensure(
            frame.bgra.len() == PIXEL_BYTES,
            "独立回读帧不是完整 4K BGRA",
        )?;
        if frame.time <= Duration::from_millis(115_250) {
            floor = Some(frame.time);
        } else {
            future = Some(frame.time);
            break;
        }
    }
    ensure(
        floor == Some(Duration::from_secs(115)) && future == Some(Duration::from_millis(115_500)),
        format!("真实尾段 PTS 不符：floor={floor:?} future={future:?}"),
    )?;
    let mut audio = AudioReader::open(path)?.ok_or("fixture AAC 无法真实解码")?;
    let block = audio.next()?.ok_or("fixture AAC 没有 PCM")?;
    ensure(
        !block.samples.is_empty()
            && block
                .samples
                .iter()
                .all(|value| value.is_finite() && value.abs() < 0.0001),
        "自造 AAC 不是非空的近零 PCM",
    )?;
    budget(started, READ_BUDGET, "独立回读")?;
    Ok(
        json!({"width":reader.width,"height":reader.height,"duration_seconds":reader.duration.as_secs_f64(),
        "first_pts_ms":first_pts.as_secs_f64()*1000.,
        "floor_pts_ms":floor.map(|pts|pts.as_secs_f64()*1000.),
        "next_pts_ms":future.map(|pts|pts.as_secs_f64()*1000.),"pixel_bytes":PIXEL_BYTES,
        "aac_rate":audio.rate,"aac_channels":audio.channels,"aac_first_samples":block.samples.len(),
        "reader":reader.diagnostics(),"inspect_ms":started.elapsed().as_secs_f64()*1000.}),
    )
}

fn health(player: &Player) -> Result<()> {
    ensure(
        player.error().is_none(),
        format!("4K Player 视频失败：{:?}", player.error()),
    )?;
    ensure(
        player.audio_error().is_none(),
        format!("4K Player 音频失败：{:?}", player.audio_error()),
    )
}
fn frame(
    player: &Player,
    started: Instant,
    label: &str,
    mut ready: impl FnMut(Duration) -> bool,
) -> Result<Duration> {
    let waiting = Instant::now();
    loop {
        budget(started, ROUND_BUDGET, label)?;
        budget(waiting, READ_BUDGET, label)?;
        health(player)?;
        // 暂停 seek/Step 的结果可能只发布一次；pending 清除前保留它，不提前取走。
        if let Some((_, pts, pixels)) = (!player.seeking())
            .then(|| player.take_frame_with_time())
            .flatten()
        {
            ensure(
                player.size() == (WIDTH, HEIGHT) && pixels.len() == PIXEL_BYTES,
                format!("{label} 的真实帧尺寸/像素长度不符"),
            )?;
            ensure(
                pts <= Duration::from_millis(119_500),
                "真实帧 PTS 超过源末帧",
            )?;
            if ready(pts) {
                return Ok(pts);
            }
        }
        std::thread::sleep(POLL);
    }
}
fn hold(player: &Player, started: Instant, position: Duration) -> Result<()> {
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        budget(started, ROUND_BUDGET, "暂停保持")?;
        health(player)?;
        ensure(
            !player.playing()
                && !player.seeking()
                && player.position().0.abs_diff(position) <= Duration::from_millis(1),
            "暂停期间主时钟推进",
        )?;
        if let Some((_, pts, pixels)) = player.take_frame_with_time() {
            ensure(
                pts == Duration::ZERO && pixels.len() == PIXEL_BYTES,
                "暂停首帧期间出现推进或无效像素",
            )?;
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}
fn closed(probe: &PlaybackShutdownProbe) -> Result<Duration> {
    let started = Instant::now();
    while !probe.finished() {
        ensure(
            started.elapsed() < Duration::from_secs(10),
            format!(
                "4K Drop 后十秒仍有 owner：video={} audio={}",
                probe.video_exited(),
                probe.audio_exited()
            ),
        )?;
        std::thread::sleep(POLL);
    }
    Ok(started.elapsed())
}
fn memory_delta(after: &Value, before: &Value) -> Value {
    let delta = |key: &str| {
        after[key]
            .as_u64()
            .zip(before[key].as_u64())
            .map(|(after, before)| i128::from(after) - i128::from(before))
    };
    json!({"working_set_bytes":delta("working_set_bytes"),"private_bytes":delta("private_bytes")})
}

fn run_round(path: &Path, cycle: usize, report: &mut Value) -> Result<()> {
    let started = Instant::now();
    let before = crate::process_memory::cpu_memory()?;
    *report = json!({"cycle":cycle,"warmup":cycle==0,"before_memory":before});
    let mut player = Player::open_file_paused_with_audio_settings(path.to_path_buf(), 0., true);
    let shutdown = player.shutdown_probe();
    let result = (|| -> Result<()> {
        frame(&player, started, "暂停首帧", |pts| {
            pts == Duration::ZERO
        })?;
        ensure(
            !player.playing()
                && player.position().1.abs_diff(Duration::from_secs(120))
                    <= Duration::from_millis(250),
            "Player 实际时长或初始暂停状态不符",
        )?;
        let audio_started = Instant::now();
        while !player
            .audio_info()
            .is_some_and(|formats| !formats.is_empty())
        {
            budget(started, ROUND_BUDGET, "暂停音频就绪")?;
            budget(audio_started, READ_BUDGET, "暂停音频就绪")?;
            health(&player)?;
            std::thread::sleep(POLL);
        }
        hold(&player, started, Duration::ZERO)?;
        let seeking = Instant::now();
        player.seek_ratio((115.25 / 120.) as f32);
        let floor = frame(&player, started, "115.25s seek floor", |pts| {
            pts == Duration::from_secs(115)
        })?;
        report["tail_seek_ms"] = (seeking.elapsed().as_secs_f64() * 1000.).into();
        report["floor_pts_ms"] = (floor.as_secs_f64() * 1000.).into();
        player.step_forward();
        let stepped = frame(&player, started, "真实下一帧", |pts| {
            pts == Duration::from_millis(115_500)
        })?;
        report["step_pts_ms"] = (stepped.as_secs_f64() * 1000.).into();
        player.set_playing(true);
        let played = frame(&player, started, "尾段真实播放", |pts| {
            pts >= Duration::from_secs(116) && player.position().0 >= Duration::from_secs(116)
        })?;
        report["played_pts_ms"] = (played.as_secs_f64() * 1000.).into();
        report["active_memory"] = crate::process_memory::cpu_memory()?;
        player.set_playing(false);
        player.seek_ratio(0.);
        frame(&player, started, "暂停 seek 回零", |pts| {
            pts == Duration::ZERO
        })?;
        hold(&player, started, Duration::ZERO)?;
        Ok(())
    })();
    player.set_muted(true);
    player.set_volume(0.);
    drop(player);
    let closing = closed(&shutdown);
    let after = crate::process_memory::cpu_memory()?;
    report["after_memory"] = after.clone();
    report["memory_delta"] = memory_delta(&after, &before);
    report["video_exited"] = shutdown.video_exited().into();
    report["audio_exited"] = shutdown.audio_exited().into();
    report["round_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
    if let Ok(elapsed) = &closing {
        report["close_ms"] = (elapsed.as_secs_f64() * 1000.).into();
    }
    if let Err(error) = &result {
        report["error"] = error.to_string().into();
    }
    match (result, closing) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(format!("{error}；关闭也失败：{close}").into()),
    }
}

#[test]
#[ignore = "显式自造120秒4K H.264/零AAC，静音连接默认输出并四轮seek/Step/关闭；无GUI，不读取用户媒体"]
fn native_player_long_4k_streaming_seek_and_repeated_close() -> Result<()> {
    let _runtime = MfRuntime::new()?;
    let fixture = Fixture::new()?;
    let mut report = json!({"ok":false,"scope":"long_4k_source_tail_playback_and_repeated_close",
        "requested":{"width":WIDTH,"height":HEIGHT,"fps":FPS,"source_seconds":SECONDS},
        "rounds":[],"volume_from_open":0,"muted_from_open":true,"fixture_pcm_silent":true,
        "whole_source_played":false,"long_recording_drift_verified":false,
        "memory_stability_verified":false,"gui_observed":false,"presented_observed":false});
    let result = (|| -> Result<()> {
        encode(&fixture.path(), &mut report)?;
        report["decoded_metadata"] = inspect(&fixture.path())?;
        report["post_fixture_memory"] = crate::process_memory::cpu_memory()?;
        for cycle in 0..4 {
            let mut observed = Value::Null;
            let result = run_round(&fixture.path(), cycle, &mut observed);
            report["rounds"].as_array_mut().unwrap().push(observed);
            result?;
        }
        Ok(())
    })();
    report["ok"] = result.is_ok().into();
    if let Err(error) = &result {
        report["error"] = error.to_string().into();
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    result
}
