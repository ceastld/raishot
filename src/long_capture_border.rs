//! 长截图选区边框与暂停缩放柄。只占窄边和八个小窗口，不创建全屏遮罩或采集线程。
use crate::{
    geometry::ScreenRect,
    long_region::RegionHandle,
    long_region_input::{NativeRegionInput, RegionInputEvent},
    Result,
};
use std::{
    cell::Cell,
    collections::HashSet,
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, InvalidateRect, HDC},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::*,
    },
};

const CLASS_NAME: PCWSTR = w!("RustShot.LongCapture.Border");
const HUD_PROPERTY: PCWSTR = w!("RustShot.LongCapture.Border.Instance.v1");
const LINE_WIDTH: u32 = 2;
const HANDLE_SIDE: u32 = 10;
/// 录屏选区不画缩放点：整条边与四角是不可见的拖动区，悬停只换光标（同窗口边框）。
const EDGE_CORNER: u32 = 14;
const EDGE_THICKNESS: u32 = 6;
const RESIZE_KINDS: [RegionHandle; 8] = [
    RegionHandle::North,
    RegionHandle::NorthEast,
    RegionHandle::East,
    RegionHandle::SouthEast,
    RegionHandle::South,
    RegionHandle::SouthWest,
    RegionHandle::West,
    RegionHandle::NorthWest,
];
// COLORREF 为 0x00BBGGRR；选区边框、缩放柄与预览当前页面统一使用 #32BE69。
const LINE_COLOR: COLORREF = COLORREF(0x0069be32);
/// GWLP_USERDATA 高位标记自定义颜色；未设置时沿用类画刷。
const CUSTOM_COLOR: u32 = 0x0100_0000;

