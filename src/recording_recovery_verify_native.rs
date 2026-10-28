//! 只在显式运行 ignored 测试时生成自有 H.264/AAC 静音短片并验证恢复入口。
//! 不打开窗口、不播放音频，也不读取用户媒体；产物保留在本次独占目录中。
#[cfg(windows)]
#[path = "recording_recovery_process_exit_native.rs"]
mod process_exit_tests;

use super::super::{MediaName, Phase, Session};
use super::{
    copy, decode, verify, AudioPresence, VerificationLimits, VerificationReport, VerificationStage,
    VerificationState,
};
use crate::{
    file_snapshot,
    media_foundation::MfRuntime,
    recording_encoder_policy::EncoderPolicy,
    recording_options::{Preset, QualityProfile},
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const DIRECTORY_ATTEMPTS: u32 = 16;
const REPORT_LIMIT: usize = 256 * 1024;

fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target/recording-recovery-verify");
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..DIRECTORY_ATTEMPTS {
        let directory = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法分配原生恢复验证的独占目录".into())
}

fn write_report(directory: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if bytes.len() > REPORT_LIMIT {
        return Err("原生恢复验证报告超过大小上限".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}

fn copy_registered_clip(directory: &Path, staging: &Path) -> Result<Value> {
    let limits = VerificationLimits::default();
    let stop = Arc::new(AtomicBool::new(false));
    let destination = std::path::absolute(directory.join("recovered.mp4"))?;
    let prepared = match copy::prepare(
        staging,
        MediaName::Video,
        &destination,
        stop.clone(),
        limits,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Ok(json!({"prepared": false, "stage": error.stage,
                "verification": error.verification, "error": error.to_string()}));
        }
    };
    let preparation = serde_json::to_value(prepared.report())?;
    let committed = match prepared.commit(|| true) {
        Ok(committed) => committed,
        Err(error) => {
            return Ok(json!({"prepared": true, "preparation": preparation,
                "committed": false, "stage": error.stage,
                "verification": error.verification, "error": error.to_string()}));
        }
    };
    // 只针对本测试刚生成的短片，先限制大小，再比较完整源和发布副本的字节。
    if committed.copied_bytes > 4 * 1024 * 1024 {
        return Err("原生恢复短片超过测试的 4 MiB 字节比较上限".into());
    }
    let source = staging.join(MediaName::Video.basename());
    let bytes_equal = fs::read(&source)? == fs::read(&committed.path)?;
    let mut readback = VerificationReport::new(directory, MediaName::Video, limits);
    let decoded = decode(&committed.path, &stop, Instant::now(), &mut readback);
    // 发布副本是普通外部路径；仅报告此次完整解码，不伪造恢复清单登记资格。
    Ok(json!({
        "prepared": true, "preparation": preparation, "committed": true,
        "publication": committed, "source_retained": source.try_exists()?,
        "bytes_equal": bytes_equal,
        "output_readback": {
            "decoded": decoded.is_ok(), "error": decoded.err().map(|issue| issue.message),
            "video_frames": readback.video_frames, "video_eof": readback.video_eof,
            "audio_presence": readback.audio_presence, "audio_frames": readback.audio_frames,
            "audio_eof": readback.audio_eof,
        },
    }))
}

#[test]
#[ignore = "原生 H.264/AAC 静音素材的登记、租约与完整回读验证；不打开窗口或播放声音"]
fn recording_recovery_verify_native_registered_clip() -> Result<()> {
    let directory = unique_directory()?;
    let staging = directory.join(".rustshot-recording-native");
    let trial_directory = directory.join("encoder-trial");
    fs::create_dir(&staging)?;
    fs::create_dir(&trial_directory)?;
    let mut session = Session::create(&staging)?;
    let stop = AtomicBool::new(false);
    let generated = (|| -> Result<Value> {
        let _runtime = MfRuntime::new()?;
        crate::recording_encoder_trial::run_profile_case(
            &trial_directory,
            EncoderPolicy::Auto,
            QualityProfile::Current(Preset::CompactMp4),
            &stop,
        )
    })();
    let trial = match generated {
        Ok(trial) => trial,
        Err(error) => {
            write_report(
                &directory,
                &json!({"staging": staging, "source_error": error.to_string()}),
            )?;
            return Err(error);
        }
    };
    if trial["encoded_and_reopened"] != true {
        write_report(&directory, &json!({"staging": staging, "source": trial}))?;
    }
    assert_eq!(
        trial["encoded_and_reopened"],
        true,
        "原生素材未通过试编码与回读，证据目录：{}；{trial}",
        directory.display()
    );
    let attempt = trial["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .ok_or("成功的原生素材没有编码尝试记录")?;
    let index = attempt["index"]
        .as_u64()
        .ok_or("成功的原生素材没有有效尝试编号")?;
    let source = trial_directory.join(format!("Auto-{index}.mp4"));
    let video = staging.join(MediaName::Video.basename());
    assert!(!video.try_exists()?);
    fs::rename(&source, &video)?;
    let snapshot = file_snapshot::inspect(&video)?.ok_or("原生素材移动后不存在")?;
    session.register_file(MediaName::Video, &snapshot)?;
    session.set_phase(Phase::Incomplete)?;

    let limits = VerificationLimits::default();
    let active = verify(&staging, MediaName::Video, &stop, limits);
    drop(session);
    let readable = verify(&staging, MediaName::Video, &stop, limits);
    let copy = copy_registered_clip(&directory, &staging);
    let copy_report = match &copy {
        Ok(report) => report.clone(),
        Err(error) => json!({"error": error.to_string()}),
    };
    stop.store(true, Ordering::Release);
    let cancelled = verify(&staging, MediaName::Video, &stop, limits);
    // 先保留实际报告再断言，失败不会被改写成成功，产物也不会自动删除。
    write_report(
        &directory,
        &json!({
            "staging": staging,
            "source": {"policy": EncoderPolicy::Auto,
                "quality_profile": QualityProfile::Current(Preset::CompactMp4),
                "encoded_and_reopened": trial["encoded_and_reopened"],
                "attempt_index": index, "reopen": attempt["reopen"]},
            "active_lease": active, "verification": readable, "pre_cancelled": cancelled,
            "copy": copy_report,
            "audio_output": false, "synthetic_silence": true,
        }),
    )?;

    assert_ne!(
        active.state,
        VerificationState::VerifiedReadable,
        "{active:?}"
    );
    assert_eq!(active.stage, VerificationStage::Lease, "{active:?}");
    assert!(!active.lease_held);
    assert_eq!(active.video_frames, 0);
    assert_eq!(
        readable.state,
        VerificationState::VerifiedReadable,
        "验证失败，证据目录：{}；{readable:?}",
        directory.display()
    );
    assert_eq!(readable.stage, VerificationStage::Complete);
    assert_eq!(readable.video_frames, 12);
    assert!(readable.video_eof);
    assert_eq!(readable.audio_presence, AudioPresence::Present);
    assert!(readable.audio_frames > 0);
    assert!(readable.audio_eof);
    assert!(readable.registered_identity_matches);
    assert!(readable.metadata_stable);
    assert!(!readable.lease_held);
    assert_eq!(
        cancelled.state,
        VerificationState::Cancelled,
        "{cancelled:?}"
    );
    assert_ne!(cancelled.state, VerificationState::VerifiedReadable);
    assert_eq!(cancelled.stage, VerificationStage::Request);
    assert_eq!(cancelled.video_frames, 0);
    assert!(!cancelled.lease_held);
    let copy = copy?;
    assert_eq!(copy["prepared"], true, "{copy}");
    assert_eq!(copy["preparation"]["lease_held"], true, "{copy}");
    assert_eq!(copy["committed"], true, "{copy}");
    assert_eq!(copy["publication"]["verification"]["lease_held"], false);
    assert_eq!(copy["source_retained"], true, "{copy}");
    assert_eq!(copy["bytes_equal"], true, "{copy}");
    assert_eq!(copy["output_readback"]["decoded"], true, "{copy}");
    assert_eq!(copy["output_readback"]["video_frames"], 12, "{copy}");
    assert_eq!(copy["output_readback"]["video_eof"], true, "{copy}");
    assert_eq!(
        copy["output_readback"]["audio_presence"], "present",
        "{copy}"
    );
    assert!(copy["output_readback"]["audio_frames"].as_u64() > Some(0));
    assert_eq!(copy["output_readback"]["audio_eof"], true, "{copy}");
    Ok(())
}

fn truncated_fixture_backend_rejected(report: &VerificationReport) -> bool {
    report.state == VerificationState::CurrentBackendCannotVerify
        && report.reason == Some("current_backend_cannot_verify")
        && matches!(report.stage, VerificationStage::VideoOpen | VerificationStage::VideoDecode)
        && report.registered_identity_matches
        && report.registration_content_unchanged == Some(true)
        && report.metadata_stable
        && !report.lease_held
}

fn native_snapshot_value(snapshot: &file_snapshot::FileSnapshot) -> Value {
    json!({"volume":snapshot.volume,"id":snapshot.id,"created":snapshot.created,
        "written":snapshot.written,"bytes":snapshot.bytes,"attributes":snapshot.attributes})
}

fn native_exclusive_lease_probe(staging: &Path) -> Value {
    let acquired = (|| -> Result<()> {
        let lease = super::super::open_checked_file(&staging.join(super::super::LEASE_NAME))?;
        lease.try_lock().map_err(super::super::lock_error)?;
        drop(lease);
        Ok(())
    })();
    match acquired {
        Ok(()) => json!({"ok":true,"exclusive_lock_acquired":true,"released_by_drop":true}),
        Err(error) => json!({"ok":false,"exclusive_lock_acquired":false,
            "error":error.to_string().chars().take(2048).collect::<String>()}),
    }
}

fn truncated_fixture_source_check(
    source: &Path,
    before: &file_snapshot::FileSnapshot,
    prefix: &[u8],
) -> Result<Value> {
    let after = file_snapshot::inspect(source)?.ok_or("截断源已不存在")?;
    let mut bytes = Vec::new();
    fs::File::open(source)?.take(9).read_to_end(&mut bytes)?;
    let unchanged = after == *before && bytes.as_slice() == prefix;
    Ok(json!({"ok":unchanged,"retained":true,"snapshot":native_snapshot_value(&after),
        "snapshot_unchanged":after == *before,"prefix_unchanged":bytes.as_slice() == prefix,
        "bytes_read":bytes.len()}))
}

#[test]
#[ignore = "真实 MF 非空头截负例；自有静音素材，不读用户文件、不播音、不开窗口"]
fn recording_recovery_verify_native_rejects_truncated_clip() -> Result<()> {
    const FIXTURE_LIMIT: u64 = 4 * 1024 * 1024;
    const PREFIX_BYTES: usize = 8;
    let directory = unique_directory()?;
    let staging = directory.join(".rustshot-recording-truncated-native");
    let trial_directory = directory.join("encoder-trial");
    let destination = std::path::absolute(directory.join("rejected-copy.mp4"))?;
    let started = Instant::now();
    let mut stage = "create_fixture_directories";
    let mut report = json!({"ok":false,"mode":"nonempty_mp4_header_truncation",
        "staging":staging,"destination":destination,
        "fixture_limit_bytes":FIXTURE_LIMIT,"truncated_prefix_bytes":PREFIX_BYTES,
        "audio_output":false,"synthetic_silence":true,
        "scope":["Fresh owned H264/AAC fixture; the original healthy trial file is retained. A create_new copy is fully written, truncated to its original first eight bytes, flushed and closed before identity registration.",
            "Only this nonempty header truncation is expected to fail current-backend video opening or decoding. This is not a verdict on every tail-truncated MP4, real crash recovery, repair, playback or audio/video synchronization.",
            "Verification and copy preparation must reject after identity checks. Healthy and truncated fixtures and the report are retained; an unexpectedly prepared receipt is dropped under its existing private-staging cleanup contract and is never committed. No user media, windows or audio output are used.",
            "OS exclusive lease probes run after each operation; lease_held=false alone is not release evidence. Existing verification limits are cooperative and cannot interrupt a blocking native call."]});
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<bool> {
        fs::create_dir(&staging)?;
        fs::create_dir(&trial_directory)?;
        stage = "generate_healthy_fixture";
        let stop = Arc::new(AtomicBool::new(false));
        let trial = {
            let _runtime = MfRuntime::new()?;
            crate::recording_encoder_trial::run_profile_case(
                &trial_directory,
                EncoderPolicy::Auto,
                QualityProfile::Current(Preset::CompactMp4),
                &stop,
            )?
        };
        let attempts = trial["attempts"].as_array();
        report["source_trial"] = json!({"encoded_and_reopened":trial["encoded_and_reopened"],
            "attempts":attempts.map(|attempts| attempts.len())});
        if trial["encoded_and_reopened"] != true {
            report["source_trial"]["diagnostic"] = Value::String(
                trial.to_string().chars().take(4096).collect()
            );
            return Err("原生负例的健康素材未通过试编码与回读".into());
        }
        let index = attempts.and_then(|attempts| attempts.last())
            .and_then(|attempt| attempt["index"].as_u64())
            .ok_or("健康素材没有成功尝试编号")?;
        let healthy = trial_directory.join(format!("Auto-{index}.mp4"));
        stage = "read_bounded_healthy_fixture";
        let healthy_before = file_snapshot::inspect(&healthy)?.ok_or("健康素材不存在")?;
        if healthy_before.bytes <= PREFIX_BYTES as u64 || healthy_before.bytes > FIXTURE_LIMIT {
            return Err("健康素材长度必须大于 8 字节且不超过 4 MiB".into());
        }
        let mut healthy_bytes = Vec::new();
        fs::File::open(&healthy)?.take(FIXTURE_LIMIT + 1).read_to_end(&mut healthy_bytes)?;
        if healthy_bytes.len() as u64 != healthy_before.bytes {
            return Err("读取健康素材时长度发生变化或超过上限".into());
        }
        let prefix = &healthy_bytes[..PREFIX_BYTES];
        let source = staging.join(MediaName::Video.basename());
        report["healthy_source"] = json!({"path":healthy,
            "before":native_snapshot_value(&healthy_before)});
        stage = "create_and_truncate_owned_copy";
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&source)?;
            file.write_all(&healthy_bytes)?;
            file.set_len(PREFIX_BYTES as u64)?;
            file.flush()?;
        }
        let before = file_snapshot::inspect(&source)?.ok_or("截断副本不存在")?;
        if before.bytes != PREFIX_BYTES as u64 {
            return Err("截断副本长度不是 8 字节".into());
        }
        report["truncated_source"] = json!({"path":source,
            "before":native_snapshot_value(&before),"prefix":prefix});
        stage = "register_truncated_identity";
        let mut session = Session::create(&staging)?;
        session.register_file(MediaName::Video, &before)?;
        session.set_phase(Phase::Incomplete)?;
        drop(session);
        let limits = VerificationLimits::default();
        report["limits"] = serde_json::to_value(limits)?;
        stage = "verify_truncated_source";
        let verification = verify(&staging, MediaName::Video, &stop, limits);
        let verification_rejected = truncated_fixture_backend_rejected(&verification);
        report["verification"] = serde_json::to_value(&verification)?;
        report["lease_after_verify"] = native_exclusive_lease_probe(&staging);
        report["source_after_verify"] = truncated_fixture_source_check(&source, &before, prefix)?;
        stage = "prepare_rejected_copy";
        let preparation_rejected = match copy::prepare(
            &staging, MediaName::Video, &destination, stop.clone(), limits,
        ) {
            Ok(prepared) => {
                report["copy_prepare"] = json!({"prepared":true,"committed":false,
                    "unexpected_success":true,"verification":prepared.report()});
                drop(prepared);
                false
            }
            Err(error) => {
                let rejected = matches!(error.stage, copy::CopyStage::Verification)
                    && error.copied_bytes == 0
                    && truncated_fixture_backend_rejected(&error.verification);
                report["copy_prepare"] = json!({"prepared":false,"committed":false,
                    "stage":error.stage,"copied_bytes":error.copied_bytes,
                    "verification":&error.verification,"reason":error.reason,
                    "error":error.to_string().chars().take(2048).collect::<String>()});
                rejected
            }
        };
        report["lease_after_prepare"] = native_exclusive_lease_probe(&staging);
        report["source_after_prepare"] = truncated_fixture_source_check(&source, &before, prefix)?;
        stage = "reverify_truncated_source";
        let repeated = verify(&staging, MediaName::Video, &stop, limits);
        let repeated_rejected = truncated_fixture_backend_rejected(&repeated);
        report["reverification"] = serde_json::to_value(&repeated)?;
        report["lease_after_reverify"] = native_exclusive_lease_probe(&staging);
        report["source_after_reverify"] = truncated_fixture_source_check(&source, &before, prefix)?;
        stage = "check_retained_sources_and_absent_destination";
        let healthy_after = file_snapshot::inspect(&healthy)?;
        let healthy_retained = healthy_after.as_ref() == Some(&healthy_before);
        report["healthy_source"]["retained_unchanged"] = healthy_retained.into();
        report["healthy_source"]["after"] = healthy_after.as_ref().map(native_snapshot_value)
            .unwrap_or(Value::Null);
        let destination_absent = !destination.try_exists()?;
        report["destination_absent"] = destination_absent.into();
        report["backend_rejection_gates"] = json!({"verification":verification_rejected,
            "copy_prepare":preparation_rejected,"reverification":repeated_rejected});
        Ok(verification_rejected && preparation_rejected && repeated_rejected
            && healthy_retained && destination_absent
            && ["lease_after_verify", "lease_after_prepare", "lease_after_reverify",
                "source_after_verify", "source_after_prepare", "source_after_reverify"]
                .iter().all(|key| report[*key]["ok"] == true))
    }));
    match outcome {
        Ok(Ok(ok)) => report["ok"] = ok.into(),
        Ok(Err(error)) => report["error"] = json!({"stage":stage,
            "message":error.to_string().chars().take(2048).collect::<String>()}),
        Err(_) => report["error"] = json!({"stage":stage,"message":"panic contained in owned truncated fixture"}),
    }
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    write_report(&directory, &report)?;
    assert_eq!(report["ok"], true, "截断源恢复负例未通过，证据目录：{}；{report}", directory.display());
    Ok(())
}
