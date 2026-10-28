//! 贴图可视标题：图片外的标题条与原地编辑。
//!
//! 标题条是贴图窗口拥有的不激活、鼠标穿透的分层小窗，只随主窗口移动和显隐；
//! 贴图窗口尺寸、内容原点、复制／保存／拖出与 OCR 输入都不包含它，桌面截图也排除它。
//! 编辑时在图片靠标题一侧临时叠一个单行输入框（输入法优先），Enter 确认、Esc 取消。
//! 标题与贴图工作区记录共用同一字段：本窗改动写入工作区并广播，工作区改动经刷新回到本窗。
use crate::{
    gdi_surface::Surface, gpui_capture::hwnd, pin_model::Placement, pin_view::PinView,
    text_field::TextField, Result,
};
use gpui::{div, prelude::*, px, AnyElement, App, Context, Entity, MouseButton, Window};
use std::sync::OnceLock;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
};

const CLASS: PCWSTR = w!("RustShot.Pin.Title.v1");
const OWNER_SUBCLASS: usize = 0x5253_5454;
/// 标题条当前有内容；为 0 时主窗口显隐、移动都不再把它带出来。
const LIVE_PROP: PCWSTR = w!("RustShot.Pin.Title.Live");
const SIDE_PROP: PCWSTR = w!("RustShot.Pin.Title.Side");
const GAP_PROP: PCWSTR = w!("RustShot.Pin.Title.Gap");
/// 新贴图登记时的默认标题（中文持久化）；视为未设置，不显示标题条。
const DEFAULT_TITLE: &str = "贴图";
pub(crate) const MAX_TITLE_CHARS: usize = 256;
const FONT_DIP: f32 = 13.;
const PAD_X_DIP: f32 = 8.;
const PAD_Y_DIP: f32 = 3.;
const GAP_DIP: f32 = 4.;
const RADIUS_DIP: f64 = 6.;
const MAX_WIDTH_DIP: f32 = 360.;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TitlePosition {
    #[default]
    Bottom,
    Top,
}
impl TitlePosition {
    pub(crate) fn from_wire(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bottom" => Some(Self::Bottom),
            "top" => Some(Self::Top),
            _ => None,
        }
    }
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Bottom => "bottom",
            Self::Top => "top",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bottom => tr!("图片下方", "Below"),
            Self::Top => tr!("图片上方", "Above"),
        }
    }
    fn other(self) -> Self {
        match self {
            Self::Bottom => Self::Top,
            Self::Top => Self::Bottom,
        }
    }
}

/// 标题条要显示的文字；空白和默认标题都不显示。
pub(crate) fn visible_title(title: &str) -> Option<&str> {
    let title = title.trim();
    (!title.is_empty() && title != DEFAULT_TITLE).then_some(title)
}
/// 编辑框初值：未设置时为空，便于直接输入。
fn editable_title(title: &str) -> String {
    visible_title(title).unwrap_or_default().to_owned()
}
/// 单行：控制字符（含粘贴进来的换行）换成空格，去首尾空白并限长。
fn normalize_title(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    text.trim().chars().take(MAX_TITLE_CHARS).collect()
}

/// 标题条左上角：默认在主窗口下方（或上方）居中；首选一侧超出工作区而另一侧放得下时换边，
/// 横向尽量留在工作区内。结果只依赖主窗口矩形，从不与主窗口重叠。
pub(crate) fn strip_origin(
    owner: Placement,
    size: (u32, u32),
    gap: i32,
    side: TitlePosition,
    work: Placement,
) -> (i32, i32) {
    let (width, height) = (i64::from(size.0), i64::from(size.1));
    let gap = i64::from(gap.max(0));
    let top = |side| match side {
        TitlePosition::Bottom => i64::from(owner.y) + i64::from(owner.height) + gap,
        TitlePosition::Top => i64::from(owner.y) - gap - height,
    };
    let (work_top, work_bottom) = (
        i64::from(work.y),
        i64::from(work.y) + i64::from(work.height),
    );
    let fits = |y: i64| y >= work_top && y + height <= work_bottom;
    let y = if fits(top(side)) || !fits(top(side.other())) {
        top(side)
    } else {
        top(side.other())
    };
    let mut x = i64::from(owner.x) + (i64::from(owner.width) - width).div_euclid(2);
    let (work_left, work_right) = (i64::from(work.x), i64::from(work.x) + i64::from(work.width));
    if width <= work_right - work_left {
        x = x.clamp(work_left, work_right - width);
    }
    let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    (clamp(x), clamp(y))
}

