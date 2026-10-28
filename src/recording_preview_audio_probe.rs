//! 显式执行的 Player + 内嵌 AAC 设备探针；取帧与系统环回均不代表 GUI 呈现。
use super::{PlaybackShutdownProbe, Player};
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
    Media::MediaFoundation::{MFAudioFormat_AAC, MFMediaType_Audio, MFVideoFormat_H264},
    Storage::FileSystem::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    },
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);
const PLAY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);
const POSITION_TOLERANCE: Duration = Duration::from_millis(1);

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

struct Fixture {
    path: PathBuf,
    duration: Duration,
    timeline: Vec<Duration>,
    aac_stream: u32,
}

impl Fixture {
    fn read() -> Result<Self> {
        let path =
            PathBuf::from(std::env::var_os("RUSTSHOT_AUDIO_SWITCH_FIXTURE").ok_or(
                "请将 RUSTSHOT_AUDIO_SWITCH_FIXTURE 指向既有的本地 2–10 秒 H.264/AAC 小样",
            )?);
        let metadata = fs::metadata(&path)?;
        ensure(metadata.is_file(), "Player 小样不是普通文件")?;
        let cloud = FILE_ATTRIBUTE_OFFLINE.0
            | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0;
        ensure(
            metadata.file_attributes() & cloud == 0,
            "不读取云端占位小样",
        )?;
        let _runtime = MfRuntime::new()?;
        let mut reader = VideoReader::open(&path)?;
        if let Some(error) = &reader.info.stream_probe_error {
            return Err(format!("fixture 真实流探测未完整成功：{error}").into());
        }
        ensure(
            reader.info.codec == MFVideoFormat_H264,
            "fixture 不是 H.264 视频",
        )?;
        ensure(
            (Duration::from_secs(2)..=Duration::from_secs(10)).contains(&reader.duration),
            "fixture 时长必须为 2–10 秒",
        )?;
        let aac_stream = reader
            .info
            .streams
            .iter()
            .find(|stream| stream.major_type == MFMediaType_Audio)
            .filter(|stream| stream.codec == Some(MFAudioFormat_AAC))
            .map(|stream| stream.stream_index)
            .ok_or("fixture 默认首音轨必须是实际探测到的 AAC 源流")?;
        let mut timeline = Vec::new();
        let deadline = Instant::now() + PLAY_TIMEOUT;
        loop {
            ensure(Instant::now() < deadline, "fixture 视频 PTS 回读超过五秒")?;
            let next = reader.next()?;
            ensure(Instant::now() < deadline, "fixture 视频 PTS 回读超过五秒")?;
            let Some(frame) = next else { break };
            ensure(timeline.len() < 4096, "fixture 视频帧数超出小样上限")?;
            ensure(
                timeline
                    .last()
                    .is_none_or(|previous| *previous <= frame.time),
                "fixture 解码 PTS 倒退",
            )?;
            timeline.push(frame.time);
        }
        ensure(timeline.len() >= 3, "fixture 至少需要三个真实视频帧")?;
        ensure(
            timeline
                .iter()
                .any(|pts| *pts > reader.duration.mul_f64(0.55)),
            "fixture 在最终跳转目标之后没有可验证的视频推进",
        )?;
        Ok(Self {
            path,
            duration: reader.duration,
            timeline,
            aac_stream,
        })
    }

    fn floor(&self, target: Duration) -> Result<Duration> {
        self.timeline
            .iter()
            .rfind(|pts| **pts <= target)
            .copied()
            .ok_or_else(|| "fixture 在目标之前没有真实视频帧".into())
    }
}

#[derive(Clone, Copy, Debug)]
struct Status {
    generation: u64,
    playing: bool,
    pending: bool,
    position: Duration,
    duration: Duration,
}

fn status(player: &Player) -> Status {
    let state = player
        .status
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    Status {
        generation: state.generation,
        playing: state.playing,
        pending: state.pending_seek.is_some() || state.pending_step,
        position: state.position,
        duration: state.duration,
    }
}

fn health(player: &Player) -> Result<()> {
    if let Some(error) = player.error() {
        return Err(format!("Player 视频失败：{error}").into());
    }
    if let Some(error) = player.audio_error() {
        return Err(format!("Player 内嵌声音失败：{error}").into());
    }
    ensure(player.audio.is_some(), "Player 没有创建内嵌声音工作线程")?;
    ensure(
        !player.audio_info().is_some_and(|tracks| tracks.is_empty()),
        "Player 将有 AAC 的小样降级为无声音频路径",
    )
}

fn clock(player: &Player, generation: u64) -> Option<PublishedClock> {
    player
        .audio
        .as_ref()?
        .clock()
        .filter(|clock| clock.transport_generation == generation)
}

fn wait<T>(
    player: &Player,
    stage: &str,
    timeout: Duration,
    mut ready: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        health(player)?;
        if let Some(value) = ready()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            let state = status(player);
            return Err(format!(
                "{stage}未在期限内完成：{state:?}，设备时钟 {:?}",
                clock(player, state.generation)
            )
            .into());
        }
        std::thread::sleep(POLL);
    }
}

#[derive(Clone, Copy)]
struct Taken {
    serial: u64,
    pts: Duration,
}

fn take(player: &Player, fixture: &Fixture, generation: u64) -> Result<Option<Taken>> {
    {
        let state = player
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.generation != generation || state.frame_generation != generation {
            return Ok(None);
        }
    }
    let Some((serial, pts, pixels)) = player.take_frame_with_time() else {
        return Ok(None);
    };
    ensure(
        status(player).generation == generation,
        "取帧跨越了控制代次",
    )?;
    let size = player.size();
    ensure(
        !pixels.is_empty() && pixels.len() == size.0 as usize * size.1 as usize * 4,
        "Taken 帧没有与公开尺寸匹配的真实像素",
    )?;
    ensure(
        fixture.timeline.contains(&pts),
        "Taken PTS 不在独立解码时间线中",
    )?;
    Ok(Some(Taken { serial, pts }))
}

