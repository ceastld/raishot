//! 贴图外缘的原生柔和阴影。仅四条可见边，主窗口与导出坐标不增加透明边距。
use crate::Result;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    sync::{Arc, Mutex, OnceLock},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
};

const CLASS: PCWSTR = w!("RustShot.Pin.Shadow.v1");
const PART_TOKEN: PCWSTR = w!("RustShot.Pin.Shadow.Instance.v1");
const OWNER_TOKEN: PCWSTR = w!("RustShot.Pin.Shadow.Owner.v1");
pub(crate) const SHADOW_BYTE_BUDGET: usize = 8_000_000;
const PART_NAMES: [&str; 4] = ["top", "bottom", "left", "right"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ShadowRect {
    #[serde(rename = "x")]
    pub(crate) x: i32,
    #[serde(rename = "y")]
    pub(crate) y: i32,
    #[serde(rename = "width")]
    pub(crate) width: u32,
    #[serde(rename = "height")]
    pub(crate) height: u32,
}
impl ShadowRect {
    fn edges(self) -> [i64; 4] {
        [
            self.x as i64,
            self.y as i64,
            self.x as i64 + self.width as i64,
            self.y as i64 + self.height as i64,
        ]
    }
    fn bytes(self) -> Result<usize> {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| "贴图阴影尺寸溢出".into())
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PinShadowPartProbe {
    #[serde(rename = "part")]
    pub(crate) part: &'static str,
    #[serde(rename = "index")]
    pub(crate) index: usize,
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "pid")]
    pub(crate) pid: u32,
    #[serde(rename = "tid")]
    pub(crate) tid: u32,
    #[serde(rename = "class")]
    pub(crate) class: String,
    #[serde(rename = "token")]
    pub(crate) token: usize,
    #[serde(rename = "owner")]
    pub(crate) owner: usize,
    #[serde(rename = "rect")]
    pub(crate) rect: Option<ShadowRect>,
    #[serde(rename = "native_rect")]
    pub(crate) native_rect: ShadowRect,
    #[serde(rename = "visible")]
    pub(crate) visible: bool,
    #[serde(rename = "affinity")]
    pub(crate) affinity: u32,
    #[serde(rename = "exstyle")]
    pub(crate) exstyle: u32,
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PinShadowProbe {
    #[serde(rename = "owner")]
    pub(crate) owner: usize,
    #[serde(rename = "owner_pid")]
    pub(crate) owner_pid: u32,
    #[serde(rename = "owner_tid")]
    pub(crate) owner_tid: u32,
    #[serde(rename = "owner_class")]
    pub(crate) owner_class: String,
    #[serde(rename = "content_rect")]
    pub(crate) content_rect: ShadowRect,
    #[serde(rename = "dpi")]
    pub(crate) dpi: u32,
    #[serde(rename = "extent")]
    pub(crate) extent: u32,
    #[serde(rename = "active")]
    pub(crate) active: bool,
    #[serde(rename = "locked")]
    pub(crate) locked: bool,
    #[serde(rename = "effective_alpha")]
    pub(crate) effective_alpha: u8,
    #[serde(rename = "owner_visible")]
    pub(crate) owner_visible: bool,
    #[serde(rename = "topmost")]
    pub(crate) topmost: bool,
    #[serde(rename = "visible_perimeter_bytes")]
    pub(crate) visible_perimeter_bytes: usize,
    #[serde(rename = "byte_budget")]
    pub(crate) byte_budget: usize,
    #[serde(rename = "parts")]
    pub(crate) parts: Vec<PinShadowPartProbe>,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct PinShadowLedgerProbe {
    #[serde(rename = "current_staging_bytes")]
    pub(crate) current_staging_bytes: usize,
    #[serde(rename = "peak_staging_bytes")]
    pub(crate) peak_staging_bytes: usize,
    #[serde(rename = "live_dcs")]
    pub(crate) live_dcs: usize,
    #[serde(rename = "live_dibs")]
    pub(crate) live_dibs: usize,
    #[serde(rename = "successful_uploads")]
    pub(crate) successful_uploads: u64,
    #[serde(rename = "upload_revision")]
    pub(crate) upload_revision: u64,
    #[serde(rename = "first_successful_upload_unix_ms")]
    pub(crate) first_successful_upload_unix_ms: u64,
    #[serde(rename = "last_successful_upload_unix_ms")]
    pub(crate) last_successful_upload_unix_ms: u64,
    #[serde(rename = "first_upload_owner_visible")]
    pub(crate) first_upload_owner_visible: bool,
    #[serde(rename = "last_visible_perimeter_bytes")]
    pub(crate) last_visible_perimeter_bytes: usize,
    #[serde(rename = "created_windows")]
    pub(crate) created_windows: usize,
    #[serde(rename = "destroyed_or_already_gone_windows")]
    pub(crate) destroyed_or_already_gone_windows: usize,
    #[serde(rename = "closed")]
    pub(crate) closed: bool,
    #[serde(rename = "errors")]
    pub(crate) errors: Vec<String>,
}
pub(crate) struct PinShadowLedger {
    state: Mutex<PinShadowLedgerProbe>,
    // 外部只读 ledger 也保留原 token 地址，不能因 HWND / 分配器复用冒充原实例。
    token: Arc<u8>,
}
impl PinShadowLedger {
    pub(crate) fn snapshot(&self) -> PinShadowLedgerProbe {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn update(&self, f: impl FnOnce(&mut PinShadowLedgerProbe)) {
        f(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()));
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PinShadowUploadProbe {
    #[serde(rename = "part")]
    pub(crate) part: &'static str,
    #[serde(rename = "index")]
    pub(crate) index: usize,
    #[serde(rename = "hwnd")]
    pub(crate) hwnd: usize,
    #[serde(rename = "owner")]
    pub(crate) owner: usize,
    #[serde(rename = "token")]
    pub(crate) token: usize,
    #[serde(rename = "rect")]
    pub(crate) rect: ShadowRect,
    #[serde(rename = "content_rect")]
    pub(crate) content_rect: ShadowRect,
    #[serde(rename = "dpi")]
    pub(crate) dpi: u32,
    #[serde(rename = "extent")]
    pub(crate) extent: u32,
    #[serde(rename = "revision")]
    pub(crate) revision: u64,
    #[serde(rename = "effective_alpha")]
    pub(crate) effective_alpha: u8,
    #[serde(rename = "owner_visible")]
    pub(crate) owner_visible: bool,
    #[serde(rename = "premultiplied_bgra")]
    pub(crate) premultiplied_bgra: bool,
}
pub(crate) type UploadObserver = dyn Fn(&PinShadowUploadProbe, &[u8]) -> Result<()>;
thread_local! {
    static SHADOWS: RefCell<HashMap<usize, Weak<PinShadowCore>>> = RefCell::new(HashMap::new());
    static UPLOAD_OBSERVER: RefCell<Option<Rc<UploadObserver>>> = RefCell::new(None);
}
pub(crate) struct UploadObservationGuard(Rc<UploadObserver>);
pub(crate) fn observe_uploads_for_qa(
    observer: Rc<UploadObserver>,
) -> Result<UploadObservationGuard> {
    UPLOAD_OBSERVER.with(|slot| {
        if slot.borrow().is_some() {
            return Err("贴图阴影上传观察器已安装".into());
        }
        *slot.borrow_mut() = Some(observer.clone());
        Ok(UploadObservationGuard(observer))
    })
}
impl Drop for UploadObservationGuard {
    fn drop(&mut self) {
        UPLOAD_OBSERVER.with(|slot| {
            let original = slot
                .borrow()
                .as_ref()
                .is_some_and(|s| Rc::ptr_eq(s, &self.0));
            if original {
                slot.borrow_mut().take();
            }
        });
    }
}

/// 贴图外观的可见外缘，相对主窗口左上角的物理像素。圆角贴图的窗口四角是透明的，
/// 阴影须沿这条圆角轮廓而不是窗口矩形渲染。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShadowOutline {
    pub(crate) left: f64,
    pub(crate) top: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) radius: f64,
}

/// 屏幕物理坐标中的阴影轮廓；radius 已按短边一半限幅。
#[derive(Clone, Copy, Debug, PartialEq)]
struct ShadowShape {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    radius: f64,
}
impl ShadowShape {
    fn new(window: ShadowRect, outline: ShadowOutline) -> Option<Self> {
        let values = [
            outline.left,
            outline.top,
            outline.width,
            outline.height,
            outline.radius,
        ];
        if values.iter().any(|v| !v.is_finite()) || outline.width < 1. || outline.height < 1. {
            return None;
        }
        let left = f64::from(window.x) + outline.left;
        let top = f64::from(window.y) + outline.top;
        Some(Self {
            left,
            top,
            right: left + outline.width,
            bottom: top + outline.height,
            radius: outline
                .radius
                .clamp(0., outline.width.min(outline.height) * 0.5),
        })
    }
    /// 直角且落在整像素上时沿用矩形路径，保持旧阴影逐像素不变。
    fn integral_rect(self) -> Option<ShadowRect> {
        let edges = [self.left, self.top, self.right, self.bottom];
        if self.radius > 0. || edges.iter().any(|v| v.fract() != 0.) {
            return None;
        }
        Some(ShadowRect {
            x: self.left as i32,
            y: self.top as i32,
            width: (self.right - self.left) as u32,
            height: (self.bottom - self.top) as u32,
        })
    }
    /// 外包整像素矩形与需要向内补阴影的圆角跨度。
    fn bounds(self) -> Result<(ShadowRect, u32)> {
        let left = self.left.floor();
        let top = self.top.floor();
        let width = self.right.ceil() - left;
        let height = self.bottom.ceil() - top;
        if !(1. ..=f64::from(i32::MAX)).contains(&width)
            || !(1. ..=f64::from(i32::MAX)).contains(&height)
            || !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&left)
            || !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&top)
        {
            return Err("贴图阴影轮廓无效".into());
        }
        let rect = ShadowRect {
            x: left as i32,
            y: top as i32,
            width: width as u32,
            height: height as u32,
        };
        // 多算 1px 覆盖轮廓抗锯齿；不超过短边一半，左右边窗口才不会与上下边重叠。
        let inset = ((self.radius.ceil() as u32) + 1).min(rect.width.min(rect.height) / 2);
        Ok((rect, inset))
    }
    fn signed_distance(self, x: f64, y: f64) -> f64 {
        let half_w = (self.right - self.left) * 0.5;
        let half_h = (self.bottom - self.top) * 0.5;
        let px = (x - self.left - half_w).abs() - half_w + self.radius;
        let py = (y - self.top - half_h).abs() - half_h + self.radius;
        px.max(0.).hypot(py.max(0.)) + px.max(py).min(0.) - self.radius
    }
    fn key(self) -> [u64; 5] {
        [self.left, self.top, self.right, self.bottom, self.radius].map(f64::to_bits)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct UploadKey {
    content: ShadowRect,
    shape: Option<[u64; 5]>,
    rect: ShadowRect,
    dpi: u32,
    alpha: u8,
    color: [u8; 3],
}
struct Part {
    hwnd: HWND,
    key: Cell<Option<UploadKey>>,
    rect: Cell<Option<ShadowRect>>,
}
pub(crate) struct PinShadowCore {
    owner: HWND,
    pid: u32,
    tid: u32,
    owner_class: String,
    parts: Vec<Part>,
    active: Cell<bool>,
    locked: Cell<bool>,
    enabled: Cell<bool>,
    inactive_color: Cell<u32>,
    outline: Cell<Option<ShadowOutline>>,
    busy: Cell<bool>,
    ledger: Arc<PinShadowLedger>,
}
/// 唯一强所有者放在 PinView；native 同步登记只保留 Weak，不能延长窗口寿命。
pub(crate) struct PinShadow {
    core: Rc<PinShadowCore>,
}
impl PinShadow {
    pub(crate) fn new(owner: HWND, enabled: bool, inactive_color: u32) -> Result<Self> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) };
        if pid != std::process::id()
            || tid != unsafe { GetCurrentThreadId() }
            || !unsafe { IsWindow(Some(owner)) }.as_bool()
            || unsafe { GetPropW(owner, w!("RustShot.Pin.v1")) }.0 != owner.0
            || !unsafe { GetPropW(owner, OWNER_TOKEN) }.is_invalid()
        {
            return Err("贴图阴影必须绑定当前线程原始自有贴图".into());
        }
        register_class()?;
        let ledger = Arc::new(PinShadowLedger {
            state: Mutex::new(Default::default()),
            token: Arc::new(0),
        });
        let core = Rc::new(PinShadowCore {
            owner,
            pid,
            tid,
            owner_class: class_name(owner)?,
            parts: Vec::new(),
            active: Cell::new(false),
            locked: Cell::new(false),
            enabled: Cell::new(enabled),
            inactive_color: Cell::new(inactive_color),
            outline: Cell::new(None),
            busy: Cell::new(false),
            ledger,
        });
        let mut result = Self { core };
        unsafe {
            SetPropW(
                owner,
                OWNER_TOKEN,
                Some(HANDLE(result.core.token() as *mut _)),
            )?;
        }
        // 先建立 RAII 再逐个创建；中途失败仍只释放已创建的原始 strip。
        for _ in 0..4 {
            let h = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                    CLASS,
                    w!(""),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    Some(owner),
                    None,
                    Some(HINSTANCE(GetModuleHandleW(None)?.0)),
                    None,
                )?
            };
            let token = result.core.token();
            if let Err(error) = unsafe { SetPropW(h, PART_TOKEN, Some(HANDLE(token as *mut _))) } {
                // 此句柄刚由本函数创建，尚未发布；属性设置失败也必须归还。
                unsafe {
                    let _ = DestroyWindow(h);
                }
                return Err(error.into());
            }
            let mutable = Rc::get_mut(&mut result.core).ok_or("贴图阴影初始化存在额外所有者")?;
            mutable.parts.push(Part {
                hwnd: h,
                key: Cell::new(None),
                rect: Cell::new(None),
            });
            mutable.ledger.update(|s| s.created_windows += 1);
            crate::remote_display::exclude_from_capture(h)?;
        }
        result.refresh()?;
        SHADOWS.with(|registry| {
            registry
                .borrow_mut()
                .insert(owner.0 as usize, Rc::downgrade(&result.core));
        });
        Ok(result)
    }
    pub(crate) fn set_presentation(
        &self,
        enabled: bool,
        active: bool,
        locked: bool,
        inactive_color: u32,
    ) -> Result<()> {
        self.core.active.set(active);
        self.core.locked.set(locked);
        self.core.enabled.set(enabled);
        self.core.inactive_color.set(inactive_color);
        self.refresh()
    }
    /// 只记录轮廓；下一次 `set_presentation`／`refresh` 再上传。
    pub(crate) fn set_outline(&self, outline: Option<ShadowOutline>) {
        self.core.outline.set(outline);
    }
    pub(crate) fn refresh(&self) -> Result<()> {
        self.core.refresh(false)
    }
    pub(crate) fn refresh_for_qa(&self) -> Result<()> {
        self.core.refresh(true)
    }
    pub(crate) fn probe(&self) -> Result<PinShadowProbe> {
        self.core.probe()
    }
    pub(crate) fn ledger(&self) -> Arc<PinShadowLedger> {
        self.core.ledger.clone()
    }
    pub(crate) fn weak(&self) -> Weak<PinShadowCore> {
        Rc::downgrade(&self.core)
    }
}
pub(crate) fn sync_native(owner: HWND) {
    let weak = SHADOWS.with(|registry| registry.borrow().get(&(owner.0 as usize)).cloned());
    if let Some(core) = weak.and_then(|weak| weak.upgrade()) {
        // 装饰失败已在 refresh 中记录并隐藏，不改变成功的主窗口定位结果。
        let _ = core.refresh(false);
    }
}
/// 异步隐藏主窗口前同步隐藏原 strip，避免主窗口停止渲染后留下孤立阴影。
pub(crate) fn hide_native(owner: HWND) {
    let weak = SHADOWS.with(|registry| registry.borrow().get(&(owner.0 as usize)).cloned());
    if let Some(core) = weak.and_then(|weak| weak.upgrade()) {
        core.hide();
    }
}
impl PinShadowCore {
    fn token(&self) -> usize {
        Arc::as_ptr(&self.ledger.token) as usize
    }
    fn owner_valid(&self) -> Result<()> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(self.owner, Some(&mut pid)) };
        if !unsafe { IsWindow(Some(self.owner)) }.as_bool()
            || pid != self.pid
            || tid != self.tid
            || tid != unsafe { GetCurrentThreadId() }
            || class_name(self.owner)? != self.owner_class
            || unsafe { GetPropW(self.owner, OWNER_TOKEN) }.0 as usize != self.token()
            || unsafe { GetPropW(self.owner, w!("RustShot.Pin.v1")) }.0 != self.owner.0
        {
            return Err("贴图阴影原主窗口身份已失效".into());
        }
        Ok(())
    }
    fn part_identity(&self, part: &Part) -> Result<()> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(part.hwnd, Some(&mut pid)) };
        if !unsafe { IsWindow(Some(part.hwnd)) }.as_bool()
            || pid != self.pid
            || tid != self.tid
            || class_name(part.hwnd)? != "RustShot.Pin.Shadow.v1"
            || unsafe { GetPropW(part.hwnd, PART_TOKEN) }.0 as usize != self.token()
            || unsafe { GetWindow(part.hwnd, GW_OWNER) }? != self.owner
        {
            return Err("贴图阴影原边窗口身份已失效".into());
        }
        Ok(())
    }
    fn part_valid(&self, part: &Part) -> Result<()> {
        self.part_identity(part)?;
        let mut affinity = 0;
        unsafe {
            GetWindowDisplayAffinity(part.hwnd, &mut affinity)?;
        }
        let required = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE;
        if affinity != crate::remote_display::capture_affinity().0
            || unsafe { GetWindowLongPtrW(part.hwnd, GWL_EXSTYLE) } & required.0 as isize
                != required.0 as isize
        {
            return Err("贴图阴影排除采集或穿透属性已改变".into());
        }
        Ok(())
    }
    fn presentation(&self) -> Result<(ShadowRect, u32, u32, u8, bool, bool)> {
        self.owner_valid()?;
        let content = native_rect(self.owner)?;
        let dpi = unsafe { GetDpiForWindow(self.owner) };
        let extent = shadow_extent(dpi)?;
        let style = unsafe { GetWindowLongPtrW(self.owner, GWL_EXSTYLE) };
        let mut alpha = 255;
        if style & WS_EX_LAYERED.0 as isize != 0 {
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
            unsafe {
                GetLayeredWindowAttributes(self.owner, None, Some(&mut alpha), Some(&mut flags))?;
            }
            if flags.0 & LWA_ALPHA.0 == 0 {
                alpha = 255;
            }
        }
        Ok((
            content,
            dpi,
            extent,
            alpha,
            unsafe { IsWindowVisible(self.owner) }.as_bool(),
            style & WS_EX_TOPMOST.0 as isize != 0,
        ))
    }
    fn hide(&self) {
        for part in &self.parts {
            if self.part_identity(part).is_ok() {
                unsafe {
                    let _ = ShowWindow(part.hwnd, SW_HIDE);
                }
            }
        }
    }
    fn fail(&self, message: String) {
        self.hide();
        self.ledger.update(|s| {
            if s.errors.len() < 16 {
                s.errors.push(message);
            }
        });
    }
    fn refresh(&self, force: bool) -> Result<()> {
        if self.busy.replace(true) {
            return Ok(());
        }
        struct Busy<'a>(&'a Cell<bool>);
        impl Drop for Busy<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _busy = Busy(&self.busy);
        let result = self.refresh_inner(force);
        if let Err(error) = &result {
            self.fail(error.to_string());
        }
        result
    }
    fn refresh_inner(&self, force: bool) -> Result<()> {
        let (content, dpi, extent, alpha, visible, topmost) = self.presentation()?;
        let shape = self
            .outline
            .get()
            .and_then(|outline| ShadowShape::new(content, outline));
        // 无外观或直角整像素轮廓走矩形路径；圆角轮廓的阴影向内补到透明四角。
        let (edge, inset, shape) = match shape {
            Some(shape) => match shape.integral_rect() {
                Some(rect) => (rect, 0, None),
                None => {
                    let (rect, inset) = shape.bounds()?;
                    (rect, inset, Some(shape))
                }
            },
            None => (content, 0, None),
        };
        let rectangles =
            presentation_perimeter(self.enabled.get(), edge, virtual_screen()?, extent, inset)?;
        let bytes =
            rectangles
                .iter()
                .flatten()
                .try_fold(0usize, |total, rect| -> Result<usize> {
                    total
                        .checked_add(rect.bytes()?)
                        .ok_or_else(|| "贴图阴影总尺寸溢出".into())
                })?;
        if bytes > SHADOW_BYTE_BUDGET {
            return Err("贴图阴影可见外缘超出内存预算".into());
        }
        self.ledger
            .update(|s| s.last_visible_perimeter_bytes = bytes);
        let color = presentation_color(
            self.active.get(),
            self.locked.get(),
            self.inactive_color.get(),
        );
        for (index, (part, rectangle)) in self.parts.iter().zip(rectangles).enumerate() {
            self.part_valid(part)?;
            part.rect.set(rectangle);
            if let Some(rect) = rectangle {
                let key = UploadKey {
                    content: edge,
                    shape: shape.map(ShadowShape::key),
                    rect,
                    dpi,
                    alpha,
                    color,
                };
                if force || part.key.get() != Some(key) || native_rect(part.hwnd)? != rect {
                    let mut dib = Dib::new(rect, self.ledger.clone())?;
                    match shape {
                        Some(shape) => fill_rounded_shadow_pixels(
                            dib.pixels(),
                            shape,
                            rect,
                            extent as f64,
                            color,
                            alpha,
                        )?,
                        None => fill_shadow_pixels(
                            dib.pixels(),
                            edge,
                            rect,
                            extent as f64,
                            color,
                            alpha,
                        )?,
                    }
                    self.part_valid(part)?;
                    unsafe {
                        UpdateLayeredWindow(
                            part.hwnd,
                            None,
                            Some(&POINT {
                                x: rect.x,
                                y: rect.y,
                            }),
                            Some(&SIZE {
                                cx: rect.width as i32,
                                cy: rect.height as i32,
                            }),
                            Some(dib.dc),
                            Some(&POINT { x: 0, y: 0 }),
                            COLORREF(0),
                            Some(&BLENDFUNCTION {
                                BlendOp: AC_SRC_OVER as u8,
                                BlendFlags: 0,
                                SourceConstantAlpha: 255,
                                AlphaFormat: AC_SRC_ALPHA as u8,
                            }),
                            ULW_ALPHA,
                        )?;
                    }
                    let mut revision = 0;
                    let unix_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis() as u64;
                    self.ledger.update(|s| {
                        s.successful_uploads += 1;
                        s.upload_revision += 1;
                        revision = s.upload_revision;
                        if s.first_successful_upload_unix_ms == 0 {
                            s.first_successful_upload_unix_ms = unix_ms;
                            s.first_upload_owner_visible = visible;
                        }
                        s.last_successful_upload_unix_ms = unix_ms;
                    });
                    let observer = UPLOAD_OBSERVER.with(|slot| slot.borrow().clone());
                    if let Some(observer) = observer {
                        observer(
                            &PinShadowUploadProbe {
                                part: PART_NAMES[index],
                                index,
                                hwnd: part.hwnd.0 as usize,
                                owner: self.owner.0 as usize,
                                token: self.token(),
                                rect,
                                content_rect: edge,
                                dpi,
                                extent,
                                revision,
                                effective_alpha: alpha,
                                owner_visible: visible,
                                premultiplied_bgra: true,
                            },
                            dib.pixels(),
                        )?;
                    }
                    part.key.set(Some(key));
                }
                let should_show = visible && alpha > 0;
                let currently_visible = unsafe { IsWindowVisible(part.hwnd) }.as_bool();
                let currently_topmost = unsafe { GetWindowLongPtrW(part.hwnd, GWL_EXSTYLE) }
                    & WS_EX_TOPMOST.0 as isize
                    != 0;
                if should_show != currently_visible || currently_topmost != topmost {
                    unsafe {
                        SetWindowPos(
                            part.hwnd,
                            Some(if topmost {
                                HWND_TOPMOST
                            } else {
                                HWND_NOTOPMOST
                            }),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOACTIVATE
                                | if should_show {
                                    SWP_SHOWWINDOW
                                } else {
                                    SWP_HIDEWINDOW
                                },
                        )?;
                    }
                }
            } else {
                unsafe {
                    let _ = ShowWindow(part.hwnd, SW_HIDE);
                }
            }
        }
        Ok(())
    }
    fn probe(&self) -> Result<PinShadowProbe> {
        let (content_rect, dpi, extent, effective_alpha, owner_visible, topmost) =
            self.presentation()?;
        let mut parts = Vec::with_capacity(4);
        for (index, part) in self.parts.iter().enumerate() {
            self.part_valid(part)?;
            let mut affinity = 0;
            unsafe {
                GetWindowDisplayAffinity(part.hwnd, &mut affinity)?;
            }
            parts.push(PinShadowPartProbe {
                part: PART_NAMES[index],
                index,
                hwnd: part.hwnd.0 as usize,
                pid: self.pid,
                tid: self.tid,
                class: class_name(part.hwnd)?,
                token: self.token(),
                owner: self.owner.0 as usize,
                rect: part.rect.get(),
                native_rect: native_rect(part.hwnd)?,
                visible: unsafe { IsWindowVisible(part.hwnd) }.as_bool(),
                affinity,
                exstyle: unsafe { GetWindowLongPtrW(part.hwnd, GWL_EXSTYLE) } as u32,
            });
        }
        Ok(PinShadowProbe {
            owner: self.owner.0 as usize,
            owner_pid: self.pid,
            owner_tid: self.tid,
            owner_class: self.owner_class.clone(),
            content_rect,
            dpi,
            extent,
            active: self.active.get(),
            locked: self.locked.get(),
            effective_alpha,
            owner_visible,
            topmost,
            visible_perimeter_bytes: self.ledger.snapshot().last_visible_perimeter_bytes,
            byte_budget: SHADOW_BYTE_BUDGET,
            parts,
        })
    }
}
impl Drop for PinShadow {
    fn drop(&mut self) {
        SHADOWS.with(|registry| {
            let same = registry
                .borrow()
                .get(&(self.core.owner.0 as usize))
                .is_some_and(|weak| Weak::ptr_eq(weak, &Rc::downgrade(&self.core)));
            if same {
                registry.borrow_mut().remove(&(self.core.owner.0 as usize));
            }
        });
        let mut failed = false;
        for part in &self.core.parts {
            let gone = !unsafe { IsWindow(Some(part.hwnd)) }.as_bool();
            let destroyed = gone
                || (self.core.part_identity(part).is_ok()
                    && unsafe { DestroyWindow(part.hwnd) }.is_ok());
            if destroyed {
                self.core
                    .ledger
                    .update(|s| s.destroyed_or_already_gone_windows += 1);
            } else {
                failed = true;
                self.core
                    .fail("贴图阴影原窗口释放失败，未操作身份不符的 HWND".into());
            }
        }
        if self.core.owner_valid().is_ok() {
            unsafe {
                let _ = RemovePropW(self.core.owner, OWNER_TOKEN);
            }
        }
        self.core.ledger.update(|s| s.closed = !failed);
        // 若原 HWND 确实仍活着，不允许 token 地址被分配器复用。
        if failed {
            std::mem::forget(self.core.ledger.token.clone());
        }
    }
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    bytes: usize,
    ledger: Arc<PinShadowLedger>,
}
impl Dib {
    fn new(rect: ShadowRect, ledger: Arc<PinShadowLedger>) -> Result<Self> {
        let bytes = rect.bytes()?;
        if bytes > SHADOW_BYTE_BUDGET {
            return Err("贴图阴影单边超出预算".into());
        }
        let mut result = Self {
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            bytes: 0,
            ledger,
        };
        unsafe {
            result.dc = CreateCompatibleDC(None);
            if result.dc.is_invalid() {
                return Err("贴图阴影 GDI DC 创建失败".into());
            }
            result.ledger.update(|s| s.live_dcs += 1);
            let info = crate::gdi_surface::bitmap_info(rect.width, rect.height);
            let mut bits = std::ptr::null_mut();
            result.bitmap =
                CreateDIBSection(Some(result.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            result.bytes = bytes;
            result.ledger.update(|s| {
                s.live_dibs += 1;
                s.current_staging_bytes += bytes;
                s.peak_staging_bytes = s.peak_staging_bytes.max(s.current_staging_bytes);
            });
            result.bits = bits.cast();
            if result.bits.is_null() {
                return Err("贴图阴影 DIB 像素不可用".into());
            }
            result.previous = SelectObject(result.dc, result.bitmap.into());
            if result.previous.is_invalid() {
                return Err("贴图阴影 DIB 选择失败".into());
            }
        }
        Ok(result)
    }
    fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.bytes) }
    }
}
impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                let _ = SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_invalid() {
                if !DeleteObject(self.bitmap.into()).as_bool() {
                    self.ledger
                        .update(|s| s.errors.push("贴图阴影 DIB 释放失败".into()));
                } else {
                    self.ledger.update(|s| {
                        s.live_dibs -= 1;
                        s.current_staging_bytes -= self.bytes;
                    });
                }
            }
            if !self.dc.is_invalid() {
                if !DeleteDC(self.dc).as_bool() {
                    self.ledger
                        .update(|s| s.errors.push("贴图阴影 DC 释放失败".into()));
                } else {
                    self.ledger.update(|s| s.live_dcs -= 1);
                }
            }
        }
    }
}
fn register_class() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
            let class = WNDCLASSW {
                hInstance: HINSTANCE(instance.0),
                lpszClassName: CLASS,
                lpfnWndProc: Some(shadow_proc),
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                Err(windows::core::Error::from_win32().to_string())
            } else {
                Ok(())
            }
        })
        .clone()
        .map_err(Into::into)
}
unsafe extern "system" fn shadow_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCDESTROY => {
            let _ = RemovePropW(h, PART_TOKEN);
            DefWindowProcW(h, message, wp, lp)
        }
        _ => DefWindowProcW(h, message, wp, lp),
    }
}
fn class_name(h: HWND) -> Result<String> {
    let mut name = [0u16; 256];
    let length = unsafe { GetClassNameW(h, &mut name) };
    if length <= 0 {
        return Err("贴图阴影窗口类名读取失败".into());
    }
    Ok(String::from_utf16_lossy(&name[..length as usize]))
}
fn native_rect(h: HWND) -> Result<ShadowRect> {
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(h, &mut r)?;
    }
    let width = i64::from(r.right) - i64::from(r.left);
    let height = i64::from(r.bottom) - i64::from(r.top);
    if width <= 0 || height <= 0 || width > i32::MAX as i64 || height > i32::MAX as i64 {
        return Err("贴图阴影窗口矩形无效".into());
    }
    Ok(ShadowRect {
        x: r.left,
        y: r.top,
        width: width as u32,
        height: height as u32,
    })
}
fn virtual_screen() -> Result<ShadowRect> {
    let r = ShadowRect {
        x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.try_into()?,
        height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.try_into()?,
    };
    if r.width == 0 || r.height == 0 {
        return Err("贴图阴影虚拟桌面为空".into());
    }
    Ok(r)
}
fn shadow_extent(dpi: u32) -> Result<u32> {
    if !(48..=768).contains(&dpi) {
        return Err("贴图阴影 DPI 超出有效范围".into());
    }
    Ok((dpi * 10).div_ceil(96))
}
fn presentation_perimeter(
    enabled: bool,
    content: ShadowRect,
    screen: ShadowRect,
    extent: u32,
    inset: u32,
) -> Result<[Option<ShadowRect>; 4]> {
    if enabled {
        perimeter_inset(content, screen, extent, inset)
    } else {
        Ok([None; 4])
    }
}
fn presentation_color(active: bool, locked: bool, inactive_color: u32) -> [u8; 3] {
    if locked {
        [40, 135, 220]
    } else if active {
        [205, 145, 70]
    } else {
        [
            inactive_color as u8,
            (inactive_color >> 8) as u8,
            (inactive_color >> 16) as u8,
        ]
    }
}
#[cfg(test)]
fn perimeter(
    content: ShadowRect,
    screen: ShadowRect,
    extent: u32,
) -> Result<[Option<ShadowRect>; 4]> {
    perimeter_inset(content, screen, extent, 0)
}
/// 四条边窗口；`inset` 让它们向内覆盖圆角外的透明四角（四者仍互不重叠）。
fn perimeter_inset(
    content: ShadowRect,
    screen: ShadowRect,
    extent: u32,
    inset: u32,
) -> Result<[Option<ShadowRect>; 4]> {
    let [x, y, right, bottom] = content.edges();
    let p = extent as i64;
    let i = inset as i64;
    let screen = screen.edges();
    let raw = [
        [x - p, y - p, right + p, y + i],
        [x - p, bottom - i, right + p, bottom + p],
        [x - p, y + i, x + i, bottom - i],
        [right - i, y + i, right + p, bottom - i],
    ];
    let mut result = [None; 4];
    let mut bytes = 0usize;
    for (index, [left, top, right, bottom]) in raw.into_iter().enumerate() {
        let left = left.max(screen[0]);
        let top = top.max(screen[1]);
        let right = right.min(screen[2]);
        let bottom = bottom.min(screen[3]);
        if right <= left || bottom <= top {
            continue;
        }
        let width = u32::try_from(right - left)?;
        let height = u32::try_from(bottom - top)?;
        if width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err("贴图阴影边矩形不可表示".into());
        }
        let r = ShadowRect {
            x: i32::try_from(left)?,
            y: i32::try_from(top)?,
            width,
            height,
        };
        bytes = bytes.checked_add(r.bytes()?).ok_or("贴图阴影总尺寸溢出")?;
        if bytes > SHADOW_BYTE_BUDGET {
            return Err("贴图阴影外缘超出预算".into());
        }
        result[index] = Some(r);
    }
    Ok(result)
}
/// 只复用同一 strip 内严格等色的像素；不保留图像缓存，也不改变原 shade 公式。
fn fill_shadow_pixels(
    pixels: &mut [u8],
    content: ShadowRect,
    rect: ShadowRect,
    extent: f64,
    color: [u8; 3],
    opacity: u8,
) -> Result<()> {
    let row_bytes = (rect.width as usize)
        .checked_mul(4)
        .ok_or("贴图阴影行尺寸溢出")?;
    if row_bytes == 0 || rect.height == 0 || pixels.len() != rect.bytes()? {
        return Err("贴图阴影填充尺寸无效".into());
    }
    let [left, top, right, bottom] = content.edges();
    let [x, y, far_x, far_y] = rect.edges();
    if (far_x <= left || x >= right) && y >= top && far_y <= bottom {
        // 左右外缘 dy 恒为 0；第一行仍用原 scalar 公式，其余行逐字节复制。
        let (first, rest) = pixels.split_at_mut(row_bytes);
        for (column, pixel) in first.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&shadow_pixel(
                content,
                rect.x as f64 + column as f64 + 0.5,
                rect.y as f64 + 0.5,
                extent,
                color,
                opacity,
            ));
        }
        for row in rest.chunks_exact_mut(row_bytes) {
            row.copy_from_slice(first);
        }
    } else if far_y <= top || y >= bottom {
        // 上下外缘每行中心段 dx 恒为 0；裁剪后可能没有中心段，角落照常计算。
        let start = (left - x).clamp(0, rect.width as i64) as usize;
        let end = (right - x).clamp(0, rect.width as i64) as usize;
        for (row_index, row) in pixels.chunks_exact_mut(row_bytes).enumerate() {
            let sample_y = rect.y as f64 + row_index as f64 + 0.5;
            let (before, middle_and_after) = row.split_at_mut(start * 4);
            let (middle, after) = middle_and_after.split_at_mut((end - start) * 4);
            for (column, pixel) in before.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&shadow_pixel(
                    content,
                    rect.x as f64 + column as f64 + 0.5,
                    sample_y,
                    extent,
                    color,
                    opacity,
                ));
            }
            if !middle.is_empty() {
                let shade = shadow_pixel(
                    content,
                    rect.x as f64 + start as f64 + 0.5,
                    sample_y,
                    extent,
                    color,
                    opacity,
                );
                for pixel in middle.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&shade);
                }
            }
            for (column, pixel) in after.chunks_exact_mut(4).enumerate() {
                pixel.copy_from_slice(&shadow_pixel(
                    content,
                    rect.x as f64 + (end + column) as f64 + 0.5,
                    sample_y,
                    extent,
                    color,
                    opacity,
                ));
            }
        }
    } else {
        // 非外围 strip 的调用仍按原 scalar 路径填满，不凭索引假设几何。
        for (n, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&shadow_pixel(
                content,
                rect.x as f64 + (n % rect.width as usize) as f64 + 0.5,
                rect.y as f64 + (n / rect.width as usize) as f64 + 0.5,
                extent,
                color,
                opacity,
            ));
        }
    }
    Ok(())
}