#[derive(Clone)]
struct HudWindow {
    hwnd: usize,
    pid: u32,
    thread: u32,
    class_name: String,
}
struct HudState {
    region: ScreenRect,
    windows: Vec<HudWindow>,
}
struct HudLifetime {
    closed: AtomicBool,
    state: Mutex<HudState>,
}
/// 仅原 Border 能创建此描述符；Weak 不延长 HWND、像素或 GPU 资源寿命。
/// Weak 留住原分配地址，窗口属性不能因 HWND/分配器复用冒充原实例。
#[derive(Clone)]
pub(crate) struct BorderHudDescriptor(Weak<HudLifetime>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BorderHudRect {
    #[serde(rename = "left")]
    left: i32,
    #[serde(rename = "top")]
    top: i32,
    #[serde(rename = "width")]
    width: u32,
    #[serde(rename = "height")]
    height: u32,
}
impl From<ScreenRect> for BorderHudRect {
    fn from(rect: ScreenRect) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            width: rect.width,
            height: rect.height,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct BorderHudWindowProbe {
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "kind")]
    kind: &'static str,
    #[serde(rename = "index")]
    index: usize,
    #[serde(rename = "original_pid")]
    original_pid: u32,
    #[serde(rename = "actual_pid")]
    actual_pid: u32,
    #[serde(rename = "original_thread")]
    original_thread: u32,
    #[serde(rename = "actual_thread")]
    actual_thread: u32,
    #[serde(rename = "original_class")]
    original_class: String,
    #[serde(rename = "actual_class")]
    actual_class: String,
    #[serde(rename = "expected_rect")]
    expected_rect: BorderHudRect,
    #[serde(rename = "actual_rect")]
    actual_rect: BorderHudRect,
    #[serde(rename = "affinity")]
    affinity: u32,
    #[serde(rename = "property_token")]
    property_token: usize,
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct BorderHudValidation {
    #[serde(rename = "instance_token")]
    instance_token: usize,
    #[serde(rename = "region")]
    region_probe: BorderHudRect,
    #[serde(skip)]
    pub(crate) region: ScreenRect,
    #[serde(rename = "windows")]
    pub(crate) windows: Vec<BorderHudWindowProbe>,
}
impl BorderHudValidation {
    pub(crate) fn contains(&self, hwnd: HWND) -> bool {
        self.windows
            .iter()
            .any(|window| window.hwnd == hwnd.0 as usize)
    }
}
impl BorderHudDescriptor {
    pub(crate) fn is_live(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|live| !live.closed.load(Ordering::Acquire))
    }
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
    pub(crate) fn validate(&self) -> Result<BorderHudValidation> {
        let live = self.0.upgrade().ok_or("登记的长截图边框实例已释放")?;
        hud_ensure(
            !live.closed.load(Ordering::Acquire),
            "登记的长截图边框已关闭",
        )?;
        let (region, originals) = {
            let state = live.state.lock().unwrap_or_else(|error| error.into_inner());
            (state.region, state.windows.clone())
        };
        let expected = hud_regions(region)?;
        hud_ensure(
            originals.len() == expected.len(),
            "长截图边框原窗口登记不完整",
        )?;
        let instance_token = Arc::as_ptr(&live) as usize;
        let mut seen = HashSet::new();
        let mut windows = Vec::with_capacity(expected.len());
        for (index, (original, expected_rect)) in originals.iter().zip(expected).enumerate() {
            hud_ensure(
                original.hwnd != 0 && seen.insert(original.hwnd),
                "长截图边框原窗口重复或无效",
            )?;
            let hwnd = HWND(original.hwnd as *mut _);
            let mut actual_pid = 0;
            let actual_thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut actual_pid)) };
            let actual_class = hud_class(hwnd);
            let mut rect = RECT::default();
            let mut affinity = 0;
            let property_token = unsafe { GetPropW(hwnd, HUD_PROPERTY) }.0 as usize;
            unsafe {
                hud_ensure(
                    IsWindow(Some(hwnd)).as_bool(),
                    "登记的长截图边框 HWND 已销毁",
                )?;
                GetWindowRect(hwnd, &mut rect)?;
                GetWindowDisplayAffinity(hwnd, &mut affinity)?;
            }
            let actual_rect = ScreenRect {
                left: rect.left,
                top: rect.top,
                width: u32::try_from(i64::from(rect.right) - i64::from(rect.left))?,
                height: u32::try_from(i64::from(rect.bottom) - i64::from(rect.top))?,
            };
            validate_hud_window(
                original,
                actual_pid,
                actual_thread,
                &actual_class,
                expected_rect,
                actual_rect,
                instance_token,
                property_token,
                affinity,
            )?;
            hud_ensure(
                unsafe { GetAncestor(hwnd, GA_ROOT) } == hwnd,
                "登记的长截图边框不再是原顶层窗口",
            )?;
            windows.push(BorderHudWindowProbe {
                hwnd: original.hwnd,
                kind: if index < 4 { "strip" } else { "handle" },
                index: if index < 4 { index } else { index - 4 },
                original_pid: original.pid,
                actual_pid,
                original_thread: original.thread,
                actual_thread,
                original_class: original.class_name.clone(),
                actual_class,
                expected_rect: expected_rect.into(),
                actual_rect: actual_rect.into(),
                affinity,
                property_token,
            });
        }
        hud_ensure(
            !live.closed.load(Ordering::Acquire),
            "长截图边框在检查期间关闭",
        )?;
        let state = live.state.lock().unwrap_or_else(|error| error.into_inner());
        hud_ensure(state.region == region, "长截图边框在检查期间变更选区")?;
        Ok(BorderHudValidation {
            instance_token,
            region_probe: region.into(),
            region,
            windows,
        })
    }
}
fn hud_ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn hud_class(hwnd: HWND) -> String {
    let mut value = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut value) }.max(0) as usize;
    String::from_utf16_lossy(&value[..length])
}
fn hud_regions(region: ScreenRect) -> Result<[ScreenRect; 12]> {
    let strips = strip_regions(region)?;
    let handles = handle_regions(region)?;
    Ok(std::array::from_fn(|index| {
        if index < 4 {
            strips[index]
        } else {
            handles[index - 4]
        }
    }))
}
#[allow(clippy::too_many_arguments)]
fn validate_hud_window(
    original: &HudWindow,
    actual_pid: u32,
    actual_thread: u32,
    actual_class: &str,
    expected_rect: ScreenRect,
    actual_rect: ScreenRect,
    instance_token: usize,
    property_token: usize,
    affinity: u32,
) -> Result<()> {
    hud_ensure(
        original.pid == std::process::id()
            && actual_pid == original.pid
            && original.thread != 0
            && actual_thread == original.thread
            && original.class_name == "RustShot.LongCapture.Border"
            && actual_class == original.class_name,
        "登记的长截图边框原 PID/TID/class 身份改变",
    )?;
    hud_ensure(
        instance_token != 0 && property_token == instance_token,
        "登记的长截图边框实例属性不匹配，可能已复用 HWND",
    )?;
    hud_ensure(
        actual_rect == expected_rect,
        "登记的长截图边框不符合原选区窄边/缩放柄几何",
    )?;
    hud_ensure(
        affinity == crate::remote_display::capture_affinity().0,
        "登记的长截图边框显示亲和性已改变",
    )
}

struct ClassState {
    users: usize,
    atom: u16,
}
static CLASS_STATE: Mutex<ClassState> = Mutex::new(ClassState { users: 0, atom: 0 });

