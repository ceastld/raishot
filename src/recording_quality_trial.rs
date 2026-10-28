//! 候选画质参数的短片协商与回读；合成色块不用于比较文字、滚动或运动画质。
use crate::{
    media_foundation::MfRuntime,
    recording_encoder_policy::EncoderPolicy,
    recording_encoder_trial::run_profile_case,
    recording_options::{Preset, QualityProfile},
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const DIRECTORY_ATTEMPTS: usize = 16;

fn output_directory(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..DIRECTORY_ATTEMPTS {
        let directory = parent.join(format!(
            "quality-trial-{}-{stamp}-{index}",
            std::process::id()
        ));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(tr!(
        "无法分配画质参数试验目录",
        "Cannot allocate a quality-parameter trial directory"
    )
    .into())
}

fn failed_case(profile: QualityProfile, stage: &str, error: String) -> Value {
    json!({
        "policy": EncoderPolicy::Auto,
        "quality_profile": profile,
        "encoded_and_reopened": false,
        "stage": stage,
        "error": error,
        "attempts": [],
    })
}

/// 每个预设持有独占目录；成功仅表示这组短片参数能写入并再次读取。
/// baseline_ok 不要求四个实验候选可用，逐项能力以 cases 为准。
pub(crate) fn run(directory: &Path, stop: &AtomicBool) -> Result<Value> {
    let output = output_directory(directory)?;
    let mut cancelled = stop.load(Ordering::Acquire);
    // run_profile_case 复用外层 MF 生命周期；取消前不初始化媒体设备。
    let runtime = (!cancelled).then(MfRuntime::new);
    let expected_profiles = QualityProfile::EXPERIMENTS.len() + 1;
    let mut cases = Vec::with_capacity(expected_profiles);
    for profile in std::iter::once(QualityProfile::Current(Preset::CompactMp4))
        .chain(QualityProfile::EXPERIMENTS)
    {
        if cancelled || stop.load(Ordering::Acquire) {
            cancelled = true;
            break;
        }
        let case_directory = output.join(profile.id());
        let started = Instant::now();
        let mut case = match &runtime {
            Some(Ok(_runtime)) => match fs::create_dir(&case_directory) {
                Ok(()) => run_profile_case(&case_directory, EncoderPolicy::Auto, profile, stop)
                    .unwrap_or_else(|error| failed_case(profile, "trial_setup", error.to_string())),
                Err(error) => failed_case(profile, "create_case_directory", error.to_string()),
            },
            Some(Err(error)) => failed_case(profile, "runtime_initialization", error.to_string()),
            None => {
                cancelled = true;
                break;
            }
        };
        case["profile_id"] = profile.id().into();
        case["profile_label"] = profile.label().into();
        case["trial_elapsed_ms"] = (started.elapsed().as_secs_f64() * 1000.).into();
        cancelled |= stop.load(Ordering::Acquire) || case["cancelled"] == true;
        cases.push(case);
    }
    let encoded_profiles = cases
        .iter()
        .filter(|case| case["encoded_and_reopened"] == true)
        .count();
    let report = json!({
        "baseline_ok": cases.first().is_some_and(|case| case["encoded_and_reopened"] == true),
        "all_profiles_encoded_and_reopened": !cancelled && cases.len() == expected_profiles && encoded_profiles == expected_profiles,
        "expected_profiles": expected_profiles,
        "attempted_profiles": cases.len(),
        "encoded_profiles": encoded_profiles,
        "cancelled": cancelled,
        "synthetic_only": true,
        "audio_output": false,
        "sample_scope": "short synthetic color-chart H.264/AAC trial; negotiation, write and reopen only",
        "quality_comparison_performed": false,
        "output": output,
        "cases": cases,
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("report.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonempty_error(value: &Value) -> bool {
        value.as_str().is_some_and(|error| !error.trim().is_empty())
    }

    fn has_failure_details(case: &Value) -> bool {
        nonempty_error(&case["error"])
            || case["attempts"].as_array().is_some_and(|attempts| {
                attempts.iter().any(|attempt| {
                    nonempty_error(&attempt["finalize_error"])
                        || nonempty_error(&attempt["reopen_error"])
                        || attempt["creation_error"]["cause"]
                            .as_object()
                            .is_some_and(|cause| !cause.is_empty())
                        || attempt["cause"]
                            .as_object()
                            .is_some_and(|cause| !cause.is_empty())
                })
            })
    }

    #[test]
    #[ignore = "真实候选 H.264/AAC 短片试编码与回读；不创建窗口或播放声音"]
    fn recording_quality_profiles_native_roundtrips() {
        let stop = AtomicBool::new(false);
        let report = run(Path::new("target/recording-quality-trial"), &stop).unwrap();
        assert_eq!(report["baseline_ok"], true, "{report}");
        assert_eq!(report["cancelled"], false, "{report}");
        assert_eq!(report["quality_comparison_performed"], false);
        let cases = report["cases"].as_array().unwrap();
        assert_eq!(cases.len(), QualityProfile::EXPERIMENTS.len() + 1);
        for (case, profile) in cases.iter().zip(
            std::iter::once(QualityProfile::Current(Preset::CompactMp4))
                .chain(QualityProfile::EXPERIMENTS),
        ) {
            assert_eq!(case["profile_id"], profile.id(), "{case}");
            if case["encoded_and_reopened"] != true {
                assert!(has_failure_details(case), "{case}");
                continue;
            }
            let attempt = case["attempts"]
                .as_array()
                .and_then(|attempts| attempts.last())
                .unwrap();
            let reopened = &attempt["reopen"];
            let payload = &reopened["video_payload"];
            assert_eq!(payload["complete"], true, "{case}");
            let samples = payload["samples"].as_u64().unwrap();
            assert!(samples > 0, "{case}");
            assert_eq!(Some(samples), reopened["video_frames"].as_u64(), "{case}");
            assert!(
                payload["payload_bytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 0),
                "{case}"
            );
        }
        // 实验候选失败仍保留逐项原因；测试通过不表示所有候选可用。
        assert_eq!(
            report["all_profiles_encoded_and_reopened"],
            cases
                .iter()
                .all(|case| case["encoded_and_reopened"] == true),
        );
    }
}