/// 圆角轮廓的阴影：按到圆角矩形的距离衰减，轮廓边缘按半像素覆盖淡入，内部完全透明。
fn fill_rounded_shadow_pixels(
    pixels: &mut [u8],
    shape: ShadowShape,
    rect: ShadowRect,
    extent: f64,
    color: [u8; 3],
    opacity: u8,
) -> Result<()> {
    if rect.width == 0 || rect.height == 0 || pixels.len() != rect.bytes()? {
        return Err("贴图阴影填充尺寸无效".into());
    }
    for (n, pixel) in pixels.chunks_exact_mut(4).enumerate() {
        let x = rect.x as f64 + (n % rect.width as usize) as f64 + 0.5;
        let y = rect.y as f64 + (n / rect.width as usize) as f64 + 0.5;
        let sd = shape.signed_distance(x, y);
        let edge = (sd + 0.5).clamp(0., 1.);
        pixel.copy_from_slice(&shade(sd.max(0.), extent, edge, color, opacity));
    }
    Ok(())
}

fn shade(distance: f64, extent: f64, coverage: f64, color: [u8; 3], opacity: u8) -> [u8; 4] {
    let t = (distance / extent).clamp(0., 1.);
    let alpha =
        (92. * (1. - t).powi(3) * (1. + 2. * t) * opacity as f64 / 255. * coverage).round() as u8;
    [
        ((color[0] as u16 * alpha as u16 + 127) / 255) as u8,
        ((color[1] as u16 * alpha as u16 + 127) / 255) as u8,
        ((color[2] as u16 * alpha as u16 + 127) / 255) as u8,
        alpha,
    ]
}

