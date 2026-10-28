//! 显式执行的系统音频解码探针：只读回 PCM，不打开输出设备或窗口。
use crate::media_foundation::{AudioReader, DecodedAudio, MfRuntime};
use crate::Result;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RATE: u32 = 48_000;
const SEGMENT_FRAMES: usize = RATE as usize / 10;
const TOTAL_FRAMES: usize = SEGMENT_FRAMES * 3;
const DECODE_TIMEOUT: Duration = Duration::from_secs(3);

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

struct Fixtures(PathBuf);

impl Fixtures {
    fn new() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustshot-preview-channels-{}-{nonce}",
            std::process::id()
        ));
        // 独占新目录；不覆盖已有样本，也不扫描临时目录。
        fs::create_dir(&directory)?;
        Ok(Self(directory))
    }

    fn write(&self, name: &str, channels: u16, mask: u32) -> Result<PathBuf> {
        let path = self.0.join(name);
        let block_align = channels * 2;
        let data_bytes = TOTAL_FRAMES as u32 * u32::from(block_align);
        let mut wave = Vec::with_capacity(data_bytes as usize + 68);
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(60 + data_bytes).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&40_u32.to_le_bytes());
        wave.extend_from_slice(&0xfffe_u16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
        wave.extend_from_slice(&channels.to_le_bytes());
        wave.extend_from_slice(&RATE.to_le_bytes());
        wave.extend_from_slice(&(RATE * u32::from(block_align)).to_le_bytes());
        wave.extend_from_slice(&block_align.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(&22_u16.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(&mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM，GUID 按 WAVEFORMATEXTENSIBLE 的小端字段写入。
        wave.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..TOTAL_FRAMES {
            for channel in 0..channels {
                let value: i16 = match channels {
                    6 => match (frame / SEGMENT_FRAMES, channel) {
                        (0, 2) => 8192,  // 仅中置，FL/FR/FC/LFE/BL/BR。
                        (1, 0) => 12288, // 仅左，正值。
                        (2, 1) => -6144, // 仅右，负值。
                        _ => 0,
                    },
                    1 => 8192,
                    2 if channel == 0 => 12288,
                    2 => -6144,
                    _ => return Err("探针只生成 mono、stereo 或 5.1 PCM".into()),
                };
                wave.extend_from_slice(&value.to_le_bytes());
            }
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&wave)?;
        Ok(path)
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        for name in ["surround.wav", "mono.wav", "stereo.wav"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

struct Decoded {
    rate: u32,
    channels: usize,
    frames: usize,
    chunks: Vec<DecodedAudio>,
}

#[derive(Debug)]
struct Window {
    frames: usize,
    mean: [f64; 2],
    rms: [f64; 2],
    difference_rms: f64,
}

impl Decoded {
    fn window(&self, start_ms: u64, end_ms: u64) -> Result<Window> {
        let start = Duration::from_millis(start_ms);
        let end = Duration::from_millis(end_ms);
        let mut count = 0_usize;
        let mut sum = [0.0_f64; 2];
        let mut squares = [0.0_f64; 2];
        let mut difference_square = 0.0;
        for chunk in &self.chunks {
            for (index, frame) in chunk.samples.chunks_exact(self.channels).enumerate() {
                let time = chunk.time + Duration::from_secs_f64(index as f64 / self.rate as f64);
                if !(start..end).contains(&time) {
                    continue;
                }
                let left = f64::from(frame[0]);
                let right = f64::from(frame.get(1).copied().unwrap_or(frame[0]));
                for (channel, sample) in [left, right].into_iter().enumerate() {
                    sum[channel] += sample;
                    squares[channel] += sample * sample;
                }
                difference_square += (left - right).powi(2);
                count += 1;
            }
        }
        ensure(count >= RATE as usize / 100, "分段窗口没有足够的真实解码帧")?;
        Ok(Window {
            frames: count,
            mean: sum.map(|value| value / count as f64),
            rms: squares.map(|value| (value / count as f64).sqrt()),
            difference_rms: (difference_square / count as f64).sqrt(),
        })
    }
}

fn decode_preview(path: &Path, native_channels: usize, preview_channels: usize) -> Result<Decoded> {
    {
        let source = AudioReader::open(path)?.ok_or("PCM 小样没有音轨")?;
        ensure(
            source.channels == native_channels && source.rate == RATE,
            "普通读取没有保留小样的源采样率和声道数",
        )?;
    }
    let mut reader = AudioReader::open_for_preview(path)?.ok_or("预览读取没有音轨")?;
    ensure(
        reader.channels == preview_channels && reader.rate == RATE,
        "预览协商返回的声道数或采样率不符合预期",
    )?;
    let mut decoded = Decoded {
        rate: reader.rate,
        channels: reader.channels,
        frames: 0,
        chunks: Vec::new(),
    };
    let deadline = Instant::now() + DECODE_TIMEOUT;
    for _ in 0..256 {
        ensure(Instant::now() < deadline, "PCM 预览解码超过三秒")?;
        let next = reader.next()?;
        ensure(Instant::now() < deadline, "PCM 预览解码超过三秒")?;
        let Some(chunk) = next else {
            ensure(decoded.frames > 0, "PCM 预览没有输出任何帧")?;
            return Ok(decoded);
        };
        ensure(
            !chunk.samples.is_empty() && chunk.samples.len() % reader.channels == 0,
            "PCM 预览块没有完整音频帧",
        )?;
        ensure(
            chunk.samples.iter().all(|sample| sample.is_finite()),
            "PCM 预览包含非有限值",
        )?;
        decoded.frames += chunk.samples.len() / reader.channels;
        ensure(
            decoded.frames <= RATE as usize * 2,
            "PCM 解码帧数超出小样上限",
        )?;
        decoded.chunks.push(chunk);
    }
    Err("PCM 预览未在有界解码次数内结束".into())
}

#[test]
#[ignore = "显式执行系统 MF 5.1→stereo PCM 下混；不使用声音设备或 GUI"]
fn preview_audio_channel_probe_mf_surround_center_and_front_channels() -> Result<()> {
    let fixtures = Fixtures::new()?;
    let surround = fixtures.write("surround.wav", 6, 0x3f)?;
    let mono = fixtures.write("mono.wav", 1, 0x4)?;
    let stereo = fixtures.write("stereo.wav", 2, 0x3)?;
    let _runtime = MfRuntime::new()?;

    let surround = decode_preview(&surround, 6, 2)?;
    // 取各段中间，避开混音器启动和分段交界处的滤波过渡。
    let center = surround.window(20, 80)?;
    let left = surround.window(120, 180)?;
    let right = surround.window(220, 280)?;
    ensure(
        center.mean.into_iter().all(|sample| sample > 0.005)
            && center.rms.into_iter().all(|sample| sample > 0.005),
        "5.1 中置没有同时贡献给预览左右声道",
    )?;
    ensure(
        center.difference_rms <= center.rms[0].max(center.rms[1]) * 0.05,
        "5.1 中置在左右声道的贡献不对称",
    )?;
    ensure(
        left.mean[0] > 0.005 && left.rms[1] <= left.rms[0] * 0.05 + 0.0001,
        "5.1 左前声道没有保留左右区分",
    )?;
    ensure(
        right.mean[1] < -0.005 && right.rms[0] <= right.rms[1] * 0.05 + 0.0001,
        "5.1 右前声道没有保留左右区分",
    )?;

    let mono = decode_preview(&mono, 1, 1)?;
    let mono_window = mono.window(20, 80)?;
    ensure(mono_window.mean[0] > 0.05, "单声道预览变为静音")?;
    let stereo = decode_preview(&stereo, 2, 2)?;
    let stereo_window = stereo.window(20, 80)?;
    ensure(
        stereo_window.mean[0] > 0.05 && stereo_window.mean[1] < -0.05,
        "双声道预览没有保留不同的左右内容",
    )?;

    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "scope": "mf_pcm_5_1_to_stereo_center_and_front_channels",
            "audio_output_opened": false,
            "gui_observed": false,
            "source_channels": 6,
            "preview_channels": surround.channels,
            "decoded_frames": surround.frames,
            "center_window_frames": center.frames,
            "center_rms": center.rms,
            "mono_channels": mono.channels,
            "stereo_channels": stereo.channels,
        })
    );
    Ok(())
}
