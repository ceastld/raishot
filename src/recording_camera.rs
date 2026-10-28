//! 显式选择设备的内部摄像头采集；默认录屏不创建此会话。
//! 所有设备/Reader COM 均留在专属线程；回调仅复制一帧数据，不向 UI 搬运 COM。
#[path = "recording_camera_devices.rs"]
pub(crate) mod devices;
#[path = "recording_camera_frame.rs"]
pub(crate) mod frame;
#[path = "recording_camera_probe.rs"]
pub(crate) mod probe;
#[path = "recording_camera_session.rs"]
pub(crate) mod session;

use crate::media_foundation::MfRuntime;
use devices::CameraDeviceId;
use frame::{CameraFrame, CameraLayout};
use serde::Serialize;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Ref, HRESULT, HSTRING},
    Win32::Media::MediaFoundation::*,
};

const WAIT_SLICE: Duration = Duration::from_millis(25);
const FLUSH_WAIT: Duration = Duration::from_millis(250);
const STALL_LIMIT: Duration = Duration::from_secs(5);
const FORMAT_NOTICE_LIMIT: u32 = 16;
const EMPTY_READ_LIMIT: u32 = 64;

#[derive(Clone, Debug)]
pub(crate) struct CameraRequest {
    pub device: CameraDeviceId,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub first_frame_budget: Duration,
    /// 内部试验的活动上限；不是对一次驱动调用的硬超时。
    pub run_budget: Duration,
}
impl CameraRequest {
    pub(crate) fn new(device: CameraDeviceId) -> Self {
        Self {
            device,
            max_width: 640,
            max_height: 480,
            max_fps: 30,
            first_frame_budget: Duration::from_secs(5),
            run_budget: Duration::from_secs(15),
        }
    }
    fn validate(&self) -> crate::Result<()> {
        if self.max_width < 2
            || self.max_height < 2
            || self.max_width > 4096
            || self.max_height > 4096
            || u64::from(self.max_width) * u64::from(self.max_height) > 4_194_304
            || !(1..=60).contains(&self.max_fps)
            || self.first_frame_budget.is_zero()
            || self.first_frame_budget > Duration::from_secs(10)
            || self.run_budget < self.first_frame_budget
            || self.run_budget > Duration::from_secs(60)
        {
            return Err(tr!(
                "摄像头内部试验请求超出边界",
                "The internal camera request exceeds its limits"
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CameraIssue {
    pub stage: &'static str,
    pub hresult: Option<i32>,
    pub message: String,
}
impl CameraIssue {
    fn message(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            hresult: None,
            message: message.into(),
        }
    }
    fn native(stage: &'static str, error: windows::core::Error) -> Self {
        Self {
            stage,
            hresult: Some(error.code().0),
            message: error.to_string(),
        }
    }
}
impl std::fmt::Display for CameraIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.message)
    }
}
impl std::error::Error for CameraIssue {}
type CaptureResult<T> = std::result::Result<T, CameraIssue>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CameraPhase {
    Starting,
    Ready,
    Closing,
    Closed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CameraFormat {
    pub stream: u32,
    pub width: u32,
    pub height: u32,
    pub fps_numerator: u32,
    pub fps_denominator: u32,
    pub requested_native_subtype: String,
    pub converted_subtype: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CameraStatus {
    pub phase: CameraPhase,
    pub negotiated: Option<CameraFormat>,
    pub frames_received: u64,
    pub frames_replaced: u64,
    pub format_notifications: u32,
    pub layout_reconfigurations: u32,
    pub source_timestamp_100ns: Option<i64>,
    pub failure: Option<CameraIssue>,
    pub cleanup_issues: Vec<CameraIssue>,
    pub delivery_closed: bool,
    pub worker_finished: bool,
    pub scope: &'static str,
}
impl Default for CameraStatus {
    fn default() -> Self {
        Self { phase: CameraPhase::Starting, negotiated: None, frames_received: 0,
            frames_replaced: 0, format_notifications: 0, layout_reconfigurations: 0, source_timestamp_100ns: None,
            failure: None, cleanup_issues: Vec::new(), delivery_closed: false, worker_finished: false,
            scope: "internal explicit-device RGB32 capture only; one outstanding async read and one latest frame; source PTS is not recording active time; software conversion is not a performance claim; budgets cannot interrupt native activation, negotiation, locks or cleanup; worker completion does not prove physical device state after a failed shutdown" }
    }
}

#[derive(Debug)]
pub(crate) struct CameraSample {
    pub frame: CameraFrame,
    pub source_timestamp_100ns: i64,
    pub received: Instant,
    pub sequence: u64,
}
struct Inbox {
    status: CameraStatus,
    latest: Option<CameraSample>,
    layout: Option<CameraLayout>,
    pending: bool,
    format_notice: bool,
    empty_reads: u32,
    reader_failed: bool,
    flushed: bool,
    last_frame_at: Option<Instant>,
}
impl Default for Inbox {
    fn default() -> Self {
        Self {
            status: CameraStatus::default(),
            latest: None,
            layout: None,
            pending: false,
            format_notice: false,
            empty_reads: 0,
            reader_failed: false,
            flushed: false,
            last_frame_at: None,
        }
    }
}
#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    changed: Condvar,
    stop: AtomicBool,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inbox> {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn fail(&self, issue: CameraIssue, reader_failed: bool) {
        let mut inbox = self.lock();
        if inbox.status.failure.is_none() {
            inbox.status.failure = Some(issue);
        }
        inbox.reader_failed |= reader_failed;
        inbox.status.phase = CameraPhase::Failed;
        inbox.status.delivery_closed = true;
        inbox.latest = None;
        self.stop.store(true, Ordering::Release);
        self.changed.notify_all();
    }
    fn fail_callback(&self, issue: CameraIssue, reader_failed: bool) {
        let mut inbox = self.lock();
        inbox.reader_failed |= reader_failed;
        if inbox.status.delivery_closed {
            return;
        }
        if inbox.status.failure.is_none() {
            inbox.status.failure = Some(issue);
        }
        inbox.status.phase = CameraPhase::Failed;
        inbox.status.delivery_closed = true;
        inbox.latest = None;
        self.stop.store(true, Ordering::Release);
        self.changed.notify_all();
    }
    fn publish(&self, frame: CameraFrame, timestamp: i64) {
        let mut inbox = self.lock();
        inbox.pending = false;
        if inbox.status.delivery_closed {
            self.changed.notify_all();
            return;
        }
        let Some(sequence) = inbox.status.frames_received.checked_add(1) else {
            drop(inbox);
            self.fail_callback(
                CameraIssue::message("frame_counter", "Camera frame counter overflowed"),
                false,
            );
            return;
        };
        if inbox.latest.is_some() {
            inbox.status.frames_replaced += 1;
        }
        let now = Instant::now();
        inbox.latest = Some(CameraSample {
            frame,
            source_timestamp_100ns: timestamp,
            received: now,
            sequence,
        });
        inbox.status.frames_received = sequence;
        inbox.status.source_timestamp_100ns = Some(timestamp);
        inbox.status.phase = CameraPhase::Ready;
        inbox.last_frame_at = Some(now);
        inbox.empty_reads = 0;
        self.changed.notify_all();
    }
    fn close_delivery(&self) {
        let mut inbox = self.lock();
        inbox.status.delivery_closed = true;
        inbox.latest = None;
        if inbox.status.failure.is_none() && !inbox.status.worker_finished {
            inbox.status.phase = CameraPhase::Closing;
        }
        self.stop.store(true, Ordering::Release);
        self.changed.notify_all();
    }
}

/// 构造只启动显式请求的内部工作线程；Ready 必须等到真实像素帧到达。
pub(crate) struct CameraCapture {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
impl CameraCapture {
    pub(crate) fn start(request: CameraRequest) -> crate::Result<Self> {
        request.validate()?;
        let shared = Arc::new(Shared::default());
        let owned = shared.clone();
        let worker = std::thread::Builder::new()
            .name("rustshot-camera".into())
            .spawn(move || {
                match catch_unwind(AssertUnwindSafe(|| capture_worker(&request, &owned))) {
                    Ok(Err(issue)) => owned.fail(issue, false),
                    Err(_) => owned.fail(
                        CameraIssue::message("worker_panic", "Camera worker panicked"),
                        true,
                    ),
                    Ok(Ok(())) => {}
                }
                let mut inbox = owned.lock();
                inbox.latest = None;
                inbox.status.delivery_closed = true;
                inbox.status.worker_finished = true;
                inbox.status.phase = if inbox.status.failure.is_some() {
                    CameraPhase::Failed
                } else {
                    CameraPhase::Closed
                };
                owned.changed.notify_all();
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }
    pub(crate) fn status(&self) -> CameraStatus {
        self.shared.lock().status.clone()
    }
    pub(crate) fn take_latest(&self) -> Option<CameraSample> {
        let mut inbox = self.shared.lock();
        if inbox.status.delivery_closed {
            None
        } else {
            inbox.latest.take()
        }
    }
    pub(crate) fn stop(&self) {
        self.shared.close_delivery();
    }
    /// 等待的是工作线程回执；超时后仍报告 Closing/Failed，不假报设备已关闭。
    pub(crate) fn stop_and_wait(mut self, budget: Duration) -> CameraStatus {
        self.stop();
        let started = Instant::now();
        let mut inbox = self.shared.lock();
        while !inbox.status.worker_finished && started.elapsed() < budget {
            let remaining = budget.saturating_sub(started.elapsed()).min(WAIT_SLICE);
            inbox = self
                .shared
                .changed
                .wait_timeout(inbox, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let status = inbox.status.clone();
        drop(inbox);
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        status
    }
}
impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
        // 不在 UI/析构路径等待可能卡住的驱动；owner 仍持有清理所需的全部 COM。
        drop(self.worker.take());
    }
}

#[implement(IMFSourceReaderCallback)]
struct CameraCallback {
    shared: Arc<Shared>,
}
impl CameraCallback {
    fn protect(&self, callback: impl FnOnce()) {
        if catch_unwind(AssertUnwindSafe(callback)).is_err() {
            self.shared.fail_callback(
                CameraIssue::message("callback_panic", "Camera callback panicked"),
                true,
            );
        }
    }
    fn read(
        &self,
        status: HRESULT,
        stream: u32,
        flags: u32,
        timestamp: i64,
        sample: Option<&IMFSample>,
    ) {
        {
            let mut inbox = self.shared.lock();
            if inbox.status.delivery_closed {
                inbox.reader_failed |= flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0;
                inbox.pending = false;
                self.shared.changed.notify_all();
                return;
            }
        }
        if status.is_err() || flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            self.shared.fail_callback(
                CameraIssue {
                    stage: "read_callback",
                    hresult: Some(status.0),
                    message: format!(
                        "Camera read failed: HRESULT=0x{:08X}, flags=0x{flags:08X}",
                        status.0 as u32
                    ),
                },
                true,
            );
            return;
        }
        let layout = {
            let mut inbox = self.shared.lock();
            if inbox.status.delivery_closed {
                inbox.pending = false;
                self.shared.changed.notify_all();
                return;
            }
            if inbox
                .status
                .negotiated
                .as_ref()
                .is_none_or(|format| format.stream != stream)
            {
                drop(inbox);
                self.shared.fail_callback(
                    CameraIssue::message("read_stream", "Unexpected camera stream"),
                    true,
                );
                return;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                drop(inbox);
                self.shared.fail_callback(
                    CameraIssue::message("device_ended", "Camera stream ended"),
                    false,
                );
                return;
            }
            let changes = MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
                | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32
                | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32
                | MF_SOURCE_READERF_NEWSTREAM.0 as u32;
            if flags & changes != 0 {
                // 此 sample 不读像素；owner 核对当前格式后才发下一次请求。
                inbox.pending = false;
                inbox.format_notice = true;
                inbox.latest = None;
                inbox.status.phase = CameraPhase::Starting;
                inbox.status.format_notifications += 1;
                self.shared.changed.notify_all();
                return;
            }
            if sample.is_none() {
                inbox.pending = false;
                inbox.empty_reads += 1;
                self.shared.changed.notify_all();
                return;
            }
            inbox.layout.clone()
        };
        let Some(layout) = layout else {
            self.shared.fail_callback(
                CameraIssue::message("pixel_layout", "Camera layout was not configured"),
                true,
            );
            return;
        };
        let Some(sample) = sample else {
            return;
        };
        let frame = match layout.read_sample(sample, flags) {
            Ok(frame) => frame,
            Err(error) => {
                self.shared
                    .fail_callback(CameraIssue::message("copy_frame", error.to_string()), false);
                return;
            }
        };
        self.shared.publish(frame, timestamp);
    }
}
#[allow(non_snake_case)]
impl IMFSourceReaderCallback_Impl for CameraCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        stream: u32,
        flags: u32,
        timestamp: i64,
        sample: Ref<'_, IMFSample>,
    ) -> windows::core::Result<()> {
        self.protect(|| self.read(status, stream, flags, timestamp, sample.as_ref()));
        Ok(())
    }
    fn OnFlush(&self, _: u32) -> windows::core::Result<()> {
        self.protect(|| {
            let mut inbox = self.shared.lock();
            inbox.flushed = true;
            inbox.pending = false;
            self.shared.changed.notify_all();
        });
        Ok(())
    }
    fn OnEvent(&self, _: u32, event: Ref<'_, IMFMediaEvent>) -> windows::core::Result<()> {
        self.protect(|| {
            if self.shared.lock().status.delivery_closed {
                return;
            }
            if let Some(event) = event.as_ref() {
                let status = unsafe { event.GetStatus() };
                match status {
                    Ok(status) if status.is_err() => self.shared.fail_callback(
                        CameraIssue::native(
                            "device_event",
                            windows::core::Error::from_hresult(status),
                        ),
                        true,
                    ),
                    Err(error) => self
                        .shared
                        .fail_callback(CameraIssue::native("device_event_status", error), true),
                    _ => {
                        if unsafe { event.GetType() }.is_ok_and(|kind| {
                            kind == MEVideoCaptureDeviceRemoved.0 as u32 || kind == MEError.0 as u32
                        }) {
                            self.shared.fail_callback(
                                CameraIssue::message(
                                    "device_event",
                                    "Camera device was removed or reported an error",
                                ),
                                true,
                            );
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

struct SourceSession {
    source: IMFMediaSource,
    reader: Option<IMFSourceReader>,
    shared: Arc<Shared>,
    issued_read: bool,
    shutdown_done: bool,
}
impl SourceSession {
    fn close(&mut self) -> Vec<CameraIssue> {
        self.shared.close_delivery();
        let mut issues = Vec::new();
        let reader_failed = self.shared.lock().reader_failed;
        // MF_SOURCE_READERF_ERROR 后不再调用 Reader 方法；释放接口仍是必要的清理。
        if self.issued_read && !reader_failed {
            if let Some(reader) = &self.reader {
                self.shared.lock().flushed = false;
                let started = Instant::now();
                match unsafe { reader.Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32) } {
                    Err(error) => issues.push(CameraIssue::native("flush", error)),
                    Ok(()) => {
                        let mut inbox = self.shared.lock();
                        while !inbox.flushed && started.elapsed() < FLUSH_WAIT {
                            let remaining =
                                FLUSH_WAIT.saturating_sub(started.elapsed()).min(WAIT_SLICE);
                            inbox = self
                                .shared
                                .changed
                                .wait_timeout(inbox, remaining)
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .0;
                        }
                        if !inbox.flushed {
                            issues.push(CameraIssue::message(
                                "flush_wait",
                                "OnFlush was not observed within the between-call wait budget",
                            ));
                        }
                    }
                }
            }
        }
        drop(self.reader.take());
        self.shutdown_done = true;
        if let Err(error) = unsafe { self.source.Shutdown() } {
            issues.push(CameraIssue::native("source_shutdown", error));
        }
        issues
    }
}
impl Drop for SourceSession {
    fn drop(&mut self) {
        self.shared.close_delivery();
        drop(self.reader.take());
        if !self.shutdown_done {
            self.shutdown_done = true;
            let _ = unsafe { self.source.Shutdown() };
        }
    }
}

struct SelectedFormat {
    native: IMFMediaType,
    format: CameraFormat,
}

fn select_native(
    reader: &IMFSourceReader,
    request: &CameraRequest,
) -> CaptureResult<SelectedFormat> {
    let mut selected: Option<SelectedFormat> = None;
    'streams: for stream in 0..8_u32 {
        for index in 0..64_u32 {
            let media = match unsafe { reader.GetNativeMediaType(stream, index) } {
                Ok(media) => media,
                Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => break 'streams,
                Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
                Err(error) => return Err(CameraIssue::native("native_types", error)),
            };
            if unsafe { media.GetGUID(&MF_MT_MAJOR_TYPE) }.ok() != Some(MFMediaType_Video) {
                continue;
            }
            let Some(subtype) = (unsafe { media.GetGUID(&MF_MT_SUBTYPE) }).ok() else {
                continue;
            };
            let (Some(size), Some(rate)) = (
                (unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }).ok(),
                (unsafe { media.GetUINT64(&MF_MT_FRAME_RATE) }).ok(),
            ) else {
                continue;
            };
            let width = (size >> 32) as u32;
            let height = size as u32;
            let fps_numerator = (rate >> 32) as u32;
            let fps_denominator = rate as u32;
            if width < 2
                || height < 2
                || width > request.max_width
                || height > request.max_height
                || fps_numerator == 0
                || fps_denominator == 0
                || u64::from(fps_numerator)
                    > u64::from(request.max_fps) * u64::from(fps_denominator)
                || (unsafe { media.GetUINT32(&MF_MT_INTERLACE_MODE) }).ok()
                    != Some(MFVideoInterlace_Progressive.0 as u32)
                || (unsafe { media.GetUINT32(&MF_MT_VIDEO_ROTATION) })
                    .ok()
                    .is_some_and(|rotation| rotation != 0)
            {
                continue;
            }
            if let Ok(par) = unsafe { media.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO) } {
                if par as u32 == 0 || (par >> 32) as u32 != par as u32 {
                    continue;
                }
            }
            let format = CameraFormat {
                stream,
                width,
                height,
                fps_numerator,
                fps_denominator,
                requested_native_subtype: format!("{subtype:?}"),
                converted_subtype: "rgb32",
            };
            let preferred = selected.as_ref().is_none_or(|old| {
                let area = u64::from(width) * u64::from(height);
                let old_area = u64::from(old.format.width) * u64::from(old.format.height);
                area > old_area
                    || (area == old_area
                        && u64::from(fps_numerator) * u64::from(old.format.fps_denominator)
                            > u64::from(old.format.fps_numerator) * u64::from(fps_denominator))
            });
            if preferred {
                selected = Some(SelectedFormat {
                    native: media,
                    format,
                });
            }
        }
    }
    selected.ok_or_else(|| {
        CameraIssue::message(
            "native_types",
            "No suitable progressive camera format in the bounded 8-stream/64-type search",
        )
    })
}

fn current_layout(
    reader: &IMFSourceReader,
    expected: &CameraFormat,
) -> CaptureResult<CameraLayout> {
    let current = unsafe { reader.GetCurrentMediaType(expected.stream) }
        .map_err(|error| CameraIssue::native("current_type", error))?;
    let size = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|error| CameraIssue::native("current_size", error))?;
    let rate = unsafe { current.GetUINT64(&MF_MT_FRAME_RATE) }
        .map_err(|error| CameraIssue::native("current_rate", error))?;
    let numerator = (rate >> 32) as u32;
    let denominator = rate as u32;
    if (size >> 32) as u32 != expected.width
        || size as u32 != expected.height
        || numerator == 0
        || denominator == 0
        || u64::from(numerator) * u64::from(expected.fps_denominator)
            != u64::from(expected.fps_numerator) * u64::from(denominator)
    {
        return Err(CameraIssue::message(
            "current_type",
            "Camera output dimensions or frame rate changed from the selected request",
        ));
    }
    CameraLayout::from_current_type(&current)
        .map_err(|error| CameraIssue::message("current_layout", error.to_string()))
}

fn configure(session: &mut SourceSession, request: &CameraRequest) -> CaptureResult<CameraFormat> {
    let callback: IMFSourceReaderCallback = CameraCallback {
        shared: session.shared.clone(),
    }
    .into();
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 3) }
        .map_err(|error| CameraIssue::native("reader_attributes", error))?;
    let attributes = attributes.ok_or_else(|| {
        CameraIssue::message("reader_attributes", "No reader attributes returned")
    })?;
    (|| -> windows::core::Result<()> {
        unsafe {
            attributes.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)?;
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
            // owner 显式 Shutdown source；不让 Reader 析构再次关闭它。
            attributes.SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 1)
        }
    })()
    .map_err(|error| CameraIssue::native("reader_attributes", error))?;
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&session.source, &attributes) }
        .map_err(|error| CameraIssue::native("reader_creation", error))?;
    session.reader = Some(reader);
    let reader = session
        .reader
        .as_ref()
        .ok_or_else(|| CameraIssue::message("reader_creation", "No reader returned"))?;
    let selected = select_native(reader, request)?;
    let wanted =
        unsafe { MFCreateMediaType() }.map_err(|error| CameraIssue::native("rgb32_type", error))?;
    (|| -> windows::core::Result<()> {
        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(selected.format.stream, true)?;
            reader.SetCurrentMediaType(selected.format.stream, None, &selected.native)?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            wanted.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(selected.format.width) << 32) | u64::from(selected.format.height),
            )?;
            wanted.SetUINT64(
                &MF_MT_FRAME_RATE,
                (u64::from(selected.format.fps_numerator) << 32)
                    | u64::from(selected.format.fps_denominator),
            )?;
            wanted.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            reader.SetCurrentMediaType(selected.format.stream, None, &wanted)
        }
    })()
    .map_err(|error| CameraIssue::native("rgb32_negotiation", error))?;
    let layout = current_layout(reader, &selected.format)?;
    let mut inbox = session.shared.lock();
    inbox.layout = Some(layout);
    inbox.status.negotiated = Some(selected.format.clone());
    Ok(selected.format)
}