fn shadow_pixel(
    content: ShadowRect,
    x: f64,
    y: f64,
    extent: f64,
    color: [u8; 3],
    opacity: u8,
) -> [u8; 4] {
    let [left, top, right, bottom] = content.edges().map(|n| n as f64);
    if x >= left && x < right && y >= top && y < bottom {
        return [0; 4];
    }
    let dx = (left - x).max(0.).max(x - right);
    let dy = (top - y).max(0.).max(y - bottom);
    shade(dx.hypot(dy), extent, 1., color, opacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_shadow_follows_rounded_outline_and_keeps_corners_clear() {
        let window = ShadowRect {
            x: -40,
            y: 20,
            width: 100,
            height: 80,
        };
        let outline = ShadowOutline {
            left: 0.,
            top: 0.,
            width: 100.,
            height: 80.,
            radius: 16.,
        };
        let shape = ShadowShape::new(window, outline).unwrap();
        assert_eq!(shape.integral_rect(), None);
        let (edge, inset) = shape.bounds().unwrap();
        assert_eq!((edge, inset), (window, 17));
        let screen = ShadowRect {
            x: -1000,
            y: -1000,
            width: 3000,
            height: 3000,
        };
        let parts = perimeter_inset(edge, screen, 10, inset).unwrap();
        let mut seen = std::collections::HashSet::new();
        for rect in parts.iter().flatten() {
            let mut pixels = vec![0; rect.bytes().unwrap()];
            fill_rounded_shadow_pixels(&mut pixels, shape, *rect, 10., [0, 0, 0], 255).unwrap();
            for row in 0..rect.height as i32 {
                for column in 0..rect.width as i32 {
                    // 四条边窗口互不重叠。
                    assert!(seen.insert((rect.x + column, rect.y + row)));
                }
            }
            let alpha = |x: i32, y: i32| {
                let n = ((y - rect.y) as usize * rect.width as usize + (x - rect.x) as usize) * 4;
                pixels[n + 3]
            };
            if rect.y < window.y {
                // 窗口左上角像素在圆弧外：透明四角由阴影覆盖，而不是直角光晕的空洞。
                let corner = alpha(window.x, window.y);
                assert!(corner > 0 && corner < alpha(window.x + 50, window.y - 1));
                // 圆角内部完全透明，不遮住内容。
                assert_eq!(alpha(window.x + 50, window.y + 10), 0);
                // 直边外侧与旧矩形阴影一致。
                assert_eq!(
                    alpha(window.x + 50, window.y - 3),
                    shadow_pixel(window, 10.5, -2.5 + 20., 10., [0, 0, 0], 255)[3]
                );
            }
        }
        // 直角整像素轮廓沿用矩形路径。
        let square = ShadowShape::new(
            window,
            ShadowOutline {
                radius: 0.,
                ..outline
            },
        )
        .unwrap();
        assert_eq!(square.integral_rect(), Some(window));
    }

    #[test]
    fn pin_shadow_disabled_has_no_perimeter_and_theme_changes_only_decorative_color() {
        let screen = ShadowRect {
            x: -1920,
            y: -700,
            width: 3840,
            height: 1800,
        };
        for content in [
            ShadowRect {
                x: -1921,
                y: -701,
                width: 22,
                height: 100,
            },
            ShadowRect {
                x: -1,
                y: -1,
                width: 1,
                height: 1,
            },
        ] {
            let expected = perimeter(content, screen, 10).unwrap();
            assert_eq!(
                presentation_perimeter(true, content, screen, 10, 0).unwrap(),
                expected
            );
            assert_eq!(
                presentation_perimeter(false, content, screen, 10, 0).unwrap(),
                [None; 4]
            );
            assert_eq!(
                presentation_perimeter(true, content, screen, 10, 0).unwrap(),
                expected
            );
        }
        let light = crate::capture_toolbar::Palette::new(true).muted;
        let dark = crate::capture_toolbar::Palette::new(false).muted;
        assert_eq!(presentation_color(false, false, light), [0x82, 0x74, 0x66]);
        assert_eq!(presentation_color(false, false, dark), [0xa6, 0x97, 0x89]);
        for neutral in [light, dark] {
            assert_eq!(presentation_color(true, false, neutral), [205, 145, 70]);
            assert_eq!(presentation_color(false, true, neutral), [40, 135, 220]);
            assert_eq!(presentation_color(true, true, neutral), [40, 135, 220]);
        }
    }

    fn scalar_shadow_pixels(
        content: ShadowRect,
        rect: ShadowRect,
        extent: f64,
        color: [u8; 3],
        opacity: u8,
    ) -> Vec<u8> {
        // 完整保留优化前的逐像素坐标/公式调用，作为独立分组与复制的 oracle。
        let mut pixels = vec![0; rect.bytes().unwrap()];
        for (n, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let x = rect.x as f64 + (n % rect.width as usize) as f64 + 0.5;
            let y = rect.y as f64 + (n / rect.width as usize) as f64 + 0.5;
            pixel.copy_from_slice(&shadow_pixel(content, x, y, extent, color, opacity));
        }
        pixels
    }

    fn assert_shadow_fill_matches_scalar(
        content: ShadowRect,
        rect: ShadowRect,
        extent: f64,
        color: [u8; 3],
        opacity: u8,
    ) {
        assert!(rect.bytes().unwrap() <= SHADOW_BYTE_BUDGET);
        let expected = scalar_shadow_pixels(content, rect, extent, color, opacity);
        let mut actual = vec![0xa5; expected.len()];
        fill_shadow_pixels(&mut actual, content, rect, extent, color, opacity).unwrap();
        let difference = actual
            .iter()
            .zip(&expected)
            .enumerate()
            .find(|(_, (actual, expected))| actual != expected)
            .map(|(index, (actual, expected))| (index, *actual, *expected));
        assert!(
            difference.is_none(),
            "阴影整条 DIB 与旧 scalar 不同：content={content:?}, rect={rect:?}, extent={extent}, color={color:?}, opacity={opacity}, first_difference={difference:?}"
        );
    }

    fn assert_shadow_perimeter_matches_scalar(
        content: ShadowRect,
        screen: ShadowRect,
        dpi: u32,
        color: [u8; 3],
        opacity: u8,
    ) -> usize {
        let extent = shadow_extent(dpi).unwrap();
        let parts = perimeter(content, screen, extent).unwrap();
        let mut count = 0;
        for rect in parts.into_iter().flatten() {
            assert_shadow_fill_matches_scalar(content, rect, extent as f64, color, opacity);
            count += 1;
        }
        count
    }

    #[test]
    fn pin_shadow_exact_fill_all_parts_colors_opacity_and_dpi_match_scalar() {
        let content = ShadowRect {
            x: -40,
            y: 15,
            width: 97,
            height: 61,
        };
        let screen = ShadowRect {
            x: -500,
            y: -500,
            width: 1500,
            height: 1500,
        };
        for dpi in [48, 96, 120, 144, 192, 384, 768] {
            for color in [[45, 45, 45], [205, 145, 70], [40, 135, 220]] {
                for opacity in [0, 1, 127, 254, 255] {
                    assert_eq!(
                        assert_shadow_perimeter_matches_scalar(
                            content, screen, dpi, color, opacity
                        ),
                        4
                    );
                }
            }
        }
    }

    #[test]
    fn pin_shadow_exact_fill_clipped_negative_corner_only_and_one_pixel_match_scalar() {
        let content = ShadowRect {
            x: -30,
            y: -20,
            width: 17,
            height: 11,
        };
        let screens = [
            ShadowRect {
                x: -100,
                y: -100,
                width: 300,
                height: 300,
            },
            ShadowRect {
                x: -39,
                y: -29,
                width: 3,
                height: 3,
            },
            ShadowRect {
                x: -11,
                y: -29,
                width: 3,
                height: 3,
            },
            ShadowRect {
                x: -39,
                y: -19,
                width: 1,
                height: 1,
            },
            ShadowRect {
                x: -28,
                y: -8,
                width: 7,
                height: 1,
            },
            ShadowRect {
                x: -30,
                y: -21,
                width: 17,
                height: 1,
            },
            ShadowRect {
                x: -30,
                y: -20,
                width: 17,
                height: 11,
            },
        ];
        for dpi in [96, 120, 768] {
            for screen in screens {
                assert_shadow_perimeter_matches_scalar(content, screen, dpi, [205, 145, 70], 127);
            }
        }
        assert_eq!(
            assert_shadow_perimeter_matches_scalar(content, content, 96, [45, 45, 45], 255),
            0
        );
        for x in [i32::MIN + 100, i32::MAX - 100] {
            let content = ShadowRect {
                x,
                y: -10,
                width: 31,
                height: 9,
            };
            let screen = ShadowRect {
                x: x - 80,
                y: -100,
                width: 160,
                height: 200,
            };
            assert_shadow_perimeter_matches_scalar(content, screen, 768, [40, 135, 220], 255);
        }
    }

    #[test]
    fn pin_shadow_exact_fill_thin_regular_and_clipped_long_perimeter_match_scalar() {
        let screen = ShadowRect {
            x: -1920,
            y: -200,
            width: 5760,
            height: 2360,
        };
        for (width, height) in [(1, 1), (1, 300), (300, 1), (2528, 813), (960, 32000)] {
            let content = ShadowRect {
                x: -1300,
                y: 100,
                width,
                height,
            };
            for dpi in [48, 120, 192, 768] {
                assert!(
                    assert_shadow_perimeter_matches_scalar(content, screen, dpi, [45, 45, 45], 255)
                        > 0
                );
            }
        }
        // 不给超预算长外缘分配 oracle：生产 perimeter 必须先拒绝。
        let long = ShadowRect {
            x: 100,
            y: 100,
            width: 960,
            height: 32000,
        };
        let unbounded_screen = ShadowRect {
            x: 0,
            y: 0,
            width: 5000,
            height: 40000,
        };
        assert!(perimeter(long, unbounded_screen, shadow_extent(768).unwrap()).is_err());
    }

    #[test]
    fn pin_shadow_exact_fill_changed_inputs_scalar_fallback_and_bad_lengths_are_checked() {
        let content = ShadowRect {
            x: -10,
            y: 20,
            width: 19,
            height: 13,
        };
        let rectangles = [
            ShadowRect {
                x: -15,
                y: 20,
                width: 5,
                height: 13,
            },
            ShadowRect {
                x: -15,
                y: 15,
                width: 29,
                height: 5,
            },
            // 穿过内容区的矩形不符合外围资格，必须走完整 scalar 回退。
            ShadowRect {
                x: -11,
                y: 19,
                width: 21,
                height: 15,
            },
        ];
        for rect in rectangles {
            let mut actual = vec![0xa5; rect.bytes().unwrap()];
            for (extent, color, opacity) in [
                (5., [45, 45, 45], 255),
                (10., [205, 145, 70], 127),
                (80., [40, 135, 220], 1),
                (5., [45, 45, 45], 0),
            ] {
                fill_shadow_pixels(&mut actual, content, rect, extent, color, opacity).unwrap();
                let expected = scalar_shadow_pixels(content, rect, extent, color, opacity);
                assert!(actual.iter().zip(&expected).all(|(a, b)| a == b));
            }
        }
        let rect = ShadowRect {
            x: -15,
            y: 20,
            width: 5,
            height: 13,
        };
        let mut short = vec![0xa5; rect.bytes().unwrap() - 1];
        let mut oversized = vec![0xa5; rect.bytes().unwrap() + 1];
        assert!(fill_shadow_pixels(&mut short, content, rect, 5., [45, 45, 45], 255).is_err());
        assert!(short.iter().all(|b| *b == 0xa5));
        assert!(fill_shadow_pixels(&mut oversized, content, rect, 5., [45, 45, 45], 255).is_err());
        assert!(oversized.iter().all(|b| *b == 0xa5));
        for empty in [
            ShadowRect { width: 0, ..rect },
            ShadowRect { height: 0, ..rect },
        ] {
            assert!(fill_shadow_pixels(&mut [], content, empty, 5., [45, 45, 45], 255).is_err());
        }
        let overflowing = ShadowRect {
            width: u32::MAX,
            height: u32::MAX,
            ..rect
        };
        assert!(overflowing.bytes().is_err());
        assert!(fill_shadow_pixels(&mut [], content, overflowing, 5., [45, 45, 45], 255).is_err());
    }

    #[test]
    fn pin_shadow_visible_perimeter_budget_and_physical_geometry() {
        let screen = ShadowRect {
            x: -1920,
            y: -200,
            width: 5760,
            height: 2360,
        };
        for dpi in [96, 120, 192, 768] {
            let extent = shadow_extent(dpi).unwrap();
            for content in [
                ShadowRect {
                    x: -1700,
                    y: -100,
                    width: 960,
                    height: 32000,
                },
                ShadowRect {
                    x: -2000,
                    y: -500,
                    width: 4000,
                    height: 900,
                },
                ShadowRect {
                    x: i32::MIN,
                    y: i32::MAX,
                    width: u32::MAX,
                    height: u32::MAX,
                },
            ] {
                let parts = perimeter(content, screen, extent).unwrap();
                let mut total = 0;
                for (i, a) in parts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| r.map(|r| (i, r)))
                {
                    let ae = a.edges();
                    let ce = content.edges();
                    let se = screen.edges();
                    assert!(ae[0] >= se[0] && ae[1] >= se[1] && ae[2] <= se[2] && ae[3] <= se[3]);
                    assert!(ae[2] <= ce[0] || ae[0] >= ce[2] || ae[3] <= ce[1] || ae[1] >= ce[3]);
                    for b in parts.iter().skip(i + 1).flatten() {
                        let be = b.edges();
                        assert!(
                            ae[2] <= be[0] || ae[0] >= be[2] || ae[3] <= be[1] || ae[1] >= be[3]
                        );
                    }
                    total += a.bytes().unwrap();
                }
                assert!(total <= SHADOW_BYTE_BUDGET);
                if content.height == 32000 {
                    assert!(total < content.width as usize * content.height as usize * 4 / 20);
                }
            }
        }
    }
    #[test]
    fn pin_shadow_soft_contact_falloff_and_corner_energy() {
        let content = ShadowRect {
            x: -100,
            y: 40,
            width: 320,
            height: 240,
        };
        let color = [205, 145, 70];
        let mut previous = 255;
        for distance in [0.5, 1., 2., 4., 6., 8., 10., 12.] {
            let side = shadow_pixel(content, -100. - distance, 100., 10., color, 255);
            assert!(side[3] <= previous);
            assert!(side[..3].iter().all(|c| *c <= side[3]));
            let corner = shadow_pixel(
                content,
                -100. - distance * 0.6,
                40. - distance * 0.8,
                10.,
                color,
                255,
            );
            assert!((side[3] as i32 - corner[3] as i32).abs() <= 1);
            previous = side[3];
        }
        assert!(shadow_pixel(content, -100.5, 100., 10., color, 255)[3] >= 75);
        assert_eq!(previous, 0);
        assert_eq!(shadow_pixel(content, -50., 100., 10., color, 255), [0; 4]);
        assert_eq!(shadow_pixel(content, -100.5, 100., 10., color, 0), [0; 4]);
    }
}