struct ClassLease {
    instance: HINSTANCE,
}
impl ClassLease {
    fn acquire() -> Result<Self> {
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
        let mut state = CLASS_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.atom == 0 {
            let brush = unsafe { CreateSolidBrush(LINE_COLOR) };
            if brush.0.is_null() {
                return Err(windows::core::Error::from_win32().into());
            }
            let atom = unsafe {
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(border_proc),
                    hInstance: instance,
                    hbrBackground: brush,
                    lpszClassName: CLASS_NAME,
                    ..Default::default()
                })
            };
            if atom == 0 {
                let error = windows::core::Error::from_win32();
                unsafe {
                    let _ = DeleteObject(brush.into());
                }
                return Err(error.into());
            }
            state.atom = atom;
        }
        state.users += 1;
        Ok(Self { instance })
    }
}
impl Drop for ClassLease {
    fn drop(&mut self) {
        let mut state = CLASS_STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.users -= 1;
        if state.users == 0 && unsafe { UnregisterClassW(CLASS_NAME, Some(self.instance)) }.is_ok()
        {
            // 系统注销类时自动删除背景画刷，不能再次 DeleteObject。
            // 注销失败时保留类供下一实例重用，不释放仍可能被窗口使用的画刷。
            state.atom = 0;
        }
    }
}

pub(crate) struct LongCaptureBorder {
    handles: [HWND; 4],
    regions: [ScreenRect; 4],
    resize_windows: [HWND; 8],
    resize_regions: [ScreenRect; 8],
    /// 录屏样式：缩放区沿整条边且近乎全透明，首帧后才创建。
    edge_handles: bool,
    /// 最近一次 set_color 的 GWLP_USERDATA，后建的缩放区沿用。
    color: Cell<Option<u32>>,
    input: NativeRegionInput,
    visible: Cell<bool>,
    resize_enabled: Cell<bool>,
    owner_thread: u32,
    hud_lifetime: Arc<HudLifetime>,
    _class: ClassLease,
    // DestroyWindow 必须在创建线程执行，阻止把 RAII 宿主送到后台线程。
    _ui_thread: PhantomData<Rc<()>>,
}
impl LongCaptureBorder {
    pub(crate) fn new(region: ScreenRect) -> Result<Self> {
        let mut border = Self::new_strips(region, false)?;
        border.create_resize_handles()?;
        Ok(border)
    }

    /// 录屏边框：先只建四条窄边，缩放区由调用方在首帧后 `create_resize_handles`，
    /// 八个窗口的创建不再挡住工具栏首显。
    pub(crate) fn new_recording(region: ScreenRect) -> Result<Self> {
        Self::new_strips(region, true)
    }

    fn new_strips(region: ScreenRect, edge_handles: bool) -> Result<Self> {
        let regions = strip_regions(region)?;
        let resize_regions = resize_rects(region, edge_handles)?;
        let class = ClassLease::acquire()?;
        let mut border = Self {
            handles: [HWND::default(); 4],
            regions,
            resize_windows: [HWND::default(); 8],
            resize_regions,
            edge_handles,
            color: Cell::new(None),
            input: NativeRegionInput::new(),
            visible: Cell::new(false),
            resize_enabled: Cell::new(false),
            owner_thread: unsafe { GetCurrentThreadId() },
            hud_lifetime: Arc::new(HudLifetime {
                closed: AtomicBool::new(false),
                state: Mutex::new(HudState {
                    region,
                    windows: Vec::with_capacity(12),
                }),
            }),
            _class: class,
            _ui_thread: PhantomData,
        };
        for (index, strip) in border.regions.iter().enumerate() {
            // 先隐藏创建并排除采集。任何一步失败都会回收已创建的所有窄边。
            let handle = unsafe {
                CreateWindowExW(
                    WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE
                        | WS_EX_TRANSPARENT
                        | WS_EX_LAYERED,
                    CLASS_NAME,
                    &windows::core::HSTRING::from(concat!(
                        crate::brand_name!(),
                        " · 长截图选区边框"
                    )),
                    WS_POPUP,
                    strip.left,
                    strip.top,
                    strip.width as i32,
                    strip.height as i32,
                    None,
                    None,
                    Some(border._class.instance),
                    None,
                )?
            };
            border.handles[index] = handle;
            unsafe {
                // Layered + transparent 保证跨进程鼠标穿透；无需接管鼠标或改变前台。
                SetLayeredWindowAttributes(handle, COLORREF(0), 255, LWA_ALPHA)?;
                crate::remote_display::exclude_from_capture(handle)
                    .map_err(|e| format!("无法排除长截图选区边框，已停止显示：{e}"))?;
            }
            border.register_hud_window(handle)?;
        }
        border.set_visible(true)?;
        Ok(border)
    }

