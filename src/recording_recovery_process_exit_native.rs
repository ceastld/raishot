//! 独立测试进程持有恢复租约时自终止；只验证已 Finalize 素材的旧登记增长。
//! 不证明未收尾媒体修复、掉电恢复或任意真实崩溃；运行权由显式 ignored 父测试控制。
#[path = "recording_recovery_process_job.rs"]
mod job;

use crate::recording::recovery::verification::{
    verify, AudioPresence, VerificationLimits, VerificationReport, VerificationStage,
    VerificationState,
};
use crate::recording::recovery::{
    self as recovery, FileIdentity, MediaName, MediaState, Phase, Session,
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
    ffi::OsString,
    fs::{self, OpenOptions, TryLockError},
    io::{self, Read, Write},
    os::windows::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant, SystemTime},
};
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess, CREATE_NO_WINDOW};

const ROLE_ENV: &str = "RUSTSHOT_RECOVERY_PROCESS_EXIT_ROLE";
const ROOT_ENV: &str = "RUSTSHOT_RECOVERY_PROCESS_EXIT_ROOT";
const TOKEN_ENV: &str = "RUSTSHOT_RECOVERY_PROCESS_EXIT_TOKEN";
const CHILD_ROLE: &str = "owned_recovery_fixture_child_v1";
const CHILD_NAME: &str = "recording_recovery_native_process_exit_child";
const STAGING_NAME: &str = ".rustshot-recording-process-exit-native";
const CHILD_REPORT: &str = "child-report.json";
const CHILD_ERROR: &str = "child-error.json";
const TERMINATION_CODE: u32 = 0x5A17;
const FIXTURE_LIMIT: u64 = 4 * 1024 * 1024;
const CHILD_REPORT_LIMIT: u64 = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 128;
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
const CHILD_SOFT_BUDGET: Duration = Duration::from_secs(18);
const CHILD_WAIT_BUDGET: Duration = Duration::from_secs(20);
const PARENT_SOFT_BUDGET: Duration = Duration::from_secs(25);
const DROP_REAP_BUDGET: Duration = Duration::from_millis(250);

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, message).into())
    }
}

fn checkpoint(started: Instant, budget: Duration) -> Result<()> {
    if started.elapsed() >= budget {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "恢复进程试验调用间预算已用尽").into());
    }
    Ok(())
}

fn exact_child_filter() -> Result<String> {
    // module_path! 首段是 crate 名，而 libtest 的 --exact 名称不含它。
    let (_, module) = module_path!()
        .split_once("::")
        .ok_or("无法取得恢复子测试的完整模块名")?;
    Ok(format!("{module}::{CHILD_NAME}"))
}

fn child_arguments(filter: &str) -> Vec<OsString> {
    ["--exact", filter, "--ignored", "--test-threads=1"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn safe_metadata(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_dir() == directory
            && (directory || metadata.is_file())
            && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
        "进程试验路径不是预期的普通本地对象",
    )?;
    Ok(metadata)
}

fn validated_root(root: &Path, token: &str) -> Result<PathBuf> {
    // 先做词法门；在此之前不 canonicalize 或读取环境所指向的任意路径。
    let parent = std::path::absolute("target/recording-recovery-verify")?;
    let name = root.file_name().and_then(|name| name.to_str());
    require(
        root.is_absolute()
            && root.parent() == Some(parent.as_path())
            && name.is_some_and(|name| {
                name.starts_with("native-")
                    && name.len() > "native-".len()
                    && name.len() <= MAX_TOKEN_BYTES
                    && name.is_ascii()
                    && token == name
            }),
        "恢复子进程的根目录或许可 token 不符合限定目录合同",
    )?;
    safe_metadata(&parent, true)?;
    safe_metadata(root, true)?;
    let canonical_parent = fs::canonicalize(&parent)?;
    let canonical_root = fs::canonicalize(root)?;
    require(
        canonical_root.parent() == Some(canonical_parent.as_path())
            && canonical_root.file_name() == root.file_name(),
        "恢复子进程根目录规范化后脱离限定父目录",
    )?;
    // 环境和 receipt 保留父传入的普通 absolute 路径，避免混用扩展路径前缀。
    Ok(root.to_path_buf())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata = safe_metadata(path, false)?;
    require(metadata.len() <= maximum, "恢复试验输入超过读取上限")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(metadata.len())?)?;
    let read_limit = maximum.checked_add(1).ok_or("恢复试验读取上限溢出")?;
    fs::File::open(path)?
        .take(read_limit)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 == metadata.len() && bytes.len() as u64 <= maximum,
        "恢复试验读取时文件长度发生变化",
    )?;
    Ok(bytes)
}

