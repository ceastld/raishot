//! 显式执行的录屏分轨重混探针；只验证编码、原生样本和取消后的源内容。
use crate::{
    audio_capture::{AudioSource, MixLevels},
    media_foundation::{AudioReader, MfRuntime, Mp4Writer, AUDIO_RATE},
    preview_audio::remux_recording_tracks,
    wav_track::WavWriter,
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::Media::MediaFoundation::*,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FPS: u32 = 10;
const SECONDS: u32 = 3;
const VIDEO_FRAMES: usize = (FPS * SECONDS) as usize;
const MAX_SMALL_FILE_BYTES: u64 = 2 * 1024 * 1024;
const STEP_BUDGET: Duration = Duration::from_secs(60);
const FIRST_LEVELS: MixLevels = MixLevels {
    system: 150,
    microphone: 75,
};
const MUTED_LEVELS: MixLevels = MixLevels {
    system: 0,
    microphone: 150,
};
const OWN_FILES: [&str; 6] = [
    "source.mp4",
    "system.wav",
    "microphone.wav",
    "mixed.mp4",
    "muted.mp4",
    "cancelled.mp4",
];

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn check_deadline(deadline: Instant, stage: &str) -> Result<()> {
    ensure(Instant::now() < deadline, format!("{stage}超过六十秒"))
}

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustshot-audio-mix-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // 原子取得全新目录；所有后续文件名仅存在于本轮自有目录。
        fs::create_dir(&directory)?;
        Ok(Self { directory })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in OWN_FILES {
            let _ = fs::remove_file(self.path(name));
        }
        // 只删除已知自有文件和空目录，不递归清理未知内容。
        let _ = fs::remove_dir(&self.directory);
    }
}

