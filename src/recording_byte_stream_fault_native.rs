//! 只检查系统 IStream 包装器的异步写入错误回传，不创建 SinkWriter、MFT 或 MP4。
#![cfg(test)]

use super::{create, SAFETY_CAP};
use crate::{media_foundation::MfRuntime, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{implement, AgileReference, Error, IUnknown, Ref},
    Win32::{
        Foundation::{E_NOTIMPL, E_UNEXPECTED, STG_E_MEDIUMFULL},
        Media::MediaFoundation::{
            IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFByteStream,
        },
    },
};

const INITIAL: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const REJECTED: [u8; 2] = [9, 10];
const WAIT_BUDGET: Duration = Duration::from_secs(5);
const RUN_BUDGET: Duration = Duration::from_secs(15);
const MAX_REPORT_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Default, Serialize)]
struct Completion {
    async_result_status_called: bool,
    async_result_status_returned: bool,
    // GetStatus 的 Err 是操作状态；不能提前跳过同一结果的 EndWrite。
    async_result_status_succeeded: Option<bool>,
    async_result_error_hresult: Option<u32>,
    async_result_error: Option<String>,
    end_write_called: bool,
    end_write_returned: bool,
    end_write_succeeded: Option<bool>,
    /// SDK EndWrite 的 Err 不提供有效 outcount；未知保持 None，不能伪填 0。
    bytes_written: Option<u32>,
    end_error_hresult: Option<u32>,
    end_error: Option<String>,
    callback_failure: Option<String>,
    callback_failure_hresult: Option<u32>,
    mutex_poisoned: bool,
}

struct WriteState {
    label: &'static str,
    // 地址从 BeginWrite 到 EndWrite 完成保持不变，回调持有此 Arc 的强引用。
    data: Box<[u8]>,
    // IMFByteStream 本身没有 Send/Sync；在回调当前线程 resolve 同一 COM 对象。
    stream: AgileReference<IMFByteStream>,
    invocations: AtomicU32,
    completion: Mutex<Completion>,
    ready: Condvar,
}

impl WriteState {
    fn lock(&self) -> MutexGuard<'_, Completion> {
        match self.completion.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                let mut guard = poison.into_inner();
                guard.mutex_poisoned = true;
                guard
            }
        }
    }

    fn fail(&self, message: &str, hresult: Option<u32>) {
        let mut completion = self.lock();
        if completion.callback_failure.is_none() {
            completion.callback_failure = Some(message.chars().take(256).collect());
            completion.callback_failure_hresult = hresult;
        }
        self.ready.notify_all();
    }

    fn invoke(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        let invocation = self.invocations.fetch_add(1, Ordering::Relaxed);
        if invocation != 0 {
            // 不对同一个异步操作重复 EndWrite，也不覆盖第一次完成结果。
            self.fail("duplicate Invoke; EndWrite was not called again", None);
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        log(self.label, "Invoke", "before");
        let work = (|| -> windows::core::Result<()> {
            let result = result.ok()?;
            self.lock().async_result_status_called = true;
            log(self.label, "async result GetStatus", "before");
            let status = unsafe { result.GetStatus() };
            log(self.label, "async result GetStatus", "returned");
            {
                let mut completion = self.lock();
                completion.async_result_status_returned = true;
                match status {
                    Ok(()) => completion.async_result_status_succeeded = Some(true),
                    Err(error) => {
                        completion.async_result_status_succeeded = Some(false);
                        completion.async_result_error_hresult = Some(error.code().0 as u32);
                        completion.async_result_error = Some(clip(&error.to_string()));
                    }
                }
            }
            let stream = self.stream.resolve()?;
            {
                let mut completion = self.lock();
                completion.end_write_called = true;
            }
            // 不持 completion Mutex 跨 COM 调用。
            log(self.label, "EndWrite", "before");
            let ended = unsafe { stream.EndWrite(result) };
            log(self.label, "EndWrite", "returned");
            let mut completion = self.lock();
            completion.end_write_returned = true;
            match ended {
                Ok(bytes) => {
                    completion.end_write_succeeded = Some(true);
                    completion.bytes_written = Some(bytes);
                }
                Err(error) => {
                    completion.end_write_succeeded = Some(false);
                    completion.end_error_hresult = Some(error.code().0 as u32);
                    completion.end_error = Some(error.to_string().chars().take(256).collect());
                    completion.bytes_written = None;
                }
            }
            self.ready.notify_all();
            Ok(())
        })();
        if let Err(error) = &work {
            self.fail(&error.to_string(), Some(error.code().0 as u32));
        }
        log(self.label, "Invoke", "returned");
        work
    }

    fn wait(&self) -> (Completion, bool) {
        log(self.label, "completion wait", "before");
        let completion = self.lock();
        let waited = self
            .ready
            .wait_timeout_while(completion, WAIT_BUDGET, |state| {
                !state.end_write_returned && state.callback_failure.is_none()
            });
        let (mut completion, timeout) = match waited {
            Ok(value) => value,
            Err(poison) => {
                let (mut completion, timeout) = poison.into_inner();
                completion.mutex_poisoned = true;
                (completion, timeout)
            }
        };
        let timed_out = timeout.timed_out() && !completion.end_write_returned;
        if timed_out && completion.callback_failure.is_none() {
            completion.callback_failure = Some("callback wait exceeded five seconds".into());
        }
        let snapshot = completion.clone();
        drop(completion);
        log(self.label, "completion wait", "returned");
        (snapshot, timed_out)
    }
}