fn read_loop(
    session: &mut SourceSession,
    format: &CameraFormat,
    request: &CameraRequest,
    started: Instant,
) -> CaptureResult<()> {
    loop {
        if session.shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut inbox = session.shared.lock();
        if let Some(failure) = &inbox.status.failure {
            return Err(failure.clone());
        }
        if inbox.status.delivery_closed {
            return Ok(());
        }
        if inbox.status.frames_received == 0 && started.elapsed() >= request.first_frame_budget {
            return Err(CameraIssue::message(
                "first_frame_wait",
                "No real camera frame arrived within the between-call budget",
            ));
        }
        if started.elapsed() >= request.run_budget {
            return Ok(());
        }
        if inbox
            .last_frame_at
            .is_some_and(|time| time.elapsed() > STALL_LIMIT)
        {
            return Err(CameraIssue::message(
                "frame_stall",
                "Camera stopped delivering frames",
            ));
        }
        if inbox.empty_reads > EMPTY_READ_LIMIT
            || inbox.status.format_notifications > FORMAT_NOTICE_LIMIT
        {
            return Err(CameraIssue::message(
                "read_limit",
                "Camera repeatedly returned empty samples or format notifications",
            ));
        }
        if inbox.format_notice {
            inbox.format_notice = false;
            let previous = inbox.layout.clone();
            drop(inbox);
            let reader = session
                .reader
                .as_ref()
                .ok_or_else(|| CameraIssue::message("reader", "Reader is unavailable"))?;
            let observed = current_layout(reader, format)?;
            // 同样 coded 尺寸/帧率下，合法 stride/aperture 通知可更新布局；旧样本已丢弃。
            // fresh 解析仍严格核验 RGB32、PAR、旋转与每行缓冲边界，不沿用旧解释。
            let mut inbox = session.shared.lock();
            if previous.as_ref() != Some(&observed) {
                inbox.status.layout_reconfigurations += 1;
            }
            inbox.layout = Some(observed);
            continue;
        }
        if !inbox.pending {
            inbox.pending = true;
            drop(inbox);
            let reader = session
                .reader
                .as_ref()
                .ok_or_else(|| CameraIssue::message("reader", "Reader is unavailable"))?;
            session.issued_read = true;
            if let Err(error) =
                unsafe { reader.ReadSample(format.stream, 0, None, None, None, None) }
            {
                session.shared.lock().reader_failed = true;
                return Err(CameraIssue::native("read_request", error));
            }
            continue;
        }
        drop(
            session
                .shared
                .changed
                .wait_timeout(inbox, WAIT_SLICE)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

fn capture_worker(request: &CameraRequest, shared: &Arc<Shared>) -> CaptureResult<()> {
    let started = Instant::now();
    let _runtime =
        MfRuntime::new().map_err(|error| CameraIssue::message("runtime", error.to_string()))?;
    if shared.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 2) }
        .map_err(|error| CameraIssue::native("device_attributes", error))?;
    let attributes = attributes.ok_or_else(|| {
        CameraIssue::message("device_attributes", "No device attributes returned")
    })?;
    use std::os::windows::ffi::OsStringExt;
    let symbolic_link = std::ffi::OsString::from_wide(request.device.as_units());
    let symbolic_link = HSTRING::from(symbolic_link.as_os_str());
    (|| -> windows::core::Result<()> {
        unsafe {
            attributes.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )?;
            attributes.SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                &symbolic_link,
            )
        }
    })()
    .map_err(|error| CameraIssue::native("device_attributes", error))?;
    // 请求必须显式给出 opaque ID；绝不默认选择枚举中的第一个设备。
    let source = unsafe { MFCreateDeviceSource(&attributes) }
        .map_err(|error| CameraIssue::native("device_creation", error))?;
    let mut session = SourceSession {
        source,
        reader: None,
        shared: shared.clone(),
        issued_read: false,
        shutdown_done: false,
    };
    let result = configure(&mut session, request)
        .and_then(|format| read_loop(&mut session, &format, request, started));
    let issues = session.close();
    let cleanup_failure = issues.first().cloned();
    shared.lock().status.cleanup_issues = issues;
    result?;
    if let Some(issue) = cleanup_failure {
        return Err(issue);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(value: u8) -> CameraFrame {
        CameraFrame {
            width: 2,
            height: 2,
            bgra: [value, 0, 0, 255].repeat(4),
        }
    }
    fn negotiated(shared: &Shared) {
        shared.lock().status.negotiated = Some(CameraFormat {
            stream: 0,
            width: 2,
            height: 2,
            fps_numerator: 30,
            fps_denominator: 1,
            requested_native_subtype: "test-only".into(),
            converted_subtype: "rgb32",
        });
    }
    #[test]
    fn recording_camera_mailbox_replaces_one_frame_and_rejects_late_delivery() {
        let shared = Shared::default();
        negotiated(&shared);
        assert_eq!(shared.lock().status.phase, CameraPhase::Starting);
        shared.publish(frame(1), -7);
        shared.publish(frame(2), 400_000);
        {
            let inbox = shared.lock();
            assert_eq!(inbox.status.frames_received, 2);
            assert_eq!(inbox.status.frames_replaced, 1);
            assert_eq!(inbox.latest.as_ref().unwrap().frame.bgra[0], 2);
            assert_eq!(inbox.latest.as_ref().unwrap().sequence, 2);
        }
        shared.close_delivery();
        shared.publish(frame(3), 700_000);
        let inbox = shared.lock();
        assert!(inbox.latest.is_none());
        assert_eq!(inbox.status.frames_received, 2);
        assert_eq!(inbox.status.phase, CameraPhase::Closing);
        assert!(!inbox.status.worker_finished);
    }
    #[test]
    fn recording_camera_initial_format_notice_is_not_a_frame_or_failure() {
        let shared = Arc::new(Shared::default());
        negotiated(&shared);
        shared.lock().pending = true;
        let callback = CameraCallback {
            shared: shared.clone(),
        };
        callback.read(
            HRESULT(0),
            0,
            MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32,
            0,
            None,
        );
        let inbox = shared.lock();
        assert!(inbox.format_notice && !inbox.pending);
        assert_eq!(inbox.status.format_notifications, 1);
        assert_eq!(inbox.status.frames_received, 0);
        assert_eq!(inbox.status.phase, CameraPhase::Starting);
        assert!(inbox.status.failure.is_none());
    }
    #[test]
    fn recording_camera_reader_error_is_terminal_but_late_shutdown_callbacks_are_ignored() {
        use windows::Win32::Foundation::E_FAIL;
        let shared = Arc::new(Shared::default());
        negotiated(&shared);
        let callback = CameraCallback {
            shared: shared.clone(),
        };
        callback.read(E_FAIL, 0, MF_SOURCE_READERF_ERROR.0 as u32, -17, None);
        shared.fail(CameraIssue::message("later_cleanup", "secondary"), false);
        {
            let inbox = shared.lock();
            assert!(inbox.reader_failed && inbox.status.delivery_closed);
            assert_eq!(inbox.status.phase, CameraPhase::Failed);
            assert_eq!(
                inbox.status.failure.as_ref().unwrap().stage,
                "read_callback"
            );
            assert_eq!(
                inbox.status.failure.as_ref().unwrap().hresult,
                Some(E_FAIL.0)
            );
        }
        let closed = Arc::new(Shared::default());
        closed.close_delivery();
        CameraCallback {
            shared: closed.clone(),
        }
        .read(E_FAIL, 0, MF_SOURCE_READERF_ERROR.0 as u32, 0, None);
        let inbox = closed.lock();
        assert!(inbox.reader_failed);
        assert!(inbox.status.failure.is_none());
        assert_eq!(inbox.status.phase, CameraPhase::Closing);
    }
    #[test]
    fn recording_camera_request_limits_are_checked_before_a_worker_or_device_exists() {
        let id = CameraDeviceId::from_utf16("test-device".encode_utf16().collect()).unwrap();
        let request = CameraRequest::new(id);
        assert!(request.validate().is_ok());
        for invalid in [
            CameraRequest {
                max_width: 1,
                ..request.clone()
            },
            CameraRequest {
                max_fps: 0,
                ..request.clone()
            },
            CameraRequest {
                max_width: 4096,
                max_height: 4096,
                ..request.clone()
            },
            CameraRequest {
                first_frame_budget: Duration::ZERO,
                ..request.clone()
            },
            CameraRequest {
                run_budget: Duration::from_secs(61),
                ..request.clone()
            },
            CameraRequest {
                run_budget: Duration::from_secs(1),
                ..request.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