    /// 创建八个缩放区；已创建时不重复。按当前可见性、颜色和缩放开关同步显示。
    pub(crate) fn create_resize_handles(&mut self) -> Result<()> {
        self.check_thread()?;
        if self.handles_ready() {
            return Ok(());
        }
        // 录屏样式 alpha=1：肉眼不可见但仍参与命中（alpha=0 会让鼠标穿透）。
        let alpha = if self.edge_handles { 1 } else { 255 };
        let border = self;
        for (index, kind) in RESIZE_KINDS.into_iter().enumerate() {
            let area = border.resize_regions[index];
            let handle = unsafe {
                CreateWindowExW(
                    // 用户点击缩放柄时由标准激活流程接管键盘和前台 capture。
                    // 四条鼠标穿透窄边仍保留 NOACTIVATE，不参与交互。
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                    CLASS_NAME,
                    &windows::core::HSTRING::from(concat!(
                        crate::brand_name!(),
                        " · 长截图选区缩放柄"
                    )),
                    WS_POPUP,
                    area.left,
                    area.top,
                    area.width as i32,
                    area.height as i32,
                    None,
                    None,
                    Some(border._class.instance),
                    None,
                )?
            };
            border.resize_windows[index] = handle;
            unsafe {
                SetLayeredWindowAttributes(handle, COLORREF(0), alpha, LWA_ALPHA)?;
                crate::remote_display::exclude_from_capture(handle)
                    .map_err(|e| format!("无法排除长截图选区缩放柄，已停止显示：{e}"))?;
                if let Some(color) = border.color.get() {
                    SetWindowLongPtrW(handle, GWLP_USERDATA, color as isize);
                }
            }
            border.register_hud_window(handle)?;
            border.input.attach_handle(handle, kind)?;
        }
        border.sync_resize_visibility()
    }

    fn handles_ready(&self) -> bool {
        !self.resize_windows[0].0.is_null()
    }

    fn register_hud_window(&self, hwnd: HWND) -> Result<()> {
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        hud_ensure(
            pid == std::process::id() && thread == self.owner_thread,
            "新建长截图边框归属无效",
        )?;
        unsafe {
            SetPropW(
                hwnd,
                HUD_PROPERTY,
                Some(HANDLE(Arc::as_ptr(&self.hud_lifetime) as *mut _)),
            )?;
        }
        self.hud_lifetime
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .windows
            .push(HudWindow {
                hwnd: hwnd.0 as usize,
                pid,
                thread,
                class_name: hud_class(hwnd),
            });
        Ok(())
    }

    pub(crate) fn hud_descriptor(&self) -> BorderHudDescriptor {
        BorderHudDescriptor(Arc::downgrade(&self.hud_lifetime))
    }

    pub(crate) fn set_visible(&self, visible: bool) -> Result<()> {
        if unsafe { GetCurrentThreadId() } != self.owner_thread {
            return Err("长截图选区边框只能在创建线程更新".into());
        }
        if !visible {
            self.input.cancel();
        }
        for handle in self.handles {
            unsafe {
                if !IsWindow(Some(handle)).as_bool() {
                    return Err("长截图选区边框窗口已关闭".into());
                }
                SetWindowPos(
                    handle,
                    visible.then_some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOACTIVATE
                        | if visible {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW | SWP_NOZORDER
                        },
                )?;
            }
        }
        self.visible.set(visible);
        self.sync_resize_visibility()?;
        Ok(())
    }

    fn check_thread(&self) -> Result<()> {
        if unsafe { GetCurrentThreadId() } != self.owner_thread {
            return Err("长截图选区边框只能在创建线程更新".into());
        }
        Ok(())
    }

    /// 同 HWND 更新边框与缩放柄，捕获期间不会销毁或重建输入窗口。
    pub(crate) fn update_region(&mut self, region: ScreenRect) -> Result<()> {
        self.check_thread()?;
        let strips = strip_regions(region)?;
        let handles = resize_rects(region, self.edge_handles)?;
        if self.regions == strips && self.resize_regions == handles {
            return Ok(());
        }
        let resize = if self.handles_ready() { 8 } else { 0 };
        let updated = self.handles.iter().copied().zip(strips).chain(
            self.resize_windows
                .iter()
                .copied()
                .zip(handles)
                .take(resize),
        );
        for (hwnd, area) in updated {
            if let Err(error) = position_window(hwnd, area) {
                // 原生部分更新失败时尽量恢复旧框，模型只在全部成功后切换。
                for (hwnd, area) in self.handles.iter().copied().zip(self.regions).chain(
                    self.resize_windows
                        .iter()
                        .copied()
                        .zip(self.resize_regions)
                        .take(resize),
                ) {
                    let _ = position_window(hwnd, area);
                }
                return Err(error);
            }
        }
        self.regions = strips;
        self.resize_regions = handles;
        self.hud_lifetime
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .region = region;
        Ok(())
    }