#[derive(Clone, Debug, PartialEq)]
struct Appearance {
    text: String,
    scale_milli: u32,
    alpha: u8,
    side: TitlePosition,
    /// 底色、文字、描边（0xRRGGBB），随浅色／深色主题。
    colors: (u32, u32, u32),
}

/// 量好标题后建一次性离屏位图并画出标题条。
fn render(look: &Appearance) -> Result<Surface> {
    let scale = look.scale_milli as f32 / 1000.;
    let mut surface = Surface::new((FONT_DIP * scale).round() as i32, w!("Microsoft YaHei UI"))?;
    let (text_width, text_height) = surface.measure(&look.text, DT_SINGLELINE, 0);
    let pad_x = (PAD_X_DIP * scale).round() as i32;
    let pad_y = (PAD_Y_DIP * scale).round() as i32;
    let max_text = ((MAX_WIDTH_DIP * scale).round() as i32 - pad_x * 2).max(1);
    let width = (text_width.clamp(1, max_text) + pad_x * 2) as u32;
    let height = (text_height.max(1) + pad_y * 2) as u32;
    let (background, foreground, border) = look.colors;
    surface.allocate(width, height, background)?;
    surface.draw(
        &look.text,
        foreground,
        RECT {
            left: pad_x,
            top: pad_y,
            right: width as i32 - pad_x,
            bottom: height as i32 - pad_y,
        },
        DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
    );
    surface.finish(scale, RADIUS_DIP, border);
    Ok(surface)
}

