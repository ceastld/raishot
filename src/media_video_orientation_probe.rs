//! 显式执行的 MP4 旋转回读探针；不打开窗口、声音设备或 GPU 实验路径。
use super::{sample, video_type, DecodedFrame, MfRuntime, VideoReader};
use crate::Result;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::Media::MediaFoundation::*,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FPS: u32 = 10;
const FRAME_COUNT: usize = 3;
const DEGREES: [u32; 4] = [0, 90, 180, 270];
const COLOR_TOLERANCE: f64 = 48.0;
type Colors = [[f64; 3]; 4];

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
            "rustshot-video-orientation-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory)?;
        Ok(Self(directory))
    }

    fn path(&self, degrees: u32) -> PathBuf {
        self.0.join(format!("rotation-{degrees}.mp4"))
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        for degrees in DEGREES {
            let _ = fs::remove_file(self.path(degrees));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn asymmetric_bgra() -> Vec<u8> {
    // A/B/C/D 分别占 TL/TR/BL/BR，颜色互异；非方形尺寸也能暴露漏换宽高。
    let colors = [
        [25, 45, 220, 255],
        [35, 210, 40, 255],
        [220, 55, 35, 255],
        [40, 210, 220, 255],
    ];
    let mut pixels = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let quadrant = usize::from(y >= HEIGHT / 2) * 2 + usize::from(x >= WIDTH / 2);
            pixels.extend_from_slice(&colors[quadrant]);
        }
    }
    pixels
}

fn write_fixture(path: &Path, degrees: u32, pixels: &[u8]) -> Result<()> {
    fn stage<T, E: std::fmt::Display>(name: &str, result: std::result::Result<T, E>) -> Result<T> {
        result.map_err(|error| format!("{name}：{error}").into())
    }
    unsafe {
        let mut attributes = None;
        stage("MFCreateAttributes", MFCreateAttributes(&mut attributes, 3))?;
        let attributes = attributes.ok_or("无法创建样本编码属性")?;
        stage(
            "禁用硬件编码",
            attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0),
        )?;
        stage(
            "禁用编码节流",
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1),
        )?;
        stage(
            "指定MP4容器",
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4),
        )?;
        let url = HSTRING::from(path.as_os_str());
        // 用 FAIL_IF_EXIST 原子创建本探针的新文件；不覆盖已有路径。
        let output_stream = stage(
            "MFCreateFile",
            MFCreateFile(
                MF_ACCESSMODE_WRITE,
                MF_OPENMODE_FAIL_IF_EXIST,
                MF_FILEFLAGS_NONE,
                PCWSTR(url.as_ptr()),
            ),
        )?;
        let writer = stage(
            "MFCreateSinkWriterFromURL",
            MFCreateSinkWriterFromURL(PCWSTR::null(), &output_stream, &attributes),
        )?;
        let output = stage(
            "创建H264输出类型",
            video_type(&MFVideoFormat_H264, WIDTH, HEIGHT, FPS),
        )?;
        stage(
            "设置输出码率",
            output.SetUINT32(&MF_MT_AVG_BITRATE, 2_000_000),
        )?;
        stage(
            "设置输出旋转",
            output.SetUINT32(&MF_MT_VIDEO_ROTATION, degrees),
        )?;
        let stream = stage("AddStream(H264)", writer.AddStream(&output))?;
        let input = stage(
            "创建RGB32输入类型",
            video_type(&MFVideoFormat_RGB32, WIDTH, HEIGHT, FPS),
        )?;
        stage(
            "设置RGB32步长",
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, WIDTH * 4),
        )?;
        stage(
            "设置RGB32独立帧",
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1),
        )?;
        // 输入像素本身没有旋转；只有压缩输出类型声明容器显示方向。
        stage(
            "SetInputMediaType(RGB32)",
            writer.SetInputMediaType(stream, &input, None),
        )?;
        stage("BeginWriting", writer.BeginWriting())?;
        let frame_duration = Duration::from_millis(100);
        for frame in 0..FRAME_COUNT {
            let time = Duration::from_millis(frame as u64 * 100);
            let sample = stage(
                &format!("构造样本({frame})"),
                sample(pixels, time, frame_duration),
            )?;
            stage(
                &format!("WriteSample({frame})"),
                writer.WriteSample(stream, &sample),
            )?;
        }
        stage("Finalize", writer.Finalize())?;
        drop(writer);
        // 释放 writer 后接收器已结束流的生命周期；再释放本地引用，随后由真实回读验成品。
        drop(output_stream);
    }
    let size = fs::metadata(path)?.len();
    ensure(size > 0 && size <= 2 * 1024 * 1024, "旋转小样大小异常")
}