    /// 录屏复用同一组排除采集的窄边与缩放柄，按状态改色；rgb 为 0xRRGGBB。
    pub(crate) fn set_color(&self, rgb: u32) -> Result<()> {
        self.check_thread()?;
        let colorref = ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff);
        self.color.set(Some(colorref | CUSTOM_COLOR));
        for hwnd in self.handles.iter().chain(self.resize_windows.iter()) {
            if hwnd.0.is_null() {
                continue;
            }
            unsafe {
                SetWindowLongPtrW(*hwnd, GWLP_USERDATA, (colorref | CUSTOM_COLOR) as isize);
                let _ = InvalidateRect(Some(*hwnd), None, true);
            }
        }
        Ok(())
    }

    pub(crate) fn bind_panel(&mut self, panel: HWND) -> Result<()> {
        self.check_thread()?;
        self.input.bind_panel(panel)
    }

    pub(crate) fn set_resize_enabled(&mut self, enabled: bool) -> Result<()> {
        self.check_thread()?;
        self.input.set_resize_enabled(enabled)?;
        self.resize_enabled.set(enabled);
        self.sync_resize_visibility()
    }

    fn sync_resize_visibility(&self) -> Result<()> {
        if !self.handles_ready() {
            return Ok(());
        }
        let active = self.input.active_hwnd();
        let foreground = unsafe { GetForegroundWindow() };
        for handle in self.resize_windows {
            // Down 已进入队列但 UI 正在等待暂停确认时，仍保留当前捕获柄。
            // Up 后先保留仍有键盘焦点的柄，等待 UI 在借用外把前台交给操作条。
            let visible = self.visible.get()
                && (self.resize_enabled.get() || active == Some(handle) || foreground == handle);
            unsafe {
                if !IsWindow(Some(handle)).as_bool() {
                    return Err("长截图选区缩放柄已关闭".into());
                }
                if IsWindowVisible(handle).as_bool() == visible {
                    continue;
                }
                SetWindowPos(
                    handle,
                    visible.then_some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOACTIVATE
                        | if visible {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW | SWP_NOZORDER
                        },
                )?;
            }
        }
        Ok(())
    }

    /// Move 的 GPUI Down 与八柄共享同一个 ID 分配器；成功后队列包含 Down(Move)。
    pub(crate) fn begin_move_capture(&mut self, panel: HWND, start: (i32, i32)) -> Result<u64> {
        self.check_thread()?;
        self.input.begin_move_capture(panel, start)
    }

    pub(crate) fn cancel_input(&self) {
        self.input.cancel();
    }

    pub(crate) fn drain_events(&self) -> Vec<RegionInputEvent> {
        self.input.drain_events()
    }

    /// 专项验收只枚举本组件的八个排除采集窗口及其物理矩形。
    pub(crate) fn resize_handles(&self) -> Vec<(RegionHandle, HWND, ScreenRect)> {
        RESIZE_KINDS
            .into_iter()
            .zip(self.resize_windows)
            .zip(self.resize_regions)
            .map(|((kind, hwnd), area)| (kind, hwnd, area))
            .collect()
    }

    /// 定向原生验收可枚举自有四边，不需要读取外部窗口或真实桌面图片。
    pub(crate) fn handles(&self) -> [HWND; 4] {
        self.handles
    }
    pub(crate) fn regions(&self) -> [ScreenRect; 4] {
        self.regions
    }
}
impl Drop for LongCaptureBorder {
    fn drop(&mut self) {
        self.hud_lifetime.closed.store(true, Ordering::Release);
        self.input.cancel();
        let token = Arc::as_ptr(&self.hud_lifetime) as usize;
        let registered: HashSet<_> = self
            .hud_lifetime
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .windows
            .iter()
            .map(|window| window.hwnd)
            .collect();
        for handle in self.handles.into_iter().chain(self.resize_windows) {
            if !handle.0.is_null() {
                unsafe {
                    // 已登记窗口只能销毁原实例；外部关闭后复用的 HWND 不属于本 Border。
                    // 尚未安装属性的构造失败窗口仍由当前同步创建流程负责回收。
                    if !registered.contains(&(handle.0 as usize))
                        || GetPropW(handle, HUD_PROPERTY).0 as usize == token
                    {
                        let _ = DestroyWindow(handle);
                    }
                }
            }
        }
        // _class 随后释放，确保 HWND 已销毁再注销窗口类，由系统回收背景画刷。
    }
}

