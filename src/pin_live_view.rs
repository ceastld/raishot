//! 独立原生宿主承载 DWM 缩略图；GPUI 仅处理投递事件和静态贴图交接。
use crate::{
    capture::Image,
    geometry::ScreenRect,
    pin_live_dwm::{probe, Thumbnail},
    pin_live_model::{client_region, source_region, zoom_placement, SourceIdentity, SourceState},
    pin_live_snapshot,
    pin_model::{PinState, Placement},
    pin_native,
    pin_view::PinView,
    pin_workspace::{self as store, PinStatus, Session},
    selection_assist::WindowTarget,
    Result,
};
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    future::{select, Either},
    StreamExt,
};
use gpui::App;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
            WindowsAndMessaging::*,
        },
    },
};

const CAPTION: i32 = 26;
const MENU_WIDTH: i32 = 238;
const MENU_ROW: i32 = 30;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static CLASS: Once = Once::new();

pub(crate) fn has_open_windows() -> bool {
    ACTIVE.load(Ordering::Acquire) != 0
}

#[derive(Clone, Copy)]
struct NativeEvent {
    hwnd: usize,
    message: u32,
    wparam: usize,
    lparam: isize,
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let sender =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const UnboundedSender<NativeEvent>;
    if !sender.is_null()
        && matches!(
            message,
            WM_PAINT
                | WM_CLOSE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_MOUSEMOVE
                | WM_MOUSEWHEEL
                | WM_MBUTTONDOWN
                | WM_RBUTTONUP
                | WM_KEYDOWN
                | WM_CAPTURECHANGED
                | WM_NULL
                | WM_SETTINGCHANGE
                | WM_KILLFOCUS
        )
    {
        let _ = unsafe { &*sender }.unbounded_send(NativeEvent {
            hwnd: hwnd.0 as usize,
            message,
            wparam: wparam.0,
            lparam: lparam.0,
        });
        if message == WM_CLOSE {
            return LRESULT(0);
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

struct Host {
    hwnd: HWND,
    sender: Box<UnboundedSender<NativeEvent>>,
}
impl Host {
    fn create(
        sender: UnboundedSender<NativeEvent>,
        bounds: Placement,
        owner: Option<HWND>,
        title: &str,
    ) -> Result<Self> {
        CLASS.call_once(|| unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                lpszClassName: w!("RustShot.LivePin.v1"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
        });
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("RustShot.LivePin.v1"),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                bounds.x,
                bounds.y,
                bounds.width as i32,
                bounds.height as i32,
                owner,
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?
        };
        let sender = Box::new(sender);
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, sender.as_ref() as *const _ as isize);
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        Ok(Self { hwnd, sender })
    }
    fn show(&self, show: bool) {
        unsafe {
            let _ = ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
        // sender 留到 HWND 完全销毁后才释放，WindowProc 不持有悬空指针。
        let _ = &self.sender;
    }
}

struct LivePin {
    thumbnail: Option<Thumbnail>,
    host: Host,
    menu: Option<Host>,
    sender: UnboundedSender<NativeEvent>,
    source: SourceIdentity,
    region: ScreenRect,
    fallback: Arc<Image>,
    placement: Placement,
    state: PinState,
    status: SourceState,
    light: bool,
    drag: Option<((i32, i32), Placement)>,
    session: Option<Session>,
    refresh: usize,
    hidden: bool,
}
impl Drop for LivePin {
    fn drop(&mut self) {
        self.thumbnail.take();
        self.menu.take();
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
enum Finish {
    Static,
    Close,
}

pub(crate) fn open(
    target: &WindowTarget,
    selection: ScreenRect,
    fallback: Image,
    cx: &mut App,
) -> Result<()> {
    let source = SourceIdentity {
        hwnd: target.hwnd,
        process_id: target.process_id,
        thread_id: target.thread_id,
    };
    if source.process_id == std::process::id() {
        return Err(trf!(
            "不能对 {} 自己的窗口做实时贴图",
            "Cannot live-pin a {} window",
            crate::brand::NAME
        )
        .into());
    }
    let observed = probe(source);
    if observed.state != SourceState::Live {
        return Err(tr!("源窗口当前不可用", "Source window unavailable").into());
    }
    let frozen = target.global_bounds;
    if observed.window.left != frozen.left
        || observed.window.top != frozen.top
        || i64::from(observed.window.left) + i64::from(observed.window.width)
            != i64::from(frozen.right)
        || i64::from(observed.window.top) + i64::from(observed.window.height)
            != i64::from(frozen.bottom)
    {
        return Err(tr!(
            "源窗口位置或尺寸已改变，请重新框选",
            "Source window moved or resized; select again"
        )
        .into());
    }
    let region = client_region(selection, observed.client).ok_or(tr!(
        "选区未覆盖源窗口客户区",
        "Selection misses the window content"
    ))?;
    let clipped = ScreenRect {
        left: observed.client.left + region.left,
        top: observed.client.top + region.top,
        width: region.width,
        height: region.height,
    };
    let fallback = crop_fallback(fallback, selection, clipped)?;
    let state = PinState {
        zoom: crate::pin_live_model::bounded_zoom(
            (region.width, region.height),
            pin_model_fit((region.width, region.height), (clipped.left, clipped.top)),
        ),
        shadow_enabled: false,
        ..Default::default()
    };
    let placement = Placement {
        x: clipped.left,
        y: clipped.top,
        width: (region.width as f64 * state.zoom as f64).round().max(1.) as u32,
        height: (region.height as f64 * state.zoom as f64).round().max(1.) as u32,
    };
    let (sender, receiver) = unbounded();
    let host = Host::create(
        sender.clone(),
        host_bounds(placement),
        None,
        brand_title!("实时窗口", "Live window"),
    )?;
    let thumbnail = Thumbnail::register(host.hwnd, HWND(source.hwnd as *mut _))?;
    let source_rect = source_region(
        region,
        observed.client,
        observed.window,
        observed.extended,
        thumbnail.size()?,
    )
    .ok_or(tr!("源窗口边框尺寸不一致", "Window bounds do not match"))?;
    thumbnail.update(source_rect, destination(placement), 255, true)?;
    let session = Session::register(state.clone(), placement)?;
    pin_native::mark(host.hwnd)?;
    unsafe {
        SetPropW(
            host.hwnd,
            w!("RustShot.Pin.WorkspaceScope"),
            Some(HANDLE(store::scope() as usize as *mut _)),
        )?;
        SetPropW(
            host.hwnd,
            w!("RustShot.Pin.WorkspaceId"),
            Some(HANDLE(id_hash(&session.id) as usize as *mut _)),
        )?;
    }
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    let pin = LivePin {
        thumbnail: Some(thumbnail),
        host,
        menu: None,
        sender,
        source,
        region,
        fallback: Arc::new(fallback),
        placement,
        state,
        status: SourceState::Live,
        light: crate::theme::is_light(cx),
        drag: None,
        session: Some(session),
        refresh: 0,
        hidden: false,
    };
    pin.paint();
    pin.host.show(true);
    let pixels = pin.fallback.clone();
    let id = pin.session.as_ref().unwrap().id.clone();
    let persisted_state = pin.state.clone();
    let first_save = cx.background_executor().spawn(async move {
        store::save_snapshot(&id, &pixels, &persisted_state, placement, PinStatus::Active)
            .map_err(|error| error.to_string())
    });
    cx.spawn(async move |cx| {
        if let Err(error) = first_save.await {
            eprintln!("窗口贴图冻结快照保存失败：{error}");
        }
        run(pin, receiver, cx).await;
    })
    .detach();
    Ok(())
}

fn pin_model_fit(size: (u32, u32), origin: (i32, i32)) -> f32 {
    crate::pin_model::fit(size, pin_native::work_area(origin))
}
fn id_hash(id: &str) -> u64 {
    id.bytes()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
        .max(1)
}
fn host_bounds(p: Placement) -> Placement {
    Placement {
        y: p.y.saturating_sub(CAPTION),
        height: p.height.saturating_add(CAPTION as u32),
        ..p
    }
}
fn destination(p: Placement) -> ScreenRect {
    ScreenRect {
        left: 0,
        top: CAPTION,
        width: p.width,
        height: p.height,
    }
}
fn coordinates(lparam: isize) -> (i32, i32) {
    (lparam as i16 as i32, (lparam >> 16) as i16 as i32)
}

fn crop_fallback(image: Image, selection: ScreenRect, clipped: ScreenRect) -> Result<Image> {
    if image.width != selection.width
        || image.height != selection.height
        || image.bgra.len() != image.width as usize * image.height as usize * 4
    {
        return Err("窗口贴图冻结图尺寸与选区不一致".into());
    }
    if selection == clipped {
        return Ok(image);
    }
    let x = (i64::from(clipped.left) - i64::from(selection.left)) as usize;
    let y = (i64::from(clipped.top) - i64::from(selection.top)) as usize;
    let mut bgra = Vec::with_capacity(clipped.width as usize * clipped.height as usize * 4);
    for row in y..y + clipped.height as usize {
        let start = (row * image.width as usize + x) * 4;
        bgra.extend_from_slice(&image.bgra[start..start + clipped.width as usize * 4]);
    }
    Ok(Image {
        width: clipped.width,
        height: clipped.height,
        bgra,
        readback_ms: image.readback_ms,
    })
}

async fn run(
    mut pin: LivePin,
    mut receiver: UnboundedReceiver<NativeEvent>,
    cx: &mut gpui::AsyncApp,
) {
    loop {
        let mut tick = Instant::now();
        let finish = loop {
            let delay = tick.saturating_duration_since(Instant::now());
            let next = receiver.next();
            let timer = cx.background_executor().timer(delay);
            futures::pin_mut!(next, timer);
            let event = match select(next, timer).await {
                Either::Left((event, _)) => event,
                Either::Right(_) => None,
            };
            if let Some(event) = event {
                match cx.update(|cx| pin.event(event, cx)) {
                    Ok(Ok(Some(finish))) => break finish,
                    Ok(Err(error)) => eprintln!("窗口贴图操作失败：{error}"),
                    Err(_) => return,
                    _ => {}
                }
            }
            if Instant::now() >= tick {
                if cx.update(|cx| pin.tick(cx)).is_err() {
                    return;
                }
                tick = Instant::now() + Duration::from_millis(200);
            }
        };
        pin.menu.take();
        let identity = pin.source;
        let region = pin.region;
        let fallback = pin.fallback.clone();
        let id = pin.session.as_ref().unwrap().id.clone();
        let state = pin.state.clone();
        let placement = pin.placement;
        let preserve =
            !unsafe { GetPropW(pin.host.hwnd, w!("RustShot.Pin.ExitPreserve")) }.is_invalid();
        let status = if matches!(finish, Finish::Static) || preserve {
            PinStatus::Active
        } else {
            PinStatus::Closed
        };
        let saved = cx
            .background_executor()
            .spawn(async move {
                let (image, latest) = pin_live_snapshot::capture(identity, region, &fallback);
                let result = store::save_snapshot(&id, &image, &state, placement, status)
                    .map_err(|error| error.to_string());
                (image, latest, result)
            })
            .await;
        if let Err(error) = &saved.2 {
            eprintln!("窗口贴图静态快照保存失败：{error}");
            if matches!(finish, Finish::Static) {
                pin.status = SourceState::Unavailable;
                if let Some(thumbnail) = &pin.thumbnail {
                    let _ = thumbnail.hide();
                }
                pin.paint();
                // 未提交快照时保留实时会话和租约，可再次尝试，不能移交旧清单配新像素。
                continue;
            }
        }
        if matches!(finish, Finish::Static) {
            let session = pin.session.take().unwrap();
            let id = session.id.clone();
            let image = saved.0;
            let latest = saved.1;
            let _ = cx
                .update(|cx| {
                    let entry = store::load()?
                        .pins
                        .into_iter()
                        .find(|entry| entry.id == id)
                        .ok_or("窗口贴图记录丢失")?;
                    pin.host.show(false);
                    let window = PinView::open_session(image, session, entry, cx)?;
                    if !latest {
                        let _ = window.update(cx, |view, _, cx| {
                            view.status = tr!(
                                "已使用冻结画面，可能不是最新内容",
                                "Frozen image; content may be outdated"
                            )
                            .into();
                            cx.notify();
                        });
                    }
                    Ok::<_, Box<dyn std::error::Error>>(())
                })
                .map(|result| {
                    if let Err(error) = result {
                        eprintln!("转静态贴图失败：{error}");
                    }
                });
        } else if !preserve && !crate::preferences::Preferences::load().pin_restore {
            if let Some(session) = pin.session.as_ref() {
                let _ = session.forget();
            }
        }
        break;
    }
    drop(pin);
    let _ = cx.update(|cx| {
        crate::pin_workspace_runtime::broadcast();
        if cx.windows().is_empty()
            && !has_open_windows()
            && !crate::pin_workspace_runtime::is_restoring(cx)
        {
            cx.quit();
        }
    });
}

impl LivePin {
    fn event(&mut self, event: NativeEvent, _cx: &mut App) -> Result<Option<Finish>> {
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.hwnd.0 as usize == event.hwnd)
        {
            match event.message {
                WM_PAINT => self.paint_menu(),
                WM_LBUTTONDOWN => {
                    let (_, y) = coordinates(event.lparam);
                    self.menu.take();
                    match y / MENU_ROW {
                        0 => return Ok(Some(Finish::Static)),
                        1 | 2 => {
                            if probe(self.source).state != SourceState::Lost {
                                let hwnd = HWND(self.source.hwnd as *mut _);
                                unsafe {
                                    if y / MENU_ROW == 2 {
                                        let _ = ShowWindowAsync(hwnd, SW_RESTORE);
                                    }
                                    let _ = SetForegroundWindow(hwnd);
                                }
                            }
                        }
                        3 => {
                            let through = !pin_native::transparent(self.host.hwnd);
                            pin_native::click_through(self.host.hwnd, through)?;
                            self.session
                                .as_ref()
                                .unwrap()
                                .persist_interaction(self.state.locked, through)?;
                        }
                        4 => return Ok(Some(Finish::Close)),
                        _ => {}
                    }
                }
                WM_CLOSE | WM_KILLFOCUS => {
                    self.menu.take();
                }
                WM_KEYDOWN if event.wparam == 0x1b => {
                    self.menu.take();
                }
                _ => {}
            }
            return Ok(None);
        }
        if event.hwnd != self.host.hwnd.0 as usize {
            return Ok(None);
        }
        match event.message {
            WM_PAINT => self.paint(),
            WM_CLOSE => return Ok(Some(Finish::Close)),
            WM_RBUTTONUP => self.open_menu()?,
            WM_KEYDOWN if event.wparam == 0x1b => {
                if self.menu.take().is_none() {
                    return Ok(Some(Finish::Close));
                }
            }
            WM_LBUTTONDOWN => {
                self.menu.take();
                let (x, y) = coordinates(event.lparam);
                if y < CAPTION && x >= self.placement.width as i32 - 24 {
                    return Ok(Some(Finish::Close));
                }
                if !self.state.locked {
                    let cursor = cursor();
                    self.drag = Some((cursor, self.placement));
                    unsafe {
                        SetCapture(self.host.hwnd);
                    }
                }
            }
            WM_MOUSEMOVE => {
                if let Some((start, placement)) = self.drag {
                    let cursor = cursor();
                    self.placement.x = placement.x.saturating_add(cursor.0.saturating_sub(start.0));
                    self.placement.y = placement.y.saturating_add(cursor.1.saturating_sub(start.1));
                    self.place()?;
                }
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                if self.drag.take().is_some() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.persist()?;
                }
            }
            WM_MBUTTONDOWN => self.zoom(1.)?,
            WM_MOUSEWHEEL => {
                let delta = (event.wparam >> 16) as i16 as f32 / 120.;
                if event.wparam & 0x8 != 0 {
                    self.state.opacity = (f32::from(self.state.opacity) + delta * 16.)
                        .round()
                        .clamp(32., 255.) as u8;
                    pin_native::opacity(self.host.hwnd, self.state.opacity)?;
                    self.persist()?;
                } else {
                    self.zoom(self.state.zoom * 1.1_f32.powf(delta))?;
                }
            }
            _ => {}
        }
        Ok(None)
    }
    fn persist(&self) -> Result<()> {
        store::update_state(
            &self.session.as_ref().unwrap().id,
            &self.state,
            self.placement,
        )
    }
    fn zoom(&mut self, zoom: f32) -> Result<()> {
        (self.placement, self.state.zoom) = zoom_placement(
            self.placement,
            (self.region.width, self.region.height),
            zoom,
            cursor(),
        );
        self.place()?;
        self.refresh_thumbnail();
        self.persist()
    }
    fn place(&self) -> Result<()> {
        let bounds = host_bounds(self.placement);
        unsafe {
            SetWindowPos(
                self.host.hwnd,
                None,
                bounds.x,
                bounds.y,
                bounds.width as i32,
                bounds.height as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )?;
        }
        self.paint();
        Ok(())
    }
    fn tick(&mut self, cx: &App) {
        let light = crate::theme::is_light(cx);
        if light != self.light {
            self.light = light;
            self.paint();
            self.paint_menu();
        }
        self.refresh_thumbnail();
        let refresh =
            unsafe { GetPropW(self.host.hwnd, w!("RustShot.Pin.WorkspaceRefresh")) }.0 as usize;
        if refresh != 0 && refresh != self.refresh {
            self.refresh = refresh;
            if let Ok(workspace) = store::load() {
                if let Some(entry) = workspace
                    .pins
                    .iter()
                    .find(|entry| entry.id == self.session.as_ref().unwrap().id)
                {
                    self.hidden = entry.hidden || entry.group_id != workspace.current_group;
                    self.state.locked = entry.state.locked;
                    // 托盘可先清除原生穿透再广播；实际样式优先，不能被旧清单重新打开。
                    let through = pin_native::transparent(self.host.hwnd);
                    if through != entry.click_through {
                        if let Err(error) = self
                            .session
                            .as_ref()
                            .unwrap()
                            .persist_interaction(self.state.locked, through)
                        {
                            // 交互已恢复；保留实际样式，下轮再补存自己的记录。
                            self.refresh = 0;
                            eprintln!("窗口贴图交互状态保存失败：{error}");
                        }
                    }
                    self.host.show(!self.hidden);
                }
            }
        }
    }
    fn refresh_thumbnail(&mut self) {
        let observed = probe(self.source);
        let mut status = observed.state;
        if status == SourceState::Live {
            let update = self
                .thumbnail
                .as_ref()
                .ok_or("缩略图已释放")
                .and_then(|thumbnail| {
                    let size = thumbnail.size().map_err(|_| "缩略图源尺寸不可用")?;
                    let source = source_region(
                        self.region,
                        observed.client,
                        observed.window,
                        observed.extended,
                        size,
                    )
                    .ok_or("客户区或边框尺寸变化")?;
                    thumbnail
                        .update(source, destination(self.placement), 255, true)
                        .map_err(|_| "缩略图更新失败")
                });
            if update.is_err() {
                status = SourceState::Unavailable;
            }
        }
        if status != SourceState::Live {
            if let Some(thumbnail) = &self.thumbnail {
                let _ = thumbnail.hide();
            }
        }
        if status != self.status {
            self.status = status;
            self.paint();
        }
    }
    fn paint(&self) {
        let hdc = unsafe { GetDC(Some(self.host.hwnd)) };
        if hdc.is_invalid() {
            return;
        }
        let bounds = host_bounds(self.placement);
        fill(
            hdc,
            RECT {
                left: 0,
                top: 0,
                right: bounds.width as i32,
                bottom: bounds.height as i32,
            },
            self.light,
        );
        text(
            hdc,
            self.status.label(),
            RECT {
                left: 8,
                top: 0,
                right: bounds.width as i32 - 26,
                bottom: CAPTION,
            },
            self.light,
        );
        text(
            hdc,
            "×",
            RECT {
                left: bounds.width as i32 - 24,
                top: 0,
                right: bounds.width as i32,
                bottom: CAPTION,
            },
            self.light,
        );
        if self.status != SourceState::Live {
            let info = crate::gdi_surface::bitmap_info(self.fallback.width, self.fallback.height);
            unsafe {
                StretchDIBits(
                    hdc,
                    0,
                    CAPTION,
                    self.placement.width as i32,
                    self.placement.height as i32,
                    0,
                    0,
                    self.fallback.width as i32,
                    self.fallback.height as i32,
                    Some(self.fallback.bgra.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
        }
        unsafe {
            ReleaseDC(Some(self.host.hwnd), hdc);
        }
    }
    fn open_menu(&mut self) -> Result<()> {
        self.menu.take();
        let pointer = cursor();
        let area = pin_native::work_area(pointer);
        let width = MENU_WIDTH.min(area.width as i32);
        let height = MENU_ROW * 5;
        let menu = Host::create(
            self.sender.clone(),
            Placement {
                x: pointer.0.clamp(area.x, area.x + area.width as i32 - width),
                y: pointer
                    .1
                    .clamp(area.y, (area.y + area.height as i32 - height).max(area.y)),
                width: width as u32,
                height: height as u32,
            },
            Some(self.host.hwnd),
            tr!("窗口贴图操作", "Live pin actions"),
        )?;
        self.menu = Some(menu);
        self.paint_menu();
        self.menu.as_ref().unwrap().show(true);
        unsafe {
            let _ = SetForegroundWindow(self.menu.as_ref().unwrap().hwnd);
        }
        Ok(())
    }
    fn paint_menu(&self) {
        let Some(menu) = &self.menu else {
            return;
        };
        let hdc = unsafe { GetDC(Some(menu.hwnd)) };
        if hdc.is_invalid() {
            return;
        }
        fill(
            hdc,
            RECT {
                left: 0,
                top: 0,
                right: MENU_WIDTH,
                bottom: MENU_ROW * 5,
            },
            self.light,
        );
        let items = [
            tr!("转为静态贴图", "Make static"),
            tr!("前置源窗口", "Bring source forward"),
            tr!("恢复源窗口", "Restore source"),
            if pin_native::transparent(self.host.hwnd) {
                tr!("取消鼠标穿透", "Disable click-through")
            } else {
                tr!("鼠标穿透", "Click-through")
            },
            tr!("关闭", "Close"),
        ];
        for (i, item) in items.into_iter().enumerate() {
            text(
                hdc,
                item,
                RECT {
                    left: 10,
                    top: i as i32 * MENU_ROW,
                    right: MENU_WIDTH - 10,
                    bottom: (i as i32 + 1) * MENU_ROW,
                },
                self.light,
            );
        }
        unsafe {
            ReleaseDC(Some(menu.hwnd), hdc);
        }
    }
}

fn cursor() -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}
fn fill(hdc: HDC, bounds: RECT, light: bool) {
    let brush = unsafe { CreateSolidBrush(COLORREF(if light { 0xf5f5f5 } else { 0x242424 })) };
    unsafe {
        FillRect(hdc, &bounds, brush);
        let _ = DeleteObject(brush.into());
    }
}
fn text(hdc: HDC, value: &str, mut bounds: RECT, light: bool) {
    let mut value: Vec<u16> = value.encode_utf16().collect();
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(if light { 0x202020 } else { 0xe8e8e8 }));
        DrawTextW(
            hdc,
            &mut value,
            &mut bounds,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}