fn verify_fixture_metadata(path: &Path, expected: u32) -> Result<()> {
    let url = HSTRING::from(path.as_os_str());
    unsafe {
        // 只读原生压缩类型，不经过 VideoReader 的处理器或方向归一化。
        let source = MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), None)?;
        let native = source.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?;
        ensure(
            native.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_H264,
            "fixture 原生编码不是 H.264",
        )?;
        let packed_size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
        ensure(
            ((packed_size >> 32) as u32, packed_size as u32) == (WIDTH, HEIGHT),
            "fixture 原始压缩尺寸发生变化",
        )?;
        let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0);
        if rotation != expected {
            return Err(format!(
                "fixture 未保留旋转元数据：请求 {expected}°，原生类型报告 {rotation}°"
            )
            .into());
        }
    }
    Ok(())
}

fn dimensions(degrees: u32) -> (u32, u32) {
    if matches!(degrees, 90 | 270) {
        (HEIGHT, WIDTH)
    } else {
        (WIDTH, HEIGHT)
    }
}

fn quadrant_colors(frame: &DecodedFrame, width: u32, height: u32) -> Result<Colors> {
    ensure(
        frame.bgra.len() == width as usize * height as usize * 4,
        "归一化帧的像素长度与公开尺寸不符",
    )?;
    let centers = [
        (width / 4, height / 4),
        (width * 3 / 4, height / 4),
        (width / 4, height * 3 / 4),
        (width * 3 / 4, height * 3 / 4),
    ];
    let mut colors = [[0.0; 3]; 4];
    for (quadrant, (cx, cy)) in centers.into_iter().enumerate() {
        // 9×9 中心区域避开块边界和色度采样边缘，忽略 RGB32 的 alpha。
        for y in cy - 4..=cy + 4 {
            for x in cx - 4..=cx + 4 {
                let pixel = (y as usize * width as usize + x as usize) * 4;
                for channel in 0..3 {
                    colors[quadrant][channel] += f64::from(frame.bgra[pixel + channel]) / 81.0;
                }
            }
        }
    }
    Ok(colors)
}

fn verify_colors(actual: Colors, baseline: Colors, degrees: u32) -> Result<()> {
    let mapping = match degrees {
        0 => [0, 1, 2, 3],
        90 => [2, 0, 3, 1],
        180 => [3, 2, 1, 0],
        270 => [1, 3, 0, 2],
        _ => return Err("探针只验证四分之一转角度".into()),
    };
    for (quadrant, expected) in mapping.into_iter().enumerate() {
        for channel in 0..3 {
            if (actual[quadrant][channel] - baseline[expected][channel]).abs() > COLOR_TOLERANCE {
                return Err(format!(
                    "{degrees}° 第 {quadrant} 块颜色不符：实际 {:?}，期望 {:?}",
                    actual[quadrant], baseline[expected]
                )
                .into());
            }
        }
    }
    Ok(())
}