fn position_window(hwnd: HWND, area: ScreenRect) -> Result<()> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err("长截图选区输入窗口已关闭".into());
        }
        SetWindowPos(
            hwnd,
            None,
            area.left,
            area.top,
            area.width as i32,
            area.height as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    Ok(())
}

fn resize_rects(region: ScreenRect, edge: bool) -> Result<[ScreenRect; 8]> {
    if edge {
        edge_handle_regions(region)
    } else {
        handle_regions(region)
    }
}

/// 录屏缩放区：四角方块加四条边（去掉角），都贴在选区内缘；顺序同 RESIZE_KINDS。
fn edge_handle_regions(region: ScreenRect) -> Result<[ScreenRect; 8]> {
    border_regions(region)?;
    let corner_w = EDGE_CORNER.min(region.width / 3).max(1);
    let corner_h = EDGE_CORNER.min(region.height / 3).max(1);
    let thick_w = EDGE_THICKNESS.min(corner_w);
    let thick_h = EDGE_THICKNESS.min(corner_h);
    let span_w = region.width - corner_w * 2;
    let span_h = region.height - corner_h * 2;
    let (left, top) = (region.left, region.top);
    let east = left + (region.width - corner_w) as i32;
    let south = top + (region.height - corner_h) as i32;
    let rect = |left: i32, top: i32, width: u32, height: u32| ScreenRect {
        left,
        top,
        width,
        height,
    };
    Ok([
        rect(left + corner_w as i32, top, span_w, thick_h),
        rect(east, top, corner_w, corner_h),
        rect(
            left + (region.width - thick_w) as i32,
            top + corner_h as i32,
            thick_w,
            span_h,
        ),
        rect(east, south, corner_w, corner_h),
        rect(
            left + corner_w as i32,
            top + (region.height - thick_h) as i32,
            span_w,
            thick_h,
        ),
        rect(left, south, corner_w, corner_h),
        rect(left, top + corner_h as i32, thick_w, span_h),
        rect(left, top, corner_w, corner_h),
    ])
}

fn handle_regions(region: ScreenRect) -> Result<[ScreenRect; 8]> {
    // 要留出中点柄与角柄之间的间距，常规 LongOptions 的 32px 最小跨轴满足此限制。
    if region.width < HANDLE_SIDE * 2 + 1 || region.height < HANDLE_SIDE * 2 + 1 {
        return Err("长截图选区不足以显示八个缩放柄".into());
    }
    border_regions(region)?;
    let left = i64::from(region.left);
    let top = i64::from(region.top);
    let east = left + i64::from(region.width - HANDLE_SIDE);
    let south = top + i64::from(region.height - HANDLE_SIDE);
    let center_x = left + i64::from((region.width - HANDLE_SIDE) / 2);
    let center_y = top + i64::from((region.height - HANDLE_SIDE) / 2);
    Ok([
        (center_x, top),
        (east, top),
        (east, center_y),
        (east, south),
        (center_x, south),
        (left, south),
        (left, center_y),
        (left, top),
    ]
    .map(|(left, top)| ScreenRect {
        left: left as i32,
        top: top as i32,
        width: HANDLE_SIDE,
        height: HANDLE_SIDE,
    }))
}

/// 本机四条边在选区内缘并排除采集；远程画面兼容时边框可见即会被采到，改画在选区外一圈，
/// 贴屏幕边的那一侧因此看不到。
fn strip_regions(region: ScreenRect) -> Result<[ScreenRect; 4]> {
    if crate::remote_display::compatible() {
        border_regions(outset(region)?)
    } else {
        border_regions(region)
    }
}

fn outset(region: ScreenRect) -> Result<ScreenRect> {
    let grow = LINE_WIDTH * 2;
    Ok(ScreenRect {
        left: region
            .left
            .checked_sub(LINE_WIDTH as i32)
            .ok_or("长截图选区边框坐标超出范围")?,
        top: region
            .top
            .checked_sub(LINE_WIDTH as i32)
            .ok_or("长截图选区边框坐标超出范围")?,
        width: region
            .width
            .checked_add(grow)
            .ok_or("长截图选区边框尺寸无效")?,
        height: region
            .height
            .checked_add(grow)
            .ok_or("长截图选区边框尺寸无效")?,
    })
}