fn snapshot_small_file(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    ensure(
        bytes > 0 && bytes <= MAX_SMALL_FILE_BYTES,
        format!("自造小样大小异常：{}，{bytes} bytes", path.display()),
    )?;
    let mut contents = Vec::with_capacity(bytes as usize);
    file.take(MAX_SMALL_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    ensure(
        contents.len() as u64 == bytes,
        "自造小样在快照期间变化或超过大小上限",
    )?;
    Ok(contents)
}

fn create_fixture(fixture: &Fixture) -> Result<()> {
    let deadline = Instant::now() + STEP_BUDGET;
    let source = fixture.path("source.mp4");
    ensure(!source.exists(), "不得覆盖既有 MP4")?;
    // 使用现有 RGB32 入口，不创建 GPU 输入表面或音频输出。
    let mut writer =
        Mp4Writer::new_legacy_for_benchmark(&source, WIDTH, HEIGHT, FPS, 500_000, false)?;
    for index in 0..VIDEO_FRAMES {
        check_deadline(deadline, "H.264 夹具编码")?;
        let mut pixels = vec![0_u8; WIDTH as usize * HEIGHT as usize * 4];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let offset = ((y * WIDTH + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&[
                    24 + (x / 32) as u8 * 4,
                    32 + (y / 24) as u8 * 5,
                    40 + index as u8 * 3,
                    255,
                ]);
            }
        }
        writer.write_video(
            &pixels,
            WIDTH,
            Duration::from_millis(index as u64 * 100),
            Duration::from_millis(100),
        )?;
    }
    writer.finish()?;
    check_deadline(deadline, "H.264 夹具收尾")?;

    for (name, channel, frequency, amplitude) in [
        ("system.wav", 0_usize, 440_f64, 0.12_f64),
        ("microphone.wav", 1_usize, 880_f64, 0.08_f64),
    ] {
        let path = fixture.path(name);
        ensure(!path.exists(), "不得覆盖既有 WAV")?;
        let mut writer = WavWriter::create(&path)?;
        for second in 0..SECONDS {
            check_deadline(deadline, "WAV 夹具写入")?;
            let mut samples = Vec::with_capacity(AUDIO_RATE as usize * 2);
            for frame in 0..AUDIO_RATE {
                let absolute = u64::from(second) * u64::from(AUDIO_RATE) + u64::from(frame);
                let value = (std::f64::consts::TAU * frequency * absolute as f64
                    / f64::from(AUDIO_RATE))
                .sin()
                    * amplitude;
                let mut pair = [0_f32; 2];
                pair[channel] = value as f32;
                samples.extend_from_slice(&pair);
            }
            writer.write(&samples)?;
        }
        writer.finish()?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct NativeVideoSample {
    reader_time: i64,
    sample_time: i64,
    duration: i64,
    bytes: Vec<u8>,
}

fn native_video(path: &Path) -> Result<Vec<NativeVideoSample>> {
    let deadline = Instant::now() + STEP_BUDGET;
    let url = HSTRING::from(path.as_os_str());
    unsafe {
        let reader = MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), None)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        ensure(
            reader
                .GetNativeMediaType(stream, 0)?
                .GetGUID(&MF_MT_SUBTYPE)?
                == MFVideoFormat_H264
                && reader
                    .GetCurrentMediaType(stream)?
                    .GetGUID(&MF_MT_SUBTYPE)?
                    == MFVideoFormat_H264,
            "视频比较必须取得原生 H.264 压缩数据",
        )?;
        let mut samples = Vec::new();
        let mut total_bytes = 0_usize;
        for _ in 0..VIDEO_FRAMES * 4 {
            check_deadline(deadline, "原生视频样本回读")?;
            let mut flags = 0_u32;
            let mut timestamp = 0_i64;
            let mut sample = None;
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
            check_deadline(deadline, "原生视频样本回读")?;
            ensure(
                flags
                    & (MF_SOURCE_READERF_ERROR.0
                        | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0
                        | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) as u32
                    == 0,
                format!("原生视频样本回读返回错误或格式变化：flags={flags:#x}"),
            )?;
            if let Some(sample) = sample {
                let length = sample.GetTotalLength()? as usize;
                ensure(length > 0, "原生视频样本为空")?;
                total_bytes = total_bytes.checked_add(length).ok_or("原生样本总长溢出")?;
                ensure(
                    samples.len() < VIDEO_FRAMES * 2 && total_bytes as u64 <= MAX_SMALL_FILE_BYTES,
                    "原生压缩小样超过数量或内存上限",
                )?;
                let sample_time = sample.GetSampleTime()?;
                let duration = sample.GetSampleDuration()?;
                ensure(duration > 0, "原生视频样本没有正时长")?;
                let buffer = sample.ConvertToContiguousBuffer()?;
                ensure(
                    buffer.GetCurrentLength()? as usize == length,
                    "原生视频连续缓冲区长度变化",
                )?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None)?;
                let bytes = std::slice::from_raw_parts(data, length).to_vec();
                buffer.Unlock()?;
                samples.push(NativeVideoSample {
                    reader_time: timestamp,
                    sample_time,
                    duration,
                    bytes,
                });
            }
            // 最后一个样本与 EOS 同时返回时也须先纳入比较。
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                ensure(samples.len() == VIDEO_FRAMES, "夹具原生视频样本数不是 30")?;
                return Ok(samples);
            }
        }
    }
    Err("原生视频样本回读未在小样上限内结束".into())
}