fn wait_paused(
    player: &Player,
    generation: u64,
    target: Option<Duration>,
) -> Result<PublishedClock> {
    wait(
        player,
        "等待 Player 与设备暂停",
        CONTROL_TIMEOUT,
        || {
            let state = status(player);
            let Some(clock) = clock(player, generation) else {
                return Ok(None);
            };
            Ok((state.generation == generation
                && !state.playing
                && !state.pending
                && clock.clock.phase == ClockPhase::Paused
                && !clock.clock.advancing
                && target.is_none_or(|target| {
                    state.position.abs_diff(target) <= POSITION_TOLERANCE
                        && clock.clock.media_position.abs_diff(target) <= POSITION_TOLERANCE
                }))
            .then_some(clock))
        },
    )
}

fn hold_paused(player: &Player, generation: u64, position: Duration) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(80);
    loop {
        health(player)?;
        let clock = clock(player, generation).ok_or("暂停期间设备时钟消失或换代")?;
        ensure(
            !player.playing()
                && clock.clock.phase == ClockPhase::Paused
                && !clock.clock.advancing
                && clock.clock.media_position == position,
            "暂停期间真实设备媒体位置仍在推进",
        )?;
        if Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

fn seek_ready(
    player: &Player,
    fixture: &Fixture,
    generation: u64,
    target: Duration,
) -> Result<Taken> {
    let paused = wait_paused(player, generation, Some(target))?;
    let frame = wait(
        player,
        "等待最终代目标 Taken 帧",
        CONTROL_TIMEOUT,
        || take(player, fixture, generation),
    )?;
    ensure(
        frame.pts.abs_diff(fixture.floor(target)?) <= Duration::from_nanos(100),
        "暂停跳转没有保留实际 PTS 对应的目标所在帧",
    )?;
    hold_paused(player, generation, paused.clock.media_position)?;
    Ok(frame)
}

fn exercise(player: &mut Player, fixture: &Fixture) -> Result<Value> {
    let initial_generation = status(player).generation;
    let first_clock = wait(player, "等待设备 Playing", CONTROL_TIMEOUT, || {
        Ok(clock(player, initial_generation)
            .filter(|clock| clock.clock.phase == ClockPhase::Playing && clock.clock.advancing))
    })?;
    let progressed = wait(
        player,
        "等待实际设备时钟推进",
        CONTROL_TIMEOUT,
        || {
            Ok(clock(player, initial_generation).filter(|clock| {
                clock.clock.phase == ClockPhase::Playing
                    && clock.clock.advancing
                    && clock.clock.qpc_100ns > first_clock.clock.qpc_100ns
                    && clock.clock.media_position
                        >= first_clock.clock.media_position + Duration::from_millis(20)
            }))
        },
    )?;
    let first_taken = wait(player, "等待真实 Taken 帧", CONTROL_TIMEOUT, || {
        take(player, fixture, initial_generation)
    })?;

    player.set_playing(false);
    let pause_generation = status(player).generation;
    let paused = wait_paused(player, pause_generation, None)?;
    hold_paused(player, pause_generation, paused.clock.media_position)?;

    let target = fixture.duration.mul_f64(0.37);
    player.seek(target);
    player.set_rate(2.);
    let seek_generation = status(player).generation;
    let seek_frame = seek_ready(player, fixture, seek_generation, target)?;
    ensure(player.rate == 2., "暂停变速没有保留 2 倍速设置")?;

    for ratio in [0.15, 0.75, 0.55] {
        player.seek(fixture.duration.mul_f64(ratio));
    }
    let final_target = fixture.duration.mul_f64(0.55);
    let final_generation = status(player).generation;
    ensure(
        final_generation != seek_generation,
        "连续跳转没有推进控制代次",
    )?;
    let final_frame = seek_ready(player, fixture, final_generation, final_target)?;
    ensure(
        final_frame.serial > seek_frame.serial,
        "最终跳转没有发布新的真实帧",
    )?;

    player.set_playing(true);
    let resume_generation = status(player).generation;
    let resumed = wait(
        player,
        "等待恢复后的真实设备时钟",
        CONTROL_TIMEOUT,
        || {
            Ok(clock(player, resume_generation).filter(|clock| {
                clock.clock.phase == ClockPhase::Playing
                    && clock.clock.advancing
                    && clock.clock.media_position > final_target
            }))
        },
    )?;
    let mut draining_endpoint = None;
    let mut drained = None;
    let mut saw_draining = false;
    let mut saw_ended = false;
    let mut latest = final_frame;
    let mut resumed_progress = None;
    let mut rate_anchor = resumed;
    let eof = wait(
        player,
        "等待设备尾部排空与 Player EOF",
        PLAY_TIMEOUT,
        || {
            // Player 可在读到 Ended 后立即暂停；同代真实回执保留这次设备观测。
            if let Some(receipt) = player
                .audio
                .as_ref()
                .and_then(|audio| audio.drained_clock(resume_generation))
            {
                ensure(
                    receipt.transport_generation == resume_generation
                        && receipt.clock.phase == ClockPhase::Ended
                        && !receipt.clock.advancing
                        && receipt.clock.queued_media.is_zero(),
                    "设备排空回执不是当前恢复代的真实 Ended 空队列",
                )?;
                saw_ended = true;
                drained = Some(receipt.clock.media_position);
            }
            if let Some(observed) = clock(player, resume_generation) {
                let value = observed.clock;
                if resumed_progress.is_none()
                    && value.generation != rate_anchor.clock.generation
                    && value.phase == ClockPhase::Playing
                    && value.advancing
                {
                    // 欠载恢复会重新锚定设备时钟；倍速观测只比较同一连续代次。
                    rate_anchor = observed;
                }
                if resumed_progress.is_none()
                    && value.phase == ClockPhase::Playing
                    && value.advancing
                    && value.generation == rate_anchor.clock.generation
                    && value.qpc_100ns > rate_anchor.clock.qpc_100ns
                    && value.media_position
                        >= rate_anchor.clock.media_position + Duration::from_millis(30)
                {
                    resumed_progress = Some(observed);
                }
                if value.phase == ClockPhase::Draining {
                    saw_draining = true;
                    draining_endpoint =
                        Some((value.generation, value.media_position + value.queued_media));
                }
                if value.phase == ClockPhase::Ended {
                    ensure(
                        !value.advancing && value.queued_media.is_zero(),
                        "Ended 时设备仍有待播队列",
                    )?;
                    saw_ended = true;
                    drained = Some(value.media_position);
                } else if draining_endpoint.is_some_and(|(epoch, end)| {
                    epoch == value.generation
                        && value.media_position >= end
                        && value.queued_media.is_zero()
                }) {
                    drained = Some(value.media_position);
                }
            }
            if let Some(frame) = take(player, fixture, resume_generation)? {
                ensure(
                    frame.serial > latest.serial && frame.pts >= latest.pts,
                    "恢复后帧序或 PTS 倒退",
                )?;
                latest = frame;
            }
            let state = status(player);
            Ok((!state.playing
                && !state.pending
                && state.position >= state.duration
                && state.duration == fixture.duration
                && latest.pts == *fixture.timeline.last().ok_or("fixture 没有末帧")?
                && latest.pts > final_frame.pts
                && resumed_progress.is_some()
                && drained.is_some())
            .then_some(state))
        },
    )
    .map_err(|error| {
        format!(
            "EOF 排空未验：{error}；saw_draining={saw_draining} saw_ended={saw_ended} drained={drained:?}"
        )
    })?;
    let resumed_progress = resumed_progress.ok_or("未观测到恢复后的真实设备推进")?;
    let media_delta = resumed_progress.clock.media_position - rate_anchor.clock.media_position;
    let qpc_delta = resumed_progress.clock.qpc_100ns - rate_anchor.clock.qpc_100ns;
    let observed_rate = media_delta.as_secs_f64() / (qpc_delta as f64 / 10_000_000.);
    ensure(
        (1.5..=2.5).contains(&observed_rate),
        "设备观测没有体现 2 倍媒体推进",
    )?;
    let report = player.sync_report();
    let groups = report["groups"]
        .as_array()
        .ok_or("Taken 同步报告缺少分组")?;
    ensure(
        !groups.is_empty() && groups.iter().all(|group| group["stage"] == "taken"),
        "同步报告没有真实 Taken 样本，或混入其它呈现证据",
    )?;
    Ok(json!({
        "ok": true,
        "scope": "native_player_muted_embedded_audio_transport_taken_and_shutdown",
        "device_clock_observed": true,
        "muted_from_open": true,
        "gui_observed": false,
        "submitted_observed": false,
        "presented_observed": false,
        "aac_stream_index": fixture.aac_stream,
        "first_taken_ms": first_taken.pts.as_secs_f64() * 1000.,
        "initial_device_ms": progressed.clock.media_position.as_secs_f64() * 1000.,
        "paused_device_ms": paused.clock.media_position.as_secs_f64() * 1000.,
        "paused_seek_floor_ms": seek_frame.pts.as_secs_f64() * 1000.,
        "rapid_seek_floor_ms": final_frame.pts.as_secs_f64() * 1000.,
        "resume_observed_media_rate": observed_rate,
        "saw_draining": saw_draining,
        "saw_ended": saw_ended,
        "drained_device_ms": drained.map(|time| time.as_secs_f64() * 1000.),
        "eof_position_ms": eof.position.as_secs_f64() * 1000.,
        "last_taken_ms": latest.pts.as_secs_f64() * 1000.,
        "taken_scope_sync_report": report,
    }))
}

fn wait_shutdown(probe: &PlaybackShutdownProbe, started: Instant) -> Result<Duration> {
    while !probe.finished() {
        ensure(
            started.elapsed() < CONTROL_TIMEOUT,
            format!(
                "Drop 后三秒内工作线程未全部退出：video={} audio={}",
                probe.video_exited(),
                probe.audio_exited()
            ),
        )?;
        std::thread::sleep(POLL);
    }
    let elapsed = started.elapsed();
    ensure(
        elapsed <= CONTROL_TIMEOUT,
        "Drop 后工作线程退出观测超过三秒",
    )?;
    Ok(elapsed)
}

#[test]
#[ignore = "显式指定既有本地 AAC 小样并连接默认输出设备；从打开起静音，无 GUI"]
fn native_player_muted_embedded_audio_transport_and_close() -> Result<()> {
    let fixture = Fixture::read()?;
    // 此工厂先静音 PreviewAudio，再启动视频线程；探针全程不解除静音。
    let mut player = Player::open_file_with_audio_settings(fixture.path.clone(), 1., true);
    let shutdown = player.shutdown_probe();
    let result = exercise(&mut player, &fixture);
    let close_started = Instant::now();
    drop(player);
    // 无论验证成功与否都观察关闭；token 只持退出标记，不延长媒体或设备寿命。
    let closed = wait_shutdown(&shutdown, close_started);
    match (result, closed) {
        (Ok(mut report), Ok(elapsed)) => {
            report["workers_exited"] = true.into();
            report["drop_to_workers_exited_ms"] = (elapsed.as_secs_f64() * 1000.).into();
            println!("{report}");
            Ok(())
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(format!("{error}；关闭验证也失败：{close}").into()),
    }
}

#[test]
#[ignore = "显式本地 AAC 小样和默认设备，静音验证保调/高质量处理与释放；无 GUI"]
fn native_player_pitch_processing_muted_transport_and_close() -> Result<()> {
    let fixture = Fixture::read()?;
    for pitch in [
        super::PitchMode::PitchPreserving,
        super::PitchMode::Resample,
    ] {
        let settings = super::AudioProcessing {
            pitch,
            quality: super::ResampleQuality::High,
        };
        let mut player = Player::open_file_with_audio_settings(fixture.path.clone(), 1., true);
        let shutdown = player.shutdown_probe();
        // 从低速开始，再由公共传输用例切到 2 倍；设置前后的瞬间也保持静音。
        player.set_rate(0.5);
        let result = player.set_audio_processing(settings).and_then(|()| {
            ensure(
                player.audio_processing() == settings,
                "处理选项没有保留当前请求",
            )?;
            exercise(&mut player, &fixture)
        });
        let close_started = Instant::now();
        drop(player);
        let closed = wait_shutdown(&shutdown, close_started);
        match (result, closed) {
            (Ok(mut report), Ok(elapsed)) => {
                report["processing_pitch"] = format!("{pitch:?}").into();
                report["processing_quality"] = "high".into();
                report["initial_rate"] = 0.5.into();
                report["workers_exited"] = true.into();
                report["drop_to_workers_exited_ms"] = (elapsed.as_secs_f64() * 1000.).into();
                println!("{report}");
            }
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => return Err(error),
            (Err(error), Err(close)) => {
                return Err(format!("{error}；关闭验证也失败：{close}").into())
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "显式本地 AAC 小样与默认输出设备，多轮静音打开关闭和内存读数；无 GUI"]
fn native_player_repeated_muted_open_close_releases_workers() -> Result<()> {
    let fixture = Fixture::read()?;
    let mut observations = Vec::new();
    let mut previous = Vec::new();
    for cycle in 0..6 {
        ensure(
            previous.iter().all(PlaybackShutdownProbe::finished),
            "前一轮播放工作线程仍残留",
        )?;
        let player = Player::open_file_with_audio_settings(fixture.path.clone(), 1., true);
        let shutdown = player.shutdown_probe();
        let generation = status(&player).generation;
        let active = wait(
            &player,
            "多轮打开等待真实设备 Playing",
            CONTROL_TIMEOUT,
            || {
                Ok(clock(&player, generation).filter(|clock| {
                    clock.clock.phase == ClockPhase::Playing && clock.clock.advancing
                }))
            },
        );
        let taken = active.and_then(|_| {
            wait(
                &player,
                "多轮打开等待真实取帧",
                CONTROL_TIMEOUT,
                || take(&player, &fixture, generation),
            )
        });
        let close_started = Instant::now();
        drop(player);
        let closed = wait_shutdown(&shutdown, close_started);
        let elapsed = match (taken, closed) {
            (Ok(_), Ok(elapsed)) => elapsed,
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => return Err(error),
            (Err(error), Err(close)) => {
                return Err(format!("{error}；关闭验证也失败：{close}").into())
            }
        };
        previous.push(shutdown);
        observations.push(json!({
            "cycle": cycle,
            "warmup": cycle == 0,
            "workers_exited": true,
            "drop_to_workers_exited_ms": elapsed.as_secs_f64() * 1000.,
            "cpu_memory": crate::process_memory::cpu_memory()?,
        }));
    }
    println!(
        "{}",
        json!({
            "ok":true,
            "scope":"repeated_muted_player_open_close_workers_and_memory_observations",
            "gui_observed":false,
            "audio_audibility_observed":false,
            "memory_stability_verified":false,
            "memory_note":"Device and decoder caches require comparison of the post-warmup observations; worker exit alone does not prove memory stability.",
            "observations":observations,
        })
    );
    Ok(())
}

#[test]
#[ignore = "自造零 PCM 的 H.264/AAC 小样，连接默认输出设备验证暂停启动；全程静音，无 GUI"]
fn native_player_paused_open_decodes_first_frame_then_waits_for_explicit_play() -> Result<()> {
    use crate::media_foundation::{Mp4Writer, AUDIO_CHANNELS, AUDIO_RATE};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TemporaryFixture(PathBuf);
    impl Drop for TemporaryFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join("paused.mp4"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "rustshot-player-paused-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&directory)?;
    let temporary = TemporaryFixture(directory);
    let path = temporary.0.join("paused.mp4");
    let fixture = {
        let _runtime = MfRuntime::new()?;
        const WIDTH: u32 = 64;
        const FPS: u32 = 30;
        const FRAMES: u32 = FPS * 2;
        let mut writer =
            Mp4Writer::new_legacy_for_benchmark(&path, WIDTH, WIDTH, FPS, 256_000, true)?;
        let silence = vec![0_i16; (AUDIO_RATE / FPS * AUDIO_CHANNELS) as usize];
        let mut pixels = vec![0_u8; (WIDTH * WIDTH * 4) as usize];
        for index in 0..FRAMES {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[index as u8, 16, 32, 255]);
            }
            let time = Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS));
            let end = Duration::from_nanos(u64::from(index + 1) * 1_000_000_000 / u64::from(FPS));
            writer.write_video(&pixels, WIDTH, time, end - time)?;
            writer.write_audio(&silence, time)?;
        }
        writer.finish()?;

        let mut reader = VideoReader::open(&path)?;
        ensure(
            reader.info.codec == MFVideoFormat_H264 && reader.info.stream_probe_error.is_none(),
            "暂停启动夹具没有可确认的 H.264 与完整流元数据",
        )?;
        let aac_stream = reader
            .info
            .streams
            .iter()
            .find(|stream| stream.major_type == MFMediaType_Audio)
            .filter(|stream| stream.codec == Some(MFAudioFormat_AAC))
            .map(|stream| stream.stream_index)
            .ok_or("暂停启动夹具没有真实 AAC 源流")?;
        let mut timeline = Vec::new();
        let deadline = Instant::now() + PLAY_TIMEOUT;
        loop {
            ensure(Instant::now() < deadline, "暂停启动夹具回读超时")?;
            let frame = reader.next()?;
            ensure(Instant::now() < deadline, "暂停启动夹具回读超时")?;
            let Some(frame) = frame else { break };
            ensure(timeline.len() < 4096, "暂停启动夹具帧数超限")?;
            timeline.push(frame.time);
        }
        ensure(
            timeline.first() == Some(&Duration::ZERO)
                && timeline.iter().any(|pts| *pts > Duration::from_millis(100))
                && timeline.windows(2).all(|pair| pair[0] <= pair[1]),
            "暂停启动夹具必须从 PTS 0 开始并有实际后继视频帧",
        )?;
        Fixture {
            path,
            duration: reader.duration,
            timeline,
            aac_stream,
        }
    };

    // PCM 本身全零，且构造前选择 volume=0、muted=true；测试全程不解除静音。
    let mut player = Player::open_file_paused_with_audio_settings(fixture.path.clone(), 0., true);
    let shutdown = player.shutdown_probe();
    let result = (|| -> Result<Value> {
        let initial = status(&player);
        ensure(!initial.playing, "暂停构造返回时已带有播放意图")?;
        let generation = initial.generation;
        let first = wait(
            &player,
            "暂停启动等待元数据与真实首帧",
            CONTROL_TIMEOUT,
            || {
                let state = status(&player);
                ensure(
                    state.generation == generation
                        && !state.playing
                        && !state.pending
                        && state.position.is_zero(),
                    "首帧初始化期间播放器提前推进或改变暂停意图",
                )?;
                let metadata_ready = player
                    .status
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .info
                    .as_ref()
                    .is_some_and(|info| {
                        info.codec == MFVideoFormat_H264
                            && info.stream_probe_error.is_none()
                            && info.streams.iter().any(|stream| {
                                stream.stream_index == fixture.aac_stream
                                    && stream.major_type == MFMediaType_Audio
                                    && stream.codec == Some(MFAudioFormat_AAC)
                            })
                    });
                if !metadata_ready {
                    return Ok(None);
                }
                ensure(
                    state.duration == fixture.duration && player.size() == (64, 64),
                    "Player 元数据与独立回读夹具不一致",
                )?;
                take(&player, &fixture, generation)
            },
        )?;
        ensure(first.pts.is_zero(), "暂停启动没有解码 PTS 0 的真实首帧")?;
        let paused = wait_paused(&player, generation, Some(Duration::ZERO))?;
        let hold_started = Instant::now();
        let hold_for = Duration::from_millis(250);
        loop {
            health(&player)?;
            let state = status(&player);
            let observed = clock(&player, generation).ok_or("暂停启动没有真实设备时钟回执")?;
            ensure(
                state.generation == generation
                    && !state.playing
                    && !state.pending
                    && state.position.is_zero()
                    && observed.clock.phase == ClockPhase::Paused
                    && !observed.clock.advancing
                    && observed.clock.media_position.is_zero(),
                "显式播放之前 Player 或设备媒体位置发生推进",
            )?;
            ensure(
                take(&player, &fixture, generation)?.is_none(),
                "显式播放之前又交付了后继视频帧",
            )?;
            if hold_started.elapsed() >= hold_for {
                break;
            }
            std::thread::sleep(POLL);
        }
        let held_for = hold_started.elapsed();

        player.set_playing(true);
        let play_generation = status(&player).generation;
        ensure(play_generation != generation, "显式播放没有推进控制代次")?;
        let mut anchor = wait(
            &player,
            "显式播放等待设备 Playing",
            CONTROL_TIMEOUT,
            || {
                Ok(clock(&player, play_generation).filter(|observed| {
                    observed.clock.phase == ClockPhase::Playing
                        && observed.clock.advancing
                        && observed.clock.qpc_100ns > paused.clock.qpc_100ns
                }))
            },
        )?;
        let progressed = wait(
            &player,
            "显式播放等待真实设备推进",
            CONTROL_TIMEOUT,
            || {
                let Some(observed) = clock(&player, play_generation) else {
                    return Ok(None);
                };
                if observed.clock.phase != ClockPhase::Playing || !observed.clock.advancing {
                    return Ok(None);
                }
                if observed.clock.generation != anchor.clock.generation {
                    anchor = observed;
                    return Ok(None);
                }
                Ok((observed.clock.qpc_100ns > anchor.clock.qpc_100ns
                    && observed.clock.media_position
                        >= anchor.clock.media_position + Duration::from_millis(20))
                .then_some(observed))
            },
        )?;
        let advanced = wait(
            &player,
            "显式播放等待后继真实视频帧",
            CONTROL_TIMEOUT,
            || {
                Ok(take(&player, &fixture, play_generation)?
                    .filter(|frame| frame.serial > first.serial && frame.pts > first.pts))
            },
        )?;
        ensure(
            status(&player).position > Duration::ZERO,
            "设备与取帧推进后 Player 位置仍未推进",
        )?;
        Ok(json!({
            "ok":true,
            "scope":"native_player_paused_open_first_taken_explicit_play_and_shutdown",
            "fixture_pcm_silent":true,
            "muted_from_open":true,
            "volume_from_open":0,
            "aac_stream_index":fixture.aac_stream,
            "first_taken_ms":first.pts.as_secs_f64() * 1000.,
            "paused_hold_ms":held_for.as_secs_f64() * 1000.,
            "advanced_taken_ms":advanced.pts.as_secs_f64() * 1000.,
            "advanced_device_ms":progressed.clock.media_position.as_secs_f64() * 1000.,
            "device_clock_observed":true,
            "gui_observed":false,
            "submitted_observed":false,
            "presented_observed":false,
        }))
    })();
    let close_started = Instant::now();
    drop(player);
    let closed = wait_shutdown(&shutdown, close_started);
    match (result, closed) {
        (Ok(mut report), Ok(elapsed)) => {
            report["workers_exited"] = true.into();
            report["drop_to_workers_exited_ms"] = (elapsed.as_secs_f64() * 1000.).into();
            println!("{report}");
            Ok(())
        }
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(format!("{error}；关闭验证也失败：{close}").into()),
    }
}

const LOOPBACK_RATE: u32 = 48_000;
const LOOPBACK_WINDOW: usize = 9_600;
const LOOPBACK_VOLUME: f32 = 0.03;
const LOOPBACK_QUIET_RMS: f64 = 0.000_01;

struct LoopbackFixture {
    directory: PathBuf,
    frequencies: [f64; 2],
    signature: u128,
}
impl LoopbackFixture {
    fn new() -> Result<Self> {
        use crate::media_foundation::{Mp4Writer, AUDIO_CHANNELS, AUDIO_RATE};
        use std::hash::BuildHasher;
        use std::time::{SystemTime, UNIX_EPOCH};

        let signature = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustshot-render-loopback-{}-{signature}",
            std::process::id(),
        ));
        fs::create_dir(&directory)?;
        // 200ms 窗内均为整数周期；邻带也落在不同的正交频点。
        let choice = std::collections::hash_map::RandomState::new()
            .hash_one((signature, std::process::id()));
        let frequencies = [
            900. + 5. * (choice % 40) as f64,
            1_500. + 5. * ((choice / 40) % 40) as f64,
        ];
        let fixture = Self {
            directory,
            frequencies,
            signature,
        };
        let mut writer =
            Mp4Writer::new_legacy_for_benchmark(&fixture.path(), 320, 240, 10, 600_000, true)?;
        let mut pixels = vec![0_u8; 320 * 240 * 4];
        for frame in 0..80_u32 {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[16, 32, frame as u8, 255]);
            }
            let time = Duration::from_millis(u64::from(frame) * 100);
            writer.write_video(&pixels, 320, time, Duration::from_millis(100))?;
            let mut pcm = Vec::with_capacity((AUDIO_RATE / 10 * AUDIO_CHANNELS) as usize);
            for offset in 0..AUDIO_RATE / 10 {
                let index = u64::from(frame) * u64::from(AUDIO_RATE / 10) + u64::from(offset);
                let seconds = index as f64 / f64::from(AUDIO_RATE);
                let value = frequencies
                    .iter()
                    .map(|frequency| (std::f64::consts::TAU * frequency * seconds).sin())
                    .sum::<f64>()
                    * 0.1;
                let sample = (value * f64::from(i16::MAX)).round() as i16;
                pcm.extend([sample, sample]);
            }
            writer.write_audio(&pcm, time)?;
        }
        writer.finish()?;
        Ok(fixture)
    }
    fn path(&self) -> PathBuf {
        self.directory.join("signature.mp4")
    }
}
impl Drop for LoopbackFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(&self.directory);
    }
}