fn write_child_report(root: &Path, name: &str, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    require(
        bytes.len() as u64 <= CHILD_REPORT_LIMIT,
        "恢复子进程回执超过 64 KiB",
    )?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn optional_child_report(root: &Path, name: &str) -> Result<Option<Value>> {
    match fs::symlink_metadata(root.join(name)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => Ok(Some(serde_json::from_slice(&bounded_read(
            &root.join(name),
            CHILD_REPORT_LIMIT,
        )?)?)),
    }
}

fn same_identity_values(left: &Value, right: &Value) -> bool {
    ["volume", "id", "created"].into_iter().all(|key| {
        left.get(key).is_some_and(|value| !value.is_null()) && left.get(key) == right.get(key)
    })
}

fn producer_ok(value: &Value) -> bool {
    value["encoded_and_reopened"] == true
        && value["selected_codec"] == "h264"
        && value["encoding_finalized"] == true
        && value["decode_completed"] == true
        && value["readback"]["video_frames"] == 12
        && value["readback"]["native_audio_codec"] == "aac"
        && value["readback"]["audio_samples"]
            .as_u64()
            .is_some_and(|samples| samples > 0)
}

fn child_body(root: &Path, token: &str, report: &mut Value, stage: &mut &str) -> Result<()> {
    let started = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let trial_directory = root.join("encoder-trial");
    *stage = "create_owned_fixture_directory";
    fs::create_dir(&trial_directory)?;
    *stage = "generate_and_fully_reopen_healthy_fixture";
    let trial = {
        let _runtime = MfRuntime::new()?;
        crate::recording_encoder_trial::run_profile_case(
            &trial_directory,
            EncoderPolicy::Auto,
            QualityProfile::Current(Preset::CompactMp4),
            &stop,
        )?
    }; // 编码/完整回读已经返回，MfRuntime 正常退出；终止阶段没有未 Finalize 的 writer。
    checkpoint(started, CHILD_SOFT_BUDGET)?;
    let attempts = trial["attempts"]
        .as_array()
        .ok_or("恢复子进程素材缺少尝试记录")?;
    let attempt = attempts.last().ok_or("恢复子进程素材没有成功尝试")?;
    let index = attempt["index"]
        .as_u64()
        .filter(|index| *index < 3)
        .ok_or("恢复子进程素材尝试编号无效")?;
    report["producer"] = json!({
        "encoded_and_reopened":trial["encoded_and_reopened"],
        "selected_codec":trial["selected_codec"], "attempt_count":attempts.len(),
        "selected_attempt_index":index,"encoding_finalized":attempt["encoding_finalized"],
        "decode_completed":attempt["decode_completed"],
        "readback": {"video_frames":attempt["reopen"]["video_frames"],
            "native_audio_codec":attempt["reopen"]["native_audio_codec"],
            "audio_samples":attempt["reopen"]["audio_samples"]}
    });
    require(
        producer_ok(&report["producer"]),
        "恢复子进程健康素材未通过完整往返",
    )?;
    *stage = "read_bounded_healthy_fixture";
    let healthy = trial_directory.join(format!("Auto-{index}.mp4"));
    let healthy_before = file_snapshot::inspect(&healthy)?.ok_or("健康素材不存在")?;
    let healthy_bytes = bounded_read(&healthy, FIXTURE_LIMIT)?;
    require(!healthy_bytes.is_empty(), "健康素材为空")?;
    require(
        file_snapshot::inspect(&healthy)?.as_ref() == Some(&healthy_before),
        "健康素材读取期间发生变化",
    )?;
    report["healthy_snapshot"] = super::native_snapshot_value(&healthy_before);
    checkpoint(started, CHILD_SOFT_BUDGET)?;

    *stage = "register_empty_same_file_identity";
    let staging = root.join(STAGING_NAME);
    fs::create_dir(&staging)?;
    let mut session = Session::create(&staging)?;
    let source = staging.join(MediaName::Video.basename());
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&source)?;
    let empty = file_snapshot::snapshot(&file)?;
    require(empty.bytes == 0, "初始登记视频不是空文件")?;
    session.register_file(MediaName::Video, &empty)?;
    session.set_phase(Phase::Recording)?;
    report["registered_snapshot"] = super::native_snapshot_value(&empty);
    *stage = "grow_registered_file_without_reregistration";
    file.write_all(&healthy_bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    let grown = file_snapshot::inspect(&source)?.ok_or("增长后的登记文件不存在")?;
    require(
        FileIdentity::from(&empty) == FileIdentity::from(&grown)
            && grown.bytes == healthy_bytes.len() as u64
            && grown.bytes > 0,
        "登记文件增长后没有保留同一身份和完整长度",
    )?;
    report["grown_snapshot"] = super::native_snapshot_value(&grown);
    report["same_identity_grew"] = true.into();
    *stage = "prove_live_session_holds_exclusive_lease";
    let competing = recovery::open_checked_file(&staging.join(recovery::LEASE_NAME))?;
    match competing.try_lock() {
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(error)) => return Err(error.into()),
        Ok(()) => return Err("Session 存活期间独占租约未阻止第二次加锁".into()),
    }
    drop(competing);
    report["lease_would_block_while_session_alive"] = true.into();
    checkpoint(started, CHILD_SOFT_BUDGET)?;
    *stage = "terminate_current_process_with_session_alive";
    report["token"] = token.into();
    report["ready_for_self_termination"] = true.into();
    report["requested_exit_code"] = TERMINATION_CODE.into();
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    write_child_report(root, CHILD_REPORT, report)?;
    std::hint::black_box(&session);
    // 只终止这个明确受门的当前测试子进程；不运行 Rust Drop 或 ExitProcess DLL detach。
    // 父进程必须实际 wait 到专用退出码；这份事前回执本身不证明终止已经发生。
    unsafe { TerminateProcess(GetCurrentProcess(), TERMINATION_CODE) }?;
    Err("当前子进程 TerminateProcess 意外返回，不能计作已终止".into())
}