fn border_regions(region: ScreenRect) -> Result<[ScreenRect; 4]> {
    if region.width < LINE_WIDTH * 2 + 1
        || region.height < LINE_WIDTH * 2 + 1
        || region.width > i32::MAX as u32
        || region.height > i32::MAX as u32
    {
        return Err("长截图选区边框尺寸无效".into());
    }
    let right = i64::from(region.left) + i64::from(region.width);
    let bottom = i64::from(region.top) + i64::from(region.height);
    if right > i64::from(i32::MAX) || bottom > i64::from(i32::MAX) {
        return Err("长截图选区边框坐标超出范围".into());
    }
    // 放在选区边内，屏幕四边也可见；所有边均排除采集，不能进入拼接成果。
    let inner_top = region.top + LINE_WIDTH as i32;
    let bottom_top = (bottom - i64::from(LINE_WIDTH)) as i32;
    let right_left = (right - i64::from(LINE_WIDTH)) as i32;
    Ok([
        ScreenRect {
            height: LINE_WIDTH,
            ..region
        },
        ScreenRect {
            top: bottom_top,
            height: LINE_WIDTH,
            ..region
        },
        ScreenRect {
            left: region.left,
            top: inner_top,
            width: LINE_WIDTH,
            height: region.height - LINE_WIDTH * 2,
        },
        ScreenRect {
            left: right_left,
            top: inner_top,
            width: LINE_WIDTH,
            height: region.height - LINE_WIDTH * 2,
        },
    ])
}

