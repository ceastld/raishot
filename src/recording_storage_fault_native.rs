//! 真实 MF 字节流的有界空间不足注入；仅显式运行，不读取桌面或启动设备采集。
//! RGB32 描述输入像素，不描述实际编码器的硬软类别。
#![cfg(test)]

use super::{classify_recording_error, MfRuntime, Mp4Writer, VideoReader};
use crate::{
    geometry::ScreenRect, recording_color_check::ColorChart, recording_fault_stream,
    recording_size_layout::RecordingSizeLayout, Result,
};
use serde_json::{json, Value};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const FPS: u32 = 30;
const FRAMES: u32 = 6;
const BITRATE: u32 = 4_000_000;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;
const RUN_BUDGET: Duration = Duration::from_secs(90);
const CASE_BUDGET: Duration = Duration::from_secs(45);
const PTS_TOLERANCE: Duration = Duration::from_millis(1);
const COLOR_TOLERANCE: f64 = 12.;

#[test]
#[ignore = "真实 MF caller IStream 空间不足注入；不读桌面、不开摄像头、不播音、不开窗口"]
fn recording_storage_fault_native_byte_stream_reports_real_failure() -> Result<()> {
    let directory = unique_directory()?;
    let started = Instant::now();
    let mut cases = Vec::with_capacity(2);
    let setup = (|| -> Result<()> {
        let _runtime = MfRuntime::new()?;
        let chart = ColorChart::new(RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: WIDTH,
            height: HEIGHT,
        })?)?;
        for inject in [false, true] {
            cases.push(run_case(&directory, inject, &chart, started));
        }
        Ok(())
    })();
    let ok = setup.is_ok()
        && cases.len() == 2
        && cases.iter().all(|case| case["ok"].as_bool() == Some(true));
    let report = json!({
        "ok": ok,
        "setup_error": setup.err().map(|error| error_value(error.as_ref())),
        "cases": cases,
        "elapsed_ms": started.elapsed().as_millis(),
        "limits": {"cases":2,"frames_per_case":FRAMES,"size":[WIDTH,HEIGHT],
            "adapter_extent_bytes":MAX_FILE_BYTES,"report_bytes":MAX_REPORT_BYTES,
            "run_seconds":RUN_BUDGET.as_secs(),"case_seconds":CASE_BUDGET.as_secs()},
        "scope": [
            "Real caller IStream -> IMFByteStream -> H264 Sink Writer; RGB32 is the requested input, not evidence of a software encoder. The actual selected MFT is in each writer inspection.",
            "One returned writer per case, without backend fallback. A successful WriteSample is acceptance by that API, not proof that container bytes were already written.",
            "The atomic injection limit is current file length plus one byte. This simulates STG_E_MEDIUMFULL from this adapter, not physical disk exhaustion or the complete recording-worker recovery path.",
            "Native Finalize call count includes failed calls on a successfully returned Mp4Writer. It does not mean successful finalization and does not count failed-construction teardown.",
            "Production error classification uses the actual returned error source chain. An injected adapter HRESULT does not relabel a generic MF error as Output.",
            "Only the uninjected baseline is decoded completely. An injected artifact is retained without any claim of readability, recoverability or successful publication.",
            "Time budgets are cooperative checks between calls; blocking native calls cannot be interrupted. No desktop capture, camera, sound output or windows are used.",
            "All newly created artifacts are retained, including failure files."
        ]
    });
    let report = write_report(&directory, report)?;
    eprintln!(
        "MF storage fault report: {}",
        directory.join("report.json").display()
    );
    assert_eq!(
        report["ok"].as_bool(),
        Some(true),
        "MF 存储故障注入未通过，证据：{}",
        directory.join("report.json").display()
    );
    Ok(())
}

