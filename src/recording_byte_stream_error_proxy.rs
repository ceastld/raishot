//! Test-only IMFByteStream forwarding with independently recorded EndWrite status lifting.
//! The generated outer ABI normalizes successful HRESULTs to S_OK; this is not a bit-exact COM proxy.
#![cfg(test)]

use serde::Serialize;
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::{Arc, Mutex, MutexGuard},
};
use windows::{
    core::{implement, AgileReference, Error, IUnknown, Interface, Ref, BOOL, HRESULT},
    Win32::{
        Foundation::E_UNEXPECTED,
        Media::MediaFoundation::{
            IMFAsyncCallback, IMFAsyncResult, IMFByteStream, IMFByteStream_Impl,
            MFBYTESTREAM_SEEK_ORIGIN,
        },
    },
};

const MAX_OBSERVATIONS: usize = 64;
const METHOD_NAMES: [&str; 15] = [
    "GetCapabilities",
    "GetLength",
    "SetLength",
    "GetCurrentPosition",
    "SetCurrentPosition",
    "IsEndOfStream",
    "Read",
    "BeginRead",
    "EndRead",
    "Write",
    "BeginWrite",
    "EndWrite",
    "Seek",
    "Flush",
    "Close",
];
const SCOPE: &str = "test-only IMFByteStream method forwarding, with EndWrite returning the actual GetStatus error only after the inner EndWrite succeeds; no input buffers, callbacks, state or async results are copied/replaced; no unknown QueryInterface forwarding, no automatic Close/Flush, no recording/recovery guarantee; the generated Result-based outer vtable normalizes successful HRESULTs to S_OK; optional interfaces and native baseline compatibility remain unverified";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ErrorSource {
    InnerEndWrite,
    StatusAfterInnerSuccess,
    InnerResolve,
    InvalidAsyncResult,
    DiagnosticMutexPoison,
    CaughtPanic,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct EndWriteObservation {
    pub(super) call_index: u64,
    pub(super) status_read_called: bool,
    pub(super) status_read_returned: bool,
    pub(super) status_hresult: Option<u32>,
    pub(super) inner_resolve_succeeded: Option<bool>,
    pub(super) inner_resolve_error_hresult: Option<u32>,
    pub(super) inner_end_called: bool,
    pub(super) inner_end_returned: bool,
    pub(super) inner_end_hresult: Option<u32>,
    pub(super) inner_end_succeeded: Option<bool>,
    /// Present only when the native inner EndWrite returned a successful HRESULT.
    pub(super) inner_bytes_written: Option<u32>,
    pub(super) proxy_returned: bool,
    pub(super) forwarded_error_source: Option<ErrorSource>,
    pub(super) forwarded_error_hresult: Option<u32>,
    /// An outer Err has no valid outcount, even if inner EndWrite succeeded with zero.
    pub(super) proxy_bytes_written: Option<u32>,
    pub(super) caught_panic: bool,
}

impl EndWriteObservation {
    fn new(call_index: u64) -> Self {
        Self {
            call_index,
            status_read_called: false,
            status_read_returned: false,
            status_hresult: None,
            inner_resolve_succeeded: None,
            inner_resolve_error_hresult: None,
            inner_end_called: false,
            inner_end_returned: false,
            inner_end_hresult: None,
            inner_end_succeeded: None,
            inner_bytes_written: None,
            proxy_returned: false,
            forwarded_error_source: None,
            forwarded_error_hresult: None,
            proxy_bytes_written: None,
            caught_panic: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProxySnapshot {
    pub(super) end_write_calls: u64,
    /// Proxy EndWrite reached its return; this alone does not prove inner EndWrite was called.
    pub(super) end_write_returns: u64,
    pub(super) inner_end_error_count: u64,
    pub(super) status_error_after_inner_success_count: u64,
    /// The first error actually returned by this proxy's EndWrite, including diagnostic failures.
    pub(super) first_forwarded_error_hresult: Option<u32>,
    pub(super) mutex_poisoned: bool,
    pub(super) caught_panics: u64,
    pub(super) observations_truncated: bool,
    pub(super) counters_saturated: bool,
    pub(super) method_calls: BTreeMap<&'static str, u64>,
    pub(super) end_observations: Vec<EndWriteObservation>,
    pub(super) observation_limit: usize,
    pub(super) scope: &'static str,
}

struct ProxyState {
    snapshot: Mutex<ProxySnapshot>,
}

#[derive(Clone)]
pub(super) struct ProxyControl {
    state: Arc<ProxyState>,
}

impl ProxyControl {
    pub(super) fn snapshot(&self) -> ProxySnapshot {
        self.state.lock().clone()
    }
}

#[derive(Clone, Copy)]
struct EndTicket {
    slot: Option<usize>,
}

impl ProxyState {
    fn lock(&self) -> MutexGuard<'_, ProxySnapshot> {
        match self.snapshot.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                let mut guard = poison.into_inner();
                guard.mutex_poisoned = true;
                guard
            }
        }
    }

    fn enter(&self, name: &'static str) -> windows::core::Result<()> {
        let mut stats = self.lock();
        method_call(&mut stats, name);
        if stats.mutex_poisoned {
            Err(Error::from_hresult(E_UNEXPECTED))
        } else {
            Ok(())
        }
    }

    fn begin_end(&self) -> (EndTicket, bool) {
        let mut stats = self.lock();
        method_call(&mut stats, "EndWrite");
        let saturated = increment(&mut stats.end_write_calls);
        stats.counters_saturated |= saturated;
        let slot = if stats.end_observations.len() < MAX_OBSERVATIONS {
            let index = stats.end_observations.len();
            let call_index = stats.end_write_calls;
            stats
                .end_observations
                .push(EndWriteObservation::new(call_index));
            Some(index)
        } else {
            stats.observations_truncated = true;
            None
        };
        (EndTicket { slot }, stats.mutex_poisoned)
    }

    fn update(&self, ticket: EndTicket, update: impl FnOnce(&mut EndWriteObservation)) {
        let mut stats = self.lock();
        if let Some(index) = ticket.slot {
            if let Some(observation) = stats.end_observations.get_mut(index) {
                update(observation);
            } else {
                stats.observations_truncated = true;
            }
        }
    }

    fn inner_result(&self, ticket: EndTicket, status: HRESULT, bytes: Option<u32>) {
        let mut stats = self.lock();
        if status.is_err() {
            let saturated = increment(&mut stats.inner_end_error_count);
            stats.counters_saturated |= saturated;
        }
        if let Some(index) = ticket.slot {
            if let Some(observation) = stats.end_observations.get_mut(index) {
                observation.inner_end_returned = true;
                observation.inner_end_hresult = Some(status.0 as u32);
                observation.inner_end_succeeded = Some(status.is_ok());
                observation.inner_bytes_written = bytes;
            }
        }
    }

    fn panic_observed(&self, ticket: Option<EndTicket>) {
        let mut stats = self.lock();
        let saturated = increment(&mut stats.caught_panics);
        stats.counters_saturated |= saturated;
        if let Some(index) = ticket.and_then(|ticket| ticket.slot) {
            if let Some(observation) = stats.end_observations.get_mut(index) {
                observation.caught_panic = true;
            }
        }
    }

    fn finish_end(
        &self,
        ticket: EndTicket,
        mut result: windows::core::Result<u32>,
        mut source: Option<ErrorSource>,
    ) -> windows::core::Result<u32> {
        let mut stats = self.lock();
        // Preserve an actual inner/status error in preference to a later diagnostic failure.
        if stats.mutex_poisoned && result.is_ok() {
            result = Err(Error::from_hresult(E_UNEXPECTED));
            source = Some(ErrorSource::DiagnosticMutexPoison);
        }
        let error = result.as_ref().err().map(|error| error.code().0 as u32);
        let bytes = result.as_ref().ok().copied();
        let saturated = increment(&mut stats.end_write_returns);
        stats.counters_saturated |= saturated;
        if error.is_some() && source == Some(ErrorSource::StatusAfterInnerSuccess) {
            let saturated = increment(&mut stats.status_error_after_inner_success_count);
            stats.counters_saturated |= saturated;
        }
        if let Some(error) = error {
            stats.first_forwarded_error_hresult.get_or_insert(error);
        }
        if let Some(index) = ticket.slot {
            if let Some(observation) = stats.end_observations.get_mut(index) {
                observation.proxy_returned = true;
                observation.forwarded_error_source = source;
                observation.forwarded_error_hresult = error;
                observation.proxy_bytes_written = bytes;
            }
        }
        result
    }
}

fn increment(value: &mut u64) -> bool {
    if *value == u64::MAX {
        true
    } else {
        *value += 1;
        false
    }
}

fn method_call(stats: &mut ProxySnapshot, name: &'static str) {
    if let Some(count) = stats.method_calls.get_mut(name) {
        let saturated = increment(count);
        stats.counters_saturated |= saturated;
    }
}

// Default windows-implement agility is backed by IAgileObject/IMarshal. No unknown QI forwarding.
#[implement(IMFByteStream)]
struct ByteStreamProxy {
    inner: AgileReference<IMFByteStream>,
    state: Arc<ProxyState>,
}

impl ByteStreamProxy {
    fn forward<T>(
        &self,
        method: &'static str,
        call: impl FnOnce(&IMFByteStream) -> windows::core::Result<T>,
    ) -> windows::core::Result<T> {
        match catch_unwind(AssertUnwindSafe(|| {
            self.state.enter(method)?;
            let inner = self.inner.resolve()?;
            call(&inner)
        })) {
            Ok(result) => result,
            Err(_) => {
                self.state.panic_observed(None);
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }

    fn end_write(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<u32> {
        let mut ticket = None;
        let mut source = None;
        let work = catch_unwind(AssertUnwindSafe(|| -> windows::core::Result<u32> {
            let (entered, poisoned) = self.state.begin_end();
            ticket = Some(entered);
            if poisoned {
                source = Some(ErrorSource::DiagnosticMutexPoison);
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
            let result = result.ok().map_err(|error| {
                source = Some(ErrorSource::InvalidAsyncResult);
                error
            })?;
            self.state
                .update(entered, |observation| observation.status_read_called = true);
            // Read the real operation status, including nonzero successful HRESULTs.
            let status =
                unsafe { (Interface::vtable(result).GetStatus)(Interface::as_raw(result)) };
            let status_result = status.ok();
            self.state.update(entered, |observation| {
                observation.status_read_returned = true;
                observation.status_hresult = Some(status.0 as u32);
            });
            // A status error does not skip the mandatory inner EndWrite completion call.
            let inner = self.inner.resolve().map_err(|error| {
                self.state.update(entered, |observation| {
                    observation.inner_resolve_succeeded = Some(false);
                    observation.inner_resolve_error_hresult = Some(error.code().0 as u32);
                });
                source = Some(ErrorSource::InnerResolve);
                error
            })?;
            self.state.update(entered, |observation| {
                observation.inner_resolve_succeeded = Some(true);
                observation.inner_end_called = true;
            });
            let mut bytes = 0_u32;
            let inner_status = unsafe {
                (Interface::vtable(&inner).EndWrite)(
                    Interface::as_raw(&inner),
                    Interface::as_raw(result),
                    &mut bytes,
                )
            };
            // Same HRESULT-to-Result interpretation as the SDK, while preserving raw HRESULT above.
            let inner_result = inner_status.map(|| bytes);
            let valid_bytes = inner_result.as_ref().ok().copied();
            self.state.inner_result(entered, inner_status, valid_bytes);
            match inner_result {
                Err(error) => {
                    source = Some(ErrorSource::InnerEndWrite);
                    Err(error)
                }
                Ok(bytes) => match status_result {
                    Err(error) => {
                        source = Some(ErrorSource::StatusAfterInnerSuccess);
                        Err(error)
                    }
                    Ok(()) => Ok(bytes),
                },
            }
        }));
        let result = match work {
            Ok(result) => result,
            Err(_) => {
                self.state.panic_observed(ticket);
                source = Some(ErrorSource::CaughtPanic);
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        };
        if let Some(ticket) = ticket {
            self.state.finish_end(ticket, result, source)
        } else {
            result
        }
    }
}

#[allow(non_snake_case)]
impl IMFByteStream_Impl for ByteStreamProxy_Impl {
    fn GetCapabilities(&self) -> windows::core::Result<u32> {
        self.forward("GetCapabilities", |inner| {
            let mut value = 0;
            unsafe {
                (Interface::vtable(inner).GetCapabilities)(Interface::as_raw(inner), &mut value)
            }
            .map(|| value)
        })
    }
    fn GetLength(&self) -> windows::core::Result<u64> {
        self.forward("GetLength", |inner| {
            let mut value = 0;
            unsafe { (Interface::vtable(inner).GetLength)(Interface::as_raw(inner), &mut value) }
                .map(|| value)
        })
    }
    fn SetLength(&self, length: u64) -> windows::core::Result<()> {
        self.forward("SetLength", |inner| {
            unsafe { (Interface::vtable(inner).SetLength)(Interface::as_raw(inner), length) }.ok()
        })
    }
    fn GetCurrentPosition(&self) -> windows::core::Result<u64> {
        self.forward("GetCurrentPosition", |inner| {
            let mut value = 0;
            unsafe {
                (Interface::vtable(inner).GetCurrentPosition)(Interface::as_raw(inner), &mut value)
            }
            .map(|| value)
        })
    }
    fn SetCurrentPosition(&self, position: u64) -> windows::core::Result<()> {
        self.forward("SetCurrentPosition", |inner| {
            unsafe {
                (Interface::vtable(inner).SetCurrentPosition)(Interface::as_raw(inner), position)
            }
            .ok()
        })
    }
    fn IsEndOfStream(&self) -> windows::core::Result<BOOL> {
        self.forward("IsEndOfStream", |inner| {
            let mut value = BOOL::default();
            unsafe {
                (Interface::vtable(inner).IsEndOfStream)(Interface::as_raw(inner), &mut value)
            }
            .map(|| value)
        })
    }
    fn Read(&self, buffer: *mut u8, count: u32, read: *mut u32) -> windows::core::Result<()> {
        self.forward("Read", |inner| {
            unsafe {
                (Interface::vtable(inner).Read)(Interface::as_raw(inner), buffer, count, read)
            }
            .ok()
        })
    }
    fn BeginRead(
        &self,
        buffer: *mut u8,
        count: u32,
        callback: Ref<'_, IMFAsyncCallback>,
        state: Ref<'_, IUnknown>,
    ) -> windows::core::Result<()> {
        self.forward("BeginRead", |inner| {
            unsafe {
                (Interface::vtable(inner).BeginRead)(
                    Interface::as_raw(inner),
                    buffer,
                    count,
                    callback.as_ref().map_or(ptr::null_mut(), Interface::as_raw),
                    state.as_ref().map_or(ptr::null_mut(), Interface::as_raw),
                )
            }
            .ok()
        })
    }
    fn EndRead(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<u32> {
        self.forward("EndRead", |inner| {
            let mut value = 0;
            unsafe {
                (Interface::vtable(inner).EndRead)(
                    Interface::as_raw(inner),
                    result.as_ref().map_or(ptr::null_mut(), Interface::as_raw),
                    &mut value,
                )
            }
            .map(|| value)
        })
    }
    fn Write(&self, buffer: *const u8, count: u32) -> windows::core::Result<u32> {
        self.forward("Write", |inner| {
            let mut value = 0;
            unsafe {
                (Interface::vtable(inner).Write)(
                    Interface::as_raw(inner),
                    buffer,
                    count,
                    &mut value,
                )
            }
            .map(|| value)
        })
    }
    fn BeginWrite(
        &self,
        buffer: *const u8,
        count: u32,
        callback: Ref<'_, IMFAsyncCallback>,
        state: Ref<'_, IUnknown>,
    ) -> windows::core::Result<()> {
        // No Rust slice is constructed; original pointers/length, callback and state reach the inner ABI.
        self.forward("BeginWrite", |inner| {
            unsafe {
                (Interface::vtable(inner).BeginWrite)(
                    Interface::as_raw(inner),
                    buffer,
                    count,
                    callback.as_ref().map_or(ptr::null_mut(), Interface::as_raw),
                    state.as_ref().map_or(ptr::null_mut(), Interface::as_raw),
                )
            }
            .ok()
        })
    }
    fn EndWrite(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<u32> {
        // Include observation completion in the FFI unwind barrier as well.
        match catch_unwind(AssertUnwindSafe(|| self.end_write(result))) {
            Ok(result) => result,
            Err(_) => {
                self.state.panic_observed(None);
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }
    fn Seek(
        &self,
        origin: MFBYTESTREAM_SEEK_ORIGIN,
        offset: i64,
        flags: u32,
    ) -> windows::core::Result<u64> {
        self.forward("Seek", |inner| {
            let mut value = 0;
            unsafe {
                (Interface::vtable(inner).Seek)(
                    Interface::as_raw(inner),
                    origin,
                    offset,
                    flags,
                    &mut value,
                )
            }
            .map(|| value)
        })
    }
    fn Flush(&self) -> windows::core::Result<()> {
        self.forward("Flush", |inner| {
            unsafe { (Interface::vtable(inner).Flush)(Interface::as_raw(inner)) }.ok()
        })
    }
    fn Close(&self) -> windows::core::Result<()> {
        self.forward("Close", |inner| {
            unsafe { (Interface::vtable(inner).Close)(Interface::as_raw(inner)) }.ok()
        })
    }
}

// No Drop implementation: releasing this wrapper does not independently Close or Flush the stream.
pub(super) fn wrap(inner: &IMFByteStream) -> crate::Result<(IMFByteStream, ProxyControl)> {
    let inner = AgileReference::new(inner)?;
    let mut end_observations = Vec::new();
    end_observations.try_reserve_exact(MAX_OBSERVATIONS)?;
    let state = Arc::new(ProxyState {
        snapshot: Mutex::new(ProxySnapshot {
            end_write_calls: 0,
            end_write_returns: 0,
            inner_end_error_count: 0,
            status_error_after_inner_success_count: 0,
            first_forwarded_error_hresult: None,
            mutex_poisoned: false,
            caught_panics: 0,
            observations_truncated: false,
            counters_saturated: false,
            method_calls: METHOD_NAMES.into_iter().map(|name| (name, 0)).collect(),
            end_observations,
            observation_limit: MAX_OBSERVATIONS,
            scope: SCOPE,
        }),
    });
    let stream: IMFByteStream = ByteStreamProxy {
        inner,
        state: state.clone(),
    }
    .into();
    Ok((stream, ProxyControl { state }))
}

#[allow(dead_code)]
fn thread_safety_contract() {
    fn check<T: Send + Sync>() {}
    check::<ProxyState>();
    check::<ProxyControl>();
    check::<ByteStreamProxy>();
}
