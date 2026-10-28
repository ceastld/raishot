//! Test-only asynchronous SinkWriter finalization observations; no media-readability claim.
#![cfg(test)]

#[path = "recording_byte_stream_error_proxy.rs"]
mod error_proxy;

use super::{classify_recording_error, MfRuntime, Mp4Writer};
use crate::{
    geometry::ScreenRect,
    recording_color_check::ColorChart,
    recording_fault_stream::{self, FaultControl},
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use error_proxy::{ErrorSource, ProxyControl, ProxySnapshot};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    error::Error as StdError,
    ffi::c_void,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{implement, Error, HRESULT},
    Win32::{
        Foundation::{E_UNEXPECTED, STG_E_MEDIUMFULL, S_OK},
        Media::MediaFoundation::{
            IMFByteStream, IMFSinkWriterCallback, IMFSinkWriterCallback_Impl,
        },
    },
};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const FPS: u32 = 30;
const FRAMES: u32 = 6;
const BITRATE: u32 = 4_000_000;
const MAX_EXTENT: u64 = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;
const WAIT_BUDGET: Duration = Duration::from_secs(5);
const RUN_BUDGET: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, Serialize)]
struct CallbackSnapshot {
    finalize_requested: bool,
    on_finalize_count: u32,
    /// The exact HRESULT supplied to OnFinalize, including successful nonzero codes.
    on_finalize_hresult: Option<u32>,
    on_marker_count: u32,
    callback_failure: Option<String>,
    mutex_poisoned: bool,
}

#[derive(Default)]
struct CallbackState {
    snapshot: Mutex<CallbackSnapshot>,
    changed: Condvar,
}

impl CallbackState {
    fn lock(&self) -> MutexGuard<'_, CallbackSnapshot> {
        match self.snapshot.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                let mut guard = poison.into_inner();
                guard.mutex_poisoned = true;
                guard
            }
        }
    }

    fn fail(&self, message: &str) {
        let mut snapshot = self.lock();
        if snapshot.callback_failure.is_none() {
            snapshot.callback_failure = Some(clip(message));
        }
        self.changed.notify_all();
    }

    fn observe_finalize(&self, status: HRESULT) {
        let mut snapshot = self.lock();
        snapshot.on_finalize_count = snapshot.on_finalize_count.saturating_add(1);
        if !snapshot.finalize_requested {
            snapshot.callback_failure =
                Some("OnFinalize arrived before the explicit request".into());
        }
        if snapshot.on_finalize_count == 1 {
            snapshot.on_finalize_hresult = Some(status.0 as u32);
        } else {
            snapshot.callback_failure = Some("duplicate OnFinalize; first status retained".into());
        }
        self.changed.notify_all();
    }

    fn wait(&self) -> (CallbackSnapshot, bool) {
        let snapshot = self.lock();
        let waited = self
            .changed
            .wait_timeout_while(snapshot, WAIT_BUDGET, |value| {
                value.on_finalize_count == 0 && value.callback_failure.is_none()
            });
        let (mut snapshot, timeout) = match waited {
            Ok(value) => value,
            Err(poison) => {
                let (mut snapshot, timeout) = poison.into_inner();
                snapshot.mutex_poisoned = true;
                (snapshot, timeout)
            }
        };
        let timed_out = timeout.timed_out() && snapshot.on_finalize_count == 0;
        if timed_out && snapshot.callback_failure.is_none() {
            snapshot.callback_failure = Some("OnFinalize wait exceeded five seconds".into());
        }
        (snapshot.clone(), timed_out)
    }
}

// windows-implement 0.60.2 supplies agile IUnknown/IMarshal by default. This callback
// holds only synchronized plain data; no writer, stream, Runtime or COM pointer crosses threads.
#[implement(IMFSinkWriterCallback)]
struct FinalizeCallback {
    state: Arc<CallbackState>,
}

#[allow(non_snake_case)]
impl IMFSinkWriterCallback_Impl for FinalizeCallback_Impl {
    fn OnFinalize(&self, status: HRESULT) -> windows::core::Result<()> {
        match catch_unwind(AssertUnwindSafe(|| {
            log("callback", "OnFinalize", "before");
            self.state.observe_finalize(status);
            log("callback", "OnFinalize", "returned");
        })) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.state.fail("panic contained inside OnFinalize");
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }

    fn OnMarker(&self, _: u32, _: *const c_void) -> windows::core::Result<()> {
        match catch_unwind(AssertUnwindSafe(|| {
            let mut snapshot = self.state.lock();
            snapshot.on_marker_count = snapshot.on_marker_count.saturating_add(1);
            snapshot.callback_failure =
                Some("OnMarker arrived without a PlaceMarker request".into());
            self.state.changed.notify_all();
        })) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.state.fail("panic contained inside OnMarker");
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }
}

#[allow(dead_code)]
fn callback_thread_safety() {
    fn check<T: Send + Sync>() {}
    check::<CallbackState>();
    check::<FinalizeCallback>();
}

/// Created before any COM operation. Only the initiating thread owns this object.
/// An early return or panic retains resources rather than invoking an implicit Finalize.
struct CaseOwner {
    label: &'static str,
    runtime: Option<MfRuntime>,
    stream: Option<IMFByteStream>,
    raw_stream: Option<IMFByteStream>,
    control: Option<FaultControl>,
    proxy_control: Option<ProxyControl>,
    callback: Option<IMFSinkWriterCallback>,
    writer: Option<Mp4Writer>,
    state: Arc<CallbackState>,
    calls: Arc<AtomicU32>,
    constructor_returned: bool,
    finalize_started: bool,
    completion_confirmed: bool,
    force_retention: bool,
}

impl CaseOwner {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            runtime: None,
            stream: None,
            raw_stream: None,
            control: None,
            proxy_control: None,
            callback: None,
            writer: None,
            state: Arc::new(CallbackState::default()),
            calls: Arc::new(AtomicU32::new(0)),
            constructor_returned: false,
            finalize_started: false,
            completion_confirmed: false,
            force_retention: false,
        }
    }

    fn retains_resources(&self) -> bool {
        !self.constructor_returned || !self.completion_confirmed || self.force_retention
    }
}

impl Drop for CaseOwner {
    fn drop(&mut self) {
        log(self.label, "owner release", "before");
        if self.retains_resources() || std::thread::panicking() {
            // Test-only safety retention until process exit; never transfer Runtime to a callback.
            retain(&mut self.writer);
            retain(&mut self.stream);
            retain(&mut self.raw_stream);
            retain(&mut self.control);
            retain(&mut self.proxy_control);
            retain(&mut self.callback);
            retain(&mut self.runtime);
            std::mem::forget(self.state.clone());
            log(
                self.label,
                "owner release",
                "returned; retained until process exit",
            );
        } else {
            // The test constructor has already disabled Mp4Writer's Drop Finalize.
            drop(self.writer.take());
            drop(self.stream.take());
            drop(self.raw_stream.take());
            drop(self.control.take());
            drop(self.proxy_control.take());
            drop(self.callback.take());
            drop(self.runtime.take());
            log(self.label, "owner release", "returned");
        }
    }
}

fn retain<T>(value: &mut Option<T>) {
    if let Some(value) = value.take() {
        std::mem::forget(value);
    }
}

fn sample_proxy(
    control: Option<&ProxyControl>,
    stage: &str,
    errors: &mut Vec<Value>,
) -> Option<ProxySnapshot> {
    let Some(control) = control else {
        errors.push(json!({"stage":stage,"error":"Missing error-status proxy control"}));
        return None;
    };
    match catch_unwind(AssertUnwindSafe(|| control.snapshot())) {
        Ok(snapshot) => Some(snapshot),
        Err(_) => {
            errors.push(json!({"stage":stage,"error":"panic contained while sampling proxy"}));
            None
        }
    }
}

fn proxy_snapshot_trusted(snapshot: &ProxySnapshot) -> bool {
    !snapshot.mutex_poisoned
        && snapshot.caught_panics == 0
        && !snapshot.observations_truncated
        && !snapshot.counters_saturated
        && snapshot.end_write_calls == snapshot.end_write_returns
        && u64::try_from(snapshot.end_observations.len()).ok() == Some(snapshot.end_write_calls)
        && snapshot.end_observations.iter().all(|observed| {
            observed.proxy_returned
                && observed.status_read_called
                && observed.status_read_returned
                && observed.status_hresult.is_some()
                && observed.inner_resolve_succeeded == Some(true)
                && observed.inner_end_called
                && observed.inner_end_returned
                && observed.inner_end_hresult.is_some()
                && observed.inner_end_succeeded.is_some()
                && !observed.caught_panic
        })
}