fn run_case(parent: &Path, inject: bool, chart: &ColorChart, run_started: Instant) -> Value {
    let case_started = Instant::now();
    let name = if inject { "injected" } else { "baseline" };
    let directory = parent.join(name);
    let path = directory.join("clip.mp4");
    let calls = Arc::new(AtomicU32::new(0));
    let mut stream = None;
    let mut control = None;
    let mut writer = None;
    let mut writer_created = false;
    let mut uses_gpu_input = None;
    let mut accepted_writes = 0_u32;
    let mut accepted_before_arm = None;
    let mut arm_limit = None;
    let mut write_error = Value::Null;
    let mut finalized_writer = Value::Null;
    let mut finalize_error = Value::Null;
    let mut explicit_finalize_calls = 0_u32;
    let mut errors = Vec::new();
    let mut stage = "create_directory";
    let work = (|| -> Result<()> {
        fs::create_dir(&directory)?;
        checkpoint(run_started, case_started)?;
        stage = "create_byte_stream";
        eprintln!("MF storage fault [{name}] create byte stream: begin");
        let (created_stream, created_control) = recording_fault_stream::create(&path)?;
        eprintln!("MF storage fault [{name}] create byte stream: returned");
        stream = Some(created_stream);
        control = Some(created_control);
        checkpoint(run_started, case_started)?;
        stage = "create_writer";
        eprintln!("MF storage fault [{name}] create writer: begin");
        let created = Mp4Writer::new_with_test_byte_stream(
            &path,
            WIDTH,
            HEIGHT,
            FPS,
            BITRATE,
            stream.as_ref().ok_or("缺少字节流")?,
            calls.clone(),
        )?;
        eprintln!("MF storage fault [{name}] create writer: returned");
        uses_gpu_input = Some(created.uses_gpu_input());
        writer_created = true;
        // 先取得所有权，之后即使预算/路线检查失败也显式 Finalize 一次。
        writer = Some(created);
        checkpoint(run_started, case_started)?;
        if uses_gpu_input != Some(false) {
            return Err("字节流夹具意外使用了 GPU 输入，拒绝改变 RGB32 路线".into());
        }
        for index in 0..FRAMES {
            stage = "write_video";
            checkpoint(run_started, case_started)?;
            eprintln!("MF storage fault [{name}] write sample {index}: begin");
            let result = writer.as_mut().ok_or("缺少 writer")?.write_video(
                chart.bgra(),
                WIDTH,
                frame_time(index),
                frame_time(index + 1) - frame_time(index),
            );
            eprintln!("MF storage fault [{name}] write sample {index}: returned");
            match result {
                Ok(()) => accepted_writes += 1,
                Err(error) => write_error = error_value(error.as_ref()),
            }
            checkpoint(run_started, case_started)?;
            if !write_error.is_null() {
                break;
            }
            if inject && index == 0 {
                stage = "arm_after_first_accepted_write";
                accepted_before_arm = Some(accepted_writes);
                eprintln!("MF storage fault [{name}] arm limit: begin");
                // metadata 与设限共用适配器的写锁，不使用旧 snapshot 的大小。
                arm_limit = Some(
                    control
                        .as_ref()
                        .ok_or("缺少故障控制器")?
                        .arm_after_current_len(1)?,
                );
                eprintln!("MF storage fault [{name}] arm limit: returned");
                checkpoint(run_started, case_started)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = work {
        errors.push(json!({"stage":stage,"error":error_value(error.as_ref())}));
    }

    // 预算耗尽也不能跳过已返回 writer 的显式收尾；记录调用数与成功状态两种证据。
    if let Some(writer) = writer.take() {
        explicit_finalize_calls += 1;
        eprintln!("MF storage fault [{name}] Finalize: begin");
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        eprintln!("MF storage fault [{name}] Finalize: returned");
        finalized_writer = diagnostics;
        if let Err(error) = finished {
            finalize_error = error_value(error.as_ref());
        }
        if let Err(error) = checkpoint(run_started, case_started) {
            errors.push(json!({"stage":"after_finalize","error":error_value(error.as_ref())}));
        }
    }
    // finish 已消费 writer 并完成其 Drop；先放掉调用方 stream，再取最终同文件统计。
    let native_finalize_calls_after_writer_drop = calls.load(Ordering::Relaxed);
    eprintln!("MF storage fault [{name}] release caller byte stream: begin");
    drop(stream.take());
    eprintln!("MF storage fault [{name}] release caller byte stream: returned");
    eprintln!("MF storage fault [{name}] snapshot: begin");
    let snapshot = match control.as_ref().map(|control| control.snapshot()) {
        Some(Ok(snapshot)) => Some(snapshot),
        Some(Err(error)) => {
            errors.push(
                json!({"stage":"snapshot_after_writer_drop","error":error_value(error.as_ref())}),
            );
            None
        }
        None => None,
    };
    eprintln!("MF storage fault [{name}] snapshot: returned");
    // 此处释放最后的写文件 guard，随后 Reader 才能按自己的共享模式打开基线。
    eprintln!("MF storage fault [{name}] release caller control: begin");
    drop(control.take());
    eprintln!("MF storage fault [{name}] release caller control: returned");
    eprintln!("MF storage fault [{name}] artifact metadata: begin");
    let file_bytes = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata.len()),
        Err(error) => {
            errors.push(json!({"stage":"artifact_metadata","error":error_value(&error)}));
            None
        }
    };
    eprintln!("MF storage fault [{name}] artifact metadata: returned");
    let decode = if !inject
        && writer_created
        && errors.is_empty()
        && write_error.is_null()
        && finalize_error.is_null()
    {
        decode_baseline(&path, chart, run_started, case_started)
    } else {
        json!({"attempted":false,"scope":"Only a successfully finalized, uninjected baseline is decoded."})
    };
    if let Err(error) = checkpoint(run_started, case_started) {
        errors.push(json!({"stage":"after_case_observation","error":error_value(error.as_ref())}));
    }
    let count_ok = writer_created
        && explicit_finalize_calls == 1
        && native_finalize_calls_after_writer_drop == 1;
    let extent_ok = file_bytes
        .is_some_and(|bytes| bytes <= MAX_FILE_BYTES && (inject || bytes > 0))
        && snapshot.as_ref().is_some_and(|stats| {
            Some(stats.actual_len) == file_bytes
                && stats.actual_len <= MAX_FILE_BYTES
                && stats.high_water <= MAX_FILE_BYTES
                && stats.safety_cap == MAX_FILE_BYTES
        });
    let evidence_ok = if inject {
        accepted_writes >= 1
            && accepted_before_arm == Some(1)
            && arm_limit.is_some()
            && snapshot.as_ref().is_some_and(|stats| {
                stats.armed
                    && stats.arm_calls == 1
                    && stats.injected_count > 0
                    && Some(stats.limit) == arm_limit
            })
            && (!write_error.is_null() || !finalize_error.is_null())
    } else {
        accepted_writes == FRAMES
            && write_error.is_null()
            && finalize_error.is_null()
            && snapshot.as_ref().is_some_and(|stats| {
                !stats.armed
                    && stats.arm_calls == 0
                    && stats.injected_count == 0
                    && stats.cap_hits == 0
            })
            && decode["ok"].as_bool() == Some(true)
    };
    json!({
        "name":name,"ok":errors.is_empty() && count_ok && extent_ok && evidence_ok,
        "writer_created":writer_created,"requested_input":"cpu_rgb32","uses_gpu_input":uses_gpu_input,
        "backend_fallback_attempted":false,"accepted_by_write_sample":accepted_writes,
        "accepted_by_write_sample_before_arm":accepted_before_arm,
        "arm_limit_bytes":arm_limit,"file_bytes_after_all_writer_handles_released":file_bytes,
        "snapshot_after_writer_drop":snapshot,
        "explicit_finalize_calls":explicit_finalize_calls,
        "native_finalize_calls_after_writer_drop":native_finalize_calls_after_writer_drop,
        "finalize_succeeded":writer_created && explicit_finalize_calls == 1 && finalize_error.is_null(),
        "write_error":write_error,"finalize_error":finalize_error,
        "finalized_writer":finalized_writer,"decode":decode,"errors":errors,
        "gates":{"one_native_finalize":count_ok,"bounded_extent":extent_ok,"required_evidence":evidence_ok},
        "elapsed_ms":case_started.elapsed().as_millis(),
        "artifact_retained":file_bytes.is_some()
    })
}

fn decode_baseline(path: &Path, chart: &ColorChart, run: Instant, case: Instant) -> Value {
    let mut frames = Vec::with_capacity(FRAMES as usize);
    let mut eof = false;
    let mut source = Value::Null;
    let mut diagnostics = Value::Null;
    let result = (|| -> Result<()> {
        checkpoint(run, case)?;
        eprintln!("MF storage fault [baseline] open reader: begin");
        let mut reader = VideoReader::open(path)?;
        eprintln!("MF storage fault [baseline] open reader: returned");
        checkpoint(run, case)?;
        source = json!({"size":[reader.width,reader.height],
            "codec":format!("{:?}",reader.info.codec),"duration_ns":reader.duration.as_nanos(),
            "stream_probe_error":reader.info.stream_probe_error,
            "audio_present":reader.info.streams.iter().any(|stream|stream.major_type==MFMediaType_Audio)});
        let work = (|| -> Result<()> {
            if (reader.width, reader.height) != (WIDTH, HEIGHT)
                || reader.info.codec != MFVideoFormat_H264
                || reader.info.stream_probe_error.is_some()
                || reader
                    .info
                    .streams
                    .iter()
                    .any(|stream| stream.major_type == MFMediaType_Audio)
            {
                return Err("基线回读的尺寸/H264/无音轨合同不符".into());
            }
            let stop = AtomicBool::new(false);
            let mut previous = None;
            loop {
                checkpoint(run, case)?;
                eprintln!(
                    "MF storage fault [baseline] read frame {}: begin",
                    frames.len()
                );
                let frame = reader.next_cancellable(&stop)?;
                eprintln!(
                    "MF storage fault [baseline] read frame {}: returned",
                    frames.len()
                );
                checkpoint(run, case)?;
                let Some(frame) = frame else {
                    eof = true;
                    break;
                };
                if frames.len() >= FRAMES as usize {
                    return Err("基线解码超过六帧上限".into());
                }
                let index = frames.len() as u32;
                let pts_error = frame.time.abs_diff(frame_time(index));
                let monotonic = previous.is_none_or(|value| frame.time > value);
                previous = Some(frame.time);
                let comparison =
                    chart.compare(&frame.bgra, reader.width, reader.height, COLOR_TOLERANCE)?;
                frames.push(json!({"index":index,"pts_ns":frame.time.as_nanos(),
                    "expected_pts_ns":frame_time(index).as_nanos(),"pts_error_ns":pts_error.as_nanos(),
                    "strictly_increasing":monotonic,"color":comparison,
                    "passed":monotonic && pts_error<=PTS_TOLERANCE && comparison.passed}));
            }
            if frames.len() != FRAMES as usize
                || !frames
                    .iter()
                    .all(|frame| frame["passed"].as_bool() == Some(true))
                || reader.duration.abs_diff(frame_time(FRAMES)) > PTS_TOLERANCE
            {
                return Err("基线完整 EOF 的帧数/PTS/颜色/容器跨度不符".into());
            }
            Ok(())
        })();
        diagnostics = reader.diagnostics();
        work
    })();
    json!({"attempted":true,"ok":result.is_ok() && eof,"complete_eof":eof,
        "frames":frames,"source":source,"reader_diagnostics":diagnostics,
        "pts_tolerance_ns":PTS_TOLERANCE.as_nanos(),"color_tolerance":COLOR_TOLERANCE,
        "error":result.err().map(|error|error_value(error.as_ref()))})
}

fn frame_time(index: u32) -> Duration {
    // 固定小夹具，按最接近的 100ns 网格生成连续六个区间。
    let ticks = (u64::from(index) * 10_000_000 + u64::from(FPS / 2)) / u64::from(FPS);
    Duration::from_nanos(ticks * 100)
}

fn checkpoint(run: Instant, case: Instant) -> Result<()> {
    if run.elapsed() > RUN_BUDGET || case.elapsed() > CASE_BUDGET {
        return Err("MF 存储故障检查超出调用间时间预算".into());
    }
    Ok(())
}

fn error_value(error: &(dyn Error + 'static)) -> Value {
    let mut chain = Vec::with_capacity(8);
    let mut current = Some(error);
    while let Some(error) = current {
        if chain.len() == 8 {
            break;
        }
        chain.push(json!({"message":clip(&error.to_string(),256),
            "hresult":error.downcast_ref::<windows::core::Error>().map(|error|format!("0x{:08X}",error.code().0 as u32)),
            "io_raw_os_error":error.downcast_ref::<std::io::Error>().and_then(|error|error.raw_os_error())}));
        current = error.source();
    }
    let mut classification = serde_json::to_value(classify_recording_error(error))
        .unwrap_or_else(|error| json!({"serialization_error":clip(&error.to_string(),256)}));
    if let Some(message) = classification.get_mut("message") {
        if let Some(text) = message.as_str() {
            *message = Value::String(clip(text, 256));
        }
    }
    json!({"message":clip(&error.to_string(),256),"production_classification":classification,
        "source_chain":chain,"source_chain_limit":8})
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target/recording-storage-fault");
    fs::create_dir_all(parent)?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!("native-{}-{time}-{index}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法创建独占 MF 存储故障检查目录".into())
}

fn write_report(directory: &Path, report: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    let report = if bytes.len() > MAX_REPORT_BYTES {
        // 失败回执仍先落盘；不把被裁掉的证据当作通过。
        let limited = json!({"ok":false,"error":"report_exceeded_256kib",
            "scope":"Detailed evidence exceeded the report budget; artifacts are retained and this run is not verified."});
        bytes = serde_json::to_vec_pretty(&limited)?;
        limited
    } else {
        report
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(report)
}