fn compare_video(source: &[NativeVideoSample], target: &Path) -> Result<()> {
    let actual = native_video(target)?;
    ensure(actual.len() == source.len(), "重混后原生视频样本数变化")?;
    for (index, (before, after)) in source.iter().zip(&actual).enumerate() {
        ensure(
            before.reader_time == after.reader_time
                && before.sample_time == after.sample_time
                && before.duration == after.duration,
            format!(
                "视频样本 {index} 时间变化：源 ({}, {}, {})，目标 ({}, {}, {})",
                before.reader_time,
                before.sample_time,
                before.duration,
                after.reader_time,
                after.sample_time,
                after.duration
            ),
        )?;
        ensure(
            before.bytes == after.bytes,
            format!("重混后视频压缩样本 {index} 内容变化"),
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct AudioStats {
    rms: [f64; 2],
    peak: [f32; 2],
    center_frames: u64,
    decoded_frames: u64,
}

impl AudioStats {
    fn report(&self) -> Value {
        json!({
            "rms": self.rms,
            "peak": self.peak,
            "center_frames": self.center_frames,
            "decoded_frames": self.decoded_frames,
        })
    }
}

fn aac_stats(path: &Path) -> Result<AudioStats> {
    let deadline = Instant::now() + STEP_BUDGET;
    let url = HSTRING::from(path.as_os_str());
    {
        let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), None)? };
        let native =
            unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)? };
        ensure(
            unsafe { native.GetGUID(&MF_MT_SUBTYPE)? } == MFAudioFormat_AAC,
            "重混目标没有真实 AAC 音轨",
        )?;
    }
    let mut reader = AudioReader::open(path)?.ok_or("重混目标缺少可解码音轨")?;
    ensure(
        reader.rate == AUDIO_RATE && reader.channels == 2,
        "AAC 解码没有保留 48 kHz 立体声",
    )?;
    let mut squares = [0_f64; 2];
    let mut peak = [0_f32; 2];
    let mut center_frames = 0_u64;
    let mut decoded_frames = 0_u64;
    let mut previous = None;
    for _ in 0..512 {
        check_deadline(deadline, "AAC 回读")?;
        let next = reader.next()?;
        check_deadline(deadline, "AAC 回读")?;
        let Some(block) = next else {
            ensure(
                center_frames.abs_diff(u64::from(AUDIO_RATE) * 2) <= 2048,
                format!("AAC 中段样本量不符：{center_frames} 帧"),
            )?;
            ensure(
                decoded_frames.abs_diff(u64::from(AUDIO_RATE) * u64::from(SECONDS)) <= 4096,
                format!("AAC 总样本量不符：{decoded_frames} 帧"),
            )?;
            return Ok(AudioStats {
                rms: squares.map(|sum| (sum / center_frames as f64).sqrt()),
                peak,
                center_frames,
                decoded_frames,
            });
        };
        ensure(
            previous.is_none_or(|time| block.time >= time),
            "AAC 解码 PTS 倒退",
        )?;
        previous = Some(block.time);
        ensure(block.samples.len() % 2 == 0, "AAC 解码返回半帧")?;
        decoded_frames = decoded_frames
            .checked_add((block.samples.len() / 2) as u64)
            .ok_or("AAC 解码计数溢出")?;
        ensure(
            decoded_frames <= u64::from(AUDIO_RATE) * 4,
            "AAC 小样超过四秒解码上限",
        )?;
        for (index, pair) in block.samples.chunks_exact(2).enumerate() {
            ensure(
                pair.iter().all(|value| value.is_finite()),
                "AAC 解码有非有限值",
            )?;
            let time = block.time.as_secs_f64() + index as f64 / f64::from(AUDIO_RATE);
            // 排除首尾各半秒，对 AAC 编码延迟及填充保留余量。
            if (0.5..2.5).contains(&time) {
                center_frames += 1;
                for channel in 0..2 {
                    squares[channel] += f64::from(pair[channel]).powi(2);
                    peak[channel] = peak[channel].max(pair[channel].abs());
                }
            }
        }
    }
    Err("AAC 回读未在小样块数上限内结束".into())
}

fn relative(actual: f64, expected: f64, tolerance: f64, label: &str) -> Result<()> {
    ensure(
        actual.is_finite() && expected > 0. && (actual - expected).abs() <= expected * tolerance,
        format!("{label} 超差：actual={actual:.6}，expected={expected:.6}，容差={tolerance}"),
    )
}

fn verify_levels(first: &AudioStats, muted: &AudioStats) -> Result<()> {
    let root_two = std::f64::consts::SQRT_2;
    relative(first.rms[0], 0.12 * 1.5 / root_two, 0.20, "系统增益 RMS")?;
    relative(first.rms[1], 0.08 * 0.75 / root_two, 0.20, "麦克风增益 RMS")?;
    relative(first.rms[0] / first.rms[1], 3., 0.15, "左右增益比例")?;
    relative(
        muted.rms[1],
        0.08 * 1.5 / root_two,
        0.20,
        "静音组合的非零 RMS",
    )?;
    relative(
        muted.rms[1] / first.rms[1],
        2.,
        0.15,
        "两组合右声道幅度比例",
    )?;
    ensure(
        muted.rms[0] <= (muted.rms[1] * 0.03).max(0.0015),
        format!("系统分轨静音后左声道泄漏过大：{:?}", muted.rms),
    )?;
    ensure(
        first.peak.iter().chain(&muted.peak).all(|peak| *peak < 0.5),
        "低幅合成混音发生异常峰值或削波",
    )
}