unsafe extern "system" fn border_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => unsafe {
            let custom = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32;
            if custom & CUSTOM_COLOR == 0 {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let brush = CreateSolidBrush(COLORREF(custom & 0x00ff_ffff));
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            FillRect(HDC(wparam.0 as *mut _), &rect, brush);
            let _ = DeleteObject(brush.into());
            LRESULT(1)
        },
        WM_NCDESTROY => unsafe {
            let _ = RemovePropW(hwnd, HUD_PROPERTY);
            DefWindowProcW(hwnd, message, wparam, lparam)
        },
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_edge_handles_cover_edges_inside_region_in_kind_order() {
        let region = ScreenRect {
            left: -100,
            top: 50,
            width: 400,
            height: 300,
        };
        let rects = edge_handle_regions(region).unwrap();
        let right = region.left + region.width as i32;
        let bottom = region.top + region.height as i32;
        for r in rects {
            assert!(r.left >= region.left && r.top >= region.top);
            assert!(r.left + r.width as i32 <= right && r.top + r.height as i32 <= bottom);
        }
        // 北边整段（去掉两角）贴上缘；角块贴各自两边。
        assert_eq!((rects[0].left, rects[0].top), (-86, 50));
        assert_eq!(rects[0].width, 400 - 28);
        assert_eq!((rects[3].left + 14, rects[3].top + 14), (right, bottom));
        assert_eq!((rects[7].left, rects[7].top), (region.left, region.top));
        // 小选区也能生成，不像八个缩放点那样要求至少 21px。
        let tiny = ScreenRect {
            width: 8,
            height: 8,
            ..region
        };
        assert!(edge_handle_regions(tiny).is_ok());
    }

    #[test]
    fn long_browser_hud_original_identity_rejects_same_class_reuse_affinity_and_geometry_drift() {
        let area = ScreenRect {
            left: -1920,
            top: -240,
            width: 960,
            height: 640,
        };
        let original = HudWindow {
            hwnd: 0x1234,
            pid: std::process::id(),
            thread: 7,
            class_name: "RustShot.LongCapture.Border".into(),
        };
        let expected = hud_regions(area).unwrap()[0];
        // 期望值随本机／远程兼容结论而定，另一种无效值总取相反的那个。
        let required = crate::remote_display::capture_affinity().0;
        let other = if required == WDA_NONE.0 {
            WDA_EXCLUDEFROMCAPTURE.0
        } else {
            WDA_NONE.0
        };
        let cases = [
            (
                original.pid,
                7,
                original.class_name.as_str(),
                expected,
                9,
                required,
                true,
            ),
            (
                original.pid.wrapping_add(1),
                7,
                original.class_name.as_str(),
                expected,
                9,
                required,
                false,
            ),
            (
                original.pid,
                8,
                original.class_name.as_str(),
                expected,
                9,
                required,
                false,
            ),
            (
                original.pid,
                7,
                "Other.Border",
                expected,
                9,
                required,
                false,
            ),
            (
                original.pid,
                7,
                original.class_name.as_str(),
                ScreenRect {
                    left: expected.left + 1,
                    ..expected
                },
                9,
                required,
                false,
            ),
            (
                original.pid,
                7,
                original.class_name.as_str(),
                expected,
                0,
                required,
                false,
            ),
            (
                original.pid,
                7,
                original.class_name.as_str(),
                expected,
                10,
                required,
                false,
            ),
            (
                original.pid,
                7,
                original.class_name.as_str(),
                expected,
                9,
                WDA_MONITOR.0,
                false,
            ),
            (
                original.pid,
                7,
                original.class_name.as_str(),
                expected,
                9,
                other,
                false,
            ),
        ];
        for (pid, thread, class, rect, token, affinity, accepted) in cases {
            assert_eq!(
                validate_hud_window(
                    &original, pid, thread, class, expected, rect, 9, token, affinity
                )
                .is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn long_browser_hud_weak_descriptor_cannot_revive_closed_instance_or_reuse_its_token() {
        let make = || {
            Arc::new(HudLifetime {
                closed: AtomicBool::new(false),
                state: Mutex::new(HudState {
                    region: ScreenRect {
                        left: 0,
                        top: 0,
                        width: 32,
                        height: 64,
                    },
                    windows: Vec::new(),
                }),
            })
        };
        let original = make();
        let token = Arc::as_ptr(&original) as usize;
        let descriptor = BorderHudDescriptor(Arc::downgrade(&original));
        assert!(descriptor.is_live());
        assert!(descriptor.same_instance(&descriptor.clone()));
        original.closed.store(true, Ordering::Release);
        assert!(!descriptor.is_live());
        assert!(descriptor.validate().is_err());
        drop(original);
        assert!(descriptor.validate().is_err());
        let replacement = make();
        assert_ne!(Arc::as_ptr(&replacement) as usize, token);
        assert!(!descriptor.same_instance(&BorderHudDescriptor(Arc::downgrade(&replacement))));
    }

    #[test]
    fn long_region_eight_handle_windows_stay_inside_a_negative_origin_narrow_region() {
        let area = ScreenRect {
            left: -30,
            top: -50,
            width: 32,
            height: 64,
        };
        let windows = handle_regions(area).unwrap();
        let expected = [
            (-19, -50),
            (-8, -50),
            (-8, -23),
            (-8, 4),
            (-19, 4),
            (-30, 4),
            (-30, -23),
            (-30, -50),
        ];
        for (window, (left, top)) in windows.into_iter().zip(expected) {
            assert_eq!(
                window,
                ScreenRect {
                    left,
                    top,
                    width: 10,
                    height: 10
                }
            );
            assert!(window.left >= -30 && window.top >= -50);
            assert!(window.left + window.width as i32 <= 2);
            assert!(window.top + window.height as i32 <= 14);
        }
        for (index, a) in windows.iter().enumerate() {
            for b in &windows[index + 1..] {
                assert!(
                    a.left + a.width as i32 <= b.left
                        || b.left + b.width as i32 <= a.left
                        || a.top + a.height as i32 <= b.top
                        || b.top + b.height as i32 <= a.top
                );
            }
        }
    }

    #[test]
    fn long_border_regions_are_inside_negative_origin_fullscreen_and_do_not_overlap() {
        let area = ScreenRect {
            left: -1920,
            top: -240,
            width: 1920,
            height: 1080,
        };
        let strips = border_regions(area).unwrap();
        for strip in strips {
            assert!(strip.left >= area.left && strip.top >= area.top);
            assert!(strip.left as i64 + strip.width as i64 <= 0);
            assert!(strip.top as i64 + strip.height as i64 <= 840);
        }
        let pixels: u64 = strips
            .iter()
            .map(|r| u64::from(r.width) * u64::from(r.height))
            .sum();
        assert_eq!(pixels, 1920 * 1080 - 1916 * 1076);
        assert_eq!(strips[2].top, strips[0].top + 2);
        assert_eq!(
            strips[2].top as i64 + strips[2].height as i64,
            strips[1].top as i64
        );
    }

    #[test]
    fn long_border_regions_reject_overflow_and_degenerate_strips() {
        for area in [
            ScreenRect {
                left: i32::MAX - 2,
                top: 0,
                width: 8,
                height: 64,
            },
            ScreenRect {
                left: 0,
                top: i32::MAX - 2,
                width: 64,
                height: 8,
            },
            ScreenRect {
                left: 0,
                top: 0,
                width: 4,
                height: 64,
            },
            ScreenRect {
                left: i32::MIN,
                top: 0,
                width: u32::MAX,
                height: 64,
            },
        ] {
            assert!(border_regions(area).is_err());
        }
    }
}