fn proxy_case_gate(snapshot: Option<&ProxySnapshot>, inject: bool, status_proxy: bool) -> bool {
    if !status_proxy {
        return true;
    }
    let Some(snapshot) = snapshot.filter(|snapshot| proxy_snapshot_trusted(snapshot)) else {
        return false;
    };
    if !inject {
        // A compatible baseline may use synchronous I/O and make no EndWrite call.
        return snapshot.status_error_after_inner_success_count == 0
            && snapshot.inner_end_error_count == 0
            && snapshot.first_forwarded_error_hresult.is_none();
    }
    let storage_error = STG_E_MEDIUMFULL.0 as u32;
    snapshot.status_error_after_inner_success_count > 0
        && snapshot.first_forwarded_error_hresult == Some(storage_error)
        && snapshot.end_observations.iter().any(|observed| {
            observed.inner_end_returned
                && observed.inner_end_succeeded == Some(true)
                && observed.status_hresult == Some(storage_error)
                && observed.proxy_returned
                && matches!(
                    observed.forwarded_error_source.as_ref(),
                    Some(ErrorSource::StatusAfterInnerSuccess)
                )
                && observed.forwarded_error_hresult == Some(storage_error)
                && observed.proxy_bytes_written.is_none()
        })
}

fn run_case(
    parent: &Path,
    inject: bool,
    status_proxy: bool,
    chart: &ColorChart,
    started: Instant,
) -> Value {
    let label = if inject { "injected" } else { "baseline" };
    let directory = parent.join(label);
    let path = directory.join("clip.mp4");
    let mut owner = CaseOwner::new(label);
    let mut stage = "create_directory";
    let mut accepted_writes = 0_u32;
    let mut accepted_before_arm = None;
    let mut uses_gpu_input = None;
    let mut arm_limit = None;
    let mut writer_snapshot = Value::Null;
    let mut write_error = Value::Null;
    let mut begin_returned_ok = None;
    let mut begin_error = Value::Null;
    let mut wait_timed_out = None;
    let mut callback_snapshot = None;
    let mut adapter_snapshot = None;
    let mut errors = Vec::new();
    let body = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        fs::create_dir(&directory)?;
        checkpoint(started)?;
        stage = "initialize_mf";
        owner.runtime = Some(MfRuntime::new()?);
        checkpoint(started)?;
        stage = "create_byte_stream";
        let (stream, control) = recording_fault_stream::create(&path)?;
        owner.control = Some(control);
        if status_proxy {
            owner.raw_stream = Some(stream);
            stage = "wrap_error_status_proxy";
            let (stream, control) =
                error_proxy::wrap(owner.raw_stream.as_ref().ok_or("Missing raw byte stream")?)?;
            owner.stream = Some(stream);
            owner.proxy_control = Some(control);
        } else {
            owner.stream = Some(stream);
        }
        owner.callback = Some(
            FinalizeCallback {
                state: owner.state.clone(),
            }
            .into(),
        );
        checkpoint(started)?;
        stage = "create_async_writer";
        log(label, "create async writer", "before");
        let writer = Mp4Writer::new_with_test_async_byte_stream(
            &path,
            WIDTH,
            HEIGHT,
            FPS,
            BITRATE,
            owner.stream.as_ref().ok_or("Missing byte stream")?,
            owner.calls.clone(),
            owner.callback.as_ref().ok_or("Missing sink callback")?,
        )?;
        // Constructor contract: finished=true already; a panic cannot re-enter Finalize via Drop.
        owner.writer = Some(writer);
        owner.constructor_returned = true;
        log(label, "create async writer", "returned");
        let writer = owner.writer.as_ref().ok_or("Missing returned writer")?;
        uses_gpu_input = Some(writer.uses_gpu_input());
        writer_snapshot = writer.diagnostics().clone();
        if uses_gpu_input != Some(false) {
            return Err("The fixture did not retain RGB32 input".into());
        }
        checkpoint(started)?;
        for index in 0..FRAMES {
            stage = "write_video";
            checkpoint(started)?;
            log(label, &format!("WriteSample {index}"), "before");
            let written = owner.writer.as_mut().ok_or("Missing writer")?.write_video(
                chart.bgra(),
                WIDTH,
                frame_time(index),
                frame_time(index + 1) - frame_time(index),
            );
            log(label, &format!("WriteSample {index}"), "returned");
            match written {
                Ok(()) => accepted_writes += 1,
                Err(error) => write_error = error_value(error.as_ref()),
            }
            checkpoint(started)?;
            if !write_error.is_null() {
                break;
            }
            if inject && index == 0 {
                stage = "arm_after_first_accepted_write";
                accepted_before_arm = Some(accepted_writes);
                log(label, "atomic arm after first accepted write", "before");
                arm_limit = Some(
                    owner
                        .control
                        .as_ref()
                        .ok_or("Missing fault control")?
                        .arm_after_current_len(1)?,
                );
                log(label, "atomic arm after first accepted write", "returned");
                checkpoint(started)?;
            }
        }
        Ok(())
    }));
    match body {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            owner.force_retention = true;
            errors.push(json!({"stage":stage,"error":error_value(error.as_ref())}));
        }
        Err(_) => {
            owner.force_retention = true;
            errors.push(json!({"stage":stage,"error":"panic contained during setup/write"}));
        }
    }

    // Even a body error must attempt exactly one Finalize on every returned writer.
    // This calls the parent's counted native entry, never finish()/finish_with_diagnostics().
    if owner.writer.is_some() {
        let finalize = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            if owner.finalize_started {
                return Err("Finalize was already requested".into());
            }
            owner.finalize_started = true;
            {
                owner.state.lock().finalize_requested = true;
            }
            log(label, "native Finalize request", "before");
            let result = owner
                .writer
                .as_ref()
                .ok_or("Missing writer")?
                .finalize_writer();
            log(label, "native Finalize request", "returned");
            begin_returned_ok = Some(result.is_ok());
            if let Err(error) = result {
                begin_error = error_value(&error);
            }
            if let Err(error) = checkpoint(started) {
                owner.force_retention = true;
                errors.push(
                    json!({"stage":"after_finalize_request","error":error_value(error.as_ref())}),
                );
            }
            // A direct Finalize error does not substitute for OnFinalize completion.
            log(label, "OnFinalize wait", "before");
            let (observed, timed_out) = owner.state.wait();
            log(label, "OnFinalize wait", "returned");
            wait_timed_out = Some(timed_out);
            owner.completion_confirmed = observed.finalize_requested
                && observed.on_finalize_count == 1
                && observed.on_finalize_hresult.is_some()
                && observed.callback_failure.is_none()
                && !observed.mutex_poisoned;
            callback_snapshot = Some(observed);
            checkpoint(started)
        }));
        match finalize {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                owner.force_retention = true;
                errors.push(
                    json!({"stage":"finalize_observation","error":error_value(error.as_ref())}),
                );
            }
            Err(_) => {
                owner.force_retention = true;
                errors.push(json!({"stage":"finalize_observation","error":"panic contained during Finalize/wait"}));
            }
        }
    }
    if callback_snapshot.is_none() {
        callback_snapshot = Some(owner.state.lock().clone());
    }
    if let Some(control) = owner.control.as_ref() {
        log(label, "final adapter snapshot", "before");
        let sampled = control.snapshot();
        log(label, "final adapter snapshot", "returned");
        match sampled {
            Ok(snapshot) => adapter_snapshot = Some(snapshot),
            Err(error) => {
                owner.force_retention = true;
                errors.push(
                    json!({"stage":"final_adapter_snapshot","error":error_value(error.as_ref())}),
                );
            }
        }
    }
    if let Some(writer) = owner.writer.as_ref() {
        writer_snapshot = writer.diagnostics().clone();
    }
    let file_bytes = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata.len()),
        Err(error) => {
            errors.push(json!({"stage":"artifact_metadata","error":error_value(&error)}));
            None
        }
    };
    if let Err(error) = checkpoint(started) {
        owner.force_retention = true;
        errors.push(json!({"stage":"after_observations","error":error_value(error.as_ref())}));
    }
    // The cloned control owns only synchronized diagnostic data, never COM resources.
    let proxy_control = owner.proxy_control.clone();
    let proxy_snapshot_before_release = if status_proxy {
        sample_proxy(
            proxy_control.as_ref(),
            "proxy_snapshot_before_owner_release",
            &mut errors,
        )
    } else {
        None
    };
    let proxy_trusted_before_release = status_proxy.then(|| {
        proxy_snapshot_before_release
            .as_ref()
            .is_some_and(proxy_snapshot_trusted)
    });
    if proxy_trusted_before_release == Some(false) {
        owner.force_retention = true;
        errors.push(json!({"stage":"proxy_snapshot_before_owner_release",
            "error":"Proxy observations are missing, incomplete or untrusted; owner retained"}));
    }
    let proxy_gate_before_release =
        proxy_case_gate(proxy_snapshot_before_release.as_ref(), inject, status_proxy);
    let calls = owner.calls.clone();
    let native_finalize_calls_before_release = owner
        .constructor_returned
        .then(|| calls.load(Ordering::Relaxed));
    let callback_gate = owner.completion_confirmed
        && callback_snapshot.as_ref().is_some_and(|value| {
            value.on_finalize_count == 1
                && value.on_finalize_hresult
                    == Some(if inject {
                        STG_E_MEDIUMFULL.0 as u32
                    } else {
                        S_OK.0 as u32
                    })
        });
    let adapter_gate = adapter_snapshot.as_ref().is_some_and(|stats| {
        stats.actual_len <= MAX_EXTENT
            && stats.high_water <= MAX_EXTENT
            && stats.safety_cap == MAX_EXTENT
            && Some(stats.actual_len) == file_bytes
            && if inject {
                stats.armed
                    && stats.arm_calls == 1
                    && stats.injected_count > 0
                    && stats.cap_hits > 0
                    && Some(stats.limit) == arm_limit
            } else {
                !stats.armed
                    && stats.arm_calls == 0
                    && stats.injected_count == 0
                    && stats.cap_hits == 0
            }
    });
    let write_gate = if inject {
        accepted_before_arm == Some(1) && accepted_writes >= 1 && arm_limit.is_some()
    } else {
        accepted_writes == FRAMES && write_error.is_null() && begin_returned_ok == Some(true)
    };
    let extent_gate = file_bytes.is_some_and(|bytes| bytes <= MAX_EXTENT && (inject || bytes > 0));
    let resources_retained = owner.retains_resources();
    let ok = errors.is_empty()
        && owner.constructor_returned
        && uses_gpu_input == Some(false)
        && native_finalize_calls_before_release == Some(1)
        && callback_gate
        && adapter_gate
        && write_gate
        && extent_gate
        && proxy_gate_before_release
        && !resources_retained
        && wait_timed_out == Some(false);
    let constructor_returned = owner.constructor_returned;
    let finalize_requested = owner.finalize_started;
    // Explicitly dispose while this initiating thread is still alive; Drop chooses safe release/retention.
    drop(owner);
    let native_finalize_calls = constructor_returned.then(|| calls.load(Ordering::Relaxed));
    let proxy_snapshot_after_release = if status_proxy {
        sample_proxy(
            proxy_control.as_ref(),
            "proxy_snapshot_after_owner_release",
            &mut errors,
        )
    } else {
        None
    };
    let proxy_trusted_after_release = status_proxy.then(|| {
        proxy_snapshot_after_release
            .as_ref()
            .is_some_and(proxy_snapshot_trusted)
    });
    if proxy_trusted_after_release == Some(false) {
        errors.push(json!({"stage":"proxy_snapshot_after_owner_release",
            "error":"Post-disposal proxy observations are missing, incomplete or untrusted; this cannot retroactively retain released COM resources"}));
    }
    let proxy_gate_after_release =
        proxy_case_gate(proxy_snapshot_after_release.as_ref(), inject, status_proxy);
    let proxy_gate = proxy_gate_before_release && proxy_gate_after_release;
    if let Err(error) = checkpoint(started) {
        errors.push(json!({"stage":"after_owner_release","error":error_value(error.as_ref())}));
    }
    json!({"name":label,"ok":ok && errors.is_empty() && native_finalize_calls == Some(1) && proxy_gate,"constructor_returned":constructor_returned,
        "requested_input":"cpu_rgb32","uses_gpu_input":uses_gpu_input,"accepted_by_write_sample":accepted_writes,
        "accepted_before_arm":accepted_before_arm,"arm_limit_bytes":arm_limit,"write_error":write_error,
        "finalize_requested":finalize_requested,"finalize_call_returned_ok":begin_returned_ok,"finalize_call_error":begin_error,
        "on_finalize":callback_snapshot,"callback_wait_timed_out":wait_timed_out,"native_finalize_calls":native_finalize_calls,
        "native_finalize_calls_before_owner_release":native_finalize_calls_before_release,
        "callback_gate":callback_gate,"adapter_gate":adapter_gate,"write_gate":write_gate,"extent_gate":extent_gate,
        "status_proxy":status_proxy,"proxy_gate":proxy_gate,
        "proxy_gate_before_owner_release":proxy_gate_before_release,"proxy_gate_after_owner_release":proxy_gate_after_release,
        "proxy_snapshot_before_owner_release":proxy_snapshot_before_release,
        "proxy_snapshot_after_owner_release":proxy_snapshot_after_release,
        "proxy_trusted_before_owner_release":proxy_trusted_before_release,
        "proxy_trusted_after_owner_release":proxy_trusted_after_release,
        "adapter_snapshot":adapter_snapshot,"file_bytes":file_bytes,"writer_snapshot":writer_snapshot,
        "owner_resources_retained_until_process_exit":resources_retained,"errors":errors})
}

