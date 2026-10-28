//! 仅浏览器 QA 使用：Job/进程/窗口与活跃 DOM 租约必须同时证明这次 ROI 的来源。
use crate::{
    geometry::ScreenRect,
    long_capture_border::{BorderHudDescriptor, BorderHudValidation},
    long_capture_runtime::{FrameGate, FrameGateFactory},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::c_void,
    mem::size_of,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{FILETIME, HWND, POINT, RECT},
    Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MonitorFromRect, ScreenToClient, MONITORINFO,
        MONITOR_DEFAULTTONULL,
    },
    UI::{
        HiDpi::{
            GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        WindowsAndMessaging::*,
    },
};

const FRESH: Duration = Duration::from_millis(500);
const MAX_LEASES: usize = 64;

/// 几何和命中检查统一使用物理像素；作用域仅限同步门，不跨 await。
struct PhysicalDpi(DPI_AWARENESS_CONTEXT);
impl PhysicalDpi {
    fn new() -> Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0.is_null() {
            return Err(format!(
                "浏览器物理DPI上下文切换失败：{}",
                windows::core::Error::from_win32()
            )
            .into());
        }
        Ok(Self(previous))
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

// QA 私有 ABI；不为产品增加 JobObjects/Dwm Cargo feature。
mod ffi {
    use super::{c_void, FILETIME};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn OpenJobObjectW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        pub fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
        pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        pub fn GetProcessTimes(
            process: *mut c_void,
            created: *mut FILETIME,
            exited: *mut FILETIME,
            kernel: *mut FILETIME,
            user: *mut FILETIME,
        ) -> i32;
        pub fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
        pub fn CloseHandle(handle: *mut c_void) -> i32;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        pub fn DwmGetWindowAttribute(
            window: *mut c_void,
            attribute: u32,
            value: *mut c_void,
            bytes: u32,
        ) -> i32;
    }
}
struct NativeHandle(usize);
impl NativeHandle {
    fn raw(&self) -> *mut c_void {
        self.0 as *mut c_void
    }
    fn process(pid: u32) -> Result<Self> {
        // PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE。
        let raw = unsafe { ffi::OpenProcess(0x1000 | 0x0010_0000, 0, pid) };
        if raw.is_null() {
            return Err(windows::core::Error::from_win32().into());
        }
        Ok(Self(raw as usize))
    }
    fn alive(&self) -> bool {
        unsafe { ffi::WaitForSingleObject(self.raw(), 0) == 258 }
    }
    fn creation(&self) -> Result<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if unsafe {
            ffi::GetProcessTimes(
                self.raw(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(windows::core::Error::from_win32().into());
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}
impl Drop for NativeHandle {
    fn drop(&mut self) {
        unsafe {
            ffi::CloseHandle(self.raw());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserRect {
    #[serde(rename = "left")]
    pub(crate) left: i32,
    #[serde(rename = "top")]
    pub(crate) top: i32,
    #[serde(rename = "width")]
    pub(crate) width: u32,
    #[serde(rename = "height")]
    pub(crate) height: u32,
}
impl BrowserRect {
    pub(crate) fn screen(self) -> ScreenRect {
        ScreenRect {
            left: self.left,
            top: self.top,
            width: self.width,
            height: self.height,
        }
    }
}
impl From<ScreenRect> for BrowserRect {
    fn from(r: ScreenRect) -> Self {
        Self {
            left: r.left,
            top: r.top,
            width: r.width,
            height: r.height,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserContentWindow {
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "window_pid")]
    pub(crate) window_pid: u32,
    #[serde(rename = "window_thread")]
    pub(crate) window_thread: u32,
    #[serde(rename = "class")]
    pub(crate) class_name: String,
    #[serde(rename = "creation_filetime")]
    pub(crate) creation_filetime: String,
    /// 原始子窗口客户区必须冻结；可见交集不能掩盖它的变化。
    #[serde(rename = "client_rect")]
    pub(crate) client_rect: BrowserRect,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserOwnerConfig {
    #[serde(rename = "job_name")]
    pub(crate) job_name: String,
    #[serde(rename = "browser_pid")]
    pub(crate) browser_pid: u32,
    #[serde(rename = "browser_creation_filetime")]
    pub(crate) browser_creation_filetime: String,
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "title_nonce")]
    pub(crate) title_nonce: String,
    #[serde(rename = "client_rect")]
    pub(crate) client_rect: BrowserRect,
    #[serde(rename = "dpi")]
    pub(crate) dpi: u32,
    #[serde(rename = "class")]
    pub(crate) class_name: String,
    #[serde(rename = "expected_document_nonce")]
    pub(crate) expected_document_nonce: String,
    #[serde(rename = "expected_target_id")]
    pub(crate) expected_target_id: String,
    #[serde(
        default,
        rename = "content_window",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) content_window: Option<BrowserContentWindow>,
}
impl BrowserOwnerConfig {
    pub(crate) fn content_region(&self) -> Result<ScreenRect> {
        configured_content_region(self)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CssRect {
    #[serde(rename = "left")]
    pub(crate) left: f64,
    #[serde(rename = "top")]
    pub(crate) top: f64,
    #[serde(rename = "width")]
    pub(crate) width: f64,
    #[serde(rename = "height")]
    pub(crate) height: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VisualViewport {
    #[serde(rename = "width")]
    pub(crate) width: f64,
    #[serde(rename = "height")]
    pub(crate) height: f64,
    #[serde(rename = "offset_left")]
    pub(crate) offset_left: f64,
    #[serde(rename = "offset_top")]
    pub(crate) offset_top: f64,
    #[serde(rename = "scale")]
    pub(crate) scale: f64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserLease {
    #[serde(rename = "lease_id")]
    pub(crate) lease_id: String,
    #[serde(rename = "document_nonce")]
    pub(crate) document_nonce: String,
    #[serde(rename = "target_id")]
    pub(crate) target_id: String,
    #[serde(rename = "layout_epoch")]
    pub(crate) layout_epoch: u64,
    #[serde(rename = "dpr")]
    pub(crate) dpr: f64,
    #[serde(rename = "visual_viewport")]
    pub(crate) visual_viewport: VisualViewport,
    #[serde(rename = "scrollport")]
    pub(crate) scrollport: CssRect,
    #[serde(rename = "observed_unix_ms")]
    pub(crate) observed_unix_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct BrowserNativeSnapshot {
    #[serde(rename = "job_name")]
    pub(crate) job_name: String,
    #[serde(rename = "browser_pid")]
    pub(crate) browser_pid: u32,
    #[serde(rename = "browser_creation_filetime")]
    pub(crate) browser_creation_filetime: String,
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "window_pid")]
    pub(crate) window_pid: u32,
    #[serde(rename = "window_thread")]
    pub(crate) window_thread: u32,
    #[serde(rename = "class")]
    pub(crate) class_name: String,
    #[serde(rename = "title_nonce")]
    pub(crate) title_nonce: String,
    #[serde(rename = "client_rect")]
    pub(crate) client_rect: BrowserRect,
    #[serde(rename = "window_rect")]
    pub(crate) window_rect: BrowserRect,
    #[serde(rename = "dpi")]
    pub(crate) dpi: u32,
    #[serde(rename = "content_window", skip_serializing_if = "Option::is_none")]
    pub(crate) content_window: Option<BrowserContentWindow>,
    #[serde(rename = "content_rect")]
    pub(crate) content_rect: BrowserRect,
}

fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn token_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.contains('\0')
}
fn physical_bounds(r: ScreenRect) -> Option<(i64, i64, i64, i64)> {
    let (right, bottom) = (
        i64::from(r.left) + i64::from(r.width),
        i64::from(r.top) + i64::from(r.height),
    );
    (r.width > 0 && r.height > 0 && right <= i64::from(i32::MAX) && bottom <= i64::from(i32::MAX))
        .then_some((i64::from(r.left), i64::from(r.top), right, bottom))
}
fn validate_region(owner: ScreenRect, region: ScreenRect) -> Result<()> {
    let a = physical_bounds(owner).ok_or("浏览器客户区物理范围无效")?;
    let b = physical_bounds(region).ok_or("浏览器采集ROI物理范围无效")?;
    ensure(
        owner.width <= 16_384
            && owner.height <= 16_384
            && u64::from(owner.width) * u64::from(owner.height) <= 32_000_000
            && b.0 >= a.0
            && b.1 >= a.1
            && b.2 <= a.2
            && b.3 <= a.3,
        "浏览器ROI未完整落在原自有客户区或超过预算",
    )
}
/// 子窗口由父客户区裁剪；使用精确半开交集，而不是放宽原始矩形的比较。
fn clipped_content_region(parent: ScreenRect, child: ScreenRect) -> Result<ScreenRect> {
    validate_region(parent, parent)?;
    let p = physical_bounds(parent).ok_or("浏览器父客户区范围无效")?;
    let c = physical_bounds(child).ok_or("浏览器内容子窗口客户区范围无效")?;
    let (left, top, right, bottom) = (p.0.max(c.0), p.1.max(c.1), p.2.min(c.2), p.3.min(c.3));
    ensure(
        left < right && top < bottom,
        "浏览器内容子窗口没有可见客户区",
    )?;
    let visible = ScreenRect {
        left: i32::try_from(left)?,
        top: i32::try_from(top)?,
        width: u32::try_from(right - left)?,
        height: u32::try_from(bottom - top)?,
    };
    validate_region(parent, visible)?;
    Ok(visible)
}
fn configured_content_region(config: &BrowserOwnerConfig) -> Result<ScreenRect> {
    match &config.content_window {
        Some(content) => {
            clipped_content_region(config.client_rect.screen(), content.client_rect.screen())
        }
        None => {
            let region = config.client_rect.screen();
            validate_region(region, region)?;
            Ok(region)
        }
    }
}
fn validate_frozen_content_rect(
    parent: ScreenRect,
    frozen: BrowserRect,
    actual: ScreenRect,
) -> Result<ScreenRect> {
    ensure(
        actual == frozen.screen(),
        "浏览器原内容子窗口客户区改变；交集不能掩盖原始矩形变化",
    )?;
    clipped_content_region(parent, actual)
}
fn device_pixels(css: f64, dpr: f64) -> Result<u32> {
    let physical = css * dpr;
    ensure(
        css.is_finite()
            && css >= 0.
            && dpr.is_finite()
            && dpr > 0.
            && physical.is_finite()
            && (physical - physical.round()).abs() < 1e-6
            && physical.round() <= f64::from(u32::MAX),
        "浏览器DOM坐标不是整数设备像素",
    )?;
    Ok(physical.round() as u32)
}
pub(crate) fn expected_content_roi(
    content: ScreenRect,
    lease: &BrowserLease,
) -> Result<ScreenRect> {
    validate_region(content, content)?;
    let r = lease.scrollport;
    let region = ScreenRect {
        left: i32::try_from(
            i64::from(content.left) + i64::from(device_pixels(r.left, lease.dpr)?),
        )?,
        top: i32::try_from(i64::from(content.top) + i64::from(device_pixels(r.top, lease.dpr)?))?,
        width: device_pixels(r.width, lease.dpr)?,
        height: device_pixels(r.height, lease.dpr)?,
    };
    validate_region(content, region)?;
    Ok(region)
}
pub(crate) fn validate_content_mapping(
    content: ScreenRect,
    lease: &BrowserLease,
    calibrated: ScreenRect,
) -> Result<()> {
    ensure(
        expected_content_roi(content, lease)? == calibrated,
        "浏览器renderer唯一校准位置与DOM内容坐标不符",
    )
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct PixelOrigin {
    #[serde(rename = "left")]
    left: i32,
    #[serde(rename = "top")]
    top: i32,
}
/// legacy HWND只是包围兼容窗；完整唯一renderer校准的ROI才证明表面的实际原点。
fn calibrated_pixel_origin(
    visible: ScreenRect,
    lease: &BrowserLease,
    calibrated: ScreenRect,
) -> Result<PixelOrigin> {
    validate_region(visible, calibrated)?;
    let r = lease.scrollport;
    ensure(
        calibrated.width == device_pixels(r.width, lease.dpr)?
            && calibrated.height == device_pixels(r.height, lease.dpr)?,
        "浏览器完整renderer校准尺寸与DOM scrollport设备尺寸不符",
    )?;
    Ok(PixelOrigin {
        left: i32::try_from(
            i64::from(calibrated.left) - i64::from(device_pixels(r.left, lease.dpr)?),
        )?,
        top: i32::try_from(
            i64::from(calibrated.top) - i64::from(device_pixels(r.top, lease.dpr)?),
        )?,
    })
}
fn overlaps(a: ScreenRect, b: ScreenRect) -> bool {
    match (physical_bounds(a), physical_bounds(b)) {
        (Some(a), Some(b)) => a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3,
        _ => false,
    }
}
fn points(r: ScreenRect) -> [(i32, i32); 9] {
    let xs = [0, r.width / 2, r.width - 1];
    let ys = [0, r.height / 2, r.height - 1];
    std::array::from_fn(|i| {
        (
            (i64::from(r.left) + i64::from(xs[i % 3])) as i32,
            (i64::from(r.top) + i64::from(ys[i / 3])) as i32,
        )
    })
}
fn same_lease(a: &BrowserLease, b: &BrowserLease) -> bool {
    a.lease_id == b.lease_id
        && a.document_nonce == b.document_nonce
        && a.target_id == b.target_id
        && a.layout_epoch == b.layout_epoch
        && a.dpr == b.dpr
        && a.visual_viewport == b.visual_viewport
        && a.scrollport == b.scrollport
}
fn wall_now() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}
fn wall_fresh(observed: u64, now: u64) -> bool {
    observed <= now && now - observed <= 500
}
fn validate_lease(lease: &BrowserLease, config: &BrowserOwnerConfig, wall: u64) -> Result<()> {
    let v = lease.visual_viewport;
    let r = lease.scrollport;
    let content = config.content_region()?;
    ensure(
        token_valid(&lease.lease_id)
            && lease.document_nonce == config.expected_document_nonce
            && lease.target_id == config.expected_target_id,
        "浏览器租约document/target/nonce身份不一致",
    )?;
    ensure(
        [
            lease.dpr,
            v.width,
            v.height,
            v.offset_left,
            v.offset_top,
            v.scale,
            r.left,
            r.top,
            r.width,
            r.height,
        ]
        .into_iter()
        .all(f64::is_finite)
            && lease.dpr > 0.
            && (config.content_window.is_none() || lease.dpr == f64::from(config.dpi) / 96.)
            && v.width > 0.
            && v.height > 0.
            && v.offset_left == 0.
            && v.offset_top == 0.
            && v.scale == 1.
            && r.left >= 0.
            && r.top >= 0.
            && r.width > 0.
            && r.height > 0.
            && r.left + r.width <= v.width
            && r.top + r.height <= v.height
            && device_pixels(v.width, lease.dpr).ok() == Some(content.width)
            && device_pixels(v.height, lease.dpr).ok() == Some(content.height),
        "浏览器租约viewport/DPR/scrollport不符合自有可见内容区",
    )?;
    expected_content_roi(content, lease)?;
    ensure(
        wall_fresh(lease.observed_unix_ms, wall),
        "浏览器DOM观测已超过500ms或时间来自未来",
    )
}
struct ActiveLease {
    identity: Arc<BrowserLease>,
    received: Instant,
    observed: u64,
}
#[derive(Default)]
struct LeaseState {
    active: Option<ActiveLease>,
    seen: HashSet<String>,
    disconnected: bool,
    revocations: u64,
    reason: Option<String>,
}
struct SharedLease {
    config: BrowserOwnerConfig,
    state: Mutex<LeaseState>,
}
impl SharedLease {
    fn revoke_locked(state: &mut LeaseState, reason: &str) {
        state.active = None;
        state.revocations = state.revocations.saturating_add(1);
        state.reason = Some(reason.into());
    }
    fn install_at(&self, lease: BrowserLease, now: Instant, wall: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::revoke_locked(&mut state, "显式建立新DOM租约，旧采集门已撤销");
        ensure(!state.disconnected, "浏览器控制管道已断开")?;
        ensure(
            state.seen.len() < MAX_LEASES && !state.seen.contains(&lease.lease_id),
            "DOM租约ID已使用或超过本次QA租约上限",
        )?;
        validate_lease(&lease, &self.config, wall)?;
        state.seen.insert(lease.lease_id.clone());
        state.active = Some(ActiveLease {
            observed: lease.observed_unix_ms,
            identity: Arc::new(lease),
            received: now,
        });
        state.reason = None;
        Ok(())
    }
    fn fresh_locked(state: &mut LeaseState, now: Instant, wall: u64) -> Result<()> {
        let active = state
            .active
            .as_ref()
            .ok_or("浏览器DOM租约已撤销，需要显式new_lease")?;
        if now.saturating_duration_since(active.received) > FRESH
            || !wall_fresh(active.observed, wall)
        {
            Self::revoke_locked(state, "浏览器DOM租约超过500ms，heartbeat不能续回旧门");
            return Err("浏览器DOM租约已过期".into());
        }
        Ok(())
    }
    fn update_at(&self, lease: BrowserLease, now: Instant, wall: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::fresh_locked(&mut state, now, wall)?;
        let active = state.active.as_ref().expect("fresh已确认租约");
        let valid = validate_lease(&lease, &self.config, wall).is_ok()
            && same_lease(&active.identity, &lease)
            && lease.observed_unix_ms >= active.observed;
        if !valid {
            Self::revoke_locked(
                &mut state,
                "浏览器document/target/layout/viewport/DPR租约改变",
            );
            return Err("浏览器DOM租约身份或几何已改变".into());
        }
        let active = state.active.as_mut().expect("租约保持有效");
        // 重放同一观测不能人为延长新鲜期。
        if lease.observed_unix_ms > active.observed {
            active.observed = lease.observed_unix_ms;
            active.received = now;
        }
        Ok(())
    }
    fn token_at(&self, now: Instant, wall: u64) -> Result<Arc<BrowserLease>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::fresh_locked(&mut state, now, wall)?;
        Ok(state
            .active
            .as_ref()
            .expect("fresh已确认租约")
            .identity
            .clone())
    }
    fn check_at(&self, token: &Arc<BrowserLease>, now: Instant, wall: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::fresh_locked(&mut state, now, wall)?;
        ensure(
            Arc::ptr_eq(
                token,
                &state.active.as_ref().expect("fresh已确认租约").identity,
            ),
            "该采集门绑定的旧DOM租约已撤销",
        )
    }
    fn revoke_if_current(&self, token: &Arc<BrowserLease>, reason: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(token, &active.identity))
        {
            Self::revoke_locked(&mut state, reason);
        }
    }
}

/// 校准失败、旧token或过期不能污染首次原点；持有租约锁直到冻结完成。
fn freeze_pixel_origin_at(
    frozen: &Mutex<Option<PixelOrigin>>,
    shared: &SharedLease,
    visible: ScreenRect,
    token: &Arc<BrowserLease>,
    calibrated: ScreenRect,
    now: Instant,
    wall: u64,
) -> Result<PixelOrigin> {
    let candidate = calibrated_pixel_origin(visible, token, calibrated)?;
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    SharedLease::fresh_locked(&mut state, now, wall)?;
    ensure(
        Arc::ptr_eq(
            token,
            &state.active.as_ref().expect("fresh已确认租约").identity,
        ),
        "该像素校准绑定的旧DOM租约已撤销",
    )?;
    let mut frozen = frozen.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(original) = *frozen {
        ensure(
            original == candidate,
            "浏览器实际DOM像素原点改变；不能用新租约重置或容忍1px",
        )?;
    } else {
        *frozen = Some(candidate);
    }
    Ok(candidate)
}

struct BrowserCalibrationIdentity;

/// 字段私有：校准不能用当前新租约重新包装先前读取的像素。
struct BrowserCalibrationStamp {
    owner: Arc<BrowserCalibrationIdentity>,
    token: Arc<BrowserLease>,
}

pub(crate) struct BrowserCalibrationPermit {
    stamp: BrowserCalibrationStamp,
    gate: FrameGate,
}
impl BrowserCalibrationPermit {
    pub(crate) fn gate(&self) -> &FrameGate {
        &self.gate
    }
}

fn calibration_stamp_at(
    owner: &Arc<BrowserCalibrationIdentity>,
    shared: &SharedLease,
    now: Instant,
    wall: u64,
) -> Result<BrowserCalibrationStamp> {
    Ok(BrowserCalibrationStamp {
        owner: owner.clone(),
        token: shared.token_at(now, wall)?,
    })
}

fn check_calibration_stamp_at(
    owner: &Arc<BrowserCalibrationIdentity>,
    shared: &SharedLease,
    stamp: &BrowserCalibrationStamp,
    now: Instant,
    wall: u64,
) -> Result<()> {
    ensure(
        Arc::ptr_eq(owner, &stamp.owner),
        "像素校准许可不属于本次原浏览器owner",
    )?;
    shared.check_at(&stamp.token, now, wall)
}

/// 与真正bind共用：只允许读取时原owner/原token冻结，不重取当前租约。
fn freeze_calibration_stamp_at(
    owner: &Arc<BrowserCalibrationIdentity>,
    shared: &SharedLease,
    frozen: &Mutex<Option<PixelOrigin>>,
    stamp: &BrowserCalibrationStamp,
    visible: ScreenRect,
    calibrated: ScreenRect,
    time: (Instant, u64),
) -> Result<PixelOrigin> {
    check_calibration_stamp_at(owner, shared, stamp, time.0, time.1)?;
    freeze_pixel_origin_at(
        frozen,
        shared,
        visible,
        &stamp.token,
        calibrated,
        time.0,
        time.1,
    )
}

#[derive(Default)]
struct Probe {
    checks: AtomicU64,
    failures: AtomicU64,
    first_failure: Mutex<Option<String>>,
    last_failure: Mutex<Option<String>>,
    hud_registrations: AtomicU64,
    hud_checks: AtomicU64,
    hud_failures: AtomicU64,
    hud_last_validation: Mutex<Option<BorderHudValidation>>,
    hud_last_failure: Mutex<Option<String>>,
}
pub(crate) struct BrowserCaptureOwner {
    config: BrowserOwnerConfig,
    native: BrowserNativeSnapshot,
    job: NativeHandle,
    process: NativeHandle,
    creation: u64,
    content: Option<OriginalContentWindow>,
    lease: SharedLease,
    pixel_origin: Mutex<Option<PixelOrigin>>,
    calibration_identity: Arc<BrowserCalibrationIdentity>,
    bound_calibration_lease: Mutex<Option<std::sync::Weak<BrowserLease>>>,
    probe: Probe,
    border_hud: Mutex<Option<BorderHudDescriptor>>,
}
struct OriginalContentWindow {
    window: BrowserContentWindow,
    process: NativeHandle,
    creation: u64,
}
fn native_rect(r: RECT) -> Result<ScreenRect> {
    Ok(ScreenRect {
        left: r.left,
        top: r.top,
        width: u32::try_from(i64::from(r.right) - i64::from(r.left))?,
        height: u32::try_from(i64::from(r.bottom) - i64::from(r.top))?,
    })
}
fn client_rect(window: HWND) -> Result<ScreenRect> {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(window, &mut rect)?;
    }
    ensure(rect.left == 0 && rect.top == 0, "浏览器客户区局部原点无效")?;
    let mut origin = POINT::default();
    unsafe {
        ClientToScreen(window, &mut origin).ok()?;
    }
    let mut result = native_rect(rect)?;
    result.left = origin.x;
    result.top = origin.y;
    Ok(result)
}
fn window_rect(window: HWND) -> Result<ScreenRect> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(window, &mut rect)?;
    }
    native_rect(rect)
}
/// 只在已验证的自有 HUD 命中处查询原浏览器内部，不枚举其它应用的子窗口。
fn browser_hit_below_hud(root: HWND, point: POINT) -> Result<HWND> {
    let mut current = root;
    let mut visited = HashSet::new();
    loop {
        ensure(
            visited.len() < 32 && visited.insert(current.0 as usize),
            "自有边框下浏览器子窗口命中发生循环或超界",
        )?;
        let mut local = point;
        unsafe {
            ScreenToClient(current, &mut local).ok()?;
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                local,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        ensure(!child.is_invalid(), "自有边框下没有原浏览器客户区命中")?;
        ensure(
            unsafe { GetAncestor(child, GA_ROOT) } == root,
            "自有边框下子窗口不属于原浏览器根窗口",
        )?;
        if child == current {
            return Ok(child);
        }
        current = child;
    }
}
fn class_name(window: HWND) -> String {
    let mut value = [0_u16; 256];
    let length = unsafe { GetClassNameW(window, &mut value) }.max(0) as usize;
    String::from_utf16_lossy(&value[..length])
}
fn title_has_nonce(window: HWND, nonce: &str) -> bool {
    let mut value = [0_u16; 1024];
    let length = unsafe { GetWindowTextW(window, &mut value) }.max(0) as usize;
    String::from_utf16_lossy(&value[..length]).contains(nonce)
}
fn cloaked(window: HWND) -> Result<bool> {
    let mut value = 0_u32;
    let result = unsafe {
        ffi::DwmGetWindowAttribute(
            window.0,
            14,
            (&mut value as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    ensure(result >= 0, "无法验证窗口DWM cloak状态")?;
    Ok(value != 0)
}
impl BrowserCaptureOwner {
    pub(crate) fn from_config(config: BrowserOwnerConfig) -> Result<Arc<Self>> {
        let _dpi = PhysicalDpi::new()?;
        ensure(
            config.browser_pid != 0
                && config.hwnd != 0
                && config.dpi != 0
                && [
                    &config.job_name,
                    &config.class_name,
                    &config.title_nonce,
                    &config.expected_document_nonce,
                    &config.expected_target_id,
                ]
                .into_iter()
                .all(|v| token_valid(v)),
            "浏览器native config身份字段无效",
        )?;
        validate_region(config.client_rect.screen(), config.client_rect.screen())?;
        ensure(
            !config.browser_creation_filetime.is_empty()
                && config
                    .browser_creation_filetime
                    .bytes()
                    .all(|b| b.is_ascii_digit()),
            "浏览器creation FILETIME必须是十进制u64字符串",
        )?;
        let creation = config.browser_creation_filetime.parse::<u64>()?;
        let name: Vec<u16> = config.job_name.encode_utf16().chain(Some(0)).collect();
        let raw = unsafe { ffi::OpenJobObjectW(4, 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(windows::core::Error::from_win32().into());
        }
        let job = NativeHandle(raw as usize);
        let process = NativeHandle::process(config.browser_pid)?;
        ensure(
            process.alive() && process.creation()? == creation,
            "浏览器leader PID/creation身份已失效",
        )?;
        let h = HWND(config.hwnd as *mut _);
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        ensure(
            pid == config.browser_pid && thread != 0,
            "浏览器根窗口不属于原leader进程",
        )?;
        let content_rect = config.content_region()?;
        let content = config
            .content_window
            .as_ref()
            .map(|window| -> Result<_> {
                ensure(
                    window.hwnd != 0
                        && window.hwnd != config.hwnd
                        && window.window_pid != 0
                        && window.window_thread != 0
                        && token_valid(&window.class_name)
                        && !window.creation_filetime.is_empty()
                        && window.creation_filetime.bytes().all(|b| b.is_ascii_digit()),
                    "浏览器内容窗口身份字段无效",
                )?;
                let creation = window.creation_filetime.parse::<u64>()?;
                ensure(creation != 0, "浏览器内容窗口creation无效")?;
                let process = NativeHandle::process(window.window_pid)?;
                ensure(
                    process.alive() && process.creation()? == creation,
                    "浏览器原内容窗口进程已退出或creation改变",
                )?;
                Ok(OriginalContentWindow {
                    window: window.clone(),
                    process,
                    creation,
                })
            })
            .transpose()?;
        let native = BrowserNativeSnapshot {
            job_name: config.job_name.clone(),
            browser_pid: config.browser_pid,
            browser_creation_filetime: creation.to_string(),
            hwnd: config.hwnd,
            window_pid: pid,
            window_thread: thread,
            class_name: config.class_name.clone(),
            title_nonce: config.title_nonce.clone(),
            client_rect: config.client_rect,
            window_rect: window_rect(h)?.into(),
            dpi: config.dpi,
            content_window: config.content_window.clone(),
            content_rect: content_rect.into(),
        };
        let owner = Arc::new(Self {
            lease: SharedLease {
                config: config.clone(),
                state: Mutex::new(LeaseState::default()),
            },
            config,
            native,
            job,
            process,
            creation,
            content,
            pixel_origin: Mutex::new(None),
            calibration_identity: Arc::new(BrowserCalibrationIdentity),
            bound_calibration_lease: Mutex::new(None),
            probe: Probe::default(),
            border_hud: Mutex::new(None),
        });
        owner.validate_native(owner.client_region())?;
        Ok(owner)
    }
    pub(crate) fn config(&self) -> &BrowserOwnerConfig {
        &self.config
    }
    pub(crate) fn client_region(&self) -> ScreenRect {
        self.native.client_rect.screen()
    }
    pub(crate) fn content_region(&self) -> ScreenRect {
        self.native.content_rect.screen()
    }
    /// QA 必须显式登记生产 Border 的原弱描述符；不按类名、PID 或 WDA 泛化豁免。
    /// 固定 ROI 验收先完成校准，再登记；旧 gate 没有登记时保持原拒绝规则。
    pub(crate) fn register_border_hud(&self, descriptor: BorderHudDescriptor) -> Result<()> {
        let _dpi = PhysicalDpi::new()?;
        let validation = descriptor.validate()?;
        validate_region(self.content_region(), validation.region)?;
        let mut registered = self
            .border_hud
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = registered.as_ref() {
            ensure(
                previous.same_instance(&descriptor) || !previous.is_live(),
                "浏览器门仍登记另一存活长截图边框实例",
            )?;
        }
        *registered = Some(descriptor);
        *self
            .probe
            .hud_last_validation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(validation);
        *self
            .probe
            .hud_last_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        self.probe.hud_registrations.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// 下一次校准前只能显式退休已经关闭的 HUD；不能在采集途中取消活实例检查。
    pub(crate) fn retire_closed_border_hud(&self) -> Result<()> {
        let mut registered = self
            .border_hud
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure(
            registered
                .as_ref()
                .is_none_or(|descriptor| !descriptor.is_live()),
            "存活长截图边框不能撤销归属登记",
        )?;
        *registered = None;
        Ok(())
    }

    fn validate_border_hud(&self, region: ScreenRect) -> Result<Option<BorderHudValidation>> {
        let descriptor = self
            .border_hud
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        self.probe.hud_checks.fetch_add(1, Ordering::AcqRel);
        let result = descriptor.validate().and_then(|validation| {
            ensure(
                validation.region == region,
                "登记长截图边框与本次固定 ROI 不一致",
            )?;
            Ok(validation)
        });
        match result {
            Ok(validation) => {
                *self
                    .probe
                    .hud_last_validation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(validation.clone());
                Ok(Some(validation))
            }
            Err(error) => {
                self.probe.hud_failures.fetch_add(1, Ordering::AcqRel);
                *self
                    .probe
                    .hud_last_failure
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                Err(error)
            }
        }
    }
    pub(crate) fn native_snapshot(&self) -> BrowserNativeSnapshot {
        self.native.clone()
    }
    pub(crate) fn install_lease(&self, lease: BrowserLease) -> Result<()> {
        // 显式重建先撤旧门；native 未恢复时不能仅凭新 DOM 消息恢复采集。
        self.revoke("显式重建DOM租约");
        self.validate_native(self.client_region())?;
        self.lease.install_at(lease, Instant::now(), wall_now()?)
    }
    pub(crate) fn update_lease(&self, lease: BrowserLease) -> Result<()> {
        self.lease.update_at(lease, Instant::now(), wall_now()?)
    }
    pub(crate) fn revoke(&self, reason: &str) {
        let mut state = self.lease.state.lock().unwrap_or_else(|e| e.into_inner());
        SharedLease::revoke_locked(&mut state, reason);
    }
    pub(crate) fn disconnect(&self) {
        let mut state = self.lease.state.lock().unwrap_or_else(|e| e.into_inner());
        state.disconnected = true;
        SharedLease::revoke_locked(&mut state, "浏览器控制管道断开");
    }
    pub(crate) fn client_gate(self: &Arc<Self>) -> Result<FrameGate> {
        let token = self.lease.token_at(Instant::now(), wall_now()?)?;
        self.make_gate(self.client_region(), self.client_region(), token)
    }
    pub(crate) fn calibration_permit(self: &Arc<Self>) -> Result<BrowserCalibrationPermit> {
        let stamp = calibration_stamp_at(
            &self.calibration_identity,
            &self.lease,
            Instant::now(),
            wall_now()?,
        )?;
        let gate = self.make_gate(
            self.client_region(),
            self.client_region(),
            stamp.token.clone(),
        )?;
        Ok(BrowserCalibrationPermit { stamp, gate })
    }
    /// 调用方须在permit.gate下完成整张renderer的唯一像素校准，并传入同一许可。
    pub(crate) fn bind_content(
        self: &Arc<Self>,
        calibrated: ScreenRect,
        permit: &BrowserCalibrationPermit,
    ) -> Result<FrameGateFactory> {
        check_calibration_stamp_at(
            &self.calibration_identity,
            &self.lease,
            &permit.stamp,
            Instant::now(),
            wall_now()?,
        )?;
        let token = &permit.stamp.token;
        if self.content.is_some() {
            // 不承诺整个DOM viewport可见；实际ROI仍必须完整位于未放宽的原交集内。
            validate_region(self.content_region(), calibrated)?;
        } else {
            // 保留已有kiosk的原生client精确映射合同。
            validate_content_mapping(self.content_region(), token, calibrated)?;
        }
        if let Err(error) = self.validate_native(calibrated) {
            self.lease.revoke_if_current(token, &error.to_string());
            return Err(error);
        }
        if self.content.is_some() {
            freeze_calibration_stamp_at(
                &self.calibration_identity,
                &self.lease,
                &self.pixel_origin,
                &permit.stamp,
                self.content_region(),
                calibrated,
                (Instant::now(), wall_now()?),
            )?;
        } else {
            check_calibration_stamp_at(
                &self.calibration_identity,
                &self.lease,
                &permit.stamp,
                Instant::now(),
                wall_now()?,
            )?;
        }
        *self
            .bound_calibration_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(token));
        let token = token.clone();
        let owner = self.clone();
        Ok(Arc::new(move |region| {
            owner.make_gate(calibrated, region, token.clone())
        }))
    }
    pub(crate) fn content_mapping_probe(&self) -> serde_json::Value {
        let state = self.lease.state.lock().unwrap_or_else(|e| e.into_inner());
        let origin = *self.pixel_origin.lock().unwrap_or_else(|e| e.into_inner());
        let dimensions = |width, height, dpr| {
            device_pixels(width, dpr)
                .ok()
                .zip(device_pixels(height, dpr).ok())
                .map(|(width, height)| serde_json::json!({"width":width,"height":height}))
        };
        let active = state.active.as_ref().map(|v| &v.identity);
        let bound = self
            .bound_calibration_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let calibration_source_same_lease = active
            .zip(bound.as_ref())
            .is_some_and(|(active, bound)| Arc::ptr_eq(active, bound));
        let roi = active.and_then(|l| dimensions(l.scrollport.width, l.scrollport.height, l.dpr));
        let viewport = active
            .and_then(|l| dimensions(l.visual_viewport.width, l.visual_viewport.height, l.dpr));
        let legacy_raw = self.content.as_ref().map(|c| PixelOrigin {
            left: c.window.client_rect.left,
            top: c.window.client_rect.top,
        });
        let kiosk_origin = PixelOrigin {
            left: self.client_region().left,
            top: self.client_region().top,
        };
        serde_json::json!({
            "source":if self.content.is_none() {"native-kiosk-client-exact"} else if origin.is_some() {"unique-full-renderer-pixel-calibration"} else {"unbound-legacy-compatible-window"},
            "frozen":origin.is_some(),
            "origin":if self.content.is_some() {origin} else {Some(kiosk_origin)},
            "legacy_raw_origin":legacy_raw,
            "native_visible_content_rect":self.native.content_rect,
            "roi_dimensions":roi,
            "viewport_dimensions":viewport,
            "calibration_source_same_lease":calibration_source_same_lease,
            "whole_dom_viewport_visibility_proven":false
        })
    }
    fn make_gate(
        self: &Arc<Self>,
        allowed: ScreenRect,
        region: ScreenRect,
        token: Arc<BrowserLease>,
    ) -> Result<FrameGate> {
        validate_region(allowed, region)?;
        let owner = self.clone();
        let gate: FrameGate = Arc::new(move || {
            owner.probe.checks.fetch_add(1, Ordering::AcqRel);
            let result: Result<()> = (|| {
                owner.lease.check_at(&token, Instant::now(), wall_now()?)?;
                if let Err(error) = owner.validate_native(region) {
                    // 仅撤销这个门的租约，迟到的旧门不能撤销另一个显式新租约。
                    owner.lease.revoke_if_current(&token, &error.to_string());
                    return Err(error);
                }
                owner.lease.check_at(&token, Instant::now(), wall_now()?)
            })();
            if let Err(error) = &result {
                owner.probe.failures.fetch_add(1, Ordering::AcqRel);
                let message = error.to_string();
                owner
                    .probe
                    .first_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(message.clone());
                *owner
                    .probe
                    .last_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(message);
            }
            result
        });
        gate()?;
        Ok(gate)
    }
    fn in_job(&self, process: &NativeHandle) -> Result<()> {
        let mut value = 0;
        if unsafe { ffi::IsProcessInJob(process.raw(), self.job.raw(), &mut value) } == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        ensure(value != 0, "窗口进程不属于本次自有浏览器Job")
    }
    fn validate_native(&self, region: ScreenRect) -> Result<()> {
        let _dpi = PhysicalDpi::new()?;
        validate_region(self.client_region(), region)?;
        let hud = self.validate_border_hud(region)?;
        ensure(
            self.process.alive() && self.process.creation()? == self.creation,
            "原浏览器进程已退出或creation变化",
        )?;
        self.in_job(&self.process)?;
        let h = HWND(self.native.hwnd as *mut _);
        unsafe {
            let mut pid = 0;
            let thread = GetWindowThreadProcessId(h, Some(&mut pid));
            ensure(
                IsWindow(Some(h)).as_bool()
                    && IsWindowVisible(h).as_bool()
                    && !IsIconic(h).as_bool()
                    && pid == self.native.window_pid
                    && thread == self.native.window_thread
                    && GetAncestor(h, GA_ROOT) == h,
                "原浏览器根窗口已关闭/隐藏/最小化或身份变化",
            )?;
            ensure(
                class_name(h) == self.native.class_name
                    && title_has_nonce(h, &self.native.title_nonce)
                    && !cloaked(h)?,
                "原浏览器class/title nonce/cloak改变",
            )?;
            ensure(
                GetWindowLongPtrW(h, GWL_EXSTYLE) & (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize
                    == 0,
                "浏览器来源不是不透明正常窗口",
            )?;
            let actual_client = client_rect(h)?;
            let actual_window = window_rect(h)?;
            let actual_dpi = GetDpiForWindow(h);
            ensure(
                actual_client == self.client_region()
                    && actual_window == self.native.window_rect.screen()
                    && actual_dpi == self.native.dpi,
                &format!(
                    "浏览器原客户区/窗口位置或DPI改变：actual_client={actual_client:?}, expected_client={:?}, actual_window_rect={actual_window:?}, expected_window_rect={:?}, actual_dpi={actual_dpi}, expected_dpi={}",
                    self.client_region(),
                    self.native.window_rect.screen(),
                    self.native.dpi,
                ),
            )?;
            if let Some(content) = &self.content {
                let original = &content.window;
                ensure(
                    content.process.alive() && content.process.creation()? == content.creation,
                    "浏览器原内容进程已退出或creation改变",
                )?;
                self.in_job(&content.process)?;
                let child = HWND(original.hwnd as *mut _);
                let mut child_pid = 0;
                let child_thread = GetWindowThreadProcessId(child, Some(&mut child_pid));
                ensure(
                    IsWindow(Some(child)).as_bool()
                        && IsWindowVisible(child).as_bool()
                        && !IsIconic(child).as_bool()
                        && child_pid == original.window_pid
                        && child_thread == original.window_thread
                        && GetAncestor(child, GA_ROOT) == h
                        && IsChild(h, child).as_bool()
                        && GetParent(child).ok() == Some(h)
                        && class_name(child) == original.class_name,
                    "浏览器原内容子窗口可见性/直接父窗口/归属/PID/TID/class改变",
                )?;
                let actual_raw = client_rect(child)?;
                ensure(
                    validate_frozen_content_rect(actual_client, original.client_rect, actual_raw)?
                        == self.content_region(),
                    "浏览器原可见内容交集改变",
                )?;
            }
            let client = self.client_region();
            let bounds = physical_bounds(client).ok_or("浏览器客户区端点无效")?;
            let rect = RECT {
                left: client.left,
                top: client.top,
                right: bounds.2 as i32,
                bottom: bounds.3 as i32,
            };
            let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONULL);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure(
                !monitor.is_invalid()
                    && GetMonitorInfoW(monitor, &mut info).as_bool()
                    && client.left >= info.rcMonitor.left
                    && client.top >= info.rcMonitor.top
                    && bounds.2 <= i64::from(info.rcMonitor.right)
                    && bounds.3 <= i64::from(info.rcMonitor.bottom),
                "浏览器自有客户区未完整落在单显示器内",
            )?;
            let mut previous = GetWindow(h, GW_HWNDPREV).ok();
            let mut visited = HashSet::new();
            while let Some(covering) = previous {
                ensure(
                    visited.len() < 1024 && visited.insert(covering.0 as usize),
                    "浏览器ROI Z序校验发生循环或超界",
                )?;
                if IsWindowVisible(covering).as_bool()
                    && !IsIconic(covering).as_bool()
                    && overlaps(window_rect(covering)?, region)
                    && !hud.as_ref().is_some_and(|hud| hud.contains(covering))
                {
                    ensure(cloaked(covering)?, "浏览器ROI被上方可见窗口遮挡")?;
                }
                previous = GetWindow(covering, GW_HWNDPREV).ok();
            }
            for (x, y) in points(region) {
                let hit = WindowFromPoint(POINT { x, y });
                // 全 ROI Z 序先排除了其它可见顶层遮挡。精确 HUD 命中仍需检查其下
                // 原浏览器的子窗口/Job，不能直接跳过九点根归属校验。
                let hit = if hud.as_ref().is_some_and(|hud| hud.contains(hit)) {
                    browser_hit_below_hud(h, POINT { x, y })?
                } else {
                    hit
                };
                let mut hit_pid = 0;
                let hit_thread = GetWindowThreadProcessId(hit, Some(&mut hit_pid));
                ensure(
                    !hit.is_invalid() && hit_thread != 0 && GetAncestor(hit, GA_ROOT) == h,
                    "浏览器ROI命中不属于原根窗口",
                )?;
                if hit_pid != self.native.window_pid {
                    let child = NativeHandle::process(hit_pid)?;
                    ensure(child.alive(), "浏览器子窗口进程已退出")?;
                    self.in_job(&child)?;
                }
            }
        }
        // 原 HWND 可能在同步 Win32 校验期间被关闭或改属性；返回前再次验证。
        if hud.is_some() {
            self.validate_border_hud(region)?;
        }
        Ok(())
    }
    pub(crate) fn probe(&self) -> serde_json::Value {
        let content_mapping = self.content_mapping_probe();
        let hud = {
            let registered = self
                .border_hud
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            serde_json::json!({
                "registered": registered.is_some(),
                "original_instance_alive": registered.as_ref().is_some_and(BorderHudDescriptor::is_live),
                "registrations": self.probe.hud_registrations.load(Ordering::Acquire),
                "checks": self.probe.hud_checks.load(Ordering::Acquire),
                "failures": self.probe.hud_failures.load(Ordering::Acquire),
                "last_failure": self.probe.hud_last_failure.lock().unwrap_or_else(|error| error.into_inner()).clone(),
                "last_successful_validation": self.probe.hud_last_validation.lock().unwrap_or_else(|error| error.into_inner()).clone(),
                "fixed_roi_required": true,
                "class_or_pid_wide_exception": false,
                "weak_lifetime_only": true
            })
        };
        let state = self.lease.state.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"checks":self.probe.checks.load(Ordering::Acquire),"failures":self.probe.failures.load(Ordering::Acquire),"first_failure":self.probe.first_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),"last_failure":self.probe.last_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),"lease_active":state.active.is_some(),"disconnected":state.disconnected,"lease_revocations":state.revocations,"lease_reason":state.reason,"installed_lease_count":state.seen.len(),"whole_roi_zorder_checked":true,"nine_point_owned_root_checked":true,"child_process_job_membership_required":true,"content_window_present":self.content.is_some(),"content_rect":self.native.content_rect,"content_mapping":content_mapping,"raw_content_client_frozen":self.content.is_some(),"foreign_windows_titled_or_read":false,"excluded_overlay_exception":false,"registered_production_border_hud":hud,"desktop_pixels_read_by_gate":false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> BrowserOwnerConfig {
        BrowserOwnerConfig {
            job_name: "qa-job".into(),
            browser_pid: 1,
            browser_creation_filetime: "1".into(),
            hwnd: 1,
            title_nonce: "title".into(),
            client_rect: BrowserRect {
                left: -960,
                top: 20,
                width: 960,
                height: 720,
            },
            dpi: 144,
            class_name: "class".into(),
            expected_document_nonce: "doc".into(),
            expected_target_id: "target".into(),
            content_window: None,
        }
    }
    fn lease(id: &str, observed: u64) -> BrowserLease {
        BrowserLease {
            lease_id: id.into(),
            document_nonce: "doc".into(),
            target_id: "target".into(),
            layout_epoch: 7,
            dpr: 1.5,
            visual_viewport: VisualViewport {
                width: 640.,
                height: 480.,
                offset_left: 0.,
                offset_top: 0.,
                scale: 1.,
            },
            scrollport: CssRect {
                left: 32.,
                top: 40.,
                width: 320.,
                height: 240.,
            },
            observed_unix_ms: observed,
        }
    }
    #[test]
    fn long_browser_owner_roi_negative_origin_edges_and_budget_are_independent() {
        let owner = config().client_rect.screen();
        assert!(validate_region(owner, owner).is_ok());
        let sub = ScreenRect {
            left: -800,
            top: 80,
            width: 320,
            height: 240,
        };
        assert!(validate_region(owner, sub).is_ok());
        for invalid in [
            ScreenRect { left: -961, ..sub },
            ScreenRect {
                left: -100,
                width: 101,
                ..sub
            },
            ScreenRect {
                top: 739,
                height: 2,
                ..sub
            },
            ScreenRect { width: 0, ..sub },
            ScreenRect {
                left: i32::MAX,
                width: 1,
                ..sub
            },
        ] {
            assert!(validate_region(owner, invalid).is_err());
        }
        let side = ScreenRect {
            left: 0,
            top: 0,
            width: 16_384,
            height: 1,
        };
        assert!(validate_region(side, side).is_ok());
        assert!(validate_region(
            ScreenRect {
                width: 16_385,
                ..side
            },
            side
        )
        .is_err());
        let area = ScreenRect {
            width: 8000,
            height: 4000,
            ..side
        };
        assert!(validate_region(area, area).is_ok());
        assert!(validate_region(
            ScreenRect {
                height: 4001,
                ..area
            },
            area
        )
        .is_err());
    }
    #[test]
    fn long_browser_owner_full_roi_occlusion_detects_strip_between_nine_samples() {
        let r = ScreenRect {
            left: -100,
            top: -50,
            width: 101,
            height: 81,
        };
        assert_eq!(
            points(r),
            [
                (-100, -50),
                (-50, -50),
                (0, -50),
                (-100, -10),
                (-50, -10),
                (0, -10),
                (-100, 30),
                (-50, 30),
                (0, 30)
            ]
        );
        let strip = ScreenRect {
            left: -75,
            top: -50,
            width: 1,
            height: 81,
        };
        assert!(points(r).into_iter().all(|p| p.0 != strip.left));
        assert!(overlaps(r, strip), "仅九点会漏掉窄条，完整Z序ROI必须拒绝");
        assert!(!overlaps(r, ScreenRect { left: 1, ..strip }));
        assert!(!overlaps(r, ScreenRect { top: 31, ..strip }));
        assert!(overlaps(
            r,
            ScreenRect {
                left: 0,
                width: 2,
                ..strip
            }
        ));
    }
    #[test]
    fn long_browser_owner_lease_geometry_requires_same_opaque_kiosk_device_viewport() {
        let c = config();
        let l = lease("a", 1000);
        assert!(validate_lease(&l, &c, 1500).is_ok());
        for bad in [
            BrowserLease {
                dpr: f64::NAN,
                ..l.clone()
            },
            BrowserLease {
                dpr: 1.,
                ..l.clone()
            },
            BrowserLease {
                visual_viewport: VisualViewport {
                    scale: 1.2,
                    ..l.visual_viewport
                },
                ..l.clone()
            },
            BrowserLease {
                scrollport: CssRect {
                    left: 400.,
                    ..l.scrollport
                },
                ..l.clone()
            },
            BrowserLease {
                document_nonce: "other".into(),
                ..l.clone()
            },
            BrowserLease {
                target_id: "other".into(),
                ..l.clone()
            },
            BrowserLease {
                observed_unix_ms: 1501,
                ..l.clone()
            },
        ] {
            assert!(validate_lease(&bad, &c, 1500).is_err());
        }
        assert!(validate_lease(&l, &c, 1501).is_err());
        let mut changed = l.clone();
        changed.layout_epoch += 1;
        assert!(!same_lease(&l, &changed));
        changed = l.clone();
        changed.observed_unix_ms += 1;
        assert!(same_lease(&l, &changed), "只有观测时间可由heartbeat更新");
    }
    #[test]
    fn long_browser_owner_expiry_change_disconnect_latch_and_new_lease_never_revive_old_gate() {
        let shared = SharedLease {
            config: config(),
            state: Mutex::new(LeaseState::default()),
        };
        let start = Instant::now();
        shared.install_at(lease("a", 1000), start, 1000).unwrap();
        let old = shared.token_at(start, 1000).unwrap();
        assert!(shared
            .check_at(&old, start + Duration::from_millis(500), 1500)
            .is_ok());
        assert!(shared
            .update_at(lease("a", 1501), start + Duration::from_millis(501), 1501)
            .is_err());
        assert!(shared
            .update_at(lease("a", 1502), start + Duration::from_millis(502), 1502)
            .is_err());
        assert!(shared
            .install_at(lease("a", 1502), start + Duration::from_millis(502), 1502)
            .is_err());
        shared
            .install_at(lease("b", 1502), start + Duration::from_millis(502), 1502)
            .unwrap();
        let new = shared
            .token_at(start + Duration::from_millis(502), 1502)
            .unwrap();
        assert!(shared
            .check_at(&old, start + Duration::from_millis(502), 1502)
            .is_err());
        shared.revoke_if_current(&old, "迟到旧门native失败");
        assert!(shared
            .check_at(&new, start + Duration::from_millis(502), 1502)
            .is_ok());
        shared.revoke_if_current(&new, "native遮挡");
        assert!(shared
            .update_at(lease("b", 1503), start + Duration::from_millis(503), 1503)
            .is_err());
        shared
            .install_at(lease("b2", 1503), start + Duration::from_millis(503), 1503)
            .unwrap();
        let mut changed = lease("b2", 1503);
        changed.layout_epoch += 1;
        assert!(shared
            .update_at(changed, start + Duration::from_millis(503), 1503)
            .is_err());
        assert!(shared
            .update_at(lease("b2", 1504), start + Duration::from_millis(504), 1504)
            .is_err());
        shared
            .install_at(lease("c", 1504), start + Duration::from_millis(504), 1504)
            .unwrap();
        {
            let mut state = shared.state.lock().unwrap();
            state.disconnected = true;
            SharedLease::revoke_locked(&mut state, "EOF");
        }
        assert!(shared
            .install_at(lease("d", 1505), start + Duration::from_millis(505), 1505)
            .is_err());
    }

    fn app_config() -> BrowserOwnerConfig {
        BrowserOwnerConfig {
            client_rect: BrowserRect {
                left: -1669,
                top: -1002,
                width: 1600,
                height: 1002,
            },
            dpi: 120,
            content_window: Some(BrowserContentWindow {
                hwnd: 2,
                window_pid: 1,
                window_thread: 3,
                class_name: "Chrome_RenderWidgetHostHWND".into(),
                creation_filetime: "1".into(),
                client_rect: BrowserRect {
                    left: -1669,
                    top: -965,
                    width: 1600,
                    height: 966,
                },
            }),
            ..config()
        }
    }
    fn app_lease(id: &str, observed: u64) -> BrowserLease {
        BrowserLease {
            dpr: 1.25,
            visual_viewport: VisualViewport {
                width: 1280.,
                height: 772.,
                offset_left: 0.,
                offset_top: 0.,
                scale: 1.,
            },
            scrollport: CssRect {
                left: 64.,
                top: 32.,
                width: 768.,
                height: 512.,
            },
            ..lease(id, observed)
        }
    }

    #[test]
    fn long_browser_content_clip_negative_origin_and_frozen_raw_bounds() {
        let config = app_config();
        let parent = config.client_rect.screen();
        let raw = config.content_window.as_ref().unwrap().client_rect;
        let expected = ScreenRect {
            left: -1669,
            top: -965,
            width: 1600,
            height: 965,
        };
        assert_eq!(configured_content_region(&config).unwrap(), expected);
        assert_eq!(
            validate_frozen_content_rect(parent, raw, raw.screen()).unwrap(),
            expected
        );
        // 多出一行仍会得到相同可见交集，但旧门必须拒绝原始子窗口的变化。
        let changed = ScreenRect {
            height: raw.height + 1,
            ..raw.screen()
        };
        assert_eq!(clipped_content_region(parent, changed).unwrap(), expected);
        assert!(validate_frozen_content_rect(parent, raw, changed).is_err());
        for invalid in [
            ScreenRect {
                left: -69,
                ..raw.screen()
            },
            ScreenRect {
                top: 0,
                ..raw.screen()
            },
            ScreenRect {
                width: 0,
                ..raw.screen()
            },
            ScreenRect {
                left: i32::MAX,
                width: 1,
                ..raw.screen()
            },
            ScreenRect {
                top: i32::MAX,
                height: 1,
                ..raw.screen()
            },
        ] {
            assert!(clipped_content_region(parent, invalid).is_err());
        }
        let parent = ScreenRect {
            left: 69,
            top: 60,
            width: 1600,
            height: 1002,
        };
        let child = ScreenRect {
            left: 69,
            top: 97,
            width: 1600,
            height: 966,
        };
        assert_eq!(
            clipped_content_region(parent, child).unwrap(),
            ScreenRect {
                left: 69,
                top: 97,
                width: 1600,
                height: 965,
            }
        );
    }

    #[test]
    fn long_browser_content_normal_lease_wire_identity_geometry_and_expiry() {
        let config = app_config();
        let lease = app_lease("normal", 1000);
        assert!(validate_lease(&lease, &config, 1500).is_ok());
        let mut kiosk = config.clone();
        kiosk.content_window = None;
        assert!(
            validate_lease(&lease, &kiosk, 1500).is_err(),
            "旧kiosk仍要求原root客户区全等"
        );
        let legacy_json = serde_json::to_value(super::tests::config()).unwrap();
        assert!(legacy_json.get("content_window").is_none());
        let legacy: BrowserOwnerConfig = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy.content_window.is_none());
        let wire = serde_json::to_value(&config).unwrap();
        assert_eq!(
            wire["content_window"]["class"],
            "Chrome_RenderWidgetHostHWND"
        );
        assert_eq!(wire["content_window"]["creation_filetime"], "1");
        assert_eq!(wire["content_window"]["window_thread"], 3);
        let decoded: BrowserOwnerConfig = serde_json::from_value(wire).unwrap();
        assert_eq!(
            configured_content_region(&decoded).unwrap(),
            configured_content_region(&config).unwrap()
        );
        for bad in [
            BrowserLease {
                document_nonce: "other".into(),
                ..lease.clone()
            },
            BrowserLease {
                target_id: "other".into(),
                ..lease.clone()
            },
            BrowserLease {
                visual_viewport: VisualViewport {
                    height: 772.8,
                    ..lease.visual_viewport
                },
                ..lease.clone()
            },
            BrowserLease {
                visual_viewport: VisualViewport {
                    height: 772.4,
                    ..lease.visual_viewport
                },
                ..lease.clone()
            },
            BrowserLease {
                dpr: 1.,
                visual_viewport: VisualViewport {
                    width: 1600.,
                    height: 965.,
                    ..lease.visual_viewport
                },
                ..lease.clone()
            },
            BrowserLease {
                observed_unix_ms: 1501,
                ..lease.clone()
            },
        ] {
            assert!(validate_lease(&bad, &config, 1500).is_err());
        }
        let shared = SharedLease {
            config,
            state: Mutex::new(LeaseState::default()),
        };
        let now = Instant::now();
        shared.install_at(lease.clone(), now, 1000).unwrap();
        let original = shared.token_at(now, 1000).unwrap();
        assert!(shared
            .check_at(&original, now + Duration::from_millis(500), 1500)
            .is_ok());
        assert!(shared
            .update_at(
                app_lease("normal", 1501),
                now + Duration::from_millis(501),
                1501
            )
            .is_err());
        shared
            .install_at(
                app_lease("new", 1501),
                now + Duration::from_millis(501),
                1501,
            )
            .unwrap();
        assert!(shared
            .check_at(&original, now + Duration::from_millis(501), 1501)
            .is_err());
        let mut layout_changed = app_lease("new", 1502);
        layout_changed.layout_epoch += 1;
        assert!(shared
            .update_at(layout_changed, now + Duration::from_millis(502), 1502)
            .is_err());
    }

    #[test]
    fn long_browser_content_calibration_exact_dom_device_position_rejects_one_pixel() {
        let config = app_config();
        let content = configured_content_region(&config).unwrap();
        let lease = app_lease("normal", 1000);
        let expected = ScreenRect {
            left: -1589,
            top: -925,
            width: 960,
            height: 640,
        };
        assert_eq!(expected_content_roi(content, &lease).unwrap(), expected);
        assert!(validate_content_mapping(content, &lease, expected).is_ok());
        for wrong in [
            ScreenRect {
                left: expected.left + 1,
                ..expected
            },
            ScreenRect {
                top: expected.top - 1,
                ..expected
            },
            ScreenRect {
                width: expected.width - 1,
                ..expected
            },
            ScreenRect {
                height: expected.height + 1,
                ..expected
            },
        ] {
            assert!(validate_region(content, wrong).is_ok());
            assert!(
                validate_content_mapping(content, &lease, wrong).is_err(),
                "仍处于owned内容区的错误匹配不能变成有效映射"
            );
        }
        for scrollport in [
            CssRect {
                left: 64.4,
                ..lease.scrollport
            },
            CssRect {
                top: f64::NAN,
                ..lease.scrollport
            },
            CssRect {
                width: 0.,
                ..lease.scrollport
            },
            CssRect {
                left: 640.,
                ..lease.scrollport
            },
            CssRect {
                left: f64::MAX,
                ..lease.scrollport
            },
        ] {
            assert!(expected_content_roi(
                content,
                &BrowserLease {
                    scrollport,
                    ..lease.clone()
                }
            )
            .is_err());
        }
        let kiosk = super::tests::config();
        let old = super::tests::lease("kiosk", 1000);
        let expected = ScreenRect {
            left: -912,
            top: 80,
            width: 480,
            height: 360,
        };
        assert_eq!(
            expected_content_roi(kiosk.client_rect.screen(), &old).unwrap(),
            expected
        );
        assert!(validate_content_mapping(kiosk.client_rect.screen(), &old, expected).is_ok());
    }

    #[test]
    fn long_browser_content_origin_independent_pixels_negative_edges_and_overflow() {
        let config = app_config();
        let visible = configured_content_region(&config).unwrap();
        let lease = app_lease("pixel", 1000);
        let actual = ScreenRect {
            left: -1589,
            top: -924,
            width: 960,
            height: 640,
        };
        let origin = PixelOrigin {
            left: -1669,
            top: -964,
        };
        assert_eq!(
            calibrated_pixel_origin(visible, &lease, actual).unwrap(),
            origin
        );
        assert!(
            validate_content_mapping(visible, &lease, actual).is_err(),
            "raw兼容窗的理论原点不能替代已经逐像素证明的实际原点"
        );
        assert_eq!(
            calibrated_pixel_origin(
                visible,
                &lease,
                ScreenRect {
                    top: actual.top - 1,
                    ..actual
                }
            )
            .unwrap(),
            PixelOrigin {
                top: origin.top - 1,
                ..origin
            },
            "首次校准不假定固定+1；真正锁定的是测得原点"
        );
        for bad in [
            ScreenRect {
                width: 959,
                ..actual
            },
            ScreenRect {
                height: 641,
                ..actual
            },
            ScreenRect { width: 0, ..actual },
            ScreenRect {
                left: visible.left - 1,
                ..actual
            },
            ScreenRect {
                top: -639,
                ..actual
            },
            ScreenRect {
                left: i32::MAX,
                width: 1,
                ..actual
            },
        ] {
            assert!(calibrated_pixel_origin(visible, &lease, bad).is_err());
        }
        for scrollport in [
            CssRect {
                left: 64.4,
                ..lease.scrollport
            },
            CssRect {
                top: f64::NAN,
                ..lease.scrollport
            },
            CssRect {
                width: f64::MAX,
                ..lease.scrollport
            },
            CssRect {
                height: 0.,
                ..lease.scrollport
            },
        ] {
            assert!(calibrated_pixel_origin(
                visible,
                &BrowserLease {
                    scrollport,
                    ..lease.clone()
                },
                actual,
            )
            .is_err());
        }
        let extreme = ScreenRect {
            left: i32::MIN,
            top: -965,
            width: 1600,
            height: 965,
        };
        assert!(
            calibrated_pixel_origin(
                extreme,
                &lease,
                ScreenRect {
                    left: i32::MIN + 30,
                    top: -924,
                    width: 960,
                    height: 640,
                }
            )
            .is_err(),
            "ROI仍在clip内也不能产生超出i32的DOM原点"
        );
    }

    #[test]
    fn long_browser_content_origin_freeze_failure_preserves_first_calibration() {
        let config = app_config();
        let visible = configured_content_region(&config).unwrap();
        let shared = SharedLease {
            config,
            state: Mutex::new(LeaseState::default()),
        };
        let now = Instant::now();
        let lease = app_lease("pixel", 1000);
        shared.install_at(lease.clone(), now, 1000).unwrap();
        let token = shared.token_at(now, 1000).unwrap();
        let frozen = Mutex::new(None);
        let actual = ScreenRect {
            left: -1589,
            top: -924,
            width: 960,
            height: 640,
        };
        let expected = PixelOrigin {
            left: -1669,
            top: -964,
        };
        assert!(freeze_pixel_origin_at(
            &frozen,
            &shared,
            visible,
            &token,
            ScreenRect {
                width: 959,
                ..actual
            },
            now,
            1000
        )
        .is_err());
        assert_eq!(*frozen.lock().unwrap(), None, "首次尺寸校准失败不污染状态");
        let foreign_token = Arc::new(lease);
        assert!(
            freeze_pixel_origin_at(&frozen, &shared, visible, &foreign_token, actual, now, 1000)
                .is_err(),
            "字段相同但不是当前实际租约的校准不能冻结"
        );
        assert_eq!(*frozen.lock().unwrap(), None);
        assert_eq!(
            freeze_pixel_origin_at(&frozen, &shared, visible, &token, actual, now, 1000).unwrap(),
            expected
        );
        assert_eq!(
            freeze_pixel_origin_at(&frozen, &shared, visible, &token, actual, now, 1000).unwrap(),
            expected
        );
        for wrong in [
            ScreenRect {
                top: actual.top + 1,
                ..actual
            },
            ScreenRect {
                left: actual.left - 1,
                ..actual
            },
            ScreenRect {
                height: 641,
                ..actual
            },
            ScreenRect {
                top: -639,
                ..actual
            },
        ] {
            assert!(
                freeze_pixel_origin_at(&frozen, &shared, visible, &token, wrong, now, 1000)
                    .is_err()
            );
            assert_eq!(*frozen.lock().unwrap(), Some(expected));
        }
    }

    #[test]
    fn long_browser_content_origin_new_lease_scrollport_and_expiry_keep_same_origin() {
        let config = app_config();
        let visible = configured_content_region(&config).unwrap();
        let shared = SharedLease {
            config,
            state: Mutex::new(LeaseState::default()),
        };
        let now = Instant::now();
        shared
            .install_at(app_lease("cycle1", 1000), now, 1000)
            .unwrap();
        let first = shared.token_at(now, 1000).unwrap();
        let frozen = Mutex::new(None);
        let actual = ScreenRect {
            left: -1589,
            top: -924,
            width: 960,
            height: 640,
        };
        let expected = PixelOrigin {
            left: -1669,
            top: -964,
        };
        freeze_pixel_origin_at(&frozen, &shared, visible, &first, actual, now, 1000).unwrap();
        let next = BrowserLease {
            scrollport: CssRect {
                left: 96.,
                top: 48.,
                width: 512.,
                height: 384.,
            },
            ..app_lease("cycle2", 1500)
        };
        let next_time = now + Duration::from_millis(500);
        shared.install_at(next, next_time, 1500).unwrap();
        let second = shared.token_at(next_time, 1500).unwrap();
        let second_roi = ScreenRect {
            left: -1549,
            top: -904,
            width: 640,
            height: 480,
        };
        assert!(
            freeze_pixel_origin_at(&frozen, &shared, visible, &first, actual, next_time, 1500)
                .is_err(),
            "旧校准租约不能进入新cycle"
        );
        assert_eq!(*frozen.lock().unwrap(), Some(expected));
        assert_eq!(
            freeze_pixel_origin_at(&frozen, &shared, visible, &second, second_roi, next_time, 1500)
                .unwrap(),
            expected,
            "新scrollport尺寸/位置依然使用同一个实测DOM原点"
        );
        assert!(
            freeze_pixel_origin_at(
                &frozen,
                &shared,
                visible,
                &second,
                ScreenRect {
                    top: second_roi.top + 1,
                    ..second_roi
                },
                next_time,
                1500
            )
            .is_err(),
            "显式新lease不能容忍1px漂移"
        );
        assert_eq!(*frozen.lock().unwrap(), Some(expected));
        assert!(
            freeze_pixel_origin_at(
                &frozen,
                &shared,
                visible,
                &second,
                second_roi,
                now + Duration::from_millis(1001),
                2001
            )
            .is_err(),
            "原500ms新鲜期不放宽"
        );
        assert_eq!(*frozen.lock().unwrap(), Some(expected));
        assert!(
            shared
                .update_at(
                    app_lease("cycle2", 2002),
                    now + Duration::from_millis(1002),
                    2002
                )
                .is_err(),
            "过期不能靠旧heartbeat复活"
        );
        shared
            .install_at(
                app_lease("cycle3", 2002),
                now + Duration::from_millis(1002),
                2002,
            )
            .unwrap();
        let third = shared
            .token_at(now + Duration::from_millis(1002), 2002)
            .unwrap();
        assert_eq!(
            freeze_pixel_origin_at(
                &frozen,
                &shared,
                visible,
                &third,
                actual,
                now + Duration::from_millis(1002),
                2002
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn long_browser_calibration_stamp_old_read_cannot_bind_new_revoked_or_expired_lease() {
        let config = app_config();
        let visible = configured_content_region(&config).unwrap();
        let shared = SharedLease {
            config,
            state: Mutex::new(LeaseState::default()),
        };
        let identity = Arc::new(BrowserCalibrationIdentity);
        let different_owner = Arc::new(BrowserCalibrationIdentity);
        let frozen = Mutex::new(None);
        let now = Instant::now();
        let actual = ScreenRect {
            left: -1589,
            top: -924,
            width: 960,
            height: 640,
        };
        shared
            .install_at(app_lease("read", 1000), now, 1000)
            .unwrap();
        let read = calibration_stamp_at(&identity, &shared, now, 1000).unwrap();
        let next_time = now + Duration::from_millis(10);
        shared
            .install_at(app_lease("same-geometry-new", 1010), next_time, 1010)
            .unwrap();
        assert!(
            freeze_calibration_stamp_at(
                &identity,
                &shared,
                &frozen,
                &read,
                visible,
                actual,
                (next_time, 1010),
            )
            .is_err(),
            "相同几何的新租约不能包装前次读取的校准像素"
        );
        assert_eq!(*frozen.lock().unwrap(), None);
        let fresh = calibration_stamp_at(&identity, &shared, next_time, 1010).unwrap();
        assert!(
            freeze_calibration_stamp_at(
                &different_owner,
                &shared,
                &frozen,
                &fresh,
                visible,
                actual,
                (next_time, 1010),
            )
            .is_err(),
            "owner对象也必须是读取时的原对象"
        );
        assert_eq!(*frozen.lock().unwrap(), None);
        assert!(
            freeze_calibration_stamp_at(
                &identity,
                &shared,
                &frozen,
                &fresh,
                visible,
                actual,
                (now + Duration::from_millis(511), 1511),
            )
            .is_err(),
            "超过500ms的原读取不能绑定"
        );
        assert_eq!(*frozen.lock().unwrap(), None);

        let later = now + Duration::from_millis(512);
        shared
            .install_at(app_lease("explicit-revoke", 1512), later, 1512)
            .unwrap();
        let revoked = calibration_stamp_at(&identity, &shared, later, 1512).unwrap();
        shared.revoke_if_current(&revoked.token, "真实受控撤销");
        assert!(freeze_calibration_stamp_at(
            &identity,
            &shared,
            &frozen,
            &revoked,
            visible,
            actual,
            (later, 1512),
        )
        .is_err());
        assert_eq!(*frozen.lock().unwrap(), None, "各失效读取均不污染初始原点");

        shared
            .install_at(app_lease("actual-new-read", 1513), later, 1513)
            .unwrap();
        let current = calibration_stamp_at(&identity, &shared, later, 1513).unwrap();
        let expected = PixelOrigin {
            left: -1669,
            top: -964,
        };
        assert_eq!(
            freeze_calibration_stamp_at(
                &identity,
                &shared,
                &frozen,
                &current,
                visible,
                actual,
                (later, 1513),
            )
            .unwrap(),
            expected
        );
        for old in [&read, &fresh, &revoked] {
            assert!(freeze_calibration_stamp_at(
                &identity,
                &shared,
                &frozen,
                old,
                visible,
                actual,
                (later, 1513),
            )
            .is_err());
            assert_eq!(*frozen.lock().unwrap(), Some(expected));
        }
    }
}