fn register_class() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
            let class = WNDCLASSW {
                hInstance: HINSTANCE(instance.0),
                lpszClassName: CLASS,
                lpfnWndProc: Some(title_proc),
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
unsafe extern "system" fn title_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCDESTROY => {
            for key in [LIVE_PROP, SIDE_PROP, GAP_PROP] {
                let _ = unsafe { RemovePropW(h, key) };
            }
            unsafe { DefWindowProcW(h, message, wp, lp) }
        }
        _ => unsafe { DefWindowProcW(h, message, wp, lp) },
    }
}
/// 主窗口子类：位置或显隐变化后立刻带上标题条，不经 GPUI 事件循环，拖动时不落后一帧。
unsafe extern "system" fn owner_proc(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    title: usize,
) -> LRESULT {
    let result = unsafe { DefSubclassProc(h, message, wp, lp) };
    match message {
        WM_WINDOWPOSCHANGED => unsafe { follow(h, HWND(title as *mut _)) },
        WM_NCDESTROY => unsafe {
            let _ = RemoveWindowSubclass(h, Some(owner_proc), id);
        },
        _ => {}
    }
    result
}
fn rect_of(h: HWND) -> Option<Placement> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.ok()?;
    Some(Placement {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
    })
}
fn work_area_of(owner: Placement) -> Placement {
    let rect = RECT {
        left: owner.x,
        top: owner.y,
        right: owner.x.saturating_add(owner.width as i32),
        bottom: owner.y.saturating_add(owner.height as i32),
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcWork;
            return Placement {
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(1) as u32,
                height: (r.bottom - r.top).max(1) as u32,
            };
        }
    }
    owner
}
fn owner_shown(owner: HWND) -> bool {
    unsafe { IsWindowVisible(owner).as_bool() && !IsIconic(owner).as_bool() }
}
fn target_origin(owner: HWND, title: HWND, size: (u32, u32)) -> Option<(i32, i32)> {
    let owner = rect_of(owner)?;
    let (side, gap) = unsafe {
        (
            if GetPropW(title, SIDE_PROP).0 as usize == 1 {
                TitlePosition::Top
            } else {
                TitlePosition::Bottom
            },
            GetPropW(title, GAP_PROP).0 as usize as i32,
        )
    };
    Some(strip_origin(owner, size, gap, side, work_area_of(owner)))
}
unsafe fn follow(owner: HWND, title: HWND) {
    unsafe {
        if !IsWindow(Some(title)).as_bool() || GetPropW(title, LIVE_PROP).is_invalid() {
            return;
        }
        if !owner_shown(owner) {
            if IsWindowVisible(title).as_bool() {
                let _ = ShowWindow(title, SW_HIDE);
            }
            return;
        }
        let Some(current) = rect_of(title) else {
            return;
        };
        let Some((x, y)) = target_origin(owner, title, (current.width, current.height)) else {
            return;
        };
        if (x, y) != (current.x, current.y) || !IsWindowVisible(title).as_bool() {
            let _ = SetWindowPos(
                title,
                None,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
}
/// 当前主窗口实际 alpha（含穿透时的降透明），标题条与之一致。
fn owner_alpha(owner: HWND) -> u8 {
    let mut alpha = 255u8;
    let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
    let ok = unsafe {
        GetWindowLongPtrW(owner, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize != 0
            && GetLayeredWindowAttributes(owner, None, Some(&mut alpha), Some(&mut flags)).is_ok()
    };
    if ok && flags.contains(LWA_ALPHA) {
        alpha
    } else {
        255
    }
}

struct NativeTitle {
    owner: HWND,
    hwnd: HWND,
}
impl NativeTitle {
    fn new(owner: HWND) -> Result<Self> {
        register_class()?;
        let hwnd = unsafe {
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
        let title = Self { owner, hwnd };
        unsafe {
            // 与阴影一样只是屏幕上的装饰：桌面截图、录屏和贴图自身输出都不带出它。
            crate::remote_display::exclude_from_capture(hwnd)?;
            SetWindowSubclass(owner, Some(owner_proc), OWNER_SUBCLASS, hwnd.0 as usize).ok()?;
        }
        Ok(title)
    }
    fn valid(&self) -> bool {
        unsafe {
            IsWindow(Some(self.hwnd)).as_bool()
                && GetWindow(self.hwnd, GW_OWNER).is_ok_and(|owner| owner == self.owner)
        }
    }
    fn hide(&self) {
        if self.valid() {
            unsafe {
                let _ = RemovePropW(self.hwnd, LIVE_PROP);
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
    fn upload(&self, look: &Appearance) -> Result<()> {
        if !self.valid() {
            return Err("标题条窗口已失效".into());
        }
        let surface = render(look)?;
        let scale = look.scale_milli as f32 / 1000.;
        let gap = (GAP_DIP * scale).round() as usize;
        let side = usize::from(look.side == TitlePosition::Top);
        unsafe {
            SetPropW(self.hwnd, SIDE_PROP, Some(HANDLE(side as *mut _)))?;
            SetPropW(self.hwnd, GAP_PROP, Some(HANDLE(gap as *mut _)))?;
            let (x, y) =
                target_origin(self.owner, self.hwnd, surface.size).ok_or("贴图窗口位置不可读")?;
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&POINT { x, y }),
                Some(&SIZE {
                    cx: surface.size.0 as i32,
                    cy: surface.size.1 as i32,
                }),
                Some(surface.dc),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: look.alpha,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            )?;
            SetPropW(self.hwnd, LIVE_PROP, Some(HANDLE(1 as *mut _)))?;
            if owner_shown(self.owner) {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            } else {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        Ok(())
    }
}
impl Drop for NativeTitle {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.owner, Some(owner_proc), OWNER_SUBCLASS);
        }
        // 主窗口先销毁时标题条已随之销毁；句柄可能被复用，只回收仍归属原主窗口的同类窗口。
        if self.valid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

pub(crate) struct TitleEditor {
    field: Entity<TextField>,
    _activation: gpui::Subscription,
}

#[derive(Default)]
pub(crate) struct PinTitleState {
    native: Option<NativeTitle>,
    shown: Option<Appearance>,
    failed: Option<Appearance>,
    /// 未登记工作区时的标题；登记后以工作区记录为准。
    local: String,
    editor: Option<TitleEditor>,
    position: Option<TitlePosition>,
    settings_modified: Option<std::time::SystemTime>,
    settings_ticks: u8,
}
impl PinTitleState {
    fn position(&mut self) -> TitlePosition {
        *self
            .position
            .get_or_insert_with(|| crate::preferences::Preferences::load().pin_title_position)
    }
    fn apply(&mut self, owner: HWND, look: Option<Appearance>) {
        if self.shown == look || (look.is_some() && self.failed == look) {
            return;
        }
        let Some(look) = look else {
            if let Some(native) = &self.native {
                native.hide();
            }
            self.shown = None;
            return;
        };
        if self.native.as_ref().is_some_and(|native| !native.valid()) {
            self.native = None;
        }
        let result = match &mut self.native {
            Some(native) => native.upload(&look),
            empty => NativeTitle::new(owner).and_then(|native| {
                let native = empty.insert(native);
                native.upload(&look)
            }),
        };
        match result {
            Ok(()) => {
                self.failed = None;
                self.shown = Some(look);
            }
            Err(error) => {
                eprintln!("贴图标题条：{error}");
                if let Some(native) = &self.native {
                    native.hide();
                }
                self.shown = None;
                self.failed = Some(look);
            }
        }
    }
}

/// 同进程已打开的贴图立即采用设置里的标题位置；其它进程的贴图在下一次检查设置时跟上。
pub(crate) fn sync_open_preferences(position: TitlePosition, cx: &mut App) {
    for window in cx.windows() {
        if let Some(pin) = window.downcast::<PinView>() {
            let _ = pin.update(cx, |v, _, cx| {
                if v.pin_title.position != Some(position) {
                    v.pin_title.position = Some(position);
                    cx.notify();
                }
            });
        }
    }
}

impl PinView {
    pub(crate) fn pin_title_text(&self) -> &str {
        self.workspace
            .as_ref()
            .map_or(self.pin_title.local.as_str(), |live| {
                live.entry.title.as_str()
            })
    }
    /// 每帧调用；外观（文字、主题、DPI、透明度、位置）不变时不碰原生窗口。
    pub(crate) fn sync_pin_title(&mut self, w: &Window, cx: &App) {
        let owner = hwnd(w);
        let side = self.pin_title.position();
        // 工具栏贴在图片下方（放不下时在上方），展开期间让出位置；编辑时由输入框代替。
        let text = (self.pin_title.editor.is_none() && !self.chrome)
            .then(|| visible_title(self.pin_title_text()).map(str::to_owned))
            .flatten();
        let look = text.map(|text| {
            let palette = crate::theme::palette(cx);
            Appearance {
                text,
                scale_milli: (w.scale_factor() * 1000.).round().max(1.) as u32,
                alpha: owner_alpha(owner),
                side,
                colors: (palette.background, palette.foreground, palette.border),
            }
        });
        self.pin_title.apply(owner, look);
    }
    /// 工作区节拍里低频检查设置文件；其它进程改了标题位置时只需一次 stat。
    pub(crate) fn pin_title_tick(&mut self, w: &Window, cx: &mut Context<Self>) {
        self.pin_title.settings_ticks = self.pin_title.settings_ticks.wrapping_add(1);
        if self.pin_title.settings_ticks % 8 == 0 {
            let modified = std::fs::metadata(crate::preferences::data_dir().join("settings.json"))
                .and_then(|meta| meta.modified())
                .ok();
            if modified != self.pin_title.settings_modified {
                let first = self.pin_title.settings_modified.is_none();
                self.pin_title.settings_modified = modified;
                if !first {
                    self.pin_title.position = None;
                    cx.notify();
                }
            }
        }
        // 穿透切换等只改原生 alpha 的路径不一定重绘；节拍内补一次比较。
        self.sync_pin_title(w, cx);
    }
    pub(crate) fn title_editor_focused(&self, w: &Window, cx: &App) -> bool {
        self.pin_title
            .editor
            .as_ref()
            .is_some_and(|editor| editor.field.read(cx).focus_handle.is_focused(w))
    }
    pub(crate) fn begin_title_edit(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.pin_title.editor {
            w.focus(&editor.field.read(cx).focus_handle);
            return;
        }
        let initial = editable_title(self.pin_title_text());
        let field = cx.new(|cx| {
            TextField::new(
                initial,
                tr!("贴图标题（留空则不显示）", "Pin title (empty hides it)"),
                cx,
            )
        });
        // 点到别的窗口即确认，和资源管理器改名一致；Esc 才放弃。
        let activation = cx.observe_window_activation(w, |v: &mut Self, w, cx| {
            if !w.is_window_active() {
                v.commit_title_edit(w, cx);
            }
        });
        if !w.is_window_active() {
            w.activate_window();
        }
        w.focus(&field.read(cx).focus_handle);
        self.pin_title.editor = Some(TitleEditor {
            field,
            _activation: activation,
        });
        cx.notify();
    }
    fn close_title_editor(&mut self, w: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let editor = self.pin_title.editor.take()?;
        let field = editor.field.read(cx);
        let text = field.text().to_owned();
        if field.focus_handle.is_focused(w) {
            w.focus(&self.focus);
        }
        cx.notify();
        Some(text)
    }
    pub(crate) fn cancel_title_edit(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.close_title_editor(w, cx);
    }
    pub(crate) fn commit_title_edit(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.close_title_editor(w, cx) else {
            return;
        };
        let title = normalize_title(&text);
        if title == editable_title(self.pin_title_text()) {
            return;
        }
        if let Err(error) = self.store_pin_title(title) {
            self.status = trf!("标题未保存：{error}", "Title not saved: {error}");
        }
    }
    fn store_pin_title(&mut self, title: String) -> Result<()> {
        let Some(live) = &mut self.workspace else {
            self.pin_title.local = title;
            return Ok(());
        };
        crate::pin_workspace::apply(crate::pin_workspace::Action::SetTitle {
            id: live.session.id.clone(),
            title: title.clone(),
        })?;
        live.entry.title = title;
        crate::pin_workspace_runtime::broadcast();
        Ok(())
    }
    /// 编辑框的 Enter／Esc 先于输入框处理；输入法合成期间全部让行。
    fn title_editor_key(
        &mut self,
        e: &gpui::KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(editor) = &self.pin_title.editor else {
            return false;
        };
        if editor.field.read(cx).is_composing() {
            return false;
        }
        let m = e.keystroke.modifiers;
        match e.keystroke.key.as_str() {
            "enter" if !m.control && !m.alt && !m.platform && !m.function => {
                if !e.is_held {
                    self.commit_title_edit(w, cx);
                }
                true
            }
            "escape" => {
                if !e.is_held {
                    self.cancel_title_edit(w, cx);
                }
                true
            }
            _ => false,
        }
    }
    pub(crate) fn render_title_editor(&self, w: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let editor = self.pin_title.editor.as_ref()?;
        let width = f32::from(w.viewport_size().width);
        let field_width = if width > 92. {
            (width - 12.).min(320.)
        } else {
            (width - 4.).max(1.)
        };
        let top = self.pin_title.position == Some(TitlePosition::Top);
        Some(
            div()
                .id("pin-title-editor")
                .absolute()
                .left(px((width - field_width) * 0.5))
                .w(px(field_width))
                .when(top, |d| d.top(px(6.)))
                .when(!top, |d| d.bottom(px(6.)))
                .rounded_md()
                .shadow_md()
                .capture_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, w, cx| {
                    if v.title_editor_key(e, w, cx) {
                        cx.stop_propagation();
                    }
                }))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(|v, _, w, cx| v.commit_title_edit(w, cx)))
                .child(editor.field.clone())
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Placement {
        Placement {
            x,
            y,
            width,
            height,
        }
    }
    fn overlaps(a: Placement, b: Placement) -> bool {
        let (ax2, ay2) = (a.x as i64 + a.width as i64, a.y as i64 + a.height as i64);
        let (bx2, by2) = (b.x as i64 + b.width as i64, b.y as i64 + b.height as i64);
        (a.x as i64) < bx2 && (b.x as i64) < ax2 && (a.y as i64) < by2 && (b.y as i64) < ay2
    }

    #[test]
    fn pin_title_visibility_hides_blank_and_default_title() {
        assert_eq!(visible_title(""), None);
        assert_eq!(visible_title("   "), None);
        assert_eq!(visible_title(DEFAULT_TITLE), None);
        assert_eq!(visible_title(" 报表 "), Some("报表"));
        assert_eq!(editable_title(DEFAULT_TITLE), "");
        assert_eq!(editable_title("报表"), "报表");
        assert_eq!(normalize_title("  a\r\nb\t "), "a  b");
        assert_eq!(
            normalize_title(&"字".repeat(MAX_TITLE_CHARS + 9))
                .chars()
                .count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn pin_title_position_wire_round_trip_and_unknown_falls_back() {
        for position in [TitlePosition::Bottom, TitlePosition::Top] {
            assert_eq!(
                TitlePosition::from_wire(position.wire_name()),
                Some(position)
            );
        }
        assert_eq!(TitlePosition::from_wire(" TOP "), Some(TitlePosition::Top));
        assert_eq!(TitlePosition::from_wire("left"), None);
        assert_eq!(TitlePosition::default(), TitlePosition::Bottom);
    }

    #[test]
    fn pin_title_strip_stays_outside_pin_and_keeps_its_side_when_room() {
        let work = rect(-1920, 0, 1920, 1040);
        let owner = rect(-1500, 300, 400, 200);
        let size = (120, 24);
        let (x, y) = strip_origin(owner, size, 4, TitlePosition::Bottom, work);
        assert_eq!((x, y), (-1500 + 140, 504));
        let (x, y) = strip_origin(owner, size, 4, TitlePosition::Top, work);
        assert_eq!((x, y), (-1360, 300 - 4 - 24));
        // 首选一侧放不下而另一侧放得下时换边。
        let low = rect(-1500, 1000, 400, 30);
        assert_eq!(
            strip_origin(low, size, 4, TitlePosition::Bottom, work).1,
            1000 - 4 - 24
        );
        let high = rect(-1500, 2, 400, 30);
        assert_eq!(strip_origin(high, size, 4, TitlePosition::Top, work).1, 36);
        // 两侧都放不下（贴图比屏幕高）时保持首选一侧，不压到图片上。
        let tall = rect(-1500, -10, 400, 1100);
        assert_eq!(
            strip_origin(tall, size, 4, TitlePosition::Bottom, work).1,
            1094
        );
        // 缩略后比标题还窄时仍居中，靠屏幕边时横向留在工作区内。
        let thumb = rect(-1915, 500, 40, 40);
        let (x, _) = strip_origin(thumb, size, 4, TitlePosition::Bottom, work);
        assert_eq!(x, -1920);
    }

    #[test]
    fn pin_title_strip_never_covers_exported_pin_area() {
        // 标题条始终在贴图窗口矩形之外：复制、保存、拖出和 OCR 只读窗口内容，不会带上标题。
        let work = rect(0, 0, 2560, 1400);
        for owner in [
            rect(10, 10, 1, 1),
            rect(100, 100, 30, 900),
            rect(-20, 1390, 300, 50),
            rect(0, 0, 2560, 1400),
            rect(2500, -30, 300, 60),
        ] {
            for side in [TitlePosition::Bottom, TitlePosition::Top] {
                for gap in [0, 4, 6] {
                    let size = (180, 26);
                    let (x, y) = strip_origin(owner, size, gap, side, work);
                    assert!(
                        !overlaps(owner, rect(x, y, size.0, size.1)),
                        "{owner:?} {side:?} {gap}"
                    );
                }
            }
        }
    }

    #[test]
    fn pin_title_window_is_excluded_from_capture_click_through_and_follows_owner_visibility() {
        register_class().unwrap();
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!(""),
                WS_POPUP,
                100,
                100,
                200,
                120,
                None,
                None,
                Some(HINSTANCE(GetModuleHandleW(None).unwrap().0)),
                None,
            )
            .unwrap()
        };
        let native = NativeTitle::new(owner).unwrap();
        let look = Appearance {
            text: "季度报表".into(),
            scale_milli: 1000,
            alpha: 200,
            side: TitlePosition::Bottom,
            colors: (0x1d222a, 0xd9e2e9, 0x3d4651),
        };
        native.upload(&look).unwrap();
        let mut affinity = 0;
        unsafe { GetWindowDisplayAffinity(native.hwnd, &mut affinity).unwrap() };
        assert_eq!(affinity, crate::remote_display::capture_affinity().0);
        let ex = unsafe { GetWindowLongPtrW(native.hwnd, GWL_EXSTYLE) } as u32;
        let required = (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0;
        assert_eq!(ex & required, required);
        let strip = rect_of(native.hwnd).unwrap();
        assert!(!overlaps(rect_of(owner).unwrap(), strip));
        // 主窗口隐藏时标题条不显示；移动隐藏的主窗口也不会把它带出来。
        assert!(!unsafe { IsWindowVisible(native.hwnd) }.as_bool());
        unsafe {
            SetWindowPos(owner, None, 300, 300, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE).unwrap();
        }
        assert!(!unsafe { IsWindowVisible(native.hwnd) }.as_bool());
        let title = native.hwnd;
        drop(native);
        assert!(!unsafe { IsWindow(Some(title)) }.as_bool());
        unsafe { DestroyWindow(owner).unwrap() };
    }
}