#[test]
#[ignore = "真实异步 SinkWriter Finalize 回调；不采桌面、不开摄像头、不播音、不开窗口"]
fn recording_async_finalize_native_reports_callback_hresult() -> Result<()> {
    run_diagnostic(false)
}

#[test]
#[ignore = "测试用异步状态错误代理；独立进程运行，不采桌面、不播音、不开窗口"]
fn recording_error_status_proxy_native_reports_callback_hresult() -> Result<()> {
    run_diagnostic(true)
}

fn run_diagnostic(status_proxy: bool) -> Result<()> {
    let directory = unique_directory(status_proxy)?;
    let started = Instant::now();
    let mut cases = Vec::with_capacity(2);
    let setup = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        let chart = ColorChart::new(RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: WIDTH,
            height: HEIGHT,
        })?)?;
        for inject in [false, true] {
            cases.push(run_case(&directory, inject, status_proxy, &chart, started));
        }
        Ok(())
    }));
    let setup_error = match setup {
        Ok(Ok(())) => Value::Null,
        Ok(Err(error)) => error_value(error.as_ref()),
        Err(_) => json!({"error":"panic contained during fixture setup or case reporting"}),
    };
    let ok = setup_error.is_null()
        && cases.len() == 2
        && cases.iter().all(|case| case["ok"].as_bool() == Some(true));
    let report = json!({"ok":ok,"cases":cases,"setup_error":setup_error,
        "mode":if status_proxy { "error_status_proxy" } else { "plain_system_byte_stream" },
        "status_proxy_enabled":status_proxy,
        "elapsed_ms":started.elapsed().as_millis(),"limits":{"cases":2,"frames_per_case":FRAMES,
            "size":[WIDTH,HEIGHT],"callback_wait_seconds":WAIT_BUDGET.as_secs(),"cooperative_run_seconds":RUN_BUDGET.as_secs(),
            "adapter_extent_bytes":MAX_EXTENT,"report_bytes":MAX_REPORT_BYTES},
        "scope":["Run this ignored diagnostic alone in its own test process. Process-lifetime retention below is limited to that test process, not a production recovery guarantee.",
            "Real test-only asynchronous SinkWriter with H264 output and RGB32 input; input pixels do not establish software encoder selection. The initialized actual MFT is in writer_snapshot.",
            "The immediate Finalize return is only request-return evidence. The baseline requires exactly one OnFinalize(S_OK); the injected case requires exactly one OnFinalize(STG_E_MEDIUMFULL) and an actual adapter injection.",
            "writer_snapshot is the original initialized/trial snapshot. This test never calls synchronous finish and never rewrites trial.finalize; asynchronous completion is reported separately.",
            "There is no Reader, decode, playback, file-readability or recovery assertion. A zero-byte injected artifact may be retained without claiming it is usable.",
            "Constructor failure, unexpected early error, panic or missing/invalid completion retains the entire initiating-thread owner until process exit. This safety retention is not native physical release or successful recovery.",
            "Callbacks contain only plain synchronized state. Caller-owned writer, stream, control and MF runtime remain on the initiating thread; their COM pointers are not moved between threads and no Send/Sync implementation is added.",
            "The native Finalize counter measures calls on a successfully returned test writer, including failed calls. A failed constructor's internal teardown is not covered by that counter.",
            "Budgets are checks between calls, not interruption of blocking native calls, locks or release. The 4 MiB adapter extent cap is not a whole-process memory or physical-disk quota.",
            "Only error_status_proxy mode intervenes in test-only error propagation: it samples GetStatus before invoking inner EndWrite; after inner EndWrite succeeds it may forward that previously sampled failing status. Inner EndWrite errors take precedence. It preserves separate inner, status and proxy-return observations; SDK Result conversions may normalize successful HRESULTs to S_OK.",
            "The error-status proxy forwards IMFByteStream methods only, without forwarding unknown QueryInterface requests. Its baseline establishes compatibility only for this observed test path; zero EndWrite calls are permitted in that baseline.",
            "Proxy snapshots are observations at their sampling instants, not a prohibition on later behavior. Missing or untrusted pre-release observations retain the owner; a new post-release failure is reported as failure and cannot retroactively retain COM objects already released. After-owner-release sampling follows the retention/release choice, so retained cases have not physically released their COM resources.",
            "An error-status proxy success cannot change any earlier plain-wrapper, synchronous or asynchronous failure into a pass, and does not establish production URL-based recording or physical disk-full behavior.",
            "All newly created artifacts are retained. Simulated storage failure does not establish physical disk exhaustion or behavior of the production recording worker."]});
    let report = write_report(&directory, report)?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "MF async Finalize report: {}",
        directory.join("report.json").display()
    );
    assert_eq!(
        report["ok"].as_bool(),
        Some(true),
        "Asynchronous Finalize evidence failed: {}",
        directory.join("report.json").display()
    );
    Ok(())
}