fn exercise(fixture: &Fixture) -> Result<Value> {
    create_fixture(fixture)?;
    let source = fixture.path("source.mp4");
    let tracks = [
        (AudioSource::System, fixture.path("system.wav")),
        (AudioSource::Microphone, fixture.path("microphone.wav")),
    ];
    let source_bytes = snapshot_small_file(&source)?;
    let track_bytes = [
        snapshot_small_file(&tracks[0].1)?,
        snapshot_small_file(&tracks[1].1)?,
    ];
    let original_video = native_video(&source)?;
    for (name, levels) in [("mixed.mp4", FIRST_LEVELS), ("muted.mp4", MUTED_LEVELS)] {
        let target = fixture.path(name);
        let deadline = Instant::now() + STEP_BUDGET;
        remux_recording_tracks(&source, &target, &tracks, levels, || {
            Instant::now() < deadline
        })?;
        check_deadline(deadline, "分轨重混")?;
        compare_video(&original_video, &target)?;
    }
    let first = aac_stats(&fixture.path("mixed.mp4"))?;
    let muted = aac_stats(&fixture.path("muted.mp4"))?;
    verify_levels(&first, &muted)?;

    let deadline = Instant::now() + STEP_BUDGET;
    let mut checks = 0_u32;
    let mut timed_out = false;
    let cancelled = fixture.path("cancelled.mp4");
    let cancel_result = remux_recording_tracks(&source, &cancelled, &tracks, FIRST_LEVELS, || {
        checks += 1;
        timed_out |= Instant::now() >= deadline;
        checks < 20 && !timed_out
    });
    ensure(!timed_out, "取消探针超过六十秒")?;
    ensure(checks == 20, format!("没有走到预定中途取消点：{checks} 次"))?;
    let cancel_error = cancel_result.err().ok_or("中途取消未返回错误")?.to_string();
    ensure(
        cancel_error.contains("取消") || cancel_error.to_ascii_lowercase().contains("cancel"),
        format!("中途取消返回了其它错误：{cancel_error}"),
    )?;
    ensure(
        snapshot_small_file(&source)? == source_bytes,
        "重混或取消修改了源 MP4 内容",
    )?;
    for (index, (_, path)) in tracks.iter().enumerate() {
        ensure(
            snapshot_small_file(path)? == track_bytes[index],
            format!("重混或取消修改了原始 WAV 分轨 {index}"),
        )?;
    }
    Ok(json!({
        "first_levels": FIRST_LEVELS,
        "muted_levels": MUTED_LEVELS,
        "first_aac": first.report(),
        "muted_aac": muted.report(),
        "aac_center_seconds": [0.5, 2.5],
        "native_video_samples_equal": original_video.len(),
        "native_video_comparison": ["bytes", "reader_time", "sample_time", "duration"],
        "cancel_checks": checks,
        "cancel_error": cancel_error,
        "source_and_wavs_unchanged": true,
    }))
}

#[test]
#[ignore = "显式运行系统 H.264/AAC 分轨重混、幅度、原生视频样本和取消检查；无音频输出"]
fn native_preview_audio_recording_mix_aac_video_passthrough_and_cancel() -> Result<()> {
    let fixture = Fixture::new()?;
    let result = (|| {
        // 本线程的所有 MF 读写器均在 exercise 返回前释放，再释放运行时。
        let _runtime = MfRuntime::new()?;
        exercise(&fixture)
    })();
    let mut report = json!({
        "scope": "synthetic_recording_tracks_native_remux_only",
        "wasapi": false,
        "audible_output": false,
        "gui": false,
        "publication_transaction_verified": false,
    });
    match &result {
        Ok(checks) => {
            report["ok"] = true.into();
            report["checks"] = checks.clone();
        }
        Err(error) => {
            report["ok"] = false.into();
            report["error"] = error.to_string().into();
        }
    }
    println!("{report}");
    result.map(|_| ())
}