fn verify_reader(
    mut reader: VideoReader,
    degrees: u32,
    baseline: Option<Colors>,
) -> Result<Colors> {
    let expected_size = dimensions(degrees);
    ensure(
        (reader.width, reader.height) == expected_size,
        "VideoReader 公开尺寸没有按旋转归一化",
    )?;
    ensure(
        reader.info.rotation_degrees == degrees,
        "VideoInfo 没有保留原始旋转元数据",
    )?;
    let mut first_colors = None;
    for index in 0..FRAME_COUNT {
        let frame = reader.next()?.ok_or("旋转小样未输出完整三帧")?;
        ensure(
            (reader.width, reader.height) == expected_size,
            "解码期间公开尺寸发生变化",
        )?;
        ensure(
            frame.time == Duration::from_millis(index as u64 * 100),
            "旋转归一化改变了帧 PTS",
        )?;
        let colors = quadrant_colors(&frame, reader.width, reader.height)?;
        if let Some(baseline) = baseline {
            verify_colors(colors, baseline, degrees)?;
        }
        first_colors.get_or_insert(colors);
    }
    ensure(reader.next()?.is_none(), "旋转小样输出了多于三帧的数据")?;
    let first_colors = first_colors.ok_or("旋转小样没有颜色基准")?;
    reader.seek(Duration::from_millis(100))?;
    let after_seek = reader
        .next_at(Duration::from_millis(100))?
        .ok_or("seek 后没有旋转视频帧")?;
    ensure(
        after_seek.time == Duration::from_millis(100)
            && (reader.width, reader.height) == expected_size
            && reader.info.rotation_degrees == degrees,
        "seek 后的 PTS、尺寸或原始旋转元数据发生变化",
    )?;
    // 0° 回读本身也须在 seek 后维持同一内容，其它角度继续对照原始基准。
    verify_colors(
        quadrant_colors(&after_seek, reader.width, reader.height)?,
        baseline.unwrap_or(first_colors),
        degrees,
    )?;
    Ok(first_colors)
}

#[test]
#[ignore = "显式执行 CPU H.264 MP4 旋转小样回读；不打开 GUI 或音频输出"]
fn native_video_orientation_quarter_turns_round_trip() -> Result<()> {
    let fixtures = Fixtures::new()?;
    let _runtime = MfRuntime::new()?;
    let pixels = asymmetric_bgra();
    for degrees in DEGREES {
        let path = fixtures.path(degrees);
        write_fixture(&path, degrees, &pixels)
            .map_err(|error| format!("fixture 编码阶段（{degrees}°）失败：{error}"))?;
        verify_fixture_metadata(&path, degrees)
            .map_err(|error| format!("fixture 元数据阶段（{degrees}°）失败：{error}"))?;
    }

    let baseline_reader = VideoReader::open(&fixtures.path(0))
        .map_err(|error| format!("默认方向打开（0°）失败：{error}"))?;
    let baseline = verify_reader(baseline_reader, 0, None)
        .map_err(|error| format!("默认方向回读（0°）失败：{error}"))?;
    for left in 0..4 {
        for right in left + 1..4 {
            ensure(
                (0..3).any(|channel| {
                    (baseline[left][channel] - baseline[right][channel]).abs() > 80.0
                }),
                "0° 回读基准色块无法可靠区分",
            )?;
        }
    }

    let mut observations = Vec::new();
    for degrees in DEGREES {
        if degrees != 0 {
            let reader = VideoReader::open(&fixtures.path(degrees))
                .map_err(|error| format!("默认方向打开（{degrees}°）失败：{error}"))?;
            verify_reader(reader, degrees, Some(baseline))
                .map_err(|error| format!("默认方向回读（{degrees}°）失败：{error}"))?;
        }
        let legacy = match VideoReader::open_legacy_for_benchmark(&fixtures.path(degrees)) {
            Ok(reader) => {
                verify_reader(reader, degrees, Some(baseline))
                    .map_err(|error| format!("legacy 方向回读（{degrees}°）失败：{error}"))?;
                serde_json::json!({"status": "normalized"})
            }
            Err(error) if degrees != 0 => {
                let message = error.to_string();
                ensure(
                    message.contains("旋转") || message.to_ascii_lowercase().contains("rotation"),
                    "legacy 失败没有明确说明旋转处理不支持",
                )?;
                serde_json::json!({"status": "unsupported", "error": message})
            }
            Err(error) => return Err(format!("legacy 0° 基线回读失败：{error}").into()),
        };
        observations.push(serde_json::json!({
            "degrees": degrees,
            "display_size": dimensions(degrees),
            "default": "normalized",
            "legacy": legacy,
        }));
    }
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "scope": "native_mp4_quarter_turn_pixel_size_pts_and_seek",
            "gpu_lab_used": false,
            "gui_observed": false,
            "audio_output_opened": false,
            "orientations": observations,
        })
    );
    Ok(())
}