/// 只保持本轮固定 render 端点活跃；不能贡献待验证的双频信号。
struct LoopbackKeepalive {
    client: windows::Win32::Media::Audio::IAudioClient,
    writer: windows::Win32::Media::Audio::IAudioRenderClient,
    frames: u32,
}
impl LoopbackKeepalive {
    fn open(device: &windows::Win32::Media::Audio::IMMDevice) -> Result<Self> {
        use windows::Win32::{
            Media::Audio::{IAudioClient, AUDCLNT_SHAREMODE_SHARED},
            System::Com::{CoTaskMemFree, CLSCTX_ALL},
        };
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let initialized =
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None);
            CoTaskMemFree(Some(format.cast()));
            initialized?;
            let render = Self {
                frames: client.GetBufferSize()?,
                writer: client.GetService()?,
                client,
            };
            ensure(render.frames != 0, "无法判定：静音保活缓冲为空")?;
            render.pump()?;
            render.client.Start()?;
            Ok(render)
        }
    }
    fn pump(&self) -> Result<()> {
        use windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT;
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let available = self
                .frames
                .checked_sub(padding)
                .ok_or("静音保活 padding 无效")?;
            if available != 0 {
                self.writer.GetBuffer(available)?;
                self.writer
                    .ReleaseBuffer(available, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}
impl Drop for LoopbackKeepalive {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

#[derive(Clone, Copy, Debug)]
struct LoopbackStats {
    rms: f64,
    tones: [f64; 2],
    guard: f64,
    residual: f64,
}
impl LoopbackStats {
    fn measure(samples: &[[f32; 2]], frequencies: [f64; 2]) -> Self {
        let mut result = Self {
            rms: 0.,
            tones: [0.; 2],
            guard: 0.,
            residual: 0.,
        };
        let mut best = 0.;
        for channel in 0..2 {
            let rms = (samples
                .iter()
                .map(|sample| f64::from(sample[channel]).powi(2))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt();
            let bin = |frequency: f64| {
                let coefficient =
                    2. * (std::f64::consts::TAU * frequency / f64::from(LOOPBACK_RATE)).cos();
                let (mut first, mut second) = (0., 0.);
                for sample in samples {
                    let next = f64::from(sample[channel]) + coefficient * first - second;
                    second = first;
                    first = next;
                }
                let power: f64 = first * first + second * second - coefficient * first * second;
                (2. * power.max(0.)).sqrt() / samples.len() as f64
            };
            let tones = frequencies.map(|frequency| bin(frequency));
            let tone_power = tones.iter().map(|value| value * value).sum::<f64>();
            if tone_power > best {
                best = tone_power;
                result.tones = tones;
            }
            result.rms = result.rms.max(rms);
            result.residual = result.residual.max((rms * rms - tone_power).max(0.).sqrt());
            for frequency in frequencies {
                result.guard = result
                    .guard
                    .max(bin(frequency - 50.))
                    .max(bin(frequency + 50.));
            }
        }
        result
    }
    fn json(self) -> Value {
        json!({"rms":self.rms,"signature_rms":self.tones,
            "neighbor_rms":self.guard,"residual_rms":self.residual})
    }
}

fn loopback_ready(
    capture: &crate::audio_capture::AudioCapture,
    enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator,
    endpoint: &str,
) -> Result<bool> {
    use crate::{
        audio_capture::CaptureSourceState,
        audio_devices::{default_id, EndpointKind},
    };
    let current = default_id(enumerator, EndpointKind::Render)
        .map_err(|reason| format!("无法判定：不能复核默认输出身份：{reason}"))?;
    ensure(
        current.as_deref() == Some(endpoint),
        "无法判定：默认输出端点在检查期间发生变化",
    )?;
    let sources = capture.source_status();
    ensure(
        sources.len() == 1 && sources[0].source == crate::audio_capture::AudioSource::System,
        "环回必须只有 System 来源，不能请求麦克风",
    )?;
    let source = &sources[0];
    ensure(
        source.error.is_none() && source.warning.is_none() && source.fallback.is_none(),
        format!("无法判定：环回来源失败或发生回退：{source:?}"),
    )?;
    if let Some(actual) = &source.endpoint {
        ensure(
            actual.kind == EndpointKind::Render && actual.id == endpoint,
            "无法判定：环回实际端点与本轮默认输出不一致",
        )?;
    }
    ensure(
        !matches!(
            source.state,
            CaptureSourceState::Failed | CaptureSourceState::Stopped
        ),
        "无法判定：环回来源已退出",
    )?;
    Ok(source.state == CaptureSourceState::Running && source.endpoint.is_some())
}

fn loopback_windows(
    player: &Player,
    capture: &crate::audio_capture::AudioCapture,
    keepalive: &LoopbackKeepalive,
    enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator,
    endpoint: &str,
    frequencies: [f64; 2],
    playing: bool,
    stage: &str,
    mut acceptable: impl FnMut(LoopbackStats) -> bool,
) -> Result<[LoopbackStats; 2]> {
    let started = Instant::now();
    // 先排掉控制前的包与输出队列；之后仍须取得两个完整真实 PCM 窗，不能把无包当静音。
    let settle = Duration::from_millis(200);
    let deadline = started + Duration::from_secs(2);
    let mut samples = Vec::with_capacity(LOOPBACK_WINDOW);
    let mut consecutive = Vec::with_capacity(2);
    let mut next_output = None;
    let mut last = None;
    loop {
        keepalive.pump()?;
        health(player)?;
        ensure(
            loopback_ready(capture, enumerator, endpoint)?,
            "无法判定：环回未就绪",
        )?;
        let qpc = super::qpc_now_100ns().ok_or("无法判定：不能读取本轮 QPC")?;
        let generation = status(player).generation;
        let observed = player
            .audio
            .as_ref()
            .and_then(|audio| audio.clock_at(qpc))
            .filter(|clock| clock.transport_generation == generation)
            .ok_or("无法判定：缺少当前代有界设备时钟")?;
        let expected_phase = if playing {
            ClockPhase::Playing
        } else {
            ClockPhase::Paused
        };
        ensure(
            player.playing() == playing
                && observed.clock.phase == expected_phase
                && observed.clock.advancing == playing,
            "无法判定：窗口期间播放状态或设备时钟发生欠载/结束/其它变化",
        )?;
        drop(player.take_frame_with_time());
        if Instant::now() - started < settle {
            capture.discard();
        } else {
            let reads = capture.take_available_with_timing(4_800)?;
            ensure(
                reads.len() == 1 && reads[0].0 == crate::audio_capture::AudioSource::System,
                "无法判定：环回读取来源不一致",
            )?;
            let read = &reads[0].1;
            let clocks = capture.clock_diagnostics();
            let clock = clocks
                .iter()
                .find(|(source, _)| *source == crate::audio_capture::AudioSource::System)
                .map(|(_, clock)| clock)
                .ok_or("无法判定：环回设备时钟缺失")?;
            ensure(
                read.samples.len() == read.real_frames * 2,
                "无法判定：频率统计不能包含补零",
            )?;
            for segment in &read.segments {
                ensure(
                    !segment.raw_time.discontinuity
                        && clock.accepts_raw_timestamp(segment.raw_time),
                    "无法判定：环回包不连续，不能把丢包作为静音证据",
                )?;
                if let Some(expected) = next_output {
                    ensure(
                        segment.output_start == expected,
                        "无法判定：环回缓冲发生裁剪或包序号间断",
                    )?;
                }
                next_output = Some(
                    segment
                        .output_start
                        .checked_add(segment.frames as u64)
                        .ok_or("环回帧序号溢出")?,
                );
            }
            for frame in read.samples.chunks_exact(2) {
                ensure(
                    frame
                        .iter()
                        .all(|value| value.is_finite() && value.abs() < 0.95),
                    "无法判定：环回出现非有限采样或接近满幅的外部声音",
                )?;
                samples.push([frame[0], frame[1]]);
                if samples.len() == LOOPBACK_WINDOW {
                    let measured = LoopbackStats::measure(&samples, frequencies);
                    last = Some(measured);
                    samples.clear();
                    if acceptable(measured) {
                        consecutive.push(measured);
                        if consecutive.len() == 2 {
                            keepalive.pump()?;
                            return Ok([consecutive[0], consecutive[1]]);
                        }
                    } else {
                        consecutive.clear();
                    }
                }
            }
        }
        ensure(Instant::now() < deadline, format!(
            "无法判定：{stage}未获得两个干净且符合签名的真实窗口；可能有桌面声干扰、余响或设备衰减，最后统计 {last:?}"
        ))?;
        keepalive.pump()?;
        std::thread::sleep(POLL);
    }
}

#[test]
#[ignore = "显式短暂播放低音量双频并读取同默认端点系统环回；仅内存统计、无麦克风或GUI，可能听到短音"]
fn native_player_low_volume_render_loopback_signature_mute_pause_and_close() -> Result<()> {
    use crate::{
        audio_capture::{AudioCapture, AudioSource},
        audio_devices::{resolve_endpoint, EndpointKind, EndpointSelection},
        media_foundation::MfRuntime,
    };
    use windows::Win32::{
        Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator},
        System::Com::{CoCreateInstance, CLSCTX_ALL},
    };
    let _runtime = MfRuntime::new()?;
    let fixture = LoopbackFixture::new()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let resolved = resolve_endpoint(
        &enumerator,
        EndpointKind::Render,
        &EndpointSelection::Default,
    )?;
    let keepalive = LoopbackKeepalive::open(&resolved.device)?;
    let endpoint = resolved.info.id;
    let (capture, warnings) = AudioCapture::start_with_devices(
        &[AudioSource::System],
        &EndpointSelection::Fixed {
            id: endpoint.clone(),
        },
        &EndpointSelection::Default,
    )?;
    ensure(
        warnings.is_empty(),
        format!("无法判定：环回初始化警告：{warnings:?}"),
    )?;
    let mut player =
        Player::open_file_paused_with_audio_settings(fixture.path(), LOOPBACK_VOLUME, true);
    let shutdown = player.shutdown_probe();
    let result = (|| -> Result<Value> {
        let mut first_frame = false;
        wait(
            &player,
            "等暂停首帧和同端点环回",
            CONTROL_TIMEOUT,
            || {
                keepalive.pump()?;
                let ready = loopback_ready(&capture, &enumerator, &endpoint)?;
                first_frame |= player.take_frame_with_time().is_some();
                let generation = status(&player).generation;
                keepalive.pump()?;
                Ok((ready
                    && first_frame
                    && clock(&player, generation).is_some_and(|clock| {
                        clock.clock.phase == ClockPhase::Paused && !clock.clock.advancing
                    }))
                .then_some(()))
            },
        )?;
        player.set_playing(true);
        wait(&player, "等 muted Playing 基线", CONTROL_TIMEOUT, || {
            keepalive.pump()?;
            drop(player.take_frame_with_time());
            let generation = status(&player).generation;
            keepalive.pump()?;
            Ok(clock(&player, generation)
                .filter(|clock| clock.clock.phase == ClockPhase::Playing && clock.clock.advancing))
        })?;
        let baseline = loopback_windows(
            &player,
            &capture,
            &keepalive,
            &enumerator,
            &endpoint,
            fixture.frequencies,
            true,
            "muted Playing 基线",
            |stats| stats.rms <= LOOPBACK_QUIET_RMS,
        )?;
        let floor = baseline.iter().map(|stats| stats.rms).fold(0_f64, f64::max);
        let quiet_limit = (floor * 4. + 0.000_001).max(LOOPBACK_QUIET_RMS);
        let signature_min = (floor * 10.).max(0.000_1);
        let signature = |stats: LoopbackStats| {
            let weakest = stats.tones[0].min(stats.tones[1]);
            let combined = stats.tones[0].hypot(stats.tones[1]);
            weakest >= signature_min
                && stats.guard <= (weakest * 0.1).max(quiet_limit)
                && stats.residual <= (combined * 0.1).max(quiet_limit)
        };
        player.set_muted(false);
        let first = loopback_windows(
            &player,
            &capture,
            &keepalive,
            &enumerator,
            &endpoint,
            fixture.frequencies,
            true,
            "第一次低音量实际输出",
            signature,
        )?;
        player.set_muted(true);
        let muted = loopback_windows(
            &player,
            &capture,
            &keepalive,
            &enumerator,
            &endpoint,
            fixture.frequencies,
            true,
            "mute 后排空",
            |stats| stats.rms <= quiet_limit,
        )?;
        player.set_muted(false);
        let second = loopback_windows(
            &player,
            &capture,
            &keepalive,
            &enumerator,
            &endpoint,
            fixture.frequencies,
            true,
            "再次解除静音",
            signature,
        )?;
        player.set_playing(false);
        wait(&player, "等真实设备暂停", CONTROL_TIMEOUT, || {
            keepalive.pump()?;
            drop(player.take_frame_with_time());
            let generation = status(&player).generation;
            keepalive.pump()?;
            Ok(clock(&player, generation)
                .filter(|clock| clock.clock.phase == ClockPhase::Paused && !clock.clock.advancing))
        })?;
        let paused = loopback_windows(
            &player,
            &capture,
            &keepalive,
            &enumerator,
            &endpoint,
            fixture.frequencies,
            false,
            "暂停后排空",
            |stats| stats.rms <= quiet_limit,
        )?;
        ensure(
            loopback_ready(&capture, &enumerator, &endpoint)?,
            "结束前环回退出",
        )?;
        Ok(json!({
            "ok":true,"scope":"player_render_endpoint_loopback_signature_controls",
            "run_signature":fixture.signature.to_string(),"frequencies_hz":fixture.frequencies,
            "endpoint_id":endpoint,"player_volume":LOOPBACK_VOLUME,"fixture_seconds":8,
            "analysis_window_frames":LOOPBACK_WINDOW,"consecutive_windows":2,
            "baseline":baseline.map(LoopbackStats::json),"playing":first.map(LoopbackStats::json),
            "muted":muted.map(LoopbackStats::json),"unmuted":second.map(LoopbackStats::json),
            "paused":paused.map(LoopbackStats::json),"quiet_limit":quiet_limit,
            "signature_min":signature_min,"microphone_requested":false,
            "audible_playback_requested":true,"desktop_mix_is_process_isolated":false,
            "silent_render_keepalive":true,
            "signature_control_pattern_matched":true,
            "captured_pcm_written":false,"gui_observed":false,"presented_observed":false,
            "physical_speaker_or_audibility_verified":false,
        }))
    })();
    // 包括判据失败在内，先撤销可听输出；随后等待 video/audio 两个 owner 退出。
    player.set_muted(true);
    player.set_volume(0.);
    let closing = Instant::now();
    drop(player);
    let closed = wait_shutdown(&shutdown, closing);
    // AudioCapture::Drop 同步停止并 join 环回线程；原始采样只曾存在有界内存中。
    drop(capture);
    drop(keepalive);
    match (result, closed) {
        (Ok(mut report), Ok(elapsed)) => {
            report["workers_exited"] = true.into();
            report["drop_to_workers_exited_ms"] = (elapsed.as_secs_f64() * 1000.).into();
            report["loopback_capture_joined"] = true.into();
            println!("{report}");
            Ok(())
        }
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close)) => Err(format!("{error}；关闭验证也失败：{close}").into()),
    }
}