fn frame_time(index: u32) -> Duration {
    Duration::from_nanos(
        ((u64::from(index) * 10_000_000 + u64::from(FPS / 2)) / u64::from(FPS)) * 100,
    )
}

fn checkpoint(started: Instant) -> Result<()> {
    if started.elapsed() > RUN_BUDGET {
        return Err(
            "Asynchronous Finalize test exceeded the twenty-second cooperative budget".into(),
        );
    }
    Ok(())
}

fn log(label: &str, operation: &str, phase: &str) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "MF async Finalize [{label}] {operation}: {phase}"
    );
}

fn clip(value: &str) -> String {
    value.chars().take(256).collect()
}

fn error_value(error: &(dyn StdError + 'static)) -> Value {
    let mut classification =
        serde_json::to_value(classify_recording_error(error)).unwrap_or(Value::Null);
    if let Some(message) = classification.get_mut("message") {
        if let Some(text) = message.as_str() {
            *message = Value::String(clip(text));
        }
    }
    let mut chain = Vec::with_capacity(8);
    let mut next = Some(error);
    while let Some(error) = next {
        if chain.len() == 8 {
            break;
        }
        chain.push(json!({"message":clip(&error.to_string()),
            "hresult":error.downcast_ref::<Error>().map(|error|error.code().0 as u32)}));
        next = error.source();
    }
    json!({"message":clip(&error.to_string()),"production_classification":classification,"source_chain":chain})
}

fn unique_directory(status_proxy: bool) -> Result<PathBuf> {
    let parent = Path::new("target/recording-storage-fault");
    fs::create_dir_all(parent)?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let prefix = if status_proxy {
        "error-status-proxy"
    } else {
        "async-finalize"
    };
    for index in 0..16 {
        let path = parent.join(format!("{prefix}-{}-{time}-{index}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("Unable to create an exclusive asynchronous Finalize directory".into())
}

fn write_report(directory: &Path, report: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    let report = if bytes.len() > MAX_REPORT_BYTES {
        let limited =
            json!({"ok":false,"error":"report_exceeded_256kib","artifacts_retained":true});
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