#[test]
#[ignore = "仅供专用父测试 exact 调用；必须 role/root/token 与 Job 后 stdin 许可，禁止单独当验证运行"]
fn recording_recovery_native_process_exit_child() -> Result<()> {
    require(
        std::env::var(ROLE_ENV).ok().as_deref() == Some(CHILD_ROLE),
        "恢复子测试缺少专用角色门",
    )?;
    let filter = exact_child_filter()?;
    require(
        std::env::args_os().skip(1).collect::<Vec<_>>() == child_arguments(&filter),
        "恢复子测试必须由父以唯一完整 --exact/--ignored 名称启动",
    )?;
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).ok_or("恢复子测试缺少根目录")?);
    let token = std::env::var(TOKEN_ENV).map_err(|_| "恢复子测试缺少许可 token")?;
    let root = validated_root(&root, &token)?;
    // 在 Job assign 前父若退出，写端关闭会得到 EOF；没有完整许可就不写文件或启动 MF。
    let expected = format!("{token}\n").into_bytes();
    let mut permission = Vec::new();
    io::stdin()
        .lock()
        .take((MAX_TOKEN_BYTES + 2) as u64)
        .read_to_end(&mut permission)?;
    require(permission == expected, "恢复子测试没有收到父进程完整许可")?;

    let mut stage = "start_owned_child_body";
    let mut report = json!({"ready_for_self_termination":false,
        "scope":"Pre-finalized owned H264/AAC fixture, registered at zero bytes and grown on the same file identity; the live Session is retained until self-TerminateProcess. This is not unfinished-writer repair, power-loss recovery or an arbitrary crash."});
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        child_body(&root, &token, &mut report, &mut stage)
    }));
    report["ready_for_self_termination"] = false.into();
    report["failure_stage"] = stage.into();
    match outcome {
        Ok(Err(error)) => {
            report["error"] = error
                .to_string()
                .chars()
                .take(2048)
                .collect::<String>()
                .into();
            write_child_report(&root, CHILD_ERROR, &report)?;
            Err(error)
        }
        Ok(Ok(())) => {
            report["error"] = "子测试正常返回，未证明自终止".into();
            write_child_report(&root, CHILD_ERROR, &report)?;
            Err("恢复子测试未执行预期自终止".into())
        }
        Err(_) => {
            report["error"] = "恢复子测试捕获 panic，未证明自终止".into();
            write_child_report(&root, CHILD_ERROR, &report)?;
            Err("恢复子测试发生 panic".into())
        }
    }
}

