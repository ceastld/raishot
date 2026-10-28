//! 无窗口、无声音输出的媒体自检，只读写调用者目录内本轮生成的合成 MP4。
use crate::{
    media_foundation::{
        AudioReader, MfRuntime, Mp4Writer, VideoReader, AUDIO_CHANNELS, AUDIO_RATE,
    },
    Result,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::Win32::Media::MediaFoundation::MFVideoFormat_H264;

#[cfg(test)]
#[path = "media_playback_long_smoke.rs"]
mod long_smoke;
#[path = "media_playback_player_smoke.rs"]
mod player_smoke;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FPS: u32 = 30;
const SECONDS: u32 = 3;
const FRAMES: u32 = FPS * SECONDS;
const BITRATE: u32 = 1_500_000;
const TOLERANCE: Duration = Duration::from_millis(250);
const CASE_BUDGET: Duration = Duration::from_secs(60);
const MAX_VIDEO_FRAMES: u32 = FRAMES * 2;
const MAX_AUDIO_BLOCKS: u32 = 4096;
const EXPECTED: Duration = Duration::from_secs(SECONDS as u64);

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn within(actual: Duration, expected: Duration, tolerance: Duration) -> bool {
    actual.abs_diff(expected) <= tolerance
}

fn video_time(frame: u32) -> Duration {
    Duration::from_nanos(u64::from(frame) * 1_000_000_000 / u64::from(FPS))
}

/// 原子创建全新子目录，不打开或覆盖调用者已经存在的媒体。
fn create_output(directory: &Path) -> Result<PathBuf> {
    let directory = if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        std::env::current_dir()?.join(directory)
    };
    fs::create_dir_all(&directory)?;
    for index in 1..=10_000 {
        let path = directory.join(format!("media-playback-{index:04}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("媒体自检输出目录过多，请指定其它目录".into())
}

fn check_budget(started: Instant) -> Result<()> {
    ensure(
        started.elapsed() <= CASE_BUDGET,
        "媒体自检超出单样本处理时限",
    )
}

/// 每帧仅生成一个小 BGRA 面。左上灰块随媒体时间变亮，便于回读检查帧确实推进。
fn synthetic_frame(index: u32) -> Vec<u8> {
    let mut pixels = vec![0; WIDTH as usize * HEIGHT as usize * 4];
    let moving_x = index * (WIDTH - 32) / (FRAMES - 1);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let pixel = &mut pixels[((y * WIDTH + x) * 4) as usize..][..4];
            let color = if x < 80 && y < 80 {
                let level = (40 + index * 2) as u8;
                [level; 3]
            } else if x >= moving_x && x < moving_x + 32 && (100..180).contains(&y) {
                [48, 180, 72]
            } else {
                [32 + (x / 40) as u8 * 8, 24 + (y / 40) as u8 * 8, 18]
            };
            pixel.copy_from_slice(&[color[0], color[1], color[2], 255]);
        }
    }
    pixels
}

/// 双声道分别为 440/660Hz 正弦；不生成或使用辅助 WAV。
fn synthetic_audio(video_frame: u32) -> Vec<i16> {
    let frames = AUDIO_RATE / FPS;
    let first = u64::from(video_frame) * u64::from(frames);
    let mut pcm = Vec::with_capacity(frames as usize * AUDIO_CHANNELS as usize);
    for offset in 0..frames {
        let time = (first + u64::from(offset)) as f64 / f64::from(AUDIO_RATE);
        for frequency in [440., 660.] {
            let value = (std::f64::consts::TAU * frequency * time).sin() * 0.2;
            pcm.push((value * f64::from(i16::MAX)) as i16);
        }
    }
    pcm
}

fn encode(path: &Path, with_audio: bool, started: Instant, diagnostics: &mut Value) -> Result<()> {
    let mut writer = Mp4Writer::new(path, WIDTH, HEIGHT, FPS, BITRATE, with_audio)?;
    let gpu_pixel_input = writer.uses_gpu_input();
    let writing = (|| -> Result<()> {
        for index in 0..FRAMES {
            check_budget(started)?;
            let time = video_time(index);
            let duration = video_time(index + 1) - time;
            writer.write_video(&synthetic_frame(index), WIDTH, time, duration)?;
            if with_audio {
                writer.write_audio(&synthetic_audio(index), time)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = writing {
        *diagnostics = writer.diagnostics().clone();
        diagnostics["gpu_pixel_input"] = gpu_pixel_input.into();
        return Err(error);
    }
    let (finalized, final_diagnostics) = writer.finish_with_diagnostics();
    *diagnostics = final_diagnostics;
    diagnostics["gpu_pixel_input"] = gpu_pixel_input.into();
    if let Err(error) = finalized {
        ensure(
            diagnostics["encoding_trial"]["finalize"]["state"] == "failed"
                && diagnostics["encoding_trial"]["finalize"]["error"].is_string(),
            format!("封装失败且最终诊断不一致：{error}"),
        )?;
        return Err(error);
    }
    ensure(
        diagnostics["encoding_trial"]["finalize"]["state"] == "succeeded",
        "编码器未记录实际封装成功",
    )?;
    ensure(
        diagnostics["encoding_trial"]["reopen"]["state"] == "not_attempted",
        "写入器提前宣称已重开验证",
    )?;
    ensure(fs::metadata(path)?.len() > 0, "合成 MP4 为空")?;
    Ok(())
}

fn patch_level(pixels: &[u8]) -> f64 {
    let mut total = 0_u64;
    for y in 16..48 {
        for x in 16..48 {
            let at = ((y * WIDTH + x) * 4) as usize;
            total += u64::from(pixels[at + 1]);
        }
    }
    total as f64 / (32. * 32.)
}

fn inspect_video(path: &Path, started: Instant) -> Result<Value> {
    let mut reader = VideoReader::open(path)?;
    ensure(
        (reader.width, reader.height) == (WIDTH, HEIGHT),
        "视频回读尺寸不符",
    )?;
    ensure(
        reader.info.codec == MFVideoFormat_H264,
        "合成文件不是 H.264 视频",
    )?;
    ensure(
        within(reader.duration, EXPECTED, TOLERANCE),
        "视频容器时长不符",
    )?;
    let mut count = 0_u32;
    let mut first = None;
    let mut last = Duration::ZERO;
    let mut first_level = 0.;
    let mut last_level = 0.;
    while let Some(frame) = reader.next()? {
        check_budget(started)?;
        count += 1;
        ensure(count <= MAX_VIDEO_FRAMES, "视频回读帧数超限")?;
        ensure(
            frame.bgra.len() == WIDTH as usize * HEIGHT as usize * 4,
            "视频回读像素长度不符",
        )?;
        if first.is_none() {
            first = Some(frame.time);
            first_level = patch_level(&frame.bgra);
        } else {
            ensure(frame.time >= last, "视频时间戳倒退")?;
        }
        last = frame.time;
        last_level = patch_level(&frame.bgra);
    }
    let first = first.ok_or("视频没有解码帧")?;
    ensure(
        (FRAMES - 2..=FRAMES + 2).contains(&count),
        "视频解码帧数不符",
    )?;
    ensure(
        first <= TOLERANCE && last > first && within(last, EXPECTED, TOLERANCE),
        "视频时间戳覆盖不足",
    )?;
    ensure(last_level > first_level + 80., "合成视频画面没有随时间推进")?;
    Ok(json!({
        "codec": "H.264", "width": reader.width, "height": reader.height,
        "duration_seconds": reader.duration.as_secs_f64(), "frame_count": count,
        "first_seconds": first.as_secs_f64(), "last_seconds": last.as_secs_f64(),
        "first_patch_level": first_level, "last_patch_level": last_level,
        "nominal_frame_rate": reader.info.frame_rate, "rotation_degrees": reader.info.rotation_degrees,
    }))
}

fn inspect_audio(path: &Path, expected_audio: bool, started: Instant) -> Result<Value> {
    let Some(mut reader) = AudioReader::open(path)? else {
        ensure(!expected_audio, "有声 MP4 未探测到内嵌音轨")?;
        return Ok(json!({"present": false}));
    };
    ensure(expected_audio, "无声 MP4 意外出现音轨")?;
    ensure(
        (reader.rate, reader.channels) == (AUDIO_RATE, AUDIO_CHANNELS as usize),
        "音轨回读格式不符",
    )?;
    let mut blocks = 0_u32;
    let mut samples = 0_u64;
    let mut max_block = 0_usize;
    let mut energy = 0.;
    let mut first = None;
    let mut last_start = Duration::ZERO;
    let mut last_end = Duration::ZERO;
    while let Some(block) = reader.next()? {
        check_budget(started)?;
        blocks += 1;
        ensure(blocks <= MAX_AUDIO_BLOCKS, "音频回读块数超限")?;
        ensure(
            !block.samples.is_empty() && block.samples.len() % reader.channels == 0,
            "音频回读块无效",
        )?;
        ensure(
            block.samples.len() <= reader.rate as usize * reader.channels * 2,
            "音频单块超过两秒缓冲限制",
        )?;
        if first.is_none() {
            first = Some(block.time);
        } else {
            ensure(block.time >= last_start, "音频时间戳倒退")?;
        }
        last_start = block.time;
        let frames = block.samples.len() / reader.channels;
        last_end = block.time + Duration::from_secs_f64(frames as f64 / f64::from(reader.rate));
        samples += block.samples.len() as u64;
        max_block = max_block.max(block.samples.len());
        for sample in block.samples {
            ensure(sample.is_finite(), "音频解码产生非有限样本")?;
            energy += f64::from(sample) * f64::from(sample);
        }
    }
    ensure(blocks >= 2 && samples > 0, "音轨未逐块返回有效声音")?;
    let first = first.ok_or("音轨没有解码块")?;
    let decoded_duration =
        Duration::from_secs_f64(samples as f64 / reader.channels as f64 / f64::from(reader.rate));
    let rms = (energy / samples as f64).sqrt();
    ensure(
        first <= TOLERANCE && within(last_end, EXPECTED, TOLERANCE),
        "音频时间戳覆盖不足",
    )?;
    ensure(
        within(decoded_duration, EXPECTED, TOLERANCE),
        "音频解码时长不符",
    )?;
    ensure(rms > 0.01 && rms < 0.5, "合成音轨能量不符")?;
    Ok(json!({
        "present": true, "rate": reader.rate, "channels": reader.channels,
        "block_count": blocks, "sample_count": samples, "max_block_samples": max_block,
        "first_seconds": first.as_secs_f64(), "last_end_seconds": last_end.as_secs_f64(),
        "decoded_seconds": decoded_duration.as_secs_f64(), "rms": rms,
    }))
}

fn inspect_seeks(path: &Path, with_audio: bool, started: Instant) -> Result<Value> {
    let mut video = VideoReader::open(path)?;
    let mut audio = AudioReader::open(path)?;
    ensure(audio.is_some() == with_audio, "跳转时音轨存在状态改变")?;
    let mut checks = Vec::new();
    // 先到中间，再回到零点，验证 EOF 后重定位及旧数据失效。
    for count in 0..=MAX_VIDEO_FRAMES {
        check_budget(started)?;
        if video.next()?.is_none() {
            break;
        }
        ensure(count < MAX_VIDEO_FRAMES, "跳转预读视频未结束")?;
    }
    if let Some(audio) = audio.as_mut() {
        for count in 0..=MAX_AUDIO_BLOCKS {
            check_budget(started)?;
            if audio.next()?.is_none() {
                break;
            }
            ensure(count < MAX_AUDIO_BLOCKS, "跳转预读音轨未结束")?;
        }
    }
    for target in [Duration::from_millis(1500), Duration::ZERO] {
        check_budget(started)?;
        video.seek(target)?;
        let mut found = None;
        let mut preroll = 0_u32;
        for _ in 0..MAX_VIDEO_FRAMES {
            check_budget(started)?;
            let Some(frame) = video.next()? else {
                break;
            };
            preroll += 1;
            if frame.time + video_time(1) >= target {
                ensure(frame.time <= target + TOLERANCE, "视频跳转越过目标太远")?;
                ensure(
                    frame.bgra.len() == WIDTH as usize * HEIGHT as usize * 4,
                    "跳转帧像素无效",
                )?;
                found = Some(frame.time);
                break;
            }
        }
        let video_time = found.ok_or("视频跳转后没有目标帧")?;
        let mut audio_time = None;
        let mut audio_blocks = 0_u32;
        if let Some(audio) = audio.as_mut() {
            audio.seek(target)?;
            for _ in 0..MAX_AUDIO_BLOCKS {
                check_budget(started)?;
                let Some(block) = audio.next()? else {
                    break;
                };
                audio_blocks += 1;
                let frames = block.samples.len() / audio.channels;
                let end =
                    block.time + Duration::from_secs_f64(frames as f64 / f64::from(audio.rate));
                if end > target {
                    ensure(block.time <= target + TOLERANCE, "音频跳转越过目标太远")?;
                    audio_time = Some(block.time.as_secs_f64());
                    break;
                }
            }
            ensure(audio_time.is_some(), "音频跳转后没有目标块")?;
        }
        checks.push(json!({
            "target_seconds": target.as_secs_f64(), "video_seconds": video_time.as_secs_f64(),
            "video_preroll_frames": preroll, "audio_seconds": audio_time, "audio_preroll_blocks": audio_blocks,
        }));
    }
    Ok(Value::Array(checks))
}

fn run_case(path: &Path, with_audio: bool) -> Value {
    let started = Instant::now();
    let mut report =
        json!({"ok": false, "path": path, "with_audio": with_audio, "stage": "encode"});
    let result = (|| -> Result<()> {
        let mut diagnostics = Value::Null;
        let encoded = encode(path, with_audio, started, &mut diagnostics);
        report["encoder"] = diagnostics;
        encoded?;
        report["stage"] = "decode_video".into();
        report["video"] = inspect_video(path, started)?;
        report["stage"] = "decode_audio".into();
        report["audio"] = inspect_audio(path, with_audio, started)?;
        report["stage"] = "seek".into();
        report["seek"] = inspect_seeks(path, with_audio, started)?;
        report["stage"] = "player_control".into();
        report["player"] = player_smoke::run(path)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            report["ok"] = true.into();
            report["stage"] = "complete".into();
        }
        Err(error) => report["error"] = error.to_string().into(),
    }
    report["elapsed_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
    report
}

/// 原生时间戳小样：0、40、120ms 起步，随后交替 20/60ms，三秒内共 74 帧。
fn vfr_timeline() -> Vec<Duration> {
    let mut times = vec![Duration::ZERO, Duration::from_millis(40)];
    let mut cursor = Duration::from_millis(120);
    for _ in 0..36 {
        times.push(cursor);
        times.push(cursor + Duration::from_millis(20));
        cursor += Duration::from_millis(80);
    }
    times
}

fn run_vfr_case(path: &Path) -> Value {
    let started = Instant::now();
    let mut report = json!({"ok":false,"path":path,"with_audio":false,"stage":"encode_vfr"});
    let result = (|| -> Result<()> {
        let times = vfr_timeline();
        let mut writer = Mp4Writer::new(path, WIDTH, HEIGHT, FPS, BITRATE, false)?;
        for (index, time) in times.iter().enumerate() {
            check_budget(started)?;
            let end = times.get(index + 1).copied().unwrap_or(EXPECTED);
            let frame = index as u32 * (FRAMES - 1) / (times.len() as u32 - 1);
            writer.write_video(&synthetic_frame(frame), WIDTH, *time, end - *time)?;
        }
        let (finish, diagnostics) = writer.finish_with_diagnostics();
        report["encoder"] = diagnostics;
        finish?;
        report["stage"] = "decode_vfr".into();
        let mut reader = VideoReader::open(path)?;
        let mut decoded = Vec::new();
        while let Some(frame) = reader.next()? {
            check_budget(started)?;
            ensure(decoded.len() < times.len(), "变帧率样本生成了额外帧")?;
            let expected = times[decoded.len()];
            if frame.time.abs_diff(expected) > Duration::from_millis(1) {
                return Err(format!(
                    "变帧率样本时间戳未保留：第{}帧，期望{}ns，实际{}ns，解码路径{}",
                    decoded.len(),
                    expected.as_nanos(),
                    frame.time.as_nanos(),
                    reader.diagnostics()
                )
                .into());
            }
            decoded.push(frame.time);
        }
        ensure(decoded.len() == times.len(), "变帧率样本丢失帧")?;
        report["frame_count"] = decoded.len().into();
        report["stage"] = "player_vfr_control".into();
        report["player"] = player_smoke::run_timeline(path, &decoded, Duration::from_millis(95))?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            report["ok"] = true.into();
            report["stage"] = "complete".into();
        }
        Err(error) => report["error"] = error.to_string().into(),
    }
    report["elapsed_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
    report
}

/// 同步后台操作，调用者自行放入工作线程。case 失败保留样本并返回 ok:false；
/// 调用者应检查顶层 ok 决定命令退出状态，Err 仅表示不能创建自有输出目录。
pub(crate) fn run(directory: &Path) -> Result<Value> {
    let output = create_output(directory)?;
    let runtime = match MfRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            return Ok(json!({
                "ok": false, "synthetic_only": true, "audio_output": false,
                "directory": output, "stage": "mf_startup", "error": error.to_string(), "cases": [],
            }))
        }
    };
    let cases = [
        run_case(&output.join("h264-aac.mp4"), true),
        run_case(&output.join("h264-silent.mp4"), false),
        run_vfr_case(&output.join("h264-vfr.mp4")),
    ];
    drop(runtime);
    Ok(json!({
        "ok": cases.iter().all(|case| case["ok"] == true),
        "synthetic_only": true, "audio_output": false, "directory": output,
        "expected": {"width": WIDTH, "height": HEIGHT, "fps": FPS, "seconds": SECONDS, "audio_rate": AUDIO_RATE, "audio_channels": AUDIO_CHANNELS},
        "duration_tolerance_ms": TOLERANCE.as_millis(), "cases": cases,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "显式运行系统H.264/AAC合成媒体集成检查，不播放声音"]
    fn media_playback_smoke_native_h264_aac_roundtrip() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/media-playback-smoke");
        let report = run(&directory).expect("创建本轮合成媒体输出目录");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        assert_eq!(report["synthetic_only"], true);
        assert_eq!(report["audio_output"], false);
        assert_eq!(report["ok"], true, "系统媒体编解码或回读失败：{report}");
    }

    #[test]
    #[ignore = "显式验证本轮自造媒体的规范化、特殊字符和长文件路径；无窗口或声音输出"]
    fn media_playback_smoke_native_canonical_file_paths() -> Result<()> {
        use std::os::windows::ffi::OsStrExt;

        let directory = crate::config_json::TestDir::new("media-file-paths");
        let _runtime = MfRuntime::new()?;
        let source = directory.join("source.mp4");
        let mut encoder = Value::Null;
        encode(&source, true, Instant::now(), &mut encoder)?;
        let special = directory.join("片段 空格 # 100%.mp4");
        fs::copy(&source, &special)?;

        // 从自有目录的规范路径扩展，保留 Windows 长路径语义；不访问现有共享或云盘。
        let mut long_directory = fs::canonicalize(&directory.0)?;
        while long_directory.as_os_str().encode_wide().count() < 320 {
            long_directory = long_directory.join("media-path-component-01234567890123456789");
            fs::create_dir(&long_directory)?;
        }
        let long = long_directory.join("片段 空格 # 100%.mp4");
        fs::copy(&source, &long)?;
        let paths = [
            ("absolute", source.clone()),
            ("canonical", fs::canonicalize(&source)?),
            ("literal_special_characters", fs::canonicalize(&special)?),
            ("extended_long", long),
        ];
        let mut cases = Vec::new();
        for (name, path) in paths {
            let started = Instant::now();
            let mut report = json!({"case":name,"ok":false});
            let result = (|| -> Result<()> {
                // 走真实读取入口，完整解码同一份 H.264/AAC，再验证 seek；不只检查可打开。
                report["video"] = inspect_video(&path, started)?;
                report["audio"] = inspect_audio(&path, true, started)?;
                report["seek"] = inspect_seeks(&path, true, started)?;
                Ok(())
            })();
            if let Err(error) = result {
                report["error"] = error.to_string().into();
                println!("{report}");
                return Err(format!("媒体文件路径检查 {name} 失败：{error}").into());
            }
            report["ok"] = true.into();
            report["path_utf16_units"] = path.as_os_str().encode_wide().count().into();
            cases.push(report);
        }
        println!(
            "{}",
            json!({
                "ok":true,"scope":"self_generated_file_paths",
                "cases":cases,"encoder":encoder,
                "unc_share_tested":false,"audio_output":false,"gui":false,
            })
        );
        Ok(())
    }

    #[test]
    fn media_playback_smoke_fixture_timeline_and_pattern_advance() {
        assert_eq!(video_time(FRAMES), EXPECTED);
        let first = synthetic_frame(0);
        let last = synthetic_frame(FRAMES - 1);
        assert_eq!(first.len(), WIDTH as usize * HEIGHT as usize * 4);
        assert!(patch_level(&last) > patch_level(&first) + 80.);
        assert_eq!(
            synthetic_audio(0).len(),
            (AUDIO_RATE / FPS * AUDIO_CHANNELS) as usize
        );
        assert!(synthetic_audio(0).iter().any(|value| *value > 0));
        assert!(synthetic_audio(0).iter().any(|value| *value < 0));
        assert!(within(EXPECTED + TOLERANCE, EXPECTED, TOLERANCE));
        assert!(!within(
            EXPECTED + TOLERANCE + Duration::from_millis(1),
            EXPECTED,
            TOLERANCE
        ));
    }
}
