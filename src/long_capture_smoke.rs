//! 自有 Win32 滚动窗口与真实 DXGI 采样；不派发系统输入、不改剪贴板。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    live_capture::LiveCapture,
    long_capture_runtime::{
        self, FrameGate, LongOptions, LongPhase, LongSession, LongSnapshot, RegionGuardFactory,
    },
    stitch::{ScrollAxis, StitchOptions, StitchSegment, StitchSnapshot, UpdateKind},
    Result,
};
use std::{
    fs::File,
    io::Write,
    mem::size_of,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{CreateMutexW, GetCurrentThreadId},
        },
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
};
const WIDTH: u32 = 480;
const HEIGHT: u32 = 360;
pub(crate) const STEP: i32 = 32;
pub(crate) const ORIGIN: i32 = 256;
const BANDS: (u32, u32) = (24, 20);
const POLL: Duration = Duration::from_millis(100);
const WAIT: Duration = Duration::from_secs(12);
const CHANGE_SCENE: u32 = WM_APP + 71;
const CHANGE_FOOTER: u32 = WM_APP + 72;
const CHANGE_LIMITS: u32 = WM_APP + 73;
const SCROLL_ANIMATION_TIMER: usize = 1;
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
pub(crate) fn copy_image(i: &Image) -> Image {
    Image {
        width: i.width,
        height: i.height,
        bgra: i.bgra.clone(),
        readback_ms: 0.,
    }
}
pub(crate) fn same_image(a: &Image, b: &Image, label: &str) -> Result<()> {
    if a.width == b.width && a.height == b.height && a.bgra == b.bgra {
        return Ok(());
    }
    let n = a.bgra.iter().zip(&b.bgra).filter(|(x, y)| x != y).count();
    Err(format!(
        "{label}：实际{}×{}，预期{}×{}，字节差异{n}",
        a.width, a.height, b.width, b.height
    )
    .into())
}
fn pixel(x: i32, y: i32) -> [u8; 4] {
    let (x, y) = (x as u32, y as u32);
    let n = (y.wrapping_mul(157).wrapping_add(x.wrapping_mul(71)))
        ^ (y.wrapping_mul(17).wrapping_add(x.wrapping_mul(113)));
    [
        (n % 251) as u8,
        (y.wrapping_mul(13).wrapping_add(x.wrapping_mul(19)) % 251) as u8,
        (y.wrapping_mul(y).wrapping_add(x.wrapping_mul(41)) % 251) as u8,
        255,
    ]
}
fn fixed(start: bool, x: u32, y: u32, footer: u8) -> [u8; 4] {
    if start {
        [34, (40 + (x + y) % 32) as u8, 218, 255]
    } else {
        [
            207u8.saturating_sub(footer),
            (170 + (x + y) % 32) as u8,
            27,
            255,
        ]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FixturePattern {
    Noise,
    SparsePage,
}
impl FixturePattern {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Noise => "noise",
            Self::SparsePage => "sparse-page",
        }
    }
}
fn sparse_pixel(x: i32, y: i32) -> [u8; 4] {
    // 左侧窄正文列，行间大量留白；每行字形随世界位置变化，避免周期纹理歧义。
    // 初始 y=256 的视口前16行空白，首轮滚动后仍有共同白边，覆盖自动固定栏问题。
    let across = x - ORIGIN - 48;
    let row = y.rem_euclid(64) - 16;
    if !(0..180).contains(&across) || !(0..11).contains(&row) {
        return [248, 248, 248, 255];
    }
    let glyph = across / 9;
    let column = across % 9;
    if column >= 6 {
        return [248, 248, 248, 255];
    }
    let line = y.div_euclid(64) as u32;
    let seed = line.wrapping_mul(0x9e37_79b9) ^ (glyph as u32).wrapping_mul(0x045d_9f3b);
    let stroke = column == 0
        || row == 0
        || row == 10
        || (seed.rotate_left(row as u32 * 3) >> column & 1) != 0;
    let value = if stroke { 45 } else { 248 };
    [value, value, value, 255]
}
pub(crate) fn scene(
    w: u32,
    h: u32,
    axis: ScrollAxis,
    bands: (u32, u32),
    origin: (i32, i32),
    footer: u8,
) -> Image {
    scene_for_pattern(FixturePattern::Noise, w, h, axis, bands, origin, footer)
}
pub(crate) fn scene_for_pattern(
    pattern: FixturePattern,
    w: u32,
    h: u32,
    axis: ScrollAxis,
    bands: (u32, u32),
    origin: (i32, i32),
    footer: u8,
) -> Image {
    let mut bytes = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let (along, length) = match axis {
                ScrollAxis::Vertical => (y, h),
                ScrollAxis::Horizontal => (x, w),
            };
            let p = if along < bands.0 {
                fixed(true, x, y, footer)
            } else if along >= length - bands.1 {
                match axis {
                    ScrollAxis::Vertical => fixed(false, x, along - (length - bands.1), footer),
                    ScrollAxis::Horizontal => fixed(false, along - (length - bands.1), y, footer),
                }
            } else {
                let sample = match pattern {
                    FixturePattern::Noise => pixel,
                    FixturePattern::SparsePage => sparse_pixel,
                };
                match axis {
                    ScrollAxis::Vertical => {
                        sample(origin.0 + x as i32, origin.1 + (y - bands.0) as i32)
                    }
                    ScrollAxis::Horizontal => {
                        sample(origin.0 + (x - bands.0) as i32, origin.1 + y as i32)
                    }
                }
            };
            bytes.extend_from_slice(&p);
        }
    }
    Image {
        width: w,
        height: h,
        bgra: bytes,
        readback_ms: 0.,
    }
}
struct FixtureState {
    native: AtomicUsize,
    x: AtomicI32,
    y: AtomicI32,
    epoch: AtomicU64,
    painted: AtomicU64,
    wheels: AtomicU64,
    wheel_events: Mutex<Vec<WheelEvent>>,
    exited: AtomicBool,
    samples: [AtomicU32; 3],
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct WheelEvent {
    pub elapsed_ms: f64,
    pub delta: i16,
    pub x: i32,
    pub y: i32,
    pub completed_notches: i32,
}
#[derive(Default)]
struct CaptureProbe {
    checks: AtomicU64,
    failures: AtomicU64,
    first_failure: Mutex<Option<String>>,
    last_failure: Mutex<Option<String>>,
    allow_own_excluded: AtomicBool,
    excluded_overlay_checks: AtomicU64,
}
pub(crate) type OwnedCaptureGate = FrameGate;

struct CaptureOwner {
    handle: usize,
    thread: u32,
    class: String,
    region: ScreenRect,
    state: Arc<FixtureState>,
    probe: Arc<CaptureProbe>,
}
impl CaptureOwner {
    fn gate(self: &Arc<Self>, capture_region: ScreenRect) -> Result<FrameGate> {
        validate_capture_region(self.region, capture_region)?;
        let owner = self.clone();
        Ok(Arc::new(move || {
            owner.probe.checks.fetch_add(1, Ordering::AcqRel);
            let result = (|| {
                ensure(
                    !owner.state.exited.load(Ordering::Acquire)
                        && owner.state.native.load(Ordering::Acquire) == owner.handle,
                    "自有采集夹具已退出",
                )?;
                validate_capture_owner(
                    HWND(owner.handle as *mut _),
                    owner.thread,
                    &owner.class,
                    owner.region,
                    capture_region,
                    &owner.probe,
                )
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
        }))
    }
}

/// ROI 门冻结自己的物理矩形；只有完整落在原夹具内才允许建立。
fn validate_capture_region(owner: ScreenRect, capture: ScreenRect) -> Result<()> {
    let owner_right = i64::from(owner.left) + i64::from(owner.width);
    let owner_bottom = i64::from(owner.top) + i64::from(owner.height);
    let right = i64::from(capture.left) + i64::from(capture.width);
    let bottom = i64::from(capture.top) + i64::from(capture.height);
    ensure(
        owner.width > 0
            && owner.height > 0
            && owner.width <= i32::MAX as u32
            && owner.height <= i32::MAX as u32
            && owner_right <= i64::from(i32::MAX)
            && owner_bottom <= i64::from(i32::MAX)
            && capture.width > 0
            && capture.height > 0
            && capture.left >= owner.left
            && capture.top >= owner.top
            && right <= owner_right
            && bottom <= owner_bottom,
        "采集 ROI 未完整落在自有夹具原始物理范围内",
    )
}

fn fixture_class(h: HWND) -> String {
    let mut buffer = [0_u16; 256];
    let length = unsafe { GetClassNameW(h, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length])
}
/// 只检查 Win32 身份与遮挡元数据，不读取屏幕像素，也不要求全局前台焦点。
fn owned_excluded_overlay(h: HWND) -> bool {
    unsafe {
        let mut process = 0;
        GetWindowThreadProcessId(h, Some(&mut process));
        let mut affinity = 0;
        process == std::process::id()
            && GetWindowDisplayAffinity(h, &mut affinity).is_ok()
            && affinity == WDA_EXCLUDEFROMCAPTURE.0
    }
}
fn validate_capture_owner(
    h: HWND,
    thread: u32,
    class: &str,
    region: ScreenRect,
    capture_region: ScreenRect,
    probe: &CaptureProbe,
) -> Result<()> {
    validate_capture_region(region, capture_region)?;
    let capture_right = i64::from(capture_region.left) + i64::from(capture_region.width);
    let capture_bottom = i64::from(capture_region.top) + i64::from(capture_region.height);
    unsafe {
        let mut process = 0;
        let actual_thread = GetWindowThreadProcessId(h, Some(&mut process));
        ensure(
            IsWindow(Some(h)).as_bool()
                && IsWindowVisible(h).as_bool()
                && !IsIconic(h).as_bool()
                && process == std::process::id()
                && actual_thread == thread
                && GetAncestor(h, GA_ROOT) == h
                && fixture_class(h) == class,
            "自有采集窗口已关闭、隐藏或身份改变",
        )?;
        let style = GetWindowLongPtrW(h, GWL_STYLE);
        let extended = GetWindowLongPtrW(h, GWL_EXSTYLE);
        ensure(
            style & WS_POPUP.0 as isize != 0
                && extended & (WS_EX_TOPMOST | WS_EX_NOACTIVATE).0 as isize
                    == (WS_EX_TOPMOST | WS_EX_NOACTIVATE).0 as isize
                && extended & (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize == 0,
            "自有采集窗口不再是不透明的无激活置顶窗口",
        )?;
        let mut affinity = 0;
        GetWindowDisplayAffinity(h, &mut affinity)?;
        ensure(affinity == WDA_NONE.0, "自有窗口已排除于桌面采集")?;
        let (mut client, mut window) = (RECT::default(), RECT::default());
        GetClientRect(h, &mut client)?;
        GetWindowRect(h, &mut window)?;
        let mut origin = POINT::default();
        ClientToScreen(h, &mut origin).ok()?;
        ensure(
            client.left == 0
                && client.top == 0
                && client.right == region.width as i32
                && client.bottom == region.height as i32
                && origin.x == region.left
                && origin.y == region.top
                && window.left == region.left
                && window.top == region.top
                && i64::from(window.right) - i64::from(window.left) == i64::from(region.width)
                && i64::from(window.bottom) - i64::from(window.top) == i64::from(region.height),
            "自有采集窗口移动或客户区物理范围改变",
        )?;
        let monitor = MonitorFromRect(&window, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        ensure(
            !monitor.is_invalid()
                && GetMonitorInfoW(monitor, &mut info).as_bool()
                && window.left >= info.rcMonitor.left
                && window.top >= info.rcMonitor.top
                && window.right <= info.rcMonitor.right
                && window.bottom <= info.rcMonitor.bottom,
            "自有采集矩形没有完整位于同一活动显示器内",
        )?;
        // WindowFromPoint 会跳过禁用/透明窗口；另外检查整个 ROI 上方的 Z 序矩形。
        // 仅 GPUI 专项允许本进程且实际 WDA_EXCLUDEFROMCAPTURE 的操作条/边框。
        // 其它相交可见窗口一律拒绝，不读内容或记录标题、类名、像素。
        let mut previous = GetWindow(h, GW_HWNDPREV).ok();
        let mut visited = std::collections::HashSet::new();
        while let Some(covering) = previous {
            ensure(
                visited.len() < 1024 && visited.insert(covering.0 as usize),
                "窗口 Z 序在校验时变化",
            )?;
            if IsWindowVisible(covering).as_bool() && !IsIconic(covering).as_bool() {
                let mut bounds = RECT::default();
                GetWindowRect(covering, &mut bounds)?;
                if probe.allow_own_excluded.load(Ordering::Acquire)
                    && owned_excluded_overlay(covering)
                {
                    probe.excluded_overlay_checks.fetch_add(1, Ordering::AcqRel);
                    previous = GetWindow(covering, GW_HWNDPREV).ok();
                    continue;
                }
                ensure(
                    !(i64::from(bounds.left) < capture_right
                        && bounds.right > capture_region.left
                        && i64::from(bounds.top) < capture_bottom
                        && bounds.bottom > capture_region.top),
                    "自有采集矩形被上方可见窗口遮挡",
                )?;
            }
            previous = GetWindow(covering, GW_HWNDPREV).ok();
        }
        for y in [
            0,
            capture_region.height / 2,
            capture_region.height.saturating_sub(1),
        ] {
            for x in [
                0,
                capture_region.width / 2,
                capture_region.width.saturating_sub(1),
            ] {
                let hit = WindowFromPoint(POINT {
                    x: (i64::from(capture_region.left) + i64::from(x)) as i32,
                    y: (i64::from(capture_region.top) + i64::from(y)) as i32,
                });
                ensure(
                    hit == h
                        || (probe.allow_own_excluded.load(Ordering::Acquire)
                            && owned_excluded_overlay(hit)),
                    "自有采集矩形的命中窗口改变",
                )?;
            }
        }
        Ok(())
    }
}
struct PaintState {
    pattern: FixturePattern,
    axis: ScrollAxis,
    bands: (u32, u32),
    x: i32,
    y: i32,
    footer: u8,
    reference: Option<Image>,
    limits: (i32, i32),
    animation: Option<(i32, i32, u32)>,
    wheel_remainder: [i32; 2],
}
impl PaintState {
    fn image(&self, r: ScreenRect) -> Image {
        self.reference.as_ref().map_or_else(
            || {
                scene_for_pattern(
                    self.pattern,
                    r.width,
                    r.height,
                    self.axis,
                    self.bands,
                    (self.x, self.y),
                    self.footer,
                )
            },
            copy_image,
        )
    }
}
// WindowProc 不借用绘制状态；外层消息循环独占它。
unsafe extern "system" fn fixture_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_CLOSE {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(h, m, w, l)
}
fn paint(h: HWND, i: &Image) -> Result<()> {
    unsafe {
        let dc = GetDC(Some(h));
        ensure(!dc.is_invalid(), "自绘 DC 创建失败")?;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: i.width as i32,
                biHeight: -(i.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let n = StretchDIBits(
            dc,
            0,
            0,
            i.width as i32,
            i.height as i32,
            0,
            0,
            i.width as i32,
            i.height as i32,
            Some(i.bgra.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        let flush = GdiFlush().ok();
        ReleaseDC(Some(h), dc);
        let _ = ValidateRect(Some(h), None);
        flush?;
        ensure(n > 0, "自有图绘制失败")
    }
}
fn sample_points(r: ScreenRect) -> [(u32, u32); 3] {
    [
        (11, 11),
        (r.width / 3, r.height / 2),
        (r.width - 12, r.height - 12),
    ]
}
fn paint_frame(h: HWND, image: &Image, shared: &FixtureState, epoch: u64) -> Result<()> {
    paint(h, image)?;
    let r = ScreenRect {
        left: 0,
        top: 0,
        width: image.width,
        height: image.height,
    };
    for (i, (x, y)) in sample_points(r).iter().enumerate() {
        let offset = ((y * image.width + x) * 4) as usize;
        let p = &image.bgra[offset..offset + 4];
        shared.samples[i].store(
            p[2] as u32 | ((p[1] as u32) << 8) | ((p[0] as u32) << 16),
            Ordering::Release,
        );
    }
    shared.painted.store(epoch, Ordering::Release);
    Ok(())
}
pub(crate) struct Fixture {
    pub(crate) region: ScreenRect,
    pub(crate) pattern: FixturePattern,
    state: Arc<FixtureState>,
    capture_probe: Arc<CaptureProbe>,
    desktop_probe: bool,
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
}
impl Fixture {
    pub(crate) fn new(
        r: ScreenRect,
        axis: ScrollAxis,
        bands: (u32, u32),
        reference: Option<Image>,
    ) -> Result<Self> {
        Self::new_with_pattern(r, axis, bands, reference, FixturePattern::Noise)
    }
    pub(crate) fn new_with_pattern(
        r: ScreenRect,
        axis: ScrollAxis,
        bands: (u32, u32),
        reference: Option<Image>,
        pattern: FixturePattern,
    ) -> Result<Self> {
        Self::new_inner(r, axis, bands, reference, pattern, true)
    }
    /// 新 DXGI 长图专项直接以真实 ROI 像素作为呈现证据，不先读取桌面 GetPixel。
    pub(crate) fn new_owned(
        r: ScreenRect,
        axis: ScrollAxis,
        bands: (u32, u32),
        pattern: FixturePattern,
    ) -> Result<Self> {
        Self::new_inner(r, axis, bands, None, pattern, false)
    }
    fn new_inner(
        r: ScreenRect,
        axis: ScrollAxis,
        bands: (u32, u32),
        reference: Option<Image>,
        pattern: FixturePattern,
        desktop_probe: bool,
    ) -> Result<Self> {
        let state = Arc::new(FixtureState {
            native: AtomicUsize::new(0),
            x: AtomicI32::new(ORIGIN),
            y: AtomicI32::new(ORIGIN),
            epoch: AtomicU64::new(1),
            painted: AtomicU64::new(0),
            wheels: AtomicU64::new(0),
            wheel_events: Mutex::new(Vec::with_capacity(256)),
            exited: AtomicBool::new(false),
            samples: std::array::from_fn(|_| AtomicU32::new(u32::MAX)),
        });
        let shared = state.clone();
        let (ready, rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("rustshot-long-qa-fixture".into())
            .spawn(move || {
                let result = fixture_thread(r, axis, bands, reference, pattern, &shared, &ready)
                    .map_err(|e| e.to_string());
                shared.exited.store(true, Ordering::Release);
                if let Err(e) = &result {
                    let _ = ready.try_send(Err(e.clone()));
                }
                result
            })?;
        let mut result = Self {
            region: r,
            pattern,
            state,
            capture_probe: Arc::new(CaptureProbe::default()),
            desktop_probe,
            worker: Some(worker),
        };
        if let Err(e) = rx
            .recv_timeout(WAIT)
            .map_err(|e| format!("自绘初始化超时：{e}"))?
        {
            result.close()?;
            return Err(e.into());
        }
        result.presented()?;
        Ok(result)
    }
    fn handle(&self) -> HWND {
        HWND(self.state.native.load(Ordering::Acquire) as *mut _)
    }
    pub(crate) fn native(&self) -> usize {
        self.handle().0 as usize
    }
    pub(crate) fn wheel_count(&self) -> u64 {
        self.state.wheels.load(Ordering::Acquire)
    }
    pub(crate) fn wheel_events(&self) -> Vec<WheelEvent> {
        self.state
            .wheel_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn origin(&self) -> (i32, i32) {
        (
            self.state.x.load(Ordering::Acquire),
            self.state.y.load(Ordering::Acquire),
        )
    }
    pub(crate) fn set_limits(&self, min: i32, max: i32) -> Result<()> {
        ensure(min >= 0 && max >= min && max <= 65_535, "自有滚动边界无效")?;
        let old = self.state.epoch.load(Ordering::Acquire);
        unsafe {
            PostMessageW(
                Some(self.owned()?),
                CHANGE_LIMITS,
                WPARAM(min as usize),
                LPARAM(max as isize),
            )?;
        }
        self.wait_epoch(old)
    }
    fn owned(&self) -> Result<HWND> {
        let h = self.handle();
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(h, Some(&mut pid));
        }
        ensure(
            pid == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
            "自有 HWND 失效",
        )?;
        Ok(h)
    }
    /// 调用方在每次 DXGI 复制/CPU 读回前执行；检查与复制间仍存在普通 Win32 竞态。
    pub(crate) fn capture_gate(&self) -> Result<OwnedCaptureGate> {
        self.capture_owner()?.gate(self.region)
    }
    fn capture_owner(&self) -> Result<Arc<CaptureOwner>> {
        let h = self.owned()?;
        let thread = unsafe { GetWindowThreadProcessId(h, None) };
        let class = fixture_class(h);
        ensure(thread != 0 && !class.is_empty(), "自有采集窗口身份未就绪")?;
        Ok(Arc::new(CaptureOwner {
            handle: h.0 as usize,
            thread,
            class,
            region: self.region,
            state: self.state.clone(),
            probe: self.capture_probe.clone(),
        }))
    }
    /// 固定大夹具不移动，每个候选 ROI 建立独立门并在创建时完成一次真实归属检查。
    pub(crate) fn capture_gate_for_region(&self, capture_region: ScreenRect) -> Result<FrameGate> {
        let gate = self.capture_owner()?.gate(capture_region)?;
        gate()?;
        Ok(gate)
    }
    /// 工厂保留原始 HWND/线程/类/完整客户区身份，不能用旧 ROI 门代替新 ROI 检查。
    pub(crate) fn region_gate_factory(&self) -> Result<RegionGuardFactory> {
        let owner = self.capture_owner()?;
        Ok(Arc::new(move |capture_region| {
            let gate = owner.gate(capture_region)?;
            gate()?;
            Ok(gate)
        }))
    }
    pub(crate) fn capture_gate_with_own_overlays(&self) -> Result<OwnedCaptureGate> {
        self.capture_probe
            .allow_own_excluded
            .store(true, Ordering::Release);
        self.capture_gate()
    }
    pub(crate) fn capture_probe_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "checks":self.capture_probe.checks.load(Ordering::Acquire),
            "failures":self.capture_probe.failures.load(Ordering::Acquire),
            "first_failure":self.capture_probe.first_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "last_failure":self.capture_probe.last_failure.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "foreground_required":false,"desktop_getpixel_probe":self.desktop_probe,
            "checks_include_fixture_readiness":true,"guard_to_copy_atomic":false,
            "allow_own_excluded_overlays":self.capture_probe.allow_own_excluded.load(Ordering::Acquire),
            "excluded_overlay_checks":self.capture_probe.excluded_overlay_checks.load(Ordering::Acquire),
        })
    }
    fn wait_epoch(&self, old: u64) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.state.epoch.load(Ordering::Acquire) > old {
                return self.presented();
            }
            ensure(
                !self.state.exited.load(Ordering::Acquire),
                "自绘线程提前退出",
            )?;
            thread::sleep(Duration::from_millis(5));
        }
        Err("自有滚动消息未处理".into())
    }
    pub(crate) fn wheel(&self, axis: ScrollAxis, n: i16) -> Result<()> {
        let d = match axis {
            ScrollAxis::Vertical => -n * 120,
            ScrollAxis::Horizontal => n * 120,
        };
        let old = self.state.epoch.load(Ordering::Acquire);
        unsafe {
            PostMessageW(
                Some(self.owned()?),
                match axis {
                    ScrollAxis::Vertical => WM_MOUSEWHEEL,
                    ScrollAxis::Horizontal => WM_MOUSEHWHEEL,
                },
                WPARAM((d as u16 as usize) << 16),
                LPARAM(0),
            )?;
        }
        self.wait_epoch(old)
    }
    fn set_scene(&self, axis: ScrollAxis, bands: (u32, u32)) -> Result<()> {
        let old = self.state.epoch.load(Ordering::Acquire);
        unsafe {
            PostMessageW(
                Some(self.owned()?),
                CHANGE_SCENE,
                WPARAM(usize::from(axis == ScrollAxis::Horizontal)),
                LPARAM(bands.0 as isize | ((bands.1 as isize) << 16)),
            )?;
        }
        self.wait_epoch(old)
    }
    fn footer(&self, v: u8) -> Result<()> {
        let old = self.state.epoch.load(Ordering::Acquire);
        unsafe {
            PostMessageW(
                Some(self.owned()?),
                CHANGE_FOOTER,
                WPARAM(v as usize),
                LPARAM(0),
            )?;
        }
        self.wait_epoch(old)
    }
    fn geometry(&self) -> Result<()> {
        let (mut c, mut r) = (RECT::default(), RECT::default());
        unsafe {
            GetClientRect(self.owned()?, &mut c)?;
            GetWindowRect(self.handle(), &mut r)?;
        }
        ensure(
            c.right == self.region.width as i32
                && c.bottom == self.region.height as i32
                && r.left == self.region.left
                && r.top == self.region.top
                && r.right - r.left == self.region.width as i32
                && r.bottom - r.top == self.region.height as i32,
            "自绘图物理范围/DPI不符",
        )
    }
    fn presented(&self) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            self.geometry()?;
            if self.state.painted.load(Ordering::Acquire)
                == self.state.epoch.load(Ordering::Acquire)
                && unsafe { DwmFlush() } >= 0
            {
                // 身份、完整客户区、置顶遮挡先通过，才允许旧专项读取自身 ROI 三点。
                self.capture_gate()?()?;
                if !self.desktop_probe {
                    // 这是 GDI/DWM 排空确认；实际显示像素由随后 guarded DXGI ROI 验证。
                    return Ok(());
                }
                // 仅采样自有矩形内的三点，确认真实DWM显示；不记录颜色/保存屏幕。
                let dc = unsafe { GetDC(None) };
                ensure(!dc.is_invalid(), "不能检查自绘显示")?;
                let ok = sample_points(self.region)
                    .iter()
                    .enumerate()
                    .all(|(i, (x, y))| {
                        (unsafe {
                            GetPixel(
                                dc,
                                self.region.left + *x as i32,
                                self.region.top + *y as i32,
                            )
                        }) == COLORREF(self.state.samples[i].load(Ordering::Acquire))
                    });
                unsafe {
                    ReleaseDC(None, dc);
                }
                if ok {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err("自有图未实际绘制/呈现".into())
    }
    pub(crate) fn close(&mut self) -> Result<()> {
        if !self.state.exited.load(Ordering::Acquire) {
            if let Ok(h) = self.owned() {
                unsafe {
                    PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0))?;
                }
            }
        }
        let deadline = Instant::now() + WAIT;
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        ensure(
            self.worker.as_ref().is_none_or(JoinHandle::is_finished),
            "自绘线程未退出，未强制终止",
        )?;
        if let Some(w) = self.worker.take() {
            w.join()
                .map_err(|_| "自绘线程panic")?
                .map_err(|e| e.to_string())?;
        }
        ensure(
            self.state.exited.load(Ordering::Acquire)
                && !unsafe { IsWindow(Some(self.handle())) }.as_bool(),
            "自绘 HWND/线程仍存活",
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn fixture_thread(
    r: ScreenRect,
    axis: ScrollAxis,
    bands: (u32, u32),
    reference: Option<Image>,
    pattern: FixturePattern,
    shared: &FixtureState,
    ready: &mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<()> {
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let name: Vec<u16> = format!(
            "RustShot.LongQa.{}.{}",
            std::process::id(),
            GetCurrentThreadId()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        ensure(
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(fixture_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            }) != 0,
            "自绘class注册失败",
        )?;
        let result = (|| -> Result<()> {
            let h = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(name.as_ptr()),
                w!("RustShot · 自有滚动验收图"),
                WS_POPUP,
                r.left,
                r.top,
                r.width as i32,
                r.height as i32,
                None,
                None,
                Some(instance),
                None,
            )?;
            shared.native.store(h.0 as usize, Ordering::Release);
            let body = (|| -> Result<()> {
                let mut state = PaintState {
                    pattern,
                    axis,
                    bands,
                    x: ORIGIN,
                    y: ORIGIN,
                    footer: 0,
                    reference,
                    limits: (0, 8192),
                    animation: None,
                    wheel_remainder: [0, 0],
                };
                let received_since = Instant::now();
                SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    r.left,
                    r.top,
                    r.width as i32,
                    r.height as i32,
                    SWP_SHOWWINDOW | SWP_NOACTIVATE,
                )?;
                paint_frame(h, &state.image(r), shared, 1)?;
                ready.send(Ok(())).map_err(|_| "自绘观察者已退出")?;
                let mut msg = MSG::default();
                loop {
                    let status = GetMessageW(&mut msg, None, 0, 0).0;
                    if status < 0 {
                        return Err("自绘GetMessage失败".into());
                    }
                    if status == 0 {
                        break;
                    }
                    let mut changed = false;
                    match msg.message {
                        WM_MOUSEWHEEL | WM_MOUSEHWHEEL if state.reference.is_none() => {
                            shared.wheels.fetch_add(1, Ordering::AcqRel);
                            let delta = (msg.wParam.0 >> 16) as u16 as i16;
                            let axis = usize::from(msg.message == WM_MOUSEHWHEEL);
                            state.wheel_remainder[axis] += i32::from(delta);
                            let n = state.wheel_remainder[axis] / 120;
                            state.wheel_remainder[axis] %= 120;
                            let (mut x, mut y, _) =
                                state.animation.unwrap_or((state.x, state.y, 0));
                            if msg.message == WM_MOUSEWHEEL {
                                y = (y - STEP * n).clamp(state.limits.0, state.limits.1);
                            } else {
                                x = (x + STEP * n).clamp(state.limits.0, state.limits.1);
                            }
                            let mut events = shared
                                .wheel_events
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if events.len() < 256 {
                                events.push(WheelEvent {
                                    elapsed_ms: received_since.elapsed().as_secs_f64() * 1000.,
                                    delta,
                                    x,
                                    y,
                                    completed_notches: n,
                                });
                            }
                            drop(events);
                            if n == 0 {
                                // 模拟仅累计满整格才响应的原生控件；半格不得触发重绘/动画。
                            } else if state.pattern == FixturePattern::SparsePage {
                                state.animation = Some((x, y, 4));
                                // 140ms 动画超过80ms采集间隔，真实采样会遇到中间帧。
                                ensure(
                                    SetTimer(Some(h), SCROLL_ANIMATION_TIMER, 35, None) != 0,
                                    "自有页面滚动动画计时器失败",
                                )?;
                            } else {
                                state.x = x;
                                state.y = y;
                                changed = true;
                            }
                        }
                        WM_TIMER if msg.wParam.0 == SCROLL_ANIMATION_TIMER => {
                            if let Some((x, y, remaining)) = state.animation {
                                state.x += (x - state.x) / remaining as i32;
                                state.y += (y - state.y) / remaining as i32;
                                state.animation = (remaining > 1).then_some((x, y, remaining - 1));
                                if state.animation.is_none() {
                                    KillTimer(Some(h), SCROLL_ANIMATION_TIMER)?;
                                }
                                changed = true;
                            }
                        }
                        CHANGE_LIMITS if state.reference.is_none() => {
                            state.wheel_remainder = [0, 0];
                            state.animation = None;
                            let _ = KillTimer(Some(h), SCROLL_ANIMATION_TIMER);
                            state.limits = (msg.wParam.0 as i32, msg.lParam.0 as i32);
                            state.x = state.x.clamp(state.limits.0, state.limits.1);
                            state.y = state.y.clamp(state.limits.0, state.limits.1);
                            changed = true;
                        }
                        CHANGE_SCENE if state.reference.is_none() => {
                            state.wheel_remainder = [0, 0];
                            state.animation = None;
                            let _ = KillTimer(Some(h), SCROLL_ANIMATION_TIMER);
                            state.axis = if msg.wParam.0 == 0 {
                                ScrollAxis::Vertical
                            } else {
                                ScrollAxis::Horizontal
                            };
                            state.bands = (
                                msg.lParam.0 as u16 as u32,
                                (msg.lParam.0 >> 16) as u16 as u32,
                            );
                            state.x = ORIGIN;
                            state.y = ORIGIN;
                            state.footer = 0;
                            changed = true;
                        }
                        CHANGE_FOOTER if state.reference.is_none() => {
                            state.footer = msg.wParam.0 as u8;
                            changed = true;
                        }
                        WM_PAINT => paint_frame(
                            h,
                            &state.image(r),
                            shared,
                            shared.epoch.load(Ordering::Acquire),
                        )?,
                        _ => {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    if changed {
                        shared.x.store(state.x, Ordering::Release);
                        shared.y.store(state.y, Ordering::Release);
                        let epoch = shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
                        paint_frame(h, &state.image(r), shared, epoch)?;
                    }
                }
                Ok(())
            })();
            let destroyed = DestroyWindow(h);
            body.and(destroyed.map_err(Into::into))
        })();
        let _ = UnregisterClassW(PCWSTR(name.as_ptr()), Some(instance));
        result
    }
}
pub(crate) fn work_area() -> Result<RECT> {
    unsafe extern "system" fn visit(
        h: HMONITOR,
        _: HDC,
        _: *mut RECT,
        p: LPARAM,
    ) -> windows::core::BOOL {
        let areas = &mut *(p.0 as *mut Vec<RECT>);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(h, &mut info).as_bool() {
            areas.push(info.rcWork);
        }
        windows::core::BOOL(1)
    }
    let mut areas = Vec::<RECT>::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM((&mut areas as *mut Vec<RECT>) as isize),
        )
        .ok()?;
    }
    areas
        .into_iter()
        .find(|r| r.right - r.left >= 1280 && r.bottom - r.top >= 700)
        .ok_or_else(|| "工作区不能容纳自有滚动图及独立参考图".into())
}
pub(crate) fn source_region(w: RECT) -> ScreenRect {
    ScreenRect {
        left: w.left + 32,
        top: w.top + 32,
        width: WIDTH,
        height: HEIGHT,
    }
}
pub(crate) fn options(axis: ScrollAxis, bands: (u32, u32)) -> LongOptions {
    LongOptions {
        stitch: StitchOptions {
            axis,
            detect_fixed_borders: false,
            fixed_start: Some(bands.0),
            fixed_end: Some(bands.1),
        },
        poll_interval: POLL,
    }
}
fn settled(s: &LongSnapshot) -> bool {
    !s.busy && !s.capture_in_flight && s.generation == s.requested_generation
}
fn wait(
    s: &LongSession,
    label: &str,
    predicate: impl Fn(&LongSnapshot) -> bool,
) -> Result<LongSnapshot> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let m = s.snapshot();
        if predicate(&m) {
            return Ok(m);
        }
        if m.phase == LongPhase::Failed {
            return Err(format!("{label}：{}", m.status).into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    let m = s.snapshot();
    Err(format!("{label}未收敛：{:?} busy={} inflight={} gen={}/{} polls={} accepted={} {}×{} viewport={:?} {}",
        m.phase,m.busy,m.capture_in_flight,m.generation,m.requested_generation,m.poll_count,m.accepted_frames,
        m.width,m.height,m.viewport,m.status).into())
}
fn pause(s: &mut LongSession) -> Result<StitchSnapshot> {
    s.set_paused(true)?;
    check_preview(s)?;
    wait(s, "暂停确认", |m| {
        settled(m) && m.phase == LongPhase::Paused
    })?;
    ensure(s.preview_frame().is_some(), "稳定长图缺少可显示预览")?;
    check_preview(s)?;
    s.stitch_snapshot().ok_or_else(|| "暂停没有稳定结果".into())
}
fn check_preview(s: &LongSession) -> Result<()> {
    let requested = s.snapshot().requested_generation;
    if let Some(p) = s.preview_frame() {
        ensure(p.generation == requested, "新请求暴露旧代预览")?;
        let (max_width, max_height) = match p.axis {
            ScrollAxis::Vertical => (200, 700),
            ScrollAxis::Horizontal => (700, 200),
        };
        ensure(
            p.image.width <= max_width
                && p.image.height <= max_height
                && p.source_rect.valid(p.width, p.height),
            "采集预览超出有界尺寸",
        )?;
        let m = s.snapshot();
        if settled(&m)
            && matches!(
                m.phase,
                LongPhase::Paused | LongPhase::Stopped | LongPhase::Failed
            )
        {
            ensure(
                p.width == m.width && p.height == m.height && p.viewport == m.viewport,
                "预览与其来源长图尺寸/视口不一致",
            )?;
        }
    }
    Ok(())
}
pub(crate) fn stitched_image(s: &StitchSnapshot) -> Result<Image> {
    ensure(
        s.width as u64 * s.height as u64 <= 32_000_000,
        "专项图片越界",
    )?;
    let mut i = Image {
        width: s.width,
        height: s.height,
        bgra: vec![0; s.width as usize * s.height as usize * 4],
        readback_ms: 0.,
    };
    for part in &s.segments {
        let r = part.source;
        ensure(
            r.valid(part.image.width, part.image.height)
                && part.origin.0 + r.width <= s.width
                && part.origin.1 + r.height <= s.height,
            "分段坐标越界",
        )?;
        for y in 0..r.height {
            let from = (((r.y + y) * part.image.width + r.x) * 4) as usize;
            let to = (((part.origin.1 + y) * s.width + part.origin.0) * 4) as usize;
            i.bgra[to..to + r.width as usize * 4]
                .copy_from_slice(&part.image.bgra[from..from + r.width as usize * 4]);
        }
    }
    ensure(
        i.bgra.chunks_exact(4).all(|p| p[3] == 255),
        "拼接区块存在缺口/透明像素",
    )?;
    Ok(i)
}
// 独立窗口绘制已知完整结果，再经真实DXGI读取；HDR系统映射同时作用于源/参考。
pub(crate) fn reference_pixels(w: RECT, image: Image) -> Result<Image> {
    let r = ScreenRect {
        left: w.left + WIDTH as i32 + 64,
        top: w.top + 32,
        width: image.width,
        height: image.height,
    };
    ensure(
        r.left + r.width as i32 <= w.right && r.top + r.height as i32 <= w.bottom,
        "参考图超出自有范围",
    )?;
    let mut fixture = Fixture::new(r, ScrollAxis::Vertical, (0, 0), Some(image))?;
    let result = LiveCapture::new()?.read(r)?;
    fixture.close()?;
    Ok(result)
}
struct SessionGuard {
    session: LongSession,
    exited: Arc<AtomicBool>,
    baseline: usize,
}
impl SessionGuard {
    fn start(r: ScreenRect, o: LongOptions) -> Result<Self> {
        let baseline = long_capture_runtime::active_workers();
        let session = LongSession::start(r, o)?;
        let exited = session.worker_probe();
        Ok(Self {
            session,
            exited,
            baseline,
        })
    }
    fn close(&mut self) -> Result<()> {
        self.session.stop();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.exited.load(Ordering::Acquire)
                && long_capture_runtime::active_workers() == self.baseline
            {
                if !self.session.try_join_stopped() {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let m = self.session.snapshot();
                return ensure(
                    m.worker_exited && !m.worker_alive && !m.capture_in_flight,
                    "退出标志早于资源回收",
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err("采集线程/DXGI未真正退出；未强制结束".into())
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
struct Qa {
    root: PathBuf,
    file: File,
    started: Instant,
}
impl Qa {
    fn new() -> Result<Self> {
        let root = std::env::var_os("RUSTSHOT_LONG_QA_DIR")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?.join(format!(
                "target/qa/long-capture/session-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
            )));
        std::fs::create_dir_all(root.join("settings"))?;
        std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
        std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
        let file = File::create(root.join("ui.jsonl"))?;
        Ok(Self {
            root,
            file,
            started: Instant::now(),
        })
    }
    fn record(&mut self, name: &str, detail: serde_json::Value, ok: bool) -> Result<()> {
        let row = serde_json::json!({"suite":"long-capture","test":name,"pid":std::process::id(),
            "elapsed_ms":self.started.elapsed().as_secs_f64()*1000.,"detail":detail,"owned_only":true,"ok":ok});
        println!("{row}");
        writeln!(self.file, "{row}")?;
        self.file.flush()?;
        Ok(())
    }
}
fn vertical_case(qa: &mut Qa, w: RECT) -> Result<()> {
    let mut fixture = Fixture::new(source_region(w), ScrollAxis::Vertical, (0, 0), None)?;
    let mut guard = SessionGuard::start(fixture.region, options(ScrollAxis::Vertical, (0, 0)))?;
    let s = &mut guard.session;
    wait(s, "纵向初帧", |m| {
        settled(m) && m.phase == LongPhase::Running && m.height == HEIGHT
    })?;
    for (n, h, y) in [
        (1, 392, 32),
        (1, 424, 64),
        (-1, 424, 32),
        (-1, 424, 0),
        (-1, 456, 0),
        (1, 456, 32),
    ] {
        fixture.wheel(ScrollAxis::Vertical, n)?;
        wait(s, "真实纵向滚动/回访", |m| {
            settled(m) && m.height == h && m.viewport.y == y
        })?;
    }
    let before = s.snapshot();
    wait(s, "不变帧", |m| {
        m.poll_count >= before.poll_count + 3
            && m.accepted_frames == before.accepted_frames
            && m.height == 456
            && m.last_result == UpdateKind::Unchanged
    })?;
    let snapshot = pause(s)?;
    let actual = stitched_image(&snapshot)?;
    guard.close()?;
    let expected = reference_pixels(
        w,
        scene(
            WIDTH,
            456,
            ScrollAxis::Vertical,
            (0, 0),
            (ORIGIN, ORIGIN - STEP),
            0,
        ),
    )?;
    same_image(&actual, &expected, "纵向双向/回访像素")?;
    ensure(
        snapshot.viewport
            == Rect {
                x: 0,
                y: 32,
                width: WIDTH,
                height: HEIGHT,
            },
        "回访位置错误",
    )?;
    fixture.close()?;
    qa.record("vertical_bidirectional_revisit_unchanged",serde_json::json!({"width":actual.width,
        "height":actual.height,"polls":before.poll_count,"accepted":before.accepted_frames,"workers_recycled":true}),true)
}
fn horizontal_case(qa: &mut Qa, w: RECT, auto: bool) -> Result<()> {
    let mut fixture = Fixture::new(source_region(w), ScrollAxis::Horizontal, BANDS, None)?;
    let mut o = options(ScrollAxis::Horizontal, BANDS);
    if auto {
        o.stitch.detect_fixed_borders = true;
        o.stitch.fixed_start = None;
        o.stitch.fixed_end = None;
    }
    let mut guard = SessionGuard::start(fixture.region, o)?;
    let s = &mut guard.session;
    wait(s, "横向初帧", |m| {
        settled(m) && m.phase == LongPhase::Running && m.width == WIDTH
    })?;
    for (n, width, x) in [(1, 512, 56), (1, 544, 88), (-1, 544, 56)] {
        fixture.wheel(ScrollAxis::Horizontal, n)?;
        wait(s, "真实横向滚动", |m| {
            settled(m) && m.width == width && m.viewport.x == x
        })?;
    }
    let initial = stitched_image(&pause(s)?)?;
    let accepted = s.snapshot().accepted_frames;
    fixture.footer(41)?;
    s.capture_once()?;
    wait(s, "固定尾栏更新", |m| {
        settled(m)
            && m.phase == LongPhase::Paused
            && m.width == 544
            && m.accepted_frames == accepted + 1
            && m.last_result == UpdateKind::Added
            && m.last_delta == 0
    })?;
    let actual = stitched_image(&s.stitch_snapshot().ok_or("尾栏更新丢结果")?)?;
    guard.close()?;
    let initial_expected = reference_pixels(
        w,
        scene(
            544,
            HEIGHT,
            ScrollAxis::Horizontal,
            BANDS,
            (ORIGIN, ORIGIN),
            0,
        ),
    )?;
    same_image(&initial, &initial_expected, "横向固定栏只保留一次")?;
    let expected = reference_pixels(
        w,
        scene(
            544,
            HEIGHT,
            ScrollAxis::Horizontal,
            BANDS,
            (ORIGIN, ORIGIN),
            41,
        ),
    )?;
    same_image(&actual, &expected, "尾栏更新不增长")?;
    fixture.close()?;
    qa.record(
        if auto {
            "horizontal_auto_fixed_borders"
        } else {
            "horizontal_manual_fixed_borders"
        },
        serde_json::json!({"width":actual.width,"height":actual.height,"footer_only_no_growth":true,
            "workers_recycled":true}),
        true,
    )
}
fn controls_case(qa: &mut Qa, w: RECT) -> Result<()> {
    let mut fixture = Fixture::new(source_region(w), ScrollAxis::Vertical, (0, 0), None)?;
    let mut guard = SessionGuard::start(fixture.region, options(ScrollAxis::Vertical, (0, 0)))?;
    let s = &mut guard.session;
    wait(s, "控制初帧", |m| {
        settled(m) && m.phase == LongPhase::Running && m.height == HEIGHT
    })?;
    fixture.wheel(ScrollAxis::Vertical, 1)?;
    wait(s, "增长", |m| settled(m) && m.height == 392)?;
    let before = stitched_image(&pause(s)?)?;
    let paused = s.snapshot();
    fixture.wheel(ScrollAxis::Vertical, 1)?;
    thread::sleep(POLL * 3);
    let after = s.snapshot();
    ensure(
        after.poll_count == paused.poll_count
            && after.accepted_frames == paused.accepted_frames
            && settled(&after),
        "暂停ACK后仍采集或变更结果",
    )?;
    same_image(
        &stitched_image(&s.stitch_snapshot().ok_or("暂停丢结果")?)?,
        &before,
        "暂停时源继续滚动",
    )?;
    s.capture_once()?;
    check_preview(s)?;
    wait(s, "单次", |m| {
        settled(m) && m.phase == LongPhase::Paused && m.height == 424
    })?;
    ensure(s.snapshot().can_undo, "新增帧没有撤销")?;
    s.undo()?;
    wait(s, "撤销", |m| {
        settled(m) && m.phase == LongPhase::Paused && m.height == 392
    })?;
    same_image(
        &stitched_image(&s.stitch_snapshot().ok_or("撤销丢结果")?)?,
        &before,
        "采集撤销",
    )?;
    s.capture_once()?;
    wait(s, "撤销后继续", |m| {
        settled(m) && m.phase == LongPhase::Paused && m.height == 424
    })?;
    let generation = s.snapshot().generation;
    fixture.set_scene(ScrollAxis::Horizontal, (0, 0))?;
    s.restart(options(ScrollAxis::Horizontal, (0, 0)))?;
    check_preview(s)?;
    wait(s, "切轴重启", |m| {
        settled(m)
            && m.phase == LongPhase::Running
            && m.axis == ScrollAxis::Horizontal
            && m.width == WIDTH
            && m.height == HEIGHT
            && m.generation > generation
    })?;
    fixture.wheel(ScrollAxis::Horizontal, 1)?;
    wait(s, "切轴增长", |m| {
        settled(m) && m.width == 512 && m.height == HEIGHT
    })?;
    let actual = stitched_image(&pause(s)?)?;
    let generation = s.snapshot().generation;
    guard.close()?;
    let expected = reference_pixels(
        w,
        scene(
            512,
            HEIGHT,
            ScrollAxis::Horizontal,
            (0, 0),
            (ORIGIN, ORIGIN),
            0,
        ),
    )?;
    same_image(&actual, &expected, "重启不得串入旧轴帧")?;
    fixture.close()?;
    qa.record(
        "pause_once_undo_restart_generation",
        serde_json::json!({"generation":generation,
        "old_pixels_preserved_when_paused":true,"workers_recycled":true}),
        true,
    )
}
fn lifecycle_case(qa: &mut Qa, w: RECT) -> Result<()> {
    let mut fixture = Fixture::new(source_region(w), ScrollAxis::Vertical, (0, 0), None)?;
    let baseline = long_capture_runtime::active_workers();
    let mut first = SessionGuard::start(fixture.region, options(ScrollAxis::Vertical, (0, 0)))?;
    first.close()?;
    ensure(
        long_capture_runtime::active_workers() == baseline,
        "初始化取消遗留线程",
    )?;
    let mut in_flight = SessionGuard::start(fixture.region, options(ScrollAxis::Vertical, (0, 0)))?;
    wait(&in_flight.session, "真实采集开始", |m| {
        m.capture_in_flight
    })?;
    in_flight.close()?;
    ensure(
        long_capture_runtime::active_workers() == baseline,
        "采集中取消遗留线程",
    )?;
    let mut invalid = SessionGuard::start(
        ScreenRect {
            left: i32::MIN / 2,
            top: i32::MIN / 2,
            width: WIDTH,
            height: HEIGHT,
        },
        options(ScrollAxis::Vertical, (0, 0)),
    )?;
    wait(&invalid.session, "屏外真实初始化失败", |m| {
        m.phase == LongPhase::Failed && !m.capture_in_flight
    })?;
    ensure(
        invalid.session.stitch_snapshot().is_none(),
        "首次失败产生虚构图片",
    )?;
    invalid.close()?;
    let session = LongSession::start(fixture.region, options(ScrollAxis::Vertical, (0, 0)))?;
    let exited = session.worker_probe();
    wait(&session, "Drop初帧", |m| {
        settled(m) && m.phase == LongPhase::Running
    })?;
    drop(session);
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && (!exited.load(Ordering::Acquire) || long_capture_runtime::active_workers() != baseline)
    {
        thread::sleep(Duration::from_millis(5));
    }
    ensure(
        exited.load(Ordering::Acquire) && long_capture_runtime::active_workers() == baseline,
        "Drop只请求停止而没有回收线程",
    )?;
    fixture.close()?;
    qa.record(
        "initial_cancel_failure_drop_recycle",
        serde_json::json!({"baseline_workers":baseline,
        "workers":long_capture_runtime::active_workers(),"worker_probe_exited":true}),
        true,
    )
}
pub(crate) fn crop_image(i: &Image, r: Rect) -> Result<Image> {
    ensure(r.valid(i.width, i.height), "参考裁剪错误")?;
    let mut bytes = Vec::new();
    for y in r.y..r.y + r.height {
        let start = ((y * i.width + r.x) * 4) as usize;
        bytes.extend_from_slice(&i.bgra[start..start + r.width as usize * 4]);
    }
    Ok(Image {
        width: r.width,
        height: r.height,
        bgra: bytes,
        readback_ms: 0.,
    })
}
fn document_case(qa: &mut Qa) -> Result<()> {
    let (width, height) = (80, 18_000);
    let mut segments = Vec::new();
    for start in (0..height).step_by(6000) {
        let image = scene(
            width,
            6000,
            ScrollAxis::Vertical,
            (0, 0),
            (ORIGIN, ORIGIN + start as i32),
            0,
        );
        segments.push(StitchSegment {
            image: Arc::new(image),
            source: Rect::full(width, 6000),
            origin: (0, start),
        });
    }
    let snapshot = StitchSnapshot {
        width,
        height,
        axis: ScrollAxis::Vertical,
        viewport: Rect::full(width, 360),
        segments,
    };
    let mut doc = crate::long_document::LongDocument::new(snapshot)?;
    let small = crate::long_document::preview(&doc.snapshot(), 320, 600)?;
    ensure(
        small.width > 0 && small.height > 0 && small.width <= 320 && small.height <= 600,
        "长图预览突破纹理边界",
    )?;
    ensure(
        small.bgra.chunks_exact(4).all(|p| p[3] == 255),
        "长图预览存在缺口",
    )?;
    ensure(
        doc.crop(Rect {
            x: 8,
            y: 17_000,
            width: 64,
            height: 600,
        })?,
        "长图裁剪未提交",
    )?;
    let baseline = scene(
        64,
        600,
        ScrollAxis::Vertical,
        (0, 0),
        (ORIGIN + 8, ORIGIN + 17_000),
        0,
    );
    same_image(&doc.export()?, &baseline, "超过16K长图裁剪")?;
    ensure(doc.cut(120, 152)?, "删带未提交")?;
    let mut expected = crop_image(
        &baseline,
        Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 120,
        },
    )?;
    expected
        .bgra
        .extend_from_slice(&baseline.bgra[152 * 64 * 4..]);
    expected.height = 568;
    same_image(&doc.export()?, &expected, "删带像素")?;
    ensure(doc.undo(), "删带不能撤销")?;
    same_image(&doc.export()?, &baseline, "删带撤销")?;
    ensure(doc.redo(), "删带不能重做")?;
    same_image(&doc.export()?, &expected, "删带重做")?;
    ensure(doc.reset() && doc.height() == height, "重置未恢复原长图")?;
    ensure(doc.undo(), "重置不能撤销")?;
    same_image(&doc.export()?, &expected, "重置撤销")?;
    let output = qa.root.join("owned-long-crop.png");
    crate::export::save_png(copy_image(&expected), &output)?;
    let decoded = image::open(&output)?.into_rgba8();
    let (decoded_width, decoded_height) = decoded.dimensions();
    let mut bytes = decoded.into_raw();
    for p in bytes.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    same_image(
        &Image {
            width: decoded_width,
            height: decoded_height,
            bgra: bytes,
            readback_ms: 0.,
        },
        &expected,
        "自有PNG像素",
    )?;
    document_boundaries(qa)?;
    qa.record(
        "document_large_bounded_crop_cut_history_png",
        serde_json::json!({"original_height":height,
        "preview_width":small.width,"preview_height":small.height,"saved_owned_only":true}),
        true,
    )
}
fn cut_image(image: &Image, axis: ScrollAxis, start: u32, end: u32) -> Image {
    let (w, h) = match axis {
        ScrollAxis::Vertical => (image.width, image.height - (end - start)),
        ScrollAxis::Horizontal => (image.width - (end - start), image.height),
    };
    let mut bgra = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..image.height {
        for x in 0..image.width {
            let coordinate = match axis {
                ScrollAxis::Vertical => y,
                ScrollAxis::Horizontal => x,
            };
            if coordinate >= start && coordinate < end {
                continue;
            }
            let offset = ((y * image.width + x) * 4) as usize;
            bgra.extend_from_slice(&image.bgra[offset..offset + 4]);
        }
    }
    Image {
        width: w,
        height: h,
        bgra,
        readback_ms: 0.,
    }
}
fn document_boundaries(qa: &mut Qa) -> Result<()> {
    use crate::long_document::LongDocument;
    let original = scene(
        1200,
        64,
        ScrollAxis::Horizontal,
        (0, 0),
        (ORIGIN, ORIGIN),
        0,
    );
    let source = Arc::new(copy_image(&original));
    let segments = (0..1200)
        .step_by(400)
        .map(|x| StitchSegment {
            image: source.clone(),
            source: Rect {
                x,
                y: 0,
                width: 400,
                height: 64,
            },
            origin: (x, 0),
        })
        .collect();
    let mut document = LongDocument::new(StitchSnapshot {
        width: 1200,
        height: 64,
        axis: ScrollAxis::Horizontal,
        viewport: Rect::full(360, 64),
        segments,
    })?;
    ensure(
        !document.crop(Rect::full(1200, 64))?
            && !document.cut(200, 200)?
            && !document.reset()
            && !document.can_undo(),
        "无变化选区错误增加历史",
    )?;
    let crop = Rect {
        x: 200,
        y: 8,
        width: 800,
        height: 48,
    };
    ensure(document.crop(crop)?, "横向裁剪未提交")?;
    let cropped = crop_image(&original, crop)?;
    same_image(&document.export()?, &cropped, "水平裁剪")?;
    ensure(document.cut(100, 132)?, "横向删带未提交")?;
    let first = cut_image(&cropped, ScrollAxis::Horizontal, 100, 132);
    ensure(document.cut(300, 348)?, "连续横向删带未提交")?;
    let second = cut_image(&first, ScrollAxis::Horizontal, 300, 348);
    same_image(&document.export()?, &second, "连续水平删带")?;
    ensure(document.undo(), "连续删带撤销失败")?;
    same_image(&document.export()?, &first, "撤销第二删带")?;
    ensure(document.redo(), "连续删带重做失败")?;
    same_image(&document.export()?, &second, "重做第二删带")?;
    ensure(document.reset() && document.undo(), "水平还原不能撤销")?;
    same_image(&document.export()?, &second, "水平还原撤销")?;
    let shared = Arc::new(Image {
        width: 1,
        height: 1000,
        bgra: [20, 50, 100, 255].repeat(1000),
        readback_ms: 0.,
    });
    let tall = StitchSnapshot {
        width: 1,
        height: 100_000,
        axis: ScrollAxis::Vertical,
        viewport: Rect::full(1, 360),
        segments: (0..100_000)
            .step_by(1000)
            .map(|y| StitchSegment {
                image: shared.clone(),
                source: Rect::full(1, 1000),
                origin: (0, y),
            })
            .collect(),
    };
    let mut document = LongDocument::new(tall)?;
    let preview = crate::long_document::preview(&document.snapshot(), 400, 600)?;
    ensure(
        preview.width <= 400 && preview.height <= 600,
        "十万行共享源预览越界",
    )?;
    ensure(document.export().is_err(), "十万行源突破导出单边上限")?;
    ensure(
        crate::long_document::preview(&document.snapshot(), 0, 600).is_err()
            && crate::long_document::preview(&document.snapshot(), 400, 0).is_err(),
        "零预览budget未拒绝",
    )?;
    ensure(
        document.crop(Rect {
            x: 0,
            y: 10_000,
            width: 1,
            height: 60_000,
        })?,
        "大源无法裁短",
    )?;
    let exported = document.export()?;
    ensure(
        exported.height == 60_000
            && exported
                .bgra
                .chunks_exact(4)
                .all(|p| p == [20, 50, 100, 255]),
        "共享源裁短导出像素不符",
    )?;
    qa.record(
        "document_horizontal_multicut_shared_100k_limits",
        serde_json::json!({
        "shared_original_height":100_000,"cropped_export_height":exported.height,
        "preview_width":preview.width,"preview_height":preview.height,"zero_budget_rejected":true}),
        true,
    )
}
pub(crate) fn run() -> Result<()> {
    crate::remote_display::force_local_for_smoke();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut qa = Qa::new()?;
    let h = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
    let _lock = crate::gpui_capture::CaptureLock::new(h);
    ensure(
        unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
        "已有截图会话，未继续专项",
    )?;
    let work = work_area()?;
    let filter = std::env::var("RUSTSHOT_LONG_QA_CASE").unwrap_or_else(|_| "all".into());
    let result = (|| -> Result<()> {
        if filter == "all" || filter == "vertical" {
            vertical_case(&mut qa, work)?;
        }
        if filter == "all" || filter == "horizontal" {
            horizontal_case(&mut qa, work, false)?;
            horizontal_case(&mut qa, work, true)?;
        }
        if filter == "all" || filter == "controls" {
            controls_case(&mut qa, work)?;
        }
        if filter == "all" || filter == "lifecycle" {
            lifecycle_case(&mut qa, work)?;
        }
        if filter == "all" || filter == "document" {
            document_case(&mut qa)?;
        }
        ensure(
            matches!(
                filter.as_str(),
                "all" | "vertical" | "horizontal" | "controls" | "lifecycle" | "document"
            ),
            "未知专项case",
        )?;
        ensure(
            long_capture_runtime::active_workers() == 0,
            "专项结束仍有工作线程",
        )?;
        qa.record(
            "long_capture_recycled",
            serde_json::json!({"workers":0,"clipboard_untouched":true}),
            true,
        )
    })();
    if let Err(e) = &result {
        let _ = qa.record(
            "long_capture_failure",
            serde_json::json!({"message":e.to_string()}),
            false,
        );
    }
    result
}