struct OwnedChild {
    child: Child,
    job: job::JobGuard,
    assigned: bool,
    permission_sent: bool,
    timed_out: bool,
    observed_exit: bool,
    exit_code: Option<i32>,
}

impl OwnedChild {
    fn new(child: Child, job: job::JobGuard) -> Self {
        Self {
            child,
            job,
            assigned: false,
            permission_sent: false,
            timed_out: false,
            observed_exit: false,
            exit_code: None,
        }
    }

    fn assign(&mut self) -> io::Result<()> {
        self.job.assign(&self.child)?;
        self.assigned = true;
        Ok(())
    }

    fn permit(&mut self, token: &str) -> Result<()> {
        require(self.assigned, "尚未分配 Job，不能许可子进程启动 MF")?;
        let mut stdin = self.child.stdin.take().ok_or("子进程许可管道不存在")?;
        stdin.write_all(format!("{token}\n").as_bytes())?;
        drop(stdin);
        self.permission_sent = true;
        Ok(())
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let started = Instant::now();
        loop {
            let status = self.child.try_wait()?;
            if let Some(status) = &status {
                self.observed_exit = true;
                self.exit_code = status.code();
            }
            if started.elapsed() >= CHILD_WAIT_BUDGET {
                self.timed_out = true;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "恢复子进程未在 20 秒调用间等待预算内结束",
                ));
            }
            if let Some(status) = status {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn snapshot(&self) -> Value {
        json!({"assigned_to_kill_on_close_job":self.assigned,
            "stdin_permission_sent":self.permission_sent,"timed_out":self.timed_out,
            "exit_observed":self.observed_exit,"exit_code":self.exit_code,
            "wait_budget_ms":CHILD_WAIT_BUDGET.as_millis(),
            "drop_reap_budget_ms":DROP_REAP_BUDGET.as_millis()})
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        if !self.observed_exit {
            // 只操作自己持有的 Child；不按名字/PID查杀。Job 随后 close 再兜住残留子进程。
            let _ = self.child.kill();
            let started = Instant::now();
            while started.elapsed() < DROP_REAP_BUDGET {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
    }
}

fn verified_grown(report: &VerificationReport) -> bool {
    report.state == VerificationState::VerifiedReadable
        && report.stage == VerificationStage::Complete
        && report.registered_bytes == Some(0)
        && report.registration_content_unchanged == Some(false)
        && report.registered_identity_matches
        && report.metadata_stable
        && report.video_frames == 12
        && report.video_eof
        && report.audio_presence == AudioPresence::Present
        && report.audio_frames > 0
        && report.audio_eof
        && !report.lease_held
}

fn verified_grown_value(report: &Value, lease_held: bool) -> bool {
    report["state"] == "verified_readable"
        && report["stage"] == "complete"
        && report["registered_bytes"] == 0
        && report["registration_content_unchanged"] == false
        && report["registered_identity_matches"] == true
        && report["metadata_stable"] == true
        && report["video_frames"] == 12
        && report["video_eof"] == true
        && report["audio_presence"] == "present"
        && report["audio_eof"] == true
        && report["audio_frames"]
            .as_u64()
            .is_some_and(|frames| frames > 0)
        && report["lease_held"] == lease_held
}

fn recovered_copy_ok(copy: &Value, bytes: u64) -> bool {
    copy["prepared"] == true
        && copy["committed"] == true
        && verified_grown_value(&copy["preparation"], true)
        && verified_grown_value(&copy["publication"]["verification"], false)
        && copy["publication"]["copied_bytes"] == bytes
        && copy["source_retained"] == true
        && copy["bytes_equal"] == true
        && copy["output_readback"]["decoded"] == true
        && copy["output_readback"]["video_frames"] == 12
        && copy["output_readback"]["video_eof"] == true
        && copy["output_readback"]["audio_presence"] == "present"
        && copy["output_readback"]["audio_frames"]
            .as_u64()
            .is_some_and(|frames| frames > 0)
        && copy["output_readback"]["audio_eof"] == true
}

#[test]
#[ignore = "独立子测试进程持有租约时自终止；自绘静音素材，不读桌面/用户媒体、不播音、不开窗口"]
fn recording_recovery_native_process_exit_recovers_grown_clip() -> Result<()> {
    let root = std::path::absolute(super::unique_directory()?)?;
    let token = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("恢复进程试验独占目录名无效")?
        .to_owned();
    let root = validated_root(&root, &token)?;
    let started = Instant::now();
    let mut stage = "create_job_before_child";
    let mut report = json!({"ok":false,"mode":"same_identity_growth_after_process_termination",
        "expected_exit_code":TERMINATION_CODE,"fixture_limit_bytes":FIXTURE_LIMIT,
        "child_report_limit_bytes":CHILD_REPORT_LIMIT,
        "parent_cooperative_budget_ms":PARENT_SOFT_BUDGET.as_millis(),
        "child_cooperative_budget_ms":CHILD_SOFT_BUDGET.as_millis(),
        "scope":["A fresh fully finalized and EOF-decoded owned H264/AAC fixture is copied into a file registered at zero bytes, without changing its identity or refreshing registration. The live Session is ended by self-TerminateProcess before publication.",
            "The parent assigns only its own Child to an anonymous kill-on-close Job before sending an stdin permission token. Before assignment, loss of the parent closes the pipe and prevents this test function from starting MF or writing fixtures. Only the dedicated expected exit code plus a complete ready receipt qualifies.",
            "OS lease probes, fresh full verification and NoReplace copy/readback are checked after child exit. This does not prove repair of unfinished writers, power-loss recovery, arbitrary crashes, physical storage faults, GUI behavior or audio/video synchronization.",
            "All artifacts are retained. Budgets are cooperative between calls; native/I/O calls can block. Child wait is 20 seconds, Drop polls only for 250 milliseconds after terminating its own Child, and Job close is the external-parent-death fallback. C1 must still use the separate 30-second process boundary."]});
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<bool> {
        let job = job::JobGuard::new()?;
        let filter = exact_child_filter()?;
        report["exact_child_filter"] = filter.clone().into();
        stage = "spawn_gated_child_waiting_for_stdin";
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(child_arguments(&filter))
            .env(ROLE_ENV, CHILD_ROLE)
            .env(ROOT_ENV, &root)
            .env(TOKEN_ENV, &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0);
        let mut child = OwnedChild::new(command.spawn()?, job);
        stage = "assign_child_before_permission";
        let assigned = child.assign();
        report["child_process"] = child.snapshot();
        assigned?;
        stage = "send_stdin_permission_after_assignment";
        child.permit(&token)?;
        report["child_process"] = child.snapshot();
        stage = "wait_for_dedicated_self_termination";
        let waited = child.wait();
        report["child_process"] = child.snapshot();
        drop(child); // 超时/Err/panic 均只回收自有进程；不存在裸 Child 脱离 Job 的许可窗口。
        report["child_receipt"] =
            optional_child_report(&root, CHILD_REPORT)?.unwrap_or(Value::Null);
        report["child_error"] = optional_child_report(&root, CHILD_ERROR)?.unwrap_or(Value::Null);
        let status = waited?;
        checkpoint(started, PARENT_SOFT_BUDGET)?;
        require(
            status.code() == Some(TERMINATION_CODE as i32)
                && report["child_process"]["timed_out"] == false
                && report["child_process"]["assigned_to_kill_on_close_job"] == true
                && report["child_process"]["stdin_permission_sent"] == true,
            "子进程没有在预算内以专用自终止码退出",
        )?;
        let receipt = report["child_receipt"].clone();
        require(
            report["child_error"].is_null()
                && receipt["ready_for_self_termination"] == true
                && receipt["token"] == token
                && receipt["requested_exit_code"] == TERMINATION_CODE
                && receipt["lease_would_block_while_session_alive"] == true
                && receipt["registered_snapshot"]["bytes"] == 0
                && receipt["same_identity_grew"] == true
                && same_identity_values(
                    &receipt["registered_snapshot"],
                    &receipt["grown_snapshot"],
                )
                && producer_ok(&receipt["producer"]),
            "子进程事前回执未满足完整证据门",
        )?;
        let index = receipt["producer"]["selected_attempt_index"]
            .as_u64()
            .filter(|index| *index < 3)
            .ok_or("子素材尝试编号无效")?;
        let staging = root.join(STAGING_NAME);
        let source = staging.join(MediaName::Video.basename());
        let healthy = root.join("encoder-trial").join(format!("Auto-{index}.mp4"));
        stage = "check_retained_owned_bytes_after_child_exit";
        let source_before = file_snapshot::inspect(&source)?.ok_or("进程退出后登记源不存在")?;
        let healthy_before = file_snapshot::inspect(&healthy)?.ok_or("进程退出后健康原件不存在")?;
        let source_bytes = bounded_read(&source, FIXTURE_LIMIT)?;
        let healthy_bytes = bounded_read(&healthy, FIXTURE_LIMIT)?;
        require(
            source_bytes == healthy_bytes
                && !source_bytes.is_empty()
                && super::native_snapshot_value(&source_before) == receipt["grown_snapshot"]
                && super::native_snapshot_value(&healthy_before) == receipt["healthy_snapshot"],
            "进程退出后源身份或完整内容不符合子进程回执",
        )?;
        report["source_before"] = super::native_snapshot_value(&source_before);
        report["healthy_before"] = super::native_snapshot_value(&healthy_before);
        stage = "scan_recording_manifest_and_probe_released_lease";
        let scanned = recovery::scan(&root, SystemTime::now(), &|| false);
        let scan_ok = !scanned.truncated
            && !scanned.cancelled
            && scanned.active_skipped == 0
            && scanned.sessions.len() == 1
            && scanned.sessions.first().is_some_and(|session| {
                session.directory == staging
                    && session.phase == Phase::Recording
                    && session.unverified
                    && !session.lease_held
                    && session.media.iter().any(|media| {
                        media.name == MediaName::Video
                            && media.state == MediaState::Present
                            && media.identity_matches == Some(true)
                            && media.content_unchanged == Some(false)
                            && media.bytes == Some(source_before.bytes)
                    })
            });
        report["scan"] = serde_json::to_value(scanned)?;
        report["lease_after_child_exit"] = super::native_exclusive_lease_probe(&staging);
        checkpoint(started, PARENT_SOFT_BUDGET)?;
        stage = "fresh_full_verification_of_grown_registration";
        let stop = AtomicBool::new(false);
        let verification = verify(
            &staging,
            MediaName::Video,
            &stop,
            VerificationLimits::default(),
        );
        let verification_ok = verified_grown(&verification)
            && verification.identity.as_ref() == Some(&FileIdentity::from(&source_before))
            && verification.bytes == Some(source_before.bytes);
        report["verification"] = serde_json::to_value(verification)?;
        report["lease_after_verification"] = super::native_exclusive_lease_probe(&staging);
        checkpoint(started, PARENT_SOFT_BUDGET)?;
        stage = "copy_commit_and_full_reopen_after_child_exit";
        let copied = super::copy_registered_clip(&root, &staging)?;
        let copy_ok = recovered_copy_ok(&copied, source_before.bytes);
        report["copy"] = copied;
        report["lease_after_copy"] = super::native_exclusive_lease_probe(&staging);
        checkpoint(started, PARENT_SOFT_BUDGET)?;
        stage = "check_all_original_artifacts_retained";
        let source_after = file_snapshot::inspect(&source)?;
        let healthy_after = file_snapshot::inspect(&healthy)?;
        let retained = source_after.as_ref() == Some(&source_before)
            && healthy_after.as_ref() == Some(&healthy_before)
            && bounded_read(&source, FIXTURE_LIMIT)? == source_bytes
            && bounded_read(&healthy, FIXTURE_LIMIT)? == healthy_bytes;
        report["source_after"] = source_after
            .as_ref()
            .map(super::native_snapshot_value)
            .unwrap_or(Value::Null);
        report["healthy_after"] = healthy_after
            .as_ref()
            .map(super::native_snapshot_value)
            .unwrap_or(Value::Null);
        report["gates"] = json!({"scan_recording_same_identity_grew":scan_ok,
            "verification_old_zero_byte_registration":verification_ok,
            "copy_old_zero_byte_registration_and_full_readback":copy_ok,"originals_retained":retained});
        checkpoint(started, PARENT_SOFT_BUDGET)?;
        Ok(scan_ok
            && verification_ok
            && copy_ok
            && retained
            && [
                "lease_after_child_exit",
                "lease_after_verification",
                "lease_after_copy",
            ]
            .into_iter()
            .all(|key| report[key]["ok"] == true))
    }));
    match outcome {
        Ok(Ok(ok)) => report["ok"] = ok.into(),
        Ok(Err(error)) => {
            report["error"] = json!({"stage":stage,
            "message":error.to_string().chars().take(2048).collect::<String>()})
        }
        Err(_) => report["error"] = json!({"stage":stage,"message":"恢复父测试捕获 panic"}),
    }
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    super::write_report(&root, &report)?;
    assert_eq!(
        report["ok"],
        true,
        "进程退出后的恢复验证未通过，证据目录：{}；{report}",
        root.display()
    );
    Ok(())
}