// 锁定 windows-implement 0.60.2 的默认 agile 实现同时提供 IAgileObject/IMarshal。
// 只有线程安全的数据和可合法 resolve 的 AgileReference 进入回调；不手写 Send/Sync。
#[implement(IMFAsyncCallback)]
struct WriteCallback {
    state: Arc<WriteState>,
}

#[allow(non_snake_case)]
impl IMFAsyncCallback_Impl for WriteCallback_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        // 官方允许 E_NOTIMPL：使用标准工作队列和默认调度标志。
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn Invoke(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        match catch_unwind(AssertUnwindSafe(|| self.state.invoke(result))) {
            Ok(result) => result,
            Err(_) => {
                self.state
                    .fail("panic contained inside Invoke", Some(E_UNEXPECTED.0 as u32));
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }
}

// 编译时核对，不启动线程或原生操作。
#[allow(dead_code)]
fn thread_safety_contract() {
    fn check<T: Send + Sync>() {}
    check::<WriteState>();
    check::<WriteCallback>();
}

struct WriteOutcome {
    report: Value,
    begin_succeeded: bool,
    begin_error_hresult: Option<u32>,
    invokes: u32,
    completion: Completion,
    pending_retained: bool,
}

fn write_once(
    stream: &IMFByteStream,
    label: &'static str,
    bytes: &[u8],
    started: Instant,
) -> Result<WriteOutcome> {
    checkpoint(started)?;
    let state = Arc::new(WriteState {
        label,
        data: bytes.to_vec().into_boxed_slice(),
        stream: AgileReference::new(stream)?,
        invocations: AtomicU32::new(0),
        completion: Mutex::new(Completion::default()),
        ready: Condvar::new(),
    });
    let callback: IMFAsyncCallback = WriteCallback {
        state: state.clone(),
    }
    .into();
    log(label, "BeginWrite", "before");
    let begin = unsafe { stream.BeginWrite(&state.data, &callback, None::<&IUnknown>) };
    log(label, "BeginWrite", "returned");
    let begin_error_hresult = begin.as_ref().err().map(|error| error.code().0 as u32);
    let begin_error = begin.as_ref().err().map(|error| clip(&error.to_string()));
    // callback 可能早于 BeginWrite 返回；wait 先检查已有结果，不会丢失唤醒。
    let (completion, timed_out) = if begin.is_ok() {
        state.wait()
    } else {
        (state.lock().clone(), false)
    };
    let invokes = state.invocations.load(Ordering::Relaxed);
    let pending_retained = !completion.end_write_returned && (begin.is_ok() || invokes != 0);
    log(label, "callback/state release", "before");
    if pending_retained {
        // 只用于失败测试的有界保活：最坏两个 callback、共十字节输入，留到进程退出。
        // 不依赖超时后 native 一定仍持 callback 引用，也不释放其可能仍在使用的输入。
        std::mem::forget(callback);
        std::mem::forget(state);
    } else {
        drop(callback);
        drop(state);
    }
    log(
        label,
        "callback/state release",
        if pending_retained {
            "returned; retained pending"
        } else {
            "returned"
        },
    );
    let report = json!({"label":label,"requested_bytes":bytes.len(),
        "begin_succeeded":begin.is_ok(),"begin_error_hresult":begin_error_hresult,"begin_error":begin_error,
        "invoke_count_observed":invokes,"completion":completion,"wait_timed_out":timed_out,
        "pending_callback_buffer_stream_retained_until_process_exit":pending_retained,
        "caller_callback_state_released":!pending_retained,
        "error_path":if begin.is_err(){"begin_failed"}else if !completion.end_write_returned{"pending_or_callback_failed"}else if completion.end_write_succeeded==Some(false){"end_failed"}else if completion.async_result_status_succeeded==Some(false){"async_status_failed_end_succeeded"}else{"end_succeeded"}});
    Ok(WriteOutcome {
        report,
        begin_succeeded: begin.is_ok(),
        begin_error_hresult,
        invokes,
        completion,
        pending_retained,
    })
}

#[test]
#[ignore = "真实 MF 字节流异步错误回传；无 SinkWriter/MFT/MP4，不采集设备或开窗口"]
fn recording_byte_stream_fault_native_async_write_returns_storage_error() -> Result<()> {
    let directory = unique_directory()?;
    let path = directory.join("stream.bin");
    let started = Instant::now();
    // Runtime 始终只由发起线程持有和释放，不放入 callback 的 Arc。
    let mut runtime = None;
    let mut stream = None;
    let mut control = None;
    let mut writes = Vec::with_capacity(2);
    let mut arm_limit = None;
    let mut snapshot = Value::Null;
    let mut pending = false;
    let mut baseline_ok = false;
    let mut fault_ok = false;
    let mut adapter_gate = false;
    let mut bytes_unchanged = None;
    let mut stage = "initialize_mf";
    let mut errors = Vec::new();
    let body = (|| -> Result<()> {
        runtime = Some(MfRuntime::new()?);
        checkpoint(started)?;
        stage = "create_byte_stream";
        let (created, created_control) = create(&path)?;
        stream = Some(created);
        control = Some(created_control);
        stage = "initial_async_write";
        let first = write_once(
            stream.as_ref().ok_or("Missing stream")?,
            "initial",
            &INITIAL,
            started,
        )?;
        pending |= first.pending_retained;
        baseline_ok = first.begin_succeeded
            && first.invokes == 1
            && first.completion.async_result_status_called
            && first.completion.async_result_status_returned
            && first.completion.async_result_status_succeeded == Some(true)
            && first.completion.end_write_called
            && first.completion.end_write_returned
            && first.completion.end_write_succeeded == Some(true)
            && first.completion.bytes_written == Some(INITIAL.len() as u32)
            && first.completion.callback_failure.is_none()
            && !first.completion.mutex_poisoned;
        writes.push(first.report);
        checkpoint(started)?;
        if !baseline_ok {
            return Err(
                "Initial eight-byte asynchronous write did not complete exactly once".into(),
            );
        }
        stage = "arm_and_position";
        let before = control.as_ref().ok_or("Missing control")?.snapshot()?;
        if before.actual_len != INITIAL.len() as u64
            || before.injected_count != 0
            || before.cap_hits != 0
        {
            return Err("Unexpected baseline file extent or injection".into());
        }
        arm_limit = Some(
            control
                .as_ref()
                .ok_or("Missing control")?
                .arm_after_current_len(1)?,
        );
        if arm_limit != Some(INITIAL.len() as u64 + 1) {
            return Err("Unexpected atomic arm limit".into());
        }
        checkpoint(started)?;
        unsafe {
            stream
                .as_ref()
                .ok_or("Missing stream")?
                .SetCurrentPosition(before.actual_len)?;
        }
        checkpoint(started)?;
        stage = "rejected_async_write";
        let rejected = write_once(
            stream.as_ref().ok_or("Missing stream")?,
            "rejected",
            &REJECTED,
            started,
        )?;
        pending |= rejected.pending_retained;
        let medium_full = STG_E_MEDIUMFULL.0 as u32;
        let direct_error = !rejected.begin_succeeded
            && rejected.begin_error_hresult == Some(medium_full)
            && rejected.invokes == 0
            && !rejected.completion.end_write_called;
        let async_error = rejected.begin_succeeded
            && rejected.invokes == 1
            && rejected.completion.end_write_called
            && rejected.completion.end_write_returned
            && rejected.completion.end_write_succeeded == Some(false)
            && rejected.completion.end_error_hresult == Some(medium_full)
            && rejected.completion.bytes_written.is_none();
        // GetStatus 单独记录；即使其报错，也不能把 EndWrite 的成功零字节改写为失败。
        fault_ok = (direct_error || async_error)
            && !rejected.pending_retained
            && rejected.completion.callback_failure.is_none()
            && !rejected.completion.mutex_poisoned;
        writes.push(rejected.report);
        checkpoint(started)?;
        if !fault_ok {
            return Err(
                "The rejected write did not return STG_E_MEDIUMFULL through BeginWrite or EndWrite"
                    .into(),
            );
        }
        Ok(())
    })();
    if let Err(error) = body {
        errors.push(json!({"stage":stage,"error":clip(&error.to_string())}));
    }

    // 即使异步完成失败或仍 pending，也保留本次实际注入观测；此统计不代表 native 完成。
    if let Some(control) = control.as_ref() {
        log("test", "final adapter snapshot", "before");
        let sampled = control.snapshot();
        log("test", "final adapter snapshot", "returned");
        match sampled {
            Ok(stats) => {
                adapter_gate = stats.armed
                    && stats.arm_calls == 1
                    && stats.injected_count > 0
                    && stats.cap_hits > 0
                    && stats.actual_len == INITIAL.len() as u64
                    && stats.high_water <= SAFETY_CAP
                    && stats.limit == INITIAL.len() as u64 + 1
                    && Some(stats.limit) == arm_limit
                    && stats.safety_cap == SAFETY_CAP;
                match serde_json::to_value(&stats) {
                    Ok(value) => snapshot = value,
                    Err(error) => {
                        errors.push(json!({"stage":"serialize_final_snapshot","error":clip(&error.to_string())}));
                    }
                }
                if !adapter_gate {
                    errors.push(json!({"stage":"adapter_gate","error":"The final adapter snapshot did not prove the expected injection and original extent"}));
                }
            }
            Err(error) => {
                errors.push(json!({"stage":"final_snapshot","error":clip(&error.to_string())}));
            }
        }
    }

    log("test", "caller byte stream/control release", "before");
    drop(stream.take());
    drop(control.take());
    log("test", "caller byte stream/control release", "returned");
    if !pending {
        let verified = (|| -> Result<()> {
            checkpoint(started)?;
            let metadata = fs::metadata(&path)?;
            if metadata.len() != INITIAL.len() as u64 {
                return Err("The file length changed from eight bytes".into());
            }
            let mut actual = Vec::with_capacity(INITIAL.len() + 1);
            File::open(&path)?
                .take(INITIAL.len() as u64 + 1)
                .read_to_end(&mut actual)?;
            bytes_unchanged = Some(actual.as_slice() == INITIAL.as_slice());
            if bytes_unchanged != Some(true) {
                return Err("The original eight bytes were changed".into());
            }
            checkpoint(started)
        })();
        if let Err(error) = verified {
            errors.push(json!({"stage":"verify_retained_file","error":clip(&error.to_string())}));
        }
    }
    log("test", "MF runtime release", "before");
    if pending {
        // 不在 MF native 工作可能仍进行时 Shutdown/CoUninitialize。只在发起线程保留。
        if let Some(runtime) = runtime.take() {
            std::mem::forget(runtime);
        }
    } else {
        drop(runtime.take());
    }
    log(
        "test",
        "MF runtime release",
        if pending {
            "returned; retained pending"
        } else {
            "returned"
        },
    );
    if let Err(error) = checkpoint(started) {
        errors.push(json!({"stage":"after_release","error":clip(&error.to_string())}));
    }
    let report = json!({"ok":errors.is_empty() && writes.len()==2 && baseline_ok && fault_ok && adapter_gate && bytes_unchanged == Some(true) && !pending,
        "writes":writes,"baseline_ok":baseline_ok,"fault_ok":fault_ok,"adapter_gate":adapter_gate,"file_contents_match_original_eight_bytes":bytes_unchanged,
        "arm_limit_bytes":arm_limit,"snapshot":snapshot,"errors":errors,"elapsed_ms":started.elapsed().as_millis(),
        "pending_runtime_callback_and_buffer_retained_until_process_exit":pending,
        "limits":{"writes":2,"input_bytes_total":10,"callback_wait_seconds":WAIT_BUDGET.as_secs(),
            "between_calls_run_seconds":RUN_BUDGET.as_secs(),"adapter_extent_bytes":SAFETY_CAP,"report_bytes":MAX_REPORT_BYTES},
        "scope":["Real MFCreateMFByteStreamOnStream wrapper only: no SinkWriter, MFT, MP4 mux, media decode, device capture or GUI.",
            "The baseline must complete one Invoke and EndWrite with eight bytes. The injected write must return actual STG_E_MEDIUMFULL, either directly from BeginWrite or after a successful BeginWrite through EndWrite; timeout is never an injected-error success.",
            "GetStatus is observed once on the callback's same IMFAsyncResult before EndWrite, including a real error status. The SDK folds nonnegative status HRESULTs into success, so no exact success HRESULT is invented. GetStatus does not replace the BeginWrite/EndWrite failure gate, and EndWrite success with zero bytes remains a failed fault-propagation check.",
            "Only plain callback state crosses threads, with an AgileReference resolved in the callback's current COM context. No hand-written Send/Sync implementation is used.",
            "Pending or callback-failed operations retain callback, immutable input buffer, byte stream and the initiating-thread MF runtime until this test process exits. This failure-only safety retention is not proof of native completion, release or recovery.",
            "Waits and total checks are cooperative; a blocking native call or release cannot be interrupted. The external test-process deadline remains the final bound.",
            "All owned file and report artifacts are retained; simulated storage exhaustion does not establish physical disk exhaustion."]});
    let report = write_report(&directory, report)?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "MF byte-stream fault report: {}",
        directory.join("report.json").display()
    );
    assert_eq!(
        report["ok"].as_bool(),
        Some(true),
        "MF byte-stream wrapper failed; evidence: {}",
        directory.join("report.json").display()
    );
    Ok(())
}

fn checkpoint(started: Instant) -> Result<()> {
    if started.elapsed() > RUN_BUDGET {
        return Err("Byte-stream test exceeded the fifteen-second cooperative budget".into());
    }
    Ok(())
}

fn log(label: &str, operation: &str, phase: &str) {
    // stderr 断开不能使 COM 回调 unwind。
    let _ = writeln!(
        std::io::stderr().lock(),
        "MF byte-stream fault [{label}] {operation}: {phase}"
    );
}

fn clip(value: &str) -> String {
    value.chars().take(256).collect()
}

fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target/recording-storage-fault");
    fs::create_dir_all(parent)?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!("wrapper-{}-{time}-{index}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("Unable to create a unique byte-stream fault directory".into())
}

fn write_report(directory: &Path, report: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    let report = if bytes.len() > MAX_REPORT_BYTES {
        let limited =
            json!({"ok":false,"error":"report_exceeded_128kib","artifacts_retained":true});
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
